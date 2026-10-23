//! One tab on a `Cdp` connection, as `browser/session.py`'s `BrowserSession` holds it: opened (and owned) or
//! attached (and left open), its cross-origin iframes auto-attached in sessions of their own, its dialogs, and
//! its paused document requests released.
//!
//! State is kept by a handler on the connection's reader, so it is current whenever a call returns.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::future::try_join_all;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::cdp::{Cdp, CdpError, Event, WeakCdp};

const SNAPSHOT_JS: &str = include_str!("../../../src/fastbrowse/browser/snapshot.js");
const INPUT_WORLD: &str = "fastbrowse-input";
const INPUT_NAVIGATION_BINDING: &str = "__fastbrowseInputNavigation";
/// session.py's `INPUT_HISTORY_JS`, verbatim.
const INPUT_HISTORY_JS: &str = concat!(
    "(() => { if (globalThis.__watching) return; globalThis.__watching = true; let input = 0; ",
    "const later = () => setTimeout(() => setTimeout(() => { input--; }, 0), 0); ",
    "globalThis.__openInput = () => { input++; return setTimeout(() => { input--; }, 2000); }; ",
    "globalThis.__closeInput = timer => { clearTimeout(timer); later(); }; ",
    "for (const type of ['click', 'submit', 'keydown', 'keyup', 'input', 'change', 'pointerup', 'mouseup']) ",
    "addEventListener(type, ev => { if (!ev.isTrusted) return; input++; later(); }, ",
    "{capture: true, passive: true}); ",
    "if (globalThis.navigation) navigation.addEventListener('navigate', ev => { ",
    "if (!(ev.userInitiated || input > 0)) return; ",
    "const report = globalThis['__fastbrowseInputNavigation']; if (typeof report === 'function') report('1'); }); ",
    "})()"
);

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Dialog {
    pub kind: String,
    pub message: String,
    pub default_prompt: Option<String>,
}

#[derive(Default)]
struct State {
    /// OOPIF target id -> its session, in attach order, as session.py's `_frame_sessions` dict.
    frame_sessions: Vec<(String, String)>,
    /// Frame session -> the session that attached it.
    frame_parents: HashMap<String, String>,
    dialogs: HashMap<String, Dialog>,
    input_navigations: u32,
    background: Vec<JoinHandle<()>>,
    closing: bool,
}

pub struct Options {
    /// session.py's scoped mode: new targets start paused and are resumed once their session is prepared.
    pub pause_new_targets: bool,
}

pub struct Session {
    pub cdp: Cdp,
    pub target_id: String,
    pub session_id: String,
    owned: bool,
    state: Arc<Mutex<State>>,
    dialog_opened: watch::Receiver<u64>,
}

struct Shared {
    cdp: WeakCdp,
    state: Arc<Mutex<State>>,
    dialog_opened: watch::Sender<u64>,
    pause: bool,
}

impl Session {
    /// Open a tab of our own, as `_open_owned_tab`; it is closed by `close`.
    pub async fn open(cdp: &Cdp, options: Options) -> Result<Self, CdpError> {
        let created = cdp.call("Target.createTarget", json!({"url": "about:blank"}), None).await?;
        let target_id = str_of(&created["targetId"]);
        Self::attach_to(cdp, target_id, true, options).await
    }

    /// Attach to the first page whose address or title contains `needle`, as `_attach_existing_tab`.
    pub async fn attach(cdp: &Cdp, needle: &str, options: Options) -> Result<Self, String> {
        let targets = cdp.call("Target.getTargets", json!({}), None).await.map_err(|e| e.to_string())?;
        let target = targets["targetInfos"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["type"] == "page" && !t["url"].as_str().unwrap_or("").starts_with("devtools://"))
            .find(|t| [&t["url"], &t["title"]].iter().any(|v| v.as_str().unwrap_or("").contains(needle)))
            .ok_or_else(|| format!("no page matching {needle:?} to attach to"))?;
        Self::attach_to(cdp, str_of(&target["targetId"]), false, options).await.map_err(|e| e.to_string())
    }

    async fn attach_to(cdp: &Cdp, target_id: String, owned: bool, options: Options) -> Result<Self, CdpError> {
        let state = Arc::new(Mutex::new(State::default()));
        let (dialog_tx, dialog_opened) = watch::channel(0);
        let shared = Arc::new(Shared {
            cdp: cdp.downgrade(),
            state: state.clone(),
            dialog_opened: dialog_tx,
            pause: options.pause_new_targets,
        });
        let handler = shared.clone();
        cdp.on_event(Box::new(move |event| handler.on_event(event)));
        let mut session = Self { cdp: cdp.clone(), target_id, session_id: String::new(), owned, state, dialog_opened };
        let attached =
            cdp.call("Target.attachToTarget", json!({"targetId": session.target_id, "flatten": true}), None).await;
        session.session_id = match attached {
            Ok(attached) => str_of(&attached["sessionId"]),
            Err(e) => {
                session.close().await;
                return Err(e);
            }
        };
        if let Err(e) = prepare(cdp, &session.session_id, shared.pause).await {
            session.close().await;
            return Err(e);
        }
        if !owned {
            // A document that loaded before the new-document scripts were registered gets them now.
            let _ = run_document_scripts(cdp, &session.session_id).await;
        }
        Ok(session)
    }

    /// Frame sessions descended from this tab's session, in attach order: session.py's `frame_sessions`.
    pub fn frame_sessions(&self) -> Vec<(String, String)> {
        let state = self.state.lock().unwrap();
        let belongs = |sid: &str| root(&state.frame_parents, sid) == self.session_id;
        state.frame_sessions.iter().filter(|(_, sid)| belongs(sid)).cloned().collect()
    }

    pub fn frame_parent(&self, session_id: &str) -> Option<String> {
        self.state.lock().unwrap().frame_parents.get(session_id).cloned()
    }

    /// The tab's dialog, else the first of its frames' (`pending_dialog`), with the session it is on.
    pub fn pending_dialog(&self) -> Option<(String, Dialog)> {
        let frames = self.frame_sessions();
        let state = self.state.lock().unwrap();
        std::iter::once(&self.session_id)
            .chain(frames.iter().map(|(_, sid)| sid))
            .find_map(|sid| state.dialogs.get(sid).map(|d| (sid.clone(), d.clone())))
    }

    /// Resolves once a dialog is pending on the tab or one of its frames. Never misses one: the watch is marked
    /// seen before the state is read.
    pub async fn wait_for_dialog(&self) {
        let mut opened = self.dialog_opened.clone();
        loop {
            opened.borrow_and_update();
            if self.pending_dialog().is_some() {
                return;
            }
            if opened.changed().await.is_err() {
                return std::future::pending().await;
            }
        }
    }

    pub async fn handle_dialog(&self, accept: bool, prompt_text: Option<&str>) -> Result<(), CdpError> {
        let Some((sid, _)) = self.pending_dialog() else { return Ok(()) };
        let mut params = json!({"accept": accept});
        if let Some(text) = prompt_text {
            params["promptText"] = text.into();
        }
        self.cdp.call("Page.handleJavaScriptDialog", params, Some(&sid)).await?;
        self.state.lock().unwrap().dialogs.remove(&sid);
        Ok(())
    }

    pub fn input_navigations(&self) -> u32 {
        self.state.lock().unwrap().input_navigations
    }

    /// session.py's `_close`: stop background work, then close the tab if this session opened it. An attached
    /// tab is left open; closing the connection detaches from it.
    pub async fn close(&self) {
        let background = {
            let mut state = self.state.lock().unwrap();
            state.closing = true;
            std::mem::take(&mut state.background)
        };
        for task in &background {
            task.abort();
        }
        for task in background {
            let _ = task.await;
        }
        if self.owned {
            let _ = self.cdp.call("Target.closeTarget", json!({"targetId": self.target_id}), None).await;
        }
    }
}

impl Shared {
    fn on_event(&self, event: &Event) {
        let Some(session) = event.session.as_deref() else { return };
        let params = &event.params;
        let mut state = self.state.lock().unwrap();
        match event.method.as_str() {
            "Target.attachedToTarget" => {
                let child = str_of(&params["sessionId"]);
                let paused = params["waitingForDebugger"] == true;
                if params["targetInfo"]["type"] == "iframe" {
                    let target = str_of(&params["targetInfo"]["targetId"]);
                    match state.frame_sessions.iter_mut().find(|(t, _)| *t == target) {
                        Some(entry) => entry.1 = child.clone(),
                        None => state.frame_sessions.push((target, child.clone())),
                    }
                    state.frame_parents.insert(child.clone(), session.to_owned());
                    let pause = self.pause;
                    self.spawn(&mut state, move |cdp| async move {
                        let _ = prepare(&cdp, &child, pause).await;
                    });
                } else if paused {
                    // A worker has no document to scope; let it run.
                    self.spawn(&mut state, move |cdp| async move {
                        let _ = cdp.call("Runtime.runIfWaitingForDebugger", json!({}), Some(&child)).await;
                    });
                }
            }
            "Target.detachedFromTarget" => {
                let child = str_of(&params["sessionId"]);
                state.frame_parents.remove(&child);
                state.frame_sessions.retain(|(_, sid)| *sid != child);
                state.dialogs.remove(&child);
            }
            "Page.javascriptDialogOpening" => {
                let dialog = Dialog {
                    kind: str_of(&params["type"]),
                    message: str_of(&params["message"]),
                    default_prompt: params["defaultPrompt"].as_str().map(Into::into),
                };
                state.dialogs.insert(session.to_owned(), dialog);
                self.dialog_opened.send_modify(|n| *n += 1);
            }
            "Page.javascriptDialogClosed" => {
                state.dialogs.remove(session);
            }
            "Runtime.bindingCalled" if params["name"] == INPUT_NAVIGATION_BINDING => state.input_navigations += 1,
            // Every paused document response is released. Capturing downloads from it is not ported.
            "Fetch.requestPaused" => {
                let (request, session) = (str_of(&params["requestId"]), session.to_owned());
                self.spawn(&mut state, move |cdp| async move {
                    let _ = cdp.call("Fetch.continueRequest", json!({"requestId": request}), Some(&session)).await;
                });
            }
            _ => {}
        }
    }

    fn spawn<F, Fut>(&self, state: &mut State, work: F)
    where
        F: FnOnce(Cdp) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let Some(cdp) = self.cdp.upgrade() else { return };
        if state.closing {
            return;
        }
        state.background.retain(|task| !task.is_finished());
        state.background.push(tokio::spawn(work(cdp)));
    }
}

/// session.py's `_prepare_session`, for a tab or a frame: every domain and script at once, all before it is used.
async fn prepare(cdp: &Cdp, sid: &str, pause: bool) -> Result<(), CdpError> {
    let track = format!("{SNAPSHOT_JS}('fingerprint')");
    let s = Some(sid);
    let patterns = json!([{"urlPattern": "*", "resourceType": "Document", "requestStage": "Response"}]);
    try_join_all([
        cdp.call("Page.enable", json!({}), s),
        cdp.call("Runtime.enable", json!({}), s),
        cdp.call("DOM.enable", json!({}), s),
        cdp.call(
            "Runtime.addBinding",
            json!({"name": INPUT_NAVIGATION_BINDING, "executionContextName": INPUT_WORLD}),
            s,
        ),
        cdp.call(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source": INPUT_HISTORY_JS, "worldName": INPUT_WORLD}),
            s,
        ),
        cdp.call(
            "Target.setAutoAttach",
            json!({"autoAttach": true, "waitForDebuggerOnStart": pause, "flatten": true}),
            s,
        ),
        cdp.call("Fetch.enable", json!({"patterns": patterns}), s),
        cdp.call("Page.addScriptToEvaluateOnNewDocument", json!({"source": track}), s),
    ])
    .await?;
    if pause {
        cdp.call("Runtime.runIfWaitingForDebugger", json!({}), s).await?;
    }
    Ok(())
}

/// `_run_document_scripts`: the new-document scripts, run once in a document that was already open.
async fn run_document_scripts(cdp: &Cdp, sid: &str) -> Result<(), CdpError> {
    let s = Some(sid);
    let tree = cdp.call("Page.getFrameTree", json!({}), s).await?;
    let world = cdp
        .call(
            "Page.createIsolatedWorld",
            json!({"frameId": tree["frameTree"]["frame"]["id"], "worldName": INPUT_WORLD}),
            s,
        )
        .await?;
    let track = format!("{SNAPSHOT_JS}('fingerprint')");
    try_join_all([
        cdp.call("Runtime.evaluate", json!({"expression": track}), s),
        cdp.call(
            "Runtime.evaluate",
            json!({"expression": INPUT_HISTORY_JS, "contextId": world["executionContextId"]}),
            s,
        ),
    ])
    .await?;
    Ok(())
}

/// The tab session a frame session descends from (itself if it has no parent).
fn root<'a>(parents: &'a HashMap<String, String>, mut sid: &'a str) -> &'a str {
    for _ in 0..64 {
        match parents.get(sid) {
            Some(parent) => sid = parent,
            None => break,
        }
    }
    sid
}

pub fn str_of(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}
