//! The CLI's flags, its preflight errors on both streams, and its exit codes, with `run_task`
//! replaced by a recorder.

mod common;

use std::collections::HashMap;
use std::io::Write;
use std::process::Command;
use std::sync::{Arc, Mutex};

use common::{Call, Recorder, binary, model_settings, result};
use fastbrowse_cli::cli::{self, Build, Context, Streams};
use fastbrowse_clients::environment::{Secret, Settings};
use fastbrowse_core::models::{CostBasis, CostBreakdown, CostComponent, CostLine, RunResult, Status};
use serde_json::Value;

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buffer {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// One CLI invocation: what it wrote to each stream, its exit code, and the run it started, if any.
struct Ran {
    out: String,
    err: String,
    code: i32,
    call: Option<Call>,
}

impl Ran {
    fn json(&self) -> Value {
        serde_json::from_str(&self.out).expect("a JSON result on stdout")
    }
}

/// Pinned to config-file-free settings, so a checkout's `.env` cannot add a profile the guards would read.
async fn cli_with(argv: &[&str], settings: Settings, environ: &[(&str, &str)], answer: RunResult) -> Ran {
    cli_built(argv, settings, environ, answer, Build::default()).await
}

async fn cli_built(
    argv: &[&str],
    settings: Settings,
    environ: &[(&str, &str)],
    answer: RunResult,
    build: Build,
) -> Ran {
    let recorder = Recorder { result: Some(answer), ..Recorder::default() };
    let (out, err) = (Buffer::default(), Buffer::default());
    let context = Context {
        runner: recorder.runner(),
        settings: Arc::new(move || Ok(settings.clone())),
        environ: environ.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>(),
        streams: Streams { out: Arc::new(Mutex::new(out.clone())), err: Arc::new(Mutex::new(err.clone())) },
        build,
    };
    let argv: Vec<String> = argv.iter().map(|arg| arg.to_string()).collect();
    let code = cli::main(&argv, &context).await;
    let call = recorder.calls().pop();
    Ran { out: out.text(), err: err.text(), code, call }
}

async fn cli(argv: &[&str]) -> Ran {
    cli_with(argv, model_settings(), &[], RunResult { answer: Some("ok".into()), ..result(Status::Complete) }).await
}

#[tokio::test]
async fn test_the_version_and_the_json_result_name_the_commit_that_ran() {
    let sha = "e1d478b".to_string() + &"0".repeat(33);
    let built = Build { git_sha: Some(sha.clone()), git_dirty: Some(true) };
    let answer = || RunResult { answer: Some("ok".into()), ..result(Status::Complete) };
    let version = cli_built(&["--version"], model_settings(), &[], answer(), built.clone()).await;
    assert_eq!(version.out, format!("fastbrowse {} (e1d478b, dirty)\n", env!("CARGO_PKG_VERSION")));
    let ran = cli_built(&["t", "--json"], model_settings(), &[], answer(), built).await;
    assert_eq!(
        ran.json()["build"],
        serde_json::json!({"fastbrowse_version": env!("CARGO_PKG_VERSION"), "git_sha": sha, "git_dirty": true})
    );
    // A release knows only its version, and says so rather than naming a commit.
    let release = cli(&["--version"]).await;
    assert_eq!(release.code, 0);
    assert_eq!(release.out, format!("fastbrowse {}\n", env!("CARGO_PKG_VERSION")));
    assert_eq!(cli(&["t", "--json"]).await.json()["build"]["git_sha"], Value::Null);
}

#[tokio::test]
async fn test_a_preflight_error_under_json_still_leaves_a_result_on_stdout() {
    let ran = cli(&["t", "--start", "https://example.com", "--json", "--secret", "p=NOT_SET"]).await;
    let result = ran.json();
    assert_eq!(result["status"], "error");
    assert!(result["answer"].is_null() && result["steps"] == serde_json::json!([]));
    assert!(result["error"].as_str().unwrap().contains("NOT_SET"));
    assert!(ran.err.contains("NOT_SET"));
    assert_eq!(ran.code, 1);
}

#[tokio::test]
async fn test_a_preflight_error_without_json_writes_nothing_to_stdout() {
    let ran = cli(&["t", "--start", "https://example.com", "--secret", "p=NOT_SET"]).await;
    assert_eq!(ran.out, "");
    assert_eq!(ran.err, "fastbrowse: --secret names unset variables: NOT_SET\n");
}

#[tokio::test]
async fn test_a_limit_argparse_accepts_but_the_run_cannot_use_is_refused_as_a_configuration_error() {
    for (flag, value) in [("--max-steps", "0"), ("--max-dollars", "-1")] {
        let ran = cli(&["t", "--start", "https://example.com", "--json", flag, value]).await;
        let result = ran.json();
        assert_eq!(result["status"], "error");
        assert!(result["error"].as_str().unwrap().contains(flag) && ran.err.contains(flag));
    }
}

#[tokio::test]
async fn test_a_credential_without_a_start_page_is_refused_rather_than_dropped() {
    for flag in [["--secret", "p=SET"], ["--bitwarden", "vault-item"]] {
        let argv = ["t", "--json", flag[0], flag[1]];
        let ran = cli_with(&argv, model_settings(), &[("SET", "value")], result(Status::Complete)).await;
        let result = ran.json();
        assert_eq!(result["status"], "error");
        assert!(result["error"].as_str().unwrap().contains("--start") && ran.err.contains("--start"));
    }
}

#[tokio::test]
async fn test_cdp_url_hands_run_task_the_url_and_no_cloud_key() {
    let ran = cli(&["t", "--start", "https://example.com", "--cdp-url", "ws://browser.test/devtools"]).await;
    assert_eq!(ran.code, 0);
    let call = ran.call.unwrap();
    assert_eq!(call.task.cdp_url.as_deref(), Some("ws://browser.test/devtools"));
    assert!(call.host.browser_api_key.is_none());
}

#[tokio::test]
async fn test_fetch_starts_no_browser_and_asks_for_no_cloud_key() {
    let ran = cli(&["t", "--start", "https://example.com", "--fetch"]).await;
    assert_eq!(ran.code, 0, "{}", ran.err);
    let call = ran.call.unwrap();
    assert!(call.task.fetch);
    assert!(call.task.chrome.is_none() && call.host.browser_api_key.is_none());
}

#[tokio::test]
async fn test_fetch_refuses_flags_that_shape_a_browser() {
    for (flag, field) in [
        (&["--local"][..], "local"),
        (&["--headed"], "chrome"),
        (&["--cdp-port", "9222"], "cdp_port"),
        (&["--cursor"], "cursor"),
        (&["--downloads", "/tmp/d"], "downloads"),
    ] {
        let mut argv = vec!["t", "--start", "https://example.com", "--fetch"];
        argv.extend_from_slice(flag);
        let ran = cli(&argv).await;
        assert!(ran.call.is_none());
        assert_eq!(ran.err, format!("fastbrowse: fetch starts no browser, so it cannot be combined with {field}\n"));
    }
}

#[tokio::test]
async fn test_browser_skips_the_read_first_and_refuses_fetch() {
    let ran = cli(&["t", "--start", "https://example.com", "--browser", "--local"]).await;
    assert_eq!(ran.code, 0, "{}", ran.err);
    let call = ran.call.unwrap();
    assert!(call.task.browser && !call.task.fetch && call.task.chrome.is_some());
    let ran = cli(&["t", "--start", "https://example.com", "--browser", "--fetch"]).await;
    assert!(ran.call.is_none());
    assert!(ran.err.contains("fetch"), "{}", ran.err);
}

#[tokio::test]
async fn test_cdp_url_refuses_flags_that_shape_a_started_browser() {
    for (flag, field) in [
        (&["--local"][..], "local"),
        (&["--headed"], "chrome.headed"),
        (&["--profile", "/tmp/kept"], "chrome.profile"),
        (&["--cloud-profile", "prof_1"], "cloud_profile"),
        (&["--proxy-country", "uk"], "proxy_country"),
    ] {
        let mut argv = vec!["t", "--start", "https://example.com", "--json", "--cdp-url", "ws://browser.test/devtools"];
        argv.extend_from_slice(flag);
        let ran = cli(&argv).await;
        let error = ran.json()["error"].as_str().unwrap().to_string();
        assert!(error.contains("cdp_url") && error.contains(field), "{error}");
        assert!(ran.err.contains(field));
    }
}

#[tokio::test]
async fn test_proxy_country_refuses_local_chrome() {
    // Local Chrome browses from this machine's IP, so a requested country would be silently ignored.
    let ran = cli(&["t", "--start", "https://example.com", "--json", "--local", "--proxy-country", "uk"]).await;
    let result = ran.json();
    assert_eq!(result["status"], "error");
    assert!(result["error"].as_str().unwrap().contains("proxy_country needs the cloud browser"));
}

#[tokio::test]
async fn test_cdp_url_counts_a_profile_from_the_environment() {
    // FASTBROWSE_PROFILE implies --profile; a handed-over browser refuses it just the same.
    let settings = Settings { profile: Some("/tmp/kept".into()), ..model_settings() };
    let argv = ["t", "--start", "https://example.com", "--json", "--cdp-url", "ws://browser.test/devtools"];
    let ran = cli_with(&argv, settings, &[], result(Status::Complete)).await;
    assert!(ran.json()["error"].as_str().unwrap().contains("chrome.profile"));
}

#[tokio::test]
async fn test_cdp_url_requires_a_websocket_url() {
    let ran =
        cli(&["t", "--start", "https://example.com", "--json", "--cdp-url", "https://browser.test/devtools"]).await;
    assert!(ran.json()["error"].as_str().unwrap().contains("ws://"));
}

#[test]
fn test_secret_preflight_reports_both_unset_variable_and_missing_origin() {
    let pairs = vec![("password".to_string(), "MISSING_PASSWORD".to_string(), None)];
    let error = cli::secrets(&pairs, None, None, &HashMap::new()).unwrap_err().0;
    assert!(error.contains("unset variables: MISSING_PASSWORD"));
    assert!(error.contains("origin"));
}

#[tokio::test]
async fn test_explicit_cloud_overrides_environment_profile() {
    let settings = Settings {
        profile: Some("/tmp/profile".into()),
        browser_use_api_key: Some(Secret::new("browser-key")),
        ..model_settings()
    };
    let ran = cli_with(&["task", "--cloud"], settings, &[], fastbrowse_cli::options::error_result("test")).await;
    let call = ran.call.unwrap();
    assert_eq!(call.host.browser_api_key.as_deref(), Some("browser-key"));
    assert!(call.task.chrome.is_none());
}

#[tokio::test]
async fn test_cli_login_and_blocked_have_distinct_exit_codes() {
    let mut codes = Vec::new();
    for status in [Status::NeedsLogin, Status::Blocked] {
        let answer = RunResult { status, ..fastbrowse_cli::options::error_result("test") };
        codes.push(cli_with(&["task", "--local"], model_settings(), &[], answer).await.code);
    }
    assert_eq!(codes, [4, 6]);
}

#[tokio::test]
async fn test_login_advice_is_added_only_to_cli_output() {
    let answer = RunResult { status: Status::NeedsLogin, ..fastbrowse_cli::options::error_result("Sign-in required") };
    let ran = cli_with(&["task", "--local", "--json"], model_settings(), &[], answer).await;
    assert_eq!(ran.code, 4);
    assert!(ran.json()["error"].as_str().unwrap().contains("--secret NAME=ENV_VAR@https://host"));
}

#[tokio::test]
async fn test_cdp_port_hands_run_task_the_port_and_no_cloud_key() {
    let argv = [
        "t",
        "--start",
        "https://example.com",
        "--cdp-port",
        "9222",
        "--attach",
        "--target-match",
        "app",
        "--foreground",
    ];
    let ran = cli(&argv).await;
    assert_eq!(ran.code, 0);
    let call = ran.call.unwrap();
    assert_eq!(call.task.cdp_port, Some(9222));
    assert!(call.task.attach && call.task.foreground);
    assert_eq!(call.task.target_match.as_deref(), Some("app"));
    assert!(call.host.browser_api_key.is_none());
}

#[tokio::test]
async fn test_cannot_combine_cdp_url_and_cdp_port() {
    let argv = [
        "t",
        "--start",
        "https://example.com",
        "--json",
        "--cdp-url",
        "ws://browser.test/devtools",
        "--cdp-port",
        "9222",
    ];
    let ran = cli(&argv).await;
    assert!(ran.json()["error"].as_str().unwrap().contains("cdp_url and cdp_port both name"));
}

#[tokio::test]
async fn test_attach_requires_cdp() {
    let ran = cli(&["t", "--start", "https://example.com", "--json", "--attach"]).await;
    assert!(ran.json()["error"].as_str().unwrap().contains("attach and target_match need"));
}

#[tokio::test]
async fn test_cdp_port_must_be_a_port() {
    for bad_port in ["0", "-5", "65536"] {
        let ran = cli(&["t", "--start", "https://example.com", "--json", "--cdp-port", bad_port]).await;
        let error = ran.json()["error"].as_str().unwrap().to_string();
        assert!(
            error.contains("cdp-port expects a port from 1 to 65535") || error.contains("cdp_port expects"),
            "{error}"
        );
    }
}

#[test]
fn test_only_serve_as_the_first_argument_is_the_subcommand() {
    // `serve` is taken as a subcommand in one position only, so every other command line is still a task. The
    // preflight refuses a run with no model key, so a task ends as a configuration error, exit 1, where `serve` would
    // have been a usage error, exit 2.
    for argv in [
        &["serve the invoices page", "--cdp-url", "ws://b.test/x"][..],
        &["server", "--cdp-url", "ws://b.test/x"],
        &["--cdp-port", "9222", "serve"],
    ] {
        let output = Command::new(binary()).args(argv).env_remove("FASTBROWSE_PROFILE").output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{argv:?}: {}", String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stderr).starts_with("fastbrowse: "));
    }
}

#[tokio::test]
async fn test_the_summary_line_says_when_some_calls_reported_no_cost() {
    for (basis, expected) in [(CostBasis::Metered, "($0.0020, 0 steps)"), (CostBasis::Unknown, "plus calls with no")] {
        let lines = vec![
            CostLine::new(CostComponent::Jev, CostBasis::Metered, Some(0.002)),
            CostLine::new(CostComponent::Llm, basis, if basis == CostBasis::Unknown { None } else { Some(0.0) }),
        ];
        let answer = RunResult { cost: CostBreakdown { lines }, ..fastbrowse_cli::options::error_result("test") };
        let ran = cli_with(&["task", "--local"], model_settings(), &[], answer).await;
        assert!(ran.out.contains(expected), "{}", ran.out);
    }
}

#[tokio::test]
async fn test_usage_errors_match_argparse() {
    let usage = include_str!("../generated/cli.txt").split("\n\n").next().unwrap();
    for (argv, message) in [
        (&[][..], "the following arguments are required: task"),
        (&["t", "--cloud", "--local"], "argument --local: not allowed with argument --cloud"),
        (&["t", "--cdp-port", "x"], "argument --cdp-port: invalid int value: 'x'"),
        (
            &["t", "--proxy-country", "gb"],
            "argument --proxy-country: Browser Use's code for the United Kingdom is uk, not gb",
        ),
        (&["t", "--secret", "abc"], "argument --secret: expected NAME=ENV_VAR, got 'abc'"),
        (&["t", "--bogus"], "unrecognized arguments: --bogus"),
        (&["t", "--max"], "ambiguous option: --max could match --max-steps, --max-dollars"),
        (&["t", "--start"], "argument --start: expected one argument"),
    ] {
        let ran = cli(argv).await;
        assert_eq!(ran.code, 2);
        assert_eq!(ran.out, "");
        assert_eq!(ran.err, format!("{usage}\nfastbrowse: error: {message}\n"));
    }
}

#[tokio::test]
async fn test_help_matches_argparse() {
    let ran = cli(&["--help"]).await;
    assert_eq!(ran.code, 0);
    assert_eq!(ran.out, include_str!("../generated/cli.txt"));
}

#[test]
fn test_the_trace_names_the_build_before_the_run() {
    let dir = std::env::temp_dir().join(format!("fastbrowse-trace-build-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let trace = dir.join("trace.jsonl");
    let output = Command::new(binary())
        .args(["t", "--cdp-url", "ws://b.test/x", "--trace"])
        .arg(&trace)
        .env_remove("FASTBROWSE_PROFILE")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", String::from_utf8_lossy(&output.stderr));
    let text = std::fs::read_to_string(&trace).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    let first: Value = serde_json::from_str(text.lines().next().expect("a trace line")).unwrap();
    assert_eq!(first["event"], "build");
    assert_eq!(first["fastbrowse_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(first["git_sha"], Value::Null);
}
