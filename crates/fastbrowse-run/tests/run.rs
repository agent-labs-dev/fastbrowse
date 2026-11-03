//! run_task decisions, including one owned-local browser initialization check.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use fastbrowse_clients::environment::Settings;
use fastbrowse_clients::http::{Http, Request, Response, TransportError};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{Answer, DecisionClient, DecisionError, Evaluation, Question};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{CostBasis, CostComponent, CostLine, Event, LLMPurpose, Status};
use fastbrowse_core::protocol::CancellationReceipt;
use fastbrowse_core::telemetry::Ledger;
use fastbrowse_run::{Cancel, Host, RunError, RunTask, on_cloud, run_task};
use indexmap::IndexMap;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

/// Clients no test here gets far enough to need.
struct Silent;

struct RecordingDecision(Arc<Mutex<Vec<Value>>>);

impl DecisionClient for RecordingDecision {
    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        fastbrowse_core::decision::DecisionProfile::jev()
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        _: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        self.0.lock().unwrap().push(state.clone());
        Box::pin(async { Err(DecisionError::Failed("stop after recording observation".into())) })
    }
}

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

#[tokio::test]
async fn an_owned_local_run_applies_viewport_before_its_first_decision() {
    if fastbrowse_browser::adapters::local_chrome::find_chrome(None).is_none() {
        return;
    }
    let start = "data:text/html,<body><script>document.body.textContent=innerWidth+'x'+innerHeight</script>";
    for (width, height) in [(640, 480), (1024, 700)] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let host = Host {
            jev: Some(Arc::new(RecordingDecision(seen.clone()))),
            llm: Some(Arc::new(Silent)),
            settings: Some(Settings::from_sources::<&str, &str>(&[], None).unwrap()),
            ..Host::default()
        };
        let task = RunTask {
            task: "read the page".into(),
            start: Some(start.into()),
            local: true,
            browser: true,
            viewport: Some((width, height)),
            ..RunTask::default()
        };
        let result = run_task(task, host).await.expect("run returns a result");
        assert_eq!(result.status, Status::Error);
        let states = seen.lock().unwrap();
        let text = states.iter().find_map(|state| state.pointer("/page/text").and_then(Value::as_str));
        assert_eq!(text, Some(format!("{width}x{height}").as_str()), "states: {states:?}");
    }
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
    let error = run(task(|o, _| o.browser = true)).await.unwrap_err();
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

#[test]
fn a_viewport_is_local_or_cloud_and_rejects_handed_browsers() {
    let local = RunTask { local: true, viewport: Some((640, 480)), ..RunTask::default() };
    assert!(fastbrowse_run::check(&local).is_ok());
    let handed = RunTask { cdp_port: Some(9222), viewport: Some((640, 480)), ..RunTask::default() };
    let error = fastbrowse_run::check(&handed).unwrap_err().to_string();
    assert!(error.contains("viewport"), "{error}");
    let error =
        fastbrowse_run::check(&RunTask { viewport: Some((0, 480)), ..RunTask::default() }).unwrap_err().to_string();
    assert!(error.contains("greater than zero"), "{error}");
}

#[tokio::test]
async fn a_viewport_rejects_open_browser_before_invoking_the_callback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = calls.clone();
    let (_, mut host) = task(|o, _| o.viewport = Some((640, 480)));
    host.open_browser = Some(Arc::new(move || {
        callback_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err("callback must not run".to_string()) })
    }));
    let error = run_task(RunTask { viewport: Some((640, 480)), ..RunTask::default() }, host).await.unwrap_err();
    assert!(matches!(error, RunError::Invalid(message) if message.contains("viewport")));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
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
    assert_eq!(refused, RunError::Cancelled(CancellationReceipt::default()));
    // The run has returned, so its end of the connection is closed: what is left to read ends.
    let mut rest = Vec::new();
    let read = tokio::time::timeout(std::time::Duration::from_secs(5), socket.read_to_end(&mut rest)).await;
    assert!(read.is_ok_and(|read| read.is_ok()), "the connection was left open");
}

/// A caller's browser that cannot be opened, which shows a run reached it.
fn no_browser() -> fastbrowse_run::OpenBrowser {
    Arc::new(|| Box::pin(async { Err("no browser here".to_string()) }))
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
async fn test_a_fetch_only_answer_survives_optional_step_frames() {
    let options = task(|o, h| {
        o.fetch = true;
        o.start = Some("https://docs.example.test/value".into());
        h.http = Some(Http::new(Arc::new(|_: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
            Box::pin(async {
                Ok(Response::new(200, "<p>The documented value is 34.</p>").with_header("content-type", "text/html"))
            })
        })));
        h.jev = Some(Arc::new(fastbrowse_agent::scripted::ScriptedJev::new(vec![fastbrowse_agent::scripted::Step {
            operation: fastbrowse_core::models::Operation::Read,
            label: String::new(),
            pick: None,
        }])));
        h.llm = Some(Arc::new(fastbrowse_agent::scripted::ScriptedLLM));
        h.config = Some(fastbrowse_core::config::Config { step_frames: true, ..Default::default() });
        h.on_event = Some(Arc::new(|_| Box::pin(async {})));
    });
    let result = run(options).await.unwrap();
    assert_eq!(result.status, Status::Complete, "{:?}", result.error);
    assert!(result.answer.as_deref().unwrap().contains("34"));
    assert!(!result.citations.is_empty());
    assert!(result.error.is_none());
}

#[tokio::test]
async fn test_a_run_goes_to_the_browser_when_the_read_needs_one() {
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
    // By default the same shell goes to the browser: the caller's is asked for.
    let mut options = read(|_| {});
    options.1.open_browser = Some(no_browser());
    let result = run(options).await.unwrap();
    assert_eq!(result.status, Status::Error);
    assert!(result.error.as_deref().is_some_and(|error| error.contains("no browser here")), "{:?}", result.error);
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
async fn test_a_run_takes_a_bot_check_jev_saw_in_the_document_to_the_browser() {
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
        h.open_browser = Some(no_browser());
    });
    let result = run(options).await.unwrap();
    assert_eq!(result.status, Status::Error, "{:?}", result.error);
    assert!(result.error.as_deref().is_some_and(|error| error.contains("no browser here")), "{:?}", result.error);
}

/// Jev as `0` answers it, each call billed a cent, so a test can count what reached the receipt.
struct Priced<J>(J);

impl<J: DecisionClient> DecisionClient for Priced<J> {
    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        self.0.profile()
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async move {
            let evaluation = self.0.evaluate(state, questions).await?;
            Ok(Evaluation { cost: CostLine::new(CostComponent::Jev, CostBasis::Metered, Some(0.01)), ..evaluation })
        })
    }
}

fn priced(script: Vec<fastbrowse_agent::scripted::Step>) -> Arc<dyn DecisionClient> {
    Arc::new(Priced(fastbrowse_agent::scripted::ScriptedJev::new(script)))
}

fn read_step() -> fastbrowse_agent::scripted::Step {
    fastbrowse_agent::scripted::Step {
        operation: fastbrowse_core::models::Operation::Read,
        label: String::new(),
        pick: None,
    }
}

/// A fetched page whose text answers the task.
fn documented() -> Http {
    Http::new(Arc::new(|_: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
        Box::pin(async {
            Ok(Response::new(200, "<p>The documented value is 34.</p>").with_header("content-type", "text/html"))
        })
    }))
}

fn cancelled(outcome: Result<fastbrowse_core::models::RunResult, RunError>) -> CancellationReceipt {
    match outcome {
        Err(RunError::Cancelled(receipt)) => receipt,
        other => panic!("the run was not cancelled: {other:?}"),
    }
}

fn billed(receipt: &CancellationReceipt) -> usize {
    receipt.cost.lines.iter().filter(|line| line.basis == CostBasis::Metered && line.dollars == Some(0.01)).count()
}

#[tokio::test]
async fn a_run_cancelled_inside_its_event_handler_keeps_the_step_and_what_it_spent() {
    // The caller's handler never returns from the step event, so the run is stopped while the step is being told.
    let stepped = Arc::new(tokio::sync::Notify::new());
    let told = stepped.clone();
    let cancel = Cancel::default();
    let options = task(|o, h| {
        o.fetch = true;
        o.start = Some("https://docs.example.test/value".into());
        h.http = Some(documented());
        h.jev = Some(priced(vec![read_step()]));
        h.llm = Some(Arc::new(fastbrowse_agent::scripted::ScriptedLLM));
        h.cancel = cancel.clone();
        h.on_event = Some(Arc::new(move |event| {
            let told = told.clone();
            Box::pin(async move {
                if matches!(event, Event::Step(_)) {
                    told.notify_one();
                    std::future::pending::<()>().await;
                }
            })
        }));
    });
    let running = tokio::spawn(run(options));
    stepped.notified().await;
    cancel.trip();
    let receipt = cancelled(running.await.unwrap());
    assert_eq!(receipt.steps.len(), 1, "{:?}", receipt.steps);
    assert_eq!(receipt.steps[0].operation, fastbrowse_core::models::Operation::Read);
    assert!(billed(&receipt) > 0, "{:?}", receipt.cost);
    assert!(receipt.recordings.is_empty());
}

/// Jev as `inner` answers it until a step has been told, and then a request that never returns. Dropped, that
/// request records the charge nobody priced, as a provider client records a request it had sent.
struct HangsAfterAStep {
    inner: Arc<dyn DecisionClient>,
    stepped: Arc<std::sync::atomic::AtomicBool>,
    asked: Arc<tokio::sync::Notify>,
}

struct Unpriced(Option<fastbrowse_clients::validation::JevSpend>);

impl Drop for Unpriced {
    fn drop(&mut self) {
        if let Some(spend) = &self.0 {
            spend.record([CostLine::new(CostComponent::Jev, CostBasis::Unknown, None)]);
        }
    }
}

impl DecisionClient for HangsAfterAStep {
    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        self.inner.profile()
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        if !self.stepped.load(std::sync::atomic::Ordering::SeqCst) {
            return self.inner.evaluate(state, questions);
        }
        Box::pin(async move {
            let _sent = Unpriced(fastbrowse_clients::validation::JevSpend::current());
            self.asked.notify_one();
            std::future::pending().await
        })
    }
}

#[tokio::test]
async fn a_request_dropped_by_cancellation_keeps_its_unknown_charge() {
    let stepped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let asked = Arc::new(tokio::sync::Notify::new());
    let cancel = Cancel::default();
    let jev = HangsAfterAStep { inner: priced(vec![read_step()]), stepped: stepped.clone(), asked: asked.clone() };
    let options = task(|o, h| {
        o.fetch = true;
        o.start = Some("https://docs.example.test/value".into());
        h.http = Some(documented());
        h.jev = Some(Arc::new(jev));
        h.llm = Some(Arc::new(fastbrowse_agent::scripted::ScriptedLLM));
        h.cancel = cancel.clone();
        h.on_event = Some(Arc::new(move |event| {
            if matches!(event, Event::Step(_)) {
                stepped.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Box::pin(async {})
        }));
    });
    let running = tokio::spawn(run(options));
    asked.notified().await;
    cancel.trip();
    let receipt = cancelled(running.await.unwrap());
    assert_eq!(receipt.steps.len(), 1);
    assert!(billed(&receipt) > 0, "{:?}", receipt.cost);
    // The dropped request is counted once, and as unknown: nothing priced it.
    let unknown: Vec<_> = receipt.cost.lines.iter().filter(|line| line.basis == CostBasis::Unknown).collect();
    assert_eq!(unknown.len(), 1, "{:?}", receipt.cost);
    assert_eq!(unknown[0].dollars, None);
}

#[tokio::test]
async fn a_run_cancelled_while_its_browser_opens_keeps_what_the_fetched_read_spent() {
    let http = Http::new(Arc::new(move |_: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
        let wall = "<html><body><h1>Just a moment...</h1><p>Checking your browser.</p></body></html>";
        Box::pin(async move { Ok(Response::new(200, wall).with_header("content-type", "text/html")) })
    }));
    let opening = Arc::new(tokio::sync::Notify::new());
    let asked = opening.clone();
    let cancel = Cancel::default();
    let options = task(|o, h| {
        h.jev = Some(Arc::new(Priced(SeesBotCheck(fastbrowse_agent::scripted::ScriptedJev::new(Vec::new())))));
        h.llm = Some(Arc::new(fastbrowse_agent::scripted::ScriptedLLM));
        o.start = Some("https://shop.example/app".into());
        h.http = Some(http);
        h.cancel = cancel.clone();
        // The caller's browser never opens, so the run is stopped after the read and before any browser step.
        h.open_browser = Some(Arc::new(move || {
            let asked = asked.clone();
            Box::pin(async move {
                asked.notify_one();
                std::future::pending().await
            })
        }));
    });
    let running = tokio::spawn(run(options));
    opening.notified().await;
    cancel.trip();
    let receipt = cancelled(running.await.unwrap());
    assert!(billed(&receipt) > 0, "the fetched read's spend was lost: {:?}", receipt.cost);
    assert!(receipt.recordings.is_empty());
}
