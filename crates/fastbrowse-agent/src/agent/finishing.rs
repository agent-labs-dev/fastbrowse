//! Whether a run may call itself finished: the done check, the verifier, the answer and its claim checks.

use std::collections::{BTreeSet, HashSet};

use fastbrowse_core::decision::{Answer, Question};
use fastbrowse_core::llm::Generation;
use fastbrowse_core::models::{Decider, Evidence, FactReader, Operation, RunResult, Status, StepOutcome, UntilCheck};
use fastbrowse_core::page::Observation;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::helpers::{
    EvidenceKey, answer_evidence, lookup, partition_proof, path_of, unread, unread_transaction, visited,
};
use super::recovery::DONE;
use super::state::{Flow, HttpFailure, RunState, Step, Stop, TransactionCandidate};
use super::{Agent, Outcome};
use crate::batches::evaluate_batches;
use crate::error::AgentError;
use crate::memory::{Fact, Notes};
use crate::planner::{Plan, RunReport, partition_plan};
use crate::retrieval::{
    AnswerCorrection, Claim, ComposeOptions, ComposedAnswer, assemble_answer, compose, draft_answer, names_subject,
};
use crate::verification::{
    DoneOptions, DoneVerdict, ExtractOptions, Extraction, RecordSchema, WHOLE_TASK, check_done, extract,
};

impl Agent {
    async fn observe_if_changed(&self, observation: &Observation) -> Step<Observation> {
        let raw = self.raw().unwrap_or_else(|| observation.clone());
        if self.page.document_changed(&raw).await? {
            return self.observe().await;
        }
        Ok(observation.clone())
    }

    pub(super) async fn finish(
        &self,
        state: &mut RunState,
        output_schema: Option<&RecordSchema>,
        until: Option<&UntilCheck>,
    ) -> Step<Option<RunResult>> {
        state.await_checks().await?;
        if state.finish_partitions.is_empty() {
            if !state.oversized_answer {
                match Box::pin(self.finish_whole(state, output_schema, until, false)).await {
                    Err(Flow::Agent(AgentError::NotesTooLarge(_))) => state.oversized_answer = true,
                    other => return other,
                }
            }
            // Independent outputs need separate evidence envelopes, not fewer source quotes.
            let plan = state.plan().clone();
            match partition_plan(&*self.llm, &state.task, &plan, &state.ledger).await {
                Ok(partition) => {
                    state.finish_partitions = partition.plans;
                    state.output_targets = partition.targets;
                }
                Err(AgentError::Llm(error)) => return Err(Stop::new(Status::Unverified, error.to_string()).into()),
                Err(other) => return Err(other.into()),
            }
        }
        if state.oversized_answer {
            for group in state.finish_partitions.clone() {
                let subjects: Vec<String> = group
                    .answer_checks
                    .iter()
                    .filter_map(|check| state.output_targets.get(check).map(|target| target.subject.clone()))
                    .collect();
                let sources: Vec<String> = state
                    .notes
                    .current_evidence(None)
                    .into_iter()
                    .filter(|(_, evidence)| {
                        subjects.iter().any(|subject| {
                            !subject.is_empty()
                                && (names_subject(&path_of(&evidence.url), subject)
                                    || state
                                        .notes
                                        .captured_page(evidence)
                                        .is_some_and(|page| names_subject(&page.title, subject)))
                        })
                    })
                    .map(|(key, _)| key)
                    .collect();
                // A description can supply one field without answering a bundled requirement. Its captured target
                // identity keeps that source in the group before a check can reject the missing field.
                let ids: Vec<&str> = group.requirements.iter().map(|r| r.id.as_str()).collect();
                if state.notes.retain_for(ids, sources.iter().map(String::as_str)) {
                    state.rejected_answer_evidence = None;
                }
            }
        }
        Box::pin(self.finish_partitioned(state, output_schema, until)).await
    }

    async fn finish_partitioned(
        &self,
        state: &mut RunState,
        output_schema: Option<&RecordSchema>,
        until: Option<&UntilCheck>,
    ) -> Step<Option<RunResult>> {
        if !state.open_answer_outputs.is_empty()
            && state.rejected_answer_evidence.as_ref() == Some(&answer_evidence(&state.notes, true))
        {
            // A spend guard: the same notes would be checked again and rejected again.
            let fresh = self.observe().await?;
            let reason = format!("DONE rejected, no new output evidence: {}", state.open_answer_outputs.join("; "));
            let reason = self.redact(&reason);
            rejected(state, &reason);
            Box::pin(self.count_progress(state, false, &fresh, DONE, Status::Unverified)).await?;
            return Ok(None);
        }
        let mut missing: Vec<String> = Vec::new();
        let mut corrections: Vec<AnswerCorrection> = Vec::new();
        let mut verified_parts: Vec<ComposedAnswer> = Vec::new();
        let mut oversized: Vec<String> = Vec::new();
        let transactions = self.transaction_evidence_ids(state).await?;
        for plan in state.finish_partitions.clone() {
            let ids: Vec<&str> = plan.requirements.iter().map(|r| r.id.as_str()).collect();
            let notes = state.notes.for_requirements(ids, transactions.iter().map(String::as_str));
            let proof = partition_proof(&plan, &notes, &state.invented);
            if lookup(&plan)
                && let Some(cached) = state.partition_answers.get(&proof)
            {
                verified_parts.push(cached.clone());
                continue;
            }
            if state.oversized_answer
                && lookup(&plan)
                && let Some((rejected, fixes)) = state.partition_failures.get(&proof)
            {
                missing.extend(rejected.iter().cloned());
                corrections.extend(fixes.iter().cloned());
                continue;
            }
            let texts: Vec<&str> = plan.requirements.iter().map(|r| r.text.as_str()).collect();
            let child_task = format!(
                "{}\n\nThis pass checks only these requirements: {}. Other requirements are checked in separate passes. Retain the original task's constraints.",
                state.task,
                texts.join("; ")
            );
            let carried = if lookup(&plan) {
                state
                    .partial_answers
                    .values()
                    .chain(state.partition_answers.values())
                    .rev()
                    .find(|answer| answer.requirements == plan.requirements)
                    .cloned()
            } else {
                None
            };
            let child_corrections: Vec<AnswerCorrection> = state
                .answer_corrections
                .iter()
                .filter(|c| plan.answer_checks.contains(&c.criterion) || texts.contains(&c.criterion.as_str()))
                .cloned()
                .collect();
            // Python finishes a partition on a shallow copy of the run: the fields below are its own, and everything
            // else (the steps, the candidates, the caches) is shared with the run. They are swapped in and back.
            let saved = Child::swap(
                state,
                Child {
                    task: child_task,
                    ready_plan: Some(plan.clone()),
                    notes,
                    finish_partitions: vec![plan.clone()],
                    verified_answer: carried,
                    missing_answer_outputs: Vec::new(),
                    open_answer_outputs: Vec::new(),
                    answer_corrections: child_corrections,
                    rejected_answer_evidence: None,
                },
            );
            let outcome = async {
                if state.oversized_answer && lookup(&plan) && unread(&plan, &state.notes) {
                    // Separate source reads can leave a bundled requirement unbound. Only a fully checked field answer
                    // can bind their combined quotes before the normal completion check.
                    let answer = Box::pin(self.answer(state, None)).await?;
                    let mut basis: Vec<String> = Vec::new();
                    for key in answer.claims.iter().flat_map(|c| c.evidence_ids.iter()) {
                        if !basis.contains(key) {
                            basis.push(key.clone());
                        }
                    }
                    for requirement in &plan.requirements {
                        let fact =
                            Fact::derived(Some(&requirement.id), &answer.answer, basis.clone(), FactReader::Llm)?;
                        state.notes.add(fact);
                    }
                    state.verified_answer = Some(answer);
                }
                Box::pin(self.finish_whole(state, None, None, true)).await
            }
            .await;
            let child = Child::swap(state, saved);
            let result = match outcome {
                Err(Flow::Agent(AgentError::NotesTooLarge(message))) => {
                    oversized.push(message);
                    None
                }
                other => other?,
            };
            let complete = result.as_ref().is_some_and(|r| r.status == Status::Complete);
            if complete && let Some(answer) = &child.verified_answer {
                if lookup(&plan) {
                    state.partition_answers.insert(proof.clone(), answer.clone());
                }
                verified_parts.push(answer.clone());
            } else if !complete {
                if child.missing_answer_outputs.is_empty() {
                    missing.extend(plan.requirements.iter().map(|r| r.text.clone()));
                } else {
                    missing.extend(child.missing_answer_outputs.iter().cloned());
                }
                corrections.extend(child.answer_corrections.iter().cloned());
                if state.oversized_answer && lookup(&plan) && !child.missing_answer_outputs.is_empty() {
                    state
                        .partition_failures
                        .insert(proof, (child.missing_answer_outputs.clone(), child.answer_corrections.clone()));
                }
            }
        }
        let mut unique: Vec<String> = Vec::new();
        for item in missing {
            if !unique.contains(&item) {
                unique.push(item);
            }
        }
        state.missing_answer_outputs = unique.clone();
        state.open_answer_outputs = unique.clone();
        state.answer_corrections = corrections;
        if !unique.is_empty() {
            let reasons: Vec<String> =
                unique.iter().map(|item| format!("Answer output not verified: {item}")).collect();
            let reason = format!("DONE rejected: {}", reasons.join("; "));
            if !oversized.is_empty() {
                let notes = state.notes.clone();
                let error = format!("{reason}; {}", oversized.join("; "));
                let ledger = state.ledger.clone();
                return Ok(Some(self.stopping_result(&notes, state, &ledger, Status::Unverified, Some(error)).await));
            }
            state.rejected_answer_evidence = Some(answer_evidence(&state.notes, true));
            let reason = self.redact(&reason);
            rejected(state, &reason);
            let fresh = self.observe().await?;
            self.record_failure(state, &fresh, Operation::Done, &reason, None, None, None, Decider::Llm).await?;
            Box::pin(self.count_progress(state, false, &fresh, DONE, Status::Unverified)).await?;
            return Ok(None);
        }
        let mut claims: Vec<Claim> = Vec::new();
        for claim in verified_parts.iter().flat_map(|part| part.claims.iter()) {
            if !claims.contains(claim) {
                claims.push(claim.clone());
            }
        }
        let answer = assemble_answer(&claims, &state.notes, &state.plan().requirements, 0);
        let result = match Box::pin(self.conclude(state, output_schema, Some(answer), true)).await {
            Err(Flow::Agent(AgentError::NotesTooLarge(message))) => {
                let notes = state.notes.clone();
                let ledger = state.ledger.clone();
                return Ok(Some(self.stopping_result(&notes, state, &ledger, Status::Unverified, Some(message)).await));
            }
            other => other?,
        };
        if let Some(until) = until {
            let url = match self.raw() {
                Some(raw) => raw.url,
                None => self.observe().await?.url,
            };
            if !until(url).await {
                let fresh = self.observe().await?;
                rejected(state, "DONE rejected: completion condition not satisfied");
                Box::pin(self.count_progress(state, false, &fresh, DONE, Status::Stuck)).await?;
                return Ok(None);
            }
        }
        Ok(Some(result))
    }

    /// The final result when DONE holds up; None sends the loop back to work.
    ///
    /// Search results can arrive or disappear without changing the URL or document. Completion needs a fresh
    /// observation after the browser's bounded loading wait, not the state that prompted the DONE choice.
    async fn finish_whole(
        &self,
        state: &mut RunState,
        output_schema: Option<&RecordSchema>,
        until: Option<&UntilCheck>,
        partition: bool,
    ) -> Step<Option<RunResult>> {
        let mut fresh = self.observe().await?;
        if let Some(failed) = self.failed_submission(state).await? {
            return Err(failed.stop().into());
        }
        state.await_checks().await?;
        state.missing_answer_outputs.clear();
        state.answer_corrections.clear();
        let plan = state.plan().clone();
        let draft = if plan.page_answer_expected() { draft_answer(&plan, &state.notes) } else { None };
        let notes = state.notes.clone();
        let history = state.history.clone();
        let shown = visited(&state.visited, |url| self.redact(url));
        let options = DoneOptions {
            tokens: self.config.tokens.clone(),
            history: &history,
            visited: &shown,
            changed_since_read: state.read_documents.get(&fresh.document_key).copied().unwrap_or(false),
            ledger: Some(&state.ledger),
            trace: self.trace.clone(),
        };
        let task = state.task.clone();
        let check =
            check_done(&*self.jev, &task, &plan, &fresh, &notes, &self.config.thresholds, draft.as_ref(), &options)
                .await?;
        state.ledger.record([check.cost.clone()])?;
        self.event("done_check", || {
            Map::from_iter([("verdict".into(), check.verdict.as_str().into()), ("unmet".into(), json!(check.unmet))])
        });
        if check.verdict == DoneVerdict::Accept {
            let prepared = check.answer.clone();
            let result = Box::pin(self.conclude(state, output_schema, prepared, !partition)).await?;
            fresh = self.observed().unwrap_or(fresh);
            let reached = match until {
                Some(until) => until(self.raw().map_or_else(|| fresh.url.clone(), |raw| raw.url)).await,
                None => true,
            };
            if reached {
                return Ok(Some(result));
            }
        }
        if partition {
            return Ok(Some(self.result(Some(state), &state.ledger.clone(), Status::Unverified, Outcome::default())));
        }
        let mut requirements: IndexMap<&str, &str> =
            plan.requirements.iter().map(|r| (r.id.as_str(), r.text.as_str())).collect();
        requirements.insert(WHOLE_TASK, &state.task);
        let named = |key: &String| format!("{key}: {}", requirements.get(key.as_str()).copied().unwrap_or(key));
        let mut unmet: Vec<String> = check.unmet.iter().map(named).collect();
        unmet.extend(check.unanswered.iter().map(|key| format!("{} (Jev gave no answer)", named(key))));
        let joined = if unmet.is_empty() { "completion not confirmed".to_string() } else { unmet.join("; ") };
        let reason = self.redact(&format!("DONE rejected: {joined}"));
        rejected(state, &reason);
        self.record_failure(state, &fresh, Operation::Done, &reason, None, None, None, Decider::Jev).await?;
        Box::pin(self.count_progress(state, false, &fresh, DONE, Status::Stuck)).await?;
        Ok(None)
    }

    async fn compose_options(&self, state: &mut RunState) -> Step<ComposeOptions<'static>> {
        let transactions = self.transaction_evidence_ids(state).await?;
        let verified = if state.finish_partitions.is_empty() {
            None
        } else {
            state.partial_answers.get(&partition_proof(state.plan(), &state.notes, &state.invented)).cloned()
        };
        Ok(ComposeOptions {
            tokens: self.config.tokens.clone(),
            ledger: None,
            transaction_evidence_ids: transactions,
            corrections: state.answer_corrections.clone(),
            preserve_collections: !state.finish_partitions.is_empty() || !state.answer_corrections.is_empty(),
            field_outputs: !state.finish_partitions.is_empty(),
            verified,
            independent_fields: state.oversized_answer,
            background: state.context.clone(),
            trace: self.trace.clone(),
        })
    }

    async fn composed(&self, state: &mut RunState) -> Step<Generation<ComposedAnswer>> {
        let options = self.compose_options(state).await?;
        let ledger = state.ledger.clone();
        let plan = state.plan().clone();
        let options = ComposeOptions { ledger: Some(&ledger), ..options };
        Ok(compose(&*self.llm, &state.task, &plan, &state.notes, &options).await?)
    }

    /// The draft Jev passed, or a composed answer. A composer that fails leaves the reader's facts, each with its
    /// quote, which answer as they stand.
    pub(super) async fn answer(&self, state: &mut RunState, prepared: Option<ComposedAnswer>) -> Step<ComposedAnswer> {
        if let Some(answer) = prepared {
            return Ok(answer);
        }
        match self.composed(state).await {
            Ok(generation) => Ok(generation.data),
            Err(flow) if flow.is_llm() => draft_answer(state.plan(), &state.notes).ok_or(flow),
            Err(flow) => Err(flow),
        }
    }

    pub(super) async fn transaction_evidence_ids(&self, state: &mut RunState) -> Step<Vec<String>> {
        if !state.authorization.irreversible_actions || state.transaction_candidates.is_empty() {
            return Ok(Vec::new());
        }
        let evidence = state.notes.usable_evidence();
        let urls: HashSet<&String> = evidence.values().map(|item| &item.url).collect();
        let pending: Vec<(String, usize)> = state
            .transaction_candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.committing.is_none()
                    && (urls.contains(&c.from_url) || c.landed_url.as_ref().is_some_and(|u| urls.contains(u)))
            })
            .map(|(index, _)| (format!("irreversible_{index}"), index))
            .collect();
        self.classify(state, &pending).await?;
        // A candidate Jev could not classify counts as committing: a failed call must not drop the receipt check.
        let committed: HashSet<&String> = state
            .transaction_candidates
            .iter()
            .filter(|c| c.committing != Some(false))
            .flat_map(|c| std::iter::once(&c.from_url).chain(c.landed_url.as_ref()))
            .collect();
        Ok(evidence.iter().filter(|(_, item)| committed.contains(&item.url)).map(|(key, _)| key.clone()).collect())
    }

    /// Whether an action Jev classifies as committing still owes a read of what it landed on.
    ///
    /// Each unread candidate is classified before the read it would force, so a step that commits nothing, such as
    /// adding an item to a cart, is not read as a receipt. A candidate Jev gives no answer for still owes its read.
    pub(super) async fn unread_receipt(&self, state: &mut RunState) -> Step<bool> {
        if !unread_transaction(state) {
            return Ok(false);
        }
        let pending: Vec<(String, usize)> = state
            .transaction_candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.outcome_read && c.committing.is_none())
            .map(|(index, _)| (format!("irreversible_{index}"), index))
            .collect();
        self.classify(state, &pending).await?;
        Ok(unread_transaction(state))
    }

    /// Ask Jev which candidates committed something, leaving `committing` unset where it gave no answer.
    async fn classify(&self, state: &mut RunState, pending: &[(String, usize)]) -> Step {
        if pending.is_empty() {
            return Ok(());
        }
        let candidates: &[TransactionCandidate] = &state.transaction_candidates;
        let mut pages: Vec<&String> = Vec::new();
        for (_, index) in pending {
            let candidate = &candidates[*index];
            for url in std::iter::once(&candidate.from_url).chain(candidate.landed_url.as_ref()) {
                if !pages.contains(&url) {
                    pages.push(url);
                }
            }
        }
        let page_state = json!({"pages": pages.iter().map(|url| json!({"url": url})).collect::<Vec<_>>()});
        let questions: IndexMap<String, Question> = pending
            .iter()
            .map(|(key, index)| (key.clone(), Question::Noul(candidates[*index].question.clone())))
            .collect();
        let answered = evaluate_batches(
            &*self.jev,
            &page_state,
            &questions,
            &self.config.tokens,
            Some(&state.ledger),
            None,
            false,
        )
        .await?;
        let Some(answered) = answered else { return Ok(()) };
        for (key, index) in pending {
            if let Some(Answer::Noul(verdict)) = answered.answers.get(key) {
                state.transaction_candidates[*index].committing =
                    Some(verdict.probability > self.config.thresholds.irreversible_above);
            }
        }
        Ok(())
    }

    /// The error the last committing action landed on, unless a later committing action landed cleanly.
    ///
    /// The page a run finishes on is not the page its submission produced: a message form whose server answered 503
    /// with a page still saying "your message has been sent" was reloaded with a GET, which answered 200.
    async fn failed_submission(&self, state: &mut RunState) -> Step<Option<HttpFailure>> {
        let landed: Vec<usize> = state
            .transaction_candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| c.landed_url.is_some())
            .map(|(index, _)| index)
            .collect();
        let Some(failed_at) = landed
            .iter()
            .position(|index| state.transaction_candidates[*index].landed_status.is_some_and(|status| status >= 400))
        else {
            return Ok(None);
        };
        let since: Vec<usize> = landed[failed_at..].to_vec();
        let pending: Vec<(String, usize)> = since
            .iter()
            .enumerate()
            .filter(|(_, index)| state.transaction_candidates[**index].committing.is_none())
            .map(|(position, index)| (format!("landed_{position}"), *index))
            .collect();
        self.classify(state, &pending).await?;
        // As with receipts, a candidate Jev could not classify counts as committing.
        let Some(last) =
            since.iter().rev().map(|index| &state.transaction_candidates[*index]).find(|c| c.committing != Some(false))
        else {
            return Ok(None);
        };
        match (&last.landed_url, last.landed_status) {
            (Some(url), Some(status)) if status >= 400 => Ok(Some(HttpFailure { url: url.clone(), status })),
            _ => Ok(None),
        }
    }

    /// The caller's schema, filled from a capture taken inside this branch so it overlaps the answer.
    async fn extraction(
        &self,
        task: &str,
        notes: &Notes,
        schema: &RecordSchema,
        ledger: &fastbrowse_core::telemetry::Ledger,
    ) -> Step<Extraction> {
        let capture = self.capture().await?;
        let options = ExtractOptions { notes: Some(notes), tokens: self.config.tokens.clone(), ledger: Some(ledger) };
        Ok(extract(&*self.jev, &*self.llm, task, &capture, schema, &options).await?)
    }

    pub(super) async fn conclude(
        &self,
        state: &mut RunState,
        output_schema: Option<&RecordSchema>,
        prepared: Option<ComposedAnswer>,
        finalize: bool,
    ) -> Step<RunResult> {
        let mut evidence: Vec<Evidence> = state.notes.facts().iter().filter_map(|fact| fact.evidence.clone()).collect();
        let answering = state.plan().page_answer_expected();
        // Writing the answer and filling the caller's schema read the same finished notes and neither needs the
        // other's output, so a task that wants both pays for the slower one rather than both.
        let notes = state.notes.clone();
        let task = state.task.clone();
        let ledger = state.ledger.clone();
        let extracting = async {
            match output_schema {
                Some(schema) => Some(self.extraction(&task, &notes, schema, &ledger).await),
                None => None,
            }
        };
        let (answered, extraction) = if answering {
            let (answered, extraction) = tokio::join!(self.answer(state, prepared), extracting);
            (Some(answered?), extraction)
        } else {
            (None, extracting.await)
        };
        let extraction = extraction.transpose()?;
        let mut verified_ok = true;
        let mut answer_text = None;
        let mut citations = Vec::new();
        if let Some(composed) = answered {
            let (text, cited) = self.public_answer(&composed);
            answer_text = Some(text);
            citations = cited;
            state.verified_answer = Some(composed);
        }
        let mut data = Value::Null;
        if let Some(extraction) = extraction {
            data = extraction.data.unwrap_or(Value::Null);
            evidence.extend(extraction.evidence);
            verified_ok = verified_ok && extraction.problem.is_none();
        }
        let status = if verified_ok { Status::Complete } else { Status::Unverified };
        // The answer's notes and each schema field cite independently; the same words on the same page are one
        // citation.
        let mut cited: IndexMap<(String, String), Evidence> = IndexMap::new();
        for item in evidence {
            cited.entry((item.url.clone(), item.quote.clone())).or_insert(item);
        }
        let outcome = Outcome {
            answer: answer_text.clone(),
            data,
            evidence: cited.into_values().collect(),
            citations,
            ..Outcome::default()
        };
        if !finalize || (!verified_ok && !state.missing_answer_outputs.is_empty()) {
            // Rejected answer outputs return to browsing; taking their ending frame added a discarded round trip.
            return Ok(self.result(Some(state), &state.ledger.clone(), status, outcome));
        }
        if let Some(observed) = self.observed() {
            self.observe_if_changed(&observed).await?;
        }
        let mut result = self.result(Some(state), &state.ledger.clone(), status, outcome);
        let requested = state.plan().run_reports.contains(&RunReport::Screenshot);
        result = self.ending_frame(result, requested).await;
        let captured = result.final_frame.is_some();
        if !captured && requested && result.status == Status::Complete {
            // The image is what the caller asked for, and a run that cannot hand it over has not done the task.
            result.status = Status::Unverified;
        }
        let report = self.run_report(state, result.final_url.as_deref(), captured);
        if !report.is_empty() {
            result.answer = Some(match answer_text.filter(|a| !a.is_empty()) {
                Some(answer) => format!("{answer}\n\n{report}"),
                None => report,
            });
        }
        Ok(result)
    }
}

/// Record a rejected DONE in the history the next decision reads, so Jev sees what is still unmet.
fn rejected(state: &mut RunState, reason: &str) {
    state.history.push(crate::policy::HistoryEntry {
        effect: Some(reason.to_string()),
        ..crate::policy::HistoryEntry::new(Some(Operation::Done), None, StepOutcome::Failed, false)
    });
}

/// The fields a partition's finish has to itself, swapped into the run and back.
struct Child {
    task: String,
    ready_plan: Option<Plan>,
    notes: Notes,
    finish_partitions: Vec<Plan>,
    verified_answer: Option<ComposedAnswer>,
    missing_answer_outputs: Vec<String>,
    open_answer_outputs: Vec<String>,
    answer_corrections: Vec<AnswerCorrection>,
    rejected_answer_evidence: Option<BTreeSet<EvidenceKey>>,
}

impl Child {
    /// Put `child` into the run, returning what it replaced.
    fn swap(state: &mut RunState, mut child: Child) -> Child {
        use std::mem::swap;
        swap(&mut state.task, &mut child.task);
        swap(&mut state.ready_plan, &mut child.ready_plan);
        swap(&mut state.notes, &mut child.notes);
        swap(&mut state.finish_partitions, &mut child.finish_partitions);
        swap(&mut state.verified_answer, &mut child.verified_answer);
        swap(&mut state.missing_answer_outputs, &mut child.missing_answer_outputs);
        swap(&mut state.open_answer_outputs, &mut child.open_answer_outputs);
        swap(&mut state.answer_corrections, &mut child.answer_corrections);
        swap(&mut state.rejected_answer_evidence, &mut child.rejected_answer_evidence);
        child
    }
}
