//! tests/test_run.py: run_task decisions that need no real browser.

use std::sync::Arc;

use fastbrowse_clients::environment::Settings;
use fastbrowse_clients::http::{Http, Request, Response, TransportError};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{DecisionClient, DecisionError, Evaluation, Question};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{LLMPurpose, Status};
use fastbrowse_core::telemetry::Ledger;
use fastbrowse_run::{RunError, RunTask, run_task};
use indexmap::IndexMap;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

/// Clients no test here gets far enough to need.
struct Silent;

impl DecisionClient for Silent {
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
        _: i64,
        _: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        Box::pin(async { Err(LLMError::Failed("not scripted".into())) })
    }
}

fn task(change: impl FnOnce(&mut RunTask)) -> RunTask {
    let mut options = RunTask {
        jev: Some(Arc::new(Silent)),
        llm: Some(Arc::new(Silent)),
        settings: Some(Settings::from_sources::<&str, &str>(&[], None).unwrap()),
        ..RunTask::default()
    };
    change(&mut options);
    options
}

fn answering(status: u16) -> Http {
    Http::new(Arc::new(move |_: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
        Box::pin(async move { Ok(Response::new(status, Vec::new())) })
    }))
}

#[tokio::test]
async fn test_a_devtools_port_without_its_discovery_route_names_the_websocket_to_pass() {
    let options = task(|o| {
        o.cdp_port = Some(9222);
        o.http = Some(answering(404));
    });
    let result = run_task("read the page", options).await.unwrap();
    assert_eq!(result.status, Status::Error);
    let error = result.error.unwrap();
    assert!(error.contains("answered 404") && error.contains("DevToolsActivePort"), "{error}");
    assert!(error.contains("ws://127.0.0.1:<port><path>"), "{error}");
}

#[tokio::test]
async fn test_a_devtools_port_refusing_discovery_names_the_status_without_the_chrome_inspect_advice() {
    for status in [401, 403, 500] {
        let options = task(|o| {
            o.cdp_port = Some(9222);
            o.http = Some(answering(status));
        });
        let result = run_task("read the page", options).await.unwrap();
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
    let options = task(|o| {
        o.cdp_url = Some(format!("ws://127.0.0.1:{port}/devtools/browser/x"));
        o.attach = true;
    });
    let result = run_task("read the page", options).await.unwrap();
    assert_eq!(result.status, Status::Unavailable, "{:?}", result.error);
    assert!(result.error.is_some());
}

#[tokio::test]
async fn browser_choices_that_contradict_each_other_are_errors_before_any_browser_starts() {
    let cases: Vec<(RunTask, &str)> = vec![
        (task(|o| o.cloud_extensions = vec!["x".into()]), "cloud_extensions"),
        (
            task(|o| {
                o.cdp_url = Some("ws://localhost:1/x".into());
                o.cdp_port = Some(1);
            }),
            "pass one",
        ),
        (task(|o| o.attach = true), "attach and target_match require cdp_url or cdp_port"),
        (
            task(|o| {
                o.cdp_url = Some("ws://localhost:1/x".into());
                o.browser_api_key = Some("key".into());
            }),
            "a cloud key would start a second one",
        ),
        (task(|o| o.cloud_profile = Some("p".into())), "needs a cloud browser"),
    ];
    for (options, expected) in cases {
        let result = run_task("read the page", options).await.unwrap();
        assert_eq!(result.status, Status::Error);
        assert!(result.error.as_deref().is_some_and(|e| e.contains(expected)), "{:?}", result.error);
    }
}

#[tokio::test]
async fn a_recording_cannot_be_combined_with_allowed_origins() {
    let options = task(|o| {
        o.allowed_origins = Some(vec!["https://example.test".into()]);
        o.record = Some("r.mp4".into());
    });
    let refused = run_task("read the page", options).await.unwrap_err();
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
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let options = task(|o| {
        o.cdp_url = Some(format!("ws://127.0.0.1:{port}/devtools/browser/x"));
        o.cancel = Some(Arc::new(move || -> BoxFuture<'static, ()> {
            let mut stopped = stopped.clone();
            Box::pin(async move {
                let _ = stopped.wait_for(|stop| *stop).await;
            })
        }));
    });
    let run = tokio::spawn(run_task("read the page", options));
    let mut socket = connected.await.unwrap();
    stop.send(true).unwrap();
    let refused = run.await.unwrap().unwrap_err();
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
        task(move |o| {
            o.start = Some("https://shop.example/app".into());
            o.http = Some(shell_and_no_devtools());
            change(o);
        })
    };
    // Fetch mode ends where the read does, naming why.
    let result = run_task("read the page", read(|o| o.fetch = true)).await.unwrap();
    assert_eq!(result.status, Status::Blocked, "{:?}", result.error);
    // Fetch-first takes the same shell to the browser: the handed-over port is asked for its socket.
    let result = run_task(
        "read the page",
        read(|o| {
            o.fetch_first = true;
            o.cdp_port = Some(9222);
        }),
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Error);
    assert!(result.error.as_deref().is_some_and(|error| error.contains("answered 404")), "{:?}", result.error);
}
