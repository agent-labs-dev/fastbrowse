//! Recovery: when Jev is unsure or the run is going round, the LLM diagnoses why and names one next action.

use std::collections::HashSet;

use fastbrowse_core::llm::Message;
use fastbrowse_core::models::{
    Bytes, Decider, LLMPurpose, Operation, Status, StepOutcome, StepResult, UNTRUSTED, background_section,
};
use fastbrowse_core::navigation::{navigation_urls, same_address};
use fastbrowse_core::page::Observation;
use fastbrowse_core::safety::{origin_of, starts_download};
use fastbrowse_core::schema::{Field, ModelTy, Schema, Ty};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::helpers::{
    controls_text, describe, field_corrections, history, lookup, partition_proof, recovery_memory, recovery_routes,
    recovery_text, unread, unread_output_section, without_failed_links,
};
use super::state::{FieldText, RunState, Step, Stop};
use super::{Agent, dumps_strings};
use crate::planner::RequirementKind;
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

const RECOVERY: &str = "# Recovery\nThe browsing agent is not making progress. Diagnose why from the screenshot and history, then give one concrete next subgoal: a single operation, naming the index of the observed control it acts on, with no alternatives. A read, scroll, back or escape acts on the page and names no control. scroll moves down; scroll_up moves up. navigate opens one listed caller-supplied address: copy it into url and name no control. Use it when another requested site has no observed link. Never invent an address. A read takes in the whole page, so scroll only to reach a control, load more, or position content the task explicitly asks to see in the viewport. Unverified answer outputs remain open even when a requirement has other cited fields. Gather their missing quotes instead of finishing from the same partial notes. When the notes answer every open requirement and no unverified outputs remain, the next subgoal is to finish. Recovery memory records earlier diagnoses and subgoals; use the recent steps to judge whether to try another way. For a covered control, dismiss the observed overlay before trying the underlying control again. Do not alternate the same blocked click with scrolling. For a reversible filter, tab or sort selection, an unchanged selection and result set do not prove the click took effect. If that selection is still required and its control is visible, retry the selection rather than scrolling to look for results that never changed. This does not permit repeating a transaction or submission. For a fill correction, return its exact non-secret task value in text, bound to the chosen control. Use field semantics rather than a validation message as its identity. A successful submission may have changed the page before it was read. Read and check the outcome before submitting again; do not create another transaction to repair an uncertain report. Manage an existing transaction through its observed controls or links. HTTP failures describe this browser session, not what the site permits in general. Failed links are withheld on unchanged source pages; use another observed route or stop. An opening shortcut is a guessed address. If its page leaves requirements open and its controls change nothing, use back toward the caller's start page and choose from the controls there instead of repeating actions on the guessed page. Dates are relative to the supplied current date.\n\n# Trust\n";

const SECTIONS: &str = "\n\nUse the unverified field and target to choose its unread source section. Observed links list exact destinations, source titles and whether they were read. navigate may also open one of those observed destinations. Prefer the relevant unread section to rereading or scrolling a section already captured. When the missing field is a link destination, read the section containing actual links. Follow the target's observed section link even when another field is already evidenced.";

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

impl Agent {
    pub(super) async fn recover(
        &self,
        state: &mut RunState,
        observation: &Observation,
        reason: &str,
        gives_up_as: Status,
    ) -> Step {
        state.form_values.clear();
        let reason = self.redact(reason);
        if observation.dialog.is_none()
            && let Some(section) = unread_output_section(state)
        {
            state.recovery_sections.insert(section.clone());
            state.directed = Some((Operation::Navigate, Some(section)));
            state.section_read = true;
            state.hint = Some(format!(
                "Read the observed section for the unverified output: {}",
                state.open_answer_outputs.join("; ")
            ));
            state.unchanged = 0;
            state.recovered_at = state.history.len();
            return Ok(());
        }
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
        state.recoveries += 1;
        // Recovery spends every tripwire's evidence so the same threshold crossing cannot trigger it again.
        state.unchanged = 0;
        state.recovered_at = state.history.len();
        // Recovery that could not see a release's address clicked its download link to look for it, in a task that
        // said not to download. Asked before the controls are listed, a link the task rules out is never offered.
        if observation.controls.iter().any(starts_download) {
            self.downloads_forbidden(state).await?;
        }
        if state.recoveries > self.config.stall.max_recoveries {
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
        if let Some(start) = state.caller_start.clone()
            && observation.dialog.is_none()
            && !remaining.is_empty()
            && state.invented.contains(&observation.url)
            && !same_address(&observation.url, &start)
            && !state.left_guesses.contains(&observation.url)
            && state.read_here
            // A list being paged, or a task with something to do on the page, can still be on the right page.
            && state.continuing.is_empty()
            && !state.through_end
            && !state
                .ready_plan
                .as_ref()
                .is_some_and(|plan| plan.requirements.iter().any(|r| r.kind == RequirementKind::Action))
            // Context a read kept from the page, a biography beside the quotes it lacked, answers nothing.
            && !state.notes.facts().iter().any(|fact| {
                fact.requirement_id.is_some() && fact.evidence.as_ref().is_some_and(|e| e.url == observation.url)
            })
        {
            // A guessed opening page that was read and gave nothing: the way on is the caller's start page, which is
            // what the recovery model chose there every time, after 3 to 4 seconds and a screenshot.
            state.left_guesses.insert(observation.url.clone());
            state.directed = Some((Operation::Navigate, Some(start)));
            state.section_read = true;
            state.hint = Some(
                "The guessed opening page held nothing the task needs; continue from the caller's start page.".into(),
            );
            self.event("recover", || {
                Map::from_iter([
                    ("reader".into(), "code".into()),
                    ("reason".into(), "guessed_page_gave_nothing".into()),
                    ("reason_detail".into(), reason.clone().into()),
                ])
            });
            return Ok(());
        }
        let caller_destinations = navigation_urls(
            &state.task,
            &observation.url,
            state.caller_start.as_deref(),
            state.start_landing_url.as_deref(),
            true,
        );
        let routes = recovery_routes(state, self.config.observation.working_notes_chars);
        let mut destinations = caller_destinations.clone();
        if !state.open_answer_outputs.is_empty() {
            for link in state.observed_links.keys() {
                if !destinations.contains(link) {
                    destinations.push(link.clone());
                }
            }
        }
        let steps = history(&state.history, &self.config.observation)
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
        let mut system = format!("{RECOVERY}{UNTRUSTED}");
        if !state.open_answer_outputs.is_empty() {
            system += SECTIONS;
        }
        let notes = state.notes.render_for_navigation(self.config.observation.working_notes_chars, false)?;
        let started = self.redact(state.started_url.as_deref().or(state.first_url.as_deref()).unwrap_or_default());
        let status = observation.response_status.map_or("None".to_string(), |s| s.to_string());
        let mut content = format!(
            "## Controls\n{}\n\n## Page\n{}\nHTTP status: {status}\n{}{secrets}\n\n## Caller start page\n{started}\n\n## Caller-supplied addresses\n{}\n\n",
            controls_text(&without_failed_links(state, observation)),
            observation.url,
            observation.viewport_text,
            dumps_strings(&caller_destinations),
        );
        if !state.open_answer_outputs.is_empty() {
            content += &format!("## Observed section links\n{routes}\n\n");
        }
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
            generate::<Recovery>(&*self.llm, LLMPurpose::Recover, &messages, 2000, Some(&state.ledger)).await?;
        state.ledger.record([generation.cost.clone()])?;
        let mut data = generation.data;
        if data.give_up
            && !data.needs_input
            && state.http_failure.is_none()
            && gives_up_as == Status::Stuck
            && !state.terminal_verification_attempted
            && state
                .ready_plan
                .as_ref()
                .is_some_and(|plan| !plan.requirements.is_empty() && !unread(plan, &state.notes))
            && state.open_answer_outputs.is_empty()
            && state
                .history
                .iter()
                .any(|e| e.operation.is_some() && e.outcome == StepOutcome::Executed && e.page_changed)
        {
            // Recovery can find no further action after a goal is reached. The completion verifier, rather than that
            // diagnosis, decides whether the changed page actually satisfies the requested outcomes.
            state.terminal_verification_attempted = true;
            data.give_up = false;
            data.operation = Some(Operation::Done);
            data.control = None;
            data.next_subgoal = "Verify the requested outcomes against the current page.".into();
        }
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
        // An unread page is still read before a finish is judged, so only a page already read is exhausted.
        let exhausted = data.operation == Some(Operation::Done) && state.ready_plan.is_some() && state.read_here;
        if exhausted
            && unread(state.plan(), &state.notes)
            && !(state.oversized_answer && lookup(state.plan()) && !state.notes.current_evidence(None).is_empty())
        {
            // The done check refuses any finish with information nobody has read, so this subgoal cannot succeed.
            if let Some(failure) = &state.http_failure
                && gives_up_as != Status::NeedsInput
            {
                return Err(failure.stop().into());
            }
            // Once per step: a later step of the same single-page app can repeat the reader's mistake, but the same
            // step asked twice answers the same way.
            let page_read =
                state.read_step.clone().unwrap_or_else(|| format!("{}\n{}", observation.url, observation.document_key));
            if state.unread_rereads.contains(&page_read) || gives_up_as != Status::Stuck {
                return Err(Stop::new(gives_up_as, diagnosis).into());
            }
            // A reader filed a date picker's selected date as context rather than as the requested value, and three
            // runs stopped stuck on the first recovery with the date on screen. The reader is asked once more; the
            // done check and the claim check still judge whatever it binds.
            state.unread_rereads.insert(page_read);
            state.reads.retain(|key| key.0 != observation.document_key);
            state.barren.retain(|key, _| key.0 != observation.document_key);
            data.operation = Some(Operation::Read);
            data.control = None;
            data.next_subgoal = "Read the page again for the requirements still open.".into();
        }
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
        let usable = without_failed_links(state, observation);
        state.directed = None;
        match (data.operation, data.control) {
            (Some(Operation::Navigate), _) if data.url.as_ref().is_some_and(|url| destinations.contains(url)) => {
                state.directed = Some((Operation::Navigate, data.url.clone()));
            }
            (Some(operation), _) if super::helpers::page_operation(operation) => {
                state.directed = Some((operation, None));
            }
            (Some(operation), Some(chosen)) if chosen >= 0 && (chosen as usize) < usable.controls.len() => {
                let named = &usable.controls[chosen as usize];
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
                        (usable.document_key.clone(), named.id.clone()),
                        (named.clone(), FieldText { missing: false, text: text.clone(), values: Vec::new() }),
                    );
                }
                let commit = (operation, Some(named.id.clone()), describe(named), usable.url.clone());
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
            url: usable.url.clone(),
            target: None,
            confidence: None,
            note: Some(note),
            facts: Vec::new(),
            page_changed: None,
            duration_ms: 0,
        };
        self.record_step(state, step, true, None).await
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
