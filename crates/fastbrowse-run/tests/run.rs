//! run_task decisions, including one owned-local browser initialization check.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use fastbrowse_clients::environment::Settings;
use fastbrowse_clients::http::{Http, Request, Response, TransportError};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{Answer, DecisionClient, DecisionError, Evaluation, Question};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{
    Authorization, BrowserUsed, BrowserVia, CostBasis, CostComponent, CostLine, Event, LLMPurpose, SecretValue, Status,
};
use fastbrowse_core::protocol::CancellationReceipt;
use fastbrowse_core::safety::ScopedSecrets;
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
        Box::pin(async { Err(DecisionError::failed("stop after recording observation")) })
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
        Box::pin(async { Err(DecisionError::failed("not scripted")) })
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
        Box::pin(async { Err(LLMError::failed(fastbrowse_core::llm::FaultClass::Client, "not scripted")) })
    }
}

/// The chat driver as the script `driver` plays it, keeping the page each turn was shown. Once `turns` turns have
/// been taken, the next is recorded and refused, so a test reads that page without the run acting on it.
struct Shown {
    driver: fastbrowse_agent::scripted::ScriptedDriver,
    turns: usize,
    prompts: Arc<Mutex<Vec<String>>>,
}

impl Shown {
    fn new(script: &str, turns: usize) -> (Arc<Shown>, Arc<Mutex<Vec<String>>>) {
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let script = fastbrowse_agent::scripted::steps(script).unwrap();
        let driver = fastbrowse_agent::scripted::ScriptedDriver::new(script);
        (Arc::new(Shown { driver, turns, prompts: prompts.clone() }), prompts)
    }
}

impl LLMClient for Shown {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        if purpose == LLMPurpose::Action {
            let taken = {
                let mut prompts = self.prompts.lock().unwrap();
                prompts.push(messages.last().map(|message| message.content.clone()).unwrap_or_default());
                prompts.len() > self.turns
            };
            if taken {
                return Box::pin(async {
                    Err(LLMError::failed(fastbrowse_core::llm::FaultClass::Client, "observation recorded"))
                });
            }
        }
        self.driver.generate_value(purpose, messages, schema, ledger)
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
        let (driver, seen) = Shown::new("", 0);
        let host = Host {
            jev: Some(Arc::new(fastbrowse_agent::scripted::ScriptedJev::new(Vec::new()))),
            llm: Some(driver),
            settings: Some(Settings::from_sources::<&str, &str>(&[], None).unwrap()),
            ..Host::default()
        };
        let task = RunTask {
            task: "read the page".into(),
            start: Some(start.into()),
            launch: true,
            browser: true,
            viewport: Some((width, height)),
            ..RunTask::default()
        };
        let result = run_task(task, host).await.expect("run returns a result");
        assert_eq!(result.status, Status::Error);
        let prompts = seen.lock().unwrap();
        assert!(prompts.iter().any(|prompt| prompt.contains(&format!("{width}x{height}"))), "shown: {prompts:?}");
    }
}

/// Records task-only startup calls, answering the rest as the script does.
struct HandoffDecision {
    startup: Arc<AtomicUsize>,
    scripted: fastbrowse_agent::scripted::ScriptedJev,
}

impl DecisionClient for HandoffDecision {
    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        self.scripted.profile()
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        if questions.contains_key("dates") {
            self.startup.fetch_add(1, Ordering::SeqCst);
            let task = state["task"].as_str().unwrap();
            let Question::Choice(output) = &questions["pick"] else { panic!("output choice is present") };
            assert!(task.contains("Inputs the user gave, by name: format = 'a screenshot'"));
            // The state carries the task with its inputs once; the questions name it rather than restating it.
            assert!(output.instructions.contains("`task`") && !output.instructions.contains(task));
        }
        self.scripted.evaluate(state, questions)
    }
}

#[tokio::test]
async fn an_initial_http_failure_keeps_task_judgments_when_the_browser_opens() {
    if fastbrowse_browser::adapters::local_chrome::find_chrome(None).is_none() {
        return;
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let start = format!("http://127.0.0.1:{}/", listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let page = "<body><script>document.body.textContent='The rendered value is 34.'</script>";
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
                page.len()
            );
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, reply.as_bytes()).await;
        }
    });
    let startup = Arc::new(AtomicUsize::new(0));
    // The fetched shell has run none of its scripts, so the driver waits there, which opens the browser. A wait the run
    // sets no time limit on lasts as long as it asks, so it asks for a short one on a page that may not change.
    let (driver, seen) = Shown::new("wait=2", 1);
    let options = task(|o, h| {
        o.start = Some(start.clone());
        o.task = "Report the value in input format.".into();
        o.inputs = Some(IndexMap::from([("format".into(), "a screenshot".into())]));
        o.local = true;
        o.launch = true;
        h.jev = Some(Arc::new(HandoffDecision {
            startup: startup.clone(),
            scripted: fastbrowse_agent::scripted::ScriptedJev::new(Vec::new()),
        }));
        h.llm = Some(driver.clone());
        h.http = Some(Http::new(Arc::new(|_: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
            Box::pin(async {
                Ok(Response::new(200, "<body><script src='/app.js'></script>").with_header("content-type", "text/html"))
            })
        })));
    });
    let result = run(options).await;
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    let result = result.expect("HTTP hands the run to the browser");
    assert_eq!(result.status, Status::Error);
    assert_eq!(startup.load(Ordering::SeqCst), 1, "task judgments must survive a failed first fetch");
    let seen = seen.lock().unwrap();
    assert!(seen.iter().any(|prompt| prompt.contains("The rendered value is 34.")), "shown: {seen:?}");
}

/// Answers whether the task answers a consent banner itself with the given probability, and everything else as the
/// script says.
struct ConsentDecision(f64, fastbrowse_agent::scripted::ScriptedJev);

impl DecisionClient for ConsentDecision {
    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        self.1.profile()
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        if !questions.contains_key("consent") {
            return self.1.evaluate(state, questions);
        }
        let answer = fastbrowse_core::decision::NoulAnswer { kind: Default::default(), probability: self.0 };
        Box::pin(async move {
            Ok(Evaluation {
                model: "jev".into(),
                answers: IndexMap::from([("consent".to_string(), Answer::Noul(answer))]),
                input_tokens: 0,
                cost: CostLine::new(CostComponent::Jev, CostBasis::Estimated, Some(0.0)),
                requests: 1,
            })
        })
    }
}

/// "Accept the cookie banner, then send a request" ended stuck: the browser refused the banner before it painted, so
/// the agent never could accept it, went back to look again, and found it refused again. A task that answers the
/// banner itself keeps it; any other still has it refused.
#[tokio::test]
async fn a_task_that_answers_the_cookie_banner_itself_is_left_the_banner() {
    if fastbrowse_browser::adapters::local_chrome::find_chrome(None).is_none() {
        return;
    }
    let start = serve_banner(
        "<button id='reject' type='button'>Reject all</button><button id='accept' type='button'>Accept all</button>",
    )
    .await;
    for (answers_it, shown) in [(0.9, true), (0.1, false)] {
        let text = page_after_refusal(&start, "Accept the cookie banner.", answers_it).await;
        assert_eq!(text.contains("Accept all"), shown, "consent answered {answers_it}: {text:?}");
    }
}

#[tokio::test]
async fn a_banner_that_offers_only_accept_is_never_accepted() {
    if fastbrowse_browser::adapters::local_chrome::find_chrome(None).is_none() {
        return;
    }
    // Refusing cookies never consents to them: a banner with nothing to refuse is left for the agent (#769).
    let start = serve_banner("<button id='accept' type='button'>Accept all</button>").await;
    let text = page_after_refusal(&start, "Report the portal's name.", 0.1).await;
    assert!(text.contains("Accept all") && !text.contains("ACCEPTED"), "{text:?}");
}

/// Serves, over HTTP as a site would (the browser's refusal does not act on a data: page), a page whose cookie banner
/// has the given buttons, each removing it and an accept leaving ACCEPTED behind.
async fn serve_banner(buttons: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let start = format!("http://127.0.0.1:{}/", listener.local_addr().unwrap().port());
    let page = format!(
        "<html><body><div id='consent' style='position:fixed;left:0;right:0;bottom:0'>This site uses cookies. \
         {buttons}</div><p>Support portal</p><script>for (const button of document.querySelectorAll('#consent button')) \
         button.onclick = () => {{ if (button.id === 'accept') document.body.append('ACCEPTED'); \
         document.getElementById('consent').remove(); }};</script></body></html>"
    );
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
                page.len()
            );
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, reply.as_bytes()).await;
        }
    });
    start
}

/// The page the driver is shown after its first step, a wait that gives the browser time to refuse the banner, with
/// Jev answering whether the task answers the banner itself at `answers_it`.
async fn page_after_refusal(start: &str, task: &str, answers_it: f64) -> String {
    let (driver, seen) = Shown::new("wait=2", 1);
    let host = Host {
        jev: Some(Arc::new(ConsentDecision(answers_it, fastbrowse_agent::scripted::ScriptedJev::new(Vec::new())))),
        llm: Some(driver),
        settings: Some(Settings::from_sources::<&str, &str>(&[], None).unwrap()),
        ..Host::default()
    };
    let task =
        RunTask { task: task.into(), start: Some(start.into()), launch: true, browser: true, ..RunTask::default() };
    let result = run_task(task, host).await.expect("run returns a result");
    assert_eq!(result.status, Status::Error);
    assert!(result.error.as_deref().is_some_and(|error| error.contains("observation recorded")), "{result:?}");
    // The page itself, not what the turn says the wait removed from it.
    let shown = seen.lock().unwrap().get(1).cloned().unwrap_or_default();
    shown.split_once("\nPage: ").map(|(_, page)| page.to_string()).unwrap_or_default()
}

/// Asked while the browser starts whether the task answers a consent banner itself, Jev was shown the task as the
/// caller wrote it, an address's credentials included: nothing had masked them yet (#1381).
#[tokio::test]
async fn test_the_consent_check_is_shown_the_task_with_its_credentials_masked() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let options = task(|o, h| {
        o.task = "Accept the cookies at https://shopper:t4sk-pw@shop.test/orders".into();
        o.browser = true;
        h.jev = Some(Arc::new(RecordingDecision(seen.clone())));
        h.open_browser = Some(no_browser());
    });
    let _ = run(options).await;
    let states = seen.lock().unwrap();
    let task = |state: &Value| state["task"].as_str().map(str::to_string);
    assert!(states.iter().filter_map(task).any(|task| task.contains("@shop.test/orders")), "{states:?}");
    for state in states.iter().map(Value::to_string) {
        assert!(!state.contains("t4sk-pw") && !state.contains("shopper"), "{state}");
    }
}

/// Stops the run at its first decision, a second after it, so the browser has had time to send frames.
struct StopsAfterASecond;

impl DecisionClient for StopsAfterASecond {
    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        fastbrowse_core::decision::DecisionProfile::jev()
    }

    fn evaluate<'a>(
        &'a self,
        _: &'a Value,
        _: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            Err(DecisionError::failed("stopped"))
        })
    }
}

/// A start with credentials can draw them from the first paint (a page writes `document.URL`, which keeps them), and
/// frames went out from the browser's opening until the agent was built and first looked (#1381). With credentials,
/// none of the run's frames goes out; without, the same page sends them.
#[tokio::test]
async fn test_no_frame_goes_out_of_a_run_whose_start_has_credentials() {
    // A silent pass without Chrome would look like no frame going out.
    assert!(
        fastbrowse_browser::adapters::local_chrome::find_chrome(None).is_some(),
        "test_no_frame_goes_out_of_a_run_whose_start_has_credentials needs Chrome on PATH"
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let page = "<html><body><p id=u></p><script>document.getElementById('u').textContent = document.URL; \
                    setInterval(() => document.body.style.background = `hsl(${Date.now() % 360}, 50%, 50%)`, 50)\
                    </script></body></html>";
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
                page.len()
            );
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, reply.as_bytes()).await;
        }
    });
    for (start, sent) in
        [(format!("http://shopper:fr4me-pw@127.0.0.1:{port}/"), false), (format!("http://127.0.0.1:{port}/"), true)]
    {
        let frames = Arc::new(AtomicUsize::new(0));
        let counted = frames.clone();
        let host = Host {
            jev: Some(Arc::new(StopsAfterASecond)),
            llm: Some(Arc::new(Silent)),
            settings: Some(Settings::from_sources::<&str, &str>(&[], None).unwrap()),
            on_frame: Some(Arc::new(move |_: Vec<u8>| {
                counted.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {})
            })),
            ..Host::default()
        };
        let task = RunTask {
            task: "read the page".into(),
            start: Some(start.clone()),
            launch: true,
            browser: true,
            ..RunTask::default()
        };
        let _ = run_task(task, host).await;
        assert_eq!(frames.load(Ordering::SeqCst) > 0, sent, "{start}");
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
    // An outage before the agent runs still names what failed, so a caller need not parse `error`.
    assert!(result.failure_kind.is_some(), "{:?}", result.error);
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
                o.launch = true;
                o.attach = true;
            }),
            "attach and target_match need cdp_url, cdp_port or extension",
        ),
        (
            task(|o, _| {
                o.extension = true;
                o.launch = true;
            }),
            "extension drives the browser the fastbrowse extension is connected in; drop launch",
        ),
        (
            task(|o, _| {
                o.extension_browser = Some("Work".into());
                o.cdp_port = Some(9222);
            }),
            "drop cdp_port",
        ),
        (
            task(|o, _| {
                o.extension = true;
                o.fetch = true;
            }),
            "cannot be combined with extension",
        ),
        (
            task(|o, _| {
                o.extension = true;
                o.allowed_origins = Some(vec!["https://example.com".into()]);
            }),
            "allowed_origins cannot be combined with extension",
        ),
        (
            task(|o, _| {
                o.fetch = true;
                o.inputs = Some([("name".to_string(), "Ada".to_string())].into_iter().collect());
                o.authorization = Some(Authorization { irreversible_actions: true });
            }),
            "fetch starts no browser, so it cannot be combined with inputs, authorization",
        ),
        (
            task(|o, h| {
                o.fetch = true;
                h.secrets = Some(Arc::new(ScopedSecrets::per_secret([(
                    "password".to_string(),
                    (SecretValue::from("hunter2"), vec!["https://example.com".to_string()]),
                )])));
            }),
            "fetch starts no browser, so it cannot be combined with secrets",
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
        (
            task(|o, _| {
                o.local = true;
                o.cloud_timeout_minutes = Some(30);
            }),
            "cloud_timeout_minutes needs the cloud browser",
        ),
        (
            task(|o, _| {
                o.cdp_url = Some("ws://localhost:1/x".into());
                o.cloud_timeout_minutes = Some(30);
            }),
            "drop cloud_timeout_minutes",
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
async fn a_fetch_run_given_a_resolver_that_declares_no_secret_is_not_refused_for_secrets() {
    // Python's `ScopedSecrets.per_secret({})` installs a resolver with nothing in it, which `serve` sees as an empty
    // list: counting the resolver itself refused that fetch run for secrets it was never given.
    let error = run(task(|o, h| {
        o.fetch = true;
        o.inputs = Some([("name".to_string(), "Ada".to_string())].into_iter().collect());
        h.secrets = Some(Arc::new(ScopedSecrets::per_secret(Vec::new())));
    }))
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "fetch starts no browser, so it cannot be combined with inputs");
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
    assert!(!on_cloud(&RunTask { launch: true, ..RunTask::default() }, &chrome));
    assert!(!on_cloud(&RunTask { cdp_port: Some(9222), ..RunTask::default() }, &chrome));
    assert!(!on_cloud(&RunTask { fetch: true, ..RunTask::default() }, &chrome));
    let headed = fastbrowse_core::models::LocalChrome { headed: true, ..chrome };
    assert!(!on_cloud(&RunTask::default(), &headed));
}

/// A local run drives the user's own Chrome through the extension, and starts a Chrome of its own only when asked
/// (#1005): a signed-out Chrome cannot do a signed-in task.
#[test]
fn local_uses_the_extension_unless_a_launch_is_asked_for() {
    let task = |task: RunTask| RunTask { task: "read the page".into(), ..task };
    let headed = fastbrowse_core::models::LocalChrome { headed: true, ..Default::default() };
    assert!(task(RunTask { local: true, ..RunTask::default() }).wants_extension());
    for launched in [
        RunTask { local: true, launch: true, ..RunTask::default() },
        RunTask { launch: true, ..RunTask::default() },
        RunTask { local: true, chrome: Some(headed), ..RunTask::default() },
        RunTask { local: true, fetch: true, ..RunTask::default() },
    ] {
        assert!(!task(launched).wants_extension());
    }
    assert!(
        fastbrowse_run::check(&task(RunTask { local: true, extension: true, ..RunTask::default() }), false).is_ok()
    );
    let refused = |given: RunTask| fastbrowse_run::check(&task(given), false).unwrap_err().to_string();
    let error = refused(RunTask { local: true, viewport: Some((640, 480)), ..RunTask::default() });
    assert!(error.contains("pass launch") && error.contains("viewport"), "{error}");
    assert!(
        !task(RunTask {
            local: true,
            chrome: Some(fastbrowse_core::models::LocalChrome {
                binary: Some("custom-chrome".into()),
                ..Default::default()
            }),
            ..RunTask::default()
        })
        .wants_extension()
    );
    let error = refused(RunTask { extension: true, launch: true, ..RunTask::default() });
    assert!(error.contains("drop launch"), "{error}");
    let error = refused(RunTask { local: true, cloud_profile: Some("p".into()), ..RunTask::default() });
    assert!(error.contains("cloud_profile needs the cloud browser"), "{error}");
}

/// `check` runs before `run_task`, outside the run's redactor, and its refusals quoted the value refused: a proxy
/// country or a DevTools endpoint holding an address's password reached the embedder's log whole (#1381).
#[test]
fn a_preflight_refusal_masks_the_credential_it_quotes() {
    let read = RunTask { task: "read the page".into(), ..RunTask::default() };
    for given in [
        RunTask { proxy_country: Some("https://ops:pf-d00r@h.test/".into()), ..read.clone() },
        RunTask { cdp_url: Some("http://ops:pf-d00r@h.test/".into()), ..read.clone() },
    ] {
        let error = fastbrowse_run::check(&given, false).unwrap_err().to_string();
        assert!(!error.contains("pf-d00r"), "{error}");
    }
}

#[test]
fn a_viewport_is_local_or_cloud_and_rejects_handed_browsers() {
    let local =
        RunTask { task: "read the page".into(), launch: true, viewport: Some((640, 480)), ..RunTask::default() };
    assert!(fastbrowse_run::check(&local, false).is_ok());
    let handed = RunTask {
        task: "read the page".into(),
        cdp_port: Some(9222),
        viewport: Some((640, 480)),
        ..RunTask::default()
    };
    let error = fastbrowse_run::check(&handed, false).unwrap_err().to_string();
    assert!(error.contains("viewport"), "{error}");
    let error = fastbrowse_run::check(
        &RunTask { task: "read the page".into(), viewport: Some((0, 480)), ..RunTask::default() },
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("greater than zero"), "{error}");
}

#[tokio::test]
async fn a_viewport_rejects_open_browser_before_invoking_the_callback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = calls.clone();
    let (task, mut host) = task(|o, _| o.viewport = Some((640, 480)));
    host.open_browser = Some(Arc::new(move || {
        callback_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err("callback must not run".to_string()) })
    }));
    let error = run_task(task, host).await.unwrap_err();
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
    // The receipt names the browser it was opening, as a finished result would.
    let browser = Some(Box::new(BrowserUsed { via: BrowserVia::Attached, profile: None, browser: None }));
    assert_eq!(refused, RunError::Cancelled(CancellationReceipt { browser, ..CancellationReceipt::default() }));
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

/// example.com's served page is a paragraph and a script that builds its heading and its one link. Read over HTTP,
/// it offered no controls, Jev was told nothing on it led anywhere else, and a run asked to follow its link ended
/// `complete` without following it (#1380). A page whose markup carries scripts is stated as read without running
/// them, and a wait there takes the run to a browser; a run limited to fetching is told no browser opens.
/// A page with no scripts still leads nowhere.
#[tokio::test]
async fn test_a_fetched_page_whose_links_a_script_builds_is_not_said_to_lead_nowhere() {
    let script_built = "<title>Example Domain</title><div><p>This domain is for use in documentation examples.</p></div>\
                        <script src=/s.js></script>";
    let plain = "<title>Example Domain</title><div><p>This domain is for use in documentation examples.</p></div>\
                 <script type=application/ld+json>{\"@type\": \"WebSite\"}</script>";
    let opens = "This page was read over HTTP: its markup holds no links or controls, but its scripts did not run. A \
                 read takes in only this markup; a wait, scroll or other step on this page opens it in a browser, \
                 which runs them.";
    let cannot = "This page was read over HTTP: its markup holds no links or controls, but its scripts did not run. \
                  This run cannot open a browser, so no step on this page runs them: a read takes in only this markup.";
    let nowhere = "This page offers no links or controls: nothing on it leads anywhere else.";
    for (markup, fetch, stated) in [(script_built, false, opens), (script_built, true, cannot), (plain, false, nowhere)]
    {
        let (driver, asked) = Shown::new("wait=2", 1);
        let options = task(|o, h| {
            o.fetch = fetch;
            o.task = "Open https://docs.example.test/, follow the link on that page, and report the main heading of \
                      the page the link opens."
                .into();
            o.start = Some("https://docs.example.test/".into());
            h.http = Some(Http::new(Arc::new(
                move |request: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
                    let response = if request.url.contains("/json/version") {
                        Response::new(404, Vec::new())
                    } else {
                        Response::new(200, markup).with_header("content-type", "text/html")
                    };
                    Box::pin(async move { Ok(response) })
                },
            )));
            h.jev = Some(Arc::new(fastbrowse_agent::scripted::ScriptedJev::new(Vec::new())));
            h.llm = Some(driver);
            h.open_browser = Some(no_browser());
        });
        let result = run(options).await.unwrap();
        let facts = asked.lock().unwrap()[0].clone();
        for fact in [opens, cannot, nowhere] {
            assert_eq!(facts.contains(fact), fact == stated, "{facts}");
        }
        // The wait hands the run to the browser, which here fails to start; a fetch-only run ends where it is.
        if stated == opens {
            assert_eq!(result.status, Status::Error, "{:?}", result.error);
            assert!(
                result.error.as_deref().is_some_and(|error| error.contains("no browser here")),
                "{:?}",
                result.error
            );
        } else if fetch {
            assert_eq!(result.status, Status::Blocked, "{:?}", result.error);
        }
    }
}

/// The chat driver waiting on a fetched page, as one shown a bot check does: a page fetched over HTTP has not run the
/// scripts that clear it.
fn waits() -> Arc<dyn LLMClient> {
    Arc::new(fastbrowse_agent::scripted::ScriptedDriver::new(fastbrowse_agent::scripted::steps("wait").unwrap()))
}

#[tokio::test]
async fn test_a_run_takes_a_fetched_bot_check_the_driver_waits_on_to_the_browser() {
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
        h.jev = Some(Arc::new(fastbrowse_agent::scripted::ScriptedJev::new(Vec::new())));
        h.llm = Some(waits());
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
        h.jev = Some(priced(Vec::new()));
        h.llm = Some(waits());
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

#[test]
fn an_empty_task_is_refused_before_provider_or_browser_setup() {
    for text in ["", " ", "\n\t"] {
        let (task, _) = task(|task, _| task.task = text.into());
        let result = fastbrowse_run::check(&task, false);
        assert!(
            matches!(result, Err(RunError::Invalid(ref message)) if message == "task must not be empty"),
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn cloud_browser_options_do_not_fetch_from_the_hosts_network_first() {
    for option in ["proxy_country", "cloud_extensions"] {
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let options = task(|o, h| {
            o.start = Some("https://example.test/document".into());
            if option == "proxy_country" {
                o.proxy_country = Some("de".into());
            } else {
                o.cloud_extensions = vec!["extension-id".into()];
            }
            h.http =
                Some(Http::new(Arc::new(move |_: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
                    count.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async {
                        Ok(Response::new(200, "<p>The documented value is 34.</p>")
                            .with_header("content-type", "text/html"))
                    })
                })));
        });
        let result = run(options).await;
        assert_eq!(requests.load(Ordering::SeqCst), 0, "{option}: must not read through the host");
        assert!(
            matches!(result, Err(RunError::Configuration(ref error)) if error.contains("BROWSER_USE_API_KEY")),
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn an_explicit_browser_run_opens_the_supplied_browser_before_reading() {
    let requests = Arc::new(AtomicUsize::new(0));
    let fetched = requests.clone();
    let opens = Arc::new(AtomicUsize::new(0));
    let opened = opens.clone();
    let options = task(|o, h| {
        o.browser = true;
        o.start = Some("https://docs.example.test/value".into());
        h.http = Some(Http::new(Arc::new(move |_: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
            fetched.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(Response::new(200, "<p>The value is 34.</p>").with_header("content-type", "text/html"))
            })
        })));
        h.open_browser = Some(Arc::new(move || {
            opened.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err("supplied browser was requested".into()) })
        }));
    });
    let result = run(options).await.unwrap();
    assert_eq!(opens.load(Ordering::SeqCst), 1, "the browser option must reach the supplied callback");
    assert_eq!(requests.load(Ordering::SeqCst), 0, "no host-network fetch may precede the requested browser");
    assert_eq!(result.status, Status::Error);
    assert!(result.error.as_deref().unwrap().contains("supplied browser was requested"));
}

#[tokio::test]
async fn settings_local_profile_or_headed_route_to_browser_before_http() {
    for variable in ["FASTBROWSE_PROFILE", "FASTBROWSE_HEADED"] {
        let value = if variable.ends_with("PROFILE") { "/tmp/kept-profile" } else { "true" };
        let (mut task, mut host) = task(|_, _| {});
        host.settings = Some(Settings::from_sources(&[(variable, value)], None).unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        host.http =
            Some(Http::new(Arc::new(move |_: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
                seen.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    Ok(Response::new(200, b"<p>fetched</p>".to_vec()).with_header("content-type", "text/html"))
                })
            })));
        let opens = Arc::new(AtomicUsize::new(0));
        let opened = opens.clone();
        host.open_browser = Some(Arc::new(move || {
            opened.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err("browser callback reached".into()) })
        }));
        let result = run_task(task, host).await.unwrap();
        assert_eq!(opens.load(Ordering::SeqCst), 1, "{variable}");
        assert_eq!(requests.load(Ordering::SeqCst), 0, "{variable}");
        assert!(result.error.as_deref().unwrap_or_default().contains("browser callback reached"), "{result:?}");

        task = RunTask {
            fetch: true,
            start: Some("https://docs.example.test/value".into()),
            ..RunTask { task: "read the page".into(), ..RunTask::default() }
        };
        let fetch_requests = Arc::new(AtomicUsize::new(0));
        let fetch_seen = fetch_requests.clone();
        let mut fetch_host = Host {
            jev: Some(Arc::new(Silent)),
            llm: Some(Arc::new(Silent)),
            settings: Some(Settings::from_sources(&[(variable, value)], None).unwrap()),
            http: Some(Http::new(Arc::new(
                move |_: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
                    fetch_seen.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async {
                        Ok(Response::new(200, b"<p>fetched</p>".to_vec()).with_header("content-type", "text/html"))
                    })
                },
            ))),
            ..Host::default()
        };
        fetch_host.open_browser = Some(Arc::new(|| Box::pin(async { Err("must not open".into()) })));
        let result = run_task(task, fetch_host).await.unwrap();
        assert_eq!(result.status, Status::Error, "{variable}: expected scripted clients to stop after fetch");
        assert_eq!(fetch_requests.load(Ordering::SeqCst), 1, "{variable}: explicit fetch must use HTTP");
    }
}

#[tokio::test]
async fn a_missing_browser_key_after_a_paid_fetch_preserves_its_receipt() {
    let http = Http::new(Arc::new(|_: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
        Box::pin(async {
            Ok(Response::new(200, "<html><body>Checking access</body></html>").with_header("content-type", "text/html"))
        })
    }));
    let options = task(|o, h| {
        o.start = Some("https://docs.example.test/value".into());
        h.http = Some(http);
        h.jev = Some(priced(Vec::new()));
        h.llm = Some(waits());
    });
    let error = run(options).await.unwrap_err();
    assert!(error.to_string().contains("BROWSER_USE_API_KEY"));
    let receipt = error.receipt().expect("the paid fetch must survive an escalation configuration error");
    assert!(receipt.cost.known_dollars() > 0.0);
    assert!(matches!(error.cause(), RunError::Configuration(_)));
    assert!(receipt.steps.is_empty(), "the bot check was read before the first action");
}

struct ScreenshotOutput(fastbrowse_agent::scripted::ScriptedJev);

impl DecisionClient for ScreenshotOutput {
    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        self.0.profile()
    }
    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async move {
            let mut result = self.0.evaluate(state, questions).await?;
            if questions
                .get("pick")
                .is_some_and(|q| matches!(q, Question::Choice(q) if q.criteria.contains_key("text_image")))
                && let Some(Answer::Choice(answer)) = result.answers.get_mut("pick")
            {
                answer.choice = "image".into();
            }
            Ok(result)
        })
    }
}

#[tokio::test]
async fn a_requested_screenshot_escalates_from_http_to_the_supplied_browser() {
    let requests = Arc::new(AtomicUsize::new(0));
    let fetched = requests.clone();
    let opens = Arc::new(AtomicUsize::new(0));
    let opened = opens.clone();
    let options = task(|o, h| {
        o.task = "Take a screenshot of the page and return it.".into();
        o.start = Some("https://docs.example.test/value".into());
        h.jev = Some(Arc::new(ScreenshotOutput(fastbrowse_agent::scripted::ScriptedJev::new(Vec::new()))));
        h.llm = Some(Arc::new(fastbrowse_agent::scripted::ScriptedLLM));
        h.http = Some(Http::new(Arc::new(move |_: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
            fetched.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(Response::new(200, "<p>The page is ready.</p>").with_header("content-type", "text/html"))
            })
        })));
        h.open_browser = Some(Arc::new(move || {
            opened.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err("screenshot browser was requested".into()) })
        }));
    });
    let result = run(options).await.unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(opens.load(Ordering::SeqCst), 1, "an HTTP screenshot refusal must trigger browser escalation");
    assert_eq!(result.status, Status::Error);
    assert!(result.error.as_deref().unwrap().contains("screenshot browser was requested"));
}

#[tokio::test]
async fn an_extension_run_with_no_browser_connected_ends_before_jev_is_asked() {
    let hosts = std::env::temp_dir().join(format!("fastbrowse-no-hosts-{}", std::process::id()));
    std::fs::create_dir_all(&hosts).unwrap();
    // SAFETY: no other test in this binary reads FASTBROWSE_EXTENSION_DIR.
    unsafe { std::env::set_var("FASTBROWSE_EXTENSION_DIR", &hosts) };
    let asked = Arc::new(Mutex::new(Vec::new()));
    let options = task(|o, h| {
        o.extension = true;
        h.jev = Some(Arc::new(RecordingDecision(asked.clone())));
    });
    let result = run(options).await;
    let _ = std::fs::remove_dir(&hosts);
    let message = format!("{result:?}");
    assert!(message.contains("fastbrowse extension install"), "{message}");
    assert!(asked.lock().unwrap().is_empty(), "Jev was asked before the run found no browser");
}

struct SearchEntry;

impl LLMClient for SearchEntry {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        _: &'a [Message],
        schema: &'a dyn ResponseSchema,
        _: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        Box::pin(async move {
            let value = match purpose {
                LLMPurpose::Start => r#"{"url":null,"queries":["returns policy"]}"#,
                LLMPurpose::KnownPages => r#"{"urls":[]}"#,
                LLMPurpose::Dates => r#"{"dates":[]}"#,
                _ => return Err(LLMError::failed(fastbrowse_core::llm::FaultClass::Client, "stop after search entry")),
            };
            Ok(Generation {
                data: schema.validate_json(value).map_err(LLMError::schema)?,
                cost: CostLine::new(CostComponent::Llm, CostBasis::Metered, Some(0.0)),
            })
        })
    }
}

impl DecisionClient for SearchEntry {
    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        fastbrowse_core::decision::DecisionProfile::jev()
    }
    fn evaluate<'a>(
        &'a self,
        _: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async move {
            if !questions.contains_key("entry") {
                return Err(DecisionError::failed("stop after search entry"));
            }
            Ok(Evaluation {
                model: "jev".into(),
                answers: IndexMap::from([(
                    "entry".into(),
                    Answer::Choice(fastbrowse_core::decision::ChoiceAnswer {
                        kind: Default::default(),
                        choice: "search/0".into(),
                        probabilities: IndexMap::from([("search/0".into(), 1.0)]),
                        confidence: 1.0,
                    }),
                )]),
                input_tokens: 0,
                cost: CostLine::new(CostComponent::Jev, CostBasis::Metered, Some(0.0)),
                requests: 1,
            })
        })
    }
}

#[tokio::test]
async fn a_search_entry_needs_no_browser_and_records_its_provider_receipt() {
    let requests = Arc::new(AtomicUsize::new(0));
    let sent = requests.clone();
    let result = run(task(move |_, host| {
        host.settings = Some(Settings::from_sources(&[("EXA_API_KEY", "key")], None).unwrap());
        host.llm = Some(Arc::new(SearchEntry));
        host.jev = Some(Arc::new(SearchEntry));
        host.http = Some(Http::new(Arc::new(
            move |request: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
                assert_eq!(request.url, "https://api.exa.ai/search");
                sent.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    Ok::<_, TransportError>(Response::json(
                        200,
                        &serde_json::json!({
                            "results":[{"url":"https://shop.example/policy","title":"Policy"}],
                            "costDollars":{"total":0.007}
                        }),
                    ))
                })
            },
        )));
    }))
    .await
    .unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(result.browser.is_none());
    assert!(
        result.cost.lines.iter().any(|cost| cost.component == CostComponent::Search && cost.dollars == Some(0.007))
    );
}

#[tokio::test]
async fn a_search_is_refused_before_dispatch_when_its_price_exceeds_the_budget() {
    let result = run(task(|options, host| {
        options.limits = Some(fastbrowse_core::models::Limits { max_dollars: Some(0.006), ..Default::default() });
        host.settings = Some(Settings::from_sources(&[("EXA_API_KEY", "key")], None).unwrap());
        host.llm = Some(Arc::new(SearchEntry));
        host.jev = Some(Arc::new(SearchEntry));
        host.http = Some(Http::new(Arc::new(|_: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
            panic!("over-budget search dispatched")
        })));
    }))
    .await
    .unwrap();
    assert_eq!(result.status, Status::BudgetExceeded);
    assert!(result.cost.lines.iter().all(|cost| cost.component != CostComponent::Search));
}

#[tokio::test]
async fn cancelling_an_in_flight_search_preserves_its_bounded_charge() {
    let cancel = Cancel::default();
    let stop = cancel.clone();
    let started = Arc::new(tokio::sync::Notify::new());
    let sent = started.clone();
    let options = task(move |_, host| {
        host.cancel = stop;
        host.settings = Some(Settings::from_sources(&[("EXA_API_KEY", "key")], None).unwrap());
        host.llm = Some(Arc::new(SearchEntry));
        host.jev = Some(Arc::new(SearchEntry));
        host.http =
            Some(Http::new(Arc::new(move |_: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
                sent.notify_one();
                Box::pin(std::future::pending())
            })));
    });
    let running = tokio::spawn(run(options));
    started.notified().await;
    cancel.trip();
    let RunError::Cancelled(receipt) = running.await.unwrap().unwrap_err() else { panic!("not cancelled") };
    assert!(receipt.cost.lines.iter().any(|cost| cost.component == CostComponent::Search
        && cost.basis == CostBasis::Bounded
        && cost.dollars == Some(0.007)));
}

/// Jev as scripted for the steps, except that it judges the authorization questions as a Jev that was told what the
/// save replaces would: a save stated to overwrite a value is not what the task asked for. The questions it was asked
/// are kept, so the test can read what the run said about the save.
struct SaveJudge {
    script: fastbrowse_agent::scripted::ScriptedJev,
    asked: Arc<Mutex<Vec<String>>>,
}

impl DecisionClient for SaveJudge {
    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        self.script.profile()
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        let Some(Question::Noul(requested)) = questions.get("requested") else {
            return self.script.evaluate(state, questions);
        };
        self.asked.lock().unwrap().extend(questions.values().filter_map(|question| match question {
            Question::Noul(question) => Some(question.instructions.clone()),
            _ => None,
        }));
        let replaces = requested.instructions.contains("the run replaced");
        let answers = questions
            .keys()
            .map(|key| {
                let probability = match key.as_str() {
                    "irreversible" => 0.4,
                    "lasting" => 0.9,
                    "requested" if replaces => 0.1,
                    "requested" => 0.8,
                    _ => 0.0,
                };
                (
                    key.clone(),
                    Answer::Noul(fastbrowse_core::decision::NoulAnswer { kind: Default::default(), probability }),
                )
            })
            .collect();
        Box::pin(async move {
            Ok(Evaluation {
                model: "jev".into(),
                answers,
                input_tokens: 0,
                cost: CostLine::new(CostComponent::Jev, CostBasis::Estimated, Some(0.0)),
                requests: 1,
            })
        })
    }
}

/// A gallery whose only offered edit belongs to the second card, which the task never names: the run opened that
/// edit and filled its Title, and the save over that card's caption was the one the task did not ask for (#1446). The
/// questions about the save state what it replaces and which edit opened it, so the save is not taken.
#[tokio::test]
async fn test_a_save_over_another_cards_caption_is_not_taken() {
    if fastbrowse_browser::adapters::local_chrome::find_chrome(None).is_none() {
        return;
    }
    let page = r#"<body><h1>Gallery</h1>
<p>First card: Sunrise over the lake</p>
<section><p>Second card: Harbour at night</p><button onclick="document.getElementById('dialog').hidden=false">Edit</button></section>
<form id="dialog" hidden onsubmit="fetch('/saved');return false">
<label>Title <input name="title" value="Harbour at night"></label><button type="submit">Apply</button></form></body>"#;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let start = format!("http://127.0.0.1:{}/", listener.local_addr().unwrap().port());
    let saves = Arc::new(AtomicUsize::new(0));
    let counted = saves.clone();
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = [0u8; 4096];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            if String::from_utf8_lossy(&buffer[..read]).starts_with("GET /saved") {
                counted.fetch_add(1, Ordering::SeqCst);
            }
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
                page.len()
            );
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, reply.as_bytes()).await;
        }
    });
    let asked = Arc::new(Mutex::new(Vec::new()));
    // The first click is on the fetched page, whose Edit the browser has to be opened for, before the dialog it opens.
    let script =
        fastbrowse_agent::scripted::steps("click Edit; click Edit; fill Title = Retitled card; click Apply").unwrap();
    let options = task(|o, h| {
        o.start = Some(start.clone());
        o.task = "Retitle the first card to Retitled card, and change no other card.".into();
        o.local = true;
        o.launch = true;
        o.authorization = Some(Authorization { irreversible_actions: true });
        h.jev = Some(Arc::new(SaveJudge {
            script: fastbrowse_agent::scripted::ScriptedJev::new(script.clone()),
            asked: asked.clone(),
        }));
        h.llm = Some(Arc::new(fastbrowse_agent::scripted::ScriptedDriver::new(script)));
    });
    let result = run(options).await;
    server.abort();
    result.expect("the run returns a result");
    assert_eq!(saves.load(Ordering::SeqCst), 0, "the save went through");
    let asked = asked.lock().unwrap();
    let saving: Vec<_> = asked.iter().filter(|question| question.contains("labelled 'Apply'")).collect();
    assert_eq!(saving.len(), 4, "the save was not asked about in each question: {asked:?}");
    for question in saving {
        assert!(question.contains("The run opened this form by clicking 'Edit"), "{question}");
        assert!(
            question.contains("the run replaced 'Title', which held 'Harbour at night', with 'Retitled card'"),
            "{question}"
        );
    }
}

/// Every request to a client, as the client was handed it, around the client that answers it, counting the images the
/// request carried. Jev's profile says it sees images, so a run that withheld none would send them.
struct Recorded<C: ?Sized>(Arc<C>, Arc<Mutex<Vec<String>>>, Arc<AtomicUsize>);

impl DecisionClient for Recorded<dyn DecisionClient> {
    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        fastbrowse_core::decision::DecisionProfile { images: true, ..self.0.profile() }
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        self.1.lock().unwrap().push(format!("{state}{}", serde_json::to_string(questions).unwrap()));
        self.0.evaluate(state, questions)
    }

    fn evaluate_images<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
        images: &'a fastbrowse_core::decision::Images,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        self.1.lock().unwrap().push(format!("{state}{}", serde_json::to_string(questions).unwrap()));
        self.2.fetch_add(images.len(), Ordering::SeqCst);
        self.0.evaluate_images(state, questions, images)
    }
}

impl LLMClient for Recorded<dyn LLMClient> {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        self.1.lock().unwrap().extend(messages.iter().map(|message| message.content.clone()));
        // The response schema is sent with the request, and its keys are written by code, never from a caller's name.
        self.1.lock().unwrap().push(format!("{}{}", schema.name(), schema.json_schema()));
        self.2.fetch_add(messages.iter().map(|message| message.images.len()).sum(), Ordering::SeqCst);
        self.0.generate_value(purpose, messages, schema, ledger)
    }
}

/// What a run's clients and callbacks were handed, gathered across every run given its host.
#[derive(Default, Clone)]
struct Handed {
    asked: Arc<Mutex<Vec<String>>>,
    written: Arc<Mutex<Vec<String>>>,
    images: Arc<AtomicUsize>,
    frames: Arc<AtomicUsize>,
    events: Arc<Mutex<Vec<String>>>,
    checked: Arc<Mutex<Vec<String>>>,
}

impl Handed {
    /// A host whose driver follows `script` and whose Jev and LLM see images, with step frames asking for a PNG with
    /// every step event and a final frame on the result.
    fn host(&self, script: &str, trace: &fastbrowse_core::telemetry::Trace) -> Host {
        let jev: Arc<dyn DecisionClient> =
            Arc::new(fastbrowse_agent::scripted::ScriptedJev::new(fastbrowse_agent::scripted::steps(script).unwrap()));
        // The driver LLM takes the steps; Jev answers the questions code still gates on.
        let llm: Arc<dyn LLMClient> = Arc::new(fastbrowse_agent::scripted::ScriptedDriver::new(
            fastbrowse_agent::scripted::steps(script).unwrap(),
        ));
        let (frames, events, checked) = (self.frames.clone(), self.events.clone(), self.checked.clone());
        Host {
            jev: Some(Arc::new(Recorded(jev, self.asked.clone(), self.images.clone()))),
            llm: Some(Arc::new(Recorded(llm, self.written.clone(), self.images.clone()))),
            config: Some(fastbrowse_core::config::Config { step_frames: true, ..Default::default() }),
            settings: Some(Settings::from_sources::<&str, &str>(&[], None).unwrap()),
            http: Some(Http::hyper().with_trace(trace.clone())),
            on_frame: Some(Arc::new(move |_: Vec<u8>| {
                frames.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {})
            })),
            on_event: Some(Arc::new(move |event: Event| {
                events.lock().unwrap().push(serde_json::to_string(&event).unwrap());
                Box::pin(async {})
            })),
            until: Some(Arc::new(move |url: String| {
                checked.lock().unwrap().push(url);
                Box::pin(async { true })
            })),
            ..Host::default()
        }
    }
}

/// Whether a serialised event or result carries an image anywhere in it: a step event's `frame`, or any base64 PNG.
fn carries_image(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.iter().any(|(key, value)| (key == "frame" && !value.is_null()) || carries_image(value))
        }
        Value::Array(items) => items.iter().any(carries_image),
        Value::String(text) => text.starts_with("iVBORw0KGgo") || text.starts_with("/9j/"),
        _ => false,
    }
}

/// A run given credentials in its start (with a tab the browser strips), its task (a
/// password-less token), its JSON context (an escaped `@`), an input's name (a quote, a tab and a newline) and value (a
/// one-letter password) and a redirect, on a page that echoes each of them plain, percent-encoded and as the
/// `Authorization: Basic` the browser sent, with and without padding, shows none of them to any model, event, trace
/// record, result, `until` check, recording, saved recipe, refused recipe or Debug print, and sends no image of the page to Jev, the
/// LLM, a step event, a frame or the result, while the browser is given every address whole (#1381). The run's only
/// stderr writes (`fastbrowse_browser::warn` and the recipe warning) carry no page text or address.
#[tokio::test]
async fn test_no_credential_of_a_run_reaches_a_model_its_events_trace_result_or_frames() {
    use fastbrowse_core::py::base64;
    // A silent pass without Chrome would look like a clean run; CI's rust job has Chrome, so this fails loudly there
    // and anywhere else Chrome is missing.
    assert!(
        fastbrowse_browser::adapters::local_chrome::find_chrome(None).is_some(),
        "test_no_credential_of_a_run_reaches_a_model_its_events_trace_result_or_frames needs Chrome on PATH"
    );
    // Each password holds characters a prose cut once ended an authority at, or a JSON or Debug print escapes.
    let (start_pw, redirect_pw, task_pw) = ("St@rt 9'pw", "R3dir<\"x", "T4sk(pw);");
    let (token, context_pw) = ("Tok3n9pw", "J5on9pw");
    // The one-letter password is a letter no path of the run holds: a path a credential's letters spell is refused.
    let (name_pw, value_pw, canvas_pw) = ("Na\"m\te\n9pw", "Z", "C4nvas9pw");
    let encode =
        |text: &str| percent_encoding::utf8_percent_encode(text, percent_encoding::NON_ALPHANUMERIC).to_string();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let reports = Arc::new(Mutex::new(Vec::<String>::new()));
    let reported = reports.clone();
    let echoes = [start_pw, redirect_pw, task_pw, context_pw]
        .map(|pw| format!("{pw} {} {}", encode(pw), base64::encode(format!("ops:{pw}").as_bytes())))
        .join(" ")
        + &format!(" {token} {}", base64::encode(format!("{token}:").as_bytes()))
        // A browser or a server may print a Basic credential without its padding.
        + &format!(" Authorization: Basic {}", base64::encode(format!("ops:{value_pw}").as_bytes()).trim_end_matches('='));
    // The address the run ends on carries the token in its query, as an `until` check would be shown it raw.
    let redirect = format!("http://ops:{}@127.0.0.1:{port}/orders?ref={token}", encode(redirect_pw));
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = vec![0u8; 16384];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
            let authorization = request
                .lines()
                .find_map(|line| line.strip_prefix("Authorization: Basic "))
                .map(str::trim)
                .map(str::to_string);
            reported.lock().unwrap().extend(authorization.clone());
            let reply = if path.starts_with("/canvas") {
                // The address is painted, never written as text, so a text check of the page finds nothing to hold.
                let page = "<html><body><h1>Canvas</h1><canvas id=c width=900 height=60></canvas><script>\
                            const c = document.getElementById('c').getContext('2d'); c.font = '24px sans-serif'; \
                            c.fillText(document.URL, 10, 40)</script></body></html>";
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
                    page.len()
                )
            } else if path == "/" {
                format!("HTTP/1.1 302 Found\r\nlocation: {redirect}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            } else if path.starts_with("/orders") && authorization.is_none() {
                "HTTP/1.1 401 Unauthorized\r\nwww-authenticate: Basic realm=\"orders\"\r\ncontent-length: 0\r\n\
                 connection: close\r\n\r\n"
                    .to_string()
            } else {
                let sent = authorization.unwrap_or_default();
                let page = format!(
                    "<html><head><title>Orders {sent}</title></head><body><h1>Orders</h1><p id=u></p><label>Code <input name=code></label>\
                     <p>Authorization: Basic {sent}</p><p>{echoes}</p><a href=\"{redirect}\">again</a>\
                     <script>document.getElementById('u').textContent = document.URL</script></body></html>"
                );
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
                    page.len()
                )
            };
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, reply.as_bytes()).await;
        }
    });
    let handed = Handed::default();
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let recording = scratch.join(format!("canary-{}.mp4", std::process::id()));
    let (replayed, saved) = (
        scratch.join(format!("canary-{}-replayed.json", std::process::id())),
        scratch.join(format!("canary-{}-saved.json", std::process::id())),
    );
    // Without ffmpeg the recording is refused before any frame, and the recording's content went unchecked in CI;
    // CI's rust job installs it, so this fails by name wherever it is missing.
    assert!(
        fastbrowse_browser::adapters::local_chrome::which("ffmpeg").is_some(),
        "test_no_credential_of_a_run_reaches_a_model_its_events_trace_result_or_frames needs ffmpeg on PATH"
    );
    let trace = fastbrowse_core::telemetry::Trace::collecting();
    // The driver reads the input's value masked, as the task writes it, and types it as it was shown; code types it as
    // the caller wrote it.
    let shown_value = fastbrowse_core::safety::mask_userinfo(&format!("http://ops:{value_pw}@127.0.0.1:{port}/vault"));
    let host = handed.host(&format!("fill Code = {shown_value}; enter Code"), &trace);
    // The browser strips the tab, so it opens with the password as written without it.
    let encoded = encode(start_pw);
    let (head, tail) = encoded.split_at(4);
    let instruction = format!(
        "Read the orders. The key page is http://ops:{task_pw}@127.0.0.1:{port}/key, and the repository \
         http://{token}@127.0.0.1:{port}/repo, neither needed now."
    );
    // A recipe's start is refused with a credential in it, by a message that does not repeat the address.
    std::fs::write(
        &replayed,
        serde_json::json!({
            "version": fastbrowse_core::recipe::RECIPE_VERSION,
            "task": instruction,
            "start": format!("http://ops:{head}\t{tail}@127.0.0.1:{port}/"),
            "steps": [],
        })
        .to_string(),
    )
    .unwrap();
    let task = RunTask {
        task: instruction.clone(),
        context: Some(format!(r#"{{"crm": "http://ops:{context_pw}@127.0.0.1:{port}/crm"}}"#)),
        inputs: Some(IndexMap::from([(
            format!("http://ops:{name_pw}@127.0.0.1:{port}/in"),
            format!("http://ops:{value_pw}@127.0.0.1:{port}/vault"),
        )])),
        start: Some(format!("http://ops:{head}\t{tail}@127.0.0.1:{port}/")),
        save_recipe: Some(saved.clone()),
        record: Some(recording.clone()),
        launch: true,
        browser: true,
        ..RunTask::default()
    };
    // A JSON caller's escaped `@` is decoded before the run reads it.
    let escaped =
        serde_json::to_string(&task).unwrap().replace(&format!("{context_pw}@"), &format!("{context_pw}\\u0040"));
    assert!(escaped.contains("\\u0040"));
    let task: RunTask = serde_json::from_str(&escaped).unwrap();
    let printed = format!("{task:?}");
    let connection = format!(
        "{:?}",
        fastbrowse_core::models::BrowserConnection::new(format!("ws://ops:{name_pw}@127.0.0.1:{port}/"), false)
    );
    let result = run_task(task, host).await.unwrap();
    // A page that paints its address on a canvas shows the credential in pixels alone, so only holding every image
    // keeps it out of a screenshot, a step frame or the result.
    let painted = run_task(
        RunTask {
            task: "Read the canvas.".into(),
            start: Some(format!("http://ops:{canvas_pw}@127.0.0.1:{port}/canvas")),
            launch: true,
            browser: true,
            ..RunTask::default()
        },
        handed.host("", &trace),
    )
    .await
    .unwrap();
    // A refusal names the DevTools address it refused, raised to an embedder before any redactor exists.
    let refused = run_task(
        RunTask {
            task: "Read it".into(),
            cdp_url: Some(format!("http://ops:{context_pw}@127.0.0.1:{port}/")),
            ..RunTask::default()
        },
        Host::default(),
    )
    .await
    .unwrap_err();
    let replay =
        run_task(RunTask { task: instruction, recipe: Some(replayed.clone()), ..RunTask::default() }, Host::default())
            .await
            .unwrap_err()
            .to_string();
    assert!(replay.contains("not a recipe"), "{replay}");
    let mut sinks = vec![
        ("recipe refusal", replay),
        ("result", serde_json::to_string(&result).unwrap()),
        ("painted result", serde_json::to_string(&painted).unwrap()),
        ("refusal", refused.to_string()),
        ("task debug", printed),
        ("connection debug", connection),
        ("saved recipe", std::fs::read_to_string(&saved).unwrap_or_default()),
    ];
    sinks.extend(handed.checked.lock().unwrap().iter().map(|url| ("until", url.clone())));
    sinks.extend(
        result
            .recordings
            .iter()
            .map(|path| ("recording", String::from_utf8_lossy(&std::fs::read(path).unwrap_or_default()).to_string())),
    );
    sinks.extend(handed.asked.lock().unwrap().iter().map(|text| ("jev", text.clone())));
    sinks.extend(handed.written.lock().unwrap().iter().map(|text| ("llm", text.clone())));
    sinks.extend(handed.events.lock().unwrap().iter().map(|text| ("event", text.clone())));
    sinks.extend(trace.events().iter().map(|record| ("trace", record.to_string())));
    assert!(sinks.iter().any(|(sink, text)| *sink == "llm" && text.contains("Orders")), "the page was not read");
    assert!(!handed.checked.lock().unwrap().is_empty(), "until was not asked");
    // Every frame of the run is held, so its recording has no clean frame to keep and is not written.
    assert!(result.recordings.is_empty() && !recording.exists(), "a recording was written");
    let written_start = format!("{head}\t{tail}");
    // A one-letter password leaves every key and control id whole, so the fill finds its field and both runs finish.
    assert_eq!((result.status, painted.status), (Status::Complete, Status::Complete), "{:?}", result.error);
    assert!(handed.events.lock().unwrap().iter().any(|event| event.contains("\"fill\"")), "the input was not typed");
    let mut leaks = Vec::new();
    for (user, pw) in [
        ("ops", start_pw),
        ("ops", redirect_pw),
        ("ops", task_pw),
        ("ops", context_pw),
        ("ops", name_pw),
        ("ops", value_pw),
        ("ops", canvas_pw),
        (token, ""),
    ] {
        let secret = if pw.is_empty() { user } else { pw };
        let basic = if pw.is_empty() { format!("{user}:") } else { format!("{user}:{pw}") };
        let padded = base64::encode(basic.as_bytes());
        let mut forms = vec![
            written_start.clone(),
            padded.clone(),
            padded.trim_end_matches('=').to_string(),
            padded.replace('+', "-").replace('/', "_"),
            format!("{user}:{pw}@"),
        ];
        // A one-letter password is in every text as a letter; its address and Basic forms are what identify it.
        if secret.chars().count() > 1 {
            forms.extend([secret.to_string(), encode(secret)]);
        }
        // JSON and Debug write a quote, a tab and a newline escaped, so each form is looked for as written and so.
        for form in forms
            .iter()
            .flat_map(|form| [form.clone(), serde_json::to_string(form).unwrap().trim_matches('"').to_string()])
        {
            leaks.extend(
                sinks
                    .iter()
                    .filter(|(_, text)| text.contains(&form))
                    .map(|(sink, _)| format!("{form:?} in the {sink}")),
            );
        }
    }
    if handed.frames.load(Ordering::SeqCst) != 0 {
        leaks.push("a live frame went out".into());
    }
    if handed.images.load(Ordering::SeqCst) != 0 {
        leaks.push(format!("{} images reached Jev or the LLM", handed.images.load(Ordering::SeqCst)));
    }
    if result.final_frame.is_some() || painted.final_frame.is_some() {
        leaks.push("the result carries a final frame".into());
    }
    let pictured =
        handed.events.lock().unwrap().iter().filter(|text| carries_image(&serde_json::from_str(text).unwrap())).count();
    if pictured != 0 {
        leaks.push(format!("{pictured} events carry an image"));
    }
    for path in [&recording, &replayed, &saved] {
        let _ = std::fs::remove_file(path);
    }
    assert!(leaks.is_empty(), "the run leaked: {leaks:#?}");
    // The browser itself was given the redirect's address whole and answered the 401 with its credentials.
    let sent = base64::encode(format!("ops:{redirect_pw}").as_bytes());
    assert!(reports.lock().unwrap().contains(&sent), "the browser was not given the redirect's credentials");
}

/// A token in a start address's query is not a credential by its structure, so a run learns it only when the caller
/// declares it as a secret. Declared, it is masked from the first request on, before anything types it: no model,
/// event, trace record, result or refusal carries it, and while the page shows it no image of the page goes out.
#[tokio::test]
async fn test_a_declared_secret_in_the_start_query_reaches_no_model_or_exit() {
    assert!(
        fastbrowse_browser::adapters::local_chrome::find_chrome(None).is_some(),
        "test_a_declared_secret_in_the_start_query_reaches_no_model_or_exit needs Chrome on PATH"
    );
    let token = "QueryTok9";
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        // The page writes its own address, query and all, as text.
        let page = "<html><head><title>Shop</title></head><body><h1>Shop</h1><p id=u></p>\
                    <script>document.getElementById('u').textContent = document.URL</script></body></html>";
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
                page.len()
            );
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, reply.as_bytes()).await;
        }
    });
    let origin = format!("http://127.0.0.1:{port}");
    let declared = || -> Arc<dyn fastbrowse_core::models::SecretResolver> {
        Arc::new(ScopedSecrets::per_secret([(
            "shop_token".to_string(),
            (SecretValue::Plain(token.to_string()), vec![origin.clone()]),
        )]))
    };
    let handed = Handed::default();
    let trace = fastbrowse_core::telemetry::Trace::collecting();
    let mut host = handed.host("", &trace);
    host.secrets = Some(declared());
    let result = run_task(
        RunTask {
            task: "Read the shop page.".into(),
            start: Some(format!("{origin}/?token={token}")),
            launch: true,
            browser: true,
            ..RunTask::default()
        },
        host,
    )
    .await
    .unwrap();
    // A refusal quoting the DevTools address it was handed, the token in its query.
    let refused = run_task(
        RunTask { task: "Read it".into(), cdp_url: Some(format!("{origin}/?token={token}")), ..RunTask::default() },
        Host { secrets: Some(declared()), ..Host::default() },
    )
    .await
    .unwrap_err();
    let mut sinks = vec![("result", serde_json::to_string(&result).unwrap()), ("refusal", refused.to_string())];
    sinks.extend(handed.asked.lock().unwrap().iter().map(|text| ("jev", text.clone())));
    sinks.extend(handed.written.lock().unwrap().iter().map(|text| ("llm", text.clone())));
    sinks.extend(handed.events.lock().unwrap().iter().map(|text| ("event", text.clone())));
    sinks.extend(handed.checked.lock().unwrap().iter().map(|url| ("until", url.clone())));
    sinks.extend(trace.events().iter().map(|record| ("trace", record.to_string())));
    assert!(sinks.iter().any(|(sink, text)| *sink == "llm" && text.contains("Shop")), "the page was not read");
    let mut leaks: Vec<String> = sinks
        .iter()
        .filter(|(_, text)| text.contains(token))
        .map(|(sink, _)| format!("the token in the {sink}"))
        .collect();
    if handed.frames.load(Ordering::SeqCst) != 0 {
        leaks.push("a live frame went out".into());
    }
    if handed.images.load(Ordering::SeqCst) != 0 {
        leaks.push(format!("{} images reached Jev or the LLM", handed.images.load(Ordering::SeqCst)));
    }
    if result.final_frame.is_some() {
        leaks.push("the result carries a final frame".into());
    }
    let pictured =
        handed.events.lock().unwrap().iter().filter(|text| carries_image(&serde_json::from_str(text).unwrap())).count();
    if pictured != 0 {
        leaks.push(format!("{pictured} events carry an image"));
    }
    assert!(leaks.is_empty(), "the run leaked: {leaks:#?}");
}
