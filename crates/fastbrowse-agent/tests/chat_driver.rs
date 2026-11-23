//! The chat driver against a scripted page: a date field whose popup opens on click, beside an inline calendar.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedJev, ScriptedLLM};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{Answer, DecisionClient, DecisionError, DecisionProfile, Evaluation, Question};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{
    Artifact, CostBasis, CostComponent, CostLine, LLMPurpose, Limits, RunResult, Status, StepOutcome,
};
use fastbrowse_core::page::{ActResult, Action, Capture, Observation, Page, PageResult};
use fastbrowse_core::telemetry::Ledger;
use serde_json::{Value, json};

/// A page whose date field opens a popup with its own Next, and that records every action dispatched to it.
#[derive(Default)]
struct DatePage {
    open: std::sync::atomic::AtomicBool,
    /// Fetched over HTTP, so its scripts never ran.
    http: bool,
    /// The page's text below the viewport.
    offscreen: String,
    acted: Mutex<Vec<Action>>,
}

impl DatePage {
    fn controls(&self) -> Value {
        let mut controls = vec![
            json!({"id": "main:date", "frame_id": null, "frame_origin": "https://example.test", "role": "textbox", "label": "Date", "operations": ["click", "fill"],
                   "sensitive": false, "offscreen": false}),
            json!({"id": "main:inline-next", "frame_id": null, "role": "link", "label": "Next", "operations": ["click"],
                   "sensitive": false, "offscreen": false}),
            json!({"id": "main:note", "frame_id": null, "role": "textbox", "label": "Note", "operations": ["fill"],
                   "sensitive": false, "offscreen": false}),
        ];
        if self.open.load(std::sync::atomic::Ordering::SeqCst) {
            controls.push(json!({"id": "main:popup-next", "frame_id": null, "role": "link", "label": "Next",
                                 "operations": ["click"], "sensitive": false, "offscreen": false}));
        }
        Value::Array(controls)
    }
}

impl Page for DatePage {
    fn over_http(&self) -> bool {
        self.http
    }

    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        let open = self.open.load(std::sync::atomic::Ordering::SeqCst);
        let controls = self.controls();
        let offscreen = self.offscreen.clone();
        Box::pin(async move {
            Ok(serde_json::from_value(json!({
                "url": "https://example.test/", "title": "Pick a date",
                "page_key": if open { "open" } else { "closed" }, "document_key": "doc",
                "captured_at": "2026-11-18T00:00:00Z", "viewport_text": "Pick a date", "offscreen_text": offscreen, "tabs": [],
                "controls": controls
            }))
            .unwrap())
        })
    }

    fn document_changed<'a>(&'a self, _: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn navigate<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        let text = "Pick a date";
        Box::pin(async move {
            Ok(serde_json::from_value(json!({
                "url": "https://example.test/", "title": "Pick a date", "captured_at": "2026-11-18T00:00:00Z",
                "sha256": fastbrowse_core::py::sha256_hex(text.as_bytes()), "text": text,
                "blocks": [{"source_id": "p", "frame_id": null, "kind": "paragraph", "start": 0, "end": text.len()}]
            }))
            .unwrap())
        })
    }

    fn act<'a>(&'a self, action: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        self.acted.lock().unwrap().push(action.clone());
        let opens = action.target_id.as_deref() == Some("main:date")
            && !self.open.swap(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            Ok(ActResult {
                outcome: StepOutcome::Executed,
                failure: None,
                // A note taking text changes the page's text but no control on it.
                page_changed: opens || action.target_id.as_deref() == Some("main:note"),
                detail: None,
                moved: false,
                landed: None,
            })
        })
    }

    fn redrawn<'a>(&'a self, _: &'a Observation, _: f64, _: Option<&'a str>) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn withhold_frames(&self, _: bool) {}

    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        Box::pin(async { Ok("https://example.test/".into()) })
    }

    fn response_status(&self) -> BoxFuture<'_, PageResult<Option<i64>>> {
        Box::pin(async { Ok(Some(200)) })
    }

    fn lifecycle_mark(&self) -> u64 {
        0
    }

    fn wait_for_lifecycle(&self, _: u64, _: f64) -> BoxFuture<'_, bool> {
        Box::pin(async { false })
    }
}

/// A driver model that answers each action turn from a script, keeping the messages each turn was sent; every other
/// purpose goes to the scripted LLM.
struct Driver {
    turns: Mutex<VecDeque<Value>>,
    sent: Mutex<Vec<Vec<Message>>>,
    /// The context window the driver model's provider lists, in tokens.
    context: Option<i64>,
}

impl Driver {
    fn new(turns: impl IntoIterator<Item = Value>) -> Arc<Driver> {
        Arc::new(Driver { turns: Mutex::new(turns.into_iter().collect()), sent: Mutex::new(Vec::new()), context: None })
    }

    /// The last user message of turn `n`.
    fn told(&self, n: usize) -> String {
        self.sent.lock().unwrap()[n].last().unwrap().content.clone()
    }
}

impl LLMClient for Driver {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        if purpose != LLMPurpose::Action {
            return ScriptedLLM.generate_value(purpose, messages, schema, ledger);
        }
        self.sent.lock().unwrap().push(messages.to_vec());
        let turn = self.turns.lock().unwrap().pop_front().expect("a scripted turn");
        Box::pin(async move {
            let data = schema.validate_json(&turn.to_string()).map_err(LLMError::schema)?;
            Ok(Generation { data, cost: CostLine::new(CostComponent::Llm, CostBasis::Metered, Some(0.0)) })
        })
    }

    fn context_tokens(&self, _: LLMPurpose) -> BoxFuture<'_, Option<i64>> {
        Box::pin(async move { self.context })
    }
}

async fn run(page: Arc<DatePage>, driver: Arc<Driver>) -> RunResult {
    run_with(page, driver, Arc::new(ScriptedJev::new(Vec::new()))).await
}

async fn run_with(page: Arc<DatePage>, driver: Arc<Driver>, jev: Arc<dyn DecisionClient>) -> RunResult {
    let agent = Agent::new(page, jev, driver).with_chat(true);
    agent
        .run(
            "Open the date field and go to the next month.",
            RunOptions {
                start: Some("https://example.test/".into()),
                limits: Some(Limits { max_steps: Some(10), ..Default::default() }),
                ..Default::default()
            },
        )
        .await
        .expect("a result")
}

fn give_up() -> Value {
    json!({"next": "", "actions": [{"tool": "give_up", "status": "stuck", "reason": "test over"}]})
}

#[tokio::test]
async fn test_a_plan_stops_at_the_page_change_and_the_next_turn_sees_the_popups_controls_as_new() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "click", "id": "1"}, {"tool": "click", "id": "2"}]}),
        json!({"next": "", "actions": [{"tool": "click", "id": "*4"}]}),
        give_up(),
    ]);
    let result = run(page.clone(), driver.clone()).await;
    assert_eq!(result.status, Status::Stuck);
    let acted: Vec<_> = page.acted.lock().unwrap().iter().map(|action| action.target_id.clone()).collect();
    assert_eq!(
        acted,
        [Some("main:date".into()), Some("main:popup-next".into())],
        "the inline Next planned after the click never ran"
    );
    let second = driver.told(1);
    assert!(second.contains("planned action(s) were not run"), "{second}");
    // Controls are shown by number, not by the page's frame-qualified ids the model wrote back without their frame.
    assert!(second.contains(r#"*[4] link "Next""#), "{second}");
    assert!(second.contains(r#"[2] link "Next""#) && !second.contains("*[2]") && !second.contains("main:"), "{second}");
}

#[tokio::test]
async fn test_typed_text_reaches_the_field_as_the_driver_wrote_it() {
    let page = Arc::new(DatePage::default());
    let driver =
        Driver::new([json!({"next": "", "actions": [{"tool": "type", "id": "1", "text": "12/01/2026"}]}), give_up()]);
    run(page.clone(), driver).await;
    let acted = page.acted.lock().unwrap();
    let fill =
        acted.iter().find(|action| action.operation == fastbrowse_core::models::Operation::Fill).expect("a fill");
    assert_eq!(fill.text.as_deref(), Some("12/01/2026"));
}

#[tokio::test]
async fn test_an_id_not_on_the_page_is_told_back_and_not_acted_on() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "click", "id": "4"}]}), give_up()]);
    run(page.clone(), driver.clone()).await;
    assert!(page.acted.lock().unwrap().is_empty());
    assert!(driver.told(1).contains("there is no control [4]"), "{}", driver.told(1));
}

#[tokio::test]
async fn test_a_give_up_ends_the_run_as_its_status() {
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "give_up", "status": "needs_login", "reason": "a wall"}]}),
    ]);
    let result = run(Arc::new(DatePage::default()), driver).await;
    assert_eq!(result.status, Status::NeedsLogin);
}

#[tokio::test]
async fn test_actions_that_change_nothing_end_the_run_stuck() {
    let page = Arc::new(DatePage::default());
    let clicks = (0..40).map(|_| json!({"next": "", "actions": [{"tool": "click", "id": "2"}]}));
    let result = run(page, Driver::new(clicks)).await;
    assert_eq!(result.status, Status::Stuck, "{:?}", result.error);
}

#[tokio::test]
async fn test_the_first_turn_states_the_task_and_the_system_prompt_is_cached() {
    let driver = Driver::new([give_up()]);
    run(Arc::new(DatePage::default()), driver.clone()).await;
    let sent = driver.sent.lock().unwrap();
    assert!(sent[0][0].cache_through, "the fixed instructions mark the cached prefix");
    assert!(
        sent[0][1].content.contains("Task: ") && sent[0][1].content.contains("next month"),
        "{}",
        sent[0][1].content
    );
}

#[tokio::test]
async fn test_wait_reaches_the_page_as_a_wait() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "wait"}]}), give_up()]);
    run(page.clone(), driver.clone()).await;
    assert!(
        page.acted.lock().unwrap().iter().any(|action| action.operation == fastbrowse_core::models::Operation::Wait),
        "{:?}",
        page.acted.lock().unwrap()
    );
}

/// The scripted Jev, except that it answers the yes/no question `.1` with probability `.2`.
struct Answering(ScriptedJev, &'static str, f64);

impl DecisionClient for Answering {
    fn profile(&self) -> DecisionProfile {
        self.0.profile()
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a indexmap::IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async move {
            let mut evaluation = self.0.evaluate(state, questions).await?;
            if let Some(Answer::Noul(answer)) = evaluation.answers.get_mut(self.1) {
                answer.probability = self.2;
            }
            Ok(evaluation)
        })
    }
}

#[tokio::test]
async fn test_an_irreversible_click_the_run_is_not_authorized_for_stops_it_unacted() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "click", "id": "1"}]})]);
    let jev = Arc::new(Answering(ScriptedJev::new(Vec::new()), "irreversible", 1.0));
    let result = run_with(page.clone(), driver, jev).await;
    assert_eq!(result.status, Status::NeedsConfirmation, "{:?}", result.error);
    assert!(page.acted.lock().unwrap().is_empty(), "{:?}", page.acted.lock().unwrap());
}

#[tokio::test]
async fn test_a_done_jev_finds_unsupported_is_refused_and_told_back() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "done"}]}), give_up()]);
    let result =
        run_with(page, driver.clone(), Arc::new(Answering(ScriptedJev::new(Vec::new()), "coverage", 0.0))).await;
    assert_eq!(result.status, Status::Stuck);
    assert!(driver.told(1).contains("done was refused"), "{}", driver.told(1));
}

/// One secret, `pw`, for the test page's origin only.
struct Secret;

impl fastbrowse_core::models::SecretResolver for Secret {
    fn available(&self) -> Vec<fastbrowse_core::models::SecretRef> {
        vec![fastbrowse_core::models::SecretRef { name: "pw".into(), origins: vec!["https://example.test".into()] }]
    }

    fn resolve<'a>(&'a self, name: &'a str, origin: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move { (name == "pw" && origin == "https://example.test").then(|| "hunter2".to_string()) })
    }
}

#[tokio::test]
async fn test_a_secret_is_typed_by_name_and_its_value_never_reaches_the_driver() {
    let page = Arc::new(DatePage::default());
    let driver =
        Driver::new([json!({"next": "", "actions": [{"tool": "type_secret", "id": "1", "name": "pw"}]}), give_up()]);
    let agent = Agent::new(page.clone(), Arc::new(ScriptedJev::new(Vec::new())), driver.clone())
        .with_chat(true)
        .with_secrets(Some(Arc::new(Secret)));
    agent
        .run(
            "Sign in with the password.",
            RunOptions { start: Some("https://example.test/".into()), ..Default::default() },
        )
        .await
        .expect("a result");
    let acted = page.acted.lock().unwrap();
    let fill =
        acted.iter().find(|action| action.operation == fastbrowse_core::models::Operation::Fill).expect("a fill");
    assert_eq!((fill.text.as_deref(), fill.secret), (Some("hunter2"), true));
    let sent = driver.sent.lock().unwrap();
    assert!(sent[0].iter().any(|message| message.content.contains("by name: pw")), "{:?}", sent[0]);
    assert!(sent.iter().flatten().all(|message| !message.content.contains("hunter2")));
}

#[tokio::test]
async fn test_done_reads_the_page_in_front_of_it_and_ends_in_one_turn() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "done"}]})]);
    let result = run(page, driver.clone()).await;
    assert_eq!(result.status, Status::Complete, "{:?}", result.error);
    assert_eq!(driver.sent.lock().unwrap().len(), 1);
    assert!(
        result.steps.iter().any(|step| step.operation == fastbrowse_core::models::Operation::Read),
        "{:?}",
        result.steps
    );
}

#[tokio::test]
async fn test_a_plan_keeps_running_while_a_field_takes_text_and_no_control_appears_or_goes() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([
        json!({"next": "", "actions": [
            {"tool": "type", "id": "3", "text": "hello"}, {"tool": "click", "id": "2"}
        ]}),
        give_up(),
    ]);
    run(page.clone(), driver.clone()).await;
    let acted: Vec<_> = page.acted.lock().unwrap().iter().map(|action| action.target_id.clone()).collect();
    assert_eq!(acted, [Some("main:note".into()), Some("main:inline-next".into())]);
    assert_eq!(driver.sent.lock().unwrap().len(), 2, "both actions ran from one turn");
}

#[tokio::test]
async fn test_a_done_cites_its_quotes_and_each_found_on_the_page_is_evidence() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "done", "quotes": ["a date"]}]}), give_up()]);
    let result = run(page, driver.clone()).await;
    assert_eq!(result.status, Status::Complete, "{:?}", result.error);
    assert_eq!(driver.sent.lock().unwrap().len(), 1);
    let quotes: Vec<&str> = result.evidence.iter().map(|evidence| evidence.quote.as_str()).collect();
    assert_eq!(quotes, ["a date"]);
}

#[tokio::test]
async fn test_a_done_quoting_words_the_page_does_not_show_is_refused_and_told_back() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "done", "quotes": ["Pick a day"]}]}), give_up()]);
    let result = run(page, driver.clone()).await;
    assert_eq!(result.status, Status::Stuck);
    assert!(driver.told(1).contains(r#"does not show "Pick a day" word for word"#), "{}", driver.told(1));
}

#[tokio::test]
async fn test_a_fetched_page_the_driver_waits_on_or_gives_up_on_goes_to_a_browser() {
    for act in [json!({"tool": "wait"}), json!({"tool": "give_up", "status": "unavailable", "reason": "Loading..."})] {
        let page = Arc::new(DatePage { http: true, ..Default::default() });
        let driver = Driver::new([json!({"next": "", "actions": [act]})]);
        let result = run(page, driver).await;
        assert_ne!(result.status, Status::Unavailable, "{:?}", result.error);
        assert_eq!(result.failure_kind.as_deref(), Some("NeedsBrowser"), "{:?}", result);
    }
}

#[tokio::test]
async fn test_a_plan_made_on_a_fetched_page_is_told_it_never_ran_once_a_browser_opens() {
    let fetched = Arc::new(DatePage { http: true, ..Default::default() });
    let driver =
        Driver::new([json!({"next": "", "actions": [{"tool": "wait"}, {"tool": "click", "id": "1"}]}), give_up()]);
    let agent = Agent::new(fetched, Arc::new(ScriptedJev::new(Vec::new())), driver.clone()).with_chat(true);
    let options = || RunOptions {
        start: Some("https://example.test/".into()),
        limits: Some(Limits { max_steps: Some(10), ..Default::default() }),
        ..Default::default()
    };
    agent.run("Open the date field.", options()).await;
    let resume = agent.resume().expect("a browser handoff");
    let browser = Agent::new(Arc::new(DatePage::default()), Arc::new(ScriptedJev::new(Vec::new())), driver.clone())
        .with_chat(true);
    let result = browser.run("Open the date field.", RunOptions { resume: Some(resume), ..options() }).await.unwrap();
    assert_eq!(result.status, Status::Stuck);
    assert!(driver.told(1).contains("none of your last actions ran"), "{}", driver.told(1));
    assert!(!driver.told(1).contains("page changed"), "{}", driver.told(1));
}

#[tokio::test]
async fn test_the_driver_opens_an_address_it_wrote_itself() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "goto", "url": "https://example.test/item/42"}]}),
        give_up(),
    ]);
    let result = run(page, driver).await;
    assert_ne!(result.failure_kind.as_deref(), Some("Navigate/unsupplied-address"), "{:?}", result.error);
    assert_eq!(result.status, Status::Stuck, "{:?}", result.error);
    assert_eq!(
        result.steps.first().map(|step| step.operation),
        Some(fastbrowse_core::models::Operation::Navigate),
        "{:?}",
        result.steps
    );
}

/// The first request's messages as JSON bytes, with a page whose text below the viewport is `offscreen`.
async fn first_request_bytes(offscreen: &str) -> i64 {
    let page = Arc::new(DatePage { offscreen: offscreen.into(), ..DatePage::default() });
    let driver = Driver::new([give_up()]);
    run(page, driver.clone()).await;
    serde_json::to_vec(&driver.sent.lock().unwrap()[0]).unwrap().len() as i64
}

#[tokio::test]
async fn test_a_conversation_past_the_models_context_drops_the_pages_it_moved_on_from() {
    let offscreen = "comment ".repeat(4000);
    let first = first_request_bytes(&offscreen).await;
    let page = Arc::new(DatePage { offscreen: offscreen.clone(), ..DatePage::default() });
    let turns = [json!({"next": "", "actions": [{"tool": "click", "id": "1"}]}), give_up()];
    let driver = Arc::new(Driver { context: Some(first + 25_000), ..Arc::into_inner(Driver::new(turns)).unwrap() });
    run(page, driver.clone()).await;
    let sent = driver.sent.lock().unwrap();
    assert!(sent[0][1].content.contains(&offscreen), "the first page fits in full");
    let earlier = &sent[1][1].content;
    assert!(!earlier.contains("comment comment") && earlier.contains("is no longer shown"), "{earlier}");
    assert!(earlier.contains(r#"Page: "Pick a date" https://example.test/"#), "{earlier}");
    assert!(sent[1].last().unwrap().content.contains(&offscreen), "the page in front of the model stays whole");
}

#[tokio::test]
async fn test_a_page_longer_than_the_models_context_is_shown_without_its_text_outside_the_viewport() {
    let offscreen = "comment ".repeat(4000);
    let first = first_request_bytes(&offscreen).await;
    let page = Arc::new(DatePage { offscreen, ..DatePage::default() });
    let driver =
        Arc::new(Driver { context: Some(first - 20_000), ..Arc::into_inner(Driver::new([give_up()])).unwrap() });
    run(page, driver.clone()).await;
    let told = driver.told(0);
    assert!(!told.contains("comment comment") && told.contains("The text outside the viewport is left out"), "{told}");
    assert!(told.contains("Pick a date"), "{told}");
}
