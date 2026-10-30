//! Port of `src/fastbrowse/verification.py`: deciding whether a run is actually finished, and whether its answer and
//! extracted data hold up.
//!
//! Jev checks completion, individual action requirements and draft quality. An LLM looks at a screenshot when those
//! answers leave completion uncertain.
//!
//! # Concurrency
//!
//! Python sends the transaction question beside the claim questions with `asyncio.gather(..., return_exceptions=True)`.
//! A replay keys each request on its body, so what has to match Python is the set of requests and every decision, not
//! the order they leave in. Here such a group runs through [`crate::support::join2`], which polls both to their end
//! before any error is looked at, so a failure never leaves a request Python sent unsent. Errors are then raised in the
//! order Python raises them: the claim questions before the transaction question.

mod prompts;

use fastbrowse_core::config::{Thresholds, TokenBudget};
use fastbrowse_core::decision::{Answer, DecisionClient, NoulQuestion, Question};
use fastbrowse_core::llm::LLMClient;
use fastbrowse_core::models::{CostBasis, CostComponent, CostLine, Evidence};
use fastbrowse_core::output_schema::OutputModel;
use fastbrowse_core::page::{Capture, Control, Observation, cut_text};
use fastbrowse_core::py::dump::{Dump, include};
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::py::text;
use fastbrowse_core::telemetry::{Ledger, Trace};
use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::error::{AgentError, Result};
use crate::memory::Notes;
use crate::planner::{Plan, RequirementKind};
use crate::policy::HistoryEntry;
use crate::retrieval::{
    ComposedAnswer, ScalarField, UnsupportedField, choose_candidate, field_candidates, field_candidates_from_notes,
    merge_candidates, propose_text_fields, propose_text_fields_from_notes,
};

str_enum! {
    pub enum DoneVerdict {
        Accept = "accept",
        Reject = "reject",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DoneCheck {
    pub verdict: DoneVerdict,
    /// Requirement ids that are not satisfied or not evidenced.
    pub unmet: Vec<String>,
    /// The offered draft when Jev judged it already answers the task, so no composer needs to run.
    pub answer: Option<ComposedAnswer>,
    pub cost: CostLine,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Extraction {
    pub data: Option<Value>,
    pub evidence: Vec<Evidence>,
    pub problem: Option<String>,
}

fn controls<'a>(controls: impl IntoIterator<Item = &'a Control>) -> Vec<Value> {
    controls
        .into_iter()
        .map(|control| {
            include(control.dump_exclude_none(), &["label", "context", "role", "value", "checked", "selected"])
        })
        .collect()
}

/// The controls holding a value, a check or a selection: the state page text does not show.
fn stateful(controls: &[Control]) -> Vec<&Control> {
    controls.iter().filter(|c| c.value.is_some() || c.checked.is_some() || c.selected.is_some()).collect()
}

/// `page_state`: the page, its stateful controls and the notes for a Jev verdict within Jev's documented input limit,
/// the notes' requirement evidence kept whole. The page text is cut when the rest leaves it no room, and a state that
/// still does not fit is refused rather than sent.
pub fn page_state(
    observation: &Observation,
    notes: &Notes,
    tokens: &TokenBudget,
    questions: &[String],
    context: Option<&Map<String, Value>>,
) -> Result<Value> {
    let mut state: Map<String, Value> = context.cloned().unwrap_or_default();
    // The dropdown choice runs apart from the action choice; it also needs today's date for relative months.
    state.insert("date".into(), observation.today().into());
    state.insert(
        "page".into(),
        json!({"url": observation.url, "title": observation.title, "text": observation.viewport_text}),
    );
    // Inputs and ARIA selection states are absent from innerText; without them a preview can pass completion even
    // though the requested filters were never applied.
    state.insert("controls".into(), Value::Array(controls(&observation.controls)));
    state.insert("notes".into(), "".into());
    let budget = tokens.input_chars(questions, true);
    let size = |state: &Map<String, Value>| text::len(&dumps(&Value::Object(state.clone()))) as i64;
    let room = |state: &Map<String, Value>| (budget - size(state)).max(0);
    let mut cut_page = false;
    match notes.render(room(&state), true, true) {
        Ok(rendered) => {
            state.insert("notes".into(), rendered.into());
        }
        Err(AgentError::NotesTooLarge(_)) => {
            // Preserve requirement evidence by cutting page text, then stateless controls, before refusing a verdict.
            cut_page = true;
            state["page"]["text"] = "".into();
            match notes.render(room(&state), true, true) {
                Ok(rendered) => {
                    state.insert("notes".into(), rendered.into());
                }
                Err(AgentError::NotesTooLarge(_)) => {
                    let kept = stateful(&observation.controls);
                    state.insert("controls_omitted".into(), (observation.controls.len() - kept.len()).into());
                    state.insert("controls".into(), Value::Array(controls(kept)));
                    let rendered = notes.render(room(&state), true, true)?;
                    state.insert("notes".into(), rendered.into());
                }
                Err(other) => return Err(other),
            }
        }
        Err(other) => return Err(other),
    }
    if cut_page || size(&state) > budget {
        state["page"]["text"] = "".into();
        state["page"]["text"] = cut_text(&observation.viewport_text, room(&state) as usize, true).into();
    }
    if size(&state) > budget {
        return Err(AgentError::ObservationTooLarge(format!(
            "the state on {} exceeds Jev's input limits with no page text",
            observation.url
        )));
    }
    Ok(Value::Object(state))
}

/// The done check's state with `steps`, the run's actions already in `context`, the oldest dropped and counted when
/// the request would otherwise run over Jev's input limit. As in a step request, actions go before the page text is
/// cut, and the page text is cut only when dropping every action is not enough.
fn done_state(
    observation: &Observation,
    notes: &Notes,
    tokens: &TokenBudget,
    questions: &[String],
    context: &Map<String, Value>,
    steps: &[String],
) -> Result<Value> {
    let attempt = |dropped: usize| {
        let mut context = context.clone();
        if dropped > 0 {
            let omitted = format!("[{dropped} earlier actions omitted to fit the request]");
            let lines: Vec<&str> =
                [omitted.as_str()].into_iter().chain(steps[dropped..].iter().map(String::as_str)).collect();
            context.insert("actions".into(), lines.join("\n").into());
        }
        page_state(observation, notes, tokens, questions, Some(&context))
    };
    let page = observation.viewport_text.as_str();
    let whole = |state: &Result<Value>| matches!(state, Ok(state) if state["page"]["text"].as_str() == Some(page));
    let full = attempt(0);
    if steps.is_empty() || whole(&full) {
        return full;
    }
    let page_kept = whole(&attempt(steps.len()));
    let (mut low, mut high) = (0, steps.len());
    while low < high {
        let middle = (low + high) / 2;
        let state = attempt(middle);
        if if page_kept { whole(&state) } else { state.is_ok() } {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    attempt(low)
}

/// `check_done`'s keyword arguments.
#[derive(Default)]
pub struct DoneOptions<'a> {
    pub tokens: TokenBudget,
    /// The run's action record. A page shows only where a process ended, so an accepted check that never saw the
    /// record would pass "enter one name, go back and correct it" on a run that typed the correction first.
    pub history: &'a [HistoryEntry],
    /// Every address the run has been on, first the one it began on, which no action records.
    pub visited: &'a [String],
    /// Reserves the one Jev call, made only when the check reaches Jev.
    pub ledger: Option<&'a Ledger>,
    pub trace: Trace,
}

fn probability(answers: &IndexMap<String, Answer>, key: &str) -> f64 {
    match answers.get(key) {
        Some(Answer::Noul(answer)) => answer.probability,
        _ => 0.0,
    }
}

/// Judge completion, and whether `draft` answers the task as written, in the one Jev call.
#[allow(clippy::too_many_arguments)]
pub async fn check_done(
    jev: &dyn DecisionClient,
    task: &str,
    plan: &Plan,
    observation: &Observation,
    notes: &Notes,
    thresholds: &Thresholds,
    draft: Option<&ComposedAnswer>,
    options: &DoneOptions<'_>,
) -> Result<DoneCheck> {
    let mut questions: IndexMap<String, Question> = IndexMap::new();
    let mut unmet: Vec<String> = plan
        .requirements
        .iter()
        .filter(|r| r.kind == RequirementKind::Information && !notes.evidenced(&r.id))
        .map(|r| r.id.clone())
        .collect();
    unmet.sort();
    for requirement in plan.requirements.iter().filter(|r| r.kind == RequirementKind::Action) {
        questions.insert(
            format!("unmet_{}", requirement.id),
            NoulQuestion::new(
                prompts::unmet_action(&requirement.text),
                Some("It is not satisfied, or there is no visible confirmation."),
                Some("The page visibly confirms it is satisfied."),
            )
            .into(),
        );
    }
    let mut draft = draft;
    if draft.is_some() && unmet.is_empty() {
        // Asked here rather than on its own because this call is already being paid for: judging the draft costs one
        // more answer in a request the run makes anyway, where a composer costs seconds.
        questions.insert(
            "draft_needs_writing".into(),
            NoulQuestion::new(
                prompts::draft_needs_writing(),
                Some("Yes, it needs rewriting before it answers the task."),
                Some("No, it answers the task as written."),
            )
            .into(),
        );
    }
    let mut context = Map::new();
    context.insert("task".into(), task.into());
    context.insert("run_reports".into(), json!(plan.run_reports));
    let steps: Vec<String> = options.history.iter().map(step).collect();
    if !steps.is_empty() {
        context.insert("actions".into(), steps.join("\n").into());
    }
    if !options.visited.is_empty() {
        context.insert("visited".into(), json!(options.visited));
    }
    if let Some(draft) = draft {
        context.insert("draft".into(), draft.answer.clone().into());
    }
    let written = |questions: &IndexMap<String, Question>| questions.values().map(Dump::dump_json).collect::<Vec<_>>();
    let state = match done_state(observation, notes, &options.tokens, &written(&questions), &context, &steps) {
        Ok(state) => state,
        Err(AgentError::NotesTooLarge(message)) => {
            if draft.is_none() {
                return Err(AgentError::NotesTooLarge(message));
            }
            // The optional answer draft crowded out the quotes required to judge completion.
            context.shift_remove("draft");
            questions.shift_remove("draft_needs_writing");
            draft = None;
            done_state(observation, notes, &options.tokens, &written(&questions), &context, &steps)?
        }
        Err(other) => return Err(other),
    };
    if !unmet.is_empty() || questions.is_empty() {
        let verdict = if unmet.is_empty() { DoneVerdict::Accept } else { DoneVerdict::Reject };
        return Ok(DoneCheck {
            verdict,
            unmet,
            answer: None,
            cost: CostLine::new(CostComponent::Jev, CostBasis::Estimated, Some(0.0)),
        });
    }
    if let Some(ledger) = options.ledger {
        ledger.reserve(CostComponent::Jev, 0.0, 1)?;
    }
    let evaluation = jev.evaluate(&state, &questions).await?;
    let answers = &evaluation.answers;
    for requirement in &plan.requirements {
        if probability(answers, &format!("unmet_{}", requirement.id)) > thresholds.claim_problem_above {
            unmet.push(requirement.id.clone());
        }
    }
    let verdict = if unmet.is_empty() { DoneVerdict::Accept } else { DoneVerdict::Reject };
    // An answer Jev did not give is not a yes: only a present, confident "no rewrite needed" skips the composer.
    let rewrite = match answers.get("draft_needs_writing") {
        Some(Answer::Noul(doubt)) => Some(doubt.probability),
        _ => None,
    };
    let ready = rewrite.is_some_and(|p| p < thresholds.rewrite_from);
    if let Some(draft) = draft {
        options.trace.event("draft", || {
            let mut fields = Map::new();
            fields.insert("rewrite".into(), json!(rewrite));
            fields.insert("ready".into(), ready.into());
            fields.insert("claims".into(), draft.claims.len().into());
            fields
        });
    }
    Ok(DoneCheck { verdict, unmet, answer: if ready { draft.cloned() } else { None }, cost: evaluation.cost })
}

/// One action as the run recorded it: the value typed and what it visibly did, both already redacted.
fn step(entry: &HistoryEntry) -> String {
    let operation = entry.operation.map_or("open", |op| op.as_str());
    let action = format!("{operation} {}", entry.target.as_deref().unwrap_or(""));
    let action = text::strip(&action);
    let typed = entry.text.as_ref().map(|t| format!(" = {}", text::repr(t))).unwrap_or_default();
    let effect = entry.effect.as_ref().filter(|e| !e.is_empty()).map(|e| format!(" ({e})")).unwrap_or_default();
    format!("- {action}{typed} -> {}{effect}", entry.outcome.as_str())
}

/// The record a caller asked for, as `extract` reads it: each field by name as retrieval copies it, and the model
/// whose validation the extracted values must pass.
#[derive(Debug, Clone)]
pub struct RecordSchema {
    pub fields: IndexMap<String, ScalarField>,
    pub model: OutputModel,
}

impl RecordSchema {
    /// The record an output schema describes, its fields under their Python names.
    pub fn from_output(model: OutputModel) -> RecordSchema {
        let fields = model.fields().iter().map(|field| (field.name.clone(), ScalarField::from_output(field))).collect();
        RecordSchema { fields, model }
    }
}

/// `extract`'s keyword arguments.
#[derive(Default)]
pub struct ExtractOptions<'a> {
    pub notes: Option<&'a Notes>,
    pub tokens: TokenBudget,
    pub ledger: Option<&'a Ledger>,
}

/// Text fields are proposed by the LLM and kept only when quoted verbatim from the page; other scalars are copied from
/// the typed spans Jev points at. A field with no supported value fails the extraction.
pub async fn extract(
    jev: &dyn DecisionClient,
    llm: &dyn LLMClient,
    task: &str,
    capture: &Capture,
    schema: &RecordSchema,
    options: &ExtractOptions<'_>,
) -> Result<Extraction> {
    use crate::retrieval::Annotation;
    let mut values: Map<String, Value> = Map::new();
    let mut evidence: Vec<Evidence> = Vec::new();
    let text_fields: IndexMap<String, ScalarField> = schema
        .fields
        .iter()
        .filter(|(_, f)| f.annotation == Annotation::Str)
        .map(|(k, f)| (k.clone(), f.clone()))
        .collect();
    if !text_fields.is_empty() {
        // Notes first: they hold every page a comparison read, where the final page shows one side of it.
        let mut proposed = match options.notes {
            Some(notes) => {
                propose_text_fields_from_notes(
                    llm,
                    task,
                    notes,
                    &text_fields,
                    Some(capture),
                    &options.tokens,
                    options.ledger,
                )
                .await?
            }
            None => IndexMap::new(),
        };
        let unseen: IndexMap<String, ScalarField> = text_fields
            .iter()
            .filter(|(name, _)| !proposed.contains_key(*name))
            .map(|(k, f)| (k.clone(), f.clone()))
            .collect();
        if !unseen.is_empty() {
            let (found, _) =
                propose_text_fields(llm, task, capture, &unseen, None, &options.tokens, options.ledger).await?;
            proposed.extend(found);
        }
        for (name, (value, quoted)) in proposed {
            values.insert(name, value.into());
            evidence.push(quoted);
        }
    }
    let record_fields: Vec<String> = schema.fields.keys().cloned().collect();
    for (name, field) in &schema.fields {
        if text_fields.contains_key(name) {
            continue;
        }
        let mut candidates = match field_candidates(capture, field) {
            Ok(candidates) => candidates,
            Err(UnsupportedField { reason }) => {
                return Ok(Extraction { data: None, evidence: Vec::new(), problem: Some(format!("{name}: {reason}")) });
            }
        };
        if let Some(notes) = options.notes {
            // The notes hold every page the run read; the final capture holds only the one it ended on, which for a
            // sorted comparison is one side of it. Both pages are offered, the current one first.
            match field_candidates_from_notes(notes, field, Some(capture)) {
                Ok(quoted) => candidates = merge_candidates(&[&candidates, &quoted]),
                Err(UnsupportedField { reason }) => {
                    return Ok(Extraction {
                        data: None,
                        evidence: Vec::new(),
                        problem: Some(format!("{name}: {reason}")),
                    });
                }
            }
        }
        if candidates.is_empty() {
            continue;
        }
        let state = json!({"task": task, "page": {"url": capture.url, "title": capture.title}});
        match choose_candidate(jev, &state, field, &candidates, name, task, &record_fields, options.ledger).await {
            Ok(Some((value, quoted))) => {
                values.insert(name.clone(), value.to_json());
                evidence.push(quoted);
            }
            Ok(None) => {}
            Err(AgentError::Invalid(error)) => {
                return Ok(Extraction { data: None, evidence, problem: Some(format!("{name}: {error}")) });
            }
            Err(other) => return Err(other),
        }
    }
    // A default the page never showed is the caller's placeholder, not data; only None may stand for absent.
    let unsupported: Vec<&String> = schema
        .fields
        .iter()
        .filter(|(name, field)| !values.contains_key(*name) && !field.required && !field.default.is_null())
        .map(|(name, _)| name)
        .collect();
    if !unsupported.is_empty() {
        let names: Vec<&str> = unsupported.iter().map(|n| n.as_str()).collect();
        return Ok(Extraction {
            data: None,
            evidence,
            problem: Some(format!("no value on the page for {}", names.join(", "))),
        });
    }
    match schema.model.validate(&Value::Object(values)) {
        Ok(data) => Ok(Extraction { data: Some(data), evidence, problem: None }),
        Err(error) => Ok(Extraction { data: None, evidence, problem: Some(error.to_string()) }),
    }
}

#[cfg(test)]
mod tests {
    //! What the golden replay cannot show on its own: the helpers' exact text, and that a failure beside a check never
    //! leaves a request unsent.
    use super::*;
    use crate::support::testing::block_on;
    use fastbrowse_core::models::{Limits, Operation, StepOutcome};

    fn entry(
        operation: Option<Operation>,
        target: Option<&str>,
        typed: Option<&str>,
        effect: Option<&str>,
    ) -> HistoryEntry {
        HistoryEntry {
            operation,
            target: target.map(str::to_string),
            outcome: StepOutcome::Executed,
            page_changed: false,
            note: None,
            text: typed.map(str::to_string),
            effect: effect.map(str::to_string),
            read_progress: false,
            setting: None,
        }
    }

    #[test]
    fn a_step_reads_as_python_writes_it() {
        let filled = entry(Some(Operation::Fill), Some("Name"), Some("O'Neil"), Some("value set"));
        assert_eq!(step(&filled), "- fill Name = \"O'Neil\" -> executed (value set)");
        assert_eq!(step(&entry(None, None, None, None)), "- open -> executed");
    }

    /// `_Jev` in tests/test_done_check.py: answers each question it is asked from a table of probabilities.
    struct TableJev(Vec<(&'static str, f64)>);

    impl DecisionClient for TableJev {
        fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
            fastbrowse_core::decision::DecisionProfile::jev()
        }

        fn evaluate<'a>(
            &'a self,
            _: &'a Value,
            questions: &'a IndexMap<String, Question>,
        ) -> fastbrowse_core::BoxFuture<
            'a,
            std::result::Result<fastbrowse_core::decision::Evaluation, fastbrowse_core::decision::DecisionError>,
        > {
            let answers: Map<String, Value> = self
                .0
                .iter()
                .filter(|(key, _)| questions.contains_key(*key))
                .map(|(key, p)| (key.to_string(), json!({"type": "noul", "probability": p})))
                .collect();
            let evaluation = json!({"model": "test", "answers": answers, "input_tokens": 1,
                "cost": {"component": "jev", "basis": "estimated", "dollars": 0.0}});
            Box::pin(async move { Ok(serde_json::from_value(evaluation).expect("evaluation")) })
        }
    }

    fn count_plan() -> Plan {
        serde_json::from_value(json!({
            "requirements": [
                {"id": "r1", "text": "Navigate through every page of the site.", "kind": "action"},
                {"id": "r2", "text": "Count the quotes tagged love.", "kind": "information"},
            ],
            "answer_expected": true,
        }))
        .expect("plan")
    }

    fn counted_notes() -> Notes {
        let total = "14 quotes are tagged love";
        let evidence: Evidence = serde_json::from_value(json!({
            "source_id": "s0", "url": "https://example.test", "frame_id": null,
            "captured_at": "2026-01-01T00:00:00Z", "capture_sha256": "love", "start": 0, "end": total.len(),
            "quote": total,
        }))
        .expect("evidence");
        Notes::new([crate::memory::Fact::read(Some("r2"), total, evidence, fastbrowse_core::models::FactReader::Llm)])
    }

    fn page() -> Observation {
        serde_json::from_value(json!({
            "url": "https://example.test/encode/httpx", "title": "encode/httpx", "page_key": "k",
            "captured_at": "2026-01-01T00:00:00Z", "controls": [], "omitted_controls": 0,
            "viewport_text": "encode/httpx", "tabs": [],
        }))
        .expect("observation")
    }

    fn done(answers: Vec<(&'static str, f64)>, notes: &Notes) -> DoneCheck {
        let jev = TableJev(answers);
        let options = DoneOptions::default();
        block_on(check_done(&jev, "Count them.", &count_plan(), &page(), notes, &Thresholds::default(), None, &options))
            .expect("check")
    }

    #[test]
    fn an_unconfirmed_browsing_step_is_asked_like_any_other_action() {
        // A requirement the planner wrote is asked of Jev whatever its wording, and an unmet one is never skipped.
        let check = done(vec![("unmet_r1", 0.9)], &counted_notes());
        assert_eq!(check.verdict, DoneVerdict::Reject);
        assert_eq!(check.unmet, ["r1"]);
    }

    #[test]
    fn an_unevidenced_answer_still_keeps_the_run_working() {
        assert_eq!(done(vec![("unmet_r1", 0.1)], &Notes::default()).verdict, DoneVerdict::Reject);
    }

    #[test]
    fn a_check_that_never_reaches_jev_spends_no_jev_call() {
        let ledger = Ledger::new(Limits { max_jev_calls: Some(0), ..Limits::default() });
        let options = DoneOptions { ledger: Some(&ledger), ..DoneOptions::default() };
        let jev = TableJev(vec![("unmet_r1", 0.1)]);
        let notes = Notes::default();
        let check = block_on(check_done(
            &jev,
            "Count them.",
            &count_plan(),
            &page(),
            &notes,
            &Thresholds::default(),
            None,
            &options,
        ));
        assert_eq!(check.expect("check").verdict, DoneVerdict::Reject);
        let reached = block_on(check_done(
            &jev,
            "Count them.",
            &count_plan(),
            &page(),
            &counted_notes(),
            &Thresholds::default(),
            None,
            &options,
        ));
        assert!(matches!(reached, Err(AgentError::Budget(_))), "{reached:?}");
    }

    fn small_budget() -> TokenBudget {
        TokenBudget { state_plus_largest_question: 2000, state_plus_all_questions: 4000, ..TokenBudget::default() }
    }

    #[test]
    fn a_long_run_loses_its_oldest_actions_before_the_page_text() {
        let steps: Vec<String> = (0..200).map(|i| format!("- click Result {i} -> executed")).collect();
        let context = Map::from_iter([("actions".to_string(), Value::from(steps.join("\n")))]);
        let (tokens, questions) = (small_budget(), ["q".to_string()]);
        let state = done_state(&page(), &Notes::default(), &tokens, &questions, &context, &steps).expect("state");
        let actions = state["actions"].as_str().unwrap();
        assert!(actions.contains("earlier actions omitted to fit the request]\n"), "{actions}");
        assert!(actions.ends_with("- click Result 199 -> executed"), "{actions}");
        assert_eq!(state["page"]["text"], page().viewport_text);
        assert!(text::len(&dumps(&state)) as i64 <= tokens.input_chars(&questions, true));
    }

    #[test]
    fn a_state_that_cannot_fit_is_refused_rather_than_sent() {
        let context = Map::from_iter([("task".to_string(), Value::from("word ".repeat(2000)))]);
        let error = page_state(&page(), &Notes::default(), &small_budget(), &["q".to_string()], Some(&context));
        assert!(matches!(error, Err(AgentError::ObservationTooLarge(_))), "{error:?}");
    }
}
