//! The CDP connection: one reader task owns the websocket's read half, one writer task its write half, and
//! `send` is `session.py`'s `_BrowserClient.send_raw`, with its liveness probe and command deadlines.
//!
//! Replies route to callers by id through oneshot channels, so any number of calls are in flight at once. Events
//! go to handlers synchronously and in socket order, so a handler sees every event before the reply that follows
//! it on the socket is delivered: state a handler keeps (frames, dialogs, tabs) is current by the time a caller
//! wakes, as cdp-use's callbacks are.
//!
//! Dropping a `call` future removes its pending entry, so a late reply is read and discarded. When the socket
//! closes, every pending call and every later one fails with `CdpError::Closed`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, timeout, timeout_at};
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use crate::error::BrowserError;

/// How long a CDP command waits before the browser is asked whether it is still there. A cloud browser's proxy can
/// keep the socket open, answering pings, after losing the browser behind it, and a run with no wall-clock limit
/// would otherwise wait on that reply forever. A slow reply is not a lost one: `Page.navigate` waits for a slow
/// server's headers, so a live browser can keep waiting until `CDP_COMMAND_SECONDS`.
pub const CDP_REPLY_SECONDS: f64 = 60.0;
/// A responsive browser can still leave one document command pending forever.
pub const CDP_COMMAND_SECONDS: f64 = 120.0;
/// `Browser.getVersion` is answered at once by any browser that is there.
pub const CDP_ALIVE_SECONDS: f64 = 10.0;
/// Command deadlines one session may meet before its commands fail at once, as `PageUnresponsive`. A read
/// that waits out the deadline is often one whose caller treats a failed read as an unreadable page and carries on,
/// so the next read waited it out again: one login run met it thirteen times, 26 minutes on a page it could not
/// use. Browser-level commands (closing the run's tabs) are still sent.
pub const CDP_DEADLINES_BEFORE_STOP: u32 = 2;
/// The code Chrome answers when the document or frame a command addressed is gone, as after a navigation.
pub const SERVER_ERROR: i64 = -32000;

const APPROVAL_HINT: &str = "If this is a Chrome you started yourself, it may be waiting for you to approve remote \
                             debugging, or the address may be stale; check the browser window and the address, then \
                             retry.";
/// websockets' handshake deadline, which cdp-use connects with.
const OPEN_SECONDS: f64 = 10.0;
/// websockets' keepalive: a ping every 20 seconds, and the connection is dropped when one goes 20 seconds unanswered.
const PING_SECONDS: f64 = 20.0;
/// cdp-use's frame bound: a full-page screenshot or a large document's evaluation fits under it.
const MAX_MESSAGE_BYTES: usize = 100 * 1024 * 1024;
/// Statuses that say nothing about the request, so a repeat can clear them (`clients/validation.py`).
pub const RETRYABLE_STATUS: [u16; 12] = [408, 429, 500, 502, 503, 504, 520, 521, 522, 523, 524, 529];

pub fn secs(seconds: f64) -> Duration {
    Duration::from_secs_f64(seconds)
}

#[derive(Debug, Clone)]
pub enum CdpError {
    /// The connection is gone. Carries why, the same for every call it failed.
    Closed(Arc<str>),
    /// The browser answered with an error. Its message is kept out: it can quote page text or evaluated source.
    Protocol { code: i64 },
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

#[derive(Deserialize)]
struct RemoteError {
    code: i64,
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
    pending: HashMap<u64, oneshot::Sender<Reply>>,
    closed: Option<Arc<str>>,
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
    /// Commands that outlived `CDP_COMMAND_SECONDS`, by the session they addressed. A cloud browser's connection
    /// carries every target the run attached, third-party frames and workers included, and one of those hanging says
    /// nothing about the tab the run drives: on The Pi Hut a frame's `Page.addScriptToEvaluateOnNewDocument` met its
    /// deadline twice and every command to the answering main tab was refused.
    deadlines: Mutex<HashMap<String, u32>>,
    pong: AtomicBool,
}

#[derive(Clone)]
pub struct Cdp(Arc<Inner>);

/// A handler holds this, not a `Cdp`, so the connection does not own a reference to itself.
#[derive(Clone)]
pub struct WeakCdp(Weak<Inner>);

impl WeakCdp {
    pub fn upgrade(&self) -> Option<Cdp> {
        self.0.upgrade().map(Cdp)
    }
}

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
    pub async fn connect(url: &str) -> Result<Self, BrowserError> {
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
                return Err(BrowserError::Failed(format!("CDP.start failed ({})", handshake_detail(&error))));
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

    pub fn downgrade(&self) -> WeakCdp {
        WeakCdp(Arc::downgrade(&self.0))
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
            state.pending.insert(id, tx);
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
            return Err(BrowserError::PageUnresponsive(format!(
                "{method} not sent: the page stopped answering, {deadlines} commands already exceeded their deadline \
                 while the browser answered"
            )));
        }
        let reply = self.call(method, params, session);
        tokio::pin!(reply);
        let deadline = Instant::now() + secs(CDP_COMMAND_SECONDS);
        let outcome = loop {
            let wait = if self.answered() { CDP_REPLY_SECONDS } else { CDP_ALIVE_SECONDS };
            let until = (Instant::now() + secs(wait)).min(deadline);
            match timeout_at(until, &mut reply).await {
                Ok(outcome) => break Some(outcome),
                Err(_) if Instant::now() >= deadline => break None,
                Err(_) => {}
            }
            // The probe runs inside the command deadline, as Python's does under `asyncio.timeout`.
            let Ok(alive) = timeout_at(deadline, self.alive()).await else { break None };
            // A reply that landed while the probe waited still counts, however the probe ended.
            if let Ok(outcome) = timeout(Duration::ZERO, &mut reply).await {
                break Some(outcome);
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
                let mut alive = true;
                if timeout(Duration::ZERO, &mut reply).await.is_err() {
                    alive = self.alive().await;
                }
                if let Ok(outcome) = timeout(Duration::ZERO, &mut reply).await {
                    // The page answered while the browser was being asked: the command is done, however late.
                    outcome
                } else if !alive {
                    return Err(BrowserError::BrowserUnresponsive(format!(
                        "{method} got no reply, and the browser stopped answering"
                    )));
                } else {
                    if let Some(id) = session {
                        *self.deadlines().entry(id.to_string()).or_default() += 1;
                    }
                    return Err(BrowserError::PageUnresponsive(format!(
                        "{method} exceeded its command deadline; the page did not answer while the browser did"
                    )));
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

    /// Run `handler` on every event, on the reader task, before any later message is routed. It must not block.
    pub fn on_event(&self, handler: Handler) {
        self.0.handlers.lock().unwrap().push(Arc::new(handler));
    }

    /// Calls still waiting for a reply.
    pub fn pending(&self) -> usize {
        self.0.state.lock().unwrap().pending.len()
    }

    pub fn is_closed(&self) -> bool {
        self.0.state.lock().unwrap().closed.is_some()
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
        CdpError::Closed(why) => BrowserError::Failed(format!("{method} failed ({why})")),
        CdpError::Protocol { code: SERVER_ERROR } => {
            BrowserError::ContextLost(format!("{method} failed (CDP {SERVER_ERROR})"))
        }
        CdpError::Protocol { code } => BrowserError::Failed(format!("{method} failed (CDP {code})")),
    }
}

impl Inner {
    fn dispatch(&self, message: Incoming) {
        if let Some(id) = message.id {
            // A reply nobody waits for any more (its caller was dropped) is read and discarded here.
            let Some(tx) = self.state.lock().unwrap().pending.remove(&id) else { return };
            let reply = match message.error {
                Some(error) => Err(CdpError::Protocol { code: error.code }),
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
    }

    fn close(&self, why: Arc<str>) {
        let mut state = self.state.lock().unwrap();
        if state.closed.is_none() {
            state.closed = Some(why.clone());
        }
        for (_, tx) in state.pending.drain() {
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
