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
    acted: Mutex<Vec<Action>>,
}

impl DatePage {
    fn controls(&self) -> Value {
        let mut controls = vec![
            json!({"id": "date", "frame_id": null, "frame_origin": "https://example.test", "role": "textbox", "label": "Date", "operations": ["click", "fill"],
                   "sensitive": false, "offscreen": false}),
            json!({"id": "inline-next", "frame_id": null, "role": "link", "label": "Next", "operations": ["click"],
                   "sensitive": false, "offscreen": false}),
        ];
        if self.open.load(std::sync::atomic::Ordering::SeqCst) {
            controls.push(json!({"id": "popup-next", "frame_id": null, "role": "link", "label": "Next",
                                 "operations": ["click"], "sensitive": false, "offscreen": false}));
        }
        Value::Array(controls)
    }
}

impl Page for DatePage {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        let open = self.open.load(std::sync::atomic::Ordering::SeqCst);
        let controls = self.controls();
        Box::pin(async move {
            Ok(serde_json::from_value(json!({
                "url": "https://example.test/", "title": "Pick a date",
                "page_key": if open { "open" } else { "closed" }, "document_key": "doc",
                "captured_at": "2026-11-18T00:00:00Z", "viewport_text": "Pick a date", "tabs": [],
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
        let opens =
            action.target_id.as_deref() == Some("date") && !self.open.swap(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            Ok(ActResult {
                outcome: StepOutcome::Executed,
                failure: None,
                page_changed: opens,
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
}

impl Driver {
    fn new(turns: impl IntoIterator<Item = Value>) -> Arc<Driver> {
        Arc::new(Driver { turns: Mutex::new(turns.into_iter().collect()), sent: Mutex::new(Vec::new()) })
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
        json!({"next": "", "actions": [{"tool": "click", "id": "date"}, {"tool": "click", "id": "inline-next"}]}),
        json!({"next": "", "actions": [{"tool": "click", "id": "popup-next"}]}),
        give_up(),
    ]);
    let result = run(page.clone(), driver.clone()).await;
    assert_eq!(result.status, Status::Stuck);
    let acted: Vec<_> = page.acted.lock().unwrap().iter().map(|action| action.target_id.clone()).collect();
    assert_eq!(
        acted,
        [Some("date".into()), Some("popup-next".into())],
        "the inline Next planned after the click never ran"
    );
    let second = driver.told(1);
    assert!(second.contains("planned action(s) were not run"), "{second}");
    assert!(second.contains(r#"*[popup-next] link "Next""#), "{second}");
    assert!(second.contains(r#"[inline-next] link "Next""#) && !second.contains("*[inline-next]"), "{second}");
}

#[tokio::test]
async fn test_typed_text_reaches_the_field_as_the_driver_wrote_it() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "type", "id": "date", "text": "12/01/2026"}]}),
        give_up(),
    ]);
    run(page.clone(), driver).await;
    let acted = page.acted.lock().unwrap();
    let fill =
        acted.iter().find(|action| action.operation == fastbrowse_core::models::Operation::Fill).expect("a fill");
    assert_eq!(fill.text.as_deref(), Some("12/01/2026"));
}

#[tokio::test]
async fn test_an_id_not_on_the_page_is_told_back_and_not_acted_on() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "click", "id": "popup-next"}]}), give_up()]);
    run(page.clone(), driver.clone()).await;
    assert!(page.acted.lock().unwrap().is_empty());
    assert!(driver.told(1).contains("there is no control [popup-next]"), "{}", driver.told(1));
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
    let clicks = (0..40).map(|_| json!({"next": "", "actions": [{"tool": "click", "id": "inline-next"}]}));
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
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "click", "id": "date"}]})]);
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
        Driver::new([json!({"next": "", "actions": [{"tool": "type_secret", "id": "date", "name": "pw"}]}), give_up()]);
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
