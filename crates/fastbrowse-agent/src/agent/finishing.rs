//! Whether a run may call itself finished: the done check, the verifier, the answer and its claim checks.

use std::collections::{BTreeSet, HashSet};
use std::sync::{Arc, Mutex};

use fastbrowse_core::jev::{Answer, Question};
use fastbrowse_core::llm::Generation;
use fastbrowse_core::models::{
    Bytes, CostComponent, Decider, Evidence, FactReader, LLMPurpose, Operation, RunResult, Status, StepOutcome,
    UntilCheck,
};
use fastbrowse_core::page::Observation;
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::json::{Style, write};
use fastbrowse_core::py::{sha256_hex, text};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::helpers::{
    EvidenceKey, answer_evidence, blocked_action, guessed, guessed_page, lookup, misread, not_acting, partition_proof,
    path_of, record, unread, verified, visited,
};
use super::state::{Flow, HttpFailure, RunState, Step, Stop, TransactionCandidate};
use super::{Agent, Outcome};
use crate::batches::evaluate_batches;
use crate::error::AgentError;
use crate::memory::{Fact, Notes};
use crate::planner::{Plan, RequirementKind, RunReport, partition_plan};
use crate::retrieval::{
    AnswerCorrection, Claim, ComposeOptions, ComposedAnswer, Stage, assemble_answer, compose, draft_answer,
    names_subject, retain_fields,
};
use crate::support::generate;
use crate::verification::{
    CheckOptions, DoneCheck, DoneOptions, DoneVerdict, ExtractOptions, Extraction, LLMVerdict, RecordSchema,
    VERDICT_TOKENS, VerifyOptions, check_claims, check_done, extract, verify_cache_key, verify_messages,
    visual_only_state,
};

/// An answer whose claims passed, or what a composer sent beside the verifier came back with.
pub(super) enum Prepared {
    Held(ComposedAnswer),
    Drafted(Result<Generation<ComposedAnswer>, AgentError>),
}

/// `repr` of an evidence key, the order Python sorts a proof's facts in.
fn repr_key(key: &EvidenceKey) -> String {
    let frame = key.1.as_deref().map_or("None".to_string(), text::repr);
    format!("({}, {frame}, {})", text::repr(&key.0), text::repr(&key.2))
}

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
            match partition_plan(&*self.llm, &state.task, &plan, &state.ledger, state.oversized_answer).await {
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
            let fresh = self.observe().await?;
            let reason = format!("DONE rejected, no new output evidence: {}", state.open_answer_outputs.join("; "));
            Box::pin(self.recover(state, &fresh, &reason, Status::Unverified)).await?;
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
                    answer_check_evidence: None,
                    answer_check_cache: Default::default(),
                },
            );
            let outcome = async {
                let mut supported = true;
                if state.oversized_answer && lookup(&plan) && unread(&plan, &state.notes) {
                    // Separate source reads can leave a bundled requirement unbound. Only a fully checked field answer
                    // can bind their combined quotes before the normal completion check.
                    let (answer, held) = Box::pin(self.answer(state, None)).await?;
                    supported = held;
                    if supported {
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
                }
                if supported { Box::pin(self.finish_whole(state, None, None, true)).await } else { Ok(None) }
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
            let fresh = self.observe().await?;
            self.record_failure(state, &fresh, Operation::Done, &reason, None, None, None, Decider::Llm).await?;
            Box::pin(self.recover(state, &fresh, &reason, Status::Unverified)).await?;
            return Ok(None);
        }
        let mut claims: Vec<Claim> = Vec::new();
        for claim in verified_parts.iter().flat_map(|part| part.claims.iter()) {
            if !claims.contains(claim) {
                claims.push(claim.clone());
            }
        }
        let answer = assemble_answer(&claims, &state.notes, &state.plan().requirements, 0);
        let result = match Box::pin(self.conclude(state, output_schema, Some(Prepared::Held(answer)), true)).await {
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
                Box::pin(self.recover(state, &fresh, "Completion condition not satisfied", Status::Stuck)).await?;
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
        let inspect_wall = matches!(fresh.response_status, Some(401 | 403))
            && super::helpers::access_inspection(&state.await_plan().await?);
        if let Some(status) = fresh.response_status
            && status >= 400
            && !inspect_wall
        {
            return Err(HttpFailure { url: fresh.url.clone(), status }.stop().into());
        }
        if let Some(failed) = self.failed_submission(state).await? {
            return Err(failed.stop().into());
        }
        state.await_checks().await?;
        state.reset_answer_check();
        if !state.open_answer_outputs.is_empty()
            && state.rejected_answer_evidence.as_ref() == Some(&answer_evidence(&state.notes, true))
        {
            // Re-reading the same partial quotes must not buy another completion check or reset recovery.
            if state.finish_partitions.is_empty() {
                let ids: Vec<String> = information_ids(state.plan());
                state.notes.unevidence(ids.iter().map(String::as_str));
                state.rejected_answer_evidence = Some(answer_evidence(&state.notes, true));
            }
            let reason = format!("DONE rejected, no new output evidence: {}", state.open_answer_outputs.join("; "));
            Box::pin(self.recover(state, &fresh, &reason, Status::Unverified)).await?;
            return Ok(None);
        }
        state.ledger.reserve(CostComponent::Jev, 0.0, 1)?;
        let plan = state.plan().clone();
        let draft = if plan.page_answer_expected() { draft_answer(&plan, &state.notes) } else { None };
        let notes = state.notes.clone();
        let history = record(&state.history, &self.config.observation);
        let shown = visited(&state.visited, &self.config.observation, |url| self.redact(url));
        let options = DoneOptions {
            tokens: self.config.tokens.clone(),
            history: &history,
            visited: &shown,
            trace: self.trace.clone(),
        };
        let task = state.task.clone();
        let checking =
            check_done(&*self.jev, &task, &plan, &fresh, &notes, &self.config.thresholds, draft.as_ref(), &options);
        // Without answer checks the draft's claims are checked beside the done check; with them, once it is offered.
        let (check, claimed) = match draft.as_ref().filter(|_| plan.answer_checks.is_empty()) {
            Some(draft) => {
                let claiming = self.holds(state, draft.clone());
                let (check, claimed) = tokio::join!(async { checking.await.map_err(Flow::from) }, claiming);
                (check?, Some(claimed?))
            }
            None => (checking.await?, None),
        };
        state.ledger.record([check.cost.clone()])?;
        let mut verifying: Option<Result<Generation<LLMVerdict>, Flow>> = None;
        let held = match claimed {
            Some(held) => held,
            None if check.answer.is_some() && check.verdict != DoneVerdict::Reject => {
                let doubted = self.doubt(state, &check);
                let answer = check.answer.clone().expect("matched on an answer");
                if doubted == DoneVerdict::Verify && !state.ledger.capped() {
                    // The verifier judges completion from the page and the notes, and the claim check judges the
                    // draft's claims against their quotes; neither reads the other's verdict, so they run side by
                    // side. Under a cap the verifier waits, so it reserves against the claim check's receipts.
                    // Python builds the request inside the verifier's task, so notes too large for it fail only when
                    // the verdict is awaited, after the claim check has run.
                    let request = self.verify_request(state, &fresh, &check).await;
                    let ledger = state.ledger.clone();
                    let judging = async {
                        match request {
                            Ok(request) => self.verify_done(ledger, request).await,
                            Err(error) => Err(error),
                        }
                    };
                    let (judged, held) = tokio::join!(judging, self.holds(state, answer));
                    let held = held?;
                    verifying = Some(judged.map(|(key, judged)| {
                        state.verify_cache.insert(key, judged.clone());
                        judged
                    }));
                    held
                } else {
                    Box::pin(self.holds(state, answer)).await?
                }
            }
            None => None,
        };
        let mut check = check;
        if check.answer.is_some() {
            check.answer = held;
        }
        self.event("done_check", || {
            Map::from_iter([
                ("verdict".into(), check.verdict.as_str().into()),
                ("complete".into(), fastbrowse_core::py::round(check.complete, 3).into()),
                ("unmet".into(), json!(check.unmet)),
            ])
        });
        check.verdict = self.doubt(state, &check);
        let mut accepted = check.verdict == DoneVerdict::Accept;
        let mut missing: Vec<String> = Vec::new();
        let mut misread_ids: HashSet<String> = HashSet::new();
        let mut handed: Option<Prepared> = None;
        if check.verdict == DoneVerdict::Verify {
            // Write the answer while the verifier is still deciding. Both read the same finished notes, and every
            // accepted run wants an answer, so the whole cost of guessing wrong is one discarded call.
            // A verdict already in hand, from the verifier sent beside the claim check, needs no answer drafted against it:
            // Python cancels that draft before it starts when its verifier has already answered.
            let drafting = plan.page_answer_expected()
                && check.answer.is_none()
                && state.verified_answer.is_none()
                && verifying.is_none();
            let request = match verifying.take() {
                Some(judged) => Err(judged),
                // A request that cannot be built fails when its verdict is awaited, after the draft was sent, as in Python.
                None => Ok(self.verify_request(state, &fresh, &check).await),
            };
            let options = if drafting { Some(self.compose_options(state).await?) } else { None };
            let ledger = state.ledger.clone();
            let judging = async {
                match request {
                    Err(judged) => judged.map(|judged| (None, judged)),
                    Ok(Err(error)) => Err(error),
                    Ok(Ok(request)) => {
                        self.verify_done(ledger.clone(), request).await.map(|(key, judged)| (Some(key), judged))
                    }
                }
            };
            let composing = async {
                let options = ComposeOptions { ledger: Some(&ledger), ..options? };
                Some(compose(&*self.llm, &state.task, &plan, &state.notes, &options).await.map_err(Flow::from))
            };
            let (judged, drafted) = tokio::join!(judging, composing);
            let judged = judged.map(|(key, judged)| {
                if let Some(key) = key {
                    state.verify_cache.insert(key, judged.clone());
                }
                judged
            });
            let judged = judged?;
            handed = match drafted {
                Some(Ok(generation)) => Some(Prepared::Drafted(Ok(generation))),
                Some(Err(Flow::Agent(error))) => Some(Prepared::Drafted(Err(error))),
                Some(Err(other)) => return Err(other),
                None => None,
            };
            // After a covered or failed action, a requirement Jev did not confirm is not excused by its quote: an ad
            // covered a new window's link, and the run cited the heading of the page it was on.
            let mut held_unmet = check.unmet.clone();
            if blocked_action(&state.history) {
                held_unmet.extend(check.doubted.iter().cloned());
            }
            accepted = verified(&judged.data, &plan, &state.notes, &state.invented, Some(&state.lineage), &held_unmet);
            // Asked to open httpx's project page, a run picked DONE on pypi.org before acting, Jev held the one
            // requirement unmet, and flash-lite called it complete. Only an action can do what Jev says is undone.
            let idle = !state.history.iter().any(|h| {
                h.outcome == StepOutcome::Executed
                    && h.operation.is_none_or(|op| !not_acting(op) && op != Operation::Escalate)
            });
            let undone =
                plan.requirements.iter().any(|r| r.kind == RequirementKind::Action && check.unmet.contains(&r.id));
            accepted = accepted && !(idle && undone);
            missing = judged.data.missing.clone();
            misread_ids = misread(&judged.data, &plan, &state.notes, &state.invented, Some(&state.lineage));
        }
        if accepted {
            let prepared = match check.answer.clone() {
                Some(answer) => Some(Prepared::Held(answer)),
                None => handed.take(),
            };
            let result = Box::pin(self.conclude(state, output_schema, prepared, !partition)).await?;
            fresh = self.observed().unwrap_or(fresh);
            let mut result = Some(result);
            if result.as_ref().is_some_and(|r| r.status == Status::Unverified)
                && !state.missing_answer_outputs.is_empty()
            {
                if partition {
                    return Ok(result);
                }
                // An ordinary rejected answer needs another read; grouped recovery retains its source proof.
                if !state.oversized_answer {
                    let ids = information_ids(state.plan());
                    state.notes.unevidence(ids.iter().map(String::as_str));
                }
                state.rejected_answer_evidence = Some(answer_evidence(&state.notes, true));
                accepted = false;
                result = None;
            }
            if accepted && let Some(until) = until {
                let url = self.raw().map_or_else(|| fresh.url.clone(), |raw| raw.url);
                accepted = until(url).await;
            }
            if accepted {
                return Ok(result);
            }
        }
        if partition {
            return Ok(Some(self.result(Some(state), &state.ledger.clone(), Status::Unverified, Outcome::default())));
        }
        let requirements: IndexMap<String, String> =
            plan.requirements.iter().map(|r| (r.id.clone(), r.text.clone())).collect();
        let mut unmet: Vec<String> = Vec::new();
        let mut named: Vec<&String> = Vec::new();
        for key in check.unmet.iter().chain(missing.iter()) {
            if !named.contains(&key) {
                named.push(key);
            }
        }
        for key in named {
            if !misread_ids.contains(key) {
                unmet.push(format!("{key}: {}", requirements.get(key).unwrap_or(key)));
            }
        }
        let mut misread_sorted: Vec<&String> = misread_ids.iter().collect();
        misread_sorted.sort();
        for key in misread_sorted {
            let mut urls: Vec<String> = Vec::new();
            for item in state.notes.supporting_evidence(key) {
                if !urls.contains(&item.url) {
                    urls.push(item.url);
                }
            }
            unmet.push(format!(
                "{key}: {} (read off {}, an address built from the task, not the page it describes)",
                requirements[key],
                urls.join(", ")
            ));
        }
        let judge = guessed_page(&state.invented, Some(&state.lineage), false);
        state.notes.discredit(misread_ids.iter().map(String::as_str), Arc::new(judge));
        unmet.extend(state.missing_answer_outputs.iter().map(|output| format!("Answer output not verified: {output}")));
        let joined = if unmet.is_empty() { "completion not confirmed".to_string() } else { unmet.join("; ") };
        let reason = self.redact(&format!("DONE rejected: {joined}"));
        state.history.push(crate::policy::HistoryEntry {
            effect: Some(reason.clone()),
            ..crate::policy::HistoryEntry::new(Some(Operation::Done), None, StepOutcome::Failed, false)
        });
        // The verifier only runs when Jev's done check doubts; otherwise Jev's verdict is the last word.
        let judge = if check.verdict == DoneVerdict::Verify { Decider::Llm } else { Decider::Jev };
        self.record_failure(state, &fresh, Operation::Done, &reason, None, None, None, judge).await?;
        let gives_up_as = if state.missing_answer_outputs.is_empty() { Status::Stuck } else { Status::Unverified };
        Box::pin(self.recover(state, &fresh, &reason, gives_up_as)).await?;
        Ok(None)
    }

    /// The verdict the run acts on: Jev judges the notes, not where they were read, so guessed searches go to the
    /// verifier, and a doubted lookup with every requirement cited needs none.
    fn doubt(&self, state: &RunState, check: &DoneCheck) -> DoneVerdict {
        let plan = state.plan();
        let lineage = Some(&state.lineage);
        let guessed_ids = guessed(plan, &state.notes, &state.invented, false, lineage);
        if check.verdict == DoneVerdict::Accept
            && !guessed(plan, &state.notes, &state.invented, true, lineage).is_empty()
        {
            DoneVerdict::Verify
        } else if check.verdict == DoneVerdict::Verify
            && check.unmet.is_empty()
            && guessed_ids.is_empty()
            && lookup(plan)
            && !blocked_action(&state.history)
        {
            // A doubted lookup reaches here with every requirement cited, and the verifier excuses a cited requirement,
            // so all it could still do was refuse naming nothing, which it never did in 76 0.5.7 verifications. A run
            // with a covered or failed action is the exception: it may be reading the page it meant to leave.
            DoneVerdict::Accept
        } else {
            check.verdict
        }
    }

    /// The verifier's request for this finish attempt, and the verdict an identical earlier request already bought.
    async fn verify_request(&self, state: &RunState, fresh: &Observation, check: &DoneCheck) -> Step<VerifyRequest> {
        // The screenshot goes only when the page carries state that the text cannot.
        let screenshots = if visual_only_state(fresh) { self.screenshots().await? } else { Vec::new() };
        let mut invented: Vec<String> = state.invented.iter().cloned().collect();
        invented.sort();
        let history = record(&state.history, &self.config.observation);
        let shown = visited(&state.visited, &self.config.observation, |url| self.redact(url));
        let options = VerifyOptions {
            doubted: &check.doubted,
            invented: &invented,
            visited: &shown,
            config: self.config.clone(),
            ledger: None,
        };
        let messages =
            verify_messages(&state.task, state.plan(), fresh, &screenshots, &state.notes, &history, &options)?;
        let key = verify_cache_key(&messages);
        let reused = state.verify_cache.get(&key).cloned();
        if reused.is_some() {
            self.event("verify_reused", || Map::from_iter([("doubted".into(), json!(check.doubted))]));
        }
        Ok(VerifyRequest { messages, key, reused })
    }

    /// The verifier's verdict, billed as it arrives: run beside the claim check, it can finish before a failure there
    /// discards it. A request byte-identical to an earlier one in this run reuses the earlier verdict.
    async fn verify_done(
        &self,
        ledger: Arc<fastbrowse_core::telemetry::Ledger>,
        request: VerifyRequest,
    ) -> Step<(String, Generation<LLMVerdict>)> {
        if let Some(reused) = request.reused {
            return Ok((request.key, reused));
        }
        let judged =
            generate::<LLMVerdict>(&*self.llm, LLMPurpose::Verify, &request.messages, VERDICT_TOKENS, Some(&ledger))
                .await?;
        ledger.record([judged.cost.clone()])?;
        Ok((request.key, judged))
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
        })
    }

    async fn composed(&self, state: &mut RunState) -> Step<Generation<ComposedAnswer>> {
        let options = self.compose_options(state).await?;
        let ledger = state.ledger.clone();
        let plan = state.plan().clone();
        let options = ComposeOptions { ledger: Some(&ledger), ..options };
        Ok(compose(&*self.llm, &state.task, &plan, &state.notes, &options).await?)
    }

    /// Return the checked draft, or compose an answer and check its claims.
    pub(super) async fn answer(
        &self,
        state: &mut RunState,
        prepared: Option<Prepared>,
    ) -> Step<(ComposedAnswer, bool)> {
        let mut prepared = match prepared {
            Some(Prepared::Held(answer)) => return Ok((answer, true)),
            other => other,
        };
        if let Some(verified_answer) = state.verified_answer.clone() {
            // New context can invalidate a cache entry without changing a previously supported answer.
            prepared = None;
            if let Some(held) = Box::pin(self.holds(state, verified_answer)).await? {
                return Ok((held, true));
            }
        }
        let mut facts = draft_answer(state.plan(), &state.notes);
        let failed_checks = |corrections: &[AnswerCorrection]| -> (usize, usize) {
            let failures: HashSet<(Stage, String, Vec<String>)> = corrections
                .iter()
                .map(|c| (c.stage, c.criterion.clone(), c.claims.iter().map(Dump::dump_json).collect()))
                .collect();
            (
                failures.iter().filter(|f| f.0 == Stage::Source).count(),
                failures.iter().filter(|f| f.0 == Stage::Assertion).count(),
            )
        };
        let mut previous = (!state.answer_corrections.is_empty()).then(|| failed_checks(&state.answer_corrections));
        let generated = match prepared {
            Some(Prepared::Drafted(drafted)) => drafted.map_err(Flow::from),
            _ => self.composed(state).await,
        };
        let mut composed = match generated {
            Ok(generation) => generation.data,
            Err(flow) if flow.is_llm() && facts.is_some() => {
                // A composer that fails (one looped to its output cap) leaves the reader's facts, each with its quote,
                // which is an answer the claim check can still pass.
                previous = Some((0, 0));
                facts.take().expect("checked above")
            }
            Err(flow) => return Err(flow),
        };
        let mut held = Box::pin(self.holds(state, composed.clone())).await?;
        while held.is_none() && !state.answer_corrections.is_empty() {
            let failures = failed_checks(&state.answer_corrections);
            // Fixing cited sources can expose assertion failures; after that, only fewer assertions buy a repair.
            if previous.is_some_and(|previous| failures >= previous) {
                break;
            }
            previous = Some(failures);
            let corrected = match self.composed(state).await {
                Ok(corrected) => corrected,
                Err(flow) if flow.is_llm() => break,
                Err(flow) => return Err(flow),
            };
            composed = retain_fields(corrected.data, &composed, &state.notes);
            held = Box::pin(self.holds(state, composed.clone())).await?;
        }
        if held.is_none()
            && let Some(facts) = facts
            && facts != composed
        {
            // A list of forty records came back as one claim citing one quote, which no claim check should pass. The
            // reader's own facts each carry the quote that shows them, so they are offered to the same check.
            let failure = (
                state.missing_answer_outputs.clone(),
                state.open_answer_outputs.clone(),
                state.answer_corrections.clone(),
            );
            let fallback = retain_fields(facts, &composed, &state.notes);
            held = Box::pin(self.holds(state, fallback)).await?;
            if held.is_none() {
                // Recovery follows the returned answer's failure, not a sparser fallback's missing fields.
                (state.missing_answer_outputs, state.open_answer_outputs, state.answer_corrections) = failure;
            }
        }
        let supported = held.is_some();
        Ok((held.unwrap_or(composed), supported))
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

    pub(super) async fn holds(&self, state: &mut RunState, answer: ComposedAnswer) -> Step<Option<ComposedAnswer>> {
        let plan = state.plan().clone();
        if !state.finish_partitions.is_empty() && !plan.answer_checks.is_empty() && !answer.is_field_answer() {
            state.missing_answer_outputs = plan.answer_checks.clone();
            state.open_answer_outputs = plan.answer_checks.clone();
            return Ok(None);
        }
        let mut answer = answer;
        if let Some(outputs) = &answer.output_claims {
            // Decomposition cannot erase an original obligation by leaving its field out of the generated checks.
            let mut merged: IndexMap<String, Vec<usize>> = plan
                .requirements
                .iter()
                .filter(|r| r.kind == RequirementKind::Information)
                .map(|r| (r.text.clone(), (0..answer.claims.len()).collect()))
                .collect();
            for (check, indices) in outputs {
                merged.insert(check.clone(), indices.clone());
            }
            answer.output_claims = Some(merged);
        }
        let mut facts: Vec<EvidenceKey> = answer_evidence(&state.notes, true).into_iter().collect();
        facts.sort_by_key(repr_key);
        let sources: Map<String, Value> = state.notes.evidence().into_iter().map(|(key, e)| (key, e.dump())).collect();
        let proof = sha256_hex(
            write(
                &json!({"facts": facts, "sources": sources, "refused": state.notes.refused_ids()}),
                Style::DUMPS.sorted(),
            )
            .as_bytes(),
        );
        if state.answer_check_evidence.as_ref() != Some(&proof) {
            state.answer_check_cache.clear();
            state.answer_check_evidence = Some(proof);
        }
        let mut transactions = self.transaction_evidence_ids(state).await?;
        transactions.sort();
        let key = (state.task.clone(), plan.dump_json(), answer.dump_json(), transactions.clone());
        let (held, missing, corrections) = match state.answer_check_cache.get(&key) {
            Some(cached) => cached.clone(),
            None => {
                let missing = Mutex::new(Vec::new());
                let corrections = Mutex::new(Vec::new());
                let verified_fields = Mutex::new(Vec::new());
                let mut checks: Vec<String> = Vec::new();
                // Each information requirement is asked after the plan's checks, so a requirement the generated checks
                // leave out (a release's date beside its version) is still found missing.
                if !plan.answer_checks.is_empty() {
                    for check in plan.answer_checks.iter().chain(
                        plan.requirements.iter().filter(|r| r.kind == RequirementKind::Information).map(|r| &r.text),
                    ) {
                        if !checks.contains(check) {
                            checks.push(check.clone());
                        }
                    }
                }
                let collect_fields = state.oversized_answer && answer.is_field_answer();
                let options = CheckOptions {
                    tokens: self.config.tokens.clone(),
                    ledger: Some(&state.ledger),
                    missing_outputs: Some(&missing),
                    corrections: Some(&corrections),
                    verified_outputs: None,
                    independent_fields: state.oversized_answer,
                    trace: self.trace.clone(),
                    transaction_evidence_ids: transactions,
                    answer_checks: checks,
                    verified_fields: collect_fields.then_some(&verified_fields),
                };
                let held = check_claims(&*self.jev, &answer, &state.notes, &self.config.thresholds, &options).await?;
                drop(options);
                let mut missing = missing.into_inner().unwrap_or_else(|p| p.into_inner());
                let corrections = corrections.into_inner().unwrap_or_else(|p| p.into_inner());
                let verified_fields = verified_fields.into_inner().unwrap_or_else(|p| p.into_inner());
                if let Some(partial) = verified_fields.first() {
                    let outputs = partial.output_claims.clone().unwrap_or_default();
                    let fields: IndexMap<String, Vec<usize>> =
                        outputs.into_iter().filter(|(check, _)| plan.answer_checks.contains(check)).collect();
                    if !fields.is_empty() {
                        let mut claims: Vec<Claim> = Vec::new();
                        for index in fields.values().flatten() {
                            let claim = &partial.claims[*index];
                            if !claims.contains(claim) {
                                claims.push(claim.clone());
                            }
                        }
                        let base = assemble_answer(&claims, &state.notes, &plan.requirements, 0);
                        let proof = partition_proof(&plan, &state.notes, &state.invented);
                        let outputs = fields
                            .iter()
                            .map(|(check, indices)| {
                                let positions = indices
                                    .iter()
                                    .map(|i| claims.iter().position(|c| *c == partial.claims[*i]).expect("collected"))
                                    .collect();
                                (check.clone(), positions)
                            })
                            .collect();
                        state.partial_answers.insert(proof, base.with_outputs(outputs));
                    }
                }
                if state.oversized_answer
                    && answer.is_field_answer()
                    && plan.answer_checks.iter().any(|check| missing.contains(check))
                {
                    missing.retain(|check| plan.answer_checks.contains(check));
                }
                // DONE and the composer fallback offered the same draft twice against unchanged proof.
                let entry = (held, missing, corrections);
                state.answer_check_cache.insert(key, entry.clone());
                entry
            }
        };
        state.answer_corrections = corrections;
        let mut unique: Vec<String> = Vec::new();
        for item in missing {
            if !unique.contains(&item) {
                unique.push(item);
            }
        }
        state.missing_answer_outputs = unique;
        if held.is_some() {
            state.open_answer_outputs = Vec::new();
            state.rejected_answer_evidence = None;
        } else if !state.missing_answer_outputs.is_empty() {
            state.open_answer_outputs = state.missing_answer_outputs.clone();
        }
        Ok(held)
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
        prepared: Option<Prepared>,
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
        if let Some((composed, held)) = answered {
            verified_ok = held;
            let (text, cited) = self.public_answer(&composed);
            answer_text = Some(text);
            citations = cited;
            if held {
                state.verified_answer = Some(composed);
            }
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
        let _ = Bytes(Vec::new());
        Ok(result)
    }
}

/// A verifier request rendered for one finish attempt.
pub(super) struct VerifyRequest {
    messages: Vec<fastbrowse_core::llm::Message>,
    key: String,
    reused: Option<Generation<LLMVerdict>>,
}

fn information_ids(plan: &Plan) -> Vec<String> {
    plan.requirements.iter().filter(|r| r.kind == RequirementKind::Information).map(|r| r.id.clone()).collect()
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
    answer_check_evidence: Option<String>,
    answer_check_cache: super::state::AnswerChecks,
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
        swap(&mut state.answer_check_evidence, &mut child.answer_check_evidence);
        swap(&mut state.answer_check_cache, &mut child.answer_check_cache);
        child
    }
}
