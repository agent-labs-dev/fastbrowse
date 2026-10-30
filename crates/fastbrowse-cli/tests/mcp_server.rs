//! tests/test_mcp_server.py: the MCP server's gates and the shape of what it returns, driven through MCP messages
//! with `run_task` replaced by a recorder, so no browser and no model keys.

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{CHROME, Call, START, browser_event, result, runner, step, step_event};
use fastbrowse_cli::mcp::{
    self, DeclaredSecret, McpServer, Outbox, ServerConfig, Session, bounded, configure, is_loopback, parse,
};
use fastbrowse_cli::runner::{RunError, Runner};
use fastbrowse_clients::environment::{DecisionSource, Secret, Settings};
use fastbrowse_core::models::{BudgetResource, BudgetStop, CostBreakdown, Limits, RunResult, Status};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};

fn parsed<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}

fn answered(status: Status) -> RunResult {
    RunResult {
        answer: Some("The backpack costs $29.99.".into()),
        data: json!({"price": 29.99}),
        evidence: vec![parsed(json!({
            "source_id": "s1", "url": START, "frame_id": null, "captured_at": "2026-09-19T00:00:00Z",
            "capture_sha256": "0".repeat(64), "start": 0, "end": 6, "quote": "$29.99",
        }))],
        steps: vec![step(0), step(1)],
        cost: serde_json::from_value::<CostBreakdown>(json!({"lines": [
            {"component": "jev", "basis": "metered", "dollars": 0.004},
            {"component": "browser", "basis": "unknown"},
        ]}))
        .unwrap(),
        artifacts: vec![parsed(json!({
            "kind": "download", "name": "receipt.pdf", "mime_type": "application/pdf", "size_bytes": 10,
            "sha256": "1".repeat(64), "uri": "file:///downloads/receipt.pdf",
        }))],
        final_url: Some(START.into()),
        ..result(status)
    }
}

/// Stands in for `run_task`: keeps what it was called with and plays the events a run would send.
#[derive(Clone)]
struct Recorder {
    result: RunResult,
    calls: Arc<Mutex<Vec<Call>>>,
}

impl Recorder {
    fn new(result: RunResult) -> Recorder {
        Recorder { result, calls: Arc::default() }
    }

    fn runner(&self) -> Arc<dyn Runner> {
        let recorder = self.clone();
        runner(move |args: Call| {
            let recorder = recorder.clone();
            async move {
                recorder.calls.lock().unwrap().push(args.clone());
                let on_event = args.host.on_event.clone().unwrap();
                on_event(browser_event(Some("https://live.example.com/session"), None)).await;
                for index in 0..recorder.result.steps.len() {
                    on_event(step_event(index as i64, None)).await;
                }
                Ok(recorder.result.clone())
            }
        })
    }

    fn last(&self) -> Call {
        self.calls.lock().unwrap().last().cloned().expect("a run was started")
    }

    fn called(&self) -> bool {
        !self.calls.lock().unwrap().is_empty()
    }
}

/// A connected, initialized session and what the server sent it.
struct Client {
    server: Arc<McpServer>,
    session: Arc<Session>,
    outbox: Outbox,
    inbox: mpsc::UnboundedReceiver<Value>,
    ids: i64,
}

impl Client {
    fn new(config: ServerConfig, runner: Arc<dyn Runner>) -> Client {
        let (sender, inbox) = mpsc::unbounded_channel();
        let outbox: Outbox = Arc::new(move |message| {
            let _ = sender.send(message);
        });
        let mut client =
            Client { server: McpServer::new(config, runner), session: Arc::default(), outbox, inbox, ids: 0 };
        client.send(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}));
        client.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        client
    }

    fn send(&mut self, message: Value) {
        self.server.receive(&self.session, message.to_string().as_bytes(), &self.outbox);
    }

    async fn reply(&mut self, id: i64, notices: &mut Vec<String>) -> Value {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(5), self.inbox.recv()).await.unwrap().unwrap();
            if message.get("id") == Some(&json!(id)) && message.get("method").is_none() {
                return message;
            }
            match message["method"].as_str() {
                Some("notifications/progress") => {
                    let params = &message["params"];
                    let total =
                        params.get("total").and_then(Value::as_f64).map_or("unlimited".into(), |t| format!("{t}"));
                    notices.push(format!("{}/{total} {}", params["progress"], params["message"].as_str().unwrap()));
                }
                Some("notifications/message") => notices.push(message["params"]["data"].as_str().unwrap().into()),
                _ => {}
            }
        }
    }

    fn start_call(&mut self, arguments: Value) -> i64 {
        self.ids += 1;
        let id = self.ids;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": "browse", "arguments": arguments, "_meta": {"progressToken": "p"}}}));
        id
    }

    async fn call(&mut self, arguments: Value) -> (Value, Vec<String>) {
        let id = self.start_call(arguments);
        let mut notices = Vec::new();
        let reply = self.reply(id, &mut notices).await;
        (reply["result"].clone(), notices)
    }
}

async fn call(config: ServerConfig, recorder: &Recorder, arguments: Value) -> (Value, Vec<String>) {
    Client::new(config, recorder.runner()).call(arguments).await
}

fn text(result: &Value) -> String {
    result["content"][0]["text"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn test_a_run_returns_structured_output_with_citations_and_reports_each_step() {
    let recorder = Recorder::new(answered(Status::Complete));
    let config = ServerConfig { downloads: Some("downloads".into()), ..ServerConfig::default() };
    let (result, notices) =
        call(config, &recorder, json!({"task": "What does the backpack cost?", "start": START})).await;
    assert_eq!(result["isError"], false);
    let structured = &result["structuredContent"];
    assert_eq!(structured["status"], "complete");
    assert_eq!(structured["citations"], json!([{"quote": "$29.99", "url": START}]));
    assert_eq!(structured["steps"], 2);
    assert_eq!(structured["dollars"], 0.004);
    assert_eq!(structured["cost_complete"], false);
    assert_eq!(structured["live_url"], "https://live.example.com/session");
    assert_eq!(structured["downloads"][0]["name"], "receipt.pdf");
    assert!(structured["next_step"].is_null());
    assert_eq!(result["content"][0]["type"], "text");
    assert!(notices.join(" ").contains("Add to cart"));
    assert!(notices.contains(&"1.0/unlimited click Add to cart -> executed".to_string()), "{notices:?}");
    assert!(notices.iter().any(|notice| notice.contains("watch live")));
}

#[tokio::test]
async fn test_downloads_are_not_listed_when_the_server_keeps_none() {
    let (result, _) =
        call(ServerConfig::default(), &Recorder::new(answered(Status::Complete)), json!({"task": "t", "start": START}))
            .await;
    assert_eq!(result["structuredContent"]["downloads"], json!([]));
}

#[tokio::test]
async fn test_a_captured_screenshot_reaches_the_caller_as_a_file() {
    // `final_frame` is left out of JSON, so without this the answer said "captured" and the image was lost.
    let directory = std::env::temp_dir().join(format!("fastbrowse-mcp-shot-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let recorder = Recorder::new(RunResult { final_frame: Some(b"final png".to_vec()), ..answered(Status::Complete) });
    let config = ServerConfig { downloads: Some(directory.clone()), ..ServerConfig::default() };
    let (result, _) = call(config, &recorder, json!({"task": "t", "start": START})).await;
    let screenshot = &result["structuredContent"]["screenshot"];
    assert_eq!(screenshot["mime_type"], "image/png");
    let path = PathBuf::from(screenshot["uri"].as_str().unwrap().strip_prefix("file://").unwrap());
    assert!(path.starts_with(std::fs::canonicalize(&directory).unwrap()));
    assert_eq!(std::fs::read(&path).unwrap(), b"final png");
    let _ = std::fs::remove_dir_all(&directory);
}

#[tokio::test]
async fn test_authorize_is_refused_unless_the_operator_allowed_it() {
    let recorder = Recorder::new(answered(Status::Complete));
    let (result, _) =
        call(ServerConfig::default(), &recorder, json!({"task": "Buy it", "start": START, "authorize": true})).await;
    assert_eq!(result["isError"], true);
    assert!(text(&result).contains("--allow-authorize"));
    assert!(!recorder.called());
    let config = ServerConfig { allow_authorize: true, ..ServerConfig::default() };
    call(config, &recorder, json!({"task": "Buy it", "start": START, "authorize": true})).await;
    assert!(recorder.last().task.authorization.unwrap().irreversible_actions);
}

#[tokio::test]
async fn test_needs_confirmation_says_what_the_caller_can_do() {
    let recorder = Recorder::new(answered(Status::NeedsConfirmation));
    let (result, _) = call(ServerConfig::default(), &recorder, json!({"task": "Buy it", "start": START})).await;
    assert!(result["structuredContent"]["next_step"].as_str().unwrap().contains("--allow-authorize"));
}

#[tokio::test]
async fn test_a_call_cannot_raise_the_servers_limits() {
    let recorder = Recorder::new(answered(Status::Complete));
    let ceilings =
        Limits { max_steps: Some(20), max_dollars: Some(0.5), max_seconds: Some(120.0), ..Limits::default() };
    let config = ServerConfig { ceilings, ..ServerConfig::default() };
    let arguments = json!({"task": "t", "start": START, "max_steps": 500, "max_dollars": 9.0, "max_seconds": 10});
    call(config, &recorder, arguments).await;
    let limits = recorder.last().task.limits.unwrap();
    assert_eq!((limits.max_steps, limits.max_dollars, limits.max_seconds), (Some(20), Some(0.5), Some(10.0)));
}

#[test]
fn test_limits_left_unset_fall_back_to_the_ceilings() {
    let ceilings = Limits { max_steps: Some(20), max_dollars: None, max_seconds: Some(120.0), ..Limits::default() };
    let limits = bounded(&ceilings, None, Some(0.2), None);
    assert_eq!((limits.max_steps, limits.max_dollars, limits.max_seconds), (Some(20), Some(0.2), Some(120.0)));
}

#[tokio::test]
async fn test_secrets_reach_a_run_only_on_their_own_origin() {
    let secret =
        DeclaredSecret { name: "password".into(), value: "hunter2".into(), origin: "https://shop.example.com".into() };
    let config = ServerConfig { secrets: vec![secret], ..ServerConfig::default() };
    let recorder = Recorder::new(answered(Status::Complete));
    call(config.clone(), &recorder, json!({"task": "Sign in", "start": START})).await;
    let resolver = recorder.last().host.secrets.unwrap();
    assert_eq!(resolver.available().iter().map(|r| r.name.clone()).collect::<Vec<_>>(), ["password"]);
    assert_eq!(resolver.resolve("password", "https://shop.example.com").await.as_deref(), Some("hunter2"));
    assert_eq!(resolver.resolve("password", "https://evil.example.com").await, None);
    call(config, &recorder, json!({"task": "Sign in", "start": "https://other.example.com/"})).await;
    assert!(recorder.last().host.secrets.is_none());
}

#[tokio::test]
async fn test_secret_values_never_appear_in_the_tool_listing() {
    let secret =
        DeclaredSecret { name: "password".into(), value: "hunter2".into(), origin: "https://shop.example.com".into() };
    let config = ServerConfig { secrets: vec![secret], ..ServerConfig::default() };
    let mut client = Client::new(config, Recorder::new(answered(Status::Complete)).runner());
    client.send(json!({"jsonrpc": "2.0", "id": 9, "method": "tools/list"}));
    let listing = client.reply(9, &mut Vec::new()).await.to_string();
    assert!(listing.contains("password on https://shop.example.com"));
    assert!(!listing.contains("hunter2"));
}

#[tokio::test]
async fn test_a_bitwarden_item_must_be_one_the_server_offers() {
    let recorder = Recorder::new(answered(Status::Complete));
    let config = ServerConfig { bitwarden: vec!["Shop".into()], ..ServerConfig::default() };
    let (result, _) = call(config, &recorder, json!({"task": "t", "start": START, "bitwarden": "Bank"})).await;
    assert_eq!(result["isError"], true);
    assert!(text(&result).contains("not an item this server offers"));
    assert!(!recorder.called());
}

#[tokio::test]
async fn test_start_must_be_a_web_url() {
    for start in ["file:///etc/passwd", "javascript:alert(1)", "shop.example.com", "https://"] {
        let recorder = Recorder::new(answered(Status::Complete));
        let (result, _) = call(ServerConfig::default(), &recorder, json!({"task": "t", "start": start})).await;
        assert_eq!(result["isError"], true, "{start}");
        assert!(!recorder.called());
    }
}

#[tokio::test]
async fn test_fields_become_the_runs_output_schema() {
    let recorder = Recorder::new(answered(Status::Complete));
    let fields = json!({"price": {"type": "number", "description": "In USD"}, "listed": {"type": "date"}});
    call(ServerConfig::default(), &recorder, json!({"task": "t", "start": START, "fields": fields})).await;
    let schema = recorder.last().task.output_schema.unwrap();
    assert_eq!(
        Value::Object(schema),
        json!({
            "type": "object",
            "properties": {"price": {"type": "number", "description": "In USD"}, "listed": {"type": "string"}},
            "required": ["price", "listed"],
        })
    );
}

#[tokio::test]
async fn test_a_configuration_error_in_a_run_is_a_tool_error() {
    let missing_key = runner(|_| async { Err(RunError::Configuration("set OPENROUTER_API_KEY for the LLM".into())) });
    let (result, _) =
        Client::new(ServerConfig::default(), missing_key).call(json!({"task": "t", "start": START})).await;
    assert_eq!(result["isError"], true);
    assert!(text(&result).contains("OPENROUTER_API_KEY"));
}

fn settings(change: impl Fn(&mut Settings)) -> Settings {
    let mut settings = Settings {
        openrouter_api_key: Some(Secret::new("or")),
        ai_gateway_api_key: Some(Secret::new("gw")),
        chrome: Some(CHROME.into()),
        ..Settings::default()
    };
    change(&mut settings);
    settings
}

fn argv(arguments: &[&str]) -> Vec<String> {
    arguments.iter().map(|argument| argument.to_string()).collect()
}

#[test]
fn test_configure_reads_secrets_and_ceilings_from_flags() {
    let args = parse(&argv(&[
        "--local",
        "--secret",
        "password=SHOP_PASSWORD@https://Shop.example.com",
        "--max-dollars",
        "0.25",
        "--allow-authorize",
    ]))
    .unwrap();
    let environ = HashMap::from([("SHOP_PASSWORD".to_string(), "hunter2".to_string())]);
    let config = configure(&args, &settings(|s| s.ai_gateway_api_key = None), &environ).unwrap();
    assert_eq!(
        config.secrets,
        [DeclaredSecret {
            name: "password".into(),
            value: "hunter2".into(),
            origin: "https://shop.example.com".into()
        }]
    );
    assert_eq!(config.ceilings.max_dollars, Some(0.25));
    assert!(config.allow_authorize);
}

#[test]
fn test_configure_refuses_a_setup_that_cannot_work() {
    type Change = fn(&mut Settings);
    let cases: Vec<(&[&str], Change, &str)> = vec![
        (&["--secret", "password=UNSET@https://a.example"], |_| {}, "unset variables: UNSET"),
        (&["--profile", "p", "--max-concurrent", "2"], |_| {}, "one Chrome at a time"),
        (&[], |_| {}, "BROWSER_USE_API_KEY"),
        (&["--local", "--cloud-profile", "p"], |_| {}, "cloud_profile needs the cloud browser"),
        (&["--headed"], |s| s.chrome = Some("no-such-chrome".into()), "Chrome was not found"),
        // FASTBROWSE_HEADED alone still means local Chrome, as it did before cloud became the default.
        (
            &[],
            |s| {
                s.chrome = Some("no-such-chrome".into());
                s.headed = true;
            },
            "Chrome was not found",
        ),
        (
            &["--local"],
            |s| {
                s.openrouter_api_key = None;
                s.ai_gateway_api_key = None;
            },
            "OPENROUTER_API_KEY",
        ),
        (
            &["--local"],
            |s| {
                s.ai_gateway_api_key = None;
                s.decision_source = Some(DecisionSource::Gateway);
            },
            "AI_GATEWAY_API_KEY",
        ),
        (&["--max-steps", "0"], |_| {}, "--max-steps"),
        (&["--max-dollars", "-1"], |_| {}, "--max-dollars"),
    ];
    for (arguments, change, message) in cases {
        let args = parse(&argv(arguments)).unwrap();
        let error = configure(&args, &settings(change), &HashMap::new()).unwrap_err().0;
        assert!(error.contains(message), "{arguments:?}: {error}");
    }
}

#[test]
fn test_a_secret_flag_must_name_an_origin() {
    for value in ["password=VAR", "password@https://a.example", "=VAR@https://a.example", "p=V@https://a.example/login"]
    {
        assert!(matches!(parse(&argv(&["--secret", value])), Err(fastbrowse_cli::argparse::Exit::Usage(_))), "{value}");
    }
}

#[test]
fn test_a_stand_in_run_that_names_nothing_is_refused() {
    assert!(matches!(parse(&argv(&["--run-task", "nope"])), Err(fastbrowse_cli::argparse::Exit::Usage(_))));
}

#[test]
fn test_a_secret_origin_is_parsed_as_an_origin() {
    let args = parse(&argv(&["--secret", "p=V@https://a.example:8443"])).unwrap();
    assert_eq!(args.secret, [("p".to_string(), "V".to_string(), "https://a.example:8443".to_string())]);
}

#[test]
fn test_loopback_hosts() {
    for (host, loopback) in [("127.0.0.1", true), ("::1", true), ("localhost", true), ("0.0.0.0", false)] {
        assert_eq!(is_loopback(host), loopback, "{host}");
    }
}

/// Every loopback address runs without a token, so each needs the Host and Origin checks that stop DNS rebinding.
#[tokio::test]
async fn test_every_loopback_address_checks_host_and_origin() {
    let server = McpServer::new(ServerConfig::default(), Recorder::new(answered(Status::Complete)).runner());
    let listener = tokio::net::TcpListener::bind("127.0.0.2:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { mcp::http::serve_on(listener, server, "127.0.0.2").await });
    let post = async |headers: &str| {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.2", port)).await.unwrap();
        let request = format!(
            "POST /mcp HTTP/1.1\r\n{headers}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\
             Connection: close\r\n\r\n{{}}"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    };
    assert!(post("Host: evil.test:80").await.starts_with("HTTP/1.1 421"));
    let origin = format!("Host: 127.0.0.2:{port}\r\nOrigin: http://evil.test:80");
    assert!(post(&origin).await.starts_with("HTTP/1.1 403"));
    let own = post(&format!("Host: 127.0.0.2:{port}")).await;
    assert!(own.starts_with("HTTP/1.1 406"), "{own}");
}

async fn http(port: u16, request: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

#[tokio::test]
async fn test_http_needs_the_bearer_token_except_for_the_health_check() {
    let config = ServerConfig { mcp_token: Some("s3cret".into()), ..ServerConfig::default() };
    let server = McpServer::new(config, Recorder::new(answered(Status::Complete)).runner());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { mcp::http::serve_on(listener, server, "127.0.0.1").await });
    let request = |method: &str, path: &str, token: Option<&str>| {
        let authorization = token.map_or(String::new(), |token| format!("Authorization: Bearer {token}\r\n"));
        format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n{authorization}\
             Content-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        )
    };
    assert!(http(port, &request("GET", "/healthz", None)).await.starts_with("HTTP/1.1 200 OK"));
    let refused = http(port, &request("POST", "/mcp", None)).await;
    assert!(refused.starts_with("HTTP/1.1 401"));
    assert!(refused.to_lowercase().contains("www-authenticate: bearer"));
    assert!(http(port, &request("POST", "/mcp", Some("nope"))).await.starts_with("HTTP/1.1 401"));
    // Past the gate, the MCP app answers (406 here: the probe does not accept an event stream).
    let allowed = http(port, &request("POST", "/mcp", Some("s3cret"))).await;
    assert!(allowed.starts_with("HTTP/1.1 406"), "{allowed}");
}

#[tokio::test]
async fn test_a_run_that_overruns_its_deadline_is_abandoned() {
    let hangs = runner(|args: Call| async move {
        args.host.cancel.guard(tokio::time::sleep(Duration::from_secs(30))).await?;
        unreachable!("the deadline stops the run first")
    });
    let config = ServerConfig { grace_seconds: 0.0, ..ServerConfig::default() };
    let (result, _) = Client::new(config, hangs).call(json!({"task": "t", "start": START, "max_seconds": 0.05})).await;
    assert_eq!(result["isError"], true);
    assert!(text(&result).contains("overran max_seconds=0.05"));
}

#[tokio::test]
async fn test_a_call_waiting_for_a_slot_does_not_spend_its_own_time_limit() {
    // The deadline is fixed when it is built, so building it before the wait would expire in the queue.
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (on_start, on_release) = (started.clone(), release.clone());
    let both = runner(move |args: Call| {
        let (started, release) = (on_start.clone(), on_release.clone());
        async move {
            if args.task.task == "first" {
                started.notify_one();
                release.notified().await;
            }
            Ok(answered(Status::Complete))
        }
    });
    let mut client = Client::new(ServerConfig { max_concurrent: 1, ..ServerConfig::default() }, both);
    client.start_call(json!({"task": "first", "start": START}));
    // The waiting call's whole limit passes while the slot is held.
    let waiting = client.start_call(json!({"task": "second", "start": START, "max_seconds": 0.2}));
    started.notified().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    release.notify_one();
    let result = client.reply(waiting, &mut Vec::new()).await;
    assert_eq!(result["result"]["isError"], false, "{result}");
}

#[test]
fn test_a_secret_may_be_named_for_the_account_it_belongs_to() {
    // The `@` that introduces the origin is the one after the variable, not the one in an email-shaped name.
    let args = parse(&argv(&["--secret", "user@example.com=LOGIN@https://shop.example"])).unwrap();
    assert_eq!(
        args.secret,
        [("user@example.com".to_string(), "LOGIN".to_string(), "https://shop.example".to_string())]
    );
}

#[tokio::test]
async fn test_budget_stop_details_survive_the_mcp_transport() {
    let run = RunResult {
        budget: Some(BudgetStop { resource: BudgetResource::Steps, limit: 20.0 }),
        ..answered(Status::BudgetExceeded)
    };
    let (result, _) =
        call(ServerConfig::default(), &Recorder::new(run), json!({"task": "Find it", "start": START})).await;
    assert_eq!(result["structuredContent"]["budget"], json!({"resource": "steps", "limit": 20.0}));
}
