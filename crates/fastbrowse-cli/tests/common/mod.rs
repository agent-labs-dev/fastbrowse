//! A fake client for `fastbrowse serve` on in-memory streams, and the stand-in runs the serve tests share: the Rust
//! side of `tests/serve_client.py` and of the recorders in `tests/test_serve_*.py`.
//!
//! The client speaks to the server in lines of JSON, as the JavaScript SDK does, so a test asserts what a client on
//! the other side of the pipe would see and never looks inside the server.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fastbrowse_cli::runner::{Host, RunError, RunTask, Runner};
use fastbrowse_cli::serve::{Server, SettingsSource, Transport};
use fastbrowse_clients::environment::{Secret, Settings};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::models::{
    BrowserEvent, BrowserTag, CostBreakdown, Decider, Event, Operation, RunResult, Status, StepEvent, StepOutcome,
    StepResult, StepTag,
};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc};
use tokio::task::JoinHandle;

/// A reply that has not come after this long never will.
pub const PATIENCE: Duration = Duration::from_secs(5);
pub const START: &str = "https://shop.example.com/cart";
/// Any program that exists stands in for Chrome, which the server only looks for.
pub const CHROME: &str = "/bin/sh";

/// Both directions of the pipe as queues of lines. None on the inbound one is end of input.
pub struct MemoryTransport {
    to_server: AsyncMutex<mpsc::UnboundedReceiver<Option<Vec<u8>>>>,
    to_client: mpsc::UnboundedSender<Vec<u8>>,
}

impl Transport for MemoryTransport {
    fn receive(&self) -> BoxFuture<'_, Option<Vec<u8>>> {
        Box::pin(async move { self.to_server.lock().await.recv().await.flatten() })
    }

    fn send(&self, line: &[u8]) {
        let _ = self.to_client.send(line.to_vec());
    }
}

pub struct FakeClient {
    to_server: mpsc::UnboundedSender<Option<Vec<u8>>>,
    from_server: mpsc::UnboundedReceiver<Vec<u8>>,
    serving: Option<JoinHandle<i32>>,
    ids: i64,
    /// What the server sent that was not the reply being waited for, in the order it arrived.
    pub inbox: Vec<Value>,
    /// Every line the server wrote, in order.
    pub written: Vec<u8>,
    held: VecDeque<Vec<u8>>,
}

impl FakeClient {
    pub fn send_line(&self, line: &[u8]) {
        let _ = self.to_server.send(Some(line.to_vec()));
    }

    pub fn send(&self, message: Value) {
        self.send_line(message.to_string().as_bytes());
    }

    pub async fn receive_line(&mut self) -> Vec<u8> {
        if let Some(line) = self.held.pop_front() {
            return line;
        }
        let line = tokio::time::timeout(PATIENCE, self.from_server.recv())
            .await
            .expect("the server answered in time")
            .expect("the server is still there");
        self.written.extend_from_slice(&line);
        line
    }

    pub fn parse(line: &[u8]) -> Value {
        assert!(line.ends_with(b"\n") && !line[..line.len() - 1].contains(&b'\n'), "not one line: {line:?}");
        serde_json::from_slice(line).expect("a line of JSON")
    }

    pub async fn receive(&mut self) -> Value {
        let line = self.receive_line().await;
        FakeClient::parse(&line)
    }

    /// Answer one of the server's own requests.
    pub fn respond(&self, request: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": request["id"], "result": result}));
    }

    /// Answer one of the server's own requests with an error, as a client whose callback threw does.
    pub fn refuse(&self, request: &Value, code: i64, message: &str) {
        self.send(json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": code, "message": message}}));
    }

    /// Send a request and return its id without waiting.
    pub fn call(&mut self, method: &str, params: Option<Value>) -> i64 {
        self.ids += 1;
        let mut message = json!({"jsonrpc": "2.0", "id": self.ids, "method": method});
        if let Some(params) = params {
            message["params"] = params;
        }
        self.send(message);
        self.ids
    }

    /// The response to one request. Anything that arrives ahead of it is kept in `inbox`.
    pub async fn reply(&mut self, id: i64) -> Value {
        loop {
            let message = self.receive().await;
            if message.get("method").is_none() && message.get("id") == Some(&json!(id)) {
                return message;
            }
            self.inbox.push(message);
        }
    }

    pub async fn request(&mut self, method: &str, params: Option<Value>) -> Value {
        let id = self.call(method, params);
        self.reply(id).await
    }

    pub async fn result(&mut self, method: &str, params: Option<Value>) -> Value {
        let response = self.request(method, params).await;
        assert!(response.get("error").is_none(), "{response}");
        response["result"].clone()
    }

    /// End of input, which is what the server sees when its parent closes the pipe or dies.
    pub fn close_input(&self) {
        let _ = self.to_server.send(None);
    }

    /// Wait for the server to stop, and return what the process would exit with.
    pub async fn exit_code(&mut self) -> i32 {
        let serving = self.serving.take().expect("the server was running");
        tokio::time::timeout(PATIENCE, serving).await.expect("the server stopped in time").expect("no panic")
    }

    /// Whether the server has written nothing that is still unread, after giving it a moment to.
    pub async fn silent(&mut self) -> bool {
        tokio::time::sleep(Duration::from_millis(50)).await;
        match self.from_server.try_recv() {
            Ok(line) => {
                self.written.extend_from_slice(&line);
                self.held.push_back(line);
                false
            }
            Err(_) => self.held.is_empty(),
        }
    }

    /// The server's next request of `method`, left unanswered.
    pub async fn asked(&mut self, method: &str) -> Value {
        loop {
            let message = self.receive().await;
            if message.get("method") == Some(&json!(method)) && message.get("id").is_some() {
                return message;
            }
        }
    }
}

/// A running server and a client connected to it.
pub fn serving(runner: Arc<dyn Runner>, settings: SettingsSource) -> FakeClient {
    let (to_server, server_inbox) = mpsc::unbounded_channel();
    let (to_client, from_server) = mpsc::unbounded_channel();
    let transport = Arc::new(MemoryTransport { to_server: AsyncMutex::new(server_inbox), to_client });
    let server = Server::new(transport, runner, settings);
    let serving = tokio::spawn(async move { server.serve().await });
    FakeClient {
        to_server,
        from_server,
        serving: Some(serving),
        ids: 0,
        inbox: Vec::new(),
        written: Vec::new(),
        held: VecDeque::new(),
    }
}

/// Every key set and nothing read from a checkout's `.env` or the environment of whoever runs the tests.
pub fn settings_with(change: impl Fn(&mut Settings) + Send + Sync + 'static) -> SettingsSource {
    Arc::new(move || {
        let mut settings = Settings {
            openrouter_api_key: Some(Secret::new("or")),
            ai_gateway_api_key: Some(Secret::new("gw")),
            browser_use_api_key: Some(Secret::new("bu")),
            chrome: Some(CHROME.into()),
            ..Settings::default()
        };
        change(&mut settings);
        Ok(settings)
    })
}

pub fn settings() -> SettingsSource {
    settings_with(|_| {})
}

pub fn result(status: Status) -> RunResult {
    RunResult {
        status,
        budget: None,
        answer: Some("42".into()),
        data: Value::Null,
        evidence: Vec::new(),
        steps: Vec::new(),
        cost: CostBreakdown { lines: Vec::new() },
        artifacts: Vec::new(),
        citations: Vec::new(),
        error: None,
        final_url: None,
        final_frame: None,
        recordings: Vec::new(),
        wall: None,
    }
}

pub fn step(index: i64) -> StepResult {
    StepResult {
        index,
        operation: Operation::Click,
        decided_by: Decider::Jev,
        outcome: StepOutcome::Executed,
        url: START.into(),
        target: Some("Add to cart".into()),
        confidence: None,
        note: None,
        facts: Vec::new(),
        page_changed: None,
        duration_ms: 5,
    }
}

pub fn browser_event(live_url: Option<&str>, browser_id: Option<&str>) -> Event {
    Event::Browser(BrowserEvent {
        kind: BrowserTag,
        live_url: live_url.map(str::to_string),
        browser_id: browser_id.map(str::to_string),
    })
}

pub fn step_event(index: i64, frame: Option<Vec<u8>>) -> Event {
    Event::Step(StepEvent { kind: StepTag, step: step(index), frame: frame.map(fastbrowse_core::models::Image) })
}

pub fn run_params(run_id: &str, fields: Value) -> Value {
    let mut params = json!({"run_id": run_id, "task": "Add the kettle to the cart"});
    if let Value::Object(fields) = fields {
        params.as_object_mut().unwrap().extend(fields);
    }
    params
}

/// What a runner was called with: the run, and what hosts it.
#[derive(Clone)]
pub struct Call {
    pub task: RunTask,
    pub host: Host,
}

/// Stands where `run_task` does, and keeps what it was called with.
#[derive(Clone, Default)]
pub struct Recorder {
    pub calls: Arc<Mutex<Vec<Call>>>,
    pub result: Option<RunResult>,
    pub events: Vec<Event>,
    pub hold: Option<Arc<Notify>>,
}

impl Recorder {
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    pub fn runner(&self) -> Arc<dyn Runner> {
        let recorder = self.clone();
        Arc::new(move |task: RunTask, host: Host| -> BoxFuture<'static, Result<RunResult, RunError>> {
            let recorder = recorder.clone();
            Box::pin(async move {
                recorder.calls.lock().unwrap().push(Call { task, host: host.clone() });
                for event in recorder.events.clone() {
                    if let Some(on_event) = &host.on_event {
                        on_event(event).await;
                    }
                }
                if let Some(hold) = &recorder.hold {
                    host.cancel.guard(hold.notified()).await?;
                }
                Ok(recorder.result.clone().unwrap_or_else(|| result(Status::Complete)))
            })
        })
    }
}

/// A runner from an async closure.
pub fn runner<F, Fut>(run: F) -> Arc<dyn Runner>
where
    F: Fn(Call) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<RunResult, RunError>> + Send + 'static,
{
    Arc::new(move |task: RunTask, host: Host| -> BoxFuture<'static, Result<RunResult, RunError>> {
        Box::pin(run(Call { task, host }))
    })
}

/// The binary under test.
pub fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_fastbrowse")
}
