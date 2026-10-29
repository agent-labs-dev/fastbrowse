//! Recovery: when Jev is unsure or the run is going round, Jev is asked again with what went wrong, and only when it
//! stays unsure does the LLM diagnose why and name one next action.

use std::collections::HashSet;

use fastbrowse_core::effects::state_key;
use fastbrowse_core::llm::Message;
use fastbrowse_core::models::{
    Bytes, Decider, LLMPurpose, Operation, Status, StepOutcome, StepResult, UNTRUSTED, background_section,
};
use fastbrowse_core::navigation::navigation_urls;
use fastbrowse_core::page::Observation;
use fastbrowse_core::py::text;
use fastbrowse_core::safety::{origin_of, starts_download};
use fastbrowse_core::schema::{Field, ModelTy, Schema, Ty};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::helpers::{
    controls_text, decision_label, describe, field_corrections, partition_proof, recovery_memory, recovery_text,
};
use super::state::{FieldText, RunState, Step, Stop};
use super::{Agent, dumps_strings};
use crate::planner::RequirementKind;
use crate::policy::{Decision, decide};
use crate::support::generate;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recovery {
    diagnosis: String,
    #[serde(default)]
    url: Option<String>,
    next_subgoal: String,
    #[serde(default)]
    control: Option<i64>,
    #[serde(default)]
    operation: Option<Operation>,
    #[serde(deserialize_with = "fastbrowse_core::py::lax::boolean")]
    give_up: bool,
    #[serde(default)]
    text: Option<String>,
    #[serde(default, deserialize_with = "fastbrowse_core::py::lax::boolean")]
    needs_input: bool,
    #[serde(default, deserialize_with = "fastbrowse_core::py::lax::boolean")]
    needs_login: bool,
}

impl Schema for Recovery {
    fn ty() -> Ty {
        let fields = vec![
            Field::required("diagnosis", Ty::Str),
            Field::default("url", Ty::optional(Ty::Str), Value::Null)
                .doc("For navigate only: one exact caller-supplied address listed."),
            Field::required("next_subgoal", Ty::Str).doc("The single next thing to achieve on the page, concretely."),
            Field::default("control", Ty::optional(Ty::Int), Value::Null)
                .doc("The index of the one listed control the subgoal acts on, or null if none."),
            Field::default("operation", Ty::optional(Operation::ty()), Value::Null)
                .doc("Operation on the control or page; scroll goes down and scroll_up goes up."),
            Field::required("give_up", Ty::Bool).doc("True only when the task cannot progress without the user: false while an observed control or listed address still offers a way on, including an alternative the task allows."),
            Field::default("text", Ty::optional(Ty::Str), Value::Null).doc(
                "For a fill correction only: the exact non-secret value supplied by the task or notes for the chosen observed field. Null for other operations, secrets, or an uncertain value.",
            ),
            Field::default("needs_input", Ty::Bool, false).doc(
                "With give_up: true when what the user must supply is a value the task never gave, such as a field it names no value for; false for any other dead end.",
            ),
            Field::default("needs_login", Ty::Bool, false).doc(
                "With give_up: true when a sign-in page or prompt is what blocks the task and no stored secret covers it; false for any other dead end.",
            ),
        ];
        ModelTy { module: "fastbrowse.agent", ..ModelTy::frozen("_Recovery", fields) }.ty()
    }
}

const RECOVERY: &str = "# Recovery\nThe browsing agent is not making progress. Diagnose why from the screenshot and history, then give one concrete next subgoal: a single operation, naming the index of the observed control it acts on, with no alternatives. A read, scroll, back or escape acts on the page and names no control. scroll moves down; scroll_up moves up. navigate opens one listed caller-supplied address: copy it into url and name no control. Use it when another requested site has no observed link. Never invent an address. A read takes in the whole page, so scroll only to reach a control, load more, or position content the task explicitly asks to see in the viewport. Unverified answer outputs remain open even when a requirement has other cited fields. Gather their missing quotes instead of finishing from the same partial notes. When the notes answer every open requirement and no unverified outputs remain, the next subgoal is to finish. Recovery memory records earlier diagnoses and subgoals; use the recent steps to judge whether to try another way. For a covered control, dismiss the observed overlay before trying the underlying control again. Do not alternate the same blocked click with scrolling. For a reversible filter, tab or sort selection, an unchanged selection and result set do not prove the click took effect. If that selection is still required and its control is visible, retry the selection rather than scrolling to look for results that never changed. This does not permit repeating a transaction or submission. For a fill correction, return its exact non-secret task value in text, bound to the chosen control. Use field semantics rather than a validation message as its identity. A successful submission may have changed the page before it was read. Read and check the outcome before submitting again; do not create another transaction to repair an uncertain report. Manage an existing transaction through its observed controls or links. HTTP failures describe this browser session, not what the site permits in general. An opening shortcut is a guessed address. If its page leaves requirements open and its controls change nothing, use back toward the caller's start page and choose from the controls there instead of repeating actions on the guessed page. Dates are relative to the supplied current date.\n\n# Trust\n";

/// Whether `text` occurs in `source` as a whole value: not inside a longer word, address or number.
fn stated_in(text: &str, source: &str) -> bool {
    let word = |c: char| fastbrowse_core::py::text::is_word(c);
    let before_ok = |c: char| !(word(c) || matches!(c, '@' | '.' | '+' | '-'));
    let mut from = 0;
    while let Some(found) = source[from..].find(text) {
        let start = from + found;
        let end = start + text.len();
        let before = source[..start].chars().next_back();
        let rest = &source[end..];
        let mut after = rest.chars();
        let next = after.next();
        let after_ok = match next {
            None => true,
            Some(c) if word(c) || matches!(c, '@' | '+' | '-') => false,
            Some('.') => !after.next().is_some_and(word),
            Some(_) => true,
        };
        if before.is_none_or(before_ok) && after_ok {
            return true;
        }
        from = start + source[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// The action a stall is about, which the next ask leaves out: its operation, and the control it acted on by label.
pub(super) type Withheld = Option<(Operation, Option<String>)>;

/// A read that found nothing new, withheld from the ask that follows it.
pub(super) const READ: Withheld = Some((Operation::Read, None));
/// A refused DONE, withheld from the ask that follows it.
pub(super) const DONE: Withheld = Some((Operation::Done, None));

/// `decision`'s action, withheld from the ask that follows it.
pub(super) fn withheld(decision: &Decision) -> Withheld {
    Some((decision.operation, decision_label(decision)))
}

impl Agent {
    pub(super) async fn recover(
        &self,
        state: &mut RunState,
        observation: &Observation,
        reason: &str,
        gives_up_as: Status,
        withheld: Withheld,
    ) -> Step {
        self.ask_recovery(state, observation, reason, gives_up_as, withheld, true).await
    }

    /// Recovery asked for the next step on a page the run is not stuck on: Jev was unsure, not stalled. Being stuck
    /// is one rule, the page unchanged for `stall.unchanged_actions` steps, so a consult neither spends the recovery
    /// budget nor clears that rule's evidence. On The Pi Hut Jev was unsure on most steps of a cart walk, and the
    /// consults on pages revisited between items spent the budget and ended five of eleven runs stuck mid-cart.
    pub(super) async fn consult(&self, state: &mut RunState, observation: &Observation, reason: &str) -> Step {
        self.ask_recovery(state, observation, reason, Status::Stuck, None, false).await
    }

    async fn ask_recovery(
        &self,
        state: &mut RunState,
        observation: &Observation,
        reason: &str,
        gives_up_as: Status,
        withheld: Withheld,
        stalled: bool,
    ) -> Step {
        state.form_values.clear();
        let reason = self.redact(reason);
        let remaining = state.ready_plan.as_ref().map(|plan| state.notes.unresolved(plan)).unwrap_or_default();
        let evidenced: HashSet<String> = match &state.ready_plan {
            Some(plan) => plan
                .requirements
                .iter()
                .filter(|r| r.kind == RequirementKind::Information && !remaining.iter().any(|left| left.id == r.id))
                .map(|r| r.id.clone())
                .collect(),
            None => HashSet::new(),
        };
        // Reading another target advances a comparison even when its next route is a previously visited list.
        // Reopening the same requirement after a failed answer check cannot restore recovery repeatedly.
        let mut verified_outputs: HashSet<String> = HashSet::new();
        if state.oversized_answer {
            for plan in &state.finish_partitions {
                let ids: Vec<&str> = plan.requirements.iter().map(|r| r.id.as_str()).collect();
                let proof = partition_proof(plan, &state.notes.for_requirements(ids, None), &state.invented);
                if state.partition_answers.contains_key(&proof) {
                    verified_outputs.extend(plan.answer_checks.iter().cloned());
                } else if let Some(partial) = state.partial_answers.get(&proof) {
                    verified_outputs.extend(partial.output_claims.iter().flat_map(|outputs| outputs.keys().cloned()));
                }
            }
        }
        let current = if state.oversized_answer { state.notes.current_evidence(None) } else { Default::default() };
        let sources: HashSet<String> = current.values().map(|source| source.url.clone()).collect();
        let mut collections: HashSet<(String, Option<String>)> = HashSet::new();
        if state.oversized_answer {
            for fact in state.notes.facts() {
                if fact.is_collection() && fact.comparison.as_ref().is_some_and(|c| c.complete) {
                    for key in &fact.basis {
                        if let Some(source) = current.get(key) {
                            collections.insert((source.url.clone(), fact.scope.clone()));
                        }
                    }
                }
            }
        }
        if (evidenced.iter().any(|id| !state.recovered_requirements.contains(id))
            && state.open_answer_outputs.is_empty())
            || verified_outputs.iter().any(|o| !state.recovered_outputs.contains(o))
            || sources.iter().any(|s| !state.recovered_sources.contains(s))
            || collections.iter().any(|c| !state.recovered_collections.contains(c))
        {
            state.recoveries = 0;
            state.recovery_log.clear();
        }
        state.recovered_requirements.extend(evidenced);
        state.recovered_outputs.extend(verified_outputs);
        state.recovered_sources.extend(sources);
        state.recovered_collections.extend(collections);
        let open_requirements = remaining
            .iter()
            .map(|r| {
                let suffix = if state.continuing.contains(&r.id) {
                    " (the reader saw the list this ranges over go on past what the page shows: load the rest first)"
                } else {
                    ""
                };
                format!("- {}{suffix}", r.text)
            })
            .collect::<Vec<_>>()
            .join("\n");
        if stalled {
            state.recoveries += 1;
            // Recovery spends every tripwire's evidence so the same threshold crossing cannot trigger it again.
            state.unchanged = 0;
        }
        state.recovered_at = state.history.len();
        // Recovery that could not see a release's address clicked its download link to look for it, in a task that
        // said not to download. Asked before the controls are listed, a link the task rules out is never offered.
        if observation.controls.iter().any(starts_download) {
            self.downloads_forbidden(state).await?;
        }
        if stalled && state.recoveries > self.config.stall.max_recoveries {
            if let Some(failure) = &state.http_failure
                && gives_up_as != Status::NeedsInput
            {
                return Err(failure.stop().into());
            }
            let detail = if open_requirements.is_empty() {
                reason
            } else {
                format!("{reason}\nStill to find:\n{open_requirements}")
            };
            return Err(Stop::new(self.walled(state, observation, gives_up_as, false), detail).into());
        }
        if self.ask_jev_again(state, observation, &reason, withheld).await? {
            return Ok(());
        }
        let caller_destinations = navigation_urls(
            &state.task,
            &observation.url,
            state.caller_start.as_deref(),
            state.start_landing_url.as_deref(),
            true,
        );
        let steps = state
            .history
            .iter()
            .map(|h| {
                let operation = h.operation.map_or("open", |o| o.as_str());
                let effect = h.effect.as_ref().map(|e| format!(": {e}")).unwrap_or_default();
                format!("- {operation} {} -> {}{effect}", h.target.as_deref().unwrap_or_default(), h.outcome)
            })
            .collect::<Vec<_>>()
            .join("\n");
        // Without the names, a sign-in page reads as a wall the user must pass: a run with a stored login gave up saying
        // no credentials were given.
        let stored = self.secret_names(&origin_of(&observation.url));
        let secrets = if stored.is_empty() {
            String::new()
        } else {
            format!("\n\n## Stored secrets\n{}. Filling a field with one types its hidden value.", stored.join(", "))
        };
        let system = format!("{RECOVERY}{UNTRUSTED}");
        let notes = state.notes.render_for_navigation(false);
        let started = self.redact(state.started_url.as_deref().or(state.first_url.as_deref()).unwrap_or_default());
        let status = observation.response_status.map_or("None".to_string(), |s| s.to_string());
        let mut content = format!(
            "## Controls\n{}\n\n## Page\n{}\nHTTP status: {status}\n{}{secrets}\n\n## Caller start page\n{started}\n\n## Caller-supplied addresses\n{}\n\n",
            controls_text(observation),
            observation.url,
            observation.viewport_text,
            dumps_strings(&caller_destinations),
        );
        let proposed = if state.invented.contains(&observation.url) { "True" } else { "False" };
        content += &format!(
            "## Current address was proposed\n{proposed}\n\n## HTTP failure\n{}\n\n## Notes read so far\n{}\n\n## Recent steps\n{steps}\n\n## Recovery memory\n{}\n\n## Current date\n{}\n\n## Still to find\n{}\n\n## Unverified answer outputs\n{}\n\n",
            state.http_failure.as_ref().map_or("none".to_string(), |f| f.message()),
            if notes.is_empty() { "none" } else { &notes },
            recovery_memory(state, self.config.stall.max_recoveries, |value| self.redact(value)),
            observation.today(),
            if open_requirements.is_empty() { "nothing" } else { &open_requirements },
            if state.open_answer_outputs.is_empty() {
                "none".to_string()
            } else {
                state.open_answer_outputs.join("; ")
            },
        );
        content += &field_corrections(state);
        let background = background_section(state.context.as_deref(), "##");
        content += &format!("## Task\n{}{background}\n\n## Problem\n{reason}", state.task);
        let images: Vec<Bytes> = self.screenshots().await?.into_iter().map(Bytes).collect();
        let messages = vec![Message::system(system), Message { images, ..Message::user(content) }];
        let generation =
            // Uncapped on purpose: fastbrowse never caps, truncates or crops an output token. What bounds this call
            // is the reasoning it may spend, and that is off for this purpose on the branch above, and the schema it
            // must answer in, not a ceiling that could cut a reply mid-JSON.
            generate::<Recovery>(&*self.llm, LLMPurpose::Recover, &messages, 0, Some(&state.ledger)).await?;
        state.ledger.record([generation.cost.clone()])?;
        let data = generation.data;
        if data.give_up {
            // Asked of the model rather than carried from the step that raised it: a missing value can surface
            // recoveries later, after an attempt to go on without it has failed for its absence.
            let status = if data.needs_input {
                Status::NeedsInput
            } else if gives_up_as == Status::Unverified {
                Status::Unverified
            } else {
                self.walled(state, observation, Status::Stuck, data.needs_login)
            };
            if let Some(failure) = &state.http_failure
                && status != Status::NeedsInput
            {
                return Err(failure.stop().into());
            }
            return Err(Stop::new(status, data.diagnosis).into());
        }
        let diagnosis = self.redact(&data.diagnosis);
        let hint = self.redact(&data.next_subgoal);
        state.hint = Some(hint.clone());
        state.log_recovery(format!(
            "- Reason: {}. Diagnosis: {}. Subgoal: {}.",
            recovery_text(&reason),
            recovery_text(&diagnosis),
            recovery_text(&hint)
        ));
        self.event("recover", || {
            Map::from_iter([
                ("reason".into(), reason.clone().into()),
                ("hint".into(), hint.clone().into()),
                ("operation".into(), json!(data.operation)),
                ("control".into(), json!(data.control)),
            ])
        });
        state.directed = None;
        match (data.operation, data.control) {
            (Some(Operation::Navigate), _)
                if data.url.as_ref().is_some_and(|url| caller_destinations.contains(url)) =>
            {
                state.directed = Some((Operation::Navigate, data.url.clone()));
            }
            (Some(operation), _) if super::helpers::page_operation(operation) => {
                state.directed = Some((operation, None));
            }
            (Some(operation), Some(chosen)) if chosen >= 0 && (chosen as usize) < observation.controls.len() => {
                let named = &observation.controls[chosen as usize];
                state.directed = Some((operation, Some(named.id.clone())));
                if operation == Operation::Fill
                    && named.operations.contains(&Operation::Fill)
                    && !named.sensitive
                    && let Some(text) = data.text.as_ref().filter(|t| !fastbrowse_core::py::text::strip(t).is_empty())
                    && !self.reveals_text(text)
                    && std::iter::once(state.task.as_str())
                        .chain(state.inputs.values().map(String::as_str))
                        .chain(state.notes.facts().iter().map(|fact| fact.text.as_str()))
                        .any(|source| stated_in(text, source))
                {
                    state.form_values.insert(
                        (observation.document_key.clone(), named.id.clone()),
                        (named.clone(), FieldText { missing: false, text: text.clone(), values: Vec::new() }),
                    );
                }
                let commit = (operation, Some(named.id.clone()), describe(named), observation.url.clone());
                if state.unsure_commit.as_ref() == Some(&commit) {
                    state.confirmed_commit = state.unsure_commit.clone();
                }
            }
            _ => {}
        }
        state.unsure_commit = None;
        let note = self.redact(&[reason.as_str(), data.diagnosis.as_str(), data.next_subgoal.as_str()].join("\n"));
        let step = StepResult {
            index: state.steps.len() as i64,
            operation: Operation::Escalate,
            decided_by: Decider::Llm,
            outcome: StepOutcome::Executed,
            url: observation.url.clone(),
            target: None,
            confidence: None,
            note: Some(note),
            facts: Vec::new(),
            page_changed: None,
            duration_ms: 0,
        };
        self.record_step(state, step, true, None).await
    }

    /// Jev asked again on this page, told what went wrong, with the action that changed nothing left out of its
    /// options and the caller's start page offered beside back, scroll and escape. Whether it was sure enough of a pick
    /// to take it as the next decision, which spares the LLM recovery.
    async fn ask_jev_again(
        &self,
        state: &mut RunState,
        observation: &Observation,
        reason: &str,
        withheld: Withheld,
    ) -> Step<bool> {
        let secrets = self.secret_names(&origin_of(&observation.url));
        let mut context = self.context(state, secrets, false, false)?;
        let mut offered = observation.clone();
        let mut problem = reason.to_string();
        if let Some((operation, label)) = withheld {
            let mut left_out = false;
            match &label {
                Some(label) => {
                    for control in offered.controls.iter_mut().filter(|control| describe(control) == *label) {
                        left_out |= control.operations.remove(&operation);
                    }
                    offered.controls.retain(|control| !control.operations.is_empty());
                }
                None => {
                    context.withheld.push(operation);
                    left_out = true;
                }
            }
            if left_out {
                let what =
                    label.map_or_else(|| operation.to_string(), |label| format!("{operation} {}", text::repr(&label)));
                problem +=
                    &format!("\n{what} did not move the task on from this page, so it is not offered this time.");
            }
        }
        context.problem = Some(problem);
        context.screenshot = self.viewport().await?;
        let decision = decide(
            &*self.jev,
            &offered,
            &context,
            &self.config,
            Some(&state.ledger),
            Some(&state.relevance_cache),
            &self.trace,
        )
        .await?;
        if decision.operation == Operation::Escalate || decision.confidence() < self.config.thresholds.recover_below {
            return Ok(false);
        }
        let label = decision_label(&decision).map(|label| self.redact(&label));
        state.log_recovery(format!(
            "- Reason: {}. Jev chose: {} {}.",
            recovery_text(reason),
            decision.operation,
            label.as_deref().unwrap_or_default()
        ));
        self.event("recover", || {
            Map::from_iter([
                ("reader".into(), "jev".into()),
                ("reason".into(), reason.into()),
                ("operation".into(), json!(decision.operation)),
                ("target".into(), json!(label)),
            ])
        });
        state.directed = None;
        state.picked = Some((state_key(observation), decision));
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::stated_in;

    #[test]
    fn a_value_is_stated_only_as_a_whole() {
        assert!(stated_in("Bath", "Find a train from Bath to York"));
        assert!(stated_in("Bath", "Leave Bath."));
        assert!(!stated_in("Bath", "Bathurst"));
        assert!(!stated_in("bob", "mail bob@example.com"));
        assert!(!stated_in("example", "visit example.com"));
    }
}
