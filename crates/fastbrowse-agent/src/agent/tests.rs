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
use fastbrowse_core::effects::step_key;
use fastbrowse_core::llm::Generation;
use fastbrowse_core::models::{
    Artifact, Authorization, CostBasis, CostComponent, CostLine, Decider, Evidence, FactReader, Limits, Operation,
    Status, StepOutcome,
};
use fastbrowse_core::page::{
    ActResult, Action, BlockKind, BrowserError, Capture, Control, Observation, Page, PageResult,
};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::text;
use fastbrowse_core::safety::Redactor;
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde_json::json;

use super::Agent;
use super::driving::edit_progress;
use super::helpers::{
    access_inspection, answer_evidence, answered, code_decision, follow_recovery, next_page_control, next_value,
    unread, visited,
};
use super::reading::covered;
use super::state::{RunState, Spawned, TransactionCandidate};
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
        "controls": controls.iter().map(Dump::dump).collect::<Vec<_>>(), "omitted_controls": 0,
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

fn draggable() -> Control {
    Control::new("drag1", "generic", "Card A", [Operation::Click, Operation::Drag])
}

/// tests/test_memory.py's `evidence`.
fn evidence(sha: &str, start: i64, end: i64) -> Evidence {
    serde_json::from_value(json!({
        "source_id": "s1", "url": "https://example.test", "frame_id": null,
        "captured_at": "2026-01-01T00:00:00Z", "capture_sha256": sha, "start": start, "end": end, "quote": "fact",
    }))
    .unwrap()
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
    assert!(follow_recovery(&mut state, &observation(&[search]), &decision, false).is_none());
    state.directed = Some((Operation::Click, Some("search".into())));
    assert!(follow_recovery(&mut state, &observation(&[]), &decision, true).is_none());
}

#[test]
fn test_a_steps_identity_ignores_a_timer_but_not_its_heading() {
    let on = observation(&[button("Next")]);
    let shown = |heading: &str, timer: &str| {
        capture(&[(BlockKind::Heading, heading), (BlockKind::Paragraph, &format!("Elapsed: {timer}"))])
    };
    assert_eq!(step_key(&on, &shown("Review", "00:01")), step_key(&on, &shown("Review", "00:07")));
    assert_ne!(step_key(&on, &shown("Review", "00:01")), step_key(&on, &shown("Submit", "00:01")));
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

#[test]
fn test_the_next_page_of_a_list_is_one_link_to_another_address() {
    for (controls, found) in [
        (vec![link("n", "Next \u{2192}", "/page/2/")], Some("n")),
        (vec![link("n", "next", "/c/mystery/page-2.html"), link("m", "Mystery", "/c/mystery/index.html")], Some("n")),
        // A pager above and below the list is one next page.
        (vec![link("top", "\u{bb}", "/page/2/"), link("bottom", "\u{bb}", "/page/2/")], Some("top")),
        // Two different next pages is doubt, left to Jev.
        (vec![link("a", "Next", "/page/2/"), link("b", "Next", "/other/2/")], None),
        // A load-more button keeps the earlier records on the page, so reading again would count them twice.
        (vec![button("Next")], None),
        (vec![link("n", "Next", "/list/")], None),
        (vec![link("n", "Next steps for your account", "/help/")], None),
    ] {
        let control = next_page_control(&at("https://example.test/list/", &controls));
        assert_eq!(control.as_ref().map(|c| c.id.as_str()), found, "{controls:?}");
    }
}

#[test]
fn test_a_pager_the_page_marks_rel_next_is_followed_whatever_its_label() {
    // A pager drawn as an icon, or in another language, says so only in the markup.
    let icon =
        Control { label: "Weiter".into(), next_page: Some(true), ..link("n", "\u{2192}\u{2192}\u{2192}", "/page/2/") };
    let found = next_page_control(&at("https://example.test/list/", &[icon]));
    assert_eq!(found.map(|c| c.id), Some("n".to_string()));
}

#[test]
fn test_a_plan_is_answered_when_what_it_asks_to_find_is_evidenced_whatever_actions_it_lists() {
    let plan = Plan::new(
        vec![
            Requirement::new("r1", "Count the books on the first page", INFO),
            Requirement::new("r2", "Open the next page", RequirementKind::Action),
            Requirement::new("r3", "Count the books on the next page", INFO),
        ],
        true,
    );
    let mut notes = Notes::default();
    assert!(!answered(&plan, &notes));
    notes.add(Fact::read(Some("r1"), "20 books", evidence("capture", 0, 4), FactReader::Llm));
    assert!(!answered(&plan, &notes));
    notes.add(Fact::read(Some("r3"), "20 books", evidence("capture", 10, 14), FactReader::Llm));
    assert!(answered(&plan, &notes));
}

#[test]
fn test_a_task_of_only_actions_is_never_answered() {
    let plan = Plan::new(vec![Requirement::new("r1", "Open page 3 of the results", RequirementKind::Action)], false);
    assert!(!answered(&plan, &Notes::default()));
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
fn test_code_pager_never_follows_another_site_or_same_page() {
    for href in ["https://other.test/list/2", "//other.test/list/2", "other.test/list/2", "#next"] {
        assert!(next_page_control(&at("https://example.test/list/1", &[link("n", "Next", href)])).is_none(), "{href}");
    }
}

#[test]
fn test_disabled_pager_is_not_followed() {
    let disabled = Control { operations: Default::default(), ..link("next", "Next", "/list/2") };
    assert!(next_page_control(&at("https://example.test/list/1", &[disabled])).is_none());
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

#[test]
fn test_access_inspection_does_not_enable_a_protected_action() {
    let plan = Plan {
        inspect_access: true,
        ..Plan::new(vec![Requirement::new("r", "Sign in and export private records", RequirementKind::Action)], true)
    };
    assert!(!access_inspection(&plan));
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

#[test]
fn test_numeric_next_page_preserves_the_current_filters() {
    for parameter in ["page", "pageno", "page_number"] {
        let controls = [
            link("previous", "1", &format!("/list?{parameter}=1&sort=weight")),
            link("next", "3", &format!("/list?{parameter}=3&sort=weight")),
            link("later", "4", &format!("/list?{parameter}=4&sort=weight")),
            link("other_filter", "3", &format!("/list?{parameter}=3&sort=name")),
        ];
        let found = next_page_control(&at(&format!("https://example.test/list?{parameter}=2&sort=weight"), &controls));
        assert_eq!(found.map(|c| c.id), Some("next".to_string()), "{parameter}");
    }
}

#[test]
fn test_numeric_next_page_rejects_ambiguous_or_changed_scope() {
    let huge = format!("/list?page={}", "9".repeat(5000));
    for (current, target, label) in [
        ("/list?page=2", "/list?page=4", "4"),
        ("/list?page=2", "/list?page=3", "4"),
        ("/list", "/list?page=3", "3"),
        ("/list?page=2", "/other?page=3", "3"),
        ("/list?page=2", "https://other.test/list?page=3", "3"),
        ("/list?page=2&filter=", "/list?page=3", "3"),
        ("/list?page=2&page=8", "/list?page=3", "3"),
        ("/list?page=2", "/list?page=3", "\u{b2}"),
        (huge.as_str(), "/list?page=3", "3"),
    ] {
        let page = at(&format!("https://example.test{current}"), &[link("candidate", label, target)]);
        assert!(next_page_control(&page).is_none(), "{target} {label}");
    }
}

#[test]
fn test_next_page_deduplicates_absolute_and_relative_destinations() {
    for fragment in ["", "#results"] {
        let controls = [
            link("next", "Next", &format!("https://example.test/list?page=3&sort=weight{fragment}")),
            link("number", "3", "/list?page=3&sort=weight"),
        ];
        let found = next_page_control(&at("https://example.test/list?page=2&sort=weight", &controls));
        assert_eq!(found.map(|c| c.id), Some("next".to_string()), "{fragment}");
    }
}

#[test]
fn test_next_page_deduplicates_reordered_unique_query_pairs() {
    let controls = [link("next", "Next", "/list?page=3&sort=weight"), link("number", "3", "/list?sort=weight&page=3")];
    let found = next_page_control(&at("https://example.test/list?page=2&sort=weight", &controls));
    assert_eq!(found.map(|c| c.id), Some("next".to_string()));
}

#[test]
fn test_numeric_next_page_preserves_repeated_filter_value_order() {
    let page = at(
        "https://example.test/list?page=2&filter=first&filter=last",
        &[link("next", "3", "/list?page=3&filter=last&filter=first")],
    );
    assert!(next_page_control(&page).is_none());
}

#[test]
fn test_next_page_keeps_reordered_repeated_parameters_ambiguous() {
    let controls = [
        link("next", "Next", "/list?page=3&filter=first&filter=last"),
        link("other", "Next", "/list?page=3&filter=last&filter=first"),
    ];
    assert!(next_page_control(&at("https://example.test/list?page=2", &controls)).is_none());
}

#[test]
fn test_numeric_next_page_accepts_implicit_first_page() {
    for parameter in ["page", "pageno", "page_number"] {
        let page = at(
            "https://example.test/list?sort=weight",
            &[
                link("next", "2", &format!("/list?{parameter}=2&sort=weight")),
                link("later", "3", &format!("/list?{parameter}=3&sort=weight")),
                link("other", "2", &format!("/list?{parameter}=2&sort=name")),
            ],
        );
        assert_eq!(next_page_control(&page).map(|c| c.id), Some("next".to_string()), "{parameter}");
    }
}

// tests/test_download_gate.py

#[test]
fn test_a_link_that_saves_a_file_is_not_a_next_page() {
    // Paging clicks without the per-step gate, so a "next" link to an export must never be taken for a pager.
    let at_page_one =
        |control: Control| Observation { url: "https://addons.test/list/page-1".into(), ..observation(&[control]) };
    assert!(next_page_control(&at_page_one(link("next", "Next", "/list/page-2.csv"))).is_none());
    let pager = link("next", "Next", "/list/page-2");
    assert_eq!(next_page_control(&at_page_one(pager.clone())), Some(pager));
}

// tests/test_redrawn_read_recovery.py

const FILES: &str = "https://addons.example.test/project/files";
const DOWNLOAD: &str = "https://addons.example.test/project/download/1";

fn buttons_at(url: &str, labels: &[&str]) -> Observation {
    Observation { url: url.into(), ..observation(&labels.iter().map(|label| button(label)).collect::<Vec<_>>()) }
}

fn clicked(label: &str) -> HistoryEntry {
    executed(Some(Operation::Click), Some(label), true)
}

fn capture_at(parts: &[(BlockKind, &str)], url: &str) -> Capture {
    Capture { url: url.into(), ..capture(parts) }
}

/// A run that read the file list, then spent one recovery on it.
async fn recovered_on_read_list() -> (Agent, RunState) {
    let mut state = run_state().await;
    state.authorization = Authorization { irreversible_actions: true };
    let agent = agent();
    agent.settle(&mut state, &buttons_at(FILES, &["Files", "v1.14.1", "Download"]));
    state.notes.remember_capture(&capture_at(&[(BlockKind::Record, "v1.14.1 Oct 13")], FILES));
    state.history.push(executed(Some(Operation::Read), None, false));
    state.recoveries = 1;
    state.recovered_at = state.history.len();
    state.history.push(clicked("v1.14.1"));
    (agent, state)
}

#[tokio::test]
async fn test_a_read_page_redrawn_by_a_repeated_click_does_not_renew_recovery() {
    let (agent, mut state) = recovered_on_read_list().await;
    // The version row expanded: one more control at the same address, a state never seen before.
    agent.settle(&mut state, &buttons_at(FILES, &["Files", "v1.14.1", "Download", "Collapse"]));
    assert_eq!(state.recoveries, 1);
}

#[tokio::test]
async fn test_reaching_an_address_not_yet_read_still_renews_recovery() {
    let (agent, mut state) = recovered_on_read_list().await;
    agent.settle(&mut state, &buttons_at(DOWNLOAD, &["try again"]));
    assert_eq!(state.recoveries, 0);
}

#[tokio::test]
async fn test_a_read_page_redrawn_after_a_read_that_paid_out_still_renews_recovery() {
    let (agent, mut state) = recovered_on_read_list().await;
    // Loading more of a list keeps its address; a read that found new quotes since recovery is progress.
    let at = state.history.len() - 1;
    state.history.insert(at, HistoryEntry { read_progress: true, ..executed(Some(Operation::Read), None, false) });
    agent.settle(&mut state, &buttons_at(FILES, &["Files", "v1.14.1", "Download", "v1.14.0"]));
    assert_eq!(state.recoveries, 0);
}

#[tokio::test]
async fn test_a_new_step_at_the_same_address_still_renews_recovery() {
    let textbox = |label: &str| Control::new(&label.to_lowercase(), "textbox", label, [Operation::Fill]);
    let mut state = run_state().await;
    state.authorization = Authorization { irreversible_actions: true };
    let agent = agent();
    let first = Observation { url: FILES.into(), ..observation(&[textbox("Name"), button("Next")]) };
    agent.settle(&mut state, &first);
    state.notes.remember_capture(&capture_at(&[(BlockKind::Record, "Step 1 of 3")], FILES));
    state.recoveries = 1;
    state.recovered_at = state.history.len();
    state.history.push(clicked("Next"));
    let second = Observation { url: FILES.into(), ..observation(&[textbox("Email"), button("Next")]) };
    agent.settle(&mut state, &second);
    assert_eq!(state.recoveries, 0);
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
        _: i64,
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
        _: i64,
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

/// The comparison an owed read is answered from: a block never read, or a quoted value the page dropped, means the
/// notes no longer describe the page and a read is worth buying.
#[tokio::test]
async fn test_a_page_is_held_only_while_the_notes_still_describe_it() {
    let read = capture(&[(BlockKind::Paragraph, "Stoneware Mug 29.00")]);
    let here = Observation { document_key: "product".into(), ..observation(&[]) };
    let mut state = run_state().await;
    // Nothing of this document has been read, so the first read of it is always paid.
    assert!(!covered(&state, &here, &read));
    state.read_coverage.insert("product".into(), format!(" {}", text::squash(&read.text)));
    state.read_shas.insert("product".into(), HashSet::from([read.sha256.clone()]));
    state.notes.add(Fact::read(Some("r1"), "The mug costs 29.00", block_evidence(&read, "s0"), FactReader::Llm));
    assert!(covered(&state, &here, &capture(&[(BlockKind::Paragraph, "Stoneware Mug 29.00")])));
    // A DOM observation is the page reporting its own change, not text the notes could hold.
    let noted = capture(&[(BlockKind::Paragraph, "Stoneware Mug 29.00"), (BlockKind::Observation, "Sorted")]);
    assert!(covered(&state, &here, &noted));
    // A block this run has never read is something new the page has to say.
    let added = capture(&[(BlockKind::Paragraph, "Stoneware Mug 29.00"), (BlockKind::Paragraph, "In stock")]);
    assert!(!covered(&state, &here, &added));
    // A value the notes quote that the page no longer shows makes them out of date, even when every block on it
    // was read before: the page went back to a price an earlier read saw.
    let repriced = capture(&[(BlockKind::Paragraph, "Stoneware Mug 31.00")]);
    state.read_coverage.get_mut("product").unwrap().push_str(" Stoneware Mug 31.00");
    assert!(!covered(&state, &here, &repriced));
    // Another document's reading says nothing about this one.
    let elsewhere = Observation { document_key: "basket".into(), ..here };
    assert!(!covered(&state, &elsewhere, &read));
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
        agent.seen().redactor.register("PASSWORD", "hunter2", None);
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

// -- An authorized run whose task names the commit acts on it without a recovery ----------------------------------

/// A Jev that judges every commit irreversible and answers `requested` as given.
struct Gatekeeper(f64);

impl fastbrowse_core::decision::DecisionClient for Gatekeeper {
    fn evaluate<'a>(
        &'a self,
        _: &'a serde_json::Value,
        questions: &'a IndexMap<String, fastbrowse_core::decision::Question>,
    ) -> BoxFuture<'a, Result<fastbrowse_core::decision::Evaluation, fastbrowse_core::decision::DecisionError>> {
        use fastbrowse_core::decision::{Answer, NoulAnswer};
        let answers = questions
            .keys()
            .map(|key| {
                let probability = if key == "requested" { self.0 } else { 0.95 };
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
async fn test_an_authorized_pick_under_the_bar_acts_when_the_task_names_it() {
    use fastbrowse_core::safety::irreversible_question;
    for (requested, acts) in [(0.97, true), (0.76, true), (0.40, false)] {
        let agent = Agent::new(Arc::new(NoPage), Arc::new(Gatekeeper(requested)), Arc::new(ScriptedLLM));
        let mut state = run_state().await;
        state.task = "Check out and finish the order.".into();
        state.authorization = Authorization { irreversible_actions: true };
        let finish = button("Finish");
        let observation = observation(std::slice::from_ref(&finish));
        // Jev's pick on the Sauce Demo overview page: Finish at 0.86, under sensitive_act_from.
        let decision = Decision {
            operation_confidence: 0.86,
            ..super::helpers::code_decision(Operation::Click, Some(finish.clone()))
        };
        let question = irreversible_question(&state.task, Operation::Click, &finish, None);
        let gated =
            agent.gate_question(&mut state, &observation, &decision, Decider::Jev, "Finish", question, false).await;
        assert_eq!(gated.is_ok(), acts, "requested {requested}");
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

/// Picks, for every read requirement, the candidate whose value is the field's, and fails if it is not offered.
struct FieldReader(&'static str);

impl fastbrowse_core::decision::DecisionClient for FieldReader {
    fn evaluate<'a>(
        &'a self,
        _: &'a serde_json::Value,
        questions: &'a IndexMap<String, fastbrowse_core::decision::Question>,
    ) -> BoxFuture<'a, Result<fastbrowse_core::decision::Evaluation, fastbrowse_core::decision::DecisionError>> {
        use fastbrowse_core::decision::{Answer, ChoiceAnswer, Question};
        let answers = questions
            .iter()
            .map(|(key, question)| {
                let Question::Choice(question) = question else { panic!("{key} is not a read choice") };
                let (choice, _) = question
                    .criteria
                    .iter()
                    .find(|(_, offered)| offered.get("value").and_then(serde_json::Value::as_str) == Some(self.0))
                    .unwrap_or_else(|| panic!("{} is not offered: {:?}", self.0, question.criteria));
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
    let agent = Agent::new(Arc::new(NoPage), Arc::new(FieldReader(field)), Arc::new(ScriptedLLM));
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
    let (added, skipped) = agent.read(&mut state, Some(page), None, false).await.expect("a read");
    assert!(added && !skipped);
    let fact = state.notes.facts().into_iter().find(|f| f.requirement_id.as_deref() == Some("shown")).expect("a fact");
    assert_eq!(fact.reader, FactReader::JevChoice);
    assert_eq!(fact.text, field);
    assert_eq!(fact.evidence.as_ref().map(|e| e.quote.as_str()), Some(field));
}

// -- A stall is put to Jev first, and the LLM only hears of it when Jev stays unsure --------------------------------

/// A page whose uses are its 1280x900 viewport screenshot and the points clicked on it.
#[derive(Default)]
struct Screenshots(std::sync::Mutex<Vec<(f64, f64)>>);

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
        unreachable!("recovery does not observe")
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
        unreachable!("recovery does not watch")
    }

    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        Box::pin(async { Ok(png(1280, 900)) })
    }

    fn click_viewport(&self, x: f64, y: f64) -> BoxFuture<'_, PageResult<ActResult>> {
        self.0.lock().unwrap().push((x, y));
        let clicked = ActResult {
            outcome: StepOutcome::Executed,
            page_changed: true,
            detail: None,
            form_unchanged: false,
            moved: false,
        };
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
struct Recoverer(std::sync::atomic::AtomicUsize);

impl fastbrowse_core::llm::LLMClient for Recoverer {
    fn generate_value<'a>(
        &'a self,
        purpose: fastbrowse_core::models::LLMPurpose,
        _: &'a [fastbrowse_core::llm::Message],
        schema: &'a dyn fastbrowse_core::llm::ResponseSchema,
        _: i64,
        _: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<serde_json::Value>, fastbrowse_core::llm::LLMError>> {
        Box::pin(async move {
            assert_eq!(purpose, fastbrowse_core::models::LLMPurpose::Recover);
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let reply =
                json!({"diagnosis": "Stuck", "next_subgoal": "Scroll on", "give_up": false, "operation": "scroll"});
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

async fn click_where_seen(images: bool, shown: f64) -> (Option<ActResult>, Arc<Screenshots>, Arc<Seeing>) {
    let page = Arc::new(Screenshots::default());
    let jev = Arc::new(Seeing { images, shown, asked: Default::default() });
    let agent = Agent::new(page.clone(), jev.clone(), Arc::new(Recoverer::default()));
    let mut state = run_state().await;
    let clicked = agent.click_where_seen(&mut state, &button("Pick a date")).await.expect("asked");
    (clicked, page, jev)
}

#[tokio::test]
async fn test_a_covered_control_jev_sees_is_clicked_at_the_centre_of_its_box() {
    let (clicked, page, jev) = click_where_seen(true, 0.9).await;
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
    let (clicked, page, _) = click_where_seen(true, 0.2).await;
    assert!(clicked.is_none() && page.0.lock().unwrap().is_empty());
    let (clicked, page, jev) = click_where_seen(false, 0.9).await;
    assert!(clicked.is_none() && page.0.lock().unwrap().is_empty() && jev.asked.lock().unwrap().is_empty());
}
