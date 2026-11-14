//! A browser-level CDP endpoint made from the extension's relay.
//!
//! An extension cannot attach to the browser target, and on a tab it is refused most of the Target domain. The shim
//! answers those browser-level calls itself with tab operations the extension can do (`chrome.tabs`,
//! `chrome.debugger.attach`), gives each tab a synthetic flat session, and forwards everything else to the tab with
//! `chrome.debugger.sendCommand`. Child sessions (frames and workers that a tab's own `Target.setAutoAttach` reports)
//! keep their real ids. Replies and events come back as CDP JSON, so [`crate::cdp::Cdp`] and the session code see
//! one kind of browser.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use fastbrowse_core::telemetry::Trace;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use super::protocol::{EXTENSION_IDS, HostRecord, MIN_EXTENSION_VERSION, PROTOCOL, required_methods};
use crate::error::BrowserError;

/// Domains the session enables with `<Domain>.enable`, which the extension must relay.
pub const ENABLED_DOMAINS: [&str; 4] = ["Page", "Runtime", "DOM", "Network"];

/// Browser-level methods the shim answers itself rather than relaying.
pub const ANSWERED: [&str; 11] = [
    "Browser.getVersion",
    // Not CDP: the extension closes a run's tabs when the run lets go of it, except a tab named to it here.
    "Fastbrowse.keepTarget",
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
    /// The frames from another extension each tab holds, by frame id, as their `chrome-extension://` origin.
    foreign_frames: HashMap<i64, HashMap<String, String>>,
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
    trace: Trace,
}

/// The `chrome-extension://<id>` origin of `url` when it belongs to an extension other than fastbrowse.
fn foreign_extension(url: &str) -> Option<String> {
    let id = url.strip_prefix("chrome-extension://")?.split('/').next()?;
    (!id.is_empty() && !EXTENSION_IDS.contains(&id)).then(|| format!("chrome-extension://{id}"))
}

/// Why a run lost its tab when Chrome detached it right after `origin`'s frame appeared in it.
fn foreign_frame_detach(origin: &str) -> String {
    let id = origin.trim_start_matches("chrome-extension://");
    format!(
        "Chrome detached fastbrowse from the run's tab when the page loaded a frame from another extension ({origin}). \
         Chrome lets no extension debug a tab that holds another extension's frame: set that extension's Site access \
         to On click at chrome://extensions/?id={id}, then run again"
    )
}

/// Record one relay message in the run's trace: `direction` is `to_extension` or `from_extension`.
///
/// Everything is kept but images. A screenshot or screencast frame can show a secret just typed, which redacting
/// values cannot catch, so its `data` is replaced by its length.
fn trace_message(trace: &Trace, direction: &str, message: &Value) {
    trace.event("extension", || {
        let mut message = message.clone();
        for pointer in ["/params/data", "/result/data"] {
            if let Some(data) = message.pointer_mut(pointer)
                && let Some(length) = data.as_str().map(str::len)
            {
                *data = format!("<{length} characters of image data, not traced>").into();
            }
        }
        Map::from_iter([("direction".into(), direction.into()), ("message".into(), message)])
    });
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
        trace_message(&self.trace, "to_extension", &body);
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
            "Fastbrowse.keepTarget" => {
                let tab = target_tab(&params)?;
                self.op("keepTab", json!({"tabId": tab})).await?;
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

    /// Keep `foreign_frames` current from a tab's frame events: a frame that starts loading another extension's page
    /// is added, and one that leaves it or goes is removed.
    fn track_frame(&self, tab: i64, method: &str, params: &Value) {
        let (frame, url) = match method {
            "Page.frameRequestedNavigation" | "Page.frameStartedNavigating" => (&params["frameId"], &params["url"]),
            "Page.frameNavigated" => (&params["frame"]["id"], &params["frame"]["url"]),
            "Page.frameDetached" => (&params["frameId"], &Value::Null),
            _ => return,
        };
        let Some(frame) = frame.as_str() else { return };
        // `frameStartedNavigating` can carry an empty address before the request goes: it changes nothing.
        if method == "Page.frameStartedNavigating" && url.as_str() == Some("") {
            return;
        }
        let mut tabs = self.tabs.lock().unwrap();
        let frames = tabs.foreign_frames.entry(tab).or_default();
        match url.as_str().and_then(foreign_extension) {
            Some(origin) => {
                frames.insert(frame.to_owned(), origin);
            }
            None => {
                frames.remove(frame);
            }
        }
    }

    /// One event from the extension, and why the connection must end when the event ends the run.
    fn event(&self, message: &Value) -> Option<String> {
        let tab = message["tabId"].as_i64().unwrap_or_default();
        match message["event"].as_str().unwrap_or_default() {
            "cdp" => {
                let method = message["method"].as_str().unwrap_or_default();
                let params = &message["params"];
                self.track_frame(tab, method, params);
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
                    let known = tabs.attached.get_mut(&tab)?;
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
                let foreign = self
                    .tabs
                    .lock()
                    .unwrap()
                    .foreign_frames
                    .remove(&tab)
                    .and_then(|frames| frames.into_values().next());
                self.trace.event("extension_tab_lost", || {
                    Map::from_iter([
                        ("tab".into(), tab.into()),
                        ("reason".into(), message["reason"].clone()),
                        ("foreign_frame".into(), foreign.clone().into()),
                    ])
                });
                // Chrome reports a tab it took away for another extension's frame as `target_closed`, though the tab
                // is still open: the run is told why rather than that its tab closed.
                if closed && let Some(origin) = foreign {
                    // So the popup can name the extension that took it. No reply comes, and an extension without
                    // the op answers with an error that has no id, which is dropped.
                    let taken = json!({"op": "taken", "tabId": tab, "origin": origin});
                    trace_message(&self.trace, "to_extension", &taken);
                    let _ = self.out.send(taken.to_string());
                    return Some(foreign_frame_detach(&origin));
                }
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
        None
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
pub async fn connect(record: &HostRecord, task: &str, trace: Trace) -> Result<Endpoint, BrowserError> {
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
    let hello_message = json!({"id": 0, "op": "hello", "protocol": PROTOCOL, "task": task});
    trace_message(&trace, "to_extension", &hello_message);
    out.send(hello_message.to_string()).map_err(|_| unavailable("the extension's host closed the connection"))?;
    let hello = loop {
        let line = tokio::time::timeout(std::time::Duration::from_secs(10), lines.next_line())
            .await
            .map_err(|_| unavailable("the fastbrowse extension did not answer"))?
            .map_err(|_| unavailable("the extension's host closed the connection"))?
            .ok_or_else(|| unavailable("the extension's host closed the connection"))?;
        let message: Value = serde_json::from_str(&line).unwrap_or_default();
        trace_message(&trace, "from_extension", &message);
        if message["id"] == 0 {
            break message;
        }
    };
    if let Some(error) = hello["error"].as_str() {
        return Err(unavailable(error));
    }
    let hello = hello["result"].clone();
    validate_hello(&hello)?;
    let (to_cdp, replies) = mpsc::unbounded_channel();
    let (commands, mut incoming) = mpsc::unbounded_channel::<String>();
    let relay = Arc::new(Relay {
        out,
        pending: Mutex::default(),
        next: Mutex::new(0),
        tabs: Mutex::default(),
        to_cdp,
        hello: hello.clone(),
        trace,
    });
    let reader = Arc::clone(&relay);
    tokio::spawn(async move {
        let mut by = Value::Null;
        let why = loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
                    trace_message(&reader.trace, "from_extension", &message);
                    if message["event"] == "stop" {
                        by = message["by"].clone();
                        break CANCELLED.to_owned();
                    }
                    if message["event"].is_string() {
                        if let Some(why) = reader.event(&message) {
                            break why;
                        }
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
                _ => break "ConnectionClosed".to_owned(),
            }
        };
        reader.pending.lock().unwrap().clear();
        let _ = reader.to_cdp.send(json!({"closed": why, "by": by}).to_string());
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

/// An older Store copy can speak the same protocol without advertising the commands this build needs.
/// The version explains recovery; only the explicit command inventory establishes compatibility.
fn validate_hello(hello: &Value) -> Result<(), BrowserError> {
    let theirs = hello["protocol"].as_u64().unwrap_or_default();
    if theirs != u64::from(PROTOCOL) {
        let update = if theirs < u64::from(PROTOCOL) { "the fastbrowse extension" } else { "fastbrowse" };
        return Err(unavailable(&format!(
            "the fastbrowse extension speaks relay protocol {theirs} and this fastbrowse speaks {PROTOCOL}: update \
             {update}"
        )));
    }
    let version = hello["extensionVersion"].as_str().unwrap_or("(version not reported)");
    let missing = missing_methods(hello);
    if !missing.is_empty() {
        // A copy that predates the inventory cannot name a single method, so every one reads as missing: name the
        // version it needs instead of a list that says nothing about why.
        let why = if hello["methods"].is_array() {
            format!(
                "the fastbrowse extension {version} does not relay {} of the {} CDP methods this fastbrowse needs \
                 (extension {MIN_EXTENSION_VERSION} or newer relays all of them): {}",
                required_methods().len() - missing.len(),
                required_methods().len(),
                missing.join(", ")
            )
        } else {
            format!(
                "the fastbrowse extension {version} predates extension {MIN_EXTENSION_VERSION} and cannot say \
                 which CDP methods it relays, so it cannot be used for a run"
            )
        };
        return Err(unavailable(&format!(
            "{why}. Install extension {MIN_EXTENSION_VERSION} or newer from the Chrome Web Store once it lists one, \
             or load the copy this fastbrowse ships: run `fastbrowse extension path`, then enable Developer mode on \
             chrome://extensions and choose Load unpacked for the folder it prints. Reloading the Store \
             installation does not install that folder"
        )));
    }
    Ok(())
}

/// The CDP methods this build cannot run without that the extension's hello does not list. A copy older than the
/// inventory sends none, so every method is missing; a newer copy relaying more passes; a copy missing only a
/// best-effort method passes, since the run works without it.
fn missing_methods(hello: &Value) -> Vec<String> {
    let theirs: HashSet<&str> = hello["methods"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    required_methods().into_iter().filter(|method| !theirs.contains(method.as_str())).collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::protocol::allowed_methods;

    #[test]
    fn an_extension_that_cannot_relay_a_method_this_build_sends_is_named_before_the_run() {
        let ours = required_methods();
        assert_eq!(missing_methods(&json!({"protocol": PROTOCOL})), ours, "a copy older than the list relays none");
        let short: Vec<&String> = ours.iter().filter(|m| *m != "Runtime.callFunctionOn").collect();
        assert_eq!(missing_methods(&json!({"methods": short})), ["Runtime.callFunctionOn"]);
        let mut more = allowed_methods();
        more.push("Some.newerMethod".to_owned());
        assert!(missing_methods(&json!({"methods": more})).is_empty(), "a newer copy relaying more passes");
    }

    #[test]
    fn an_old_store_copy_names_the_required_extension_before_the_run() {
        let error = validate_hello(&json!({"protocol": PROTOCOL})).unwrap_err().to_string();
        assert!(error.contains("version not reported"));
        assert!(error.contains("extension 0.1.1 or newer"));
        assert!(!error.contains("does not relay DOM"));
        let error = validate_hello(&json!({
            "protocol": PROTOCOL, "extensionVersion": "0.1.0", "methods": null
        }))
        .unwrap_err()
        .to_string();
        assert!(error.contains("extension 0.1.0"));
    }

    #[test]
    fn commands_are_required_even_when_the_extension_version_is_newer() {
        let mut hello = json!({"protocol": PROTOCOL, "extensionVersion": "0.1.1", "methods": allowed_methods()});
        assert!(validate_hello(&hello).is_ok());
        hello["extensionVersion"] = json!("99.0.0");
        hello["methods"] = json!([]);
        assert!(validate_hello(&hello).unwrap_err().to_string().contains("does not relay"));
        hello["methods"] = json!(allowed_methods());
        hello["protocol"] = json!(PROTOCOL + 1);
        assert!(validate_hello(&hello).unwrap_err().to_string().contains("relay protocol"));
    }

    /// The Store's 0.1.0 package lacks only this one of the methods a run sends, and the session already runs on when
    /// it is refused, so a copy without it must still drive a task.
    #[test]
    fn a_copy_that_lacks_only_a_best_effort_method_still_runs() {
        let mut relayed = allowed_methods();
        relayed.retain(|method| method != "Page.setInterceptFileChooserDialog");
        assert!(missing_methods(&json!({"methods": relayed})).is_empty());
        assert!(validate_hello(&json!({"protocol": PROTOCOL, "methods": relayed})).is_ok());
    }

    /// A Store install cannot pick up the shipped folder by reloading it, so the recovery step has to name the
    /// Developer-mode Load unpacked path that actually installs the directory `extension path` writes.
    #[test]
    fn the_recovery_step_names_loading_the_unpacked_copy_not_reloading_the_store_install() {
        let error = validate_hello(&json!({"protocol": PROTOCOL})).unwrap_err().to_string();
        assert!(error.contains("fastbrowse extension path"), "names the command that prints the folder: {error}");
        assert!(error.contains("Developer mode"), "names where the Developer mode toggle lives: {error}");
        assert!(error.contains("Load unpacked"), "names the action that installs the shipped folder: {error}");
        assert!(!error.contains("click Reload"), "reloading a Store install installs nothing: {error}");
    }

    fn relay() -> Relay {
        relay_to(mpsc::unbounded_channel().0)
    }

    fn relay_to(out: mpsc::UnboundedSender<String>) -> Relay {
        let (to_cdp, _) = mpsc::unbounded_channel();
        Relay {
            out,
            pending: Mutex::default(),
            next: Mutex::new(0),
            tabs: Mutex::default(),
            to_cdp,
            hello: Value::Null,
            trace: Trace::collecting(),
        }
    }

    fn frame(url: &str) -> Value {
        json!({"event": "cdp", "tabId": 7, "method": "Page.frameRequestedNavigation", "params": {"frameId": "F", "url": url}})
    }

    const DETACHED: &str = r#"{"event": "detached", "tabId": 7, "reason": "target_closed"}"#;

    #[test]
    fn a_tab_chrome_takes_away_for_another_extensions_frame_ends_the_run_naming_it() {
        let (out, mut to_extension) = mpsc::unbounded_channel();
        let relay = relay_to(out);
        assert_eq!(relay.event(&frame("chrome-extension://abcdefgh/audio-devices.html")), None);
        let why = relay.event(&serde_json::from_str(DETACHED).unwrap()).expect("the run ends");
        assert!(
            why.contains("chrome-extension://abcdefgh") && why.contains("chrome://extensions/?id=abcdefgh"),
            "{why}"
        );
        let lost: Vec<Value> =
            relay.trace.events().into_iter().filter(|event| event["event"] == "extension_tab_lost").collect();
        assert_eq!(lost[0]["foreign_frame"], "chrome-extension://abcdefgh");
        // The popup names it too.
        let taken: Value = serde_json::from_str(&to_extension.try_recv().unwrap()).unwrap();
        assert_eq!(taken, json!({"op": "taken", "tabId": 7, "origin": "chrome-extension://abcdefgh"}));
    }

    #[test]
    fn a_closed_tab_and_fastbrowses_own_frames_still_read_as_a_closed_tab() {
        let relay = relay();
        relay.event(&frame(&format!("chrome-extension://{}/popup.html", EXTENSION_IDS[0])));
        relay.event(&frame("https://example.com/"));
        assert_eq!(relay.event(&serde_json::from_str(DETACHED).unwrap()), None);
    }

    #[test]
    fn only_a_frame_still_holding_another_extensions_page_counts() {
        let relay = relay();
        let request = json!({"event": "cdp", "tabId": 7, "method": "Network.requestWillBeSent",
            "params": {"url": "chrome-extension://abcdefgh/icon.png"}});
        relay.event(&request);
        assert_eq!(relay.event(&serde_json::from_str(DETACHED).unwrap()), None, "an extension's image is no frame");
        relay.event(&frame("chrome-extension://abcdefgh/audio-devices.html"));
        relay.event(&json!({"event": "cdp", "tabId": 7, "method": "Page.frameDetached", "params": {"frameId": "F"}}));
        assert_eq!(relay.event(&serde_json::from_str(DETACHED).unwrap()), None, "the frame went before the detach");
    }

    #[tokio::test]
    async fn a_tab_handed_over_is_named_to_the_extension_that_would_close_it() {
        // #887: the extension closes every tab a run opened when the run lets go of it, so a form left for a person
        // to confirm was lost even though the session had not closed its tab.
        let (out, mut to_extension) = mpsc::unbounded_channel();
        let relay = Arc::new(relay_to(out));
        let keeping = tokio::spawn({
            let relay = relay.clone();
            async move { relay.browser_command("Fastbrowse.keepTarget", json!({"targetId": target_id(7)})).await }
        });
        let sent = tokio::time::timeout(std::time::Duration::from_secs(5), to_extension.recv()).await;
        keeping.abort();
        let sent: Value = serde_json::from_str(&sent.expect("an operation sent").unwrap()).unwrap();
        assert_eq!((sent["op"].as_str(), sent["tabId"].as_i64()), (Some("keepTab"), Some(7)));
    }

    #[test]
    fn image_data_stays_out_of_the_trace() {
        let trace = Trace::collecting();
        let frame =
            json!({"event": "cdp", "method": "Page.screencastFrame", "params": {"data": "AAAA", "sessionId": 1}});
        trace_message(&trace, "from_extension", &frame);
        trace_message(&trace, "from_extension", &json!({"id": 3, "result": {"data": "BBBBBB"}}));
        let events = trace.events();
        assert_eq!(events[0]["message"]["params"]["data"], "<4 characters of image data, not traced>");
        assert_eq!(events[0]["message"]["params"]["sessionId"], 1);
        assert_eq!(events[1]["message"]["result"]["data"], "<6 characters of image data, not traced>");
    }
}
