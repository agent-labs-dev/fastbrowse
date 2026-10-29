//! tests/test_agent.py's cases, and those of its sibling files, about the loop's pure helpers and small state
//! transitions. Whole runs, and the async methods that need a scripted page and models, are replayed in
//! tests/agent.rs instead.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use fastbrowse_core::BoxFuture;
use fastbrowse_core::addresses::search_address;
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
    access_inspection, answer_evidence, answered, code_decision, follow_recovery, guessed, guessed_page,
    next_page_control, next_value, plan_ids, try_unsure, unread, unread_output_section, verified, visited,
    without_missing,
};
use super::reading::covered;
use super::state::{RunState, Spawned};
use crate::memory::{Fact, Notes, Tally, fact_id};
use crate::planner::{OutputTarget, Plan, Requirement, RequirementKind};
use crate::policy::{Decision, HistoryEntry};
use crate::retrieval::testing::capture;
use crate::scripted::{ScriptedJev, ScriptedLLM};
use crate::verification::{LLMVerdict, grounding};

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

/// Leave grouped checks open as `partition_plan` makes them, each with its field and subject.
fn open_outputs(state: &mut RunState, targets: &[(&str, &str)]) {
    for (name, subject) in targets {
        let key = format!("Report {name} for {subject}.");
        state
            .output_targets
            .insert(key.clone(), OutputTarget { field: name.to_string(), subject: subject.to_string() });
    }
    state.open_answer_outputs = targets.iter().map(|(name, subject)| format!("Report {name} for {subject}.")).collect();
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

fn entry(url: &str, entry: Option<&str>) -> Observation {
    Observation { history_entry: entry.map(String::from), ..at(url, &[]) }
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

fn fare(url: &str, sha: &str) -> Fact {
    Fact::read(Some("r1"), "$320", Evidence { url: url.into(), ..evidence(sha, 0, 4) }, FactReader::Llm)
}

fn fare_plan() -> Plan {
    Plan::new(vec![Requirement::new("r1", "Fare", INFO)], true)
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

/// An executed step; `moved` is whether the page reported that the step itself took it elsewhere.
fn did(state: &mut RunState, operation: Option<Operation>, target: Option<&str>, moved: bool) {
    state.history.push(executed(operation, target, true));
    state.moved_by_action = moved;
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
fn test_a_field_found_to_have_no_value_is_no_longer_offered_to_jev() {
    let name = Control { id: "name".into(), ..field("Enter Name") };
    let submit = Control::new("submit", "button", "Enter Name", [Operation::Click]);
    let page = observation(&[name, submit, field("Start Date")]);
    assert_eq!(without_missing(page.clone(), &HashSet::new()), page);
    let offered = without_missing(page.clone(), &HashSet::from([("Enter Name".to_string(), None)]));
    assert_eq!(offered.controls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["submit", "field"]);
    assert_eq!(page.controls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["name", "submit", "field"]);
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
async fn test_an_unsure_observation_move_is_acted_on_once_per_page_state() {
    let mut state = run_state().await;
    let (first, second) = (observation(&[button("Done")]), observation(&[button("Close dialog")]));
    let done = code_decision(Operation::Hover, Some(button("Done")));
    let close = code_decision(Operation::Hover, Some(button("Close")));
    assert!(try_unsure(&mut state, &first, &done));
    assert!(!try_unsure(&mut state, &first, &done));
    assert!(try_unsure(&mut state, &second, &close));
}

#[tokio::test]
async fn test_an_unsure_state_changing_action_needs_a_second_opinion() {
    for label in ["Sign out", "Continue with Google", "Unrelated action"] {
        let mut state = run_state().await;
        let obs = observation(&[button(label)]);
        let decision = code_decision(Operation::Click, Some(obs.controls[0].clone()));
        assert!(!try_unsure(&mut state, &obs, &decision), "{label}");
    }
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

fn cited_notes(ids: &[&str]) -> Notes {
    Notes::new(
        ids.iter().enumerate().map(|(i, r)| Fact::read(Some(r), r, evidence("capture", i as i64, 4), FactReader::Llm)),
    )
}

fn verdict(complete: bool, missing: &[&str], ungrounded: &[&str]) -> LLMVerdict {
    LLMVerdict {
        complete,
        missing: missing.iter().map(|id| id.to_string()).collect(),
        ungrounded: ungrounded.iter().map(|id| id.to_string()).collect(),
    }
}

#[test]
fn test_the_verifier_cannot_hold_open_a_requirement_the_notes_cite() {
    let plan = Plan::new(
        vec![
            Requirement::new("httpx", "Find httpx's latest release date", INFO),
            Requirement::new("requests", "Find requests' latest release date", INFO),
            Requirement::new("compare", "Compare the two dates", INFO),
            Requirement::new("signed-in", "Be signed in", RequirementKind::Action),
        ],
        true,
    );
    let notes = cited_notes(&["httpx", "compare"]);
    for (complete, missing, accepted) in [
        (true, &[][..], true),
        (false, &["compare"][..], true),
        (false, &["signed-in"][..], false),
        (false, &["requests"][..], false),
        (false, &[][..], false),
    ] {
        let doubt = verdict(complete, missing, &[]);
        assert_eq!(verified(&doubt, &plan, &notes, &HashSet::new(), None, &[]), accepted, "{complete} {missing:?}");
    }
}

#[test]
fn test_evidence_does_not_excuse_a_requirement_read_off_a_guessed_address() {
    let plan = Plan::new(
        vec![
            Requirement::new("httpx", "Find httpx's latest release date", INFO),
            Requirement::new("compare", "Compare the two dates", INFO),
        ],
        true,
    );
    let notes = cited_notes(&["httpx", "compare"]);
    for (ungrounded, invented, accepted, complete) in [
        (&[][..], &["https://example.test/"][..], true, true),
        // The citing excusal cannot see this: the requirement has evidence, read on an address the run guessed.
        (&["httpx"][..], &["https://example.test/?q=httpx"][..], false, true),
        // Read on the page the caller named or one the run clicked to, the doubt is the verifier's alone. The
        // synthesis requirement is satisfied from notes, and no page ever shows a comparison.
        (&["httpx"][..], &[][..], true, true),
        (&["compare"][..], &[][..], true, true),
        // A verifier that calls the run incomplete only for the excused doubt is excused with it, as for missing.
        (&["httpx"][..], &[][..], true, false),
        // A verdict naming something the plan never asked for says nothing about this run.
        (&["invented-id"][..], &["https://example.test/"][..], true, true),
    ] {
        let doubt = verdict(complete, &[], ungrounded);
        assert_eq!(verified(&doubt, &plan, &notes, &set(invented), None, &[]), accepted, "{ungrounded:?} {invented:?}");
    }
}

#[tokio::test]
async fn test_identical_text_at_another_address_does_not_free_a_guessed_reading() {
    let (guess, own) = ("https://example.test/flights?q=BRS+to+YRK", "https://example.test/flights?tfs=set");
    let mut state = run_state().await;
    (state.invented, state.guessing) = (set(&[guess]), true);
    let plan = fare_plan();
    state.lineage.read("same", &[guess.to_string()], true);
    let on_guess = fare(guess, "same");
    state.notes.add(on_guess.clone());
    let judge = Arc::new(guessed_page(&state.invented, Some(&state.lineage), false));
    state.notes.discredit(["r1"], judge);

    state.guessing = false;
    state.lineage.read("same", &[own.to_string()], false);
    state.notes.add(on_guess);
    assert_eq!(state.notes.unresolved(&plan), plan.requirements);
    let on_own = fare(own, "same");
    state.notes.add(on_own.clone());
    let supporting: HashSet<String> = state.notes.supporting("r1").into_iter().map(|(key, _)| key).collect();
    assert_eq!(supporting, HashSet::from([fact_id(&on_own)]));
}

const GUESS: &str = "https://example.test/flights?q=BRS+to+YRK";
const START: &str = "https://example.test/flights";
const OTHER_GUESS: &str = "https://example.test/flights?q=BRS+to+YRK+one+way";

/// One step of a lineage case: the operation and its target, where it landed, the history entry and whether the
/// action itself moved the page.
type LineageStep = (Option<Operation>, Option<&'static str>, String, Option<&'static str>, bool);

#[tokio::test]
async fn test_lineage_follows_the_browser_history_entry() {
    use Operation::{Back, Click, Navigate, SwitchTab};
    let step = |operation, target, url: &str, entry| -> LineageStep { (operation, target, url.into(), entry, false) };
    let moved = |operation, target, url: &str, entry| -> LineageStep { (operation, target, url.into(), entry, true) };
    let cases: Vec<(&str, Vec<LineageStep>, bool)> = vec![
        // Back to an entry made on the guess restores it; Forward to one made off it restores that.
        (
            "back-to-guess",
            vec![step(Some(Navigate), Some(START), START, Some("t:2")), step(Some(Back), None, GUESS, Some("t:1"))],
            true,
        ),
        (
            "forward-off-guess",
            vec![
                step(Some(Navigate), Some(START), START, Some("t:2")),
                step(Some(Back), None, GUESS, Some("t:1")),
                step(None, None, START, Some("t:2")),
            ],
            false,
        ),
        // Back past the start loads the caller's page afresh; it was never built.
        ("back-past-start", vec![step(Some(Back), None, START, Some("t:0"))], false),
        // A reload or a redraw is the same entry.
        ("reload-on-guess", vec![step(Some(Click), None, GUESS, Some("t:1"))], true),
        (
            "reload-off-guess",
            vec![step(Some(Navigate), Some(START), START, Some("t:2")), step(Some(Click), None, START, Some("t:2"))],
            false,
        ),
        // A redirect is the navigation it follows: what the run asked for decides, as well as where it landed.
        ("redirect-of-guess", vec![step(Some(Navigate), Some(GUESS), "https://example.test/moved", Some("t:2"))], true),
        ("redirect-of-own", vec![step(Some(Navigate), Some(START), "https://example.test/home", Some("t:2"))], false),
        // An entry the run's action on the site's controls itself made is the site's, whatever its path, unless it
        // is a built address again; any other the page made, even just after an action, inherits the page.
        ("site-search", vec![moved(Some(Click), Some("Search"), &format!("{START}?q=BRS+to+LDS"), Some("t:2"))], false),
        ("site-sort", vec![moved(Some(Click), None, &format!("{GUESS}&sort=price"), Some("t:2"))], false),
        ("site-resubmits-guess", vec![moved(Some(Click), Some("Search"), GUESS, Some("t:2"))], true),
        ("page-pushstate-on-guess", vec![step(None, None, &format!("{GUESS}&page=2"), Some("t:2"))], true),
        ("page-pushstate-after-click", vec![step(Some(Click), None, &format!("{GUESS}&ts=1"), Some("t:2"))], true),
        // A NAVIGATE to another address the run built is still a guess.
        ("navigate-to-other-guess", vec![step(Some(Navigate), Some(OTHER_GUESS), OTHER_GUESS, Some("t:2"))], true),
        (
            "pushstate-off-guess",
            vec![step(Some(Navigate), Some(START), START, Some("t:2")), step(Some(Click), None, GUESS, Some("t:3"))],
            false,
        ),
        // replaceState keeps the entry: the page rewriting its own address keeps its lineage, and the site moving it
        // elsewhere after the run's action is the site's.
        ("page-replacestate", vec![step(None, None, "https://example.test/other", Some("t:1"))], true),
        ("site-replacestate", vec![moved(Some(Click), None, "https://example.test/other", Some("t:1"))], false),
        (
            "page-replacestate-after-click",
            vec![step(Some(Click), None, "https://example.test/other", Some("t:1"))],
            true,
        ),
        // A tab the page opened by itself inherits the page that opened it, one the run's click opened is the
        // site's, and switching back restores the opener's entry.
        ("page-popup-from-guess", vec![step(None, None, "https://example.test/popup", Some("u:1"))], true),
        ("site-popup", vec![moved(Some(Click), None, "https://example.test/popup", Some("u:1"))], false),
        (
            "switch-back-to-guess",
            vec![
                step(Some(Navigate), Some(START), START, Some("t:2")),
                step(Some(Click), None, "https://example.test/popup", Some("u:1")),
                step(Some(SwitchTab), None, GUESS, Some("t:1")),
            ],
            true,
        ),
        // A browser that names no entry is judged as a new entry each time.
        ("no-entry", vec![step(Some(Navigate), Some(START), START, None)], false),
    ];
    let agent = agent();
    for (id, steps, guessing) in cases {
        let mut state = run_state().await;
        (state.invented, state.guessing) = (set(&[GUESS, OTHER_GUESS]), true);
        agent.note_effect(&mut state, &entry(GUESS, Some("t:1")));
        for (operation, target, url, history_entry, moved) in steps {
            did(&mut state, operation, target, moved);
            agent.note_effect(&mut state, &entry(&url, history_entry));
        }
        assert_eq!(state.guessing, guessing, "{id}");
    }
}

#[tokio::test]
async fn test_back_to_a_search_rebuilt_at_the_guessed_address_stays_the_runs_own() {
    let agent = agent();
    let mut state = run_state().await;
    (state.invented, state.guessing) = (set(&[GUESS]), true);
    agent.note_effect(&mut state, &entry(GUESS, Some("t:1")));
    state.lineage.read("guess", &[GUESS.to_string()], state.guessing);
    for (operation, target, url, history_entry) in [
        (Operation::Navigate, Some(START), START, "t:2"),
        (Operation::Click, Some("Search"), GUESS, "t:3"),
        (Operation::Click, Some("Cheapest result"), "https://example.test/flights/result/1", "t:4"),
        (Operation::Back, None, GUESS, "t:3"),
    ] {
        did(&mut state, Some(operation), target, false);
        agent.note_effect(&mut state, &entry(url, Some(history_entry)));
    }
    assert!(!state.guessing);
    state.lineage.read("rebuilt", &[GUESS.to_string()], state.guessing);
    let plan = fare_plan();
    let judged = |sha| guessed(&plan, &Notes::new([fare(GUESS, sha)]), &state.invented, false, Some(&state.lineage));
    assert_eq!(judged("rebuilt"), set(&[]));
    assert_eq!(judged("guess"), set(&["r1"]));
}

#[tokio::test]
async fn test_a_corrected_search_kept_in_the_fragment_is_the_runs_own() {
    let (old, new) = ("https://example.test/search#from=BRS&to=YRK", "https://example.test/search#from=BRS&to=LDS");
    let agent = agent();
    let mut state = run_state().await;
    (state.invented, state.guessing) = (set(&[old]), true);
    agent.note_effect(&mut state, &entry(old, Some("t:1")));
    state.lineage.read("old", &[old.to_string()], state.guessing);
    did(&mut state, Some(Operation::Navigate), Some(new), false);
    agent.note_effect(&mut state, &entry(new, Some("t:2")));
    assert!(!state.guessing);
    state.lineage.read("new", &[new.to_string()], state.guessing);
    let plan = fare_plan();
    let new_fare = Notes::new([fare(new, "new")]);
    assert_eq!(guessed(&plan, &new_fare, &state.invented, false, Some(&state.lineage)), set(&[]));
    did(&mut state, Some(Operation::Navigate), Some(old), false);
    agent.note_effect(&mut state, &entry(old, Some("t:3")));
    assert!(state.guessing);
    did(&mut state, Some(Operation::Click), None, false);
    agent.note_effect(&mut state, &entry(old, Some("t:3")));
    assert!(state.guessing);
    let old_fare = Notes::new([fare(old, "old")]);
    assert_eq!(guessed(&plan, &old_fare, &state.invented, false, Some(&state.lineage)), set(&["r1"]));
}

#[tokio::test]
async fn test_a_guessed_address_reached_again_by_the_run_itself_is_judged_by_that_visit() {
    let agent = agent();
    let mut state = run_state().await;
    (state.invented, state.guessing) = (set(&[GUESS]), true);
    agent.note_effect(&mut state, &at(GUESS, &[]));
    state.lineage.read("guess", &[GUESS.to_string()], state.guessing);
    for (operation, landed) in [(Operation::Navigate, START), (Operation::Click, GUESS)] {
        state.history.push(executed(Some(operation), None, true));
        agent.note_effect(&mut state, &at(landed, &[]));
    }
    state.lineage.read("fresh", &[GUESS.to_string()], state.guessing);
    let plan = fare_plan();
    let judged = |sha| guessed(&plan, &Notes::new([fare(GUESS, sha)]), &state.invented, false, Some(&state.lineage));
    assert_eq!(judged("fresh"), set(&[]));
    assert_eq!(judged("guess"), set(&["r1"]));
    // A redraw of the page the run is on, with no new arrival, keeps the judgement it arrived with.
    agent.note_effect(&mut state, &at(GUESS, &[]));
    assert_eq!(state.lineage.address(&search_address(GUESS)), Some(false));
}

#[tokio::test]
async fn test_a_refused_reading_is_freed_when_the_run_reads_the_same_text_off_the_guess() {
    let agent = agent();
    let mut state = run_state().await;
    (state.invented, state.guessing) = (set(&[GUESS]), true);
    let plan = fare_plan();
    agent.note_effect(&mut state, &at(GUESS, &[]));
    state.lineage.read("same", &[GUESS.to_string()], state.guessing);
    let read = fare(GUESS, "same");
    state.notes.add(read.clone());
    let judge = Arc::new(guessed_page(&state.invented, Some(&state.lineage), false));
    state.notes.discredit(["r1"], judge);
    state.notes.add(read.clone());
    assert_eq!(state.notes.unresolved(&plan), plan.requirements);

    for (operation, landed) in [(Operation::Navigate, START), (Operation::Click, GUESS)] {
        state.history.push(executed(Some(operation), None, true));
        agent.note_effect(&mut state, &at(landed, &[]));
    }
    state.lineage.read("same", &[GUESS.to_string()], state.guessing);
    state.notes.add(read);
    assert!(state.notes.unresolved(&plan).is_empty());
    assert_eq!(guessed(&plan, &state.notes, &state.invented, false, Some(&state.lineage)), set(&[]));
}

#[tokio::test]
async fn test_leaving_a_guess_for_the_callers_page_by_any_navigation_makes_later_reads_the_runs_own() {
    let set_url = "https://example.test/flights?tfs=set";
    for left_by in [Operation::Navigate, Operation::Back] {
        let agent = agent();
        let mut state = run_state().await;
        (state.invented, state.guessing) = (set(&[GUESS]), true);
        agent.note_effect(&mut state, &at(GUESS, &[]));
        for (operation, landed) in [(left_by, START), (Operation::Click, set_url)] {
            state.history.push(executed(Some(operation), None, true));
            agent.note_effect(&mut state, &at(landed, &[]));
        }
        state.lineage.read("set", &[set_url.to_string()], state.guessing);
        let notes = Notes::new([fare(set_url, "set")]);
        assert_eq!(guessed(&fare_plan(), &notes, &state.invented, false, Some(&state.lineage)), set(&[]), "{left_by}");
    }
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
fn test_tallies_keep_guessed_sources_subject_to_verification() {
    let source = Evidence {
        quote: "A quote by Ada".into(),
        url: "https://example.test/quotes?tag=ada".into(),
        ..evidence("capture", 0, 4)
    };
    let quote = Fact::read(None, &source.quote, source.clone(), FactReader::Llm);
    let mut notes = Notes::new([quote.clone()]);
    notes.add_tally(Tally { requirement_id: "r".into(), key: "Ada".into(), records: vec![fact_id(&quote)] }).unwrap();
    notes.complete_tallies("r");
    let plan = Plan::new(vec![Requirement::new("r", "Count quotes", INFO)], true);
    assert_eq!(guessed(&plan, &notes, &set(&[&source.url]), false, None), set(&["r"]));
    assert!(grounding(&notes, &plan, &[]).contains(&source.url));
}

#[test]
fn test_a_verifier_naming_a_requirement_by_another_spelling_names_that_requirement() {
    let plan = Plan::new(
        vec![Requirement::new("req-1", "a", INFO), Requirement::new("req-2", "b", RequirementKind::Action)],
        true,
    );
    let named: Vec<String> = ["req_1", "2", "REQ-2", "req-9", "summary"].map(String::from).to_vec();
    assert_eq!(plan_ids(&named, &plan), set(&["req-1", "req-2"]));
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

// tests/test_recovery_sections.py

fn titled(url: &str, title: &str, controls: &[Control]) -> Observation {
    Observation { url: url.into(), title: title.into(), ..observation(controls) }
}

#[tokio::test]
async fn test_file_records_do_not_become_output_sections() {
    let mut state = run_state().await;
    open_outputs(&mut state, &[("links", "birch")]);
    let main = "https://example.test/projects/birch";
    let files = format!("{main}/files");
    let seen = titled(&files, "Birch files", &[link("file", "version 1.2", &format!("{files}/123"))]);
    agent().note_effect(&mut state, &seen);
    state.read_urls.extend([main.to_string(), files]);
    assert_eq!(unread_output_section(&state), None);
}

#[tokio::test]
async fn test_directory_and_other_targets_are_not_sections_of_this_target() {
    let mut state = run_state().await;
    open_outputs(&mut state, &[("links", "Project Birch")]);
    let root = "https://example.test/projects/birch";
    let seen =
        titled(root, "Project Birch", &[link("list", "Projects", "/projects"), link("other", "Elm", "/projects/elm")]);
    agent().note_effect(&mut state, &seen);
    state.read_urls.insert(root.into());
    assert_eq!(unread_output_section(&state), None);
}

#[tokio::test]
async fn test_an_exhausted_output_does_not_hide_another_targets_unread_section() {
    let mut state = run_state().await;
    open_outputs(&mut state, &[("links", "Elm"), ("links", "Birch")]);
    let root = "https://example.test/projects/birch";
    let seen = titled(&format!("{root}/comments"), "Birch", &[link("description", "Description", root)]);
    agent().note_effect(&mut state, &seen);
    assert_eq!(unread_output_section(&state).as_deref(), Some(root));
}

#[tokio::test]
async fn test_a_target_name_holding_for_keeps_its_whole_name() {
    let mut state = run_state().await;
    open_outputs(&mut state, &[("links", "Leatrix Plus for Classic")]);
    let root = "https://example.test/addons/leatrix-plus-for-classic";
    let seen = titled(
        root,
        "Leatrix Plus for Classic",
        &[
            link("classic", "Classic", "/classic-era"),
            link("news", "News", "/classic-era/news"),
            link("description", "Description", &format!("{root}/description")),
        ],
    );
    agent().note_effect(&mut state, &seen);
    state.read_urls.insert(root.into());
    assert_eq!(unread_output_section(&state), Some(format!("{root}/description")));
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
    for (requested, acts) in [(0.97, true), (0.40, false)] {
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
