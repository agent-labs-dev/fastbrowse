//! The loop's pure helpers and small state transitions. Whole runs, and the async methods that need a scripted page and models, are replayed in
//! tests/agent.rs instead.

use std::collections::HashSet;
use std::sync::Arc;

use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{
    Answer, BoxAnswer, ChoiceAnswer, DecisionClient, DecisionError, DecisionProfile, Edges, Evaluation, Images,
    NoulAnswer, Question,
};
use fastbrowse_core::models::{
    Artifact, Authorization, CostBasis, CostComponent, CostLine, Decider, Event, EventHandler, Evidence, FactReader,
    Limits, Operation, Status, StepOutcome, StepResult,
};
use fastbrowse_core::page::{
    ActResult, Action, BlockKind, BrowserError, Capture, Control, Observation, Page, PageResult,
};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::telemetry::{Ledger, Trace};
use indexmap::IndexMap;
use serde_json::json;

use super::Agent;
use super::driving::edit_progress;
use super::helpers::{read_sources, unsent_fact};
use super::state::{Flow, RunState};
use crate::memory::{Fact, Notes};
use crate::policy::{Decision, HistoryEntry};
use crate::retrieval::testing::capture;
use crate::scripted::{ScriptedJev, ScriptedLLM};

/// The page behind an agent whose tests never touch it.
struct NoPage;

impl Page for NoPage {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        unreachable!("the helpers under test do not observe")
    }

    fn document_changed<'a>(&'a self, _: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        unreachable!("the helpers under test do not observe")
    }

    fn navigate<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        unreachable!("the helpers under test do not navigate")
    }

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        unreachable!("the helpers under test do not capture")
    }

    fn act<'a>(&'a self, _: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        unreachable!("the helpers under test do not act")
    }

    fn redrawn<'a>(&'a self, _: &'a Observation, _: f64, _: Option<&'a str>) -> BoxFuture<'a, PageResult<bool>> {
        unreachable!("the helpers under test do not watch")
    }

    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        unreachable!("the helpers under test take no screenshot")
    }

    fn withhold_frames(&self, _: bool) {}

    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        unreachable!("the helpers under test do not ask the address")
    }

    fn response_status(&self) -> BoxFuture<'_, PageResult<Option<i64>>> {
        unreachable!("the helpers under test do not ask the status")
    }

    fn lifecycle_mark(&self) -> u64 {
        unreachable!("the helpers under test do not wait on the page")
    }

    fn wait_for_lifecycle(&self, _: u64, _: f64) -> BoxFuture<'_, bool> {
        unreachable!("the helpers under test do not wait on the page")
    }
}

fn agent() -> Agent {
    Agent::new(Arc::new(NoPage), Arc::new(ScriptedJev::new(Vec::new())), Arc::new(ScriptedLLM))
}

fn free() -> CostLine {
    CostLine::new(CostComponent::Jev, CostBasis::Estimated, Some(0.0))
}

/// A run at its first step.
fn run_state() -> RunState {
    RunState::new(
        "Find a train from Bristol to York on 16 October 2026".into(),
        IndexMap::new(),
        Vec::new(),
        Authorization::default(),
        Arc::new(Ledger::new(Limits::default())),
    )
}

fn observation(controls: &[Control]) -> Observation {
    serde_json::from_value(json!({
        "url": "https://example.test/", "title": "t", "page_key": "k", "captured_at": "2026-09-26T20:00:00Z",
        "controls": controls.iter().map(Dump::dump).collect::<Vec<_>>(),
        "viewport_text": "", "tabs": [],
    }))
    .unwrap()
}

fn at(url: &str, controls: &[Control]) -> Observation {
    Observation { url: url.into(), page_key: url.into(), ..observation(controls) }
}

fn field(label: &str) -> Control {
    let mut control = Control::new("field", "combobox", label, [Operation::Fill, Operation::Click, Operation::Enter]);
    control.value = Some("Bath".into());
    control
}

fn button(label: &str) -> Control {
    Control::new(&label.to_lowercase(), "button", label, [Operation::Click])
}

fn code_decision(operation: Operation, target: Option<Control>) -> Decision {
    Decision {
        operation,
        url: None,
        target_confidence: target.as_ref().map(|_| 1.0),
        target,
        tab_id: None,
        operation_confidence: 1.0,
        login_required: None,
        offered_controls: 0,
        reduction: crate::policy::Reduction::None,
        cost: Vec::new(),
        input_tokens: 0,
        bot_check: None,
        shows_answer: None,
        site_failure: None,
        destination: None,
        offered: Vec::new(),
        ahead: IndexMap::new(),
    }
}

/// One whole block, as the reader's cite of it copies.
fn block_evidence(page: &Capture, source_id: &str) -> Evidence {
    let block = page.blocks.iter().find(|block| block.source_id == source_id).unwrap();
    serde_json::from_value(json!({
        "source_id": source_id, "url": page.url, "frame_id": block.frame_id, "captured_at": page.captured_at,
        "capture_sha256": page.sha256, "start": block.start, "end": block.end,
        "quote": page.text.chars().skip(block.start as usize).take((block.end - block.start) as usize).collect::<String>(),
        "heading_path": block.heading_path,
    }))
    .unwrap()
}

fn executed(operation: Option<Operation>, target: Option<&str>, page_changed: bool) -> HistoryEntry {
    HistoryEntry::new(operation, target, StepOutcome::Executed, page_changed)
}

fn set(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

#[tokio::test]
async fn test_the_next_observation_records_what_an_action_did() {
    let field_before = field("Search elsewhere");
    let mut state = run_state();
    state.acted_from = Some(observation(std::slice::from_ref(&field_before)));
    state.history.push(executed(Some(Operation::Fill), Some("Search elsewhere"), true));
    let after = Control { value: Some("York".into()), ..field_before };
    agent().note_effect(&mut state, &observation(&[after]));
    assert_eq!(state.history.last().unwrap().effect.as_deref(), Some("changed Search elsewhere value: Bath -> York"));
    assert!(state.acted_from.is_none());
}

#[tokio::test]
async fn test_what_the_browser_saw_an_action_do_stays_beside_the_page_change() {
    let field_before = field("Search elsewhere");
    let mut state = run_state();
    state.acted_from = Some(observation(std::slice::from_ref(&field_before)));
    state.history.push(HistoryEntry {
        effect: Some("downloaded the file the link points at".into()),
        ..executed(Some(Operation::Click), Some("export"), true)
    });
    agent().note_effect(&mut state, &observation(&[field_before]));
    assert_eq!(
        state.history.last().unwrap().effect.as_deref(),
        Some("downloaded the file the link points at; nothing visible changed")
    );
}

/// A decision to write `value` into a field that currently holds `holds`, and the action doing it.
fn edit(value: &str, holds: Option<&str>, secret: bool, operation: Operation) -> (Decision, Action) {
    let target = Control { value: holds.map(String::from), ..Control::new("f", "textbox", "Name", [Operation::Fill]) };
    let action =
        Action { target_id: Some(target.id.clone()), text: Some(value.into()), secret, ..Action::new(operation) };
    (code_decision(operation, Some(target)), action)
}

fn progress(value: &str, holds: Option<&str>, secret: bool, operation: Operation, written: &[&str]) -> Option<bool> {
    let (decision, action) = edit(value, holds, secret, operation);
    edit_progress(&decision, &action, &set(written))
}

#[test]
fn test_writing_a_value_a_field_already_holds_is_not_progress() {
    assert_eq!(progress("Ada", Some("Ada"), false, Operation::Fill, &[]), Some(false));
    // A field's first value is progress: a checkout's three fields changed no page fingerprint and tripped the
    // stall recovery. Only the first: every new value credited let a PyPI run grind on one search box.
    assert_eq!(progress("Ada", None, false, Operation::Fill, &[]), Some(true));
    assert_eq!(progress("Ada", Some(""), false, Operation::Fill, &["f"]), None);
    // A corrected value is not vetoed, even though the field is not empty.
    assert_eq!(progress("Ada", Some("Adz"), false, Operation::Fill, &["f"]), None);
}

#[test]
fn test_a_secret_is_compared_by_the_length_the_page_reveals() {
    let bullets = |n| "\u{2022}".repeat(n);
    assert_eq!(progress("hunter2", Some(&bullets(7)), true, Operation::Fill, &[]), Some(false));
    assert_eq!(progress("hunter2", Some(&bullets(4)), true, Operation::Fill, &["f"]), None);
}

#[test]
fn test_an_upload_is_judged_by_the_page_not_by_a_value() {
    assert_eq!(progress("a.pdf", None, false, Operation::Upload, &[]), None);
}

#[test]
fn test_a_run_carried_on_keeps_what_it_learned_and_no_action_in_flight() {
    let ledger = Arc::new(Ledger::new(Limits::default()));
    let mut state = RunState::new("t".into(), IndexMap::new(), Vec::new(), Authorization::default(), ledger.clone());
    state.compose = true;
    state.history.push(HistoryEntry::new(Some(Operation::Read), None, StepOutcome::Executed, false));
    state.unchanged = 2;
    state.landing = Some(None);
    let browser = Arc::new(Ledger::new(Limits::default()));
    let carried = state.carried_on(browser.clone());
    assert!(carried.compose && carried.history.len() == 1);
    assert!(carried.unchanged == 0 && carried.landing.is_none());
    assert!(Arc::ptr_eq(&carried.ledger, &browser));
}

#[tokio::test]
async fn test_http_error_visit_cannot_evidence_navigation_completion() {
    let agent = agent();
    let mut state = run_state();
    let front = observation(&[button("Discussion")]);
    let failed =
        Observation { url: "https://example.test/discussion".into(), response_status: Some(419), ..front.clone() };
    state.visited.insert(front.url.clone(), ());
    state.visited.insert(failed.url.clone(), ());
    state.history.push(executed(Some(Operation::Click), Some("Discussion"), true));
    state.acted_from = Some(front);
    agent.note_effect(&mut state, &failed);
    assert!(!state.visited.contains_key(&failed.url));
    assert!(state.history.last().unwrap().effect.as_deref().unwrap_or_default().contains("HTTP 419"));
    agent.note_effect(&mut state, &Observation { response_status: Some(200), ..failed.clone() });
    assert!(state.visited.contains_key(&failed.url));
}

fn capacities() -> (Fact, Fact) {
    let page = capture(&[(BlockKind::Paragraph, "Pine capacity 10"), (BlockKind::Paragraph, "Oak capacity 7")]);
    let pine = Fact::read("Pine capacity 10", block_evidence(&page, "s0"), FactReader::JevChoice);
    let oak = Fact::read("Oak capacity 7", block_evidence(&page, "s1"), FactReader::JevChoice);
    (pine, oak)
}

#[test]
fn test_a_new_summary_of_held_quotes_is_not_a_new_source() {
    let (pine, oak) = capacities();
    let mut notes = Notes::new([pine.clone(), oak]);
    let before = read_sources(&notes);
    notes.add(Fact { text: "Pine has greater capacity.".into(), ..pine });
    assert_eq!(read_sources(&notes), before);
    let third = capture(&[(BlockKind::Paragraph, "Third capacity 12")]);
    notes.add(Fact::read("Third capacity 12", block_evidence(&third, "s0"), FactReader::JevChoice));
    assert_ne!(read_sources(&notes), before);
}

#[test]
fn test_a_source_keeps_its_identifying_context_without_unrelated_capture_changes() {
    let page = capture(&[(BlockKind::Paragraph, "12")]);
    let quoted = block_evidence(&page, "s0");
    let fact = Fact::read("12", quoted.clone(), FactReader::JevChoice);
    let before = read_sources(&Notes::new([fact.clone()]));
    let headed = Evidence { heading_path: vec!["Admission price".into()], ..quoted };
    let contextual = Fact { evidence: Some(headed.clone()), ..fact };
    let changed = read_sources(&Notes::new([contextual.clone()]));
    assert_ne!(changed, before);
    let redraw =
        Fact { evidence: Some(Evidence { capture_sha256: "unrelated-redraw".into(), ..headed }), ..contextual };
    assert_eq!(read_sources(&Notes::new([redraw])), changed);
}

// -- The one stall rule and the one budget-renewal rule -------------------------------------------------------------

const FILES: &str = "https://addons.example.test/project/files";
const DOWNLOAD: &str = "https://addons.example.test/project/download/1";

fn typed_into(label: &str) -> HistoryEntry {
    HistoryEntry::new(Some(Operation::Fill), Some(label), StepOutcome::Executed, false)
}

fn holding(label: &str, value: &str) -> Control {
    Control { value: Some(value.into()), ..Control::new("f", "textbox", label, [Operation::Fill]) }
}

#[test]
fn test_a_typed_value_stays_unsent_through_reads_and_settings() {
    let page = observation(&[holding("Reference", "REF-1"), holding("Note", "")]);
    let read = HistoryEntry::new(Some(Operation::Read), None, StepOutcome::Executed, false);
    let tick = HistoryEntry {
        setting: Some(true),
        ..HistoryEntry::new(Some(Operation::Click), Some("Urgent"), StepOutcome::Executed, true)
    };
    let refused = HistoryEntry::new(Some(Operation::Done), None, StepOutcome::Failed, false);
    let history = [typed_into("Reference"), typed_into("Note"), tick, read, refused];
    let fact = unsent_fact(&history, &page).expect("a fact");
    assert!(fact.contains("into Reference:"), "{fact}");
}

#[test]
fn test_an_action_after_the_typing_may_have_sent_it() {
    let page = observation(&[holding("Reference", "REF-1")]);
    let pressed = HistoryEntry::new(Some(Operation::Click), Some("Send"), StepOutcome::Executed, false);
    assert_eq!(unsent_fact(&[typed_into("Reference"), pressed.clone()], &page), None);
    let failed = HistoryEntry { outcome: StepOutcome::Failed, ..pressed };
    assert!(unsent_fact(&[typed_into("Reference"), failed], &page).is_some());
}

fn buttons_at(url: &str, labels: &[&str]) -> Observation {
    Observation { url: url.into(), ..observation(&labels.iter().map(|label| button(label)).collect::<Vec<_>>()) }
}

/// A run on the file list that spent one recovery there, then clicked through to the download page.
async fn left_the_list() -> (Agent, RunState) {
    let mut state = run_state();
    let agent = agent();
    agent.settle(&mut state, &buttons_at(FILES, &["Files", "v1.14.1"])).await.expect("settled");
    state.recoveries = 1;
    state.landing = Some(Some((Operation::Click, Some("v1.14.1".into()))));
    agent.settle(&mut state, &buttons_at(DOWNLOAD, &["Back to files"])).await.expect("settled");
    (agent, state)
}

#[tokio::test]
async fn test_a_page_state_never_seen_is_progress_and_renews_recovery() {
    let (_, state) = left_the_list().await;
    assert_eq!((state.unchanged, state.recoveries), (0, 0));
}

#[tokio::test]
async fn test_going_back_to_a_page_state_with_nothing_read_since_is_no_progress() {
    let (agent, mut state) = left_the_list().await;
    state.recoveries = 1;
    state.landing = Some(Some((Operation::Click, Some("Back to files".into()))));
    agent.settle(&mut state, &buttons_at(FILES, &["Files", "v1.14.1"])).await.expect("settled");
    assert_eq!((state.unchanged, state.recoveries), (1, 1));
}

#[tokio::test]
async fn test_going_back_to_a_page_state_after_new_evidence_is_progress() {
    let (agent, mut state) = left_the_list().await;
    state.unchanged = 1;
    state.evidence_reads += 1;
    state.landing = Some(Some((Operation::Click, Some("Back to files".into()))));
    agent.settle(&mut state, &buttons_at(FILES, &["Files", "v1.14.1"])).await.expect("settled");
    assert_eq!(state.unchanged, 0);
}

/// A page with no browser behind it, which cannot open the address it is given.
struct Unfetchable {
    opens: bool,
}

impl Page for Unfetchable {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        Box::pin(async { Err(BrowserError::NeedsBrowser("no page has been fetched yet".into())) })
    }

    fn document_changed<'a>(&'a self, _: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(true) })
    }

    fn navigate<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        let opens = self.opens;
        Box::pin(async move {
            if opens {
                return Ok(());
            }
            Err(BrowserError::NeedsBrowser("the page renders its content with scripts".into()))
        })
    }

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        Box::pin(async { Err(BrowserError::NeedsBrowser("no page has been fetched yet".into())) })
    }

    fn act<'a>(&'a self, _: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        unreachable!("no page opened, so no step acts")
    }

    fn redrawn<'a>(&'a self, _: &'a Observation, _: f64, _: Option<&'a str>) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        Box::pin(async { Err(BrowserError::NeedsBrowser("no pixels".into())) })
    }

    fn withhold_frames(&self, _: bool) {}

    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        Box::pin(async { Err(BrowserError::NeedsBrowser("no page has been fetched yet".into())) })
    }

    fn response_status(&self) -> BoxFuture<'_, PageResult<Option<i64>>> {
        Box::pin(async { Ok(None) })
    }

    fn lifecycle_mark(&self) -> u64 {
        0
    }

    fn wait_for_lifecycle(&self, _: u64, _: f64) -> BoxFuture<'_, bool> {
        Box::pin(async { false })
    }
}

#[tokio::test]
async fn test_a_page_that_needs_a_browser_ends_the_run_blocked_with_its_reason() {
    let agent = Agent::new(
        Arc::new(Unfetchable { opens: false }),
        Arc::new(ScriptedJev::new(Vec::new())),
        Arc::new(ScriptedLLM),
    );
    let options = super::RunOptions { start: Some("https://example.test/".into()), ..Default::default() };
    let result = agent.run("Read the price", options).await.expect("a result, not an error");
    assert_eq!(result.status, Status::Blocked);
    assert_eq!(result.error.as_deref(), Some("the page renders its content with scripts"));
    assert!(result.steps.is_empty() && result.evidence.is_empty() && result.final_url.is_none());
}

#[tokio::test]
async fn test_page_ready_is_told_once_the_opening_page_is_open_and_only_then() {
    for (opens, told) in [(true, 1), (false, 0)] {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = calls.clone();
        let agent =
            Agent::new(Arc::new(Unfetchable { opens }), Arc::new(ScriptedJev::new(Vec::new())), Arc::new(ScriptedLLM))
                .with_page_ready(Some(Arc::new(move || {
                    let counter = counter.clone();
                    Box::pin(async move {
                        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    })
                })));
        let options = super::RunOptions { start: Some("https://example.test/".into()), ..Default::default() };
        let _ = agent.run("Read the price", options).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), told);
    }
}

#[test]
fn test_the_trace_keeps_each_page_a_step_saw_without_its_secrets() {
    let trace = Trace::collecting();
    let agent = agent().with_trace(trace.clone());
    agent.redactor().register("password", "hunter2", Some("https://acme.test"));
    let seen = Observation { viewport_text: "Signed in with hunter2".into(), ..at("https://acme.test/a", &[]) };
    agent.record_page(&run_state(), &seen);
    let events = trace.events();
    assert_eq!(events[0]["event"], "page");
    assert_eq!(events[0]["url"], "https://acme.test/a");
    assert_eq!(events[0]["text"], "Signed in with [secret:password]");
}

#[test]
fn test_a_stop_at_a_wall_names_what_the_wall_asks_for() {
    use super::state::Stop;
    use fastbrowse_core::models::WallKind;
    let mut code = Control::new("otp", "textbox", "Code from hunter2", [Operation::Fill]);
    code.autocomplete = Some("one-time-code".into());
    let mut password = Control::new("pw", "textbox", "Password", [Operation::Fill]);
    password.input_type = Some("password".into());
    let stop = |status: Status, wall: Option<WallKind>| Stop { wall, ..Stop::new(status, "stopped") };
    for (controls, status, hint, expected) in [
        (vec![code.clone()], Status::NeedsLogin, None, Some(WallKind::OneTimeCode)),
        (vec![code.clone(), password.clone()], Status::NeedsLogin, None, Some(WallKind::Password)),
        (vec![button("Sign in")], Status::NeedsLogin, None, Some(WallKind::SignIn)),
        (vec![code.clone()], Status::Blocked, Some(WallKind::BotCheck), Some(WallKind::BotCheck)),
        // A refusal that is not a bot check, and a run that stopped for any other reason, name no wall.
        (vec![code.clone()], Status::Blocked, None, None),
        (vec![code.clone()], Status::Stuck, None, None),
    ] {
        let agent = agent();
        agent.redactor().register("PASSWORD", "hunter2", None);
        agent.seen().raw = Some(at("https://shop.test:443/login?next=1", &controls));
        let wall = agent.wall(&stop(status, hint));
        assert_eq!(wall.as_ref().map(|w| w.kind), expected, "{status:?} {controls:?}");
        let Some(wall) = wall else { continue };
        assert_eq!(wall.origin, "https://shop.test");
        assert!(wall.seen_at_ms > 1_700_000_000_000);
        let fields: Vec<String> = match wall.kind {
            WallKind::BotCheck | WallKind::SignIn => Vec::new(),
            WallKind::OneTimeCode => vec!["Code from [secret:PASSWORD]".into()],
            WallKind::Password => vec!["Code from [secret:PASSWORD]".into(), "Password".into()],
        };
        assert_eq!(wall.fields, fields);
    }
}

/// Declares a password for the site and then cannot produce it, as a vault that is locked does.
struct Unresolved;

impl fastbrowse_core::models::SecretResolver for Unresolved {
    fn available(&self) -> Vec<fastbrowse_core::models::SecretRef> {
        vec![fastbrowse_core::models::SecretRef { name: "password".into(), origins: vec!["https://b.test".into()] }]
    }

    fn resolve<'a>(&'a self, _: &'a str, _: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async { None })
    }
}

#[tokio::test]
async fn test_a_credential_field_its_secret_cannot_fill_stops_needs_login_not_needs_input() {
    let mut password = Control::new("pw", "textbox", "Password", [Operation::Fill]);
    password.input_type = Some("password".into());
    password.sensitive = true;
    password.frame_origin = Some("https://b.test".into());
    let agent = agent().with_secrets(Some(Arc::new(Unresolved)));
    let page = at("https://b.test/login", std::slice::from_ref(&password));
    agent.seen().raw = Some(page.clone());
    let mut state = run_state();
    let Err(super::state::Flow::Stop(stop)) =
        agent.text(&mut state, &page, &code_decision(Operation::Fill, None), &password).await
    else {
        panic!("the field was filled");
    };
    assert_eq!(stop.status, Status::NeedsLogin);
    assert_eq!(
        stop.error.as_deref(),
        Some("sign-in required at https://b.test: secret password is not available there")
    );
}

/// A run is replayed from its result's steps, the step events a host shows as it goes, and the trace file, and a step's
/// builder or an event's author, in the loop or anywhere it hands its trace, that forgets to redact must not put a
/// resolved secret into any of them.
#[tokio::test]
async fn test_a_secret_in_a_step_or_trace_event_is_redacted_on_the_way_out() {
    let events: Arc<std::sync::Mutex<Vec<Event>>> = Arc::default();
    let sink = events.clone();
    let on_event: EventHandler = Arc::new(move |event: Event| {
        sink.lock().unwrap().push(event);
        Box::pin(async {})
    });
    let trace = Trace::collecting();
    let agent = agent().with_events(Some(on_event)).with_trace(trace.clone());
    agent.redactor().register("password", "hunter2", Some("https://acme.test"));
    let mut state = run_state();
    let step = StepResult {
        index: 0,
        operation: Operation::Done,
        decided_by: Decider::Jev,
        outcome: StepOutcome::Failed,
        url: "https://acme.test/account?pw=hunter2".into(),
        target: Some("Signed in with hunter2".into()),
        confidence: None,
        note: Some("DONE rejected: Answer output not verified: hunter2".into()),
        facts: Vec::new(),
        page_changed: None,
        duration_ms: 0,
        choices: Vec::new(),
    };
    // Choices taken before the secret was registered are masked as the step is recorded.
    state.choices = vec![fastbrowse_core::models::StepChoice {
        id: "a".into(),
        role: "textbox".into(),
        label: "Account hunter2".into(),
        context: Some("signed in as hunter2".into()),
        operations: vec![Operation::Fill],
        chosen: true,
    }];
    agent.record_step(&mut state, step, false, None).await.unwrap();
    agent.event("read", || serde_json::Map::from_iter([("quotes".into(), json!(["typed hunter2", 3]))]));
    // Readers and checks trace through a clone of the run's trace, never through the agent.
    agent
        .trace
        .clone()
        .event("record_filter", || serde_json::Map::from_iter([("requirement".into(), "hunter2".into())]));
    let Some(Event::Step(event)) = events.lock().unwrap().pop() else { panic!("no step event") };
    let redacted = StepResult {
        url: "https://acme.test/account?pw=[secret:password]".into(),
        target: Some("Signed in with [secret:password]".into()),
        note: Some("DONE rejected: Answer output not verified: [secret:password]".into()),
        ..event.step.clone()
    };
    let masked = "\u{2022}".repeat(7);
    assert_eq!(event.step.choices[0].label, format!("Account {masked}"));
    assert_eq!(event.step.choices[0].context.as_deref(), Some(format!("signed in as {masked}").as_str()));
    assert_eq!(event.step, redacted);
    assert_eq!(state.steps, vec![redacted]);
    assert_eq!(
        trace.events(),
        vec![
            json!({"event": "read", "quotes": ["typed [secret:password]", 3]}),
            json!({"event": "record_filter", "requirement": "[secret:password]"}),
        ]
    );
}

// -- The one rule for an action that may commit ------------------------------------------------------------------

/// A Jev that answers `irreversible` and `requested` as given, and counts its calls.
struct Gatekeeper {
    irreversible: f64,
    requested: f64,
    calls: std::sync::atomic::AtomicUsize,
}

impl fastbrowse_core::decision::DecisionClient for Gatekeeper {
    fn evaluate<'a>(
        &'a self,
        _: &'a serde_json::Value,
        questions: &'a IndexMap<String, fastbrowse_core::decision::Question>,
    ) -> BoxFuture<'a, Result<fastbrowse_core::decision::Evaluation, fastbrowse_core::decision::DecisionError>> {
        use fastbrowse_core::decision::{Answer, NoulAnswer};
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let answers = questions
            .keys()
            .map(|key| {
                let probability = if key == "requested" { self.requested } else { self.irreversible };
                (key.clone(), Answer::Noul(NoulAnswer { kind: Default::default(), probability }))
            })
            .collect();
        Box::pin(async move {
            Ok(fastbrowse_core::decision::Evaluation {
                model: "jev".into(),
                answers,
                input_tokens: 0,
                cost: free(),
                requests: 1,
            })
        })
    }

    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        ScriptedJev::new(Vec::new()).profile()
    }
}

#[tokio::test]
async fn test_a_commit_proceeds_when_jev_calls_it_reversible_or_the_authorized_task_asks_for_it() {
    use fastbrowse_core::safety::{control_action, irreversible_question, requested_question};
    use std::sync::atomic::Ordering;
    // (authorized, a credential change, irreversible, requested, acts, Jev calls)
    for (authorized, credential, irreversible, requested, acts, calls) in [
        (false, false, 0.10, 0.0, true, 1),
        (false, false, 0.95, 0.97, false, 1),
        (true, false, 0.95, 0.97, true, 1),
        (true, false, 0.95, 0.76, true, 1),
        (true, false, 0.95, 0.40, false, 1),
        // A credential change is never taken on Jev's word in an unauthorized run, and it is not even asked.
        (false, true, 0.10, 0.97, false, 0),
        (true, true, 0.10, 0.97, true, 1),
        (true, true, 0.10, 0.40, false, 1),
    ] {
        let jev = Arc::new(Gatekeeper { irreversible, requested, calls: Default::default() });
        let agent = Agent::new(Arc::new(NoPage), jev.clone(), Arc::new(ScriptedLLM));
        let mut state = run_state();
        state.task = "Check out and finish the order.".into();
        state.authorization = Authorization { irreversible_actions: authorized };
        let finish = button("Finish");
        let observation = observation(std::slice::from_ref(&finish));
        let decision = Decision { operation_confidence: 0.86, ..code_decision(Operation::Click, Some(finish.clone())) };
        let question = (!credential).then(|| irreversible_question(Operation::Click, &finish, None));
        let asked = requested_question(&control_action(Operation::Click, &finish, &[]));
        let gated = agent.gate_question(&mut state, &observation, &decision, "Finish", question, asked).await;
        let case = format!("authorized {authorized}, credential {credential}, requested {requested}");
        assert_eq!(gated.is_ok(), acts, "{case}");
        assert_eq!(jev.calls.load(Ordering::SeqCst), calls, "{case}");
        // A refusal stops the run for confirmation rather than going round recovery.
        if let Err(flow) = gated {
            assert!(matches!(flow, Flow::Stop(stop) if stop.status == Status::NeedsConfirmation), "{case}");
        }
    }
}

/// The page used by the finish-path test, whose read has already supplied the cited requirement claim.
struct FinishPage(Observation);

impl Page for FinishPage {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        let observation = self.0.clone();
        Box::pin(async move { Ok(observation) })
    }

    fn document_changed<'a>(&'a self, _: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn navigate<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        unreachable!("the finish-path test does not navigate")
    }

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        unreachable!("the finish-path test does not capture")
    }

    fn act<'a>(&'a self, _: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        unreachable!("the finish-path test does not act")
    }

    fn redrawn<'a>(&'a self, _: &'a Observation, _: f64, _: Option<&'a str>) -> BoxFuture<'a, PageResult<bool>> {
        unreachable!("the finish-path test does not watch")
    }

    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        unreachable!("the finish-path test does not take a screenshot")
    }

    fn withhold_frames(&self, _: bool) {}

    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        unreachable!("the finish-path test does not ask for the address")
    }

    fn response_status(&self) -> BoxFuture<'_, PageResult<Option<i64>>> {
        Box::pin(async { Ok(None) })
    }

    fn lifecycle_mark(&self) -> u64 {
        0
    }

    fn wait_for_lifecycle(&self, _: u64, _: f64) -> BoxFuture<'_, bool> {
        Box::pin(async { false })
    }
}

/// Accepts the draft made from a read claim and counts the done-check requests.
struct DoneCheckJev(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl DecisionClient for DoneCheckJev {
    fn evaluate<'a>(
        &'a self,
        _: &'a serde_json::Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let answers = questions
            .keys()
            .map(|key| (key.clone(), Answer::Noul(NoulAnswer { kind: Default::default(), probability: 0.0 })))
            .collect();
        Box::pin(
            async move { Ok(Evaluation { model: "jev".into(), answers, input_tokens: 0, cost: free(), requests: 1 }) },
        )
    }

    fn profile(&self) -> DecisionProfile {
        ScriptedJev::new(Vec::new()).profile()
    }
}

#[tokio::test]
async fn test_a_done_answers_from_the_notes_with_no_further_question() {
    let page = capture(&[(BlockKind::Paragraph, "The answer is 42")]);
    let fact = Fact::read("The answer is 42", block_evidence(&page, "s0"), FactReader::JevChoice).answering();
    let mut state = run_state();
    state.task = "Report the answer.".into();
    state.notes = Notes::new([fact]);
    state.history.push(crate::policy::HistoryEntry::new(Some(Operation::Click), None, StepOutcome::Executed, true));
    let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let agent = Agent::new(
        Arc::new(FinishPage(observation(&[]))),
        Arc::new(DoneCheckJev(requests.clone())),
        Arc::new(ScriptedLLM),
    );

    let page = observation(&[]);
    let result = agent.finish(&mut state, &page, None, None).await.expect("finish").expect("a completed result");
    assert_eq!(result.status, Status::Complete);
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(result.answer.as_deref().is_some_and(|answer| answer.contains("The answer is 42")));
}

/// Picks, for every read requirement, the candidate whose value is the field's, and fails if it is not offered; with no
/// field, it says the page holds nothing for the requirement. It says yes to the actions' context question with the given
/// probability.
struct FieldReader(Option<&'static str>, f64);

impl fastbrowse_core::decision::DecisionClient for FieldReader {
    fn evaluate<'a>(
        &'a self,
        _: &'a serde_json::Value,
        questions: &'a IndexMap<String, fastbrowse_core::decision::Question>,
    ) -> BoxFuture<'a, Result<fastbrowse_core::decision::Evaluation, fastbrowse_core::decision::DecisionError>> {
        use fastbrowse_core::decision::{Answer, ChoiceAnswer, NoulAnswer, Question};
        let answers = questions
            .iter()
            .map(|(key, question)| {
                let question = match question {
                    Question::Choice(question) => question,
                    Question::Noul(_) => {
                        let answer = NoulAnswer { kind: Default::default(), probability: self.1 };
                        return (key.clone(), Answer::Noul(answer));
                    }
                    _ => panic!("{key} is neither a read choice nor the context question"),
                };
                let wanted = |offered: &serde_json::Value| match self.0 {
                    Some(field) => offered.get("value").and_then(serde_json::Value::as_str) == Some(field),
                    None => offered.as_str().is_some_and(|text| text.starts_with("This page contains no evidence")),
                };
                let (choice, _) = question
                    .criteria
                    .iter()
                    .find(|(_, offered)| wanted(offered))
                    .unwrap_or_else(|| panic!("{:?} is not offered: {:?}", self.0, question.criteria));
                let probabilities =
                    question.criteria.keys().map(|c| (c.clone(), f64::from(u8::from(c == choice)))).collect();
                let answer =
                    ChoiceAnswer { kind: Default::default(), choice: choice.clone(), probabilities, confidence: 0.97 };
                (key.clone(), Answer::Choice(answer))
            })
            .collect();
        Box::pin(async move {
            Ok(fastbrowse_core::decision::Evaluation {
                model: "jev".into(),
                answers,
                input_tokens: 0,
                cost: free(),
                requests: 1,
            })
        })
    }

    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        ScriptedJev::new(Vec::new()).profile()
    }
}

/// Answers every question with `pick`, or leaves them unanswered when there is none.
struct Answers(Option<&'static str>);

impl DecisionClient for Answers {
    fn evaluate<'a>(
        &'a self,
        _: &'a serde_json::Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        let answers = self
            .0
            .iter()
            .flat_map(|pick| {
                let answer = ChoiceAnswer {
                    kind: Default::default(),
                    choice: (*pick).into(),
                    probabilities: IndexMap::new(),
                    confidence: 1.0,
                };
                questions.keys().map(move |key| (key.clone(), Answer::Choice(answer.clone())))
            })
            .collect();
        Box::pin(
            async move { Ok(Evaluation { model: "jev".into(), answers, input_tokens: 0, cost: free(), requests: 1 }) },
        )
    }

    fn profile(&self) -> DecisionProfile {
        ScriptedJev::new(Vec::new()).profile()
    }
}

#[tokio::test]
async fn test_a_choice_jev_was_not_offered_is_a_failed_decision_and_not_an_empty_value() {
    let agent = Agent::new(Arc::new(NoPage), Arc::new(Answers(Some("input:nope"))), Arc::new(ScriptedLLM));
    let mut state = run_state();
    state.inputs.insert("name".into(), "Ada".into());
    let page = observation(&[]);
    let error = agent
        .text(&mut state, &page, &code_decision(Operation::Fill, None), &field("Name"))
        .await
        .expect_err("an unusable pick");
    assert!(format!("{error:?}").contains("which was not offered"), "{error:?}");
}

#[tokio::test]
async fn test_jev_reads_a_field_value_on_a_task_that_also_acts() {
    let field = "Date Picker 2: 27/12/2026";
    let agent = Agent::new(Arc::new(NoPage), Arc::new(FieldReader(Some(field), 0.1)), Arc::new(ScriptedLLM));
    let mut state = run_state();
    state.task = "Pick the last Sunday of December 2026 in Date Picker 2 and report the date shown there.".into();
    // How a capture states a form field: its label, then the value it holds.
    let page = capture(&[
        (BlockKind::Heading, "Date Picker"),
        (BlockKind::Paragraph, "Date Picker 1: 01/01/2026"),
        (BlockKind::Paragraph, field),
    ]);
    let added = agent.read(&mut state, page, &observation(&[]), false).await.expect("a read");
    assert!(added);
    let fact = state.notes.facts().into_iter().find(|f| f.answers).expect("a fact");
    assert_eq!(fact.reader, FactReader::JevChoice);
    assert_eq!(fact.text, field);
    assert_eq!(fact.evidence.as_ref().map(|e| e.quote.as_str()), Some(field));
}

// -- A stall is put to Jev again, with what went wrong -----------------------------------------------------------

/// A page whose uses are its 1280x900 viewport screenshot and the points clicked on it, and that has moved on from
/// the observation asked about when its flag is set.
#[derive(Default)]
struct Screenshots(std::sync::Mutex<Vec<(f64, f64)>>, bool);

/// A PNG's signature and header, which is all of it the image limits read.
fn png(width: u32, height: u32) -> Vec<u8> {
    [&b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR"[..], &width.to_be_bytes(), &height.to_be_bytes()].concat()
}

impl Page for Screenshots {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        unreachable!("recovery does not observe")
    }

    fn document_changed<'a>(&'a self, _: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        let moved = self.1;
        Box::pin(async move { Ok(moved) })
    }

    fn navigate<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        unreachable!("recovery does not navigate")
    }

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        unreachable!("recovery does not capture")
    }

    fn act<'a>(&'a self, _: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        unreachable!("recovery does not act")
    }

    fn redrawn<'a>(&'a self, _: &'a Observation, _: f64, _: Option<&'a str>) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        Box::pin(async { Ok(png(1280, 900)) })
    }

    fn click_viewport(&self, x: f64, y: f64) -> BoxFuture<'_, PageResult<ActResult>> {
        self.0.lock().unwrap().push((x, y));
        let clicked = ActResult { outcome: StepOutcome::Executed, page_changed: true, detail: None, moved: false };
        Box::pin(async move { Ok(clicked) })
    }

    fn withhold_frames(&self, _: bool) {}

    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        unreachable!("recovery does not ask the address")
    }

    fn response_status(&self) -> BoxFuture<'_, PageResult<Option<i64>>> {
        unreachable!("recovery does not ask the status")
    }

    fn lifecycle_mark(&self) -> u64 {
        0
    }

    fn wait_for_lifecycle(&self, _: u64, _: f64) -> BoxFuture<'_, bool> {
        Box::pin(async { true })
    }
}

/// The scripted Jev, its every choice held at `confidence`, and the states it was shown.
struct Unsure(ScriptedJev, f64, std::sync::Mutex<Vec<serde_json::Value>>);

impl fastbrowse_core::decision::DecisionClient for Unsure {
    fn evaluate<'a>(
        &'a self,
        state: &'a serde_json::Value,
        questions: &'a IndexMap<String, fastbrowse_core::decision::Question>,
    ) -> BoxFuture<'a, Result<fastbrowse_core::decision::Evaluation, fastbrowse_core::decision::DecisionError>> {
        use fastbrowse_core::decision::Answer;
        self.2.lock().unwrap().push(state.clone());
        let answered = self.0.evaluate(state, questions);
        Box::pin(async move {
            let mut evaluation = answered.await?;
            for answer in evaluation.answers.values_mut() {
                if let Answer::Choice(choice) = answer {
                    choice.confidence = self.1;
                }
            }
            Ok(evaluation)
        })
    }

    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        self.0.profile()
    }
}

/// Recovery after a click on Apply changed nothing, put to a Jev that would click Next at `confidence`.
async fn stalled_on_apply(confidence: f64) -> (RunState, Arc<Unsure>) {
    let script = vec![crate::scripted::Step { operation: Operation::Click, label: "Next".into(), pick: None }];
    let jev = Arc::new(Unsure(ScriptedJev::new(script), confidence, Default::default()));
    let agent = Agent::new(Arc::new(Screenshots::default()), jev.clone(), Arc::new(ScriptedLLM));
    let mut state = run_state();
    let apply = button("Apply");
    let page = observation(&[apply.clone(), button("Next")]);
    let decision = code_decision(Operation::Click, Some(apply));
    let withheld = super::recovery::withheld(&decision);
    agent.recover(&mut state, &page, "nothing changed", Status::Stuck, withheld).await.expect("recovered");
    (state, jev)
}

#[tokio::test]
async fn test_past_the_recovery_budget_the_run_ends_with_the_reason() {
    let script = vec![crate::scripted::Step { operation: Operation::Click, label: "Next".into(), pick: None }];
    let jev = Arc::new(Unsure(ScriptedJev::new(script), 0.1, Default::default()));
    let agent = Agent::new(Arc::new(Screenshots::default()), jev, Arc::new(ScriptedLLM));
    let mut state = run_state();
    state.recoveries = agent.config.stall.max_recoveries;
    let apply = button("Apply");
    let page = observation(&[apply.clone(), button("Next")]);
    let decision = code_decision(Operation::Click, Some(apply));
    let withheld = super::recovery::withheld(&decision);
    let error = agent.recover(&mut state, &page, "nothing changed", Status::Stuck, withheld).await.err();
    assert!(matches!(error, Some(Flow::Stop(stop)) if stop.error.as_deref() == Some("nothing changed")));
}

#[tokio::test]
async fn test_a_stall_is_answered_by_jev_even_when_it_is_unsure() {
    let (state, jev) = stalled_on_apply(0.3).await;
    let (_, picked) = state.picked.expect("Jev's pick is the next decision");
    assert_eq!((picked.operation, picked.target.map(|c| c.label)), (Operation::Click, Some("Next".to_string())));
    // Jev was told what failed, and the click that changed nothing was not among its options.
    let shown = jev.2.lock().unwrap()[0].to_string();
    assert!(shown.contains("did not move the task on from this page"), "{shown}");
    assert!(!shown.contains("\"Apply\""), "{shown}");
}

// -- A covered click is put to Jev's eyes: pressed where the screenshot shows the control, only when Jev is sure ----

/// A Jev that is sent images when `images`, answers that the control is shown with `shown`, finds it in the box from
/// 0.2 to 0.4 across and 0.5 to 0.7 down, and keeps what it was asked.
struct Seeing {
    images: bool,
    shown: f64,
    asked: std::sync::Mutex<Vec<Asked>>,
}

/// A state, its questions and the images it refers to.
type Asked = (serde_json::Value, IndexMap<String, Question>, Images);

impl DecisionClient for Seeing {
    fn evaluate<'a>(
        &'a self,
        _: &'a serde_json::Value,
        _: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        unreachable!("a covered control is asked about with its screenshot")
    }

    fn evaluate_images<'a>(
        &'a self,
        state: &'a serde_json::Value,
        questions: &'a IndexMap<String, Question>,
        images: &'a Images,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        self.asked.lock().unwrap().push((state.clone(), questions.clone(), images.clone()));
        let answers = IndexMap::from([
            ("shown".to_string(), Answer::Noul(NoulAnswer { kind: Default::default(), probability: self.shown })),
            (
                "where".to_string(),
                Answer::Box(BoxAnswer {
                    kind: Default::default(),
                    edges: Edges { top: 0.5, bottom: 0.7, left: 0.2, right: 0.4 },
                    confidence: Some(0.9),
                }),
            ),
        ]);
        let cost = CostLine::new(CostComponent::Jev, CostBasis::Estimated, Some(0.0));
        Box::pin(async move { Ok(Evaluation { model: "test".into(), answers, input_tokens: 1, requests: 1, cost }) })
    }

    fn profile(&self) -> DecisionProfile {
        DecisionProfile { images: self.images, ..DecisionProfile::jev() }
    }
}

async fn click_where_seen(images: bool, shown: f64, moved: bool) -> (Option<ActResult>, Arc<Screenshots>, Arc<Seeing>) {
    let page = Arc::new(Screenshots(Default::default(), moved));
    let jev = Arc::new(Seeing { images, shown, asked: Default::default() });
    let agent = Agent::new(page.clone(), jev.clone(), Arc::new(ScriptedLLM));
    let mut state = run_state();
    let target = button("Pick a date");
    let seen = observation(std::slice::from_ref(&target));
    let clicked = agent.click_where_seen(&mut state, &seen, &target).await.expect("asked");
    (clicked, page, jev)
}

#[tokio::test]
async fn test_a_covered_control_jev_sees_is_clicked_at_the_centre_of_its_box() {
    let (clicked, page, jev) = click_where_seen(true, 0.9, false).await;
    assert_eq!(clicked.map(|result| result.outcome), Some(StepOutcome::Executed));
    let (x, y) = page.0.lock().unwrap()[0];
    assert!((x - 0.3).abs() < 1e-9 && (y - 0.6).abs() < 1e-9, "{x}, {y}");
    let asked = jev.asked.lock().unwrap();
    let (state, questions, images) = &asked[0];
    let reference = state["viewport_screenshot"].as_str().unwrap();
    assert_eq!(images.get(reference), Some(&png(1280, 900)));
    assert_eq!(state["control"]["label"], "Pick a date");
    assert!(matches!(questions["shown"], Question::Noul(_)) && matches!(questions["where"], Question::Box(_)));
}

#[tokio::test]
async fn test_a_covered_control_jev_doubts_or_cannot_see_is_not_clicked() {
    let (clicked, page, _) = click_where_seen(true, 0.2, false).await;
    assert!(clicked.is_none() && page.0.lock().unwrap().is_empty());
    let (clicked, page, jev) = click_where_seen(false, 0.9, false).await;
    assert!(clicked.is_none() && page.0.lock().unwrap().is_empty() && jev.asked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn test_a_covered_control_is_not_clicked_once_the_page_moved_on_from_its_screenshot() {
    let (clicked, page, jev) = click_where_seen(true, 0.9, true).await;
    assert!(clicked.is_none() && page.0.lock().unwrap().is_empty() && !jev.asked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn test_a_gate_answered_with_the_decision_asks_jev_nothing_more() {
    use std::sync::atomic::Ordering;

    use fastbrowse_core::safety::{control_action, irreversible_question, requested_question};

    let jev = Arc::new(Gatekeeper { irreversible: 0.1, requested: 0.1, calls: Default::default() });
    let agent = Agent::new(Arc::new(NoPage), jev.clone(), Arc::new(ScriptedLLM));
    let mut state = run_state();
    let finish = button("Finish");
    let page = observation(std::slice::from_ref(&finish));
    let reversible = Answer::Noul(NoulAnswer { kind: Default::default(), probability: 0.1 });
    let decision = Decision {
        ahead: IndexMap::from([("irreversible".to_string(), reversible)]),
        ..code_decision(Operation::Click, Some(finish.clone()))
    };
    let question = Some(irreversible_question(Operation::Click, &finish, None));
    let asked = requested_question(&control_action(Operation::Click, &finish, &[]));
    agent.gate_question(&mut state, &page, &decision, "Finish", question, asked).await.expect("a reversible click");
    assert_eq!(jev.calls.load(Ordering::SeqCst), 0);
}

/// A Jev that says the notes state the answer and picks only the note holding `winner`.
struct Verdict(&'static str);

impl DecisionClient for Verdict {
    fn evaluate<'a>(
        &'a self,
        state: &'a serde_json::Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        let answers = questions
            .keys()
            .map(|key| {
                let answer = match key.strip_prefix("note") {
                    Some(index) => {
                        let note = state["notes"][index.parse::<usize>().unwrap()].as_str().unwrap();
                        let probability = if note.contains(self.0) { 0.9 } else { 0.1 };
                        Answer::Noul(NoulAnswer { kind: Default::default(), probability })
                    }
                    None => Answer::Choice(ChoiceAnswer {
                        kind: Default::default(),
                        choice: "quote".into(),
                        probabilities: IndexMap::new(),
                        confidence: 0.9,
                    }),
                };
                (key.clone(), answer)
            })
            .collect();
        Box::pin(
            async move { Ok(Evaluation { model: "jev".into(), answers, input_tokens: 0, cost: free(), requests: 1 }) },
        )
    }

    fn profile(&self) -> DecisionProfile {
        ScriptedJev::new(Vec::new()).profile()
    }
}

#[tokio::test]
async fn test_a_winner_jev_picks_from_the_notes_needs_no_composing() {
    let page = capture(&[(BlockKind::Paragraph, "Shop A 84.99"), (BlockKind::Paragraph, "Shop B 92.50")]);
    let facts = ["s0", "s1"].map(|id| {
        let text = if id == "s0" { "Shop A 84.99" } else { "Shop B 92.50" };
        Fact::read(text, block_evidence(&page, id), FactReader::JevChoice).answering()
    });
    let answer = crate::retrieval::verdict(&Verdict("Shop A"), "Which is cheaper?", &Notes::new(facts), None)
        .await
        .expect("a verdict")
        .expect("an answer");
    assert!(answer.answer.contains("Shop A") && !answer.answer.contains("Shop B"));
}

#[test]
fn test_a_control_that_added_and_stays_may_hold_more() {
    let click = |added| {
        let entry = HistoryEntry::new(Some(Operation::Click), Some("More"), StepOutcome::Executed, true);
        vec![HistoryEntry { added, ..entry }]
    };
    let page = |label: &str| observation(&[Control::new("1", "button", label, [Operation::Click])]);
    assert!(super::helpers::more_behind_fact(&click(true), &page("More")).is_some());
    assert!(super::helpers::more_behind_fact(&click(true), &page("Other")).is_none());
    assert!(super::helpers::more_behind_fact(&click(false), &page("More")).is_none());
}

#[tokio::test]
async fn test_a_stale_click_on_a_page_that_moved_leaves_the_read_out_of_date() {
    let before = observation(&[button("More")]);
    let after = observation(&[]);
    let agent = agent();
    let mut state = run_state();
    state.read_documents.insert(before.document_key.clone(), false);
    state.history.push(HistoryEntry::new(Some(Operation::Click), Some("More"), StepOutcome::Stale, false));
    state.idle.push(Some((Operation::Click, Some("More".into()))));
    state.acted_from = Some(before.clone());
    agent.note_effect(&mut state, &after);
    assert!(state.idle.is_empty());
    assert_eq!(state.read_documents.get(&after.document_key), Some(&true));
}
