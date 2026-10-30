//! Whether a run is finished, and whether its answer and extracted data hold up.
//!
//! Jev judges completion in one call: one question per requirement, against the page, the run's actions and the cited
//! notes, and whether the drafted answer needs rewriting.

mod prompts;

use std::collections::HashSet;

use fastbrowse_core::config::{Thresholds, TokenBudget};
use fastbrowse_core::decision::{Answer, DecisionClient, NoulQuestion, Question};
use fastbrowse_core::llm::LLMClient;
use fastbrowse_core::models::{CostComponent, CostLine, Evidence};
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

/// The id the done check reports the task under when the plan holds no requirement to ask about.
pub const WHOLE_TASK: &str = "task";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DoneCheck {
    pub verdict: DoneVerdict,
    /// Requirement ids Jev judged not satisfied.
    pub unmet: Vec<String>,
    /// Requirement ids Jev gave no answer for: not judged, so not satisfied.
    pub unanswered: Vec<String>,
    /// The offered draft when Jev judged it already answers the task, so no composer needs to run.
    pub answer: Option<ComposedAnswer>,
    pub costs: Vec<CostLine>,
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
    let budget = tokens.input_chars(questions);
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
    /// Whether an action changed the page after its notes were last read: what the notes quote may no longer show.
    pub changed_since_read: bool,
    /// The current page was read and has not changed since, so it is the evidence the notes quote.
    pub page_read: bool,
    /// The site failure standing where the run is, stated so an answer drawn from it is not taken as the task's.
    pub site_failure: Option<String>,
    /// Requirements whose list the reader saw go on past what the run read.
    pub continuing: HashSet<String>,
    /// Reserves Jev's calls.
    pub ledger: Option<&'a Ledger>,
    pub trace: Trace,
}

/// Judge completion, and whether `draft` answers the task as written, in the one Jev call.
///
/// Every requirement is a question to Jev, information requirements included, so a run never ends complete on code's
/// say-so. Code's facts are stated in the question: an information requirement no note cites, and a page that changed
/// after its notes were read.
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
    for requirement in &plan.requirements {
        let (question, unmet, met) = match requirement.kind {
            RequirementKind::Action => (
                prompts::unmet_action(&requirement.text),
                "It is not satisfied, or neither the page nor the action record shows it.",
                "The page or the action record shows it is satisfied.",
            ),
            RequirementKind::Information => (
                prompts::unmet_information(
                    &requirement.text,
                    notes.evidenced(&requirement.id),
                    options.continuing.contains(&requirement.id),
                ),
                "It is not answered, or no cited note supports the answer.",
                "A cited note answers it.",
            ),
        };
        questions
            .insert(format!("unmet_{}", requirement.id), NoulQuestion::new(question, Some(unmet), Some(met)).into());
    }
    if plan.requirements.is_empty() {
        // A plan of run reports alone has no requirement to ask about, and the task itself is still Jev's to judge.
        questions.insert(
            format!("unmet_{WHOLE_TASK}"),
            NoulQuestion::new(
                prompts::unmet_task(),
                Some("It is not done, or there is no visible confirmation."),
                Some("The page and the actions show it is done."),
            )
            .into(),
        );
    }
    let mut draft = draft;
    if draft.is_some() {
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
    if let Some(status) = observation.response_status {
        context.insert("http_status".into(), status.into());
    }
    if options.changed_since_read {
        context.insert("page_changed_since_read".into(), true.into());
    }
    if let Some(failure) = &options.site_failure {
        context.insert("site_failure".into(), failure.clone().into());
    }
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
    // The answer is written from the notes, so a page never read cannot answer an information requirement: shown one, Jev
    // passed a comparison on a price only that page held. Action requirements are judged on the page either way.
    let on_page = |key: &String| {
        options.page_read
            || plan.requirements.is_empty()
            || plan.requirements.iter().any(|r| r.kind == RequirementKind::Action && format!("unmet_{}", r.id) == *key)
    };
    let (page_questions, notes_questions): (IndexMap<_, _>, IndexMap<_, _>) =
        questions.into_iter().partition(|(key, _)| on_page(key));
    let mut notes_state = state.clone();
    if let Some(fields) = notes_state.as_object_mut() {
        fields.shift_remove("page");
        fields.shift_remove("controls");
    }
    let mut answers = IndexMap::new();
    let mut lines = Vec::new();
    for (state, questions) in [(&state, &page_questions), (&notes_state, &notes_questions)] {
        if questions.is_empty() {
            continue;
        }
        if let Some(ledger) = options.ledger {
            ledger.reserve(CostComponent::Jev, 0.0, 1)?;
        }
        let evaluation = jev.evaluate(state, questions).await?;
        answers.extend(evaluation.answers);
        lines.push(evaluation.cost);
    }
    let mut unmet: Vec<String> = Vec::new();
    let mut unanswered: Vec<String> = Vec::new();
    let mut ids: Vec<&str> = plan.requirements.iter().map(|r| r.id.as_str()).collect();
    if ids.is_empty() {
        ids.push(WHOLE_TASK);
    }
    for id in ids {
        let id = id.to_string();
        match answers.get(&format!("unmet_{id}")) {
            // Jev's answer is the more probable one: a requirement it more likely than not judges unmet is unmet.
            Some(Answer::Noul(answer)) if answer.probability < 0.5 => {}
            Some(Answer::Noul(_)) => unmet.push(id),
            _ => unanswered.push(id),
        }
    }
    let verdict = if unmet.is_empty() && unanswered.is_empty() { DoneVerdict::Accept } else { DoneVerdict::Reject };
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
    Ok(DoneCheck { verdict, unmet, unanswered, answer: if ready { draft.cloned() } else { None }, costs: lines })
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
    /// Answers from a table, keeping the instructions of every question asked.
    struct TableJev(Vec<(&'static str, f64)>, std::sync::Mutex<IndexMap<String, String>>, std::sync::Mutex<Vec<Value>>);

    impl TableJev {
        fn new(answers: Vec<(&'static str, f64)>) -> TableJev {
            TableJev(answers, Default::default(), Default::default())
        }
    }

    impl DecisionClient for TableJev {
        fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
            fastbrowse_core::decision::DecisionProfile::jev()
        }

        fn evaluate<'a>(
            &'a self,
            state: &'a Value,
            questions: &'a IndexMap<String, Question>,
        ) -> fastbrowse_core::BoxFuture<
            'a,
            std::result::Result<fastbrowse_core::decision::Evaluation, fastbrowse_core::decision::DecisionError>,
        > {
            self.2.lock().unwrap().push(state.clone());
            let mut asked = self.1.lock().unwrap();
            for (key, question) in questions {
                if let Question::Noul(noul) = question {
                    asked.insert(key.clone(), noul.instructions.clone());
                }
            }
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
            "captured_at": "2026-01-01T00:00:00Z", "controls": [],
            "viewport_text": "encode/httpx", "tabs": [],
        }))
        .expect("observation")
    }

    fn done(answers: Vec<(&'static str, f64)>, notes: &Notes) -> DoneCheck {
        let jev = TableJev::new(answers);
        let options = DoneOptions::default();
        block_on(check_done(&jev, "Count them.", &count_plan(), &page(), notes, &Thresholds::default(), None, &options))
            .expect("check")
    }

    #[test]
    fn an_unconfirmed_browsing_step_is_asked_like_any_other_action() {
        // A requirement the planner wrote is asked of Jev whatever its wording, and an unmet one is never skipped.
        let check = done(vec![("unmet_r1", 0.9), ("unmet_r2", 0.1)], &counted_notes());
        assert_eq!(check.verdict, DoneVerdict::Reject);
        assert_eq!(check.unmet, ["r1"]);
    }

    #[test]
    fn jev_judges_every_requirement_and_its_yes_finishes() {
        let check = done(vec![("unmet_r1", 0.1), ("unmet_r2", 0.1)], &counted_notes());
        assert_eq!(check.verdict, DoneVerdict::Accept);
    }

    #[test]
    fn a_requirement_jev_more_likely_than_not_judges_unmet_keeps_the_run_working() {
        // A completion check that read 0.69 as met ended a count with one page of three unread.
        let check = done(vec![("unmet_r1", 0.1), ("unmet_r2", 0.69)], &counted_notes());
        assert_eq!(check.verdict, DoneVerdict::Reject);
        assert_eq!(check.unmet, ["r2"]);
    }

    #[test]
    fn a_requirement_jev_did_not_answer_is_not_satisfied() {
        let check = done(vec![("unmet_r1", 0.1)], &counted_notes());
        assert_eq!(check.verdict, DoneVerdict::Reject);
        assert!(check.unmet.is_empty());
        assert_eq!(check.unanswered, ["r2"]);
    }

    #[test]
    fn an_uncited_information_requirement_is_stated_to_jev_as_a_fact() {
        let fact = "No note in state cites evidence for this requirement.";
        let asked = |notes: &Notes| {
            let jev = TableJev::new(Vec::new());
            let options = DoneOptions::default();
            let check = block_on(check_done(
                &jev,
                "Count them.",
                &count_plan(),
                &page(),
                notes,
                &Thresholds::default(),
                None,
                &options,
            ));
            assert_eq!(check.expect("check").unanswered, ["r1", "r2"]);
            jev.1.lock().unwrap()["unmet_r2"].clone()
        };
        assert!(asked(&Notes::default()).contains(fact));
        assert!(!asked(&counted_notes()).contains(fact));
    }

    #[test]
    fn a_plan_of_run_reports_alone_still_asks_jev_about_the_task() {
        let plan: Plan = serde_json::from_value(json!({"requirements": [], "answer_expected": false,
            "run_reports": ["final_url"]}))
        .expect("plan");
        let jev = TableJev::new(vec![("unmet_task", 0.1)]);
        let options = DoneOptions::default();
        let check = block_on(check_done(
            &jev,
            "Report the URL.",
            &plan,
            &page(),
            &Notes::default(),
            &Thresholds::default(),
            None,
            &options,
        ))
        .expect("check");
        assert_eq!(check.verdict, DoneVerdict::Accept);
    }

    #[test]
    fn an_unread_page_is_withheld_from_the_information_check() {
        let jev = TableJev::new(vec![("unmet_r2", 0.9)]);
        let options = DoneOptions { continuing: HashSet::from(["r2".to_string()]), ..DoneOptions::default() };
        let check = block_on(check_done(
            &jev,
            "Count them.",
            &count_plan(),
            &page(),
            &Notes::default(),
            &Thresholds::default(),
            None,
            &options,
        ))
        .expect("check");
        assert_eq!(check.unmet, vec!["r2".to_string()]);
        let states = jev.2.lock().unwrap();
        let from_notes = states.last().expect("a state");
        assert!(from_notes.get("page").is_none() && from_notes.get("controls").is_none());
        assert!(jev.1.lock().unwrap()["unmet_r2"].contains("go on past what has been read"));
    }

    #[test]
    fn a_page_read_unchanged_stays_in_the_information_check() {
        let jev = TableJev::new(vec![]);
        let options = DoneOptions { page_read: true, ..DoneOptions::default() };
        block_on(check_done(
            &jev,
            "Count them.",
            &count_plan(),
            &page(),
            &Notes::default(),
            &Thresholds::default(),
            None,
            &options,
        ))
        .expect("check");
        let states = jev.2.lock().unwrap();
        assert_eq!(states.len(), 1);
        assert!(states[0].get("page").is_some());
    }

    #[test]
    fn every_check_reaches_jev() {
        let ledger = Ledger::new(Limits { max_jev_calls: Some(0), ..Limits::default() });
        let options = DoneOptions { ledger: Some(&ledger), ..DoneOptions::default() };
        let jev = TableJev::new(vec![("unmet_r1", 0.1), ("unmet_r2", 0.1)]);
        let reached = block_on(check_done(
            &jev,
            "Count them.",
            &count_plan(),
            &page(),
            &Notes::default(),
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
        assert!(text::len(&dumps(&state)) as i64 <= tokens.input_chars(&questions));
    }

    #[test]
    fn a_state_that_cannot_fit_is_refused_rather_than_sent() {
        let context = Map::from_iter([("task".to_string(), Value::from("word ".repeat(2000)))]);
        let error = page_state(&page(), &Notes::default(), &small_budget(), &["q".to_string()], Some(&context));
        assert!(matches!(error, Err(AgentError::ObservationTooLarge(_))), "{error:?}");
    }
}
