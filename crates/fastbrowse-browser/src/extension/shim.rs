//! A browser-level CDP endpoint made from the extension's relay.
//!
//! An extension cannot attach to the browser target, and on a tab it is refused most of the Target domain. The shim
//! answers those browser-level calls itself with tab operations the extension can do (`chrome.tabs`,
//! `chrome.debugger.attach`), gives each tab a synthetic flat session, and forwards everything else to the tab with
//! `chrome.debugger.sendCommand`. Child sessions (frames and workers that a tab's own `Target.setAutoAttach` reports)
//! keep their real ids. Replies and events come back as CDP JSON, so [`crate::cdp::Cdp`] and the session code see
//! one kind of browser.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use super::protocol::{HostRecord, PROTOCOL};
use crate::error::BrowserError;

/// Domains the session enables with `<Domain>.enable`, which the extension must relay.
pub const ENABLED_DOMAINS: [&str; 4] = ["Page", "Runtime", "DOM", "Network"];

/// Browser-level methods the shim answers itself rather than relaying.
pub const ANSWERED: [&str; 10] = [
    "Browser.getVersion",
    "SystemInfo.getProcessInfo",
    "Target.activateTarget",
    "Target.attachToTarget",
    "Target.closeTarget",
    "Target.createTarget",
    "Target.getTargetInfo",
    "Target.getTargets",
    "Target.setDiscoverTargets",
    // Browser-level auto-attach; a tab's own is relayed.
    "Target.setAutoAttach",
];

/// Why the connection to the extension ended because the user stopped the run from the browser.
pub const CANCELLED: &str = "CancelledByUser";

const SERVER_ERROR: i64 = -32000;

fn target_id(tab: i64) -> String {
    format!("fbx-tab-{tab}")
}

fn top_session(tab: i64) -> String {
    format!("fbx-session-{tab}")
}

fn tab_of_target(target: &str) -> Option<i64> {
    target.strip_prefix("fbx-tab-")?.parse().ok()
}

/// The tab a browser-level call's `targetId` names.
fn target_tab(params: &Value) -> Result<i64, String> {
    let target = params["targetId"].as_str().unwrap_or_default();
    tab_of_target(target).ok_or_else(|| format!("No target with given id found: {target}"))
}

fn tab_of_top_session(session: &str) -> Option<i64> {
    session.strip_prefix("fbx-session-")?.parse().ok()
}

/// Where a session's commands go.
#[derive(Debug, Clone, PartialEq)]
enum Route {
    Browser,
    Tab(i64),
    Child { tab: i64, session: String },
}

#[derive(Default)]
struct Tabs {
    /// Tabs this run holds a debugger attachment to.
    attached: HashMap<i64, TabInfo>,
    /// Child sessions a tab's own auto-attach reported, by the tab they belong to.
    children: HashMap<String, i64>,
    discover: bool,
    auto_attach: bool,
}

#[derive(Debug, Clone, Default)]
struct TabInfo {
    url: String,
    title: String,
    opener: Option<i64>,
}

impl TabInfo {
    fn target_info(&self, tab: i64) -> Value {
        let mut info = json!({
            "targetId": target_id(tab), "type": "page", "title": self.title, "url": self.url,
            "attached": true, "canAccessOpener": false, "browserContextId": "fbx",
        });
        if let Some(opener) = self.opener {
            info["openerId"] = target_id(opener).into();
        }
        info
    }
}

struct Relay {
    out: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    next: Mutex<u64>,
    tabs: Mutex<Tabs>,
    /// CDP JSON for the reader: replies and events.
    to_cdp: mpsc::UnboundedSender<String>,
    hello: Value,
}

impl Relay {
    async fn op(&self, op: &str, mut body: Value) -> Result<Value, String> {
        let (tx, rx) = oneshot::channel();
        let id = {
            let mut next = self.next.lock().unwrap();
            *next += 1;
            *next
        };
        self.pending.lock().unwrap().insert(id, tx);
        body["id"] = id.into();
        body["op"] = op.into();
        if self.out.send(body.to_string()).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err("the extension's host closed the connection".into());
        }
        rx.await.unwrap_or_else(|_| Err("the extension's host closed the connection".into()))
    }

    fn emit(&self, method: &str, params: Value, session: Option<&str>) {
        let mut event = json!({"method": method, "params": params});
        if let Some(session) = session {
            event["sessionId"] = session.into();
        }
        let _ = self.to_cdp.send(event.to_string());
    }

    fn route(&self, session: Option<&str>) -> Result<Route, String> {
        let Some(session) = session else { return Ok(Route::Browser) };
        if let Some(tab) = tab_of_top_session(session) {
            return Ok(Route::Tab(tab));
        }
        let tabs = self.tabs.lock().unwrap();
        match tabs.children.get(session) {
            Some(tab) => Ok(Route::Child { tab: *tab, session: session.to_owned() }),
            None => Err(format!("No session with given id: {session}")),
        }
    }

    /// One CDP command from the session code: its reply, as CDP returns it.
    async fn command(self: Arc<Self>, message: Value) -> Value {
        let id = message["id"].clone();
        let method = message["method"].as_str().unwrap_or_default().to_owned();
        let params = message.get("params").cloned().unwrap_or(json!({}));
        let session = message["sessionId"].as_str();
        let outcome = match self.route(session) {
            Err(error) => Err(error),
            Ok(Route::Browser) => self.browser_command(&method, params).await,
            Ok(Route::Tab(tab)) => self.tab_command(tab, None, &method, params).await,
            Ok(Route::Child { tab, session }) => self.tab_command(tab, Some(session), &method, params).await,
        };
        match outcome {
            Ok(result) => json!({"id": id, "result": result}),
            Err(message) => json!({"id": id, "error": {"code": SERVER_ERROR, "message": message}}),
        }
    }

    async fn tab_command(&self, tab: i64, child: Option<String>, method: &str, params: Value) -> Result<Value, String> {
        // The tab is not paused: the extension attached it as soon as it opened, and Chrome has no debugger pause
        // for an extension to release. A child session's pause is real and is relayed.
        if child.is_none() && method == "Runtime.runIfWaitingForDebugger" {
            return Ok(json!({}));
        }
        let mut body = json!({"tabId": tab, "method": method, "params": params});
        if let Some(child) = child {
            body["sessionId"] = child.into();
        }
        self.op("cdp", body).await
    }

    async fn browser_command(&self, method: &str, params: Value) -> Result<Value, String> {
        match method {
            // A real round trip, so a liveness probe finds a lost extension as it would a lost browser.
            "Browser.getVersion" => {
                self.op("ping", json!({})).await?;
                Ok(json!({
                    "protocolVersion": "1.3",
                    "product": format!("{}/{}", self.hello["browser"].as_str().unwrap_or("Chrome"),
                        self.hello["version"].as_str().unwrap_or_default()),
                    "userAgent": self.hello["userAgent"],
                }))
            }
            "SystemInfo.getProcessInfo" => Err("the extension backend has no browser process to report".into()),
            "Target.setDiscoverTargets" => {
                self.tabs.lock().unwrap().discover = params["discover"] == true;
                Ok(json!({}))
            }
            "Target.setAutoAttach" => {
                self.tabs.lock().unwrap().auto_attach = params["autoAttach"] == true;
                Ok(json!({}))
            }
            "Target.createTarget" => {
                let url = params["url"].as_str().unwrap_or("about:blank");
                let background = params["background"] == true;
                let created = self.op("newTab", json!({"url": url, "background": background})).await?;
                let tab = created["tabId"].as_i64().ok_or("the extension opened no tab")?;
                let info = TabInfo { url: url.to_owned(), ..TabInfo::default() };
                self.tabs.lock().unwrap().attached.insert(tab, info);
                Ok(json!({"targetId": target_id(tab)}))
            }
            "Target.attachToTarget" => {
                let tab = target_tab(&params)?;
                let known = self.tabs.lock().unwrap().attached.get(&tab).cloned();
                let info = match known {
                    Some(info) => info,
                    // A tab of the user's that `--attach --target-match` chose: the extension moves it into the
                    // run's group and attaches it.
                    None => {
                        let got = self.op("adopt", json!({"tabId": tab})).await?;
                        let info = tab_info(&got);
                        self.tabs.lock().unwrap().attached.insert(tab, info.clone());
                        info
                    }
                };
                let session = top_session(tab);
                self.emit(
                    "Target.attachedToTarget",
                    json!({"sessionId": session, "targetInfo": info.target_info(tab), "waitingForDebugger": false}),
                    None,
                );
                Ok(json!({"sessionId": session}))
            }
            "Target.getTargets" => {
                let listed = self.op("listTabs", json!({"all": params["filter"].is_null()})).await?;
                let infos: Vec<Value> = listed["tabs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|tab| Some(tab_info(tab).target_info(tab["tabId"].as_i64()?)))
                    .collect();
                Ok(json!({"targetInfos": infos}))
            }
            "Target.getTargetInfo" => {
                let tab = target_tab(&params)?;
                let got = self.op("getTab", json!({"tabId": tab})).await?;
                let info = tab_info(&got);
                if let Some(known) = self.tabs.lock().unwrap().attached.get_mut(&tab) {
                    known.url.clone_from(&info.url);
                    known.title.clone_from(&info.title);
                }
                Ok(json!({"targetInfo": info.target_info(tab)}))
            }
            "Target.closeTarget" => {
                let tab = target_tab(&params)?;
                self.op("closeTab", json!({"tabId": tab})).await?;
                Ok(json!({"success": true}))
            }
            "Target.activateTarget" => {
                let tab = target_tab(&params)?;
                self.op("activate", json!({"tabId": tab})).await?;
                Ok(json!({}))
            }
            "Target.detachFromTarget" => {
                let session = params["sessionId"].as_str().unwrap_or_default();
                match self.route(Some(session))? {
                    // The run's own tab stays attached until it closes; a second session on it does not exist.
                    Route::Tab(_) | Route::Browser => Ok(json!({})),
                    Route::Child { tab, session } => {
                        self.op(
                            "cdp",
                            json!({"tabId": tab, "method": "Target.detachFromTarget",
                            "params": {"sessionId": session}}),
                        )
                        .await
                    }
                }
            }
            _ => Err(format!("{method} is not available through the fastbrowse extension")),
        }
    }

    /// One event from the extension.
    fn event(&self, message: &Value) {
        let tab = message["tabId"].as_i64().unwrap_or_default();
        match message["event"].as_str().unwrap_or_default() {
            "cdp" => {
                let method = message["method"].as_str().unwrap_or_default();
                let params = &message["params"];
                let session = match message["sessionId"].as_str() {
                    Some(child) => child.to_owned(),
                    None => top_session(tab),
                };
                match method {
                    "Target.attachedToTarget" => {
                        if let Some(child) = params["sessionId"].as_str() {
                            self.tabs.lock().unwrap().children.insert(child.to_owned(), tab);
                        }
                    }
                    "Target.detachedFromTarget" => {
                        if let Some(child) = params["sessionId"].as_str() {
                            self.tabs.lock().unwrap().children.remove(child);
                        }
                    }
                    _ => {}
                }
                self.emit(method, params.clone(), Some(&session));
            }
            // A tab the run's own tab opened. The extension attached it at once and put it in the run's group.
            "popup" => {
                let info = TabInfo {
                    url: message["url"].as_str().unwrap_or_default().to_owned(),
                    title: String::new(),
                    opener: message["openerTabId"].as_i64(),
                };
                let auto_attach = {
                    let mut tabs = self.tabs.lock().unwrap();
                    tabs.attached.insert(tab, info.clone());
                    tabs.auto_attach
                };
                if auto_attach {
                    // Reported paused, as browser auto-attach reports a new page, so the session adopts it as a popup.
                    self.emit(
                        "Target.attachedToTarget",
                        json!({"sessionId": top_session(tab), "targetInfo": info.target_info(tab),
                            "waitingForDebugger": true}),
                        None,
                    );
                }
            }
            "tabUpdated" => {
                let info = {
                    let mut tabs = self.tabs.lock().unwrap();
                    let Some(known) = tabs.attached.get_mut(&tab) else { return };
                    if let Some(url) = message["url"].as_str() {
                        url.clone_into(&mut known.url);
                    }
                    if let Some(title) = message["title"].as_str() {
                        title.clone_into(&mut known.title);
                    }
                    known.clone()
                };
                self.emit("Target.targetInfoChanged", json!({"targetInfo": info.target_info(tab)}), None);
            }
            "detached" => {
                let closed = message["reason"] == "target_closed";
                let known = {
                    let mut tabs = self.tabs.lock().unwrap();
                    tabs.children.retain(|_, owner| *owner != tab);
                    tabs.attached.remove(&tab).is_some()
                };
                if known {
                    self.emit(
                        "Target.detachedFromTarget",
                        json!({"sessionId": top_session(tab), "targetId": target_id(tab)}),
                        None,
                    );
                    if closed {
                        self.emit("Target.targetDestroyed", json!({"targetId": target_id(tab)}), None);
                    }
                }
            }
            "tabRemoved" => {
                let known = self.tabs.lock().unwrap().attached.remove(&tab).is_some();
                if known {
                    self.emit("Target.targetDestroyed", json!({"targetId": target_id(tab)}), None);
                }
            }
            _ => {}
        }
    }
}

fn tab_info(tab: &Value) -> TabInfo {
    TabInfo {
        url: tab["url"].as_str().unwrap_or_default().to_owned(),
        title: tab["title"].as_str().unwrap_or_default().to_owned(),
        opener: tab["openerTabId"].as_i64(),
    }
}

/// A connected extension, presented as a CDP endpoint: CDP JSON goes into `commands`, replies and events come out of
/// `replies`. `replies` ends with the reason the connection closed, as a final `{"closed": reason}` line.
pub struct Endpoint {
    pub commands: mpsc::UnboundedSender<String>,
    pub replies: mpsc::UnboundedReceiver<String>,
    pub hello: Value,
}

/// Dial the host listening at `record.socket` and agree on the protocol.
pub async fn connect(record: &HostRecord, task: &str) -> Result<Endpoint, BrowserError> {
    let stream = dial(&record.socket).await?;
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    let (out, mut outgoing) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(line) = outgoing.recv().await {
            if write.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                return;
            }
        }
        let _ = write.shutdown().await;
    });
    let hello_line = json!({"id": 0, "op": "hello", "protocol": PROTOCOL, "task": task}).to_string();
    out.send(hello_line).map_err(|_| unavailable("the extension's host closed the connection"))?;
    let hello = loop {
        let line = tokio::time::timeout(std::time::Duration::from_secs(10), lines.next_line())
            .await
            .map_err(|_| unavailable("the fastbrowse extension did not answer"))?
            .map_err(|_| unavailable("the extension's host closed the connection"))?
            .ok_or_else(|| unavailable("the extension's host closed the connection"))?;
        let message: Value = serde_json::from_str(&line).unwrap_or_default();
        if message["id"] == 0 {
            break message;
        }
    };
    if let Some(error) = hello["error"].as_str() {
        return Err(unavailable(error));
    }
    let hello = hello["result"].clone();
    let theirs = hello["protocol"].as_u64().unwrap_or_default();
    if theirs != u64::from(PROTOCOL) {
        let update = if theirs < u64::from(PROTOCOL) { "the fastbrowse extension" } else { "fastbrowse" };
        return Err(unavailable(&format!(
            "the fastbrowse extension speaks relay protocol {theirs} and this fastbrowse speaks {PROTOCOL}: update \
             {update}"
        )));
    }
    let (to_cdp, replies) = mpsc::unbounded_channel();
    let (commands, mut incoming) = mpsc::unbounded_channel::<String>();
    let relay = Arc::new(Relay {
        out,
        pending: Mutex::default(),
        next: Mutex::new(0),
        tabs: Mutex::default(),
        to_cdp,
        hello: hello.clone(),
    });
    let reader = Arc::clone(&relay);
    tokio::spawn(async move {
        let why = loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
                    if message["event"] == "stop" {
                        break CANCELLED;
                    }
                    if message["event"].is_string() {
                        reader.event(&message);
                    } else if let Some(id) = message["id"].as_u64() {
                        let waiter = reader.pending.lock().unwrap().remove(&id);
                        if let Some(waiter) = waiter {
                            let outcome = match message["error"].as_str() {
                                Some(error) => Err(error.to_owned()),
                                None => Ok(message["result"].clone()),
                            };
                            let _ = waiter.send(outcome);
                        }
                    }
                }
                _ => break "ConnectionClosed",
            }
        };
        reader.pending.lock().unwrap().clear();
        let _ = reader.to_cdp.send(json!({"closed": why}).to_string());
    });
    tokio::spawn(async move {
        // Each command on its own task: a hung one must not hold back a liveness probe or a resume.
        while let Some(line) = incoming.recv().await {
            let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
            let relay = Arc::clone(&relay);
            tokio::spawn(async move {
                let reply = Arc::clone(&relay).command(message).await;
                let _ = relay.to_cdp.send(reply.to_string());
            });
        }
    });
    Ok(Endpoint { commands, replies, hello })
}

fn unavailable(message: &str) -> BrowserError {
    BrowserError::Unavailable(message.to_owned())
}

#[cfg(unix)]
async fn dial(socket: &Path) -> Result<tokio::net::UnixStream, BrowserError> {
    tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|_| unavailable("the fastbrowse extension's host is not listening: open the browser, or reinstall"))
}

#[cfg(windows)]
async fn dial(socket: &Path) -> Result<tokio::net::windows::named_pipe::NamedPipeClient, BrowserError> {
    tokio::net::windows::named_pipe::ClientOptions::new()
        .open(socket)
        .map_err(|_| unavailable("the fastbrowse extension's host is not listening: open the browser, or reinstall"))
}
