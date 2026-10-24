//! Stand-ins for `run_task` that `fastbrowse serve --stdio --run-task NAME` runs, and the altered servers the SDK's
//! tests start, so a test of the binary needs no browser and no model.
//!
//! They are the Rust twins of `tests/serve_scripts.py` and `packages/sdk/test/servers/*.py`, named as Python names
//! them (`tests.serve_scripts:echo`, `scripted_run:echo`), so one transcript drives either binary.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use fastbrowse_core::BoxFuture;
use fastbrowse_core::models::{
    BrowserEvent, BrowserTag, Decider, Event, Operation, RunResult, Status, StepEvent, StepOutcome, StepResult, StepTag,
};
use fastbrowse_core::safety::{origin_of, resolve_secret};
use serde_json::{Value, json};

use crate::runner::{RunArgs, RunError, Runner};
use crate::serve::Transport;

type Outcome = Result<RunResult, RunError>;

/// The params of each `run` request, as the client wrote them and before the server has read anything into them.
pub type Requests = Arc<Mutex<Vec<Value>>>;

fn result(status: Status) -> RunResult {
    RunResult { status, error: None, ..crate::options::error_result("") }
}

fn browser(live_url: Option<&str>, browser_id: Option<&str>) -> Event {
    Event::Browser(BrowserEvent {
        kind: BrowserTag,
        live_url: live_url.map(str::to_string),
        browser_id: browser_id.map(str::to_string),
    })
}

fn step(index: i64) -> Event {
    Event::Step(StepEvent {
        kind: StepTag,
        step: StepResult {
            index,
            operation: Operation::Click,
            decided_by: Decider::Jev,
            outcome: StepOutcome::Executed,
            url: "https://shop.example/".into(),
            target: None,
            confidence: None,
            note: None,
            facts: Vec::new(),
            page_changed: None,
            duration_ms: 1,
        },
        frame: None,
    })
}

async fn emit(args: &RunArgs, event: Event) {
    if let Some(on_event) = &args.on_event {
        on_event(event).await;
    }
}

fn length(requests: &Requests) -> usize {
    requests.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).len()
}

/// Waits to be stopped, then closes its "browser" (a short pause and a file written) before it answers.
async fn until_stopped(args: &RunArgs, closed: Option<String>) -> Outcome {
    args.cancel.tripped().await;
    // A real browser takes a moment to close, and the server has to wait for it.
    tokio::time::sleep(Duration::from_millis(50)).await;
    if let Some(path) = closed {
        let _ = std::fs::write(path, "closed");
    }
    Err(RunError::Cancelled)
}

async fn run(name: &str, args: RunArgs, requests: Requests) -> Outcome {
    match name {
        // tests/serve_scripts.py
        "tests.serve_scripts:echo" => {
            emit(&args, browser(None, None)).await;
            Ok(RunResult { answer: Some(format!("echo: {}", args.task)), ..result(Status::Complete) })
        }
        "tests.serve_scripts:browse_until_cancelled" => {
            emit(&args, browser(None, None)).await;
            let path = args.task.clone();
            until_stopped(&args, Some(path)).await
        }
        "tests.serve_scripts:type_secret" => {
            let start = args.start.clone().unwrap_or_default();
            let value = match &args.secrets {
                Some(secrets) => {
                    let name = secrets.available().first().map(|secret| secret.name.clone()).unwrap_or_default();
                    args.cancel.guard(resolve_secret(secrets.as_ref(), &name, &origin_of(&start))).await?
                }
                None => None,
            };
            emit(&args, browser(None, None)).await;
            let answer = match value {
                None => "nothing to type".to_string(),
                Some(value) => format!("typed {} characters", value.chars().count()),
            };
            Ok(RunResult { answer: Some(answer), ..result(Status::Complete) })
        }
        // packages/sdk/test/servers/scripted_run.py
        "scripted_run:echo" => {
            let params = requests.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).last().cloned();
            let attachments: Vec<Value> =
                args.attachments.iter().map(|attachment| json!(attachment.content.0)).collect();
            let data = json!({"params": params, "attachments": attachments});
            Ok(RunResult { answer: Some(args.task.clone()), data, ..result(Status::Complete) })
        }
        "scripted_run:found" => {
            let found = args.inputs.as_ref().and_then(|inputs| inputs.get("found")).cloned().unwrap_or_default();
            let found: Value =
                serde_json::from_str(&found).map_err(|error| RunError::Failed(format!("JSONDecodeError: {error}")))?;
            let model = args
                .output_schema
                .as_ref()
                .ok_or_else(|| RunError::Failed("AttributeError: no output schema".into()))?;
            let data = model.validate(&found).map_err(|error| RunError::Failed(format!("ValidationError: {error}")))?;
            Ok(RunResult { data, ..result(Status::Complete) })
        }
        "scripted_run:request_count" => Ok(RunResult { data: json!(length(&requests)), ..result(Status::Complete) }),
        "scripted_run:three_events" => {
            emit(&args, browser(Some("https://live.example/1"), Some("browser-1"))).await;
            emit(&args, step(0)).await;
            emit(&args, step(1)).await;
            Ok(result(Status::Complete))
        }
        "scripted_run:needs_login" => {
            Ok(RunResult { error: Some("the site asked for a password".into()), ..result(Status::NeedsLogin) })
        }
        "scripted_run:never_ends" => {
            emit(&args, browser(None, None)).await;
            until_stopped(&args, None).await
        }
        "scripted_run:exits" => {
            emit(&args, browser(None, None)).await;
            std::process::exit(7)
        }
        "scripted_run:killed" => {
            emit(&args, browser(None, None)).await;
            let _ = std::process::Command::new("kill").args(["-9", &std::process::id().to_string()]).status();
            until_stopped(&args, None).await
        }
        "scripted_run:until_cancelled" => {
            let closed = args.inputs.as_ref().and_then(|inputs| inputs.get("closed")).cloned();
            emit(&args, browser(None, None)).await;
            match closed {
                None => Ok(RunResult { data: json!(length(&requests)), ..result(Status::Complete) }),
                Some(path) => until_stopped(&args, Some(path)).await,
            }
        }
        "scripted_run:types_secrets" => {
            let inputs = args.inputs.clone().unwrap_or_default();
            let name = inputs.get("name").cloned().unwrap_or_default();
            let origin = inputs.get("origin").cloned().unwrap_or_default();
            let secrets = args.secrets.clone().ok_or_else(|| RunError::Failed("AttributeError: no secrets".into()))?;
            let first = args.cancel.guard(secrets.resolve(&name, &origin)).await?;
            let second = args.cancel.guard(secrets.resolve(&name, &origin)).await?;
            if args.cancel.is_tripped() {
                return Err(RunError::Cancelled);
            }
            Ok(RunResult { data: json!([first, second]), ..result(Status::Complete) })
        }
        "scripted_run:ends_on_an_order" => {
            let passed = match &args.until {
                None => true,
                Some(until) => args.cancel.guard(until("https://shop.example/orders/17".into())).await?,
            };
            if args.cancel.is_tripped() {
                return Err(RunError::Cancelled);
            }
            Ok(result(if passed { Status::Complete } else { Status::Unverified }))
        }
        "scripted_run:two_frames" => {
            let Some(on_frame) = &args.on_frame else {
                return Ok(RunResult { data: json!(0), ..result(Status::Complete) });
            };
            on_frame((0..=255).collect()).await;
            on_frame(b"\xff\xd8 second".to_vec()).await;
            emit(&args, step(0)).await;
            Ok(RunResult { data: json!(2), ..result(Status::Complete) })
        }
        // tests/test_mcp_server.py's `Recorder`: the events a run sends, then a cited answer with a download.
        "tests.test_mcp_server:Recorder" => {
            emit(&args, browser(Some("https://live.example.com/session"), None)).await;
            let answered = mcp_result();
            for index in 0..answered.steps.len() {
                emit(&args, mcp_step(index as i64)).await;
            }
            Ok(answered)
        }
        _ => Err(RunError::Failed(format!("NameError: {name}"))),
    }
}

const START: &str = "https://shop.example.com/cart";

fn mcp_step(index: i64) -> Event {
    let Event::Step(mut event) = step(index) else { unreachable!("step() makes a step event") };
    event.step.url = START.into();
    event.step.target = Some("Add to cart".into());
    event.step.duration_ms = 5;
    Event::Step(event)
}

fn parsed<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).expect("a model the tests write")
}

fn mcp_result() -> RunResult {
    let steps = (0..2).map(|index| match mcp_step(index) {
        Event::Step(event) => event.step,
        Event::Browser(_) => unreachable!("mcp_step() makes a step event"),
    });
    RunResult {
        answer: Some("The backpack costs $29.99.".into()),
        data: json!({"price": 29.99}),
        evidence: vec![parsed(json!({
            "source_id": "s1", "url": START, "frame_id": null, "captured_at": "2026-09-19T00:00:00Z",
            "capture_sha256": "0".repeat(64), "start": 0, "end": 6, "quote": "$29.99",
        }))],
        steps: steps.collect(),
        cost: parsed(json!({"lines": [
            {"component": "jev", "basis": "metered", "dollars": 0.004},
            {"component": "browser", "basis": "unknown"},
        ]})),
        artifacts: vec![parsed(json!({
            "kind": "download", "name": "receipt.pdf", "mime_type": "application/pdf", "size_bytes": 10,
            "sha256": "1".repeat(64), "uri": "file:///downloads/receipt.pdf",
        }))],
        final_url: Some(START.into()),
        ..result(Status::Complete)
    }
}

const NAMES: [&str; 16] = [
    "tests.test_mcp_server:Recorder",
    "tests.serve_scripts:echo",
    "tests.serve_scripts:browse_until_cancelled",
    "tests.serve_scripts:type_secret",
    "scripted_run:echo",
    "scripted_run:found",
    "scripted_run:request_count",
    "scripted_run:three_events",
    "scripted_run:needs_login",
    "scripted_run:never_ends",
    "scripted_run:exits",
    "scripted_run:killed",
    "scripted_run:until_cancelled",
    "scripted_run:types_secrets",
    "scripted_run:ends_on_an_order",
    "scripted_run:two_frames",
];

/// The run `name` stands for, as `--run-task` names it. `__main__:x` is how the SDK's scripted server names its runs.
pub fn named(name: &str, requests: Requests) -> Option<Arc<dyn Runner>> {
    let name = match name.strip_prefix("__main__:") {
        Some(run) => format!("scripted_run:{run}"),
        None => name.to_string(),
    };
    let name: &'static str = NAMES.iter().find(|known| **known == name)?;
    Some(Arc::new(move |args: RunArgs| -> BoxFuture<'static, Outcome> { Box::pin(run(name, args, requests.clone())) }))
}

/// A transport that notes the params of every `run` line it hands on, as the SDK's scripted server does.
pub struct Noting {
    pub inner: Arc<dyn Transport>,
    pub requests: Requests,
}

impl Transport for Noting {
    fn receive(&self) -> BoxFuture<'_, Option<Vec<u8>>> {
        Box::pin(async move {
            let line = self.inner.receive().await?;
            if let Ok(message) = serde_json::from_slice::<Value>(&line)
                && message.get("method").and_then(Value::as_str) == Some("run")
            {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                self.requests.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(params);
            }
            Some(line)
        })
    }

    fn send(&self, line: &[u8]) {
        self.inner.send(line);
    }
}

/// How FASTBROWSE_TEST_SERVER alters the real server, as each of the SDK's `servers/*.py` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestServer {
    /// `environment.py`: first writes the environment it was started with to FASTBROWSE_TEST_ENVIRONMENT_FILE.
    Environment,
    /// `protocol_2.py`: answers `initialize` with a protocol version the SDK does not speak.
    Protocol2,
    /// `stderr_line.py`: first writes one known line to stderr.
    StderrLine,
    /// `outlives_shutdown.py`: answers `shutdown`, then never exits, and shrugs off a polite signal.
    OutlivesShutdown,
    /// `scripted_run.py`: the run FASTBROWSE_TEST_RUN names stands where `run_task` does.
    ScriptedRun(String),
}

pub fn test_server() -> Option<TestServer> {
    match std::env::var("FASTBROWSE_TEST_SERVER").ok()?.as_str() {
        "environment" => Some(TestServer::Environment),
        "protocol_2" => Some(TestServer::Protocol2),
        "stderr_line" => Some(TestServer::StderrLine),
        "outlives_shutdown" => Some(TestServer::OutlivesShutdown),
        "scripted_run" => Some(TestServer::ScriptedRun(std::env::var("FASTBROWSE_TEST_RUN").unwrap_or_default())),
        _ => None,
    }
}

/// The runner, protocol version and transport `serve` runs with, as `--run-task` and FASTBROWSE_TEST_SERVER alter
/// them.
pub fn setup(run_task: Option<&str>, transport: Arc<dyn Transport>) -> (Arc<dyn Runner>, i64, Arc<dyn Transport>) {
    let mut run_task = run_task.map(str::to_string);
    let mut protocol = fastbrowse_core::protocol::PROTOCOL_VERSION;
    match test_server() {
        Some(TestServer::Environment) => {
            let environment: serde_json::Map<String, Value> =
                crate::options::environ().into_iter().map(|(key, value)| (key, value.into())).collect();
            if let Ok(path) = std::env::var("FASTBROWSE_TEST_ENVIRONMENT_FILE") {
                let _ = std::fs::write(path, fastbrowse_core::py::json::dumps(&Value::Object(environment)));
            }
        }
        Some(TestServer::Protocol2) => protocol = 2,
        Some(TestServer::StderrLine) => eprintln!("a line the server wrote to stderr"),
        Some(TestServer::ScriptedRun(run)) => run_task = Some(format!("scripted_run:{run}")),
        Some(TestServer::OutlivesShutdown) | None => {}
    }
    let requests: Requests = Arc::default();
    match run_task.and_then(|name| named(&name, requests.clone())) {
        Some(runner) => (runner, protocol, Arc::new(Noting { inner: transport, requests })),
        None => (crate::runner::default_runner(), protocol, transport),
    }
}

/// After `serve` returns: `outlives_shutdown.py` never exits, and shrugs off a polite signal.
pub async fn linger() {
    if test_server() != Some(TestServer::OutlivesShutdown) {
        return;
    }
    #[cfg(unix)]
    if let Ok(mut terminate) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        loop {
            terminate.recv().await;
        }
    }
    // Windows sends no polite signal to shrug off, so there it only never exits.
    #[cfg(not(unix))]
    std::future::pending::<()>().await;
}
