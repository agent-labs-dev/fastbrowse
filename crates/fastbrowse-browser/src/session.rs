//! One owned tab on a `BrowserConnection`: attach, OOPIF sessions, popup ownership, dialogs, downloads, live
//! frames and origin scoping. `browser/session.py`'s `BrowserSession`.
//!
//! State is kept by a handler on the connection's reader, so it is current whenever a call returns. The handler
//! never blocks: work an event starts (preparing a frame's session, adopting a popup, releasing a paused request)
//! runs as a background task the session tracks and stops when it closes.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use data_encoding::BASE64;
use fastbrowse_core::BoxFuture;
use fastbrowse_core::models::{Artifact, ArtifactKind, ArtifactSink, BrowserConnection, FrameHandler};
use fastbrowse_core::origins::OriginGrant;
use fastbrowse_core::page::{Dialog, Tab};
use futures_util::future::{BoxFuture as FBoxFuture, try_join_all};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};

use crate::cdp::{Cdp, Event, secs};
use crate::error::BrowserError;
use crate::warn;

const SCREENCAST_COMMAND_SECONDS: f64 = 0.5;
/// How long closing waits for a cancelled frame callback. The callback is the caller's code, and one that swallows
/// its cancellation held the run open with no result while its cloud browser went on billing.
const FRAME_RELEASE_SECONDS: f64 = 2.0;

pub const SNAPSHOT_JS: &str = include_str!("../../../src/fastbrowse/browser/snapshot.js");
pub const CAPTURE_JS: &str = include_str!("../../../src/fastbrowse/browser/capture.js");
/// What a call to the installed snapshot function throws in a document that never installed it.
pub const SNAPSHOT_UNINSTALLED: &str = "fastbrowse: snapshot.js is not installed in this document";

/// snapshot.js, compiled once in each document and called by name after that.
///
/// Sent whole with every evaluate, its source cost about 3ms of transfer and compiling per call, and each call started
/// again from unoptimized code. A document installs it as it starts, from the tracking script every new document runs;
/// a document older than the session installs it when a call first finds it missing. The key names the source and the
/// grant, so another build or another grant never calls this one's function.
#[derive(Debug, Clone)]
pub struct SnapshotScript {
    /// An expression that installs the function, if this document has none, and evaluates to it.
    pub install: String,
    /// An expression for the installed function, which throws `SNAPSHOT_UNINSTALLED` where there is none.
    pub function: String,
}

impl SnapshotScript {
    /// `origins` is the grant as the page script reads it: `null`, or the sorted origins as a JSON list.
    pub fn new(origins: &str) -> Self {
        let digest = crate::page::sha256_hex(&format!("{origins}\n{SNAPSHOT_JS}"));
        let key = py_json(&json!(&digest[..16]));
        Self {
            install: format!(
                "((globalThis.__fastbrowseScripts ||= {{}})[{key}] ||= \
                 (snapshot => (mode, limits) => snapshot(mode, {origins}, limits))({SNAPSHOT_JS}))"
            ),
            function: format!(
                "(globalThis.__fastbrowseScripts?.[{key}] ?? (() => {{ throw new Error({}); }})())",
                py_json(&json!(SNAPSHOT_UNINSTALLED))
            ),
        }
    }
}
/// DuckDuckGo's autoconsent (MPL-2.0, unmodified): refuses consent banners on known platforms and hides them
/// before they paint, so neither the agent's steps nor a recording are spent on one.
const REFUSE_COOKIES_JS: &str = include_str!("../../../src/fastbrowse/browser/autoconsent/autoconsent.standalone.js");

/// The isolated world the input watch runs in: the page's own scripts cannot see a global, a listener or a binding
/// there, and a binding or listener in the page's world is a fingerprint any site can read.
pub const INPUT_WORLD: &str = "fastbrowse-input";
/// The binding the input watch calls, from its isolated world only, when the run's own input moved the page.
pub const INPUT_NAVIGATION_BINDING: &str = "__fastbrowseInputNavigation";
/// Whether the run's own input moved the page. A trusted input event's task, its promise reactions included, and
/// the task after it are the run's: a form's submit() is planned as a task of its own, queued after the first
/// timer. A navigation the Navigation API reports in that window, or one the user agent began for a link or form
/// itself, to another document or on the same one, is reported through the binding, which outlives the document. A
/// page that moves later, on a timer or after a fetch, is not: a script that set its location after a delay was once
/// read as the move of whatever click came before it. DOM events and the navigate event reach an isolated world's
/// listeners too. Code's own events are not trusted, so code opens the window around them, from this world only: a
/// page that could open it could pass its own move off as the run's. A window left open closes itself.
pub const INPUT_HISTORY_JS: &str = concat!(
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

/// How long a click that is opening a window waits for Chrome to announce its tab. In ten runs of a link opening a
/// new window, the tab was announced from 0.08 to 2.5 seconds after the click, and the one a loaded machine
/// announced later than the settle after the click was never followed: the run read the page that opened it.
pub const POPUP_ANNOUNCE_SECONDS: f64 = 5.0;
/// The isolated world a tab's HTTP status is read in, out of reach of the page's scripts.
pub const STATUS_WORLD: &str = "fastbrowse-status";

const ENABLE_DOMAINS: [&str; 3] = ["Page", "Runtime", "DOM"];
const DOWNLOAD_TEXT_BYTES: usize = 16_384;
const DOWNLOAD_TEXT_TOTAL: usize = 65_536;

/// Response-stage interception is enough: fastbrowse only needs the bytes of a save-as download, never to rewrite a
/// request. Chrome also classifies download-attribute anchors as Document; intercepting Other needlessly pauses
/// favicons. Browser download events only expose paths on the browser's filesystem, so they cannot deliver remote
/// file bytes and changing context-wide behavior would affect unowned tabs.
fn download_pattern() -> Value {
    json!({"urlPattern": "*", "resourceType": "Document", "requestStage": "Response"})
}

/// A scoped run also pauses each document request before it is sent, to refuse one outside the grant. Redirect
/// hops, iframes and popups all pause as Documents, so one check covers them.
fn scope_pattern() -> Value {
    json!({"urlPattern": "*", "resourceType": "Document", "requestStage": "Request"})
}

fn keep_painting() -> Value {
    json!({"format": "jpeg", "quality": 1, "maxWidth": 16, "maxHeight": 16, "everyNthFrame": 1000})
}

/// The error a caller's `check_access` returns; the session reports only that it refused, never its message.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A `check_access` refusal that names the exception type a host language raised, which the report may repeat.
#[derive(Debug)]
pub struct AccessRefused(pub String);

impl std::fmt::Display for AccessRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AccessRefused {}
/// `check_access`: the caller's live permission, awaited before browser startup and every read and action.
pub type CheckAccess = Arc<dyn Fn() -> BoxFuture<'static, Result<(), BoxError>> + Send + Sync>;
/// Sees every `Page.screencastFrame` after the session has handled it, as a recording does.
pub type FrameTap = Arc<dyn Fn(&Event) + Send + Sync>;

/// `check_browser_access`: check a caller's live permission without exposing its error's details.
pub async fn check_browser_access(check: Option<&CheckAccess>) -> Result<(), BrowserError> {
    let Some(check) = check else { return Ok(()) };
    check().await.map_err(|error| {
        let kind = error.downcast_ref::<AccessRefused>().map_or("Exception", |refused| refused.0.as_str());
        BrowserError::Failed(format!("check_access refused the browser ({kind}, caller supplied)"))
    })
}

pub struct SessionOptions {
    pub max_download_bytes: usize,
    pub refuse_cookie_banners: bool,
    pub on_frame: Option<FrameHandler>,
    pub check_access: Option<CheckAccess>,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self { max_download_bytes: 200 * 1024 * 1024, refuse_cookie_banners: true, on_frame: None, check_access: None }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TabState {
    pub(crate) target_id: String,
    pub(crate) session_id: String,
    pub(crate) url: String,
    pub(crate) title: String,
    pub(crate) opener_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadText {
    pub mime: String,
    pub name: String,
    pub url: String,
    pub text: String,
    pub complete: bool,
}

#[derive(Clone)]
pub(crate) struct Frame {
    pub(crate) data: String,
    pub(crate) ack_id: Value,
    pub(crate) session_id: Option<String>,
}

pub(crate) struct Popup {
    pub(crate) target_id: String,
    pub(crate) opener_id: String,
    pub(crate) adopted: watch::Sender<Option<bool>>,
}

#[derive(Default)]
pub(crate) struct State {
    /// Insertion ordered, as Python's dict: `tabs()` lists them in this order and a closed active tab hands over to
    /// the first.
    pub(crate) tabs: Vec<TabState>,
    pub(crate) owned: HashSet<String>,
    /// Tab target id -> the session whose cast keeps that tab painting, in a background run.
    pub(crate) keepers: HashMap<String, String>,
    /// Targets already open when an attached session began, which never join the run.
    pub(crate) open_before: HashSet<String>,
    /// Popup target id -> (opener target id, whether it was adopted), in the order they opened.
    pub(crate) popups: Vec<Popup>,
    /// `Page.windowOpen` events from the run's pages: each one is a window on its way, announced or not.
    pub(crate) window_opens: u32,
    /// (session, context id) -> unique id, for each world named `STATUS_WORLD` Chrome reported creating.
    pub(crate) status_contexts: HashMap<(String, i64), String>,
    pub(crate) active_target_id: String,
    /// Set while the page may show a secret: frames are still acked, so the cast keeps pace, but not delivered.
    pub(crate) frames_withheld: bool,
    pub(crate) screencast_session_id: Option<String>,
    pub(crate) pending_frame: Option<Frame>,
    pub(crate) frame_scheduled: bool,
    pub(crate) delivery: Option<JoinHandle<()>>,
    /// OOPIF frame_id -> session_id, in attach order, keyed by target id.
    pub(crate) frame_sessions: Vec<(String, String)>,
    pub(crate) frame_parents: HashMap<String, String>,
    pub(crate) artifacts: Vec<Artifact>,
    /// Captured downloads by count: a navigation that aborts on one is not a navigation that failed.
    pub(crate) downloads_captured: u32,
    /// A captured download's readable text, oldest first: kept as evidence for a capture, not consumed.
    pub(crate) download_texts: Vec<DownloadText>,
    /// How many of them an observation has already shown, so each is shown to the run once.
    pub(crate) download_texts_shown: usize,
    /// Pending JS dialog per tab session id; cleared once handled.
    pub(crate) dialogs: HashMap<String, Dialog>,
    /// The address of the document each pending dialog came from, which `Dialog` does not carry.
    pub(crate) dialog_urls: HashMap<String, String>,
    pub(crate) input_navigations: u32,
    /// Document requests the grant refused, so a navigation that was cut short is told from one that failed.
    pub(crate) denials: u32,
    /// Requests paused and not yet released, so closing can release what a stopped task left paused.
    pub(crate) paused: HashSet<(String, String)>,
    pub(crate) background: Vec<JoinHandle<()>>,
    pub(crate) closing: bool,
    pub(crate) frame_tap: Option<FrameTap>,
}

pub(crate) struct Shared {
    pub(crate) connection: BrowserConnection,
    pub(crate) check_access: Option<CheckAccess>,
    pub(crate) grant: Option<OriginGrant>,
    pub(crate) origins: Vec<String>,
    pub(crate) artifact_sink: Arc<dyn ArtifactSink>,
    pub(crate) max_download_bytes: usize,
    pub(crate) new_document_scripts: Vec<String>,
    pub(crate) on_frame: Option<FrameHandler>,
    pub(crate) client: Mutex<Option<Cdp>>,
    pub(crate) state: Mutex<State>,
    pub(crate) dialog_opened: watch::Sender<u64>,
    /// Bumped on every `Page.lifecycleEvent`: the page's own signal that it is still loading or has settled, so a
    /// settle waits on the page rather than a fixed interval.
    pub(crate) lifecycle: watch::Sender<u64>,
    pub(crate) snapshot: SnapshotScript,
    /// Bumped each time a popup is begun, so a wait for one wakes as it is announced.
    pub(crate) popup_signal: watch::Sender<u64>,
    pub(crate) screencast_lock: tokio::sync::Mutex<()>,
}

/// Async handle over one owned tab (plus any popups it opens). Closes only the targets it owns; the caller's browser
/// and any of its other tabs are never touched. Holds no global state, so many sessions can run in one process.
#[derive(Clone)]
pub struct BrowserSession(pub(crate) Arc<Shared>);

/// Closes a session whose `open` was dropped before it finished.
struct OpenGuard(Option<BrowserSession>);

impl Drop for OpenGuard {
    fn drop(&mut self) {
        if let Some(session) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = session.close().await;
            });
        }
    }
}

impl BrowserSession {
    pub fn new(connection: BrowserConnection, artifact_sink: Arc<dyn ArtifactSink>, options: SessionOptions) -> Self {
        let grant = connection.allowed_origins.clone().map(OriginGrant::new);
        let mut origins = connection.allowed_origins.clone().unwrap_or_default();
        origins.sort();
        let snapshot = SnapshotScript::new(&match &grant {
            None => "null".to_owned(),
            Some(_) => py_json(&json!(origins)),
        });
        let mut new_document_scripts = vec![format!("{}('fingerprint')", snapshot.install)];
        if options.refuse_cookie_banners {
            new_document_scripts.push(REFUSE_COOKIES_JS.to_owned());
        }
        // A scoped run casts nothing: a frame is pixels of whatever the tab shows, and the browser's reports of where
        // it is can trail the screen, so no ordering of events makes a delivered frame provably in the grant.
        let on_frame = if grant.is_some() { None } else { options.on_frame };
        Self(Arc::new(Shared {
            connection,
            check_access: options.check_access,
            grant,
            origins,
            artifact_sink,
            max_download_bytes: options.max_download_bytes,
            new_document_scripts,
            on_frame,
            client: Mutex::new(None),
            state: Mutex::default(),
            dialog_opened: watch::channel(0).0,
            lifecycle: watch::channel(0).0,
            snapshot,
            popup_signal: watch::channel(0).0,
            screencast_lock: tokio::sync::Mutex::new(()),
        }))
    }

    /// snapshot.js as this session's documents install it.
    pub fn snapshot(&self) -> &SnapshotScript {
        &self.0.snapshot
    }

    /// The current lifecycle generation: pass one back to `wait_for_lifecycle` to wait for the next event.
    pub fn lifecycle_mark(&self) -> u64 {
        *self.0.lifecycle.borrow()
    }

    /// Wait for a lifecycle event newer than `mark`, returning whether one arrived before `seconds`.
    ///
    /// A timeout is not a failure: the caller re-reads the page and decides. This replaces the fixed-interval poll a
    /// settle used to run, so the wait costs nothing while the page reports nothing new. The generation counts every
    /// tab's events: a wake for another tab only makes a waiter re-check the page, which is what it would do anyway.
    pub async fn wait_for_lifecycle(&self, mark: u64, seconds: f64) -> bool {
        let mut generation = self.0.lifecycle.subscribe();
        if *generation.borrow_and_update() != mark {
            return true;
        }
        if seconds <= 0.0 {
            return false;
        }
        matches!(timeout(secs(seconds), generation.changed()).await, Ok(Ok(())))
    }

    /// `__aenter__`: connect, then open the run's tab or attach to the window asked for.
    pub async fn open(&self) -> Result<(), BrowserError> {
        self.check_access().await?;
        self.0.state.lock().unwrap().closing = false;
        let client = Cdp::connect(&self.0.connection.cdp_url).await?;
        let weak = Arc::downgrade(&self.0);
        client.on_event(Box::new(move |event| {
            if let Some(shared) = weak.upgrade() {
                Shared::on_event(&shared, event);
            }
        }));
        *self.0.client.lock().unwrap() = Some(client);
        // A caller that gives up while the tab is being set up (a run's time budget) drops this future; the tab it may
        // already own and the socket are then closed in the background, as Python's `except BaseException` closes them.
        let mut guard = OpenGuard(Some(self.clone()));
        let opened = async {
            self.check_access().await?;
            if self.0.connection.attach {
                // Turning discovery on replays targetCreated for every window already open, so it waits until those
                // are known and can be told apart from windows opened during the run.
                self.attach_existing_tab(self.0.connection.target_match.as_deref()).await?;
                self.send("Target.setDiscoverTargets", json!({"discover": true}), None).await?;
            } else {
                // Discovery has to be on only before the tab can open a popup, which it cannot do while it is still
                // being created, so the two are sent at once rather than paying a cloud round trip for each.
                let discover = self.send("Target.setDiscoverTargets", json!({"discover": true}), None);
                together(vec![
                    Box::pin(async { discover.await.map(drop) }),
                    Box::pin(self.open_owned_tab("about:blank")),
                ])
                .await?;
            }
            if self.0.grant.is_some() {
                // A popup's first request can leave before a session attached after targetCreated has Fetch on, so a
                // scoped run has every new page attach paused and resumes it only once its Fetch is enabled.
                self.send(
                    "Target.setAutoAttach",
                    json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true,
                        "filter": [{"type": "page", "exclude": false}, {"exclude": true}]}),
                    None,
                )
                .await?;
            }
            Ok::<(), BrowserError>(())
        }
        .await;
        guard.0 = None;
        if let Err(error) = opened {
            let _ = self.close().await;
            return Err(error);
        }
        Ok(())
    }

    /// `_close`: stop background work, release paused requests, stop casts, close owned tabs, close the socket.
    pub async fn close(&self) -> Result<(), BrowserError> {
        let (background, delivery, paused) = {
            let mut state = self.0.state.lock().unwrap();
            state.closing = true;
            let delivery = state.delivery.take();
            (std::mem::take(&mut state.background), delivery, std::mem::take(&mut state.paused))
        };
        for task in &background {
            task.abort();
        }
        for task in background {
            let _ = task.await;
        }
        if let Some(delivery) = delivery {
            delivery.abort();
            if timeout(secs(FRAME_RELEASE_SECONDS), delivery).await.is_err() {
                warn("The frame callback ignored cancellation; closing without it");
            }
        }
        {
            // What the stopped delivery's `finally` would have reset, so a session opened again delivers frames.
            let mut state = self.0.state.lock().unwrap();
            state.frame_scheduled = false;
            state.pending_frame = None;
        }
        // Download finalizers still need the socket to release intercepted requests; a stopped one is released here.
        if let Some(client) = self.client_opt() {
            for (request_id, session_id) in paused {
                let _ = client.send("Fetch.continueRequest", json!({"requestId": request_id}), Some(&session_id)).await;
            }
        }
        let cast = self.0.state.lock().unwrap().screencast_session_id.take();
        if let Some(cast) = cast {
            self.screencast_command("Page.stopScreencast", json!({}), Some(&cast)).await;
        }
        if self.0.grant.is_some() {
            let kept: Vec<String> = {
                let state = self.0.state.lock().unwrap();
                state
                    .tabs
                    .iter()
                    .filter(|t| !state.owned.contains(&t.target_id))
                    .map(|t| t.session_id.clone())
                    .collect()
            };
            // A window the user keeps gets its service workers back.
            for session_id in kept {
                let _ = self.bypass_service_workers(&session_id, false).await;
            }
        }
        let owned: Vec<String> = self.0.state.lock().unwrap().owned.iter().cloned().collect();
        for target_id in owned {
            // Best-effort teardown; the browser may already be gone.
            let _ = self.send("Target.closeTarget", json!({"targetId": target_id}), None).await;
        }
        let client = self.0.client.lock().unwrap().take();
        if let Some(client) = client {
            client.close();
            let mut state = self.0.state.lock().unwrap();
            state.owned.clear();
            state.tabs.clear();
            state.keepers.clear();
        }
        Ok(())
    }

    // -- accessors -------------------------------------------------------------------------------------------

    /// Always true in a scoped run, which delivers and records no frames at all.
    pub fn frames_withheld(&self) -> bool {
        self.0.state.lock().unwrap().frames_withheld || self.0.grant.is_some()
    }

    pub fn set_frames_withheld(&self, withheld: bool) {
        self.0.state.lock().unwrap().frames_withheld = withheld;
    }

    pub fn grant(&self) -> Option<&OriginGrant> {
        self.0.grant.as_ref()
    }

    /// The grant's origins, sorted, as the page scripts are told them.
    pub fn grant_origins(&self) -> Option<&[String]> {
        self.0.grant.as_ref().map(|_| self.0.origins.as_slice())
    }

    pub fn connection(&self) -> &BrowserConnection {
        &self.0.connection
    }

    pub fn has_frame_handler(&self) -> bool {
        self.0.on_frame.is_some()
    }

    pub fn set_frame_tap(&self, tap: Option<FrameTap>) {
        self.0.state.lock().unwrap().frame_tap = tap;
    }

    /// Ask the caller whether the run may still touch the browser, failing closed on any error it raises.
    pub async fn check_access(&self) -> Result<(), BrowserError> {
        check_browser_access(self.0.check_access.as_ref()).await
    }

    /// Refuse revoked callers and active documents outside the exact-origin grant.
    pub async fn assert_clear(&self) -> Result<(), BrowserError> {
        self.check_access().await?;
        let Some(grant) = &self.0.grant else { return Ok(()) };
        let target = self.active_target_id();
        let info = self.send("Target.getTargetInfo", json!({"targetId": target}), None).await?;
        if !grant.allows(info["targetInfo"]["url"].as_str().unwrap_or_default()) {
            return Err(BrowserError::OriginNotAllowed);
        }
        Ok(())
    }

    fn client_opt(&self) -> Option<Cdp> {
        self.0.client.lock().unwrap().clone()
    }

    pub fn client(&self) -> Result<Cdp, BrowserError> {
        self.client_opt().ok_or_else(|| BrowserError::Failed("CDP.client failed (SessionClosed)".into()))
    }

    /// A command through the client's deadlines.
    pub async fn send(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value, BrowserError> {
        self.client()?.send(method, params, session).await
    }

    pub fn active_target_id(&self) -> String {
        self.0.state.lock().unwrap().active_target_id.clone()
    }

    pub fn active_session_id(&self) -> Result<String, BrowserError> {
        self.0.state.lock().unwrap().active_session_id()
    }

    /// Only frame sessions descended from the active tab may contribute observations.
    pub fn frame_sessions(&self) -> Vec<(String, String)> {
        self.0.state.lock().unwrap().frame_sessions_of_active()
    }

    /// `frame_id` and every frame drawn inside it, whose sessions descend from that frame's session.
    pub fn frame_family(&self, frame_id: &str) -> HashSet<String> {
        let state = self.0.state.lock().unwrap();
        let Some(root) = state.frame_sessions.iter().find(|(f, _)| f == frame_id).map(|(_, s)| s.clone()) else {
            return HashSet::new();
        };
        let inside = |session_id: &str| {
            let mut session_id = session_id.to_owned();
            let mut seen = HashSet::new();
            while seen.insert(session_id.clone()) {
                if session_id == root {
                    return true;
                }
                match state.frame_parents.get(&session_id) {
                    Some(parent) => session_id.clone_from(parent),
                    None => return false,
                }
            }
            false
        };
        state.frame_sessions_of_active().into_iter().filter(|(_, sid)| inside(sid)).map(|(fid, _)| fid).collect()
    }

    pub fn artifacts(&self) -> Vec<Artifact> {
        self.0.state.lock().unwrap().artifacts.clone()
    }

    pub fn downloads_captured(&self) -> u32 {
        self.0.state.lock().unwrap().downloads_captured
    }

    pub fn denials(&self) -> u32 {
        self.0.state.lock().unwrap().denials
    }

    pub fn window_opens(&self) -> u32 {
        self.0.state.lock().unwrap().window_opens
    }

    /// Whether a download landed since `since`, waiting briefly for the intercept's own task.
    ///
    /// The capture runs off the request-paused event in its own task, so it can complete a moment after the
    /// navigation it belongs to aborts. A short bounded wait tells a download abort from a plain failure without
    /// racing that task; a download that never came is not this kind of abort.
    pub async fn download_settled(&self, since: u32, timeout_seconds: f64) -> bool {
        let deadline = Instant::now() + secs(timeout_seconds);
        while self.downloads_captured() <= since {
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        true
    }

    /// Every readable download captured so far, oldest first. Kept rather than consumed: an observation shows each
    /// download once, but a capture has to be able to record it as evidence long after.
    pub fn download_texts(&self) -> Vec<DownloadText> {
        self.0.state.lock().unwrap().download_texts.clone()
    }

    /// Readable downloads the run has not been shown yet, and now marked as shown.
    pub fn pending_download_texts(&self) -> Vec<DownloadText> {
        let mut state = self.0.state.lock().unwrap();
        let pending = state.download_texts[state.download_texts_shown.min(state.download_texts.len())..].to_vec();
        state.download_texts_shown = state.download_texts.len();
        pending
    }

    pub fn frame_parent_session(&self, session_id: &str) -> Option<String> {
        self.0.state.lock().unwrap().frame_parents.get(session_id).cloned()
    }

    pub fn tabs(&self) -> Vec<Tab> {
        let state = self.0.state.lock().unwrap();
        let granted = |t: &TabState| self.0.grant.as_ref().is_none_or(|g| g.allows(&t.url));
        state
            .tabs
            .iter()
            .map(|t| Tab {
                id: t.target_id.clone(),
                url: if granted(t) { t.url.clone() } else { "about:blank".into() },
                title: if granted(t) { t.title.clone() } else { String::new() },
                active: t.target_id == state.active_target_id,
                opener_id: t.opener_id.clone(),
            })
            .collect()
    }

    pub fn set_tab_info(&self, target_id: &str, url: &str, title: &str) {
        self.0.state.lock().unwrap().set_tab_info(target_id, url, title);
    }

    pub fn pending_dialog(&self) -> Option<Dialog> {
        self.0.state.lock().unwrap().pending_dialog().map(|(_, d)| d)
    }

    /// The address of the document the pending dialog came from, if one is pending.
    pub fn pending_dialog_url(&self) -> Option<String> {
        let state = self.0.state.lock().unwrap();
        let (session_id, _) = state.pending_dialog()?;
        state.dialog_urls.get(&session_id).cloned()
    }

    /// Resolves once a dialog is pending on the active tab or one of its frames. Never misses one: the signal is
    /// marked seen before the state is read.
    pub async fn wait_for_dialog(&self) {
        let mut opened = self.0.dialog_opened.subscribe();
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

    /// How many navigations to another document the run's own input has begun, on any page, so far.
    pub fn input_navigations(&self) -> u32 {
        self.0.state.lock().unwrap().input_navigations
    }

    /// The unique id of the status world `context_id` names on `session_id`, once, if Chrome reported it.
    pub fn status_context(&self, session_id: &str, context_id: i64) -> Option<String> {
        self.0.state.lock().unwrap().status_contexts.remove(&(session_id.to_owned(), context_id))
    }

    pub fn popups(&self) -> Vec<String> {
        self.0.state.lock().unwrap().popups.iter().map(|p| p.target_id.clone()).collect()
    }

    // -- tabs ------------------------------------------------------------------------------------------------

    pub async fn switch_tab(&self, target_id: &str) -> Result<(), BrowserError> {
        if !self.0.state.lock().unwrap().tabs.iter().any(|t| t.target_id == target_id) {
            return Err(BrowserError::Failed(format!("Unknown tab {target_id}")));
        }
        Shared::set_active_target(&self.0, target_id);
        self.bring_to_front().await
    }

    /// Put the active tab in front where the run may. A background run sends nothing: its tabs already render where
    /// they are, and Chrome raises and focuses the whole window along with an activated tab.
    pub async fn bring_to_front(&self) -> Result<(), BrowserError> {
        self.activate(&self.active_target_id()).await.map(drop)
    }

    /// Activate a tab when the run may bring it forward, and say whether it did.
    async fn activate(&self, target_id: &str) -> Result<bool, BrowserError> {
        if self.0.connection.foreground {
            self.send("Target.activateTarget", json!({"targetId": target_id}), None).await?;
        }
        Ok(self.0.connection.foreground)
    }

    /// Have a tab that joins the run render: in front of its window, or behind it and kept painting.
    pub(crate) async fn show(&self, target_id: &str) -> Result<(), BrowserError> {
        if self.activate(target_id).await? {
            return Ok(());
        }
        // Focus emulation alone leaves a tab behind another with a compositor that sleeps between repaints, where a
        // screenshot of an idle page waits seconds for a frame. Chrome keeps a tab painting while it is being cast, so
        // a cast too small and too sparse to cost anything runs on a session of its own, where the casts that deliver
        // live frames and recordings cannot replace it. A start can go unanswered while a tab swaps renderer, and a
        // tab without the cast is only slower to capture, so it is bounded and never fails the run.
        let attach = self.send("Target.attachToTarget", json!({"targetId": target_id, "flatten": true}), None);
        let keeper = match timeout(secs(SCREENCAST_COMMAND_SECONDS), attach).await {
            Ok(Ok(keeper)) => str_of(&keeper["sessionId"]),
            Ok(Err(error)) => {
                warn(&format!("Target.attachToTarget for the paint keeper failed ({})", error.kind()));
                return Ok(());
            }
            Err(_) => {
                warn("Target.attachToTarget for the paint keeper failed (TimeoutError)");
                return Ok(());
            }
        };
        self.0.state.lock().unwrap().keepers.insert(target_id.to_owned(), keeper.clone());
        self.screencast_command("Page.startScreencast", keep_painting(), Some(&keeper)).await;
        Ok(())
    }

    /// Get the active tab painting again when a capture of it stalls. True when the tab was brought to the front,
    /// which always yields a frame; a background run restarts the cast that keeps it painting instead, which may have
    /// gone unanswered or ended with the renderer it was started on.
    pub async fn wake(&self) -> Result<bool, BrowserError> {
        let active = self.active_target_id();
        if self.activate(&active).await? {
            return Ok(true);
        }
        let keeper = self.0.state.lock().unwrap().keepers.get(&active).cloned();
        if let Some(keeper) = keeper {
            self.screencast_command("Page.startScreencast", keep_painting(), Some(&keeper)).await;
        }
        Ok(false)
    }

    async fn attach_existing_tab(&self, target_match: Option<&str>) -> Result<String, BrowserError> {
        let targets = self.send("Target.getTargets", json!({}), None).await?;
        let targets: Vec<Value> = targets["targetInfos"].as_array().cloned().unwrap_or_default();
        self.0.state.lock().unwrap().open_before = targets.iter().map(|t| str_of(&t["targetId"])).collect();
        // An Electron app or a browser with DevTools open lists the DevTools window as a page too.
        let mut listed: Vec<&Value> = targets.iter().filter(|t| t["type"] == "page").collect();
        if let Some(grant) = &self.0.grant {
            // A window outside the grant is never matched, named or touched: it is not the run's to see.
            listed.retain(|t| grant.allows(t["url"].as_str().unwrap_or_default()));
        }
        let mut pages: Vec<&Value> = listed
            .iter()
            .copied()
            .filter(|t| !t["url"].as_str().unwrap_or_default().starts_with("devtools://"))
            .collect();
        if let Some(needle) = target_match {
            pages.retain(|t| {
                t["url"].as_str().unwrap_or_default().contains(needle)
                    || t["title"].as_str().unwrap_or_default().contains(needle)
            });
        }
        let Some(target) = pages.first() else {
            let seen: Vec<String> = listed
                .iter()
                .map(|t| format!("{} ({})", py_repr(t["title"].as_str().unwrap_or_default()), str_of(&t["url"])))
                .collect();
            let wanted = match target_match {
                None => "no page".to_owned(),
                Some(needle) => format!("no page matching {}", py_repr(needle)),
            };
            let seen = seen.iter().map(|s| py_repr(s)).collect::<Vec<_>>().join(", ");
            return Err(BrowserError::Failed(format!("{wanted} to attach to; pages: [{seen}]")));
        };
        let target_id = str_of(&target["targetId"]);
        let attach = self.send("Target.attachToTarget", json!({"targetId": target_id, "flatten": true}), None).await?;
        let session_id = str_of(&attach["sessionId"]);
        together(vec![Box::pin(self.show(&target_id)), Box::pin(self.prepare_session(&session_id))]).await?;
        self.run_document_scripts(&session_id).await;
        self.0.state.lock().unwrap().insert_tab(TabState {
            target_id: target_id.clone(),
            session_id,
            url: str_of(&target["url"]),
            title: str_of(&target["title"]),
            opener_id: None,
        });
        Shared::set_active_target(&self.0, &target_id);
        Ok(target_id)
    }

    /// A window that was already open loaded its document before the new-document scripts were registered, so
    /// without this its freshness tracking and banner refusal would start only at its next navigation.
    async fn run_document_scripts(&self, session_id: &str) {
        let scripts = self.0.new_document_scripts.iter().map(|source| async move {
            let _ = self.send("Runtime.evaluate", json!({"expression": source}), Some(session_id)).await;
        });
        futures_util::future::join(futures_util::future::join_all(scripts), async {
            let _ = self.watch_input_now(session_id).await;
        })
        .await;
    }

    /// Count a move the page makes while code dispatches its own input events as the run's: a select's or a date
    /// field's change handler that moved the page is the run's doing, though no event it got was trusted.
    pub async fn input_window<T>(&self, session_id: &str, work: impl Future<Output = T>) -> T {
        let mut opened: Option<(i64, Value)> = None;
        if let Ok(context) = self.input_context(session_id).await {
            let timer = self
                .send(
                    "Runtime.evaluate",
                    json!({"expression": "globalThis.__openInput?.()", "contextId": context, "returnByValue": true}),
                    Some(session_id),
                )
                .await
                .map(|out| out["result"]["value"].clone())
                .unwrap_or(Value::Null);
            opened = Some((context, timer));
        }
        let out = work.await;
        if let Some((context, timer)) = opened {
            let expression = format!("globalThis.__closeInput?.({})", py_json(&timer));
            let _ = self
                .send("Runtime.evaluate", json!({"expression": expression, "contextId": context}), Some(session_id))
                .await;
        }
        out
    }

    /// The input watch's isolated world in the session's main frame; asking by name returns the same world.
    async fn input_context(&self, session_id: &str) -> Result<i64, BrowserError> {
        let tree = self.send("Page.getFrameTree", json!({}), Some(session_id)).await?;
        let world = self
            .send(
                "Page.createIsolatedWorld",
                json!({"frameId": tree["frameTree"]["frame"]["id"], "worldName": INPUT_WORLD}),
                Some(session_id),
            )
            .await?;
        Ok(world["executionContextId"].as_i64().unwrap_or_default())
    }

    /// Start the input watch in a document that was already open, in its own isolated world as on a new one.
    async fn watch_input_now(&self, session_id: &str) -> Result<(), BrowserError> {
        let context = self.input_context(session_id).await?;
        self.send("Runtime.evaluate", json!({"expression": INPUT_HISTORY_JS, "contextId": context}), Some(session_id))
            .await
            .map(drop)
    }

    pub(crate) async fn open_owned_tab(&self, url: &str) -> Result<(), BrowserError> {
        // A tab that is neither in front nor focus-emulated does not render: animation frames never fire, so menus
        // that animate open never become visible, screenshots hang and clicks read as covered. A foreground run
        // activates its tab. A background run opens it behind the window's current tab, because Chrome focuses the
        // window of a tab it opens in front, and `prepare_session` turns focus emulation on instead.
        let mut params = json!({"url": url});
        if !self.0.connection.foreground {
            params["background"] = true.into();
        }
        let created = self.send("Target.createTarget", params, None).await?;
        let target_id = str_of(&created["targetId"]);
        self.0.state.lock().unwrap().owned.insert(target_id.clone());
        let attach = self.send("Target.attachToTarget", json!({"targetId": target_id, "flatten": true}), None).await?;
        let session_id = str_of(&attach["sessionId"]);
        // Foregrounding the tab and enabling its session are independent, and both finish before the tab is used.
        together(vec![Box::pin(self.show(&target_id)), Box::pin(self.prepare_session(&session_id))]).await?;
        self.0.state.lock().unwrap().insert_tab(TabState {
            target_id: target_id.clone(),
            session_id,
            url: url.to_owned(),
            title: String::new(),
            opener_id: None,
        });
        Shared::set_active_target(&self.0, &target_id);
        Ok(())
    }

    pub(crate) async fn prepare_session(&self, session_id: &str) -> Result<(), BrowserError> {
        self.check_access().await?;
        let scoped = self.0.grant.is_some();
        let mut patterns = vec![download_pattern()];
        if scoped {
            patterns.push(scope_pattern());
        }
        let s = Some(session_id);
        let mut commands: Vec<FBoxFuture<'_, Result<(), BrowserError>>> = vec![];
        for domain in ENABLE_DOMAINS {
            commands
                .push(Box::pin(async move { self.send(&format!("{domain}.enable"), json!({}), s).await.map(drop) }));
        }
        let sends: Vec<(&str, Value)> = vec![
            // `Page.enable` alone does not emit `Page.lifecycleEvent`: CDP gates those events behind this switch, so
            // without it the settle path's lifecycle waits never wake and each one runs to its own deadline.
            ("Page.setLifecycleEventsEnabled", json!({"enabled": true})),
            ("Runtime.addBinding", json!({"name": INPUT_NAVIGATION_BINDING, "executionContextName": INPUT_WORLD})),
            ("Page.addScriptToEvaluateOnNewDocument", json!({"source": INPUT_HISTORY_JS, "worldName": INPUT_WORLD})),
            ("Target.setAutoAttach", json!({"autoAttach": true, "waitForDebuggerOnStart": scoped, "flatten": true})),
            ("Fetch.enable", json!({"patterns": patterns})),
        ];
        for (method, params) in sends {
            commands.push(Box::pin(async move { self.send(method, params, s).await.map(drop) }));
        }
        // A page behind another tab is hidden and unfocused, so it paints nothing and takes no typing. Focus emulation
        // has it render and hold focus where it is, which is what lets a run leave the window alone.
        if !self.0.connection.foreground {
            commands.push(Box::pin(async move {
                self.send("Emulation.setFocusEmulationEnabled", json!({"enabled": true}), s).await.map(drop)
            }));
        }
        if scoped {
            commands.push(Box::pin(self.bypass_service_workers(session_id, true)));
        }
        // Track parsing and hydration before the first post-navigation read, so an already quiet document does not
        // pay another full window just to install its observer.
        for source in &self.0.new_document_scripts {
            commands.push(Box::pin(async move {
                self.send("Page.addScriptToEvaluateOnNewDocument", json!({"source": source}), s).await.map(drop)
            }));
        }
        together(commands).await?;
        if scoped {
            self.send("Runtime.runIfWaitingForDebugger", json!({}), s).await?;
        }
        Ok(())
    }

    /// A service worker answers a navigation without the request reaching the Fetch gate, so a scoped run sends every
    /// one to the network, where the scope pattern sees it. The flag is the Network agent's, and Chrome ignores it
    /// until that domain is on.
    async fn bypass_service_workers(&self, session_id: &str, bypass: bool) -> Result<(), BrowserError> {
        self.send("Network.enable", json!({}), Some(session_id)).await?;
        self.send("Network.setBypassServiceWorker", json!({"bypass": bypass}), Some(session_id)).await.map(drop)
    }

    /// Make active the latest tab the active tab opened since `known` once it is adopted, and return its id.
    ///
    /// A link that opens its page in a new window left the run on the opener: the next step was spent choosing the
    /// new tab, and the read before it credited the opener's heading to the new window. Adoption is waited out however
    /// long it takes, since each of its commands has its own deadline: a fixed two seconds for it was outlasted on a
    /// loaded machine, and the run read the opener while the tab it had opened sat unused.
    ///
    /// With `expected`, the click is known to be opening a window (it named a new browsing context, or Chrome reported
    /// a window opening since `known` was taken), so a tab not announced yet is waited for, up to
    /// `POPUP_ANNOUNCE_SECONDS`; any other click returns at once when nothing has been announced.
    pub async fn follow_popup(&self, known: &[String], expected: bool) -> Result<Option<String>, BrowserError> {
        let opener = self.active_target_id();
        let deadline = Instant::now() + secs(if expected { POPUP_ANNOUNCE_SECONDS } else { 0.0 });
        let mut signal = self.0.popup_signal.subscribe();
        let (fresh, mut adopted) = loop {
            signal.borrow_and_update();
            let found = {
                let state = self.0.state.lock().unwrap();
                state
                    .popups
                    .iter()
                    .rev()
                    .find(|p| p.opener_id == opener && !known.contains(&p.target_id))
                    .map(|p| (p.target_id.clone(), p.adopted.subscribe()))
            };
            if let Some(found) = found {
                break found;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let _ = timeout(remaining, signal.changed()).await;
        };
        let adopted = adopted.wait_for(Option::is_some).await.map(|v| *v == Some(true)).unwrap_or(false);
        let joined = self.0.state.lock().unwrap().tabs.iter().any(|t| t.target_id == fresh);
        if adopted && joined {
            self.switch_tab(&fresh).await?;
            return Ok(Some(fresh));
        }
        Ok(None)
    }

    async fn screencast_command(&self, method: &str, params: Value, session_id: Option<&str>) {
        screencast_command(&self.0, method, params, session_id).await;
    }

    pub async fn handle_dialog(&self, accept: bool, prompt_text: Option<&str>) -> Result<(), BrowserError> {
        let session_id = {
            let state = self.0.state.lock().unwrap();
            state.pending_dialog().map(|(sid, _)| sid)
        };
        // Python's `next(...)` raises when nothing is pending; a dispatch for a dialog that is gone fails the same way.
        let Some(session_id) = session_id else {
            return Err(BrowserError::Failed("Page.handleJavaScriptDialog failed (StopIteration)".into()));
        };
        let mut params = json!({"accept": accept});
        if let Some(text) = prompt_text {
            params["promptText"] = text.into();
        }
        self.send("Page.handleJavaScriptDialog", params, Some(&session_id)).await?;
        self.0.state.lock().unwrap().dialogs.remove(&session_id);
        Ok(())
    }
}

impl State {
    fn active_session_id(&self) -> Result<String, BrowserError> {
        self.tabs
            .iter()
            .find(|t| t.target_id == self.active_target_id)
            .map(|t| t.session_id.clone())
            .ok_or_else(|| BrowserError::Unavailable("active browser tab closed; no owned tab remains".into()))
    }

    fn frame_sessions_of_active(&self) -> Vec<(String, String)> {
        let Ok(active) = self.active_session_id() else { return vec![] };
        let belongs = |session_id: &str| {
            let mut session_id = session_id.to_owned();
            let mut seen = HashSet::new();
            while let Some(parent) = self.frame_parents.get(&session_id) {
                if !seen.insert(session_id.clone()) {
                    break;
                }
                session_id.clone_from(parent);
            }
            session_id == active
        };
        self.frame_sessions.iter().filter(|(_, sid)| belongs(sid)).cloned().collect()
    }

    fn pending_dialog(&self) -> Option<(String, Dialog)> {
        let active = self.active_session_id().ok()?;
        std::iter::once(active)
            .chain(self.frame_sessions_of_active().into_iter().map(|(_, sid)| sid))
            .find_map(|sid| self.dialogs.get(&sid).map(|d| (sid.clone(), d.clone())))
    }

    fn set_tab_info(&mut self, target_id: &str, url: &str, title: &str) {
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.target_id == target_id) {
            tab.url = url.to_owned();
            tab.title = title.to_owned();
        }
    }

    fn has_tab(&self, target_id: &str) -> bool {
        self.tabs.iter().any(|t| t.target_id == target_id)
    }

    /// `self._tabs[id] = state`: a known tab keeps its place.
    fn insert_tab(&mut self, tab: TabState) {
        match self.tabs.iter_mut().find(|t| t.target_id == tab.target_id) {
            Some(existing) => *existing = tab,
            None => self.tabs.push(tab),
        }
    }
}

impl Shared {
    fn session(self: &Arc<Self>) -> BrowserSession {
        BrowserSession(self.clone())
    }

    pub(crate) fn spawn(
        self: &Arc<Self>,
        state: &mut State,
        work: impl Future<Output = ()> + Send + 'static,
    ) -> Option<()> {
        if state.closing {
            return None;
        }
        state.background.retain(|task| !task.is_finished());
        state.background.push(tokio::spawn(work));
        Some(())
    }

    fn set_active_target(self: &Arc<Self>, target_id: &str) {
        let mut state = self.state.lock().unwrap();
        state.active_target_id = target_id.to_owned();
        if let Some(frame) = state.pending_frame.take() {
            self.ack(&mut state, frame);
        }
        if self.on_frame.is_some() {
            let shared = self.clone();
            self.spawn(&mut state, async move { update_screencast(&shared).await });
        }
    }

    pub(crate) fn on_event(self: &Arc<Self>, event: &Event) {
        let params = &event.params;
        let session = event.session.as_deref();
        match event.method.as_str() {
            "Runtime.executionContextCreated" => {
                let context = &params["context"];
                if let Some(session) = session
                    && context["name"] == STATUS_WORLD
                {
                    let id = context["id"].as_i64().unwrap_or_default();
                    let unique = str_of(&context["uniqueId"]);
                    self.state.lock().unwrap().status_contexts.insert((session.to_owned(), id), unique);
                }
            }
            "Page.windowOpen" => self.state.lock().unwrap().window_opens += 1,
            // A frame reported a lifecycle transition (init, DOMContentLoaded, load, networkIdle...).
            "Page.lifecycleEvent" => self.lifecycle.send_modify(|generation| *generation += 1),
            "Target.attachedToTarget" => self.on_attached(params, session),
            "Target.detachedFromTarget" => {
                let child = str_of(&params["sessionId"]);
                let mut state = self.state.lock().unwrap();
                state.frame_parents.remove(&child);
                state.frame_sessions.retain(|(_, sid)| *sid != child);
            }
            // A scoped run adopts popups where they attach paused, not here, after their first request may have left.
            "Target.targetCreated" if self.grant.is_none() => {
                let info = &params["targetInfo"];
                let mut state = self.state.lock().unwrap();
                if let Some(opener) = self.popup_opener(&state, info) {
                    self.begin_popup(&mut state, &str_of(&info["targetId"]), &opener, None);
                }
            }
            "Target.targetInfoChanged" => {
                let info = &params["targetInfo"];
                let target_id = str_of(&info["targetId"]);
                let mut state = self.state.lock().unwrap();
                if state.has_tab(&target_id) {
                    state.set_tab_info(
                        &target_id,
                        info["url"].as_str().unwrap_or_default(),
                        info["title"].as_str().unwrap_or_default(),
                    );
                } else if self.grant.is_none()
                    && info["openerId"].as_str().is_some_and(|o| !o.is_empty())
                    && !state.popups.iter().any(|p| p.target_id == target_id)
                    && let Some(opener) = self.popup_opener(&state, info)
                {
                    // An opener Chrome reports only after the target was created makes it the run's popup from then on.
                    self.begin_popup(&mut state, &target_id, &opener, None);
                }
            }
            "Target.targetDestroyed" => {
                let target_id = str_of(&params["targetId"]);
                let next = {
                    let mut state = self.state.lock().unwrap();
                    state.tabs.retain(|t| t.target_id != target_id);
                    state.keepers.remove(&target_id);
                    state.owned.remove(&target_id);
                    (target_id == state.active_target_id)
                        .then(|| state.tabs.first().map(|t| t.target_id.clone()).unwrap_or_default())
                };
                if let Some(next) = next {
                    self.set_active_target(&next);
                }
            }
            "Page.javascriptDialogOpening" => {
                let Some(session) = session else { return };
                let dialog = Dialog {
                    kind: str_of(&params["type"]),
                    message: str_of(&params["message"]),
                    default_prompt: params["defaultPrompt"].as_str().map(Into::into),
                };
                {
                    let mut state = self.state.lock().unwrap();
                    state.dialogs.insert(session.to_owned(), dialog);
                    state.dialog_urls.insert(session.to_owned(), str_of(&params["url"]));
                }
                self.dialog_opened.send_modify(|n| *n += 1);
            }
            "Runtime.bindingCalled" if params["name"] == INPUT_NAVIGATION_BINDING => {
                self.state.lock().unwrap().input_navigations += 1;
            }
            "Page.javascriptDialogClosed" => {
                // Navigation can dismiss a dialog without going through handle_dialog.
                if let Some(session) = session {
                    let mut state = self.state.lock().unwrap();
                    state.dialogs.remove(session);
                    state.dialog_urls.remove(session);
                }
            }
            "Page.screencastFrame" => self.on_screencast_frame(event),
            "Fetch.requestPaused" => self.on_request_paused(params, session),
            _ => {}
        }
    }

    fn on_attached(self: &Arc<Self>, params: &Value, session: Option<&str>) {
        let info = &params["targetInfo"];
        let child = str_of(&params["sessionId"]);
        let paused = params["waitingForDebugger"] == true;
        let mut state = self.state.lock().unwrap();
        match session {
            None => {
                if self.grant.is_some() && paused {
                    self.on_new_page(&mut state, info, &child);
                }
            }
            Some(parent) if info["type"] == "iframe" => {
                let target = str_of(&info["targetId"]);
                match state.frame_sessions.iter_mut().find(|(t, _)| *t == target) {
                    Some(entry) => entry.1 = child.clone(),
                    None => state.frame_sessions.push((target, child.clone())),
                }
                state.frame_parents.insert(child.clone(), parent.to_owned());
                let session = self.session();
                self.spawn(&mut state, async move {
                    let _ = session.prepare_session(&child).await;
                });
            }
            Some(_) if self.grant.is_some() && paused => {
                // A worker has no document to scope.
                let session = self.session();
                self.spawn(&mut state, async move {
                    let _ = session.send("Runtime.runIfWaitingForDebugger", json!({}), Some(&child)).await;
                });
            }
            Some(_) => {}
        }
    }

    /// A page that opened since a scoped run asked for them paused. Sessions this run attached itself, and windows
    /// open before it asked, arrive with nothing to wait on and are left as they are.
    fn on_new_page(self: &Arc<Self>, state: &mut State, info: &Value, child: &str) {
        match self.popup_opener(state, info) {
            Some(opener) => self.begin_popup(state, &str_of(&info["targetId"]), &opener, Some(child.to_owned())),
            None => {
                // Not the run's page, as a tab a person opened beside an attached window: let it start, and let go.
                let session = self.session();
                let child = child.to_owned();
                self.spawn(state, async move {
                    if session.send("Runtime.runIfWaitingForDebugger", json!({}), Some(&child)).await.is_ok() {
                        let _ = session.send("Target.detachFromTarget", json!({"sessionId": child}), None).await;
                    }
                });
            }
        }
    }

    /// The tab a new page belongs to, when the run adopts it.
    fn popup_opener(&self, state: &State, info: &Value) -> Option<String> {
        let opener_id = info["openerId"].as_str().filter(|o| !o.is_empty());
        if state.closing || info["type"] != "page" {
            return None;
        }
        if let Some(opener) = opener_id
            && state.owned.contains(opener)
        {
            return Some(opener.to_owned());
        }
        // An attached window is not owned, so neither are its popups: they stay open with it. An Electron app's main
        // process opens windows with no opener, which count as the active window's. A browser opens one too for every
        // tab its user opens, so they are adopted only when a target was matched.
        let target_id = str_of(&info["targetId"]);
        if self.connection.attach
            && !state.has_tab(&target_id)
            && !state.open_before.contains(&target_id)
            && (opener_id.is_some_and(|o| state.has_tab(o))
                || (opener_id.is_none() && self.connection.target_match.is_some()))
        {
            return Some(opener_id.map_or_else(|| state.active_target_id.clone(), str::to_owned));
        }
        None
    }

    pub(crate) fn begin_popup(
        self: &Arc<Self>,
        state: &mut State,
        target_id: &str,
        opener_id: &str,
        session: Option<String>,
    ) {
        if state.popups.iter().any(|p| p.target_id == target_id) {
            return;
        }
        if state.owned.contains(opener_id) {
            state.owned.insert(target_id.to_owned());
        }
        let adopted = watch::channel(None).0;
        state.popups.push(Popup { target_id: target_id.to_owned(), opener_id: opener_id.to_owned(), adopted });
        self.popup_signal.send_modify(|n| *n += 1);
        let shared = self.clone();
        let (target_id, opener_id) = (target_id.to_owned(), opener_id.to_owned());
        let spawned = self.spawn(state, async move { adopt_popup(&shared, &target_id, &opener_id, session).await });
        if spawned.is_none()
            && let Some(popup) = state.popups.last()
        {
            popup.adopted.send_replace(Some(false));
        }
    }

    fn on_screencast_frame(self: &Arc<Self>, event: &Event) {
        let mut state = self.state.lock().unwrap();
        let tap = state.frame_tap.clone();
        // A background run casts without a frame handler too, and a frame nobody acks stalls the cast that sent it.
        if self.on_frame.is_some() || !self.connection.foreground || tap.is_some() {
            let frame = Frame {
                data: str_of(&event.params["data"]),
                ack_id: event.params["sessionId"].clone(),
                session_id: event.session.clone(),
            };
            let active = state.active_session_id().ok();
            if self.on_frame.is_none() || state.closing || active.is_none() || frame.session_id != active {
                self.ack(&mut state, frame);
            } else {
                // Chrome sends the next frame only once this one is acked, so acking after delivery paces the cast to
                // what the consumer can take, with no fixed rate. Latest wins: a frame superseded while one is being
                // delivered is acked unseen rather than dropped unacked, because an unacked frame stalls the cast.
                if let Some(previous) = state.pending_frame.replace(frame) {
                    self.ack(&mut state, previous);
                }
                if !state.frame_scheduled {
                    state.frame_scheduled = true;
                    let shared = self.clone();
                    if state.closing {
                        state.frame_scheduled = false;
                    } else {
                        state.delivery = Some(tokio::spawn(async move { deliver_frames(&shared).await }));
                    }
                }
            }
        }
        drop(state);
        if let Some(tap) = tap {
            tap(event);
        }
    }

    fn ack(self: &Arc<Self>, state: &mut State, frame: Frame) {
        let shared = self.clone();
        self.spawn(state, async move {
            let params = json!({"sessionId": frame.ack_id});
            screencast_command(&shared, "Page.screencastFrameAck", params, frame.session_id.as_deref()).await;
        });
    }

    fn on_request_paused(self: &Arc<Self>, params: &Value, session: Option<&str>) {
        let Some(session) = session else { return };
        let request_id = str_of(&params["requestId"]);
        let mut state = self.state.lock().unwrap();
        let shared = self.clone();
        // A response-stage pause carries a status or an error; a request-stage one is before anything is sent.
        if let Some(grant) = &self.grant
            && params.get("responseStatusCode").is_none()
            && params.get("responseErrorReason").is_none()
        {
            let session = session.to_owned();
            if !grant.allows(params["request"]["url"].as_str().unwrap_or_default()) {
                // Counted here, not in the task, so a navigation that returns next sees it. ERR_ABORTED rather than a
                // block page, which would commit a document that names the address it refused.
                state.denials += 1;
                self.spawn(&mut state, async move {
                    let params = json!({"requestId": request_id, "errorReason": "Aborted"});
                    let _ = shared.session().send("Fetch.failRequest", params, Some(&session)).await;
                });
                return;
            }
            self.spawn(&mut state, async move {
                let _ = shared
                    .session()
                    .send("Fetch.continueRequest", json!({"requestId": request_id}), Some(&session))
                    .await;
            });
            return;
        }
        state.paused.insert((request_id, session.to_owned()));
        let params = params.clone();
        let session = session.to_owned();
        self.spawn(&mut state, async move { handle_paused(&shared, params, session).await });
    }
}

pub(crate) async fn adopt_popup(shared: &Arc<Shared>, target_id: &str, opener_id: &str, session: Option<String>) {
    let session_handle = shared.session();
    let adopt = async {
        let session_id = match session {
            Some(session_id) => session_id,
            None => {
                let attach = session_handle
                    .send("Target.attachToTarget", json!({"targetId": target_id, "flatten": true}), None)
                    .await?;
                str_of(&attach["sessionId"])
            }
        };
        together(vec![Box::pin(session_handle.show(target_id)), Box::pin(session_handle.prepare_session(&session_id))])
            .await?;
        if shared.connection.attach {
            session_handle.run_document_scripts(&session_id).await;
        }
        shared.state.lock().unwrap().insert_tab(TabState {
            target_id: target_id.to_owned(),
            session_id,
            url: "about:blank".into(),
            title: String::new(),
            opener_id: Some(opener_id.to_owned()),
        });
        // The popup may already have navigated to its final URL before this task got scheduled (targetCreated ->
        // targetInfoChanged can both fire while attachToTarget above is awaited), so a targetInfoChanged event carrying
        // it can arrive and be dropped because the tab was not in the tabs yet. Fetch current info now rather than
        // relying on a future event.
        let info = session_handle.send("Target.getTargetInfo", json!({"targetId": target_id}), None).await?;
        let info = &info["targetInfo"];
        session_handle.set_tab_info(
            target_id,
            info["url"].as_str().unwrap_or_default(),
            info["title"].as_str().unwrap_or_default(),
        );
        Ok::<(), BrowserError>(())
    };
    // Settled however this ends, as Python's `finally` does: a session closing mid-adoption drops this future, and a
    // `follow_popup` waiting on the popup must still learn it never joined.
    let _settle = AdoptionSettled { shared, target_id };
    let _ = adopt.await;
}

struct AdoptionSettled<'a> {
    shared: &'a Arc<Shared>,
    target_id: &'a str,
}

impl Drop for AdoptionSettled<'_> {
    fn drop(&mut self) {
        let state = self.shared.state.lock().unwrap();
        let joined = state.has_tab(self.target_id);
        if let Some(popup) = state.popups.iter().find(|p| p.target_id == self.target_id)
            && popup.adopted.borrow().is_none()
        {
            popup.adopted.send_replace(Some(joined));
        }
    }
}

async fn screencast_command(shared: &Arc<Shared>, method: &str, params: Value, session_id: Option<&str>) {
    let Some(client) = shared.client.lock().unwrap().clone() else { return };
    match timeout(secs(SCREENCAST_COMMAND_SECONDS), client.send(method, params, session_id)).await {
        Ok(Ok(_)) => {}
        // Transport and consumer errors may contain credentials or page text.
        Ok(Err(error)) => warn(&format!("{method} failed ({})", error.kind())),
        Err(_) => warn(&format!("{method} failed (TimeoutError)")),
    }
}

async fn update_screencast(shared: &Arc<Shared>) {
    // A tab can switch again while Chrome is answering the previous start or stop.
    let _lock = shared.screencast_lock.lock().await;
    let (session_id, cast, closing) = {
        let state = shared.state.lock().unwrap();
        (state.active_session_id().ok(), state.screencast_session_id.clone(), state.closing)
    };
    if closing || session_id == cast {
        return;
    }
    if let Some(cast) = cast {
        screencast_command(shared, "Page.stopScreencast", json!({}), Some(&cast)).await;
        shared.state.lock().unwrap().screencast_session_id = None;
    }
    let active = {
        let mut state = shared.state.lock().unwrap();
        let active = state.active_session_id().ok().filter(|_| !state.closing);
        state.screencast_session_id.clone_from(&active);
        active
    };
    if let Some(active) = active {
        let params = json!({"format": "jpeg", "quality": 60, "maxWidth": 1280, "maxHeight": 800});
        screencast_command(shared, "Page.startScreencast", params, Some(&active)).await;
    }
}

pub(crate) async fn deliver_frames(shared: &Arc<Shared>) {
    loop {
        let (frame, deliver) = {
            let mut state = shared.state.lock().unwrap();
            if state.closing {
                break;
            }
            let Some(frame) = state.pending_frame.take() else { break };
            let active = state.active_session_id().ok();
            let withheld = state.frames_withheld || shared.grant.is_some();
            let deliver = shared.on_frame.is_some() && !withheld && active.is_some() && frame.session_id == active;
            (frame, deliver)
        };
        let mut failed = false;
        if deliver && let Some(on_frame) = &shared.on_frame {
            match BASE64.decode(frame.data.as_bytes()) {
                Ok(bytes) => on_frame(bytes).await,
                Err(_) => failed = true,
            }
        }
        {
            let mut state = shared.state.lock().unwrap();
            shared.ack(&mut state, frame);
        }
        if failed {
            warn("Live frame delivery failed (Exception)");
            break;
        }
    }
    let mut state = shared.state.lock().unwrap();
    state.frame_scheduled = false;
    if let Some(frame) = state.pending_frame.take() {
        shared.ack(&mut state, frame);
    }
}

async fn handle_paused(shared: &Arc<Shared>, event: Value, session_id: String) {
    let request_id = str_of(&event["requestId"]);
    let mut headers: HashMap<String, String> = HashMap::new();
    for header in event["responseHeaders"].as_array().into_iter().flatten() {
        headers.insert(str_of(&header["name"]).to_lowercase(), str_of(&header["value"]));
    }
    let disposition = headers.get("content-disposition").cloned().unwrap_or_default();
    let mime = headers.get("content-type").cloned().unwrap_or_default();
    if is_download(&mime, &disposition) {
        let complete = event["responseStatusCode"] == 200 && !headers.contains_key("content-range");
        let url = str_of(&event["request"]["url"]);
        let _ = capture_download(shared, &request_id, &session_id, &url, &disposition, &mime, complete).await;
    }
    // Never leave a request paused, even if capture above failed. The request or session may already be gone
    // (navigation, tab close), which is fine to swallow here.
    let _ = shared.session().send("Fetch.continueRequest", json!({"requestId": request_id}), Some(&session_id)).await;
    shared.state.lock().unwrap().paused.remove(&(request_id, session_id));
}

async fn capture_download(
    shared: &Arc<Shared>,
    request_id: &str,
    session_id: &str,
    url: &str,
    disposition: &str,
    mime: &str,
    complete: bool,
) -> Result<(), BoxError> {
    let body =
        shared.session().send("Fetch.getResponseBody", json!({"requestId": request_id}), Some(session_id)).await?;
    let text = body["body"].as_str().unwrap_or_default();
    let raw = if body["base64Encoded"] == true { BASE64.decode(text.as_bytes())? } else { text.as_bytes().to_vec() };
    if raw.len() > shared.max_download_bytes {
        // Bounded size: refuse to hold an oversized body in memory as an artifact.
        return Ok(());
    }
    let name = filename_from_disposition(disposition)
        .or_else(|| url.rsplit('/').next().filter(|s| !s.is_empty()).map(str::to_owned))
        .unwrap_or_else(|| "download".into());
    let kind = response_mime(mime);
    let stored_mime = if kind.is_empty() { "application/octet-stream".to_owned() } else { kind.clone() };
    let readable = readable_text(mime, &raw);
    let size = raw.len();
    let artifact = shared.artifact_sink.put(ArtifactKind::Download, &name, &stored_mime, &raw).await?;
    let mut state = shared.state.lock().unwrap();
    state.artifacts.push(artifact);
    state.downloads_captured += 1;
    if let Some(text) = readable {
        state.download_texts.push(DownloadText {
            mime: kind,
            name,
            url: url.to_owned(),
            text,
            complete: complete && size <= DOWNLOAD_TEXT_BYTES,
        });
        while state.download_texts.iter().map(|d| d.text.chars().count()).sum::<usize>() > DOWNLOAD_TEXT_TOTAL {
            state.download_texts.remove(0);
            state.download_texts_shown = state.download_texts_shown.saturating_sub(1);
        }
    }
    Ok(())
}

/// `_together`: send independent commands at once and wait for all of them, failing with the first error as
/// sequential sends would, and dropping the rest when one fails so none outlives the session it was sent on.
pub async fn together(commands: Vec<FBoxFuture<'_, Result<(), BrowserError>>>) -> Result<(), BrowserError> {
    try_join_all(commands).await.map(drop)
}

fn filename_from_disposition(disposition: &str) -> Option<String> {
    disposition
        .split(';')
        .map(str::trim)
        .find(|part| part.to_lowercase().starts_with("filename="))
        .map(|part| part.split_once('=').map(|(_, v)| v).unwrap_or_default().trim_matches('"').to_owned())
}

/// Chrome hands these to the download manager rather than rendering them as a document, so a document navigation to
/// one is a download and aborts. A `content-disposition: attachment` (or a save-as filename) is the site saying so
/// explicitly; a bare `text/csv` says it by type. This is a property of the response, not of any one site, so a link
/// whose target is a file is captured the same way everywhere.
const DOWNLOAD_MIME_TYPES: [&str; 10] = [
    "text/csv",
    "application/csv",
    "application/octet-stream",
    "application/zip",
    "application/gzip",
    "application/x-gzip",
    "application/x-tar",
    "application/x-7z-compressed",
    "application/vnd.ms-excel",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
];

fn response_mime(mime: &str) -> String {
    mime.split(';').next().unwrap_or_default().trim().to_lowercase()
}

/// Whether this response is a file Chrome downloads rather than a document it renders.
fn is_download(mime: &str, disposition: &str) -> bool {
    let lowered = disposition.to_lowercase();
    if lowered.contains("attachment") || lowered.contains("filename=") {
        return true;
    }
    DOWNLOAD_MIME_TYPES.contains(&response_mime(mime).as_str())
}

/// The response's text when it is a text file the run should be able to read, else None.
fn readable_text(mime: &str, raw: &[u8]) -> Option<String> {
    let kind = response_mime(mime);
    if !(kind.starts_with("text/") || kind == "application/csv" || kind == "application/json") {
        return None;
    }
    let mut text = String::from_utf8_lossy(&raw[..raw.len().min(DOWNLOAD_TEXT_BYTES)]).into_owned();
    if raw.len() > DOWNLOAD_TEXT_BYTES {
        let omitted = raw.len() - DOWNLOAD_TEXT_BYTES;
        text.push_str(&format!("\n[Download cut: {omitted} bytes omitted; full file in artifact]"));
    }
    Some(text)
}

pub fn str_of(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

/// `json.dumps` with Python's default separators (`", "` and `": "`) and `ensure_ascii`, so a script built here is
/// the script Python builds, byte for byte.
pub fn py_json(value: &Value) -> String {
    let mut out = String::new();
    write_py_json(value, &mut out);
    out
}

fn write_py_json(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_py_str(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_py_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_py_str(key, out);
                out.push_str(": ");
                write_py_json(item, out);
            }
            out.push('}');
        }
    }
}

fn write_py_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7f => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Python's `repr` of a str, for messages Python builds with `!r`.
pub fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::new();
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downloads_are_told_by_disposition_or_type() {
        assert!(is_download("text/plain", "attachment; filename=\"a.txt\""));
        assert!(is_download("text/csv; charset=utf-8", ""));
        assert!(!is_download("text/html", ""));
        assert_eq!(filename_from_disposition("attachment; filename=\"report.CSV\"").as_deref(), Some("report.CSV"));
        assert_eq!(filename_from_disposition("inline"), None);
    }

    #[test]
    fn long_text_downloads_are_cut_with_a_note() {
        let raw = vec![b'a'; DOWNLOAD_TEXT_BYTES + 5];
        let text = readable_text("text/csv", &raw).unwrap();
        assert!(text.ends_with("\n[Download cut: 5 bytes omitted; full file in artifact]"));
        assert_eq!(readable_text("application/zip", b"PK"), None);
    }

    #[test]
    fn py_json_matches_python_dumps() {
        assert_eq!(py_json(&json!(["a", "b"])), r#"["a", "b"]"#);
        assert_eq!(py_json(&json!({"id": 3, "text": "caf\u{e9} \"x\""})), r#"{"id": 3, "text": "caf\u00e9 \"x\""}"#);
        assert_eq!(py_json(&json!("\u{1f600}")), r#""\ud83d\ude00""#);
    }

    #[test]
    fn py_repr_matches_python() {
        assert_eq!(py_repr("abc"), "'abc'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a\nb"), "'a\\nb'");
    }
}
