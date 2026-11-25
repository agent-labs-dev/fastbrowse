//! The CDP connection: one reader task owns the websocket's read half, one writer task its write half, and
//! commands use a liveness probe and deadlines to detect an unresponsive browser.
//!
//! Replies route to callers by id through oneshot channels, so any number of calls are in flight at once. Events
//! go to handlers synchronously and in socket order, so a handler sees every event before the reply that follows
//! it on the socket is delivered: state a handler keeps (frames, dialogs, tabs) is current by the time a caller
//! wakes.
//!
//! Dropping a `call` future removes its pending entry, so a late reply is read and discarded. When the socket
//! closes, every pending call and every later one fails with `CdpError::Closed`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, timeout, timeout_at};
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use crate::error::{BrowserError, Failure};

/// How long an ordinary CDP command waits before the browser is asked whether it is still there. A cloud browser's
/// proxy can keep the socket open, answering pings, after losing the browser behind it, and a run with no wall-clock
/// limit would otherwise wait on that reply forever. A slow reply is not a lost one: `Page.navigate` waits for a slow
/// server's headers, so a live browser can keep waiting until `CDP_COMMAND_SECONDS`.
pub const CDP_REPLY_SECONDS: f64 = 60.0;
/// The deadline for an ordinary CDP command. A responsive browser can still leave one document command pending forever.
pub const CDP_COMMAND_SECONDS: f64 = 120.0;
/// Chrome's hung-renderer delay for an input event. An input that reached a renderer blocked in script must return
/// before the next input can be dispatched, so it gets Chrome's own shorter deadline.
pub const CDP_INPUT_SECONDS: f64 = 15.0;
/// `Browser.getVersion` is answered at once by any browser that is there.
pub const CDP_ALIVE_SECONDS: f64 = 10.0;
/// How long a command to a page waits before the page is asked whether it still answers. A renderer whose main thread
/// never returns, as after a click handler that loops, answers nothing it is sent, while a page waiting on a slow
/// server still answers a trivial evaluation. Only a page that leaves that unanswered for `CDP_ALIVE_SECONDS` too
/// is unresponsive, so a hung page is known in about 20 seconds rather than at `CDP_COMMAND_SECONDS`.
pub const CDP_PAGE_SECONDS: f64 = 10.0;
/// Command deadlines one session may meet before its commands fail at once, as `PageUnresponsive`. A read
/// that waits out the deadline is often one whose caller treats a failed read as an unreadable page and carries on,
/// so the next read waited it out again: one login run met it thirteen times, 26 minutes on a page it could not
/// use. Browser-level commands (closing the run's tabs) are still sent.
pub const CDP_DEADLINES_BEFORE_STOP: u32 = 2;
/// CDP's generic server error. Unclassified replies keep the existing document-loss recovery.
pub const SERVER_ERROR: i64 = -32000;
/// Chrome's reply to a command sent to a session that has detached.
pub const SESSION_NOT_FOUND: i64 = -32001;

const APPROVAL_HINT: &str = "If this is a Chrome you started yourself, it may be waiting for you to approve remote \
                             debugging, or the address may be stale; check the browser window and the address, then \
                             retry.";
/// websockets' handshake deadline, which cdp-use connects with.
const OPEN_SECONDS: f64 = 10.0;
/// websockets' keepalive: a ping every 20 seconds, and the connection is dropped when one goes 20 seconds unanswered.
const PING_SECONDS: f64 = 20.0;
/// cdp-use's frame bound: a full-page screenshot or a large document's evaluation fits under it.
const MAX_MESSAGE_BYTES: usize = 100 * 1024 * 1024;
/// Transient response statuses permit retrying a failed handshake or cloud browser request.
pub const RETRYABLE_STATUS: [u16; 12] = [408, 429, 500, 502, 503, 504, 520, 521, 522, 523, 524, 529];

/// A command past its deadline: its reply if one has landed, however late, else whether the browser still answers.
async fn after_deadline<R>(
    mut reply: Pin<&mut impl Future<Output = R>>,
    alive: impl Future<Output = bool>,
) -> Result<R, bool> {
    // A reply taken here is returned, never polled for again: a finished future must not be resumed.
    if let Ok(reply) = timeout(Duration::ZERO, &mut reply).await {
        return Ok(reply);
    }
    let alive = alive.await;
    timeout(Duration::ZERO, &mut reply).await.map_err(|_| alive)
}

pub fn secs(seconds: f64) -> Duration {
    Duration::from_secs_f64(seconds)
}

fn command_seconds(method: &str) -> f64 {
    if method.starts_with("Input.") { CDP_INPUT_SECONDS } else { CDP_COMMAND_SECONDS }
}

#[derive(Debug, Clone)]
pub enum CdpError {
    /// The connection is gone. Carries why, the same for every call it failed.
    Closed(Arc<str>),
    /// The browser answered with an error. Its message is kept out: it can quote page text or evaluated source, so
    /// the refusals whose text Chrome fixes are named by `failure` instead.
    /// `reason` is text the extension shim wrote for a browser-level call, such as Chrome refusing to open or attach a
    /// tab, which quotes no page (#1314).
    Protocol { code: i64, failure: Option<ProtocolFailure>, reason: Option<String> },
}

#[derive(Debug, Clone, Copy, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolFailure {
    NavigationDenied,
    /// Chrome refused a Page-domain command because the session no longer has a page behind it.
    NotAttached,
}

impl ProtocolFailure {
    /// Chrome's own fixed wording, matched whole: a message that merely contains it can be quoting page text.
    fn of_chrome_message(message: &str) -> Option<Self> {
        (message == "Not attached to an active page").then_some(Self::NotAttached)
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    pub method: String,
    pub session: Option<String>,
    pub params: Value,
}

/// Every message from the browser: a reply has an id, an event a method. Typed because it is parsed for every
/// message; everything inside `params` and `result` stays a `Value` and is read where it is used.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Incoming {
    id: Option<u64>,
    method: Option<String>,
    session_id: Option<String>,
    #[serde(default)]
    params: Value,
    #[serde(default)]
    result: Value,
    error: Option<RemoteError>,
}

impl Incoming {
    /// An event with no session.
    fn event(method: &str, params: Value) -> Incoming {
        Incoming {
            id: None,
            method: Some(method.to_owned()),
            session_id: None,
            params,
            result: Value::Null,
            error: None,
        }
    }
}

#[derive(Deserialize)]
struct RemoteError {
    code: i64,
    #[serde(default)]
    message: String,
    #[serde(default, rename = "fastbrowseFailure")]
    failure: Option<ProtocolFailure>,
    #[serde(default, rename = "fastbrowseReason")]
    reason: Option<String>,
}

type Reply = Result<Value, CdpError>;
pub type Handler = Box<dyn Fn(&Event) + Send + Sync>;

enum Outgoing {
    Text(String),
    Ping,
    Close,
}

#[derive(Default)]
struct State {
    next_id: u64,
    pending: HashMap<u64, Pending>,
    closed: Option<Arc<str>>,
}

/// A call waiting for its reply, and the session it addressed.
struct Pending {
    session: Option<Arc<str>>,
    tx: oneshot::Sender<Reply>,
}

struct Inner {
    state: Mutex<State>,
    /// Separate from `state` so a handler can call, or spawn work that calls, without deadlocking the reader.
    handlers: Mutex<Vec<Arc<Handler>>>,
    out: mpsc::UnboundedSender<Outgoing>,
    /// Whether the browser has replied to anything on this connection. Chrome can accept the socket and then hold
    /// every command while its remote debugging approval prompt waits, which would otherwise read as a lost browser
    /// only after `CDP_REPLY_SECONDS`.
    answered: AtomicBool,
    /// Commands that outlived their method-specific deadline, by the session they addressed. A cloud browser's connection
    /// carries every target the run attached, third-party frames and workers included, and one of those hanging says
    /// nothing about the tab the run drives. A child target's deadline must not prevent commands to an answering tab.
    deadlines: Mutex<HashMap<String, u32>>,
    pong: AtomicBool,
}

#[derive(Clone)]
pub struct Cdp(Arc<Inner>);

/// `_unreachable`: a connection the browser's endpoint dropped or refused for now, as a cloud browser still starting
/// does. A malformed reply, such as a proxy's bare 401, and a TLS failure both need the configuration changed.
fn unreachable(error: &WsError) -> bool {
    match error {
        WsError::Http(response) => RETRYABLE_STATUS.contains(&response.status().as_u16()),
        WsError::Io(io) => !tls_failure(io),
        // A handshake the endpoint closed before its status line is Python's `InvalidMessage` from an EOF.
        WsError::Protocol(ProtocolError::HandshakeIncomplete) => true,
        WsError::ConnectionClosed | WsError::AlreadyClosed => true,
        _ => false,
    }
}

/// rustls reports a refused certificate through the stream as an I/O error carrying its own error.
fn tls_failure(io: &std::io::Error) -> bool {
    io.get_ref().is_some_and(|inner| inner.downcast_ref::<rustls::Error>().is_some())
}

/// The exception class name Python reports for a handshake failure; its text can carry the credential in the URL.
fn handshake_detail(error: &WsError) -> &'static str {
    match error {
        WsError::Http(_) => "InvalidStatus",
        WsError::Io(io) if tls_failure(io) => "SSLError",
        WsError::Io(io) => match io.kind() {
            std::io::ErrorKind::ConnectionRefused => "ConnectionRefusedError",
            std::io::ErrorKind::ConnectionReset => "ConnectionResetError",
            std::io::ErrorKind::TimedOut => "TimeoutError",
            _ => "OSError",
        },
        WsError::Tls(_) => "SSLError",
        WsError::Url(_) => "InvalidURI",
        WsError::ConnectionClosed | WsError::AlreadyClosed => "ConnectionClosed",
        _ => "InvalidMessage",
    }
}

impl Cdp {
    /// `_BrowserClient.start`: open the DevTools socket.
    /// `trace` records the extension relay's traffic; a DevTools socket records nothing to it.
    pub async fn connect(url: &str, trace: &fastbrowse_core::telemetry::Trace) -> Result<Self, BrowserError> {
        if let Some(record) = url.strip_prefix(crate::extension::SCHEME) {
            return Self::connect_extension(record, trace).await;
        }
        // The websocket's TLS provider for wss:// (cloud browsers); a second install is refused and harmless.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut config = WebSocketConfig::default();
        config.max_message_size = Some(MAX_MESSAGE_BYTES);
        config.max_frame_size = Some(MAX_MESSAGE_BYTES);
        // Nagle's algorithm holds a command written while an earlier one is unacknowledged, so the second and third
        // of a burst sent together waited a whole round trip more: 210ms, not 105, to a cloud browser.
        let connecting = tokio_tungstenite::connect_async_with_config(url, Some(config), true);
        let ws = match timeout(secs(OPEN_SECONDS), connecting).await {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(error)) if unreachable(&error) => {
                return Err(BrowserError::BrowserUnresponsive(format!(
                    "CDP.start failed ({}): the browser did not complete the DevTools handshake. {APPROVAL_HINT}",
                    handshake_detail(&error)
                )));
            }
            Ok(Err(error)) => {
                return Err(BrowserError::failed(
                    "CDP.start/handshake",
                    format!("CDP.start failed ({})", handshake_detail(&error)),
                ));
            }
            Err(_) => {
                return Err(BrowserError::BrowserUnresponsive(format!(
                    "CDP.start failed (TimeoutError): the browser did not complete the DevTools handshake. \
                     {APPROVAL_HINT}"
                )));
            }
        };
        let (mut sink, mut stream) = ws.split();
        let (out, mut outgoing) = mpsc::unbounded_channel::<Outgoing>();
        let inner = Arc::new(Inner {
            state: Mutex::default(),
            handlers: Mutex::default(),
            out,
            answered: AtomicBool::new(false),
            deadlines: Mutex::default(),
            pong: AtomicBool::new(true),
        });
        // Sending is a synchronous push onto this queue, so a dropped caller can never leave half a frame written.
        tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                let sent = match message {
                    Outgoing::Text(text) => sink.send(Message::text(text)).await,
                    Outgoing::Ping => sink.send(Message::Ping(Vec::new().into())).await,
                    Outgoing::Close => break,
                };
                if sent.is_err() {
                    return;
                }
            }
            let _ = sink.close().await;
        });
        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move {
            let why: Arc<str> = loop {
                let text = match stream.next().await {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Pong(_))) => {
                        if let Some(inner) = weak.upgrade() {
                            inner.pong.store(true, Ordering::Relaxed);
                        }
                        continue;
                    }
                    Some(Ok(Message::Close(_))) | None => break "ConnectionClosed".into(),
                    Some(Err(_)) => break "ConnectionClosed".into(),
                    Some(Ok(_)) => continue,
                };
                let Some(inner) = weak.upgrade() else { return };
                if let Ok(message) = serde_json::from_str::<Incoming>(&text) {
                    inner.dispatch(message);
                }
            };
            if let Some(inner) = weak.upgrade() {
                inner.close(why);
            }
        });
        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(secs(PING_SECONDS)).await;
                let Some(inner) = weak.upgrade() else { return };
                if inner.state.lock().unwrap().closed.is_some() {
                    return;
                }
                if !inner.pong.swap(false, Ordering::Relaxed) {
                    // websockets drops a connection whose keepalive went unanswered; a dead route is found here.
                    let _ = inner.out.send(Outgoing::Close);
                    inner.close("ConnectionClosed".into());
                    return;
                }
                let _ = inner.out.send(Outgoing::Ping);
            }
        });
        Ok(Self(inner))
    }

    /// Reach a browser through the fastbrowse extension: `record` is the path of its host's record, optionally
    /// followed by `#` and the run's task, which names the run's tab group.
    async fn connect_extension(record: &str, trace: &fastbrowse_core::telemetry::Trace) -> Result<Self, BrowserError> {
        let (path, task) = record.split_once('#').unwrap_or((record, ""));
        let text = std::fs::read_to_string(path).map_err(|_| {
            BrowserError::Unavailable("the fastbrowse extension is no longer connected: open the browser".into())
        })?;
        let host: crate::extension::protocol::HostRecord = serde_json::from_str(&text)
            .map_err(|_| BrowserError::Unavailable(format!("{path} is not a fastbrowse host record")))?;
        let endpoint = crate::extension::shim::connect(&host, task, trace.clone()).await?;
        let (out, mut outgoing) = mpsc::unbounded_channel::<Outgoing>();
        let inner = Arc::new(Inner {
            state: Mutex::default(),
            handlers: Mutex::default(),
            out,
            answered: AtomicBool::new(false),
            deadlines: Mutex::default(),
            pong: AtomicBool::new(true),
        });
        let commands = endpoint.commands;
        tokio::spawn(async move {
            // Liveness is checked by `Browser.getVersion`, which the shim answers with a round trip to the extension.
            while let Some(message) = outgoing.recv().await {
                match message {
                    Outgoing::Text(text) => {
                        if commands.send(text).is_err() {
                            return;
                        }
                    }
                    Outgoing::Ping => {}
                    Outgoing::Close => return,
                }
            }
        });
        let weak = Arc::downgrade(&inner);
        let mut replies = endpoint.replies;
        tokio::spawn(async move {
            let why: Arc<str> = loop {
                let Some(text) = replies.recv().await else { break "ConnectionClosed".into() };
                let Some(inner) = weak.upgrade() else { return };
                if let Ok(closed) = serde_json::from_str::<Value>(&text)
                    && let Some(why) = closed["closed"].as_str()
                {
                    if why == crate::extension::shim::CANCELLED {
                        let by = json!({"by": closed["by"]});
                        inner.dispatch(Incoming::event(crate::extension::STOPPED_EVENT, by));
                    }
                    break why.into();
                }
                if let Ok(message) = serde_json::from_str::<Incoming>(&text) {
                    inner.dispatch(message);
                }
            };
            if let Some(inner) = weak.upgrade() {
                inner.close(why);
            }
        });
        Ok(Self(inner))
    }

    /// One command with no deadline of its own: the reply, or why there is none.
    pub async fn call(&self, method: &str, params: Value, session: Option<&str>) -> Reply {
        let (tx, rx) = oneshot::channel();
        let id = {
            let mut state = self.0.state.lock().unwrap();
            if let Some(why) = &state.closed {
                return Err(CdpError::Closed(why.clone()));
            }
            state.next_id += 1;
            let id = state.next_id;
            state.pending.insert(id, Pending { session: session.map(Arc::from), tx });
            id
        };
        // Removes the entry however this future ends: replied, failed, or dropped mid-wait.
        let _pending = PendingGuard { inner: &self.0, id };
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = session.into();
        }
        if self.0.out.send(Outgoing::Text(message.to_string())).is_err() {
            return Err(self.0.closed_error());
        }
        rx.await.unwrap_or_else(|_| Err(self.0.closed_error()))
    }

    /// `_BrowserClient.send_raw`: a command under the reply, liveness and command deadlines, its failure mapped to
    /// the browser error a run reports.
    pub async fn send(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value, BrowserError> {
        let deadlines = session.map_or(0, |id| self.deadlines().get(id).copied().unwrap_or(0));
        if deadlines >= CDP_DEADLINES_BEFORE_STOP {
            return Err(BrowserError::PageUnresponsive(Failure::new(
                format!("{method}/not-sent"),
                format!(
                    "{method} not sent: the page stopped answering, {deadlines} commands already exceeded their deadline \
                 while the browser answered"
                ),
            )));
        }
        let reply = self.call(method, params, session);
        tokio::pin!(reply);
        let deadline = Instant::now() + secs(command_seconds(method));
        let outcome = loop {
            let wait = match (self.answered(), session) {
                (false, _) => CDP_ALIVE_SECONDS,
                (true, Some(_)) => CDP_PAGE_SECONDS,
                (true, None) => CDP_REPLY_SECONDS,
            };
            let until = (Instant::now() + secs(wait)).min(deadline);
            match timeout_at(until, &mut reply).await {
                Ok(outcome) => break Some(outcome),
                Err(_) if Instant::now() >= deadline => break None,
                Err(_) => {}
            }
            // The probe runs inside the command deadline, as Python's does under `asyncio.timeout`.
            let Ok(alive) = timeout_at(deadline, self.alive()).await else { break None };
            let page_answers = match session {
                Some(id) if alive => timeout_at(deadline, self.page_answers(id)).await.unwrap_or(false),
                _ => true,
            };
            // A reply that landed while a probe waited still counts, however the probe ended.
            if let Ok(outcome) = timeout(Duration::ZERO, &mut reply).await {
                break Some(outcome);
            }
            if !page_answers {
                break None;
            }
            if !alive {
                if !self.answered() {
                    return Err(BrowserError::BrowserUnresponsive(format!(
                        "{method} got no reply: the browser accepted the DevTools connection but has not answered \
                         anything on it. {APPROVAL_HINT}"
                    )));
                }
                return Err(BrowserError::BrowserUnresponsive(format!(
                    "{method} got no reply, and the browser stopped answering"
                )));
            }
        };
        let outcome = match outcome {
            Some(outcome) => outcome,
            None => {
                // The browser can go during the last wait, after the probe before it answered, so it is asked once
                // more: only a browser that is still there makes this the page's failure rather than an outage.
                match after_deadline(reply.as_mut(), self.alive()).await {
                    // The page answered while the browser was being asked: the command is done, however late.
                    Ok(outcome) => outcome,
                    Err(false) => {
                        return Err(BrowserError::BrowserUnresponsive(format!(
                            "{method} got no reply, and the browser stopped answering"
                        )));
                    }
                    Err(true) => {
                        if let Some(id) = session {
                            *self.deadlines().entry(id.to_string()).or_default() += 1;
                        }
                        return Err(BrowserError::PageUnresponsive(Failure::new(
                            format!("{method}/deadline"),
                            format!(
                                "{method} exceeded its command deadline; the page did not answer while the browser did"
                            ),
                        )));
                    }
                }
            }
        };
        match outcome {
            Ok(value) => {
                self.0.answered.store(true, Ordering::Relaxed);
                Ok(value)
            }
            Err(error) => {
                if matches!(error, CdpError::Protocol { .. }) {
                    // An error reply is an answer too: the browser is there and reading the connection.
                    self.0.answered.store(true, Ordering::Relaxed);
                }
                Err(browser_error(method, &error, true))
            }
        }
    }

    fn deadlines(&self) -> MutexGuard<'_, HashMap<String, u32>> {
        self.0.deadlines.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Count a deadline the caller met on `session` itself, as when it cancelled an evaluation the renderer did not
    /// answer in time, so `wedged` probes that renderer before the next navigation.
    pub fn missed_deadline(&self, session: &str) {
        *self.deadlines().entry(session.to_string()).or_default() += 1;
    }

    /// Whether this session's renderer is stuck, so a new target is needed. A met deadline alone does not say so: a
    /// slow server's `Page.navigate` meets one on a live renderer. Past `CDP_DEADLINES_BEFORE_STOP` its commands are
    /// refused anyway; below it the renderer is asked to evaluate once, outside the deadline count.
    pub async fn wedged(&self, session: &str) -> bool {
        let deadlines = self.deadlines().get(session).copied().unwrap_or(0);
        if deadlines == 0 {
            return false;
        }
        if deadlines >= CDP_DEADLINES_BEFORE_STOP {
            return true;
        }
        let probe = self.call("Runtime.evaluate", json!({"expression": "1"}), Some(session));
        // An error reply is an answer: mid-navigation the context is gone, but the renderer is live.
        !matches!(timeout(secs(CDP_ALIVE_SECONDS), probe).await, Ok(Ok(_) | Err(CdpError::Protocol { .. })))
    }

    fn answered(&self) -> bool {
        self.0.answered.load(Ordering::Relaxed)
    }

    async fn alive(&self) -> bool {
        // No answer, or a closed socket: either way nothing is there to reply.
        let answered = timeout(secs(CDP_ALIVE_SECONDS), self.call("Browser.getVersion", json!({}), None)).await;
        let alive = matches!(answered, Ok(Ok(_)));
        if alive {
            self.0.answered.store(true, Ordering::Relaxed);
        }
        alive
    }

    /// Whether the page behind `session` still runs anything: an error reply is an answer too, since the page read
    /// the command.
    async fn page_answers(&self, session: &str) -> bool {
        let probe = self.call("Runtime.evaluate", json!({"expression": "0"}), Some(session));
        matches!(timeout(secs(CDP_ALIVE_SECONDS), probe).await, Ok(Ok(_) | Err(CdpError::Protocol { .. })))
    }

    /// Run `handler` on every event, on the reader task, before any later message is routed. It must not block.
    pub fn on_event(&self, handler: Handler) {
        self.0.handlers.lock().unwrap().push(Arc::new(handler));
    }

    /// Calls still waiting for a reply.
    pub fn pending(&self) -> usize {
        self.0.state.lock().unwrap().pending.len()
    }

    /// `_BrowserClient.stop`: close the socket; every call still pending fails, and so does every later one.
    pub fn close(&self) {
        let _ = self.0.out.send(Outgoing::Close);
        self.0.close("ConnectionError".into());
        self.0.handlers.lock().unwrap().clear();
    }
}

/// `_browser_error`: a failure safe to report. A lost connection is an outage, not the run's failure, unless it was
/// lost while closing.
pub fn browser_error(method: &str, error: &CdpError, lost_is_outage: bool) -> BrowserError {
    match error {
        CdpError::Closed(why) if lost_is_outage => {
            BrowserError::BrowserUnresponsive(format!("{method} failed ({why})"))
        }
        CdpError::Closed(why) => BrowserError::failed(format!("{method}/{why}"), format!("{method} failed ({why})")),
        CdpError::Protocol { code: SERVER_ERROR, failure: Some(ProtocolFailure::NavigationDenied), .. }
            if method == "Page.navigate" =>
        {
            BrowserError::failed("Page.navigate/denied", "Page.navigate was denied by the browser access policy")
        }
        CdpError::Protocol { code: SERVER_ERROR, failure: Some(ProtocolFailure::NotAttached), .. } => {
            BrowserError::ContextLost(format!("{method} failed (CDP {SERVER_ERROR}, not attached to an active page)"))
        }
        CdpError::Protocol { code: SERVER_ERROR, reason, .. } => {
            BrowserError::ContextLost(protocol_failed(method, SERVER_ERROR, reason.as_deref()))
        }
        CdpError::Protocol { code, reason, .. } => {
            BrowserError::failed(format!("{method}/cdp-{code}"), protocol_failed(method, *code, reason.as_deref()))
        }
    }
}

fn protocol_failed(method: &str, code: i64, reason: Option<&str>) -> String {
    match reason {
        Some(reason) => format!("{method} failed (CDP {code}): {reason}"),
        None => format!("{method} failed (CDP {code})"),
    }
}

impl Inner {
    fn dispatch(&self, message: Incoming) {
        if let Some(id) = message.id {
            // A reply nobody waits for any more (its caller was dropped) is read and discarded here.
            let Some(Pending { tx, .. }) = self.state.lock().unwrap().pending.remove(&id) else { return };
            let reply = match message.error {
                Some(error) => {
                    let failure = error.failure.or_else(|| ProtocolFailure::of_chrome_message(&error.message));
                    Err(CdpError::Protocol { code: error.code, failure, reason: error.reason })
                }
                None => Ok(message.result),
            };
            let _ = tx.send(reply);
            return;
        }
        let Some(method) = message.method else { return };
        let event = Event { method, session: message.session_id, params: message.params };
        let handlers = self.handlers.lock().unwrap().clone();
        for handler in handlers {
            handler(&event);
        }
        // Chrome drops the replies a detached session still owed, while a command sent after the detach is refused at
        // once. A wait's hold on a frame removed from its page therefore ended or hung for good depending on which
        // came first: both now get the refusal. Sent after the handlers, so a caller it wakes finds the session gone.
        if event.method == "Target.detachedFromTarget"
            && let Some(child) = event.params["sessionId"].as_str()
        {
            // A session id that came back would otherwise inherit the deadlines its last target met.
            self.deadlines.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(child);
            let mut state = self.state.lock().unwrap();
            for (_, Pending { tx, .. }) in state.pending.extract_if(|_, p| p.session.as_deref() == Some(child)) {
                let _ = tx.send(Err(CdpError::Protocol { code: SESSION_NOT_FOUND, failure: None, reason: None }));
            }
        }
    }

    fn close(&self, why: Arc<str>) {
        let mut state = self.state.lock().unwrap();
        if state.closed.is_none() {
            state.closed = Some(why.clone());
        }
        for (_, Pending { tx, .. }) in state.pending.drain() {
            let _ = tx.send(Err(CdpError::Closed(why.clone())));
        }
    }

    fn closed_error(&self) -> CdpError {
        CdpError::Closed(self.state.lock().unwrap().closed.clone().unwrap_or_else(|| "ConnectionError".into()))
    }
}

struct PendingGuard<'a> {
    inner: &'a Inner,
    id: u64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.inner.state.lock().unwrap().pending.remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The error a browser reply with this body becomes.
    async fn error_of(response: serde_json::Value) -> CdpError {
        let (tx, rx) = oneshot::channel();
        let (out, _) = mpsc::unbounded_channel();
        let inner = Inner {
            state: Mutex::new(State {
                pending: HashMap::from([(7, Pending { session: None, tx })]),
                ..Default::default()
            }),
            handlers: Default::default(),
            out,
            answered: false.into(),
            deadlines: Default::default(),
            pong: false.into(),
        };
        inner.dispatch(serde_json::from_value(response).unwrap());
        rx.await.unwrap().unwrap_err()
    }

    /// A frame removed from its page detaches its session, and Chrome drops what that session owed: a wait's hold on it
    /// hung, while one sent a moment later was refused and ended the wait.
    #[tokio::test]
    async fn a_detached_session_refuses_what_it_owed_and_only_that() {
        let (gone, owed) = oneshot::channel();
        let (kept, other) = oneshot::channel();
        let (out, _) = mpsc::unbounded_channel();
        let pending = HashMap::from([
            (7, Pending { session: Some("frame".into()), tx: gone }),
            (8, Pending { session: Some("tab".into()), tx: kept }),
        ]);
        let inner = Inner {
            state: Mutex::new(State { pending, ..Default::default() }),
            handlers: Default::default(),
            out,
            answered: false.into(),
            deadlines: Default::default(),
            pong: false.into(),
        };
        inner.deadlines.lock().unwrap().extend([("frame".to_owned(), 2), ("tab".to_owned(), 1)]);
        // Handlers clear what they keep of the session first: a caller woken before them would find it still there.
        let owed = Arc::new(Mutex::new(owed));
        let seen = Arc::new(Mutex::new(None));
        let (reply, handled) = (owed.clone(), seen.clone());
        inner.handlers.lock().unwrap().push(Arc::new(Box::new(move |_: &Event| {
            *handled.lock().unwrap() = Some(reply.lock().unwrap().try_recv().is_err());
        })));
        inner.dispatch(Incoming::event("Target.detachedFromTarget", serde_json::json!({"sessionId": "frame"})));
        assert_eq!(*seen.lock().unwrap(), Some(true), "a caller was woken before the handlers ran");
        let refused = owed.lock().unwrap().try_recv().unwrap();
        assert!(matches!(refused, Err(CdpError::Protocol { code: SESSION_NOT_FOUND, .. })));
        assert_eq!(inner.state.lock().unwrap().pending.keys().collect::<Vec<_>>(), vec![&8]);
        // Deadlines met on a session that is gone say nothing about one that reuses its id.
        assert_eq!(*inner.deadlines.lock().unwrap(), HashMap::from([("tab".to_owned(), 1)]));
        drop(other);
    }

    #[tokio::test]
    async fn navigation_denial_is_typed_without_reporting_remote_text() {
        for (method, marked, denied) in
            [("Page.navigate", true, true), ("Page.navigate", false, false), ("Runtime.evaluate", true, false)]
        {
            let mut response = serde_json::json!({"id": 7, "error": {"code": -32000,
                "message": "private page text and https://private.test/"}});
            if marked {
                response["error"]["fastbrowseFailure"] = serde_json::json!("navigation_denied");
            }
            let public = browser_error(method, &error_of(response).await, true);
            assert_eq!(matches!(public, BrowserError::Failed(_)), denied);
            assert_eq!(matches!(public, BrowserError::ContextLost(_)), !denied);
            let text = public.to_string();
            assert!(!text.contains("private"));
            if denied {
                assert!(text.contains("access policy"));
            }
        }
    }

    /// Chrome's fixed refusal is named; the same words inside other text are page text and are not.
    #[tokio::test]
    async fn a_refusal_with_chromes_fixed_text_names_its_reason_and_nothing_else_of_the_message() {
        for (message, named) in [
            ("Not attached to an active page", true),
            ("Not attached to an active page: private page text", false),
            ("private page text", false),
        ] {
            let response = serde_json::json!({"id": 7, "error": {"code": -32000, "message": message}});
            let public = browser_error("Page.startScreencast", &error_of(response).await, true);
            assert!(matches!(public, BrowserError::ContextLost(_)));
            let text = public.to_string();
            assert_eq!(text.contains("not attached to an active page"), named, "{text}");
            assert!(!text.contains("private"), "{text}");
        }
    }

    #[tokio::test]
    async fn a_reason_the_extension_shim_wrote_is_reported_and_a_native_message_is_not() {
        // #1314: Chrome's refusal to open or attach a tab reached the run only as "Target.createTarget failed (CDP
        // -32000)", so twelve runs in a row failed with nothing saying why.
        for (reason, reported) in [(Some("Cannot attach to this target"), true), (None, false)] {
            let (tx, rx) = oneshot::channel();
            let (out, _) = mpsc::unbounded_channel();
            let inner = Inner {
                state: Mutex::new(State {
                    pending: HashMap::from([(7, Pending { session: None, tx })]),
                    ..Default::default()
                }),
                handlers: Default::default(),
                out,
                answered: false.into(),
                deadlines: Default::default(),
                pong: false.into(),
            };
            let mut response = serde_json::json!({"id": 7, "error": {"code": -32000, "message": "private page text"}});
            if let Some(reason) = reason {
                response["error"]["fastbrowseReason"] = reason.into();
            }
            inner.dispatch(serde_json::from_value(response).unwrap());
            let text = browser_error("Target.createTarget", &rx.await.unwrap().unwrap_err(), true).to_string();
            assert!(text.contains("Target.createTarget failed (CDP -32000)"), "{text}");
            assert_eq!(text.contains("Cannot attach to this target"), reported, "{text}");
            assert!(!text.contains("private"), "{text}");
        }
    }

    #[tokio::test]
    async fn a_reply_already_landed_at_the_deadline_is_the_answer() {
        let (tx, rx) = oneshot::channel::<u8>();
        tx.send(7).unwrap();
        tokio::pin!(rx);
        // The browser is not asked: the reply settles it.
        assert_eq!(after_deadline(rx.as_mut(), async { false }).await.map(Result::unwrap), Ok(7));
    }

    #[tokio::test]
    async fn a_reply_that_lands_while_the_browser_is_asked_is_the_answer() {
        let (tx, rx) = oneshot::channel::<u8>();
        tokio::pin!(rx);
        let alive = async move {
            tx.send(7).unwrap();
            true
        };
        assert_eq!(after_deadline(rx.as_mut(), alive).await.map(Result::unwrap), Ok(7));
    }

    #[tokio::test]
    async fn no_reply_reports_whether_the_browser_answered() {
        let (_tx, rx) = oneshot::channel::<u8>();
        tokio::pin!(rx);
        assert!(matches!(after_deadline(rx.as_mut(), async { true }).await, Err(true)));
    }
}
