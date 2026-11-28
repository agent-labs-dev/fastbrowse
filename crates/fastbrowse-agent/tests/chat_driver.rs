//! The chat driver against a scripted page: a date field whose popup opens on click, beside an inline calendar.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedJev, ScriptedLLM};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{Answer, DecisionClient, DecisionError, DecisionProfile, Edges, Evaluation, Question};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{
    Artifact, CostBasis, CostComponent, CostLine, LLMPurpose, Limits, RunResult, Status, StepOutcome,
};
use fastbrowse_core::page::{ActResult, Action, CaptchaSolve, Capture, Observation, Page, PageResult};
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde_json::{Value, json};

/// A page whose date field opens a popup with its own Next, and that records every action dispatched to it.
#[derive(Default)]
struct DatePage {
    open: std::sync::atomic::AtomicBool,
    /// Fetched over HTTP, so its scripts never ran.
    http: bool,
    /// The page's text below the viewport.
    offscreen: String,
    /// The snapshot gave no viewport text, so the page has no place in its text to window around and goes whole.
    unplaced: bool,
    /// How many "reply" links the page has below the viewport.
    replies: usize,
    /// The CAPTCHA vendor the browser's solver is working on, on this page.
    solving: Option<String>,
    /// A sign-in field in a frame from another origin.
    framed: bool,
    /// The page's text before its date field is opened, when it differs from after.
    before: Option<String>,
    /// The modal open over the page, which lists none of its own controls.
    modal: Option<String>,
    /// The note belongs to a customer record, and the inline Next shows the next customer in the same row.
    records: bool,
    next_record: std::sync::atomic::AtomicBool,
    /// A real PNG for a screenshot, in place of none.
    png: bool,
    /// Each control sits in the screenshot's lower right quarter, as `edges` measures it, in place of nowhere.
    boxed: bool,
    /// The text the Date field holds, shown as its value as a browser reads it.
    dated: Mutex<Option<String>>,
    acted: Mutex<Vec<Action>>,
}

impl DatePage {
    fn controls(&self) -> Value {
        let mut controls = vec![
            json!({"id": "main:date", "frame_id": null, "frame_origin": "https://example.test", "role": "textbox", "label": "Date", "operations": ["click", "fill"],
                   "sensitive": false, "offscreen": false}),
            json!({"id": "main:inline-next", "frame_id": null, "role": "link", "label": "Next", "operations": ["click"],
                   "sensitive": false, "offscreen": false}),
            json!({"id": "main:note", "frame_id": null, "frame_origin": "https://example.test", "role": "textbox", "label": "Note", "operations": ["fill", "drag"],
                   "sensitive": false, "offscreen": false}),
        ];
        if let Some(dated) = self.dated.lock().unwrap().clone() {
            controls[0]["value"] = json!(dated);
        }
        if self.records {
            let next = self.next_record.load(std::sync::atomic::Ordering::SeqCst);
            controls[2]["context"] = json!(if next { "Customer Bob" } else { "Customer Alice" });
        }
        if self.framed {
            controls.push(json!({"id": "auth:password", "frame_id": "auth", "frame_origin": "https://auth.test",
                                 "role": "textbox", "label": "Password", "operations": ["fill"], "sensitive": true,
                                 "offscreen": false}));
        }
        for reply in 0..self.replies {
            controls
                .push(json!({"id": format!("main:reply-{reply}"), "frame_id": null, "role": "link", "label": "reply",
                                 "operations": ["click"], "sensitive": false, "offscreen": true}));
        }
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

    fn captcha_solve(&self) -> Option<CaptchaSolve> {
        let vendor = self.solving.clone()?;
        Some(CaptchaSolve { url: "https://example.test/".into(), vendor, working: true })
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        let open = self.open.load(std::sync::atomic::Ordering::SeqCst);
        let controls = self.controls();
        let offscreen = self.offscreen.clone();
        let viewport = if self.unplaced { "" } else { "Pick a date" };
        let modal = self.modal.clone();
        Box::pin(async move {
            Ok(serde_json::from_value(json!({
                "modal": modal,
                "url": "https://example.test/", "title": "Pick a date",
                "page_key": if open { "open" } else { "closed" }, "document_key": "doc",
                "captured_at": "2026-11-18T00:00:00Z", "viewport_text": viewport, "offscreen_text": offscreen, "tabs": [],
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
        let text = match &self.before {
            Some(before) if !self.open.load(std::sync::atomic::Ordering::SeqCst) => before.clone(),
            _ => "Pick a date".to_string(),
        };
        // A capture reads the whole page, its text below the viewport too.
        let text = if self.offscreen.is_empty() { text } else { format!("{text}\n\n{}", self.offscreen) };
        Box::pin(async move {
            Ok(serde_json::from_value(json!({
                "url": "https://example.test/", "title": "Pick a date", "captured_at": "2026-11-18T00:00:00Z",
                "sha256": fastbrowse_core::py::sha256_hex(text.as_bytes()), "text": text,
                "blocks": [{"source_id": "p", "frame_id": null, "kind": "paragraph", "start": 0, "end": text.chars().count()}]
            }))
            .unwrap())
        })
    }

    fn act<'a>(&'a self, action: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        self.acted.lock().unwrap().push(action.clone());
        if action.target_id.as_deref() == Some("main:date")
            && let Some(text) = &action.text
        {
            *self.dated.lock().unwrap() = Some(text.clone());
        }
        let opens = action.target_id.as_deref() == Some("main:date")
            && !self.open.swap(true, std::sync::atomic::Ordering::SeqCst);
        let paged = self.records && action.target_id.as_deref() == Some("main:inline-next");
        if paged {
            self.next_record.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Box::pin(async move {
            Ok(ActResult {
                outcome: StepOutcome::Executed,
                failure: None,
                // A note taking text changes the page's text but no control on it.
                page_changed: opens || paged || action.target_id.as_deref() == Some("main:note"),
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
        let mut png = std::io::Cursor::new(Vec::new());
        if self.png {
            image::DynamicImage::new_rgba8(200, 100).write_to(&mut png, image::ImageFormat::Png).unwrap();
        }
        Box::pin(async move { Ok(png.into_inner()) })
    }

    fn edges<'a>(&'a self, ids: &'a [String]) -> BoxFuture<'a, PageResult<IndexMap<String, Edges>>> {
        let edges = Edges { left: 0.5, top: 0.5, right: 0.75, bottom: 0.75 };
        Box::pin(async move { Ok(ids.iter().filter(|_| self.boxed).map(|id| (id.clone(), edges)).collect()) })
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
    /// The messages each compose call was sent.
    composed: Mutex<Vec<Vec<Message>>>,
    /// The context window the driver model's provider lists, in tokens.
    context: Option<i64>,
    /// The answer each compose call returns, in place of the scripted LLM's.
    composition: Option<Value>,
}

impl Driver {
    fn new(turns: impl IntoIterator<Item = Value>) -> Arc<Driver> {
        Arc::new(Driver {
            turns: Mutex::new(turns.into_iter().collect()),
            sent: Mutex::new(Vec::new()),
            composed: Mutex::new(Vec::new()),
            context: None,
            composition: None,
        })
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
        if purpose == LLMPurpose::Compose {
            self.composed.lock().unwrap().push(messages.to_vec());
            if let Some(answer) = self.composition.clone() {
                return Box::pin(async move {
                    let data = schema.validate_json(&answer.to_string()).map_err(LLMError::schema)?;
                    Ok(Generation { data, cost: CostLine::new(CostComponent::Llm, CostBasis::Metered, Some(0.0)) })
                });
            }
        }
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
    run_task(page, driver, jev, "Open the date field and go to the next month.").await
}

async fn run_task(page: Arc<DatePage>, driver: Arc<Driver>, jev: Arc<dyn DecisionClient>, task: &str) -> RunResult {
    let agent = Agent::new(page, jev, driver);
    agent
        .run(
            task,
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
async fn test_step_choices_record_the_controls_the_driver_was_shown_and_the_one_it_acted_on() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "click", "id": "1"}]}), give_up()]);
    let agent = Agent::new(page, Arc::new(ScriptedJev::new(Vec::new())), driver)
        .with_config(fastbrowse_core::config::Config { step_choices: true, ..Default::default() });
    let result = agent
        .run(
            "Open the date field and go to the next month.",
            RunOptions { start: Some("https://example.test/".into()), ..Default::default() },
        )
        .await
        .expect("a result");
    let click = result.steps.iter().find(|step| step.target.is_some()).expect("the click's step");
    let chosen: Vec<&str> =
        click.choices.iter().filter(|choice| choice.chosen).map(|choice| choice.id.as_str()).collect();
    assert_eq!(chosen, ["main:date"]);
    assert!(click.choices.len() > 1, "every control shown that turn is recorded: {:?}", click.choices);
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
async fn test_a_blocked_give_up_names_the_bot_check() {
    // A caller routes a blocked run on `wall.kind`, and the driver's give_up left no wall.
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "give_up", "status": "blocked", "reason": "a CAPTCHA on every route"}]}),
    ]);
    let result = run(Arc::new(DatePage::default()), driver).await;
    assert_eq!(result.status, Status::Blocked);
    assert_eq!(result.wall.map(|wall| wall.kind), Some(fastbrowse_core::models::WallKind::BotCheck));
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
        sent[0][1].content.contains("<task>\n")
            && sent[0][1].content.contains("\n</task>\nToday, not part of the task: ")
            && sent[0][1].content.contains("next month"),
        "{}",
        sent[0][1].content
    );
}

/// A comment task named its text "the TEXT input" and ended needs_input on the box the input was for (#1112): the
/// task the driver is given states every input by name and value.
#[tokio::test]
async fn test_the_first_turn_states_every_input_by_name_and_value() {
    let driver = Driver::new([give_up()]);
    let agent = Agent::new(Arc::new(DatePage::default()), Arc::new(ScriptedJev::new(Vec::new())), driver.clone());
    let inputs = IndexMap::from([("TEXT".to_string(), "Version 2.4.1 is out.".to_string())]);
    agent
        .run(
            "Post one comment whose text is exactly the TEXT input.",
            RunOptions { start: Some("https://example.test/".into()), inputs, ..Default::default() },
        )
        .await
        .expect("a result");
    let sent = driver.sent.lock().unwrap();
    let first = &sent[0][1].content;
    assert!(
        first.contains("exactly the TEXT input.\nInputs the user gave, by name: TEXT = 'Version 2.4.1 is out.'"),
        "{first}"
    );
}

#[tokio::test]
async fn test_wait_reaches_the_page_as_a_wait() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "wait", "seconds": 5}]}), give_up()]);
    run(page.clone(), driver.clone()).await;
    assert!(
        page.acted.lock().unwrap().iter().any(|action| action.operation == fastbrowse_core::models::Operation::Wait),
        "{:?}",
        page.acted.lock().unwrap()
    );
}

/// The scripted Jev, except that it answers each named yes/no question in `.1` with its probability.
struct Answering(ScriptedJev, &'static [(&'static str, f64)]);

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
            for (key, probability) in self.1 {
                if let Some(Answer::Noul(answer)) = evaluation.answers.get_mut(*key) {
                    answer.probability = *probability;
                }
            }
            Ok(evaluation)
        })
    }
}

#[tokio::test]
async fn test_an_irreversible_click_the_run_is_not_authorized_for_stops_it_unacted() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "click", "id": "1"}]})]);
    let jev = Arc::new(Answering(ScriptedJev::new(Vec::new()), &[("irreversible", 1.0)]));
    let result = run_with(page.clone(), driver, jev).await;
    assert_eq!(result.status, Status::NeedsConfirmation, "{:?}", result.error);
    assert!(page.acted.lock().unwrap().is_empty(), "{:?}", page.acted.lock().unwrap());
}

/// Escape went to whatever held focus as an untargeted step, unasked, so a widget that discards a draft on Escape lost it
/// without the authorization gate.
#[tokio::test]
async fn test_an_irreversible_escape_the_run_is_not_authorized_for_stops_it_unacted() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "key", "id": "1", "key": "Escape"}]})]);
    let jev = Arc::new(Answering(ScriptedJev::new(Vec::new()), &[("irreversible", 1.0)]));
    let result = run_with(page.clone(), driver, jev).await;
    assert_eq!(result.status, Status::NeedsConfirmation, "{:?}", result.error);
    assert!(page.acted.lock().unwrap().is_empty(), "{:?}", page.acted.lock().unwrap());
}

#[tokio::test]
async fn test_a_done_jev_finds_unsupported_is_refused_and_told_back() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "done"}]}), give_up()]);
    let result =
        run_with(page, driver.clone(), Arc::new(Answering(ScriptedJev::new(Vec::new()), &[("coverage", 0.0)]))).await;
    assert_eq!(result.status, Status::Stuck);
    let told = driver.told(1);
    assert!(
        told.contains("done was refused: The composed answer was rejected")
            && !told.contains("from this page or any page shown to you before"),
        "{told}"
    );
}

/// The driver ended a flights search complete on the site's "Oops, something went wrong. Reload" panel: a done on a
/// page Jev reads as the site's failure is sent back to retry it, and a second ends the run as that failure.
#[tokio::test]
async fn test_a_done_on_a_page_reporting_the_site_failed_is_sent_back_and_then_ends_unavailable() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "done"}]}),
        json!({"next": "", "actions": [{"tool": "done"}]}),
    ]);
    let jev = Arc::new(Answering(ScriptedJev::new(Vec::new()), &[("site_failure", 1.0)]));
    let result = run_with(page, driver.clone(), jev).await;
    assert_eq!(result.status, Status::Unavailable, "{:?}", result.error);
    let told = driver.told(1);
    assert!(told.contains("reports that the site failed") && told.contains("goto(its address)"), "{told}");
}

#[tokio::test]
async fn test_a_stuck_run_does_not_complete_on_notes_from_a_page_reporting_the_site_failed() {
    let page = Arc::new(DatePage { offscreen: "No results returned.".into(), ..DatePage::default() });
    let turns = [
        json!({"next": "", "actions": [{"tool": "done", "answer": [
            {"text": "No results returned.", "quotes": ["Pick a date", "No results returned."]}
        ]}]}),
        give_up(),
    ];
    let composition = json!({"claims": [{"text": [{"prose": "No results returned."}], "evidence_ids": ["e0"]}]});
    let driver = Arc::new(Driver { composition: Some(composition), ..Arc::into_inner(Driver::new(turns)).unwrap() });
    let jev = Arc::new(Answering(ScriptedJev::new(Vec::new()), &[("site_failure", 1.0), ("claim0", 1.0)]));
    let result = run_task(page, driver.clone(), jev, "Report what the page says.").await;
    assert!(driver.told(1).contains("reports that the site failed"));
    assert_eq!(result.status, Status::Stuck, "{:?}", result.answer);
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

/// A plan is made on each action doing what it was meant to: after a click on a dialog row out of view failed, the
/// plan's next action pressed OK and submitted the dialog's default row (#1403).
#[tokio::test]
async fn test_a_plan_stops_after_an_action_that_changed_nothing() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "click", "id": "2"}, {"tool": "type", "id": "3", "text": "x"}]}),
        give_up(),
    ]);
    run(page.clone(), driver.clone()).await;
    let acted: Vec<_> = page.acted.lock().unwrap().iter().map(|action| action.target_id.clone()).collect();
    assert_eq!(acted, [Some("main:inline-next".into())], "the typing planned after it never ran");
    let second = driver.told(1);
    assert!(
        second.contains("1 planned action(s) were not run because") && second.contains("changed nothing"),
        "{second}"
    );
}

#[tokio::test]
async fn test_a_plan_stops_once_a_control_it_named_shows_another_record() {
    let page = Arc::new(DatePage { records: true, ..DatePage::default() });
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "click", "id": "2"}, {"tool": "type", "id": "3", "text": "Alice"}]}),
        give_up(),
    ]);
    run(page.clone(), driver.clone()).await;
    let acted: Vec<_> = page.acted.lock().unwrap().iter().map(|action| action.target_id.clone()).collect();
    assert_eq!(acted, [Some("main:inline-next".into())], "the note planned for Alice now belongs to Bob");
    assert!(driver.told(1).contains("Customer Bob"));
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

/// Progress and effect are two things: a password typed again into a form already submitted once returned the page to
/// a state seen before, which is no progress, and the Submit planned after it was dropped as if the typing had changed
/// nothing, costing the sign-in two turns.
#[tokio::test]
async fn test_a_plan_keeps_running_after_a_fill_that_changed_a_value_but_no_control() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "type", "id": "1", "text": "1 Dec"}]}),
        json!({"next": "", "actions": [
            {"tool": "type", "id": "1", "text": "2 Dec"}, {"tool": "click", "id": "2"}
        ]}),
        give_up(),
    ]);
    run(page.clone(), driver.clone()).await;
    let acted: Vec<_> = page.acted.lock().unwrap().iter().map(|action| action.target_id.clone()).collect();
    assert_eq!(acted, [Some("main:date".into()), Some("main:date".into()), Some("main:inline-next".into())]);
    assert!(!driver.told(2).contains("were not run"), "{}", driver.told(2));
}

/// A note names an action as the driver writes one, not in Rust's debug form.
#[tokio::test]
async fn test_a_dropped_plan_names_the_action_as_the_driver_wrote_it() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "click", "id": "2"}, {"tool": "type", "id": "3", "text": "x"}]}),
        give_up(),
    ]);
    run(page, driver.clone()).await;
    let told = driver.told(1);
    assert!(told.contains(r#"because {"tool":"click","id":"2","on":null} changed nothing"#), "{told}");
}

#[tokio::test]
async fn test_a_done_cites_its_quotes_and_each_found_on_the_page_is_evidence() {
    let page = Arc::new(DatePage::default());
    let driver =
        Driver::new([json!({"next": "", "actions": [done("The field asks for a date.", &["a date"])]}), give_up()]);
    let result = run_with(page, driver.clone(), supporting()).await;
    assert_eq!(result.status, Status::Complete, "{:?}", result.error);
    assert_eq!(driver.sent.lock().unwrap().len(), 1);
    let quotes: Vec<&str> = result.evidence.iter().map(|evidence| evidence.quote.as_str()).collect();
    assert_eq!(quotes, ["a date"]);
}

#[tokio::test]
async fn test_a_done_quoting_words_the_page_does_not_show_is_refused_and_told_back() {
    let page = Arc::new(DatePage::default());
    let driver =
        Driver::new([json!({"next": "", "actions": [done("The page says pick a day.", &["Pick a day"])]}), give_up()]);
    let result = run(page, driver.clone()).await;
    assert_eq!(result.status, Status::Stuck);
    assert!(
        driver.told(1).contains(r#"no page shown to you in this run shows "Pick a day" word for word"#),
        "{}",
        driver.told(1)
    );
}

#[tokio::test]
async fn test_a_fetched_page_the_driver_waits_on_or_gives_up_on_goes_to_a_browser() {
    for act in [
        json!({"tool": "wait", "seconds": 5}),
        json!({"tool": "give_up", "status": "unavailable", "reason": "Loading..."}),
    ] {
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
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "wait", "seconds": 5}, {"tool": "click", "id": "1"}]}),
        give_up(),
    ]);
    let agent = Agent::new(fetched, Arc::new(ScriptedJev::new(Vec::new())), driver.clone());
    let options = || RunOptions {
        start: Some("https://example.test/".into()),
        limits: Some(Limits { max_steps: Some(10), ..Default::default() }),
        ..Default::default()
    };
    let _ = agent.run("Open the date field.", options()).await;
    let resume = agent.resume().expect("a browser handoff");
    let browser = Agent::new(Arc::new(DatePage::default()), Arc::new(ScriptedJev::new(Vec::new())), driver.clone());
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

#[tokio::test]
async fn test_an_address_written_as_a_link_on_the_page_opens_against_its_url() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "goto", "url": "/item/42"}]}), give_up()]);
    let result = run(page.clone(), driver).await;
    assert_eq!(result.status, Status::Stuck, "{:?}", result.error);
    let acted = page.acted.lock().unwrap();
    assert!(acted.iter().any(|action| action.url.as_deref() == Some("https://example.test/item/42")), "{acted:?}");
}

#[tokio::test]
async fn test_an_address_goto_cannot_open_is_told_back_and_the_run_goes_on() {
    let driver =
        Driver::new([json!({"next": "", "actions": [{"tool": "goto", "url": "mailto:a@example.test"}]}), give_up()]);
    let result = run(Arc::new(DatePage::default()), driver.clone()).await;
    assert_eq!(result.error.as_deref().map(|error| error.contains("test over")), Some(true), "{:?}", result.error);
    assert!(driver.told(1).contains("goto opens only an http or https address"), "{}", driver.told(1));
}

#[tokio::test]
async fn test_naming_ids_the_page_lacks_turn_after_turn_ends_the_run_stuck() {
    let turns = (0..40).map(|_| json!({"next": "", "actions": [{"tool": "click", "id": "99"}]}));
    let driver = Driver::new(turns);
    let result = run(Arc::new(DatePage::default()), driver.clone()).await;
    assert_eq!(result.status, Status::Stuck, "{:?}", result.error);
    assert!(driver.sent.lock().unwrap().len() < 40, "each refusal counts toward the stall rule");
}

#[tokio::test]
async fn test_an_operation_the_control_does_not_take_is_refused_and_told_back() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "type", "id": "2", "text": "x"}]}), give_up()]);
    run(page.clone(), driver.clone()).await;
    assert!(page.acted.lock().unwrap().is_empty());
    assert!(driver.told(1).contains("does not take fill"), "{}", driver.told(1));
}

/// The first request's messages as JSON bytes, on `page`.
async fn first_request_bytes(page: DatePage) -> i64 {
    let page = Arc::new(page);
    let driver = Driver::new([give_up()]);
    run(page, driver.clone()).await;
    serde_json::to_vec(&driver.sent.lock().unwrap()[0]).unwrap().len() as i64
}

#[tokio::test]
async fn test_a_screenshot_kept_in_the_conversation_is_counted_against_the_models_context() {
    let page = Arc::new(DatePage { png: true, ..DatePage::default() });
    let turns = [
        json!({"next": "", "actions": [{"tool": "screenshot"}]}),
        json!({"next": "", "actions": [{"tool": "wait", "seconds": 0}]}),
        give_up(),
    ];
    let driver = Arc::new(Driver { context: Some(128_000), ..Arc::into_inner(Driver::new(turns)).unwrap() });
    let result = run(page, driver.clone()).await;
    assert_eq!(result.status, Status::Stuck, "{:?}", result.error);
    assert!(!driver.sent.lock().unwrap()[2].iter().all(|message| message.images.is_empty()));
}

/// The driver's screenshot carries the number of each listed control the page could measure, and its notice says so;
/// with none measured, the screenshot goes as taken (#1574).
#[tokio::test]
async fn test_a_screenshot_numbers_the_listed_controls_it_shows() {
    for boxed in [true, false] {
        let page = Arc::new(DatePage { png: true, boxed, ..DatePage::default() });
        let turns = [json!({"next": "", "actions": [{"tool": "screenshot"}]}), give_up()];
        let driver = Driver::new(turns);
        run(page, driver.clone()).await;
        let told = driver.told(1);
        assert_eq!(told.contains("its number drawn in a magenta tag"), boxed, "{told}");
        let sent = driver.sent.lock().unwrap();
        let image = sent[1].iter().find_map(|message| message.images.first()).expect("the screenshot is attached");
        let png = image::load_from_memory(&image.0).unwrap().to_rgba8();
        let tagged = png.pixels().any(|pixel| *pixel == image::Rgba([255, 0, 255, 255]));
        assert_eq!(tagged, boxed);
    }
}

#[tokio::test]
async fn test_a_conversation_past_the_models_context_drops_the_pages_it_moved_on_from() {
    let offscreen = "comment ".repeat(4000);
    let first =
        first_request_bytes(DatePage { offscreen: offscreen.clone(), unplaced: true, ..DatePage::default() }).await;
    let page = Arc::new(DatePage { offscreen: offscreen.clone(), unplaced: true, ..DatePage::default() });
    let turns = [json!({"next": "", "actions": [{"tool": "click", "id": "1"}]}), give_up()];
    let driver = Arc::new(Driver { context: Some(first + 12_000), ..Arc::into_inner(Driver::new(turns)).unwrap() });
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
    let first =
        first_request_bytes(DatePage { offscreen: offscreen.clone(), unplaced: true, ..DatePage::default() }).await;
    let page = Arc::new(DatePage { offscreen, unplaced: true, ..DatePage::default() });
    let driver =
        Arc::new(Driver { context: Some(first - 20_000), ..Arc::into_inner(Driver::new([give_up()])).unwrap() });
    run(page, driver.clone()).await;
    let told = driver.told(0);
    assert!(
        !told.contains("comment comment") && told.contains("The controls and text outside the viewport are left out"),
        "{told}"
    );
    assert!(told.contains("Pick a date"), "{told}");
}

#[tokio::test]
async fn test_done_may_quote_a_page_the_driver_was_shown_before_an_action_replaced_it() {
    let page = Arc::new(DatePage { before: Some("Total: $43.18".into()), ..DatePage::default() });
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "click", "id": "1"}]}),
        json!({"next": "", "actions": [done("The total is $43.18.", &["Total: $43.18"])]}),
        give_up(),
        give_up(),
    ]);
    run(page, driver.clone()).await;
    let sent = driver.sent.lock().unwrap();
    // The turn after the done is where a refused quote is told: without it the loop below checks nothing.
    assert!(sent.len() >= 3, "the done was judged and the driver told how it went");
    for turn in sent.iter().skip(2) {
        let told = &turn.last().unwrap().content;
        assert!(!told.contains("word for word"), "the earlier page's total was refused: {told}");
    }
}

#[tokio::test]
async fn test_a_done_quoting_a_page_shown_before_traces_what_it_read_there() {
    let page = Arc::new(DatePage { before: Some("Total: $43.18".into()), ..DatePage::default() });
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "click", "id": "1"}]}),
        json!({"next": "", "actions": [done("The total is $43.18.", &["Total: $43.18"])]}),
        give_up(),
        give_up(),
    ]);
    let trace = fastbrowse_core::telemetry::Trace::collecting();
    let agent = Agent::new(page, Arc::new(ScriptedJev::new(Vec::new())), driver).with_trace(trace.clone());
    let options = RunOptions {
        start: Some("https://example.test/".into()),
        limits: Some(Limits { max_steps: Some(10), ..Default::default() }),
        ..Default::default()
    };
    agent.run("Open the date field and go to the next month.", options).await.expect("a result");
    // A replay rebuilds the notes from the trace's reads, so the earlier page's quote must be among them.
    let read = trace.events().into_iter().filter(|event| event["event"] == "read").any(|event| {
        event["facts"].as_array().is_some_and(|facts| facts.iter().any(|fact| fact["text"] == "Total: $43.18"))
    });
    assert!(read, "the quote from the earlier page was not traced as read");
}

#[tokio::test]
async fn test_a_page_whose_controls_outrun_the_models_context_is_shown_without_those_outside_the_viewport() {
    let first = first_request_bytes(DatePage { replies: 2000, ..DatePage::default() }).await;
    let page = Arc::new(DatePage { replies: 2000, ..DatePage::default() });
    let driver =
        Arc::new(Driver { context: Some(first - 20_000), ..Arc::into_inner(Driver::new([give_up()])).unwrap() });
    run(page, driver.clone()).await;
    let told = driver.told(0);
    assert!(!told.contains(r#"link "reply""#) && told.contains("outside the viewport are left out"), "{told}");
    assert!(told.contains(r#"textbox "Date""#), "{told}");
}

#[tokio::test]
async fn test_the_driver_is_told_the_browser_is_solving_a_captcha_on_the_page() {
    let page = Arc::new(DatePage { solving: Some("cloudflare".into()), ..DatePage::default() });
    let driver = Driver::new([give_up()]);
    run(page, driver.clone()).await;
    let told = driver.told(0);
    assert!(told.contains("CAPTCHA solver is working on this page's cloudflare CAPTCHA; wait(seconds)"), "{told}");
}

#[tokio::test]
async fn test_a_fetched_page_longer_than_the_models_context_goes_to_a_browser() {
    let first = first_request_bytes(DatePage { replies: 2000, http: true, ..DatePage::default() }).await;
    let page = Arc::new(DatePage { replies: 2000, http: true, ..DatePage::default() });
    let driver =
        Arc::new(Driver { context: Some(first - 20_000), ..Arc::into_inner(Driver::new([give_up()])).unwrap() });
    let result = run(page, driver.clone()).await;
    assert_eq!(result.failure_kind.as_deref(), Some("NeedsBrowser"), "{result:?}");
    assert!(driver.sent.lock().unwrap().is_empty(), "the page too long for the model was never sent");
}

#[tokio::test]
async fn test_the_browser_a_fetched_page_was_handed_to_is_told_the_task() {
    let first = first_request_bytes(DatePage { replies: 2000, http: true, ..DatePage::default() }).await;
    let fetched = Arc::new(DatePage { replies: 2000, http: true, ..DatePage::default() });
    let driver =
        Arc::new(Driver { context: Some(first - 20_000), ..Arc::into_inner(Driver::new([give_up()])).unwrap() });
    let agent = Agent::new(fetched, Arc::new(ScriptedJev::new(Vec::new())), driver.clone());
    let options = || RunOptions {
        start: Some("https://example.test/".into()),
        limits: Some(Limits { max_steps: Some(10), ..Default::default() }),
        ..Default::default()
    };
    let result = agent.run("Open the date field and go to the next month.", options()).await.expect("a result");
    assert_eq!(result.failure_kind.as_deref(), Some("NeedsBrowser"), "{result:?}");
    let resume = agent.resume().expect("the fetched run hands its state on");
    let browser = Agent::new(Arc::new(DatePage::default()), Arc::new(ScriptedJev::new(Vec::new())), driver.clone());
    browser
        .run("Open the date field and go to the next month.", RunOptions { resume: Some(resume), ..options() })
        .await
        .expect("a result");
    let sent = driver.sent.lock().unwrap();
    assert!(sent[0].iter().any(|message| message.content.contains("<task>\n")), "{:?}", sent[0]);
}

#[tokio::test]
async fn test_the_driver_is_told_a_check_that_bars_one_way_leaves_the_others() {
    let driver = Driver::new([give_up()]);
    run(Arc::new(DatePage::default()), driver.clone()).await;
    let system = &driver.sent.lock().unwrap()[0][0].content;
    assert!(system.contains("a check that bars one way, such as a search, leaves the others"), "{system}");
}

/// A Jev that holds the answer's one claim supported by the quotes it cites.
fn supporting() -> Arc<dyn DecisionClient> {
    Arc::new(Answering(ScriptedJev::new(Vec::new()), &[("claim0", 0.9)]))
}

/// A done whose answer is one claim, `text`, resting on `quotes`.
fn done(text: &str, quotes: &[&str]) -> Value {
    json!({"tool": "done", "answer": [{"text": text, "quotes": quotes, "tally": null, "within": null}], "numbered": false})
}

#[tokio::test]
async fn test_the_answer_the_driver_writes_in_done_is_reviewed_without_a_composer() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [done("The field asks for a date.", &["a date"])]})]);
    let result = run_with(page, driver.clone(), supporting()).await;
    assert_eq!(result.status, Status::Complete, "{:?}", result.error);
    assert_eq!(
        result.answer.as_deref(),
        Some("The field asks for a date. [1](<https://example.test/#:~:text=a%20date>)")
    );
    assert!(driver.composed.lock().unwrap().is_empty(), "a composer was asked to write the answer again");
}

#[tokio::test]
async fn test_a_tally_whose_value_the_claim_text_does_not_write_is_refused_and_told_back() {
    let page = Arc::new(DatePage::default());
    let tally = json!({"op": "count", "of": "date", "scope": null, "field": null, "rank": null, "by": null,
        "group": null, "written": "3"});
    let claim = json!({"text": "The page asks for one date.", "quotes": ["a date"], "tally": tally, "within": null});
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "done", "answer": [claim], "numbered": false}]}),
        give_up(),
    ]);
    run(page, driver.clone()).await;
    let told = driver.told(1);
    assert!(told.contains(r#"written value "3" must appear exactly once in its text"#), "{told}");
}

#[tokio::test]
async fn test_the_driver_is_told_to_name_the_pages_a_count_ranges_over() {
    // Told to quote every record, the driver copied all 26 quotes of a site's top three authors into one reply, and the
    // provider's filter stopped it; code already reads each quoted page whole.
    let driver = Driver::new([give_up()]);
    run(Arc::new(DatePage::default()), driver.clone()).await;
    let system = &driver.sent.lock().unwrap()[0][0].content;
    assert!(
        system.contains(
            "Set pages to the address of every page shown to you that its records are on, and quote no record"
        ),
        "{system}"
    );
}

/// A run of the date task with `recipe` to replay, returning its result and the recipe it saved.
async fn run_recipe(
    page: Arc<DatePage>,
    driver: Arc<Driver>,
    recipe: Option<fastbrowse_core::recipe::Recipe>,
) -> (RunResult, fastbrowse_agent::agent::RecipeOutcome) {
    let sink: fastbrowse_agent::agent::RecipeSink = Arc::new(Mutex::new(None));
    let agent = Agent::new(page, Arc::new(ScriptedJev::new(Vec::new())), driver);
    let result = agent
        .run(
            "Open the date field and go to the next month.",
            RunOptions {
                start: Some("https://example.test/".into()),
                limits: Some(Limits { max_steps: Some(10), ..Default::default() }),
                recipe,
                recipe_sink: Some(sink.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("a result");
    let outcome = sink.lock().unwrap().clone().expect("a saved recipe");
    (result, outcome)
}

/// Records a run that opens the date field, then types the task's own words into the note.
async fn recorded() -> fastbrowse_core::recipe::Recipe {
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "click", "id": "1"}]}),
        json!({"next": "", "actions": [{"tool": "type", "id": "3", "text": "next month"}]}),
        give_up(),
    ]);
    let (_, outcome) = run_recipe(Arc::new(DatePage::default()), driver, None).await;
    assert_eq!(outcome.recipe.steps.len(), 2, "both actions are saved");
    outcome.recipe
}

#[tokio::test]
async fn test_saved_steps_run_before_the_first_turn_and_the_driver_is_told_they_were_taken() {
    let recipe = recorded().await;
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([give_up()]);
    let (_, outcome) = run_recipe(page.clone(), driver.clone(), Some(recipe)).await;
    let acted: Vec<_> = page.acted.lock().unwrap().iter().map(|a| (a.target_id.clone(), a.text.clone())).collect();
    assert_eq!(acted, [(Some("main:date".into()), None), (Some("main:note".into()), Some("next month".into()))]);
    assert_eq!((outcome.replayed, outcome.fell_back), (2, 0));
    assert_eq!(driver.sent.lock().unwrap().len(), 1, "no turn was spent on a saved step");
    let told = driver.told(0);
    assert!(told.contains("the 2 steps saved from an earlier run of this task were all taken"), "{told}");
}

#[tokio::test]
async fn test_saved_steps_stop_at_the_first_one_not_on_the_page_and_the_driver_is_told_where() {
    let mut recipe = recorded().await;
    recipe.steps[1].target.as_mut().unwrap().label = "Comment".into();
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([give_up()]);
    let (_, outcome) = run_recipe(page.clone(), driver.clone(), Some(recipe)).await;
    let acted: Vec<_> = page.acted.lock().unwrap().iter().map(|a| a.target_id.clone()).collect();
    assert_eq!(acted, [Some("main:date".into())]);
    assert_eq!((outcome.replayed, outcome.fell_back), (1, 1));
    let told = driver.told(0);
    assert!(told.contains("1 of 2 steps saved from an earlier run of this task were taken; step 2"), "{told}");
    assert!(told.contains("its control is not on this page exactly once"), "{told}");
}

#[tokio::test]
async fn test_text_the_driver_wrote_that_the_task_does_not_state_is_left_to_the_driver_on_replay() {
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "type", "id": "3", "text": "copied from the page"}]}),
        give_up(),
    ]);
    let (_, outcome) = run_recipe(Arc::new(DatePage::default()), driver, None).await;
    assert!(!outcome.recipe.steps[0].replayable(), "{:?}", outcome.recipe.steps[0]);
}

#[tokio::test]
async fn test_a_drag_reaches_the_page_with_its_drop_target() {
    // With no drag tool, a task that moves a card between board columns had nothing to move it with and ended stuck.
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "drag", "id": "3", "to": "2"}]}), give_up()]);
    run(page.clone(), driver.clone()).await;
    let acted = page.acted.lock().unwrap().clone();
    let drag = acted.iter().find(|action| action.operation == fastbrowse_core::models::Operation::Drag);
    let drag = drag.unwrap_or_else(|| panic!("{acted:?}"));
    assert_eq!(
        (drag.target_id.as_deref(), drag.destination_id.as_deref()),
        (Some("main:note"), Some("main:inline-next"))
    );
}

#[tokio::test]
async fn test_a_drag_onto_a_control_not_on_the_page_names_the_missing_drop_target() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "drag", "id": "3", "to": "99"}]}), give_up()]);
    run(page.clone(), driver.clone()).await;
    let told = driver.told(1);
    assert!(told.contains("there is no control [99]"), "{told}");
}

/// A drag with no drop target on a control not drawn on is the driver's to finish: ended as needs_input, a run asked a
/// person for a target the page had.
#[tokio::test]
async fn test_a_drag_with_no_drop_target_off_a_drawn_surface_is_told_back() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "drag", "id": "3", "to": null}]}), give_up()]);
    let result = run(page.clone(), driver.clone()).await;
    assert_ne!(result.status, Status::NeedsInput, "{:?}", result.error);
    let told = driver.told(1);
    assert!(told.contains("is not drawn on, so a drag needs to"), "{told}");
    assert!(
        page.acted.lock().unwrap().iter().all(|action| action.operation != fastbrowse_core::models::Operation::Drag)
    );
}

#[tokio::test]
async fn test_an_operation_its_control_does_not_list_is_refused_before_the_browser() {
    // Only click, type and select were checked, so a hover on a control that lists none reached the browser.
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "hover", "id": "2"}]}), give_up()]);
    run(page.clone(), driver.clone()).await;
    assert!(page.acted.lock().unwrap().is_empty(), "{:?}", page.acted.lock().unwrap());
    let told = driver.told(1);
    assert!(told.contains("that control does not take hover"), "{told}");
}

#[tokio::test]
async fn test_a_tab_that_is_not_open_is_told_as_missing() {
    // An unchecked tab id reached the browser, which refused it, and the run ended in error.
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "switch_tab", "tab": "9"}]}), give_up()]);
    run(page.clone(), driver.clone()).await;
    assert!(page.acted.lock().unwrap().is_empty(), "{:?}", page.acted.lock().unwrap());
    let told = driver.told(1);
    assert!(told.contains("there is no open tab [9]"), "{told}");
}

/// One secret, `sso`, for an embedded sign-in's origin only.
struct FramedSecret;

impl fastbrowse_core::models::SecretResolver for FramedSecret {
    fn available(&self) -> Vec<fastbrowse_core::models::SecretRef> {
        vec![fastbrowse_core::models::SecretRef { name: "sso".into(), origins: vec!["https://auth.test".into()] }]
    }

    fn resolve<'a>(&'a self, _: &'a str, _: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async { None })
    }
}

#[tokio::test]
async fn test_a_framed_sign_in_from_another_origin_names_its_secrets() {
    // Told only the page's own origin, a run whose sign-in sat in a frame from another site had no secret name to type.
    let page = Arc::new(DatePage { framed: true, ..Default::default() });
    let driver = Driver::new([give_up()]);
    Agent::new(page, Arc::new(ScriptedJev::new(Vec::new())), driver.clone())
        .with_secrets(Some(Arc::new(FramedSecret)))
        .run("Sign in.", RunOptions { start: Some("https://example.test/".into()), ..Default::default() })
        .await
        .expect("a result");
    let told = driver.told(0);
    assert!(told.contains("Secrets you may type on https://auth.test, by name: sso"), "{told}");
}

/// An action planned beside a secret was written before it resolved, so it may carry the value, and the next gate
/// question about it would send it to Jev.
#[tokio::test]
async fn test_an_action_planned_with_the_secret_value_is_not_taken() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([
        json!({"next": "", "actions": [
            {"tool": "type_secret", "id": "3", "name": "pw"},
            {"tool": "type", "id": "3", "text": "hunter2"}
        ]}),
        give_up(),
    ]);
    Agent::new(page.clone(), Arc::new(ScriptedJev::new(Vec::new())), driver)
        .with_secrets(Some(Arc::new(Secret)))
        .run(
            "Sign in with the password.",
            RunOptions { start: Some("https://example.test/".into()), ..Default::default() },
        )
        .await
        .expect("a result");
    let acted = page.acted.lock().unwrap();
    let fills: Vec<_> =
        acted.iter().filter(|action| action.operation == fastbrowse_core::models::Operation::Fill).collect();
    assert_eq!(fills.len(), 1, "{acted:?}");
    assert!(fills[0].secret);
}

#[tokio::test]
async fn test_a_value_shown_before_its_secret_resolved_is_not_sent_again() {
    // Turns are sent whole each time, so a page that showed the value before the run typed it carried it on.
    let page = Arc::new(DatePage { offscreen: "Demo password: hunter2".into(), unplaced: true, ..Default::default() });
    let driver =
        Driver::new([json!({"next": "", "actions": [{"tool": "type_secret", "id": "1", "name": "pw"}]}), give_up()]);
    Agent::new(page, Arc::new(ScriptedJev::new(Vec::new())), driver.clone())
        .with_secrets(Some(Arc::new(Secret)))
        .run(
            "Sign in with the password.",
            RunOptions { start: Some("https://example.test/".into()), ..Default::default() },
        )
        .await
        .expect("a result");
    let sent = driver.sent.lock().unwrap();
    assert!(sent[0].iter().any(|message| message.content.contains("hunter2")), "{:?}", sent[0]);
    assert!(sent[1].iter().all(|message| !message.content.contains("hunter2")), "{:?}", sent[1]);
    assert!(sent[1].iter().any(|message| message.content.contains("[secret:pw]")), "{:?}", sent[1]);
}

/// Pixels cannot be checked for a value learnt after they were taken, and a screenshot of a page showing a demo
/// password was sent again on every turn after the run typed it as a secret.
#[tokio::test]
async fn test_a_screenshot_taken_before_a_secret_resolved_is_not_sent_again() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "screenshot"}]}),
        json!({"next": "", "actions": [{"tool": "type_secret", "id": "1", "name": "pw"}]}),
        give_up(),
    ]);
    Agent::new(page, Arc::new(ScriptedJev::new(Vec::new())), driver.clone())
        .with_secrets(Some(Arc::new(Secret)))
        .run(
            "Sign in with the password.",
            RunOptions { start: Some("https://example.test/".into()), ..Default::default() },
        )
        .await
        .expect("a result");
    let sent = driver.sent.lock().unwrap();
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert!(sent[1].iter().any(|message| !message.images.is_empty()), "the screenshot was sent");
    assert!(sent[2].iter().all(|message| message.images.is_empty()), "{:?}", sent[2]);
}

/// Controls behind a modal are not listed, so a modal listing none of its own left the driver nothing to press Escape
/// in. close() presses it, asked about as any key press is.
#[tokio::test]
async fn test_close_presses_escape_on_a_modal_only_once_authorization_allows_it() {
    let modal = || DatePage { modal: Some("Subscribe to our newsletter".into()), ..Default::default() };
    let close = || json!({"next": "", "actions": [{"tool": "close"}]});

    let page = Arc::new(modal());
    let jev = Arc::new(Answering(ScriptedJev::new(Vec::new()), &[("irreversible", 1.0)]));
    let result = run_with(page.clone(), Driver::new([close()]), jev).await;
    assert_eq!(result.status, Status::NeedsConfirmation, "{:?}", result.error);
    assert!(page.acted.lock().unwrap().is_empty(), "{:?}", page.acted.lock().unwrap());

    let page = Arc::new(modal());
    let jev = Arc::new(Answering(ScriptedJev::new(Vec::new()), &[("irreversible", 0.0)]));
    run_with(page.clone(), Driver::new([close(), give_up()]), jev).await;
    let acted = page.acted.lock().unwrap();
    assert_eq!(acted.len(), 1, "{acted:?}");
    assert_eq!((acted[0].operation, acted[0].target_id.as_deref()), (fastbrowse_core::models::Operation::Escape, None));
}

#[tokio::test]
async fn test_close_with_no_modal_open_is_told_back() {
    let page = Arc::new(DatePage::default());
    let driver = Driver::new([json!({"next": "", "actions": [{"tool": "close"}]}), give_up()]);
    run(page.clone(), driver.clone()).await;
    assert!(page.acted.lock().unwrap().is_empty());
    assert!(driver.told(1).contains("close() was refused: no modal is open."), "{}", driver.told(1));
}

/// A list of quotes over three pages, each with its own next-page link, and a page-per-url capture.
struct QuotesPage {
    at: std::sync::atomic::AtomicUsize,
    /// Lines of text each page holds, so a long page can be windowed.
    filler: usize,
}

impl QuotesPage {
    fn new(filler: usize) -> Arc<QuotesPage> {
        Arc::new(QuotesPage { at: std::sync::atomic::AtomicUsize::new(1), filler })
    }

    fn page(&self) -> usize {
        self.at.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn text(&self) -> String {
        let n = self.page();
        let mut lines = vec![format!("Quote {n}a by Ann"), format!("Quote {n}b by Bob")];
        lines.extend((0..self.filler).map(|line| format!("Filler line {line} of page {n}")));
        lines.join("\n")
    }
}

impl Page for QuotesPage {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        let n = self.page();
        let mut controls = vec![json!({"id": "main:about", "frame_id": null, "role": "link", "label": "about",
            "operations": ["click"], "sensitive": false, "offscreen": false, "href": "/about/"})];
        if n < 3 {
            controls.push(json!({"id": "main:next", "frame_id": null, "role": "link", "label": "Next",
                "operations": ["click"], "sensitive": false, "offscreen": false, "next_page": true,
                "href": format!("/page/{}/", n + 1)}));
        }
        controls.push(json!({"id": "main:footer", "frame_id": null, "role": "link", "label": "Terms",
            "operations": ["click"], "sensitive": false, "offscreen": self.filler > 0, "screen": if self.filler > 0 { 60 } else { 0 },
            "href": "/terms/"}));
        let text = self.text();
        let viewport = text.lines().take(3).collect::<Vec<_>>().join("\n");
        let rest = text.lines().skip(3).collect::<Vec<_>>().join("\n");
        Box::pin(async move {
            Ok(serde_json::from_value(json!({
                "url": format!("https://quotes.test/page/{n}/"), "title": "Quotes",
                "page_key": format!("k{n}"), "document_key": format!("d{n}"),
                "captured_at": "2026-11-18T00:00:00Z", "viewport_text": viewport, "offscreen_text": rest,
                "tabs": [], "controls": controls
            }))
            .unwrap())
        })
    }

    fn document_changed<'a>(&'a self, _: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn navigate<'a>(&'a self, url: &'a str, _: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        let n: usize = url.trim_end_matches('/').rsplit('/').next().and_then(|n| n.parse().ok()).unwrap_or(1);
        self.at.store(n, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        let (n, text) = (self.page(), self.text());
        // The first three lines are the viewport's, as the observation's `viewport_text` is; the rest is one block.
        let viewport = text.lines().take(3).collect::<Vec<_>>().join("\n").chars().count();
        let total = text.chars().count();
        let mut blocks = vec![json!({"source_id": "p", "frame_id": null, "kind": "paragraph", "start": 0,
            "end": viewport, "in_viewport": true})];
        if total > viewport {
            blocks.push(json!({"source_id": "q", "frame_id": null, "kind": "paragraph", "start": viewport + 1,
                "end": total}));
        }
        Box::pin(async move {
            Ok(serde_json::from_value(json!({
                "url": format!("https://quotes.test/page/{n}/"), "title": "Quotes", "captured_at": "2026-11-18T00:00:00Z",
                "sha256": fastbrowse_core::py::sha256_hex(text.as_bytes()), "text": text,
                "blocks": blocks
            }))
            .unwrap())
        })
    }

    fn act<'a>(&'a self, action: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        let to = action.url.as_deref().and_then(|url| url.trim_end_matches('/').rsplit('/').next()?.parse().ok());
        if let Some(n) = to {
            self.at.store(n, std::sync::atomic::Ordering::SeqCst);
        }
        Box::pin(async move {
            Ok(ActResult {
                outcome: StepOutcome::Executed,
                failure: None,
                page_changed: to.is_some(),
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
        let n = self.page();
        Box::pin(async move { Ok(format!("https://quotes.test/page/{n}/")) })
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

/// The supporting Jev, which also answers a count's questions yes: each quote line is one whole quote, and the task
/// asks to be told how many there are.
struct Counting(Arc<dyn DecisionClient>);

impl DecisionClient for Counting {
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
            for (key, question) in questions {
                if let (Question::Noul(noul), Some(Answer::Noul(answer))) = (question, evaluation.answers.get_mut(key))
                {
                    let counts = ["exactly one whole", "Does any part of the task ask", "Is every factual statement"];
                    if counts.iter().any(|asked| noul.instructions.contains(asked)) {
                        answer.probability = 1.0;
                    }
                }
            }
            Ok(evaluation)
        })
    }
}

async fn run_quotes(page: Arc<QuotesPage>, driver: Arc<Driver>, task: &str) -> RunResult {
    let agent = Agent::new(page, Arc::new(Counting(supporting())), driver);
    agent
        .run(
            task,
            RunOptions {
                start: Some("https://quotes.test/page/1/".into()),
                limits: Some(Limits { max_steps: Some(10), ..Default::default() }),
                ..Default::default()
            },
        )
        .await
        .expect("a result")
}

fn count_all(pages: &[&str]) -> Value {
    let tally = json!({"op": "count", "of": "quote", "scope": null, "field": null, "rank": null,
        "by": null, "group": null, "written": "6"});
    let claim = json!({"text": "There are 6 quotes on the site.", "quotes": [], "pages": pages,
        "tally": tally, "within": null});
    json!({"next": "", "actions": [{"tool": "done", "answer": [claim], "numbered": false}]})
}

#[tokio::test]
async fn test_a_list_across_pages_is_counted_in_two_turns_with_read_pages() {
    let page = QuotesPage::new(0);
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "read_pages", "id": "2", "max_pages": null}]}),
        count_all(&["https://quotes.test/page/1/", "https://quotes.test/page/2/", "https://quotes.test/page/3/"]),
        give_up(),
    ]);
    let result = run_quotes(page, driver.clone(), "How many quotes are there on the whole site?").await;
    let told = driver.told(1);
    let turns = driver.sent.lock().unwrap().len();
    let third = if turns > 2 { driver.told(2) } else { String::new() };
    assert_eq!(turns, 2, "{:?} {third}", result.error);
    assert!(told.contains("read_pages read the page you were on and 2 more page(s) of the list"), "{told}");
    assert!(told.contains("you have not seen the text of the other pages listed"), "{told}");
    assert!(told.contains("is a tally: set tally and name these pages in `pages`"), "{told}");
    assert!(told.contains("code checks the value or record you write against every record on them"), "{told}");
    assert!(told.contains("the list ended there"), "{told}");
    assert!(told.contains("https://quotes.test/page/1/"), "page 1, the page the read began on, is named: {told}");
    assert!(told.contains("https://quotes.test/page/2/") && told.contains("https://quotes.test/page/3/"), "{told}");
    assert!(told.contains("Page: \"Quotes\" https://quotes.test/page/3/"), "{told}");
    // The three pages were never named by a refused done: every one was shown to the run.
    assert!(!told.contains("done was refused"), "{told}");
    assert_eq!(result.status, Status::Complete, "{:?} {:?}", result.error, result.answer);
}

#[tokio::test]
async fn test_read_pages_stops_where_the_model_asked_and_says_the_list_goes_on() {
    let driver =
        Driver::new([json!({"next": "", "actions": [{"tool": "read_pages", "id": "2", "max_pages": 1}]}), give_up()]);
    run_quotes(QuotesPage::new(0), driver.clone(), "Read the quotes.").await;
    let told = driver.told(1);
    assert!(
        told.contains("read_pages read the page you were on and 1 more page(s)")
            && told.contains("links to a further page"),
        "{told}"
    );
    assert!(told.contains("quotes.test/page/2/") && !told.contains("quotes.test/page/3/"), "{told}");
}

#[tokio::test]
async fn test_read_pages_refuses_a_link_the_page_does_not_mark_as_a_next_page() {
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "read_pages", "id": "1", "max_pages": null}]}),
        give_up(),
    ]);
    run_quotes(QuotesPage::new(0), driver.clone(), "Read the quotes.").await;
    assert!(driver.told(1).contains("read_pages was refused: [1] is not a link its page marks"), "{}", driver.told(1));
}

#[tokio::test]
async fn test_read_text_naming_no_lines_is_refused_as_no_progress() {
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "read_text", "page": null, "from": 5, "lines": 0}]}),
        give_up(),
    ]);
    run_quotes(QuotesPage::new(200), driver.clone(), "Read the quotes.").await;
    let told = driver.told(1);
    assert!(told.contains("read_text was refused: it names no lines"), "{told}");
    assert!(!told.contains("lines 5 to 4"), "{told}");
}

#[tokio::test]
async fn test_read_controls_naming_no_controls_is_refused() {
    let driver =
        Driver::new([json!({"next": "", "actions": [{"tool": "read_controls", "from": 1, "count": 0}]}), give_up()]);
    run_quotes(QuotesPage::new(200), driver.clone(), "Read the quotes.").await;
    let told = driver.told(1);
    assert!(told.contains("read_controls was refused"), "{told}");
}

#[tokio::test]
async fn test_read_controls_from_past_the_controls_left_out_is_refused_with_their_range() {
    // An id passed as `from` was refused with only the count, and the driver asked again from another id.
    let driver =
        Driver::new([json!({"next": "", "actions": [{"tool": "read_controls", "from": 110, "count": 5}]}), give_up()]);
    run_quotes(QuotesPage::new(200), driver.clone(), "Read the quotes.").await;
    let told = driver.told(1);
    assert!(
        told.contains(
            "read_controls was refused: from counts the 1 controls further than a screen from the viewport, 1 to 1"
        ),
        "{told}"
    );
}

#[tokio::test]
async fn test_a_read_too_long_for_the_context_is_cut_and_the_driver_told_where_to_resume() {
    let first = {
        let driver = Driver::new([give_up()]);
        run_quotes(QuotesPage::new(3000), driver.clone(), "Read the quotes.").await;
        serde_json::to_vec(&driver.sent.lock().unwrap()[0]).unwrap().len() as i64
    };
    let driver = Arc::new(Driver {
        context: Some(first + 12_000),
        ..Arc::into_inner(Driver::new([
            json!({"next": "", "actions": [{"tool": "read_text", "page": null, "from": 10, "lines": 2500}]}),
            give_up(),
        ]))
        .unwrap()
    });
    run_quotes(QuotesPage::new(3000), driver.clone(), "Read the quotes.").await;
    let told = driver.told(1);
    assert!(told.contains("does not fit your context: read it again from the line after the last one shown"), "{told}");
    assert!(told.contains("Filler line 6 of page 1") && !told.contains("Filler line 2000 of page 1"), "{told}");
    // The header names the lines still shown, not the range asked for.
    let header = told.split("read_text shows lines 10 to ").nth(1).expect("the header keeps its first line");
    let last: usize = header.split(" of ").next().unwrap().parse().unwrap();
    let body = header.split_once(":\n").unwrap().1.split("\n(the rest").next().unwrap();
    assert_eq!(last, 10 + body.lines().count() - 1, "{told}");
    assert!(last < 2509, "{told}");
}

#[tokio::test]
async fn test_a_long_page_goes_as_its_window_with_the_rest_named_and_readable() {
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "read_text", "page": null, "from": 40, "lines": 2}]}),
        json!({"next": "", "actions": [{"tool": "read_controls", "from": 1, "count": 5}]}),
        give_up(),
    ]);
    run_quotes(QuotesPage::new(200), driver.clone(), "Read the quotes.").await;
    let first = driver.told(0);
    assert!(first.contains("Page text, lines 1 to 6 of 203"), "{first}");
    assert!(first.contains("The last 197 lines are not shown: read_text(null, from, lines, null)"), "{first}");
    assert!(!first.contains("Filler line 100"), "{first}");
    assert!(
        first.contains("1 more controls are further than a screen from the viewport: read_controls(from, count) lists them in reading order, from the `from`th of them (1 to 1)"),
        "{first}"
    );
    assert!(!first.contains("Terms"), "{first}");
    let second = driver.told(1);
    assert!(second.contains("read_text shows lines 40 to 41 of 203 of https://quotes.test/page/1/:\nFiller line 36 of page 1\nFiller line 37"), "{second}");
    let third = driver.told(2);
    assert!(third.contains("read_controls shows controls 1 to 1 of 1") && third.contains("link \"Terms\""), "{third}");
}

#[tokio::test]
async fn test_read_text_with_find_lists_the_lines_holding_it_under_their_numbers() {
    let driver = Driver::new([
        json!({"next": "", "actions": [{"tool": "read_text", "page": null, "from": 1, "lines": 3, "find": "FILLER line 15 of"}]}),
        give_up(),
    ]);
    run_quotes(QuotesPage::new(200), driver.clone(), "Read the quotes.").await;
    let told = driver.told(1);
    assert!(
        told.contains("read_text shows matches 1 to 1 of 1 of https://quotes.test/page/1/ for \"FILLER line 15 of\""),
        "{told}"
    );
    assert!(told.contains("\n19: Filler line 15 of page 1\n"), "{told}");
}

#[tokio::test]
async fn test_the_driver_is_told_how_to_read_a_list_and_a_long_page() {
    let driver = Driver::new([give_up()]);
    run(Arc::new(DatePage::default()), driver.clone()).await;
    let system = &driver.sent.lock().unwrap()[0][0].content;
    for told in [
        "read_pages(id, max_pages) follows the next-page link `id`",
        "Their text is held for done: cite them in `pages`",
        "read_text(page, from, lines, find) shows lines of a page's text",
        "read_controls(from, count) lists controls further than a screen from the viewport",
        "A long page is shown as its viewport and the screen above and below it",
    ] {
        assert!(system.contains(told), "{told}\n{system}");
    }
}
