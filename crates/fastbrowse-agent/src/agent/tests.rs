//! tests/test_agent.py's cases, and those of its sibling files, about the loop's pure helpers and small state
//! transitions. Whole runs, and the async methods that need a scripted page and models, are replayed in
//! tests/agent.rs instead.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{
    Answer, BoxAnswer, DecisionClient, DecisionError, DecisionProfile, Edges, Evaluation, Images, NoulAnswer, Question,
};
use fastbrowse_core::llm::Generation;
use fastbrowse_core::models::{
    Artifact, Authorization, CostBasis, CostComponent, CostLine, Decider, Event, EventHandler, Evidence, FactReader,
    Limits, Operation, Status, StepOutcome, StepResult,
};
use fastbrowse_core::page::{
    ActResult, Action, BlockKind, BrowserError, Capture, Control, Observation, Page, PageResult,
};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::safety::Redactor;
use fastbrowse_core::telemetry::{Ledger, Trace};
use indexmap::IndexMap;
use serde_json::json;

use super::Agent;
use super::driving::edit_progress;
use super::helpers::{answer_evidence, follow_recovery, next_value, unread, visited};
use super::reading::{ListRead, leaves_unread_page, list_links};
use super::state::{Flow, RunState, Spawned, TransactionCandidate};
use crate::memory::{Fact, Notes, fact_id};
use crate::planner::{Plan, Requirement, RequirementKind};
use crate::policy::{Decision, HistoryEntry};
use crate::retrieval::testing::capture;
use crate::scripted::{ScriptedJev, ScriptedLLM};

const INFO: RequirementKind = RequirementKind::Information;

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

fn planned(plan: Plan) -> Arc<Spawned<Generation<Plan>>> {
    Arc::new(Spawned::spawn(async move { Ok(Generation { data: plan, cost: free() }) }))
}

/// tests/test_agent.py's `run_state`: a run whose empty plan has already arrived.
async fn run_state() -> RunState {
    let plan = Plan::new(Vec::new(), false);
    let planning = planned(plan.clone());
    planning.wait().await;
    let mut state = RunState::new(
        "Find a train from Bristol to York on 16 October 2026".into(),
        IndexMap::new(),
        Vec::new(),
        Authorization::default(),
        Arc::new(Ledger::new(Limits::default())),
        planning,
        false,
    );
    state.ready_plan = Some(plan);
    state
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

fn link(key: &str, label: &str, href: &str) -> Control {
    let mut control = Control::new(key, "link", label, [Operation::Click]);
    control.href = Some(href.into());
    control
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
        site_failure: None,
        read_assessment: crate::policy::ReadAssessment::Absent,
        directed: false,
        destination: None,
    }
}

/// A link the page marks `rel="next"`.
fn pager(key: &str, label: &str, href: &str) -> Control {
    Control { next_page: Some(true), ..link(key, label, href) }
}

fn draggable() -> Control {
    Control::new("drag1", "generic", "Card A", [Operation::Click, Operation::Drag])
}

/// tests/test_retrieval.py's `block_evidence`: one whole block, as the reader's cite of it copies.
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

fn typed(target: &str, text: &str) -> HistoryEntry {
    HistoryEntry { text: Some(text.into()), ..executed(Some(Operation::Fill), Some(target), false) }
}

fn set(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

#[tokio::test]
async fn test_a_step_abandoned_while_waiting_for_the_plan_leaves_the_plan_to_the_rest_of_the_run() {
    let plan = Plan::new(Vec::new(), false);
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let mut state = run_state().await;
    state.ready_plan = None;
    state.planning = Arc::new(Spawned::spawn({
        let plan = plan.clone();
        async move {
            let _ = released.await;
            Ok(Generation { data: plan, cost: free() })
        }
    }));
    // A redraw drops the waiting step's future mid-wait, as Python cancels its task.
    assert!(tokio::time::timeout(Duration::from_millis(10), state.await_plan()).await.is_err());
    release.send(()).unwrap();
    assert_eq!(state.await_plan().await.unwrap(), plan);
}

#[test]
fn test_an_answer_owed_with_nothing_read_is_unread() {
    let action_only = Plan::new(vec![Requirement::new("r1", "Search for the quote", RequirementKind::Action)], true);
    assert!(unread(&action_only, &Notes::default()));
    assert!(!unread(&Plan { answer_expected: false, ..action_only }, &Notes::default()));
}

#[tokio::test]
async fn test_the_next_observation_records_what_an_action_did() {
    let field_before = field("Search elsewhere");
    let mut state = run_state().await;
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
    let mut state = run_state().await;
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

#[tokio::test]
async fn test_a_drag_named_by_recovery_is_not_built_from_a_choice_with_no_drop_target() {
    let (source, destination) = (draggable(), button("Done"));
    let page = observation(&[source.clone(), destination.clone()]);
    // Jev chose a click on the drop target, so there is no drag destination to build from.
    let decision = code_decision(Operation::Click, Some(destination));
    let mut state = run_state().await;
    state.directed = Some((Operation::Drag, Some(source.id)));
    assert!(follow_recovery(&mut state, &page, &decision, true).is_none());
}

#[tokio::test]
async fn test_a_named_action_is_not_taken_over_a_confident_choice_or_on_a_control_that_went() {
    let search = Control::new("search", "button", "Search", [Operation::Click]);
    let decision = code_decision(Operation::Click, Some(search.clone()));
    let mut state = run_state().await;
    state.directed = Some((Operation::Click, Some("search".into())));
    assert!(follow_recovery(&mut state, &observation(std::slice::from_ref(&search)), &decision, false).is_none());
    // Answer outputs still open do not let the direction overrule a pick Jev is sure of.
    state.open_answer_outputs = vec!["price".into()];
    state.directed = Some((Operation::Click, Some("search".into())));
    assert!(follow_recovery(&mut state, &observation(&[search]), &decision, false).is_none());
    state.directed = Some((Operation::Click, Some("search".into())));
    assert!(follow_recovery(&mut state, &observation(&[]), &decision, true).is_none());
}

#[test]
fn test_every_visited_address_is_shown_redacted() {
    let mut redactor = Redactor::new();
    let mut addresses: IndexMap<String, ()> = IndexMap::new();
    addresses.insert("https://a.test/?user=ada".into(), ());
    for i in 0..5 {
        addresses.insert(format!("https://a.test/{i}"), ());
    }
    redactor.register("username", "ada", None);
    let shown = visited(&addresses, |url| redactor.redact(url));
    assert_eq!(shown[0], "https://a.test/?user=[secret:username]");
    assert_eq!(shown[1..], (0..5).map(|i| format!("https://a.test/{i}")).collect::<Vec<_>>());
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
fn test_a_field_set_back_to_an_earlier_value_is_typed_in_the_order_the_task_gives() {
    let values: Vec<String> = ["A", "B", "A", "C"].map(String::from).to_vec();
    for (typed_so_far, expected) in [
        (&[][..], "A"),
        (&["A"][..], "B"),
        (&["A", "B"][..], "A"),
        (&["A", "B", "A"][..], "C"),
        (&["A", "B", "A", "C"][..], "C"),
    ] {
        let entries: Vec<HistoryEntry> = typed_so_far.iter().map(|text| typed("Name", text)).collect();
        assert_eq!(next_value(&values, &entries, Some("Name")), expected, "{typed_so_far:?}");
    }
}

#[tokio::test]
async fn test_http_error_visit_cannot_evidence_navigation_completion() {
    let agent = agent();
    let mut state = run_state().await;
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
    let pine = Fact::read(None, "Pine capacity 10", block_evidence(&page, "s0"), FactReader::Llm);
    let oak = Fact::read(None, "Oak capacity 7", block_evidence(&page, "s1"), FactReader::Llm);
    (pine, oak)
}

#[test]
fn test_derived_paraphrases_do_not_create_new_read_evidence() {
    let (pine, oak) = capacities();
    let both = vec![fact_id(&pine), fact_id(&oak)];
    let mut notes = Notes::new([pine, oak]);
    let before = answer_evidence(&notes, false);
    let completion = answer_evidence(&notes, true);
    notes.add(Fact::derived(None, "Pine has greater capacity.", both.clone(), FactReader::Llm).unwrap());
    assert_eq!(answer_evidence(&notes, false), before);
    assert_ne!(answer_evidence(&notes, true), completion);
    notes.add(Fact::derived(None, "Pine is the capacity winner.", both, FactReader::Llm).unwrap());
    assert_eq!(answer_evidence(&notes, false), before);
    let third = capture(&[(BlockKind::Paragraph, "Third capacity 12")]);
    notes.add(Fact::read(None, "Third capacity 12", block_evidence(&third, "s0"), FactReader::Llm));
    assert_ne!(answer_evidence(&notes, false), before);
}

#[test]
fn test_answer_evidence_tracks_identifying_context_without_unrelated_capture_changes() {
    let page = capture(&[(BlockKind::Paragraph, "12")]);
    let quoted = block_evidence(&page, "s0");
    let fact = Fact::read(None, "12", quoted.clone(), FactReader::Llm);
    let before = answer_evidence(&Notes::new([fact.clone()]), true);
    let headed = Evidence { heading_path: vec!["Admission price".into()], ..quoted };
    let contextual = Fact { evidence: Some(headed.clone()), ..fact };
    let changed = answer_evidence(&Notes::new([contextual.clone()]), true);
    assert_ne!(changed, before);
    let redraw =
        Fact { evidence: Some(Evidence { capture_sha256: "unrelated-redraw".into(), ..headed }), ..contextual };
    assert_eq!(answer_evidence(&Notes::new([redraw]), true), changed);
}

#[test]
fn test_answer_evidence_changes_when_a_conclusion_gains_an_existing_supporting_quote() {
    for quoted in [false, true] {
        let (pine, oak) = capacities();
        let text = "Pine has the highest capacity.";
        let winner = if quoted {
            Fact::read(None, text, pine.evidence.clone().unwrap(), FactReader::Llm)
        } else {
            Fact::derived(None, text, vec![fact_id(&pine)], FactReader::Llm).unwrap()
        };
        let oak_id = fact_id(&oak);
        let mut notes = Notes::new([pine, oak, winner.clone()]);
        let before = answer_evidence(&notes, true);
        let mut basis = winner.basis.clone();
        basis.push(oak_id);
        notes.add(Fact { basis, ..winner });
        assert_ne!(answer_evidence(&notes, true), before, "quoted: {quoted}");
    }
}

#[test]
fn test_answer_evidence_tracks_promoted_context_without_turning_paraphrases_into_read_progress() {
    let page = capture(&[(BlockKind::Paragraph, "Admission 12; hours 9am")]);
    let fact = Fact::read(None, "Admission 12", block_evidence(&page, "s0"), FactReader::Llm);
    let mut notes = Notes::new([fact.clone()]);
    let answer_before = answer_evidence(&notes, true);
    let read_before = answer_evidence(&notes, false);
    notes.add(Fact { text: "Hours 9am".into(), requirement_id: Some("r".into()), ..fact.clone() });
    assert_ne!(answer_evidence(&notes, true), answer_before);
    assert_eq!(answer_evidence(&notes, false), read_before);
    assert_eq!(notes.fact_requirements(&fact_id(&fact)), ["r"]);
    let promoted = answer_evidence(&notes, true);
    notes.unevidence(["r"]);
    assert_ne!(answer_evidence(&notes, true), promoted);
    assert!(notes.fact_requirements(&fact_id(&fact)).is_empty());
}

// tests/test_download_gate.py

// -- The one stall rule and the one budget-renewal rule -------------------------------------------------------------

const FILES: &str = "https://addons.example.test/project/files";
const DOWNLOAD: &str = "https://addons.example.test/project/download/1";

fn buttons_at(url: &str, labels: &[&str]) -> Observation {
    Observation { url: url.into(), ..observation(&labels.iter().map(|label| button(label)).collect::<Vec<_>>()) }
}

/// A run on the file list that spent one recovery there, then clicked through to the download page.
async fn left_the_list() -> (Agent, RunState) {
    let mut state = run_state().await;
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

// -- tests/test_accuracy_fixes.py: a stopping run commits an answer ------------------------------------------------

/// An LLM that writes `answer` for the one composer call a stopping run makes.
struct Composer(serde_json::Value);

impl fastbrowse_core::llm::LLMClient for Composer {
    fn generate_value<'a>(
        &'a self,
        purpose: fastbrowse_core::models::LLMPurpose,
        _: &'a [fastbrowse_core::llm::Message],
        schema: &'a dyn fastbrowse_core::llm::ResponseSchema,
        _: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<serde_json::Value>, fastbrowse_core::llm::LLMError>> {
        Box::pin(async move {
            assert_eq!(purpose, fastbrowse_core::models::LLMPurpose::Compose);
            let data = schema.validate_json(&self.0.to_string()).map_err(fastbrowse_core::llm::LLMError::Failed)?;
            Ok(Generation { data, cost: CostLine::new(CostComponent::Llm, CostBasis::Metered, Some(0.0)) })
        })
    }
}

/// A run whose one requirement was read off a table, with the agent that answers its composer call with `claims`.
async fn stopped_with(claims: impl Fn(&Fact) -> serde_json::Value) -> (Agent, RunState) {
    let mut state = run_state().await;
    let page = capture(&[(BlockKind::Paragraph, "| Country | IMF (2026) |\n| South Korea | 2,245,847 |")]);
    let fact = Fact::read(Some("r1"), "South Korea 2,245,847", block_evidence(&page, "s0"), FactReader::Llm);
    state.notes = Notes::new([fact.clone()]);
    state.ready_plan = Some(Plan::new(vec![Requirement::new("r1", "Report the 12th-highest GDP.", INFO)], true));
    let agent = Agent::new(
        Arc::new(NoPage),
        Arc::new(ScriptedJev::new(Vec::new())),
        Arc::new(Composer(json!({"claims": claims(&fact)}))),
    );
    (agent, state)
}

#[tokio::test]
async fn test_a_stopping_run_commits_an_answer_instead_of_its_notes() {
    // A table lookup used to arrive as its rows with no sentence naming the country.
    let (agent, mut state) = stopped_with(|fact| {
        json!([{"text": "South Korea has the 12th-highest GDP at 2,245,847 US$ millions.", "evidence_ids": [fact_id(fact)]}])
    })
    .await;
    let (notes, ledger) = (state.notes.clone(), state.ledger.clone());
    let error = Some("ran out of recoveries".to_string());
    let committed = agent.stopping_result(&notes, &mut state, &ledger, Status::Stuck, error).await;
    assert_eq!(committed.status, Status::Stuck);
    let answer = committed.answer.expect("an answer");
    assert!(answer.starts_with("Answer (unverified)"), "{answer}");
    assert!(answer.contains("South Korea") && answer.contains("2,245,847"), "{answer}");
}

#[tokio::test]
async fn test_a_commit_with_no_claims_falls_back_to_the_notes() {
    let (agent, mut state) = stopped_with(|_| json!([])).await;
    let (notes, ledger) = (state.notes.clone(), state.ledger.clone());
    let result = agent.stopping_result(&notes, &mut state, &ledger, Status::Unverified, None).await;
    assert_eq!(result.status, Status::Unverified);
    let answer = result.answer.expect("an answer");
    assert!(!answer.starts_with("Answer (unverified)") && answer.contains("2,245,847"), "{answer}");
}

/// The plan call, priced at a cent as a metered provider prices it.
struct MeteredPlan;

impl fastbrowse_core::llm::LLMClient for MeteredPlan {
    fn generate_value<'a>(
        &'a self,
        purpose: fastbrowse_core::models::LLMPurpose,
        _: &'a [fastbrowse_core::llm::Message],
        schema: &'a dyn fastbrowse_core::llm::ResponseSchema,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<serde_json::Value>, fastbrowse_core::llm::LLMError>> {
        Box::pin(async move {
            if let Some(ledger) = ledger {
                ledger.reserve(CostComponent::Llm, 0.0, 1)?;
            }
            let reply = json!({"requirements": [], "answer_expected": true, "answer_checks": ["Report the price"]});
            let data = schema.validate_json(&reply.to_string()).map_err(fastbrowse_core::llm::LLMError::Failed)?;
            let cost = CostLine {
                purpose: Some(purpose),
                ..CostLine::new(CostComponent::Llm, CostBasis::Metered, Some(0.01))
            };
            Ok(Generation { data, cost })
        })
    }
}

#[tokio::test]
async fn test_abandoned_head_start_bills_the_finished_plan_receipt() {
    // tests/clients/test_openai_compatible.py: the plan is the only call, so an abandoned run bills its receipt once
    // and starts no follow-up.
    let head = super::HeadStart::begin(Arc::new(MeteredPlan), "Report the price", None, None, None);
    head.planning.wait().await;
    let costs = head.abandon().await;
    assert_eq!(costs.len(), 1);
    assert_eq!((costs[0].basis, costs[0].dollars), (CostBasis::Metered, Some(0.01)));
}

#[tokio::test]
async fn test_a_head_start_asks_for_a_shortcut_only_at_a_sites_entry_point() {
    let llm = Arc::new(ScriptedLLM);
    let entry = super::HeadStart::begin(llm.clone(), "Find the price", Some("https://shop.test/"), None, None);
    let inside = super::HeadStart::begin(llm, "Find the price", Some("https://shop.test/cart"), None, None);
    assert!(entry.proposing.is_some());
    assert!(inside.proposing.is_none());
    entry.discard().await;
    inside.discard().await;
}

#[test]
fn test_a_shortcut_is_worth_proposing_only_from_a_sites_entry_point() {
    for (start, worth) in [
        ("https://shop.test", true),
        ("https://shop.test/", true),
        ("https://shop.test/?q=mug", true),
        ("https://shop.test/#top", true),
        ("https://shop.test/cart", false),
        ("https://shop.test/cart/", false),
        ("about:blank", false),
    ] {
        assert_eq!(super::shortcut_worth_proposing(start), worth, "{start}");
    }
}

/// A page with no browser behind it, which cannot open the address it is given.
struct Unfetchable;

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
        Box::pin(async { Err(BrowserError::NeedsBrowser("the page renders its content with scripts".into())) })
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
    let agent = Agent::new(Arc::new(Unfetchable), Arc::new(ScriptedJev::new(Vec::new())), Arc::new(ScriptedLLM));
    let options = super::RunOptions { start: Some("https://example.test/".into()), ..Default::default() };
    let result = agent.run("Read the price", options).await.expect("a result, not an error");
    assert_eq!(result.status, Status::Blocked);
    assert_eq!(result.error.as_deref(), Some("the page renders its content with scripts"));
    assert!(result.steps.is_empty() && result.evidence.is_empty() && result.final_url.is_none());
    // The plan was asked for before the page, and what it spent is still reported.
    assert!(!result.cost.lines.is_empty());
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
    let mut state = run_state().await;
    let Err(super::state::Flow::Stop(stop)) = agent.text(&mut state, &page, &password).await else {
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
    let mut state = run_state().await;
    let step = StepResult {
        index: 0,
        operation: Operation::Done,
        decided_by: Decider::Llm,
        outcome: StepOutcome::Failed,
        url: "https://acme.test/account?pw=hunter2".into(),
        target: Some("Signed in with hunter2".into()),
        confidence: None,
        note: Some("DONE rejected: Answer output not verified: hunter2".into()),
        facts: Vec::new(),
        page_changed: None,
        duration_ms: 0,
    };
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
        let mut state = run_state().await;
        state.task = "Check out and finish the order.".into();
        state.authorization = Authorization { irreversible_actions: authorized };
        let finish = button("Finish");
        let observation = observation(std::slice::from_ref(&finish));
        let decision = Decision { operation_confidence: 0.86, ..code_decision(Operation::Click, Some(finish.clone())) };
        let question = (!credential).then(|| irreversible_question(&state.task, Operation::Click, &finish, None));
        let asked = requested_question(&state.task, &control_action(Operation::Click, &finish, &[]));
        let gated =
            agent.gate_question(&mut state, &observation, &decision, Decider::Jev, "Finish", question, asked).await;
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
async fn test_a_cited_read_claim_finishes_after_one_done_check() {
    let page = capture(&[(BlockKind::Paragraph, "The answer is 42")]);
    let fact = Fact::read(Some("r1"), "The answer is 42", block_evidence(&page, "s0"), FactReader::Llm);
    let mut state = run_state().await;
    state.task = "Report the answer.".into();
    state.ready_plan = Some(Plan::new(vec![Requirement::new("r1", "Report the answer", INFO)], true));
    state.notes = Notes::new([fact]);
    let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let agent = Agent::new(
        Arc::new(FinishPage(observation(&[]))),
        Arc::new(DoneCheckJev(requests.clone())),
        Arc::new(ScriptedLLM),
    );

    let result = agent.finish(&mut state, None, None).await.expect("finish").expect("a completed result");
    assert_eq!(result.status, Status::Complete);
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(result.answer.as_deref().is_some_and(|answer| answer.contains("The answer is 42")));
}

#[tokio::test]
async fn test_a_done_jev_does_not_answer_is_rejected_and_the_history_says_why() {
    let mut state = run_state().await;
    state.task = "Report the answer.".into();
    state.ready_plan = Some(Plan::new(vec![Requirement::new("r1", "Report the answer", INFO)], true));
    state.recoveries = 99;
    let agent = Agent::new(Arc::new(FinishPage(observation(&[]))), Arc::new(Receipts(None)), Arc::new(ScriptedLLM));

    // With the recovery budget spent, the rejection ends the run; what matters is that nothing finished it.
    let finished = agent.finish(&mut state, None, None).await;
    assert!(!matches!(finished, Ok(Some(ref result)) if result.status == Status::Complete));
    let effect = state.history.last().and_then(|entry| entry.effect.clone()).unwrap_or_default();
    assert_eq!(effect, "DONE rejected: r1: Report the answer (Jev gave no answer)");
}

/// A Jev that answers every yes/no with one probability, or with nothing.
struct Receipts(Option<f64>);

impl fastbrowse_core::decision::DecisionClient for Receipts {
    fn evaluate<'a>(
        &'a self,
        _: &'a serde_json::Value,
        questions: &'a IndexMap<String, fastbrowse_core::decision::Question>,
    ) -> BoxFuture<'a, Result<fastbrowse_core::decision::Evaluation, fastbrowse_core::decision::DecisionError>> {
        let answers = self
            .0
            .map(|probability| {
                let answer = Answer::Noul(NoulAnswer { kind: Default::default(), probability });
                questions.keys().map(|key| (key.clone(), answer.clone())).collect()
            })
            .unwrap_or_default();
        Box::pin(
            async move { Ok(Evaluation { model: "jev".into(), answers, input_tokens: 0, cost: free(), requests: 1 }) },
        )
    }

    fn profile(&self) -> DecisionProfile {
        ScriptedJev::new(Vec::new()).profile()
    }
}

#[tokio::test]
async fn test_a_receipt_is_read_only_for_a_step_jev_says_committed() {
    use fastbrowse_core::safety::irreversible_question;
    // Adding to a cart on an authorized checkout used to force a full read of the list after every click.
    for (probability, owed) in [(Some(0.1), false), (Some(0.9), true), (None, true)] {
        let agent = Agent::new(Arc::new(NoPage), Arc::new(Receipts(probability)), Arc::new(ScriptedLLM));
        let mut state = run_state().await;
        state.task = "Add the backpack to the cart and finish the order.".into();
        state.authorization = Authorization { irreversible_actions: true };
        state.ready_plan =
            Some(Plan::new(vec![Requirement::new("a1", "Finish the order", RequirementKind::Action)], false));
        state.transaction_candidates.push(TransactionCandidate {
            question: irreversible_question(&state.task, Operation::Click, &button("Add to cart"), None),
            from_url: "https://shop.test/inventory".into(),
            landed_url: Some("https://shop.test/inventory".into()),
            landed_status: None,
            committing: None,
            outcome_read: false,
        });
        assert_eq!(agent.unread_receipt(&mut state).await.ok(), Some(owed), "{probability:?}");
    }
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

#[tokio::test]
async fn test_jev_reads_a_field_value_on_a_task_that_also_acts() {
    let field = "Date Picker 2: 27/12/2026";
    // The scripted LLM has no reader reply, so a read that reached it would fail.
    let agent = Agent::new(Arc::new(NoPage), Arc::new(FieldReader(Some(field), 0.1)), Arc::new(ScriptedLLM));
    let mut state = run_state().await;
    state.ready_plan = Some(Plan::new(
        vec![
            Requirement::new("pick", "Pick the last Sunday of December 2026 in Date Picker 2", RequirementKind::Action),
            Requirement::new("shown", "Report the date shown in the Date Picker 2 field", INFO),
        ],
        true,
    ));
    // How a capture states a form field: its label, then the value it holds.
    let page = capture(&[
        (BlockKind::Heading, "Date Picker"),
        (BlockKind::Paragraph, "Date Picker 1: 01/01/2026"),
        (BlockKind::Paragraph, field),
    ]);
    let added = agent.read(&mut state, Some(page), None).await.expect("a read");
    assert!(added);
    let fact = state.notes.facts().into_iter().find(|f| f.requirement_id.as_deref() == Some("shown")).expect("a fact");
    assert_eq!(fact.reader, FactReader::JevChoice);
    assert_eq!(fact.text, field);
    assert_eq!(fact.evidence.as_ref().map(|e| e.quote.as_str()), Some(field));
}

// -- A stall is put to Jev first, and the LLM only hears of it when Jev stays unsure --------------------------------

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

/// An LLM that answers every recovery with a scroll and counts the times it was asked.
#[derive(Default)]
struct Recoverer(std::sync::atomic::AtomicUsize, bool);

impl fastbrowse_core::llm::LLMClient for Recoverer {
    fn generate_value<'a>(
        &'a self,
        purpose: fastbrowse_core::models::LLMPurpose,
        _: &'a [fastbrowse_core::llm::Message],
        schema: &'a dyn fastbrowse_core::llm::ResponseSchema,
        _: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<serde_json::Value>, fastbrowse_core::llm::LLMError>> {
        Box::pin(async move {
            assert_eq!(purpose, fastbrowse_core::models::LLMPurpose::Recover);
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let reply =
                json!({"diagnosis": "Stuck", "next_subgoal": "Scroll on", "give_up": self.1, "operation": "scroll"});
            let data = schema.validate_json(&reply.to_string()).map_err(fastbrowse_core::llm::LLMError::Failed)?;
            Ok(Generation { data, cost: CostLine::new(CostComponent::Llm, CostBasis::Metered, Some(0.0)) })
        })
    }
}

/// Recovery after a click on Apply changed nothing, put to a Jev that would click Next at `confidence`.
async fn stalled_on_apply(confidence: f64) -> (RunState, Arc<Unsure>, Arc<Recoverer>) {
    let script = vec![crate::scripted::Step { operation: Operation::Click, label: "Next".into(), pick: None }];
    let jev = Arc::new(Unsure(ScriptedJev::new(script), confidence, Default::default()));
    let llm = Arc::new(Recoverer::default());
    let agent = Agent::new(Arc::new(Screenshots::default()), jev.clone(), llm.clone());
    let mut state = run_state().await;
    let apply = button("Apply");
    let page = observation(&[apply.clone(), button("Next")]);
    let decision = code_decision(Operation::Click, Some(apply));
    let withheld = super::recovery::withheld(&decision);
    agent.recover(&mut state, &page, "nothing changed", Status::Stuck, withheld).await.expect("recovered");
    (state, jev, llm)
}

#[tokio::test]
async fn test_giving_up_ends_the_run_with_the_diagnosis() {
    let script = vec![crate::scripted::Step { operation: Operation::Click, label: "Next".into(), pick: None }];
    let jev = Arc::new(Unsure(ScriptedJev::new(script), 0.1, Default::default()));
    let llm = Arc::new(Recoverer(Default::default(), true));
    let agent = Agent::new(Arc::new(Screenshots::default()), jev, llm);
    let mut state = run_state().await;
    let apply = button("Apply");
    let page = observation(&[apply.clone(), button("Next")]);
    let decision = code_decision(Operation::Click, Some(apply));
    let withheld = super::recovery::withheld(&decision);
    let error = agent.recover(&mut state, &page, "nothing changed", Status::Stuck, withheld).await.err();
    assert!(matches!(error, Some(Flow::Stop(stop)) if stop.error.as_deref() == Some("Stuck")));
    assert_eq!(state.hint, None);
}

#[tokio::test]
async fn test_a_stall_jev_is_sure_of_is_answered_without_the_llm() {
    let (state, jev, llm) = stalled_on_apply(0.9).await;
    assert_eq!(llm.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    let (_, picked) = state.picked.expect("Jev's pick is the next decision");
    assert_eq!((picked.operation, picked.target.map(|c| c.label)), (Operation::Click, Some("Next".to_string())));
    // Jev was told what failed, and the click that changed nothing was not among its options.
    let shown = jev.2.lock().unwrap()[0].to_string();
    assert!(shown.contains("did not move the task on from this page"), "{shown}");
    assert!(!shown.contains("\"Apply\""), "{shown}");
}

#[tokio::test]
async fn test_the_llm_recovers_only_when_jev_stays_unsure() {
    let (state, _, llm) = stalled_on_apply(0.3).await;
    assert_eq!(llm.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(state.picked.is_none());
    assert_eq!(state.directed, Some((Operation::Scroll, None)));
    assert_eq!(state.steps.last().map(|step| step.decided_by), Some(Decider::Llm));
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
    let agent = Agent::new(page.clone(), jev.clone(), Arc::new(Recoverer::default()));
    let mut state = run_state().await;
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

// -- Jev reads a step as text, and sees the page only when that leaves it unsure ---------------------------------------

/// The scripted Jev, its every choice held at the given confidence from text and at 0.9 once shown the page, and the
/// number of images each request carried.
struct Looks(ScriptedJev, f64, std::sync::Mutex<Vec<usize>>);

impl DecisionClient for Looks {
    fn evaluate<'a>(
        &'a self,
        state: &'a serde_json::Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        self.evaluate_images(state, questions, &fastbrowse_core::decision::NO_IMAGES)
    }

    fn evaluate_images<'a>(
        &'a self,
        state: &'a serde_json::Value,
        questions: &'a IndexMap<String, Question>,
        images: &'a Images,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        self.2.lock().unwrap().push(images.len());
        let confidence = if images.is_empty() { self.1 } else { 0.9 };
        let answered = self.0.evaluate(state, questions);
        Box::pin(async move {
            let mut evaluation = answered.await?;
            for answer in evaluation.answers.values_mut() {
                if let Answer::Choice(choice) = answer {
                    choice.confidence = confidence;
                }
            }
            Ok(evaluation)
        })
    }

    fn profile(&self) -> DecisionProfile {
        DecisionProfile { images: true, ..DecisionProfile::jev() }
    }
}

#[tokio::test]
async fn test_jev_sees_the_page_only_when_its_text_pick_is_unsure() {
    for (text, images, confidence) in [(0.9, vec![0], 0.9), (0.3, vec![0, 1], 0.9)] {
        let next = || crate::scripted::Step { operation: Operation::Click, label: "Next".into(), pick: None };
        let jev = Arc::new(Looks(ScriptedJev::new(vec![next(), next()]), text, Default::default()));
        let agent = Agent::new(Arc::new(Screenshots::default()), jev.clone(), Arc::new(Recoverer::default()));
        let state = run_state().await;
        let context = agent.context(&state, &observation(&[button("Next")]), Vec::new(), false, true).expect("context");
        let decision =
            agent.decide_looking(&state, &observation(&[button("Next")]), &context).await.expect("a decision");
        assert_eq!(*jev.2.lock().unwrap(), images, "text pick at {text}");
        assert!((decision.confidence() - confidence).abs() < 1e-9, "{}", decision.confidence());
        assert_eq!(decision.cost.len(), images.len());
    }
}

/// The LLM reader's one reply: the code a sign-in needs, quoted as context from the second block.
struct ContextReader;

impl fastbrowse_core::llm::LLMClient for ContextReader {
    fn generate_value<'a>(
        &'a self,
        purpose: fastbrowse_core::models::LLMPurpose,
        messages: &'a [fastbrowse_core::llm::Message],
        schema: &'a dyn fastbrowse_core::llm::ResponseSchema,
        _: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<serde_json::Value>, fastbrowse_core::llm::LLMError>> {
        Box::pin(async move {
            assert_eq!(purpose, fastbrowse_core::models::LLMPurpose::Read);
            assert!(format!("{messages:?}").contains("Read only the context claims"), "{messages:?}");
            let reply = json!({
                "claims": [{"cite": {"first": "s1", "last": "s1"}, "text": "ada@example.com: 587373", "requirement_id": null}],
                "answered": false,
            });
            let data = schema.validate_json(&reply.to_string()).map_err(fastbrowse_core::llm::LLMError::Failed)?;
            Ok(Generation { data, cost: CostLine::new(CostComponent::Llm, CostBasis::Metered, Some(0.0)) })
        })
    }
}

#[tokio::test]
async fn test_a_code_jev_finds_no_requirement_in_is_still_kept_for_the_sign_in() {
    // Jev found nothing on the Phone page for the orders the read was asked about, and the code never reached the notes.
    let agent = Agent::new(Arc::new(NoPage), Arc::new(FieldReader(None, 0.98)), Arc::new(ContextReader));
    let mut state = run_state().await;
    state.ready_plan = Some(Plan::new(
        vec![
            Requirement::new(
                "in",
                "Sign in as ada@example.com with the code from the Phone page",
                RequirementKind::Action,
            ),
            Requirement::new("orders", "Report the most expensive order", INFO),
        ],
        true,
    ));
    let page = capture(&[(BlockKind::Heading, "Authenticator"), (BlockKind::Paragraph, "ada@example.com: 587373")]);
    agent.read(&mut state, Some(page), None).await.expect("a read");
    let fact = state.notes.facts().into_iter().find(|f| f.text.contains("587373")).expect("the code");
    assert_eq!(fact.requirement_id, None);
}

/// A plan still being written when the run stops. A provider finishes and bills a non-streamed generation after the
/// connection closes, so a request dropped in flight is charged as unknown, as the OpenAI-compatible client does.
struct SlowPlan;

struct ChargedIfDropped<'a> {
    ledger: Option<&'a Ledger>,
    armed: bool,
}

impl Drop for ChargedIfDropped<'_> {
    fn drop(&mut self) {
        if let (true, Some(ledger)) = (self.armed, self.ledger) {
            ledger.state().lines.push(CostLine::new(CostComponent::Llm, CostBasis::Unknown, None));
        }
    }
}

impl fastbrowse_core::llm::LLMClient for SlowPlan {
    fn generate_value<'a>(
        &'a self,
        purpose: fastbrowse_core::models::LLMPurpose,
        messages: &'a [fastbrowse_core::llm::Message],
        schema: &'a dyn fastbrowse_core::llm::ResponseSchema,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<serde_json::Value>, fastbrowse_core::llm::LLMError>> {
        Box::pin(async move {
            let mut charged = ChargedIfDropped { ledger, armed: true };
            tokio::time::sleep(Duration::from_millis(200)).await;
            charged.armed = false;
            MeteredPlan.generate_value(purpose, messages, schema, ledger).await
        })
    }
}

/// A tab that crashed before the run could read it.
struct Crashed;

impl Page for Crashed {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        // Long enough for the plan's request to be in flight when the run stops.
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Err(BrowserError::Failed("the tab crashed".into()))
        })
    }

    fn document_changed<'a>(&'a self, _: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(true) })
    }

    fn navigate<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        unreachable!("the run is given no start")
    }

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        Box::pin(async { Err(BrowserError::Failed("the tab crashed".into())) })
    }

    fn act<'a>(&'a self, _: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        unreachable!("nothing was observed, so no step acts")
    }

    fn redrawn<'a>(&'a self, _: &'a Observation, _: f64, _: Option<&'a str>) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        Box::pin(async { Err(BrowserError::Failed("the tab crashed".into())) })
    }

    fn withhold_frames(&self, _: bool) {}

    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        Box::pin(async { Err(BrowserError::Failed("the tab crashed".into())) })
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
async fn test_a_run_that_stops_before_its_plan_arrives_keeps_the_plans_receipt_and_its_own_status() {
    // A run stopped at its first step used to cancel the plan in flight. The provider billed it anyway, the receipt
    // was lost, and the unknown charge replaced the stop with "spend limit cannot be enforced".
    let agent = Agent::new(Arc::new(Crashed), Arc::new(ScriptedJev::new(Vec::new())), Arc::new(SlowPlan));
    let options = super::RunOptions {
        limits: Some(Limits { max_dollars: Some(1.0), ..Limits::default() }),
        ..Default::default()
    };
    let result = agent.run("Tidy up my list", options).await.expect("a result, not an error");
    assert_eq!(result.status, Status::Error, "{:?}", result.error);
    assert!(result.error.as_deref().is_some_and(|error| error.contains("the tab crashed")), "{:?}", result.error);
    assert!(!result.cost.has_unknown(), "{:?}", result.cost);
    assert_eq!(result.cost.known_dollars(), 0.01);
}

#[tokio::test]
async fn a_task_cancelled_before_it_began_never_runs() {
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = ran.clone();
    let task = Spawned::<()>::spawn(async move {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    });
    assert!(task.cancel_unstarted());
    assert!(task.wait().await.is_none());
    assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn a_task_already_begun_is_joined_not_cancelled() {
    let task = Spawned::spawn(async {
        tokio::task::yield_now().await;
        Ok(7)
    });
    tokio::task::yield_now().await;
    assert!(!task.cancel_unstarted());
    assert_eq!(task.wait().await.and_then(Result::ok), Some(7));
}

/// Answers whether a page continues the list left open.
struct SameList(f64);

impl fastbrowse_core::decision::DecisionClient for SameList {
    fn evaluate<'a>(
        &'a self,
        _: &'a serde_json::Value,
        questions: &'a IndexMap<String, fastbrowse_core::decision::Question>,
    ) -> BoxFuture<'a, Result<fastbrowse_core::decision::Evaluation, fastbrowse_core::decision::DecisionError>> {
        use fastbrowse_core::decision::{Answer, Evaluation, NoulAnswer};
        let noul =
            |key: &String| (key.clone(), Answer::Noul(NoulAnswer { kind: Default::default(), probability: self.0 }));
        let answers = questions.keys().map(noul).collect();
        Box::pin(async move {
            Ok(Evaluation { model: "same-list".into(), answers, input_tokens: 0, cost: free(), requests: 1 })
        })
    }

    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        ScriptedJev::new(Vec::new()).profile()
    }
}

/// Jev's word places a later page, and it is given the page's links with the one the page marks as next.
#[tokio::test]
async fn test_jev_places_a_later_page_of_a_list_from_its_links() {
    let mut state = run_state().await;
    state.list_url = Some("https://site.test/page/1".into());
    let capture = crate::retrieval::testing::capture(&[]);
    let page = at("https://site.test/page/2", &[link("n", "3", "/page/3"), pager("m", "Weiter", "/page/3")]);
    for (probability, place) in [(0.06, (false, false)), (0.97, (true, true))] {
        let agent = Agent::new(Arc::new(NoPage), Arc::new(SameList(probability)), Arc::new(ScriptedLLM));
        let placed = agent.place_in_list(&state, &capture, &page).await.ok().flatten().expect("placed");
        assert_eq!((placed.same_list, placed.goes_on), place);
    }
    let links = list_links(&page);
    assert_eq!(links[0], json!({"label": "3", "href": "/page/3"}));
    assert_eq!(links[1], json!({"label": "Weiter", "href": "/page/3", "next_page": true}));
}

#[tokio::test]
async fn test_a_page_jev_puts_off_the_list_leaves_the_list_open() {
    let mut state = run_state().await;
    state.through_end = true;
    state.list_url = Some("https://site.test/page/1".into());
    let capture = crate::retrieval::testing::capture(&[]);
    let agent = Agent::new(Arc::new(NoPage), Arc::new(SameList(0.06)), Arc::new(ScriptedLLM));
    let page = at("https://site.test/item/4", &[]);
    assert_eq!(agent.read_list_page(&mut state, &capture, &page, &[]).await.ok(), Some(ListRead::OffList));
    assert!(state.through_end);
}

#[tokio::test]
async fn test_moving_on_from_an_unread_page_of_an_open_list_is_put_to_jev() {
    let mut state = run_state().await;
    state.ready_plan = Some(Plan::new(vec![Requirement::new("count", "Count the quotes", INFO)], true));
    let page = at("https://site.test/page/5", &[]);
    let click = code_decision(Operation::Click, None);
    assert!(!leaves_unread_page(&state, &page, &click));
    state.continuing.insert("count".into());
    assert!(leaves_unread_page(&state, &page, &click));
    assert!(!leaves_unread_page(&state, &page, &code_decision(Operation::Scroll, None)));
    state.read_documents.insert(page.document_key.clone(), false);
    assert!(!leaves_unread_page(&state, &page, &click));
}

/// An LLM reader that finds nothing on the page.
struct EmptyReader;

impl fastbrowse_core::llm::LLMClient for EmptyReader {
    fn generate_value<'a>(
        &'a self,
        _: fastbrowse_core::models::LLMPurpose,
        _: &'a [fastbrowse_core::llm::Message],
        schema: &'a dyn fastbrowse_core::llm::ResponseSchema,
        _: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<serde_json::Value>, fastbrowse_core::llm::LLMError>> {
        Box::pin(async move {
            let data = schema
                .validate_json(&json!({"claims": [], "answered": false}).to_string())
                .map_err(fastbrowse_core::llm::LLMError::Failed)?;
            Ok(Generation { data, cost: CostLine::new(CostComponent::Llm, CostBasis::Metered, Some(0.0)) })
        })
    }
}

/// A detour to an item's own page, read in full, leaves the list to be read whole when the run comes back to it.
#[tokio::test]
async fn test_reading_a_page_off_the_list_keeps_the_list_read_whole() {
    let mut state = run_state().await;
    state.ready_plan = Some(Plan::new(vec![Requirement::new("count", "Count the quotes", INFO)], true));
    state.through_end = true;
    state.continuing.insert("count".into());
    state.list_url = Some("https://site.test/page/1".into());
    let agent = Agent::new(Arc::new(NoPage), Arc::new(SameList(0.06)), Arc::new(EmptyReader));
    let mut capture = crate::retrieval::testing::capture(&[(BlockKind::Paragraph, "An author's biography")]);
    capture.url = "https://site.test/item/4".into();
    let page = at("https://site.test/item/4", &[]);
    agent.read(&mut state, Some(capture), Some(&page)).await.expect("a read");
    assert!(state.through_end);
    assert_eq!(state.list_url.as_deref(), Some("https://site.test/page/1"));
    assert!(state.continuing.contains("count"));
}

#[tokio::test]
async fn test_a_page_off_the_list_leaves_it_open_and_a_later_page_ends_it() {
    let mut state = run_state().await;
    state.continuing.insert("count".into());
    state.list_url = Some("https://site.test/page/7".into());
    let mut capture = crate::retrieval::testing::capture(&[]);
    capture.url = "https://site.test/author".into();
    for (probability, ended) in [(0.06, false), (0.97, true)] {
        let agent = Agent::new(Arc::new(NoPage), Arc::new(SameList(probability)), Arc::new(ScriptedLLM));
        assert_eq!(agent.on_open_list(&mut state, &capture).await.ok(), Some(ended));
    }
}
