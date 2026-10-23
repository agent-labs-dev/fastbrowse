//! The CDP connection: one reader task owns the websocket's read half, one writer task its write half.
//!
//! Replies route to callers by id through oneshot channels, so any number of calls are in flight at once. Events
//! go first to handlers, synchronously and in socket order, then to channel subscribers matching their
//! (method, session). A handler sees every event before the reply that follows it on the socket is delivered, so
//! state a handler keeps (frames, dialogs) is current by the time a caller wakes, as cdp-use's callbacks are.
//!
//! Dropping a `call` future removes its pending entry, so a late reply is read and discarded. When the socket
//! closes, every pending call and every later one fails with `CdpError::Closed`, and subscriptions end.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, Weak};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

/// The code Chrome answers when the document or frame a command addressed is gone, as after a navigation.
const SERVER_ERROR: i64 = -32000;

#[derive(Debug, Clone)]
pub enum CdpError {
    /// The connection is gone. Carries why, the same for every call it failed.
    Closed(Arc<str>),
    /// The browser answered with an error. Its message is kept out of `Display`: it can quote page text.
    Protocol { method: String, code: i64 },
}

impl CdpError {
    /// Python's `ContextLost`: the read is worth retrying on the next document.
    pub fn context_lost(&self) -> bool {
        matches!(self, Self::Protocol { code: SERVER_ERROR, .. })
    }
}

impl fmt::Display for CdpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed(why) => write!(f, "the browser connection closed ({why})"),
            Self::Protocol { method, code } => write!(f, "{method} failed (CDP {code})"),
        }
    }
}

impl std::error::Error for CdpError {}

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

struct Subscriber {
    method: String,
    session: Option<String>,
    tx: mpsc::UnboundedSender<Event>,
}

#[derive(Default)]
struct State {
    next_id: u64,
    pending: HashMap<u64, (String, oneshot::Sender<Reply>)>,
    subscribers: Vec<Subscriber>,
    closed: Option<Arc<str>>,
}

struct Inner {
    state: Mutex<State>,
    /// Separate from `state` so a handler can call, subscribe or spawn without deadlocking the reader.
    handlers: Mutex<Vec<Arc<Handler>>>,
    out: mpsc::UnboundedSender<String>,
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

impl Cdp {
    pub async fn connect(url: &str) -> Result<Self, CdpError> {
        let (ws, _) = connect_async(url).await.map_err(|e| CdpError::Closed(e.to_string().into()))?;
        let (mut sink, mut stream) = ws.split();
        let (out, mut outgoing) = mpsc::unbounded_channel::<String>();
        let inner = Arc::new(Inner { state: Mutex::default(), handlers: Mutex::default(), out });
        // Sending is a synchronous push onto this queue, so a dropped caller can never leave half a frame written.
        tokio::spawn(async move {
            while let Some(text) = outgoing.recv().await {
                if sink.send(Message::text(text)).await.is_err() {
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
                    Some(Ok(Message::Close(_))) | None => break "closed by the browser".into(),
                    Some(Err(e)) => break e.to_string().into(),
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
        Ok(Self(inner))
    }

    pub fn downgrade(&self) -> WeakCdp {
        WeakCdp(Arc::downgrade(&self.0))
    }

    pub async fn call(&self, method: &str, params: Value, session: Option<&str>) -> Reply {
        let (tx, rx) = oneshot::channel();
        let id = {
            let mut state = self.0.state.lock().unwrap();
            if let Some(why) = &state.closed {
                return Err(CdpError::Closed(why.clone()));
            }
            state.next_id += 1;
            let id = state.next_id;
            state.pending.insert(id, (method.to_owned(), tx));
            id
        };
        // Removes the entry however this future ends: replied, failed, or dropped mid-wait.
        let _pending = PendingGuard { inner: &self.0, id };
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = session.into();
        }
        if self.0.out.send(message.to_string()).is_err() {
            return Err(self.0.closed_error());
        }
        rx.await.unwrap_or_else(|_| Err(self.0.closed_error()))
    }

    /// Events named `method` on `session` (`None`: the browser's own), from now on, in socket order. Subscribe
    /// before the call that causes the event, or it can arrive first. Ends when the connection closes.
    pub fn subscribe(&self, method: &str, session: Option<&str>) -> mpsc::UnboundedReceiver<Event> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut state = self.0.state.lock().unwrap();
        if state.closed.is_none() {
            state.subscribers.push(Subscriber { method: method.into(), session: session.map(Into::into), tx });
        }
        rx
    }

    /// Run `handler` on every event, on the reader task, before any later message is routed. It must not block.
    pub fn on_event(&self, handler: Handler) {
        self.0.handlers.lock().unwrap().push(Arc::new(handler));
    }

    /// Calls still waiting for a reply.
    pub fn pending(&self) -> usize {
        self.0.state.lock().unwrap().pending.len()
    }
}

impl Inner {
    fn dispatch(&self, message: Incoming) {
        if let Some(id) = message.id {
            // A reply nobody waits for any more (its caller was dropped) is read and discarded here.
            let Some((method, tx)) = self.state.lock().unwrap().pending.remove(&id) else { return };
            let reply = match message.error {
                Some(error) => Err(CdpError::Protocol { method, code: error.code }),
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
        let mut state = self.state.lock().unwrap();
        state.subscribers.retain(|s| {
            if s.method == event.method && s.session == event.session {
                return s.tx.send(event.clone()).is_ok();
            }
            !s.tx.is_closed()
        });
    }

    fn close(&self, why: Arc<str>) {
        let mut state = self.state.lock().unwrap();
        state.closed = Some(why.clone());
        for (_, (_, tx)) in state.pending.drain() {
            let _ = tx.send(Err(CdpError::Closed(why.clone())));
        }
        state.subscribers.clear();
    }

    fn closed_error(&self) -> CdpError {
        CdpError::Closed(self.state.lock().unwrap().closed.clone().unwrap_or_else(|| "writer stopped".into()))
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
