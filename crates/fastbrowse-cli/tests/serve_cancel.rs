//! Cancelling a run over the serve protocol, as a client sees it.
//!
//! `run_task` is replaced by a stand-in that holds a browser open until it is stopped. What is asserted is the order of
//! the messages the server wrote and whether that browser was closed by then.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{CHROME, Call, Recorder, binary, browser_event, result, run_params, runner, serving, settings};
use fastbrowse_cli::runner::{RunError, Runner};
use fastbrowse_core::models::{RunResult, Status};
use serde_json::{Value, json};
use tokio::sync::{Notify, watch};

const CANCELLED: i64 = -32002;

/// A run that says its browser is open and then works until it is stopped or told to finish.
#[derive(Clone)]
struct Browsing {
    started: Arc<Mutex<Vec<String>>>,
    closed: Arc<Mutex<Vec<String>>>,
    finish: watch::Sender<bool>,
    /// Set once the browser starts closing.
    closing: Arc<Notify>,
    /// A browser that takes a while to close, and closes only when the test lets it.
    let_close: watch::Sender<bool>,
}

impl Browsing {
    fn new(slow_close: bool) -> Browsing {
        Browsing {
            started: Arc::default(),
            closed: Arc::default(),
            finish: watch::channel(false).0,
            closing: Arc::default(),
            let_close: watch::channel(!slow_close).0,
        }
    }

    fn runner(&self) -> Arc<dyn Runner> {
        let browsing = self.clone();
        runner(move |args: Call| {
            let browsing = browsing.clone();
            async move {
                browsing.started.lock().unwrap().push(args.task.task.clone());
                let mut finish = browsing.finish.subscribe();
                let outcome = args
                    .host
                    .cancel
                    .guard(async {
                        (args.host.on_event.as_ref().unwrap())(browser_event(None, None)).await;
                        let _ = finish.wait_for(|done| *done).await;
                    })
                    .await;
                browsing.closing.notify_one();
                let _ = browsing.let_close.subscribe().wait_for(|go| *go).await;
                browsing.closed.lock().unwrap().push(args.task.task.clone());
                outcome?;
                Ok(RunResult { answer: Some(args.task.task.clone()), ..result(Status::Complete) })
            }
        })
    }

    fn closed(&self) -> Vec<String> {
        self.closed.lock().unwrap().clone()
    }

    fn started(&self) -> Vec<String> {
        self.started.lock().unwrap().clone()
    }
}

/// Start a run and wait for its first event, which is the proof that its browser is open.
async fn under_way(client: &mut common::FakeClient, run_id: &str, fields: Value) -> i64 {
    let id = client.call("run", Some(run_params(run_id, fields)));
    assert_eq!(client.receive().await["params"]["run_id"], run_id);
    id
}

async fn closing(browsing: &Browsing) {
    tokio::time::timeout(Duration::from_secs(5), browsing.closing.notified()).await.expect("the browser began closing");
}

const KETTLE: &str = "Add the kettle to the cart";

#[tokio::test]
async fn test_cancelling_the_active_run_ends_it_with_the_cancelled_error() {
    let browsing = Browsing::new(false);
    let mut client = serving(browsing.runner(), settings());
    let run = under_way(&mut client, "run-1", json!({})).await;
    let cancel = client.request("run/cancel", Some(json!({"run_id": "run-1"}))).await;
    let reply = client.reply(run).await;
    assert_eq!(cancel, json!({"jsonrpc": "2.0", "id": cancel["id"], "result": null}));
    assert_eq!(reply["error"]["code"], CANCELLED);
    assert!(reply["error"]["message"].as_str().unwrap().contains("run-1"));
    assert!(reply.get("result").is_none());
    assert_eq!(browsing.closed(), [KETTLE]);
}

#[tokio::test]
async fn test_the_browser_is_closed_before_the_cancelled_reply_is_written() {
    let browsing = Browsing::new(true);
    let mut client = serving(browsing.runner(), settings());
    let run = under_way(&mut client, "run-1", json!({})).await;
    client.result("run/cancel", Some(json!({"run_id": "run-1"}))).await;
    closing(&browsing).await;
    // Still closing, and a handshake goes through, so the server is not merely slow to write.
    client.result("initialize", Some(json!({"protocol_version": 1}))).await;
    assert!(client.inbox.is_empty() && client.silent().await);
    browsing.let_close.send_replace(true);
    let reply = client.reply(run).await;
    assert_eq!(reply["error"]["code"], CANCELLED);
    assert_eq!(browsing.closed(), [KETTLE]);
}

#[tokio::test]
async fn test_a_cancel_sent_straight_after_the_run_still_cancels_it() {
    let browsing = Browsing::new(false);
    let mut client = serving(browsing.runner(), settings());
    // Both lines are waiting before the server reads either, as when an AbortSignal fires at once.
    let run = client.call("run", Some(run_params("run-1", json!({}))));
    let cancel = client.call("run/cancel", Some(json!({"run_id": "run-1"})));
    let reply = client.reply(run).await;
    assert_eq!(reply["error"]["code"], CANCELLED);
    assert_eq!(browsing.started(), browsing.closed());
    assert!(client.inbox.contains(&json!({"jsonrpc": "2.0", "id": cancel, "result": null})));
}

#[tokio::test]
async fn test_cancelling_an_unknown_run_is_answered_and_leaves_the_active_run_alone() {
    let browsing = Browsing::new(false);
    let mut client = serving(browsing.runner(), settings());
    let run = under_way(&mut client, "run-1", json!({})).await;
    let cancel = client.request("run/cancel", Some(json!({"run_id": "run-0"}))).await;
    assert!(cancel["result"].is_null() && cancel.get("error").is_none());
    assert!(browsing.closed().is_empty());
    browsing.finish.send_replace(true);
    assert_eq!(client.reply(run).await["result"]["status"], "complete");
}

#[tokio::test]
async fn test_cancelling_a_run_that_has_finished_is_answered_and_leaves_the_next_run_alone() {
    let browsing = Browsing::new(false);
    browsing.finish.send_replace(true);
    let mut client = serving(browsing.runner(), settings());
    client.result("run", Some(run_params("run-1", json!({})))).await;
    browsing.finish.send_replace(false);
    let run = under_way(&mut client, "run-2", json!({"task": "Then check out"})).await;
    let cancel = client.request("run/cancel", Some(json!({"run_id": "run-1"}))).await;
    assert!(cancel["result"].is_null() && cancel.get("error").is_none());
    browsing.finish.send_replace(true);
    assert_eq!(client.reply(run).await["result"]["answer"], "Then check out");
}

#[tokio::test]
async fn test_cancelling_twice_does_not_interrupt_the_browser_closing() {
    let browsing = Browsing::new(true);
    let mut client = serving(browsing.runner(), settings());
    let run = under_way(&mut client, "run-1", json!({})).await;
    client.result("run/cancel", Some(json!({"run_id": "run-1"}))).await;
    closing(&browsing).await;
    client.result("run/cancel", Some(json!({"run_id": "run-1"}))).await;
    assert!(browsing.closed().is_empty());
    browsing.let_close.send_replace(true);
    assert_eq!(client.reply(run).await["error"]["code"], CANCELLED);
    assert_eq!(browsing.closed(), [KETTLE]);
}

#[tokio::test]
async fn test_a_run_after_a_cancelled_one_succeeds() {
    let browsing = Browsing::new(false);
    let mut client = serving(browsing.runner(), settings());
    let cancelled = under_way(&mut client, "run-1", json!({})).await;
    client.result("run/cancel", Some(json!({"run_id": "run-1"}))).await;
    assert_eq!(client.reply(cancelled).await["error"]["code"], CANCELLED);
    browsing.finish.send_replace(true);
    let result = client.result("run", Some(run_params("run-2", json!({"task": "Then check out"})))).await;
    assert_eq!(result["status"], "complete");
    assert_eq!(browsing.started(), [KETTLE, "Then check out"]);
}

#[tokio::test]
async fn test_a_cancel_without_a_run_id_is_refused_and_cancels_nothing() {
    let hold = Arc::new(Notify::new());
    let mut client = serving(Recorder { hold: Some(hold.clone()), ..Recorder::default() }.runner(), settings());
    let run = client.call("run", Some(run_params("run-1", json!({}))));
    let refused = client.request("run/cancel", Some(json!({}))).await;
    assert_eq!(refused["error"]["code"], -32602);
    assert!(refused["error"]["message"].as_str().unwrap().contains("run_id"));
    hold.notify_one();
    assert_eq!(client.reply(run).await["result"]["status"], "complete");
}

#[tokio::test]
async fn test_shutdown_during_a_run_closes_its_browser_answers_it_cancelled_and_exits_zero() {
    let browsing = Browsing::new(true);
    let mut client = serving(browsing.runner(), settings());
    let run = under_way(&mut client, "run-1", json!({})).await;
    let shutdown = client.request("shutdown", None).await;
    closing(&browsing).await;
    assert!(client.silent().await);
    browsing.let_close.send_replace(true);
    let reply = client.reply(run).await;
    assert_eq!(client.exit_code().await, 0);
    assert!(shutdown["result"].is_null());
    assert_eq!(reply["error"]["code"], CANCELLED);
    assert_eq!(browsing.closed(), [KETTLE]);
}

#[tokio::test]
async fn test_end_of_input_during_a_run_does_what_shutdown_does() {
    let browsing = Browsing::new(true);
    let mut client = serving(browsing.runner(), settings());
    let run = under_way(&mut client, "run-1", json!({})).await;
    client.close_input();
    closing(&browsing).await;
    assert!(client.silent().await);
    browsing.let_close.send_replace(true);
    let reply = client.reply(run).await;
    assert_eq!(client.exit_code().await, 0);
    assert_eq!(reply["error"]["code"], CANCELLED);
    assert_eq!(browsing.closed(), [KETTLE]);
}

#[tokio::test]
async fn test_shutdown_straight_after_the_run_still_cancels_it() {
    let browsing = Browsing::new(false);
    let mut client = serving(browsing.runner(), settings());
    let run = client.call("run", Some(run_params("run-1", json!({}))));
    client.call("shutdown", None);
    let reply = client.reply(run).await;
    assert_eq!(client.exit_code().await, 0);
    assert_eq!(reply["error"]["code"], CANCELLED);
    assert_eq!(browsing.started(), browsing.closed());
}

/// The command on a run that never finishes, and that writes `marker` once its browser has closed.
fn served(
    marker: &std::path::Path,
) -> (std::process::Child, std::process::ChildStdin, BufReader<std::process::ChildStdout>) {
    let mut process = Command::new(binary())
        .args(["serve", "--stdio", "--run-task", "tests.serve_scripts:browse_until_cancelled"])
        .env("OPENROUTER_API_KEY", "unused")
        .env("AI_GATEWAY_API_KEY", "unused")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = process.stdin.take().unwrap();
    let params = json!({"run_id": "run-1", "task": marker, "local": true, "chrome": {"binary": CHROME}});
    let mut line = json!({"jsonrpc": "2.0", "id": 1, "method": "run", "params": params}).to_string();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();
    let mut stdout = BufReader::new(process.stdout.take().unwrap());
    let mut event = String::new();
    stdout.read_line(&mut event).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&event).unwrap()["method"], "run/event");
    (process, stdin, stdout)
}

fn marker(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("fastbrowse-cancel-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

#[test]
fn test_the_command_exits_zero_with_its_browser_closed_when_its_input_ends_during_a_run() {
    let marker = marker("input");
    let (mut process, stdin, mut stdout) = served(&marker);
    assert!(!marker.exists());
    drop(stdin);
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    assert!(process.wait().unwrap().success());
    let replies: Vec<Value> = rest.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(replies.len(), 1);
    assert_eq!((replies[0]["id"].clone(), replies[0]["error"]["code"].clone()), (json!(1), json!(CANCELLED)));
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "closed");
}

#[test]
fn test_the_command_exits_zero_with_its_browser_closed_on_shutdown_during_a_run() {
    let marker = marker("shutdown");
    let (mut process, mut stdin, mut stdout) = served(&marker);
    stdin.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"shutdown\"}\n").unwrap();
    stdin.flush().unwrap();
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    // The input is still open, as it is when the SDK's `close()` waits for the exit.
    assert!(process.wait().unwrap().success());
    let replies: Vec<Value> = rest.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let by_id = |id: i64| replies.iter().find(|reply| reply["id"] == id).unwrap().clone();
    assert!(by_id(2)["result"].is_null());
    assert_eq!(by_id(1)["error"]["code"], CANCELLED);
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "closed");
    drop(stdin);
}

#[tokio::test]
async fn test_cancelling_a_run_that_is_waiting_on_a_secret_ends_it_and_the_late_value_goes_nowhere() {
    let resolved: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let seen = resolved.clone();
    let typist = runner(move |args: Call| {
        let resolved = seen.clone();
        async move {
            (args.host.on_event.as_ref().unwrap())(browser_event(None, None)).await;
            let secrets = args.host.secrets.clone().unwrap();
            let value = args
                .host
                .cancel
                .guard(fastbrowse_core::safety::resolve_secret(&*secrets, "SHOP_PASSWORD", "https://shop.example.com"))
                .await?;
            resolved.lock().unwrap().push(value);
            Err::<RunResult, _>(RunError::Cancelled)
        }
    });
    let mut client = serving(typist, settings());
    let secrets = json!([{"name": "SHOP_PASSWORD", "origins": ["https://shop.example.com"]}]);
    let run = under_way(&mut client, "run-1", json!({"secrets": secrets})).await;
    let question = client.receive().await;
    assert_eq!(question["method"], "secrets/resolve");
    client.result("run/cancel", Some(json!({"run_id": "run-1"}))).await;
    let reply = client.reply(run).await;
    client.send(json!({"jsonrpc": "2.0", "id": question["id"], "result": "hunter2-correct-horse"}));
    let handshake = client.result("initialize", Some(json!({"protocol_version": 1}))).await;
    assert_eq!(reply["error"]["code"], CANCELLED);
    assert!(resolved.lock().unwrap().is_empty());
    assert_eq!(handshake["protocol_version"], 1);
    assert!(client.inbox.is_empty());
}
