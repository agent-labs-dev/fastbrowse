//! Reading pages into the notes: one read, and a run of next pages read side by side while code turns them.

use std::sync::Arc;
use std::time::Instant;

use fastbrowse_core::decision::{DecisionClient, NoulQuestion, Question};
use fastbrowse_core::effects::state_key;
use fastbrowse_core::llm::LLMClient;
use fastbrowse_core::models::{CostLine, Decider, Operation, StepOutcome, StepResult, UNTRUSTED};
use fastbrowse_core::page::{Capture, Control, Observation};
use fastbrowse_core::py::text;
use fastbrowse_core::safety::origin_of;
use fastbrowse_core::telemetry::{Ledger, Trace};
use serde_json::{Map, json};

use super::Agent;
use super::driving::StepOptions;
use super::helpers::{answer_evidence, code_decision, describe, lookup, next_page_control, read_question};
use super::state::{Flow, RunState, Spawned, Step};
use crate::comparison::complete_comparison;
use crate::error::AgentError;
use crate::memory::{Notes, fact_id};
use crate::planner::{Requirement, RequirementKind};
use crate::policy::HistoryEntry;
use crate::retrieval::{ReadOptions, ReadOutcome, TallyReader, filter_tallies, read, read_tallies};

/// A page of a list captured and being read while the browser moves on.
struct PageRead {
    capture: Capture,
    observation: Observation,
    task: Spawned<ReadOutcome>,
    started: Instant,
    frame: Option<Vec<u8>>,
}

fn sorted(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut values: Vec<String> = values.into_iter().collect();
    values.sort();
    values
}

/// Read `capture`'s records for `wanted` alone, as a page in a run of pages is read: from a counted field when the
/// reader set one up, else by Jev's record filter, else by the reader, retried once on a saved capture that lost a
/// record.
///
/// On the `last` page, the one with no next-page control, a list code counted ends there: the readers exist only for a
/// list whose every page is needed.
#[allow(clippy::too_many_arguments)]
async fn page_records(
    jev: Arc<dyn DecisionClient>,
    llm: Arc<dyn LLMClient>,
    ledger: Arc<Ledger>,
    trace: Trace,
    capture: Capture,
    wanted: Vec<Requirement>,
    question: String,
    tokens: fastbrowse_core::config::TokenBudget,
    tally_readers: Vec<TallyReader>,
    last: bool,
) -> Result<ReadOutcome, AgentError> {
    let ids: Vec<String> = wanted.iter().map(|r| r.id.clone()).collect();
    let mut counted = None;
    if !tally_readers.is_empty() {
        counted = read_tallies(&capture, &tally_readers, &ids)?;
    }
    if counted.is_none() {
        counted = filter_tallies(&*jev, &*llm, &capture, &tally_readers, &ids, &tokens, Some(&ledger), &trace).await?;
    }
    if let Some(counted) = counted {
        return Ok(if last { ReadOutcome { ended: ids, ..counted } } else { counted });
    }
    let mut costs: Vec<CostLine> = Vec::new();
    let mut retried = false;
    loop {
        let options = ReadOptions {
            tokens: tokens.clone(),
            ledger: Some(&ledger),
            requirements: wanted.clone(),
            records_only: true,
            ..ReadOptions::default()
        };
        let mut notes = Notes::default();
        let outcome = read(&*llm, &capture, &question, &ids, &mut notes, &options).await?;
        costs.extend(outcome.cost_lines.iter().cloned());
        if outcome.incomplete.is_empty() || retried {
            return Ok(ReadOutcome { cost_lines: costs, ..outcome });
        }
        // A bad record range used to poison the whole tally; later pages cannot repair its missing quotes. Retry the
        // saved capture before merging anything, while the browser continues paging.
        retried = true;
    }
}

/// Jev's yes or no on whether a page shows a value one of the task's actions must carry to another page. Jev can settle
/// every requirement a page is read for and still leave such a value unquoted: a sign-in read on the Phone page found
/// nothing for the orders it was asked about, so the code it showed never reached the notes, and the run went back for
/// it three times. On 16 labelled pages (codes, a discount, a reference, an address and a key, against sign-in forms,
/// listings and unrelated pages) every page holding such a value scored 0.92 or more and every other 0.21 or less.
fn context_question(task: &str, actions: &[&str]) -> Question {
    let listed: Vec<String> = actions.iter().map(|action| format!("- {action}")).collect();
    NoulQuestion::new(
        format!(
            "{UNTRUSTED}\n\n# Task\n{task}\n\n# Actions still to take\n{}\n\nDoes this page show a value one of these actions needs, such as a code, reference, address or key, that the agent must carry to another page?",
            listed.join("\n")
        ),
        Some("Yes: the page shows a value an action needs."),
        Some("No: no value on this page is needed by these actions."),
    )
    .into()
}

impl Agent {
    /// Return (added evidence, skipped duplicate), since only new evidence makes a read progress.
    pub(super) async fn read(
        &self,
        state: &mut RunState,
        capture: Option<Capture>,
        observation: Option<&Observation>,
        require_all_evidence: bool,
    ) -> Step<(bool, bool)> {
        let capture = match capture {
            Some(capture) => capture,
            None => self.capture().await?,
        };
        let plan = state.await_plan().await?;
        if lookup(&plan) {
            // Reader-selected excerpts cannot decide whether the full sources fit together in one prompt.
            state.read_source_sizes.insert(capture.url.clone(), text::len(&capture.text) as i64);
            let total: i64 = state.read_source_sizes.values().sum();
            state.oversized_answer |= total > self.config.tokens.llm_chars();
        }
        let information = |r: &&Requirement| r.kind == RequirementKind::Information;
        let mut wanted: Vec<Requirement> =
            state.notes.unresolved(&plan).into_iter().filter(|r| information(&r)).collect();
        let transaction_pending = self.unread_receipt(state).await?;
        let changed_since_read =
            observation.is_some_and(|o| state.read_documents.get(&o.document_key).copied().unwrap_or(false));
        let owed = transaction_pending || (wanted.is_empty() && changed_since_read && plan.page_answer_expected());
        if owed || (wanted.is_empty() && !state.open_answer_outputs.is_empty()) {
            // Every requirement was evidenced off the page before an interaction changed it, so they are asked again
            // of what it shows now.
            wanted = plan.requirements.iter().filter(information).cloned().collect();
        }
        if !state.open_answer_outputs.is_empty()
            && state.oversized_answer
            && !state.finish_partitions.is_empty()
            && !transaction_pending
        {
            let mut regrouped = Vec::new();
            for group in &state.finish_partitions {
                let outputs: Vec<&String> = state
                    .open_answer_outputs
                    .iter()
                    .filter(|check| {
                        group.answer_checks.contains(check) || group.requirements.iter().any(|r| &&r.text == check)
                    })
                    .collect();
                if outputs.is_empty() {
                    continue;
                }
                let joined = outputs.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("; ");
                for requirement in group.requirements.iter().filter(information) {
                    regrouped.push(Requirement { text: joined.clone(), ..requirement.clone() });
                }
            }
            wanted = regrouped;
        }
        let wanted_ids: Vec<String> = wanted.iter().map(|r| r.id.clone()).collect();
        if let Some(observation) = observation {
            // This exact content is either read now or was read before, so the notes hold what the page shows.
            state.read_documents.insert(observation.document_key.clone(), false);
            let content = (
                observation.document_key.clone(),
                capture.read_key(),
                wanted_ids.clone(),
                state.open_answer_outputs.clone(),
            );
            if state.reads.contains(&content) && !transaction_pending {
                self.event("read_skipped", || {
                    Map::from_iter([("reason".into(), "unchanged_content_and_requirements".into())])
                });
                return Ok((false, true));
            }
            state.reads.insert(content);
        }
        if text::strip(&capture.text).is_empty() {
            // An empty page may still be rendering; only new content warrants another read.
            if let Some(observation) = observation
                && self.outwait(observation).await?
            {
                let drawn = self.capture().await?;
                if !text::strip(&drawn.text).is_empty() {
                    return Box::pin(Box::pin(self.read(state, Some(drawn), Some(observation), false))).await;
                }
            }
            // Nothing on the page can evidence anything, so the reader is not asked. A blank outcome was inspected too:
            // it proves nothing, but must not prevent recovery from navigating.
            for candidate in &mut state.transaction_candidates {
                candidate.outcome_read = true;
            }
            return Ok((false, false));
        }
        let following = observation.and_then(next_page_control);
        let began = if following.is_some() || state.pages > 0 { state.first_url.clone() } else { None };
        let began = began.map(|url| self.redact(&url));
        let mut question = read_question(&state.task, &wanted, began.as_deref());
        if !state.open_answer_outputs.is_empty() {
            question +=
                &format!("\nStill missing or unsupported answer outputs: {}", state.open_answer_outputs.join("; "));
        }
        let actions: Vec<&str> =
            plan.requirements.iter().filter(|r| r.kind == RequirementKind::Action).map(|r| r.text.as_str()).collect();
        if !actions.is_empty() {
            question += "\nAlso keep quotes containing values needed for the task's remaining actions, even when they do not answer an information requirement. Give these context claims a null requirement_id.";
        }
        let before = state.notes.facts().len();
        let known = answer_evidence(&state.notes, false);
        let evidenced: Vec<String> =
            wanted.iter().filter(|r| state.notes.evidenced(&r.id)).map(|r| r.id.clone()).collect();
        let options = ReadOptions {
            tokens: self.config.tokens.clone(),
            ledger: Some(&state.ledger),
            // What Jev cannot quote, and the context the remaining actions need, still go to the LLM reader.
            jev: Some(&*self.jev),
            context_question: (!actions.is_empty()).then(|| context_question(&state.task, &actions)),
            requirements: wanted.clone(),
            targets: Some(plan.requirements.iter().filter(information).cloned().collect()),
            // Rebinding old quotes during output recovery does not make revisiting their page a fresh source.
            revalidate: owed
                && (transaction_pending || state.open_answer_outputs.is_empty() || !state.oversized_answer),
            recovering_outputs: state.oversized_answer && !state.open_answer_outputs.is_empty(),
            preserve_collections: state.oversized_answer
                || !state.finish_partitions.is_empty()
                || !state.open_answer_outputs.is_empty(),
            field_outputs: state.oversized_answer && !state.finish_partitions.is_empty(),
            continuing: sorted(state.continuing.iter().cloned()),
            incomplete: state.incomplete.iter().cloned().collect(),
            require_all_evidence: require_all_evidence || (state.through_end && state.pages > 0),
            trace: self.trace.clone(),
            ..ReadOptions::default()
        };
        let ledger = state.ledger.clone();
        let options = ReadOptions { ledger: Some(&ledger), ..options };
        let outcome = read(&*self.llm, &capture, &question, &wanted_ids, &mut state.notes, &options).await?;
        let mut context_added = false;
        if state.oversized_answer && !state.open_answer_outputs.is_empty() {
            // A field read can return context while another field or collection is still open. Its quotes must reach
            // that target's check even when the reader cannot bind the whole requirement.
            let added: Vec<String> = outcome.facts.iter().map(fact_id).collect();
            context_added =
                state.notes.retain_for(wanted.iter().map(|r| r.id.as_str()), added.iter().map(String::as_str));
            if context_added {
                state.rejected_answer_evidence = None;
            }
        }
        if lookup(&plan) {
            let retained = match state.pending_collections.get(&capture.url) {
                Some((sha, facts)) if *sha == capture.sha256 => facts.clone(),
                _ => Vec::new(),
            };
            let mut merged: indexmap::IndexMap<String, crate::memory::Fact> = indexmap::IndexMap::new();
            for fact in retained.into_iter().chain(outcome.collection_facts.iter().cloned()) {
                merged.insert(fact_id(&fact), fact);
            }
            state
                .pending_collections
                .insert(capture.url.clone(), (capture.sha256.clone(), merged.into_values().collect()));
            if state.oversized_answer {
                // Earlier reads keep page-proven coverage outside ordinary notes until the sources need separate
                // prompts. Entering recovery must not require the same collection to be read again.
                let pending = std::mem::take(&mut state.pending_collections);
                for (_, facts) in pending.into_values() {
                    for fact in facts {
                        state.notes.add(fact);
                    }
                }
            }
        }
        for candidate in &mut state.transaction_candidates {
            candidate.outcome_read = true;
        }
        for requirement in &wanted {
            state.notes.supersede(&requirement.id, &capture.url, &capture.sha256, &capture.text);
        }
        state.incomplete.extend(outcome.incomplete.iter().cloned());
        state.tally_readers = outcome.tally_readers.clone();
        state.comparisons = outcome.comparisons.clone();
        // A records-only recovery can prove that a finite list ended. Apply its code-owned result here, where a normal
        // read can finish without the paged pipeline consuming an ended marker.
        for requirement_id in &outcome.ended {
            if state.incomplete.contains(requirement_id) {
                continue;
            }
            if let Some(comparison) = state.comparisons.get(requirement_id).cloned() {
                complete_comparison(&mut state.notes, requirement_id, &comparison);
            }
            if !state.notes.has_untallied_records(requirement_id) {
                state.notes.complete_tallies(requirement_id);
            }
        }
        // A timer changes the capture hash and a reader can paraphrase the same claim, so only a new source quote or a
        // newly evidenced requirement restores the read budget.
        let progressed = context_added
            || answer_evidence(&state.notes, false).iter().any(|key| !known.contains(key))
            || (state.open_answer_outputs.is_empty()
                && wanted.iter().any(|r| !evidenced.contains(&r.id) && state.notes.evidenced(&r.id)));
        let continues: Vec<String> =
            outcome.continues.iter().filter(|key| !state.notes.evidenced(key)).cloned().collect();
        self.event("read", || {
            Map::from_iter([
                ("url".into(), self.redact(&capture.url).into()),
                ("chars".into(), text::len(&capture.text).into()),
                ("wanted".into(), json!(wanted_ids)),
                ("facts_added".into(), (state.notes.facts().len() - before).into()),
                ("rejected_claims".into(), outcome.rejected_claims.into()),
                ("continues".into(), json!(continues)),
                ("incomplete".into(), json!(state.incomplete)),
                ("uncovered".into(), outcome.uncovered.into()),
            ])
        });
        if progressed {
            state.evidence_reads += 1;
        }
        state.through_end = !continues.is_empty() && wanted.iter().all(|r| outcome.through_end.contains(&r.id));
        if let Some(observation) = observation {
            self.follow_pages(state, &continues, following.as_ref(), observation, outcome.expands.as_deref());
        }
        Ok((progressed, false))
    }

    /// Start reading a page of a list, its readings judged as any other read is.
    fn start_page(
        &self,
        state: &mut RunState,
        capture: &Capture,
        wanted: &[Requirement],
        last: bool,
    ) -> Spawned<ReadOutcome> {
        let question = read_question(&state.task, wanted, state.first_url.as_deref());
        Spawned::spawn(page_records(
            self.jev.clone(),
            self.llm.clone(),
            state.ledger.clone(),
            self.trace.clone(),
            capture.clone(),
            wanted.to_vec(),
            question,
            self.config.tokens.clone(),
            state.tally_readers.clone(),
            last,
        ))
    }

    async fn join_page(
        &self,
        state: &mut RunState,
        page: &PageRead,
        wanted: &[Requirement],
        last: bool,
    ) -> Step<Option<ReadOutcome>> {
        let outcome = match page.task.wait().await.unwrap_or_else(|| Err(super::state::cancelled())) {
            Ok(outcome) => outcome,
            Err(error) if Flow::from(error.clone()).is_llm() => {
                // The browser has moved on, so retry the saved capture, never the page now on screen.
                state.paging_failed = true;
                let retry = self.start_page(state, &page.capture, wanted, last);
                match retry.wait().await.unwrap_or_else(|| Err(super::state::cancelled())) {
                    Ok(outcome) => outcome,
                    Err(error) if Flow::from(error.clone()).is_llm() => {
                        state.incomplete.extend(wanted.iter().map(|r| r.id.clone()));
                        let reason = self.redact(&format!("Could not read saved page {}: {error}", page.capture.url));
                        state.hint = Some(reason.clone());
                        state.history.push(HistoryEntry {
                            effect: Some(reason.clone()),
                            ..HistoryEntry::new(Some(Operation::Read), None, StepOutcome::Failed, false)
                        });
                        let step = StepResult {
                            index: state.steps.len() as i64,
                            operation: Operation::Read,
                            decided_by: Decider::Code,
                            outcome: StepOutcome::Failed,
                            url: page.capture.url.clone(),
                            target: None,
                            confidence: None,
                            note: Some(reason),
                            facts: Vec::new(),
                            page_changed: None,
                            duration_ms: 0,
                        };
                        self.record_step(state, step, false, page.frame.clone()).await?;
                        return Ok(None);
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        };
        let before = state.notes.facts().len();
        state.notes.remember_capture(&page.capture);
        outcome.merge_records(&mut state.notes)?;
        state.read_documents.insert(page.observation.document_key.clone(), false);
        state.incomplete.extend(outcome.incomplete.iter().cloned());
        let progressed = state.notes.facts().len() > before;
        state.evidence_reads += usize::from(progressed);
        state.history.push(HistoryEntry {
            effect: Some("Read captured pagination records.".into()),
            read_progress: progressed,
            ..HistoryEntry::new(Some(Operation::Read), None, StepOutcome::Executed, false)
        });
        let facts = state.notes.facts()[before..].iter().map(|fact| self.public_fact(fact)).collect();
        let step = StepResult {
            index: state.steps.len() as i64,
            operation: Operation::Read,
            decided_by: Decider::Code,
            outcome: StepOutcome::Executed,
            url: page.capture.url.clone(),
            target: None,
            confidence: None,
            note: None,
            facts,
            page_changed: None,
            duration_ms: page.started.elapsed().as_millis() as i64,
        };
        self.record_step(state, step, false, page.frame.clone()).await?;
        Ok(Some(outcome))
    }

    /// Whether `steps` more steps fit beside the reads still owed a step each.
    fn steps_room(state: &RunState, pending: usize, steps: i64) -> bool {
        let steps_taken = state.ledger.state().steps;
        state.ledger.limits.max_steps.is_none_or(|max| steps_taken + pending as i64 + steps <= max)
    }

    /// Whether `calls` more LLM calls fit.
    fn calls_room(state: &RunState, calls: i64) -> bool {
        let made = state.ledger.state().llm_calls;
        state.ledger.limits.max_llm_calls.is_none_or(|max| made + calls <= max)
    }

    pub(super) async fn pipeline_pages(&self, state: &mut RunState, observation: &Observation) -> Step {
        let plan = state.plan().clone();
        let wanted: Vec<Requirement> =
            state.notes.unresolved(&plan).into_iter().filter(|r| r.kind == RequirementKind::Information).collect();
        let mut pending: Vec<PageRead> = Vec::new();
        let mut last_records: Option<usize> = None;
        let mut ended: Vec<String> = Vec::new();
        let mut capture: Option<Capture> = None;
        let origin = origin_of(&observation.url);
        let mut seen = std::collections::HashSet::from([observation.url.clone()]);
        let mut observation = observation.clone();
        state.next_page = false;
        let mut before_click = state.steps.len();
        let mut target: Option<Control> = None;
        let walked = async {
            loop {
                target = next_page_control(&observation);
                let Some(next) = target.clone() else { break };
                if state.pages >= self.config.max_pages {
                    break;
                }
                state.ledger.check(0.0)?;
                // Each queued read still owes a step. Leave room for both the next click and its read.
                if !Self::steps_room(state, pending.len(), 2) {
                    break;
                }
                if state.ledger.limits.max_llm_calls.is_some_and(|max| state.ledger.state().llm_calls >= max) {
                    break;
                }
                if pending.iter().any(|page| page.task.result().is_some_and(|r| r.is_err())) {
                    state.paging_failed = true;
                    break;
                }
                state.pages += 1;
                state.paged_from = Some(state_key(&observation));
                before_click = state.steps.len();
                let options = StepOptions { gate: false, follow_pager: true, ..StepOptions::default() };
                self.step(state, &observation, &code_decision(Operation::Click, Some(next)), Decider::Code, options)
                    .await?;
                if state.steps.len() == before_click
                    || state
                        .steps
                        .last()
                        .is_some_and(|s| s.outcome != StepOutcome::Executed || s.page_changed != Some(true))
                {
                    break;
                }
                // Navigation already settled this page. Its controls and source blocks are independent reads, and
                // waiting for one before starting the other costs a cloud round trip on every page.
                let (landed, captured) = tokio::join!(self.observe(), self.capture());
                let landed = landed?;
                self.note_effect(state, &landed);
                if landed.response_status.is_some_and(|status| status >= 400) {
                    state.paging_failed = true;
                    break;
                }
                self.settle(state, &landed).await?;
                state.paged_from = None;
                if seen.contains(&landed.url) || origin_of(&landed.url) != origin {
                    break;
                }
                seen.insert(landed.url.clone());
                observation = landed;
                if observation.dialog.is_some() {
                    state.paging_failed = true;
                    break;
                }
                let page = captured?;
                if text::strip(&page.text).is_empty() || page.url != observation.url {
                    state.paging_failed = true;
                    break;
                }
                let mut following = next_page_control(&observation);
                if following.is_none() {
                    // The capture outwaits a loader the concurrent observation may have seen, and a loader can hide the
                    // pager: only controls observed after that wait can end the pages.
                    let settled = self.observe().await?;
                    if settled.url == observation.url {
                        following = next_page_control(&settled);
                        observation = settled;
                    }
                }
                let mut computable: std::collections::HashSet<String> =
                    state.notes.tallies().into_iter().map(|t| t.requirement_id).collect();
                computable.extend(state.comparisons.keys().cloned());
                // A count Jev matches every page of is code's even before a page holds a match.
                computable.extend(state.tally_readers.iter().map(|reader| reader.requirement_id.clone()));
                let fallback_room = state.comparisons.is_empty()
                    || (Self::steps_room(state, pending.len(), 2) && Self::calls_room(state, 2));
                let frames = self.on_event.is_some() && self.config.step_frames;
                if following.is_none() && wanted.iter().all(|r| computable.contains(&r.id)) && fallback_room {
                    // Rankings and counts merge in code, so the last read need not wait for earlier pages.
                    let frame = if frames { self.frame().await? } else { None };
                    let task = self.start_page(state, &page, &wanted, true);
                    pending.push(PageRead {
                        capture: page,
                        observation: observation.clone(),
                        task,
                        started: Instant::now(),
                        frame,
                    });
                    last_records = Some(pending.len() - 1);
                    break;
                }
                let llm_room = Self::calls_room(state, 2);
                if following.is_none()
                    || state.pages >= self.config.max_pages
                    || !Self::steps_room(state, pending.len(), 3)
                    || !llm_room
                {
                    capture = Some(page);
                    break;
                }
                let frame = if frames { self.frame().await? } else { None };
                let task = self.start_page(state, &page, &wanted, false);
                pending.push(PageRead {
                    capture: page,
                    observation: observation.clone(),
                    task,
                    started: Instant::now(),
                    frame,
                });
                // Let the client reserve its call before the next navigation checks the remaining budget.
                tokio::task::yield_now().await;
            }
            Ok::<(), Flow>(())
        };
        let walked = walked.await;
        let result = async {
            match walked {
                Err(Flow::Browser(error)) if !error.needs_browser() => {
                    state.paging_failed = true;
                    state.next_page = false;
                    let hint = self
                        .redact(&format!("Pagination stopped: {error}. Inspect the current page before continuing."));
                    state.hint = Some(hint.clone());
                    if state.steps.len() == before_click {
                        let label = target.as_ref().map(describe);
                        state.history.push(HistoryEntry {
                            effect: Some(hint.clone()),
                            ..HistoryEntry::new(Some(Operation::Click), label.as_deref(), StepOutcome::Failed, false)
                        });
                        self.record_failure(
                            state,
                            &observation,
                            Operation::Click,
                            &hint,
                            label,
                            None,
                            None,
                            Decider::Code,
                        )
                        .await?;
                    }
                }
                other => other?,
            }
            for (index, page) in pending.iter().enumerate() {
                let outcome = self.join_page(state, page, &wanted, Some(index) == last_records).await?;
                if Some(index) == last_records
                    && let Some(outcome) = outcome
                {
                    ended = outcome.ended;
                }
            }
            Ok::<(), Flow>(())
        }
        .await;
        for page in &pending {
            page.task.cancel();
        }
        for page in &pending {
            page.task.wait().await;
        }
        result?;
        if let Some(index) = last_records
            && !state.paging_failed
        {
            let records = &pending[index];
            for requirement_id in &ended {
                if !state.incomplete.contains(requirement_id)
                    && let Some(comparison) = state.comparisons.get(requirement_id).cloned()
                {
                    complete_comparison(&mut state.notes, requirement_id, &comparison);
                }
                if !state.incomplete.contains(requirement_id) && !state.notes.has_untallied_records(requirement_id) {
                    state.notes.complete_tallies(requirement_id);
                }
            }
            if wanted.iter().all(|r| state.notes.evidenced(&r.id)) {
                state.continuing.clear();
                state.through_end = false;
            } else {
                capture = Some(records.capture.clone());
            }
        }
        // No intermediate read can close the comparison. Only this read sees every earlier page's records.
        match capture {
            Some(capture) if !state.paging_failed => {
                state.ledger.check(0.0)?;
                let read_key = capture.read_key();
                let options =
                    StepOptions { capture: Some(capture), require_all_evidence: true, ..StepOptions::default() };
                match Box::pin(self.step(
                    state,
                    &observation,
                    &code_decision(Operation::Read, None),
                    Decider::Code,
                    options,
                ))
                .await
                {
                    Err(flow) if flow.is_llm() => {
                        let Flow::Agent(error) = &flow else { unreachable!("an LLM error is an agent error") };
                        state.paging_failed = true;
                        state.reads.remove(&(
                            observation.document_key.clone(),
                            read_key,
                            wanted.iter().map(|r| r.id.clone()).collect(),
                            state.open_answer_outputs.clone(),
                        ));
                        let hint = self.redact(&format!("Pagination read failed: {error}. Read this page again."));
                        state.hint = Some(hint.clone());
                        state.history.push(HistoryEntry {
                            effect: Some(hint.clone()),
                            ..HistoryEntry::new(Some(Operation::Read), None, StepOutcome::Failed, false)
                        });
                        self.record_failure(
                            state,
                            &observation,
                            Operation::Read,
                            &hint,
                            None,
                            None,
                            None,
                            Decider::Code,
                        )
                        .await?;
                    }
                    other => {
                        other?;
                    }
                }
            }
            _ => {
                state.paged_from = None;
                state.next_page = false;
            }
        }
        Ok(())
    }

    /// Arrange for the rest of a list the reader says it needs: the next page by code, the control the reader named,
    /// or a word to Jev.
    fn follow_pages(
        &self,
        state: &mut RunState,
        continues: &[String],
        following: Option<&Control>,
        observation: &Observation,
        expands: Option<&str>,
    ) {
        state.continuing = continues.iter().cloned().collect();
        if continues.is_empty() {
            return;
        }
        if following.is_some() && state.pages < self.config.max_pages {
            // Deciding each hop costs a Jev call to pick a link code has already found, on every page of the list.
            state.next_page = true;
            return;
        }
        // "Otherwise finish" was a dead end: the reader withholds a list it has not seen the end of, so the done check
        // refused every such finish.
        let mut show = "open the control that loads or pages through more of the list".to_string();
        // The reader saw which control shows the rest. Matched against the page's own labels rather than trusted, so a
        // label the reader invented directs nothing.
        if let Some(expands) = expands {
            let named: Vec<&Control> = observation
                .controls
                .iter()
                .filter(|c| c.label == expands && c.operations.contains(&Operation::Click))
                .collect();
            if named.len() == 1 {
                state.directed = Some((Operation::Click, Some(named[0].id.clone())));
                show = format!("open \"{expands}\"");
            }
        }
        state.hint = Some(format!(
            "The reader saw the list this task needs go on past what the page shows, so what was read cannot settle it. Show the rest and READ it: {show}, or narrow the list with the page's own filter or sort so the answer is in view."
        ));
    }
}
