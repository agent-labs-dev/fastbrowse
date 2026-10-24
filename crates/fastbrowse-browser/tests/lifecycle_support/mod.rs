//! test_lifecycle.py's `CdpTransport`, for the Rust suites: a DevTools websocket on 127.0.0.1 that records every
//! command and answers it from a script the test edits as it goes. Python patches cdp-use's client in place; the Rust
//! session can only be handed an address, so the script sits behind a real socket instead.
//!
//! What Python reads off a cancelled send (`transport.finished`) has no wire form: a dropped call only forgets its id.
//! Tests read that from the client instead, as `Cdp::pending`.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use fastbrowse_browser::session::{BrowserSession, SessionOptions};
use fastbrowse_core::models::BrowserConnection;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

use crate::common::RecordingArtifactSink;

/// How a scripted command is answered.
#[derive(Clone, Debug)]
pub enum Reply {
    Result(Value),
    /// A CDP error reply, its message "secret": a message must never reach what the session reports.
    Error(i64),
    /// The socket drops instead of answering, as Python's `ConnectionError` from the transport.
    Close,
}

/// `RuntimeError({"code": -32000, "message": "secret"})`.
pub const SECRET: Reply = Reply::Error(-32000);

pub type Hook = Arc<dyn Fn(&Value) + Send + Sync>;

struct Held {
    id: Value,
    method: String,
    out: mpsc::UnboundedSender<Message>,
}

#[derive(Default)]
pub struct Script {
    /// Methods in the order they arrived, and "stop" when the client closed the socket.
    pub calls: Vec<String>,
    pub requests: Vec<(String, Value, Option<String>)>,
    pub failures: HashMap<String, Reply>,
    /// Commands left unanswered until `release`.
    pub blocked: HashSet<String>,
    pub delays: HashMap<String, Duration>,
    /// Answers taken in order before the default, as `transport.results`.
    pub results: HashMap<String, VecDeque<Reply>>,
    hooks: Vec<(String, Hook)>,
    held: Vec<Held>,
}

impl Script {
    pub fn count(&self, method: &str) -> usize {
        self.calls.iter().filter(|m| *m == method).count()
    }

    pub fn index(&self, method: &str) -> usize {
        self.calls.iter().position(|m| m == method).unwrap_or_else(|| panic!("{method} was never sent"))
    }

    /// `transport.requests.count((method, params, session))`.
    pub fn requested(&self, method: &str, params: &Value, session: Option<&str>) -> usize {
        self.requests.iter().filter(|(m, p, s)| m == method && p == params && s.as_deref() == session).count()
    }

    pub fn params_of(&self, method: &str) -> Vec<Value> {
        self.requests.iter().filter(|(m, _, _)| m == method).map(|(_, p, _)| p.clone()).collect()
    }

    pub fn queue(&mut self, method: &str, results: impl IntoIterator<Item = Value>) {
        self.results.entry(method.to_owned()).or_default().extend(results.into_iter().map(Reply::Result));
    }
}

pub struct FakeChrome {
    script: Arc<Mutex<Script>>,
    seen: watch::Sender<u64>,
    pub url: String,
}

fn default_result(method: &str) -> Value {
    match method {
        "Target.createTarget" => json!({"targetId": "owned"}),
        "Target.attachToTarget" => json!({"sessionId": "session"}),
        "Runtime.evaluate" => json!({"result": {"value": "loading"}}),
        _ => json!({}),
    }
}

fn answer(out: &mpsc::UnboundedSender<Message>, id: &Value, reply: Reply, dropped: &AtomicBool) {
    let message = match reply {
        Reply::Result(result) => json!({"id": id, "result": result}),
        Reply::Error(code) => json!({"id": id, "error": {"code": code, "message": "secret"}}),
        Reply::Close => {
            dropped.store(true, Ordering::SeqCst);
            let _ = out.send(Message::Close(None));
            return;
        }
    };
    let _ = out.send(Message::text(message.to_string()));
}

impl FakeChrome {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("ws://{}/devtools/browser/fake", listener.local_addr().expect("addr"));
        let script = Arc::new(Mutex::new(Script::default()));
        let seen = watch::channel(0).0;
        let (accepting, notify) = (script.clone(), seen.clone());
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, accepting.clone(), notify.clone()));
            }
        });
        Self { script, seen, url }
    }

    pub fn script(&self) -> MutexGuard<'_, Script> {
        self.script.lock().unwrap()
    }

    pub fn calls(&self) -> Vec<String> {
        self.script().calls.clone()
    }

    pub fn fail(&self, method: &str, reply: Reply) {
        self.script().failures.insert(method.to_owned(), reply);
    }

    pub fn block(&self, method: &str) {
        self.script().blocked.insert(method.to_owned());
    }

    pub fn delay(&self, method: &str, seconds: f64) {
        self.script().delays.insert(method.to_owned(), Duration::from_secs_f64(seconds));
    }

    pub fn queue(&self, method: &str, results: impl IntoIterator<Item = Value>) {
        self.script().queue(method, results);
    }

    /// Run `hook` with each `method` command's params as it arrives, before it is answered.
    pub fn on_call(&self, method: &str, hook: Hook) {
        self.script().hooks.push((method.to_owned(), hook));
    }

    /// Answer every held `method` command with `reply`, and stop holding new ones.
    pub fn release(&self, method: &str, reply: Reply) {
        let held = {
            let mut script = self.script();
            script.blocked.remove(method);
            let (released, kept) = std::mem::take(&mut script.held).into_iter().partition(|h| h.method == method);
            script.held = kept;
            released
        };
        let dropped = AtomicBool::new(false);
        for Held { id, out, .. } in held {
            answer(&out, &id, reply.clone(), &dropped);
        }
    }

    /// Wait, up to five seconds, for `check` to hold of the script.
    pub async fn wait_until(&self, what: &str, check: impl Fn(&Script) -> bool) {
        let mut seen = self.seen.subscribe();
        let waited = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                seen.borrow_and_update();
                if check(&self.script()) {
                    return;
                }
                if seen.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
        assert!(waited.is_ok(), "timed out waiting for {what}");
    }

    /// `await started.wait()` on a blocked command: the session has sent it.
    pub async fn sent(&self, method: &str) {
        self.wait_until(method, |s| s.calls.iter().any(|m| m == method)).await;
    }

    /// The client closed its socket.
    pub async fn stopped(&self) {
        self.wait_until("stop", |s| s.calls.last().is_some_and(|m| m == "stop")).await;
    }

    /// `CONNECTION`: what headless local Chrome yields, where no window follows the tab, so the run activates it.
    pub fn connection(&self) -> BrowserConnection {
        BrowserConnection { foreground: true, ..BrowserConnection::new(self.url.clone(), false) }
    }

    /// `BACKGROUND`: a visible window someone sits in front of.
    pub fn background(&self) -> BrowserConnection {
        BrowserConnection { foreground: false, ..self.connection() }
    }

    pub fn new_session(&self, connection: BrowserConnection, options: SessionOptions) -> BrowserSession {
        BrowserSession::new(connection, Arc::new(RecordingArtifactSink::default()), options)
    }

    /// `async with BrowserSession(connection, RecordingArtifactSink()) as session`, entered.
    pub async fn session_with(&self, connection: BrowserConnection, options: SessionOptions) -> BrowserSession {
        let session = self.new_session(connection, options);
        session.open().await.expect("session opens");
        session
    }

    pub async fn session(&self, connection: BrowserConnection) -> BrowserSession {
        self.session_with(connection, SessionOptions::default()).await
    }
}

async fn serve(stream: TcpStream, script: Arc<Mutex<Script>>, seen: watch::Sender<u64>) {
    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else { return };
    let (mut sink, mut source) = ws.split();
    let (out, mut outgoing) = mpsc::unbounded_channel::<Message>();
    tokio::spawn(async move {
        while let Some(message) = outgoing.recv().await {
            let last = matches!(message, Message::Close(_));
            if sink.send(message).await.is_err() || last {
                break;
            }
        }
        let _ = sink.close().await;
    });
    let dropped = Arc::new(AtomicBool::new(false));
    while let Some(Ok(message)) = source.next().await {
        match message {
            Message::Text(text) => {
                if let Ok(request) = serde_json::from_str::<Value>(&text) {
                    handle(&request, &script, &seen, &out, &dropped);
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
        if dropped.load(Ordering::SeqCst) {
            break;
        }
    }
    if !dropped.load(Ordering::SeqCst) {
        script.lock().unwrap().calls.push("stop".into());
        seen.send_modify(|n| *n += 1);
    }
}

fn handle(
    request: &Value,
    script: &Arc<Mutex<Script>>,
    seen: &watch::Sender<u64>,
    out: &mpsc::UnboundedSender<Message>,
    dropped: &Arc<AtomicBool>,
) {
    let id = request["id"].clone();
    let method = request["method"].as_str().unwrap_or_default().to_owned();
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let session = request["sessionId"].as_str().map(str::to_owned);
    let (reply, delay, hooks) = {
        let mut script = script.lock().unwrap();
        script.calls.push(method.clone());
        script.requests.push((method.clone(), params.clone(), session));
        let hooks: Vec<Hook> = script.hooks.iter().filter(|(m, _)| *m == method).map(|(_, h)| h.clone()).collect();
        let reply = match script.failures.get(&method) {
            Some(failure) => Some(failure.clone()),
            None => script.results.get_mut(&method).and_then(VecDeque::pop_front),
        };
        (reply, script.delays.get(&method).copied(), hooks)
    };
    seen.send_modify(|n| *n += 1);
    for hook in hooks {
        hook(&params);
    }
    if let Some(reply) = reply {
        answer(out, &id, reply, dropped);
        return;
    }
    let (script, out, dropped) = (script.clone(), out.clone(), dropped.clone());
    tokio::spawn(async move {
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        // Read when the answer is due, as Python's transport does: a test can start holding a method mid-command.
        {
            let mut script = script.lock().unwrap();
            if script.blocked.contains(&method) {
                script.held.push(Held { id, method, out });
                return;
            }
        }
        answer(&out, &id, Reply::Result(default_result(&method)), &dropped);
    });
}
