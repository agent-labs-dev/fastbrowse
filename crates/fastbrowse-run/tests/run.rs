//! run_task decisions that need no real browser.

use std::sync::Arc;

use fastbrowse_clients::environment::Settings;
use fastbrowse_clients::http::{Http, Request, Response, TransportError};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{Answer, DecisionClient, DecisionError, Evaluation, Question};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{LLMPurpose, Status};
use fastbrowse_core::telemetry::Ledger;
use fastbrowse_run::{Cancel, Host, RunError, RunTask, on_cloud, run_task};
use indexmap::IndexMap;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

/// Clients no test here gets far enough to need.
struct Silent;

impl DecisionClient for Silent {
    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        fastbrowse_core::decision::DecisionProfile::jev()
    }

    fn evaluate<'a>(
        &'a self,
        _: &'a Value,
        _: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async { Err(DecisionError::Failed("not scripted".into())) })
    }
}

impl LLMClient for Silent {
    fn generate_value<'a>(
        &'a self,
        _: LLMPurpose,
        _: &'a [Message],
        _: &'a dyn ResponseSchema,
        _: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        Box::pin(async { Err(LLMError::Failed("not scripted".into())) })
    }
}

/// "read the page", run with clients that answer nothing and settings from nowhere, as `change` alters them.
fn task(change: impl FnOnce(&mut RunTask, &mut Host)) -> (RunTask, Host) {
    let mut task = RunTask { task: "read the page".into(), ..RunTask::default() };
    let mut host = Host {
        jev: Some(Arc::new(Silent)),
        llm: Some(Arc::new(Silent)),
        settings: Some(Settings::from_sources::<&str, &str>(&[], None).unwrap()),
        ..Host::default()
    };
    change(&mut task, &mut host);
    (task, host)
}

async fn run((task, host): (RunTask, Host)) -> Result<fastbrowse_core::models::RunResult, RunError> {
    run_task(task, host).await
}

fn answering(status: u16) -> Http {
    Http::new(Arc::new(move |_: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
        Box::pin(async move { Ok(Response::new(status, Vec::new())) })
    }))
}

#[tokio::test]
async fn test_a_devtools_port_without_its_discovery_route_names_the_websocket_to_pass() {
    let options = task(|o, h| {
        o.cdp_port = Some(9222);
        h.http = Some(answering(404));
    });
    let result = run(options).await.unwrap();
    assert_eq!(result.status, Status::Error);
    let error = result.error.unwrap();
    assert!(error.contains("answered 404") && error.contains("DevToolsActivePort"), "{error}");
    assert!(error.contains("ws://127.0.0.1:<port><path>"), "{error}");
}

#[tokio::test]
async fn test_a_devtools_port_refusing_discovery_names_the_status_without_the_chrome_inspect_advice() {
    for status in [401, 403, 500] {
        let options = task(|o, h| {
            o.cdp_port = Some(9222);
            h.http = Some(answering(status));
        });
        let result = run(options).await.unwrap();
        assert_eq!(result.status, Status::Error);
        let error = result.error.unwrap();
        assert!(error.contains(&format!("answered {status}")), "{error}");
        assert!(!error.contains("chrome://inspect") && !error.contains("DevToolsActivePort"), "{error}");
    }
}

#[tokio::test]
async fn test_a_browser_connection_that_closes_is_an_outage() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = [0u8; 1024];
            let _ = socket.read(&mut buffer).await;
        }
    });
    let options = task(|o, _| {
        o.cdp_url = Some(format!("ws://127.0.0.1:{port}/devtools/browser/x"));
        o.attach = true;
    });
    let result = run(options).await.unwrap();
    assert_eq!(result.status, Status::Unavailable, "{:?}", result.error);
    assert!(result.error.is_some());
}

#[tokio::test]
async fn browser_choices_that_contradict_each_other_are_errors_before_any_browser_starts() {
    let cases: Vec<((RunTask, Host), &str)> = vec![
        (
            task(|o, _| {
                o.local = true;
                o.cloud_extensions = vec!["x".into()];
            }),
            "cloud_extensions",
        ),
        (
            task(|o, _| {
                o.cdp_url = Some("ws://localhost:1/x".into());
                o.cdp_port = Some(1);
            }),
            "pass one",
        ),
        (
            task(|o, _| {
                o.local = true;
                o.attach = true;
            }),
            "attach and target_match need cdp_url or cdp_port",
        ),
        (
            task(|o, h| {
                o.cdp_url = Some("ws://localhost:1/x".into());
                h.browser_api_key = Some("key".into());
            }),
            "a cloud key would start a second one",
        ),
        (
            task(|o, _| {
                o.local = true;
                o.cloud_profile = Some("p".into());
            }),
            "needs the cloud browser",
        ),
        (
            task(|o, _| {
                o.local = true;
                o.proxy_country = Some("de".into());
            }),
            "proxy_country",
        ),
    ];
    for (options, expected) in cases {
        // Arguments that contradict each other are refused outright; a browser that cannot start ends the run.
        let error = match run(options).await {
            Ok(result) => {
                assert_eq!(result.status, Status::Error);
                result.error.unwrap_or_default()
            }
            Err(refused) => refused.to_string(),
        };
        assert!(error.contains(expected), "{error}");
    }
}

#[tokio::test]
async fn a_run_naming_no_browser_needs_the_cloud_key() {
    let error = run(task(|_, _| {})).await.unwrap_err();
    assert!(matches!(&error, RunError::Configuration(message) if message.contains("BROWSER_USE_API_KEY")), "{error}");
}

#[test]
fn only_a_named_local_or_handed_browser_leaves_the_cloud() {
    let chrome = fastbrowse_core::models::LocalChrome::default();
    assert!(on_cloud(&RunTask::default(), &chrome));
    assert!(!on_cloud(&RunTask { local: true, ..RunTask::default() }, &chrome));
    assert!(!on_cloud(&RunTask { cdp_port: Some(9222), ..RunTask::default() }, &chrome));
    assert!(!on_cloud(&RunTask { fetch: true, ..RunTask::default() }, &chrome));
    let headed = fastbrowse_core::models::LocalChrome { headed: true, ..chrome };
    assert!(!on_cloud(&RunTask::default(), &headed));
}

#[tokio::test]
async fn a_recording_cannot_be_combined_with_allowed_origins() {
    let options = task(|o, _| {
        o.allowed_origins = Some(vec!["https://example.test".into()]);
        o.record = Some("r.mp4".into());
    });
    let refused = run(options).await.unwrap_err();
    assert!(matches!(refused, RunError::Invalid(message) if message.contains("allowed_origins")));
}

#[tokio::test]
async fn a_cancelled_run_closes_its_connection_before_it_returns() {
    // A browser that takes the connection and never answers, so the run is still opening it when it is stopped.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (accepted, connected) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let _ = accepted.send(socket);
    });
    let cancel = Cancel::default();
    let options = task(|o, h| {
        o.cdp_url = Some(format!("ws://127.0.0.1:{port}/devtools/browser/x"));
        h.cancel = cancel.clone();
    });
    let running = tokio::spawn(run(options));
    let mut socket = connected.await.unwrap();
    cancel.trip();
    let refused = running.await.unwrap().unwrap_err();
    assert_eq!(refused, RunError::Cancelled);
    // The run has returned, so its end of the connection is closed: what is left to read ends.
    let mut rest = Vec::new();
    let read = tokio::time::timeout(std::time::Duration::from_secs(5), socket.read_to_end(&mut rest)).await;
    assert!(read.is_ok_and(|read| read.is_ok()), "the connection was left open");
}

/// A page the static read refuses as a script-rendered shell, and a DevTools port with no discovery route.
fn shell_and_no_devtools() -> Http {
    Http::new(Arc::new(move |request: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
        let response = if request.url.contains("/json/version") {
            Response::new(404, Vec::new())
        } else {
            let shell = format!("<html><body><div id=root></div><script>{}</script></body></html>", "x".repeat(5000));
            Response::new(200, shell).with_header("content-type", "text/html")
        };
        Box::pin(async move { Ok(response) })
    }))
}

#[tokio::test]
async fn test_fetch_first_goes_to_the_browser_when_the_read_needs_one() {
    let read = |change: fn(&mut RunTask)| {
        task(move |o, h| {
            o.start = Some("https://shop.example/app".into());
            h.http = Some(shell_and_no_devtools());
            change(o);
        })
    };
    // Fetch mode ends where the read does, naming why.
    let result = run(read(|o| o.fetch = true)).await.unwrap();
    assert_eq!(result.status, Status::Blocked, "{:?}", result.error);
    // Fetch-first takes the same shell to the browser: the handed-over port is asked for its socket.
    let result = run(read(|o| {
        o.fetch_first = true;
        o.cdp_port = Some(9222);
    }))
    .await
    .unwrap();
    assert_eq!(result.status, Status::Error);
    assert!(result.error.as_deref().is_some_and(|error| error.contains("answered 404")), "{:?}", result.error);
}

/// Jev as the scripted client answers, except that every page is a bot check.
struct SeesBotCheck(fastbrowse_agent::scripted::ScriptedJev);

impl DecisionClient for SeesBotCheck {
    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        self.0.profile()
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async move {
            let mut evaluation = self.0.evaluate(state, questions).await?;
            if let Some(Answer::Noul(answer)) = evaluation.answers.get_mut("bot_check") {
                answer.probability = 1.0;
            }
            Ok(evaluation)
        })
    }
}

#[tokio::test]
async fn test_fetch_first_takes_a_bot_check_jev_saw_in_the_document_to_the_browser() {
    let http = Http::new(Arc::new(move |request: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
        let response = if request.url.contains("/json/version") {
            Response::new(404, Vec::new())
        } else {
            let wall = "<html><body><h1>Just a moment...</h1><p>Checking your browser.</p></body></html>";
            Response::new(200, wall).with_header("content-type", "text/html")
        };
        Box::pin(async move { Ok(response) })
    }));
    let options = task(|o, h| {
        h.jev = Some(Arc::new(SeesBotCheck(fastbrowse_agent::scripted::ScriptedJev::new(Vec::new()))));
        h.llm = Some(Arc::new(fastbrowse_agent::scripted::ScriptedLLM));
        o.start = Some("https://shop.example/app".into());
        h.http = Some(http);
        o.fetch_first = true;
        o.cdp_port = Some(9222);
    });
    let result = run(options).await.unwrap();
    // The handed-over port is asked for its socket, so the run reached the browser.
    assert_eq!(result.status, Status::Error, "{:?}", result.error);
    assert!(result.error.as_deref().is_some_and(|error| error.contains("answered 404")), "{:?}", result.error);
}
