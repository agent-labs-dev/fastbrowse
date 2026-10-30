//! tests/test_serve_run.py: `run` over the serve protocol, as a client sees it, with `run_task` replaced by a recorder.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use common::{
    CHROME, Recorder, START, binary, browser_event, result, run_params, runner, serving, settings, settings_with,
    step_event,
};
use fastbrowse_cli::runner::RunError;
use fastbrowse_core::models::{Authorization, Limits, LocalChrome, Status};
use serde_json::{Value, json};
use tokio::sync::Notify;

async fn received(fields: Value) -> common::Call {
    let recorder = Recorder::default();
    let mut client = serving(recorder.runner(), settings());
    client.result("run", Some(run_params("run-1", fields))).await;
    let mut calls = recorder.calls();
    assert_eq!(calls.len(), 1);
    calls.remove(0)
}

/// A PATH whose only program is something named ffmpeg, which is all a recording is checked for.
fn with_ffmpeg() -> tempdir::Dir {
    let dir = tempdir::Dir::new();
    let ffmpeg = dir.path.join("ffmpeg");
    std::fs::write(&ffmpeg, "#!/bin/sh\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&ffmpeg, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

mod tempdir {
    pub struct Dir {
        pub path: std::path::PathBuf,
    }

    impl Dir {
        pub fn new() -> Dir {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "fastbrowse-cli-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Dir { path }
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[tokio::test]
async fn test_run_calls_run_task_and_replies_with_its_result() {
    let recorder = Recorder {
        result: Some(fastbrowse_core::models::RunResult {
            answer: Some("The latest version is 0.28.1".into()),
            final_url: Some("https://pypi.org/project/httpx/".into()),
            ..result(Status::Complete)
        }),
        ..Recorder::default()
    };
    let mut client = serving(recorder.runner(), settings());
    let response =
        client.request("run", Some(json!({"run_id": "run-1", "task": "What is the latest version of httpx?"}))).await;
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["status"], "complete");
    assert_eq!(response["result"]["answer"], "The latest version is 0.28.1");
    assert_eq!(response["result"]["final_url"], "https://pypi.org/project/httpx/");
    assert_eq!(recorder.calls()[0].task.task, "What is the latest version of httpx?");
}

#[tokio::test]
async fn test_the_fields_that_describe_the_task_arrive_as_run_tasks_arguments() {
    let tools = tempdir::Dir::new();
    let call = received(json!({
        "start": START,
        "inputs": {"email": "ada@example.com"},
        // Bytes that are not text, and whose base64 differs between the standard and the URL-safe alphabet.
        "attachments": [{"name": "receipt.pdf", "mime_type": "application/pdf", "content": "+//+"}],
        "limits": {"max_steps": 12, "max_dollars": 0.5, "max_seconds": 90},
        "authorization": {"irreversible_actions": true},
        "downloads": tools.path.join("downloads"),
        "viewport": [1280, 720],
    }))
    .await;
    assert_eq!(call.task.task, "Add the kettle to the cart");
    assert_eq!(call.task.start.as_deref(), Some(START));
    assert_eq!(call.task.inputs.unwrap()["email"], "ada@example.com");
    assert_eq!(call.task.attachments[0].name, "receipt.pdf");
    assert_eq!(call.task.attachments[0].content.0, b"\xfb\xff\xfe");
    assert_eq!(
        call.task.limits,
        Some(Limits { max_steps: Some(12), max_dollars: Some(0.5), max_seconds: Some(90.0), ..Limits::default() })
    );
    assert_eq!(call.task.authorization, Some(Authorization { irreversible_actions: true }));
    assert_eq!(call.task.downloads, Some(tools.path.join("downloads")));
    assert_eq!(call.task.viewport, Some((1280, 720)));
}

/// One `run` with `record` to the binary, whose PATH is `path`: the reply, or the refusal.
fn recorded(path: &std::path::Path) -> Value {
    let params = run_params("run-1", json!({"record": path.join("run.mp4")}));
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": "run", "params": params});
    let mut process = command(&["--stdio", "--run-task", "scripted_run:echo"])
        .env("PATH", path)
        .env("BROWSER_USE_API_KEY", "unused")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = request.to_string();
    line.push('\n');
    process.stdin.take().unwrap().write_all(line.as_bytes()).unwrap();
    let output = process.wait_with_output().unwrap();
    let first = output.stdout.split(|b| *b == b'\n').next().unwrap();
    serde_json::from_slice(first).unwrap()
}

// PATH is the process's own, so the recording cases run the binary with a PATH of their choosing.
#[test]
fn test_a_recording_reaches_run_task_when_ffmpeg_is_on_path() {
    let tools = with_ffmpeg();
    let reply = recorded(&tools.path);
    assert_eq!(reply["result"]["data"]["params"]["record"], json!(tools.path.join("run.mp4")));
}

#[test]
fn test_a_recording_without_ffmpeg_is_a_configuration_error() {
    let empty = tempdir::Dir::new();
    let reply = recorded(&empty.path);
    assert_eq!(reply["error"]["code"], -32003);
    assert!(reply["error"]["message"].as_str().unwrap().contains("ffmpeg"));
}

#[tokio::test]
async fn test_fields_left_out_leave_run_task_its_own_defaults() {
    let call = received(json!({})).await;
    assert!(call.task.start.is_none() && call.task.inputs.is_none() && call.task.attachments.is_empty());
    // None is what `run_task` reads as "the default limits" and "nothing authorized".
    assert!(call.task.limits.is_none() && call.task.authorization.is_none());
    assert!(call.task.downloads.is_none() && call.task.record.is_none() && call.task.viewport.is_none());
    assert!(!call.task.attach);
    // A run in a visible window stays behind it unless the caller asks to watch.
    assert!(!call.task.foreground && received(json!({"foreground": true})).await.task.foreground);
}

#[tokio::test]
async fn test_the_browser_is_the_cloud_one_unless_the_run_asks_for_another() {
    let call = received(json!({})).await;
    assert_eq!(call.host.browser_api_key.as_deref(), Some("bu"));
    assert!(call.task.cdp_url.is_none() && call.task.cdp_port.is_none());
    // Unset here: the run browses from the United States when no country is asked for.
    assert!(call.task.proxy_country.is_none());
}

#[tokio::test]
async fn test_cloud_options_reach_the_cloud_browser() {
    let call = received(json!({"cloud_profile": "prof_1", "proxy_country": "UK"})).await;
    assert_eq!(call.host.browser_api_key.as_deref(), Some("bu"));
    assert_eq!(call.task.cloud_profile.as_deref(), Some("prof_1"));
    assert_eq!(call.task.proxy_country.as_deref(), Some("UK"));
}

#[tokio::test]
async fn test_local_chrome_runs_without_the_cloud_key() {
    let chrome = |headed, profile: Option<&str>| LocalChrome {
        binary: Some(CHROME.into()),
        headed,
        profile: profile.map(PathBuf::from),
    };
    for (fields, expected) in [
        (json!({"local": true}), chrome(false, None)),
        (json!({"chrome": {"headed": true}}), chrome(true, None)),
        (json!({"chrome": {"profile": "/tmp/kept"}}), chrome(false, Some("/tmp/kept"))),
    ] {
        // A headed window and a kept profile are things only local Chrome has, so each implies it, as in the CLI.
        let call = received(fields).await;
        assert!(call.host.browser_api_key.is_none());
        assert_eq!(call.task.chrome, Some(expected));
    }
}

#[tokio::test]
async fn test_local_chrome_needs_no_cloud_key_in_the_environment() {
    let mut client =
        serving(Recorder::default().runner(), settings_with(|settings| settings.browser_use_api_key = None));
    let result = client.result("run", Some(run_params("run-1", json!({"local": true})))).await;
    assert_eq!(result["status"], "complete");
}

#[tokio::test]
async fn test_headed_and_profile_from_the_environment_count_as_they_do_for_the_cli() {
    let recorder = Recorder::default();
    let mut client = serving(
        recorder.runner(),
        settings_with(|settings| {
            settings.headed = true;
            settings.profile = Some("/tmp/from-env".into());
        }),
    );
    client.result("run", Some(run_params("run-1", json!({})))).await;
    let call = &recorder.calls()[0];
    assert!(call.host.browser_api_key.is_none());
    assert_eq!(
        call.task.chrome,
        Some(LocalChrome { binary: Some(CHROME.into()), headed: true, profile: Some("/tmp/from-env".into()) })
    );
}

#[tokio::test]
async fn test_a_chrome_binary_named_by_the_run_replaces_the_one_in_the_environment() {
    let recorder = Recorder::default();
    let mut client =
        serving(recorder.runner(), settings_with(|settings| settings.chrome = Some("not-installed-anywhere".into())));
    client.result("run", Some(run_params("run-1", json!({"local": true, "chrome": {"binary": CHROME}})))).await;
    assert_eq!(recorder.calls()[0].task.chrome.as_ref().unwrap().binary.as_deref(), Some(CHROME));
}

#[tokio::test]
async fn test_a_browser_handed_over_is_driven_without_starting_one() {
    for (fields, url, port) in [
        (
            json!({"cdp_url": "ws://127.0.0.1:9222/devtools/browser/abc"}),
            Some("ws://127.0.0.1:9222/devtools/browser/abc"),
            None,
        ),
        (json!({"cdp_port": 9222, "attach": true, "target_match": "Inbox"}), None, Some(9222)),
    ] {
        let call = received(fields.clone()).await;
        assert_eq!((call.task.cdp_url.as_deref(), call.task.cdp_port), (url, port));
        assert!(call.host.browser_api_key.is_none());
        assert_eq!(call.task.attach, fields.get("attach").is_some());
        assert_eq!(call.task.target_match.as_deref(), fields.get("target_match").and_then(Value::as_str));
    }
}

#[tokio::test]
async fn test_events_arrive_in_order_with_the_run_id_and_before_the_reply() {
    let recorder = Recorder {
        events: vec![
            browser_event(Some("https://live.example.com/b1"), Some("b1")),
            step_event(1, None),
            step_event(2, Some(b"\xfb\xff\xfe".to_vec())),
        ],
        ..Recorder::default()
    };
    let mut client = serving(recorder.runner(), settings());
    let request_id = client.call("run", Some(run_params("run-7", json!({}))));
    let mut messages = Vec::new();
    for _ in 0..4 {
        messages.push(client.receive().await);
    }
    let methods: Vec<Value> = messages.iter().map(|m| m.get("method").cloned().unwrap_or(Value::Null)).collect();
    assert_eq!(methods, [json!("run/event"), json!("run/event"), json!("run/event"), Value::Null]);
    assert!(messages[..3].iter().all(|message| message.get("id").is_none()));
    assert!(messages[..3].iter().all(|message| message["params"]["run_id"] == "run-7"));
    let events: Vec<&Value> = messages[..3].iter().map(|message| &message["params"]["event"]).collect();
    assert_eq!(*events[0], json!({"type": "browser", "live_url": "https://live.example.com/b1", "browser_id": "b1"}));
    assert_eq!(
        (&events[1]["type"], &events[1]["step"]["index"], &events[1]["frame"]),
        (&json!("step"), &json!(1), &Value::Null)
    );
    assert_eq!((&events[2]["step"]["index"], &events[2]["step"]["target"]), (&json!(2), &json!("Add to cart")));
    // Base64 in the standard alphabet. The URL-safe one would have written "-__-".
    assert_eq!(events[2]["frame"], "+//+");
    assert_eq!(messages[3]["id"], request_id);
    assert_eq!(messages[3]["result"]["status"], "complete");
}

#[tokio::test]
async fn test_a_run_that_ends_in_any_status_replies_with_its_result() {
    for status in Status::ALL {
        let ended = fastbrowse_core::models::RunResult {
            answer: None,
            error: Some("what stopped it".into()),
            final_frame: Some(b"\x89PNG\xff".to_vec()),
            ..result(*status)
        };
        let recorder = Recorder { result: Some(ended), ..Recorder::default() };
        let mut client = serving(recorder.runner(), settings());
        let response = client.request("run", Some(run_params("run-1", json!({})))).await;
        assert!(response.get("error").is_none());
        assert_eq!(response["result"]["status"], status.as_str());
        assert_eq!(response["result"]["error"], "what stopped it");
        assert!(response["result"].get("final_frame").is_none());
    }
}

#[tokio::test]
async fn test_invalid_params_are_refused_before_run_task_is_called() {
    for (params, named) in [
        (json!({"run_id": "run-1"}), "task"),
        (json!({"task": "t"}), "run_id"),
        (run_params("run-1", json!({"headless": true})), "headless"),
        (run_params("run-1", json!({"limits": {"max_steps": 0}})), "limits.max_steps"),
        (run_params("run-1", json!({"chrome": {"headed": true, "devtools": true}})), "chrome.devtools"),
        (
            run_params(
                "run-1",
                json!({"attachments": [{"name": "a", "mime_type": "text/plain", "content": "not base64!"}]}),
            ),
            "attachments[0].content",
        ),
        (run_params("run-1", json!({"viewport": [1280]})), "viewport"),
        (run_params("run-1", json!({"proxy_country": "gb"})), "proxy_country"),
        (run_params("run-1", json!({"local": true, "cloud_profile": "prof_1"})), "cloud_profile needs the cloud"),
        (run_params("run-1", json!({"local": true, "proxy_country": "uk"})), "proxy_country needs the cloud"),
        (run_params("run-1", json!({"cdp_url": "ws://b.test/devtools", "cdp_port": 9222})), "cdp_port both"),
        (run_params("run-1", json!({"cdp_url": "http://b.test/devtools"})), "ws://"),
        (run_params("run-1", json!({"cdp_port": 70000})), "cdp_port"),
        (run_params("run-1", json!({"attach": true})), "attach and target_match need"),
        (run_params("run-1", json!({"cdp_port": 9222, "chrome": {"headed": true}})), "chrome.headed"),
    ] {
        let recorder = Recorder::default();
        let mut client = serving(recorder.runner(), settings());
        let response = client.request("run", Some(params)).await;
        assert_eq!(response["error"]["code"], -32602, "{response}");
        assert!(response["error"]["message"].as_str().unwrap().contains(named), "{response}");
        assert!(recorder.calls().is_empty());
        assert!(client.inbox.is_empty());
    }
}

#[tokio::test]
async fn test_a_profile_from_the_environment_counts_against_a_handed_over_browser() {
    let recorder = Recorder::default();
    let settings = settings_with(|settings| settings.profile = Some("/tmp/from-env".into()));
    let mut client = serving(recorder.runner(), settings);
    let response = client.request("run", Some(run_params("run-1", json!({"cdp_port": 9222})))).await;
    assert_eq!(response["error"]["code"], -32602, "{response}");
    assert!(response["error"]["message"].as_str().unwrap().contains("chrome.profile"), "{response}");
    assert!(recorder.calls().is_empty());
}

#[tokio::test]
async fn test_a_configuration_error_is_refused_before_run_task_is_called() {
    type Change = fn(&mut fastbrowse_clients::environment::Settings);
    let no_model_keys: Change = |s| {
        s.openrouter_api_key = None;
        s.ai_gateway_api_key = None;
    };
    let cases: Vec<(Change, Value, &str)> = vec![
        (no_model_keys, json!({}), "OPENROUTER_API_KEY"),
        (no_model_keys, json!({"local": true}), "OPENROUTER_API_KEY"),
        (|s| s.browser_use_api_key = None, json!({}), "BROWSER_USE_API_KEY"),
        (|s| s.chrome = Some("not-installed-anywhere".into()), json!({"local": true}), "Chrome was not found"),
    ];
    for (change, fields, named) in cases {
        let recorder = Recorder::default();
        let mut client = serving(recorder.runner(), settings_with(change));
        let response = client.request("run", Some(run_params("run-1", fields))).await;
        assert_eq!(response["error"]["code"], -32003, "{response}");
        assert!(response["error"]["message"].as_str().unwrap().contains(named), "{response}");
        assert!(recorder.calls().is_empty());
        assert!(client.inbox.is_empty());
    }
}

#[tokio::test]
async fn test_a_configuration_error_from_the_run_itself_gets_the_same_code() {
    let unconfigured =
        runner(|_| async { Err(RunError::Configuration("invalid settings: FASTBROWSE_LLM_REASONING".into())) });
    let mut client = serving(unconfigured, settings());
    let response = client.request("run", Some(run_params("run-1", json!({})))).await;
    assert_eq!(response["error"], json!({"code": -32003, "message": "invalid settings: FASTBROWSE_LLM_REASONING"}));
}

#[tokio::test]
async fn test_a_second_run_while_one_is_active_is_busy_and_does_not_disturb_the_first() {
    let hold = Arc::new(Notify::new());
    let recorder =
        Recorder { events: vec![browser_event(None, None)], hold: Some(hold.clone()), ..Recorder::default() };
    let mut client = serving(recorder.runner(), settings());
    let first = client.call("run", Some(run_params("run-1", json!({}))));
    // The first run's event is the proof that it is under way and not merely queued.
    assert_eq!(client.receive().await["params"]["run_id"], "run-1");
    let refused = client.request("run", Some(run_params("run-2", json!({})))).await;
    assert_eq!(refused["error"]["code"], -32001);
    assert!(refused["error"]["message"].as_str().unwrap().contains("run-1"));
    assert_eq!(recorder.calls().len(), 1);
    hold.notify_one();
    assert_eq!(client.reply(first).await["result"]["status"], "complete");
}

#[tokio::test]
async fn test_a_run_after_the_first_has_finished_succeeds() {
    let recorder = Recorder::default();
    let mut client = serving(recorder.runner(), settings());
    let first = client.result("run", Some(run_params("run-1", json!({})))).await;
    let second = client.result("run", Some(run_params("run-2", json!({"task": "Then check out"})))).await;
    assert_eq!((first["status"].clone(), second["status"].clone()), (json!("complete"), json!("complete")));
    let tasks: Vec<String> = recorder.calls().into_iter().map(|call| call.task.task).collect();
    assert_eq!(tasks, ["Add the kettle to the cart", "Then check out"]);
}

#[tokio::test]
async fn test_a_refused_run_leaves_the_server_free_for_the_next() {
    for refused in [
        run_params("run-2", json!({"limits": {"max_steps": 0}})),
        run_params("run-2", json!({"local": true, "cloud_profile": "prof_1"})),
    ] {
        let mut client = serving(Recorder::default().runner(), settings());
        assert!(client.request("run", Some(refused)).await.get("error").is_some());
        let result = client.result("run", Some(run_params("run-3", json!({})))).await;
        assert_eq!(result["status"], "complete");
    }
}

#[tokio::test]
async fn test_the_server_answers_other_requests_while_a_run_is_active() {
    let hold = Arc::new(Notify::new());
    let mut client = serving(Recorder { hold: Some(hold.clone()), ..Recorder::default() }.runner(), settings());
    let run = client.call("run", Some(run_params("run-1", json!({}))));
    let handshake = client.result("initialize", Some(json!({"protocol_version": 1}))).await;
    hold.notify_one();
    let reply = client.reply(run).await;
    assert_eq!(handshake["protocol_version"], 1);
    assert_eq!(reply["result"]["status"], "complete");
}

#[tokio::test]
async fn test_a_run_that_raises_ends_as_an_error_result_and_the_server_keeps_serving() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let flaky = runner(move |_| {
        let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async move {
            if attempt == 0 {
                return Err(RunError::Invalid("Chrome went away".into()));
            }
            Ok(result(Status::Complete))
        }
    });
    let mut client = serving(flaky, settings());
    let failed = client.request("run", Some(run_params("run-1", json!({})))).await;
    let result = client.result("run", Some(run_params("run-2", json!({})))).await;
    assert_eq!(
        (failed["result"]["status"].clone(), failed["result"]["error"].clone()),
        (json!("error"), json!("Chrome went away"))
    );
    assert_eq!(result["status"], "complete");
}

fn command(arguments: &[&str]) -> Command {
    let mut command = Command::new(binary());
    command.arg("serve").args(arguments).env("OPENROUTER_API_KEY", "unused").env("AI_GATEWAY_API_KEY", "unused");
    command
}

#[test]
fn test_the_hidden_option_swaps_in_a_named_callable() {
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "run",
        "params": run_params("run-1", json!({"local": true, "chrome": {"binary": CHROME}})),
    });
    let mut process = command(&["--stdio", "--run-task", "tests.serve_scripts:echo"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = process.stdin.take().unwrap();
    stdin.write_all(format!("{request}\n").as_bytes()).unwrap();
    stdin.flush().unwrap();
    // Read before the input is closed: the end of input ends the server, and a run with it.
    let mut stdout = BufReader::new(process.stdout.take().unwrap());
    let (mut event, mut reply) = (String::new(), String::new());
    stdout.read_line(&mut event).unwrap();
    stdout.read_line(&mut reply).unwrap();
    drop(stdin);
    assert!(process.wait().unwrap().success());
    assert_eq!(
        serde_json::from_str::<Value>(&event).unwrap(),
        json!({
            "jsonrpc": "2.0",
            "method": "run/event",
            "params": {"run_id": "run-1", "event": {"type": "browser", "live_url": null, "browser_id": null}},
        })
    );
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["result"]["answer"], "echo: Add the kettle to the cart");
}

#[test]
fn test_the_hidden_option_is_not_in_the_help() {
    let served = command(&["--help"]).output().unwrap();
    assert!(served.status.success());
    let stdout = String::from_utf8_lossy(&served.stdout);
    assert!(stdout.contains("--stdio"));
    assert!(!stdout.contains("run-task") && !stdout.to_lowercase().contains("run_task"));
}

#[test]
fn test_a_name_that_is_not_a_callable_is_a_usage_error() {
    for name in [
        "tests.serve_scripts:missing",
        "tests.no_such_module:echo",
        "tests.serve_scripts",
        "tests.serve_scripts:__doc__",
    ] {
        let served = command(&["--stdio", "--run-task", name]).stdin(Stdio::null()).output().unwrap();
        assert_eq!(served.status.code(), Some(2));
        assert!(served.stdout.is_empty());
        assert!(String::from_utf8_lossy(&served.stderr).contains(name));
    }
}
