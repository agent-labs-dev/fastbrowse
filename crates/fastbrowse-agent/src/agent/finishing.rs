//! How a run ends: Jev says DONE, and the answer is what Jev read.

use fastbrowse_core::models::{Decider, Evidence, Operation, RunResult, Status, StepOutcome, UntilCheck};
use fastbrowse_core::page::Observation;
use indexmap::IndexMap;
use serde_json::Value;

use super::helpers::unsent_fact;
use super::recovery::DONE;
use super::state::{RunState, Step, Stop};
use super::{Agent, Outcome};
use crate::memory::Notes;
use crate::retrieval::{compose, draft_answer, verdict};
use crate::verification::{ExtractOptions, Extraction, RecordSchema, extract};

impl Agent {
    async fn observe_if_changed(&self, observation: &Observation) -> Step<Observation> {
        let raw = self.raw().unwrap_or_else(|| observation.clone());
        if self.page.document_changed(&raw).await? {
            return self.observe().await;
        }
        Ok(observation.clone())
    }

    /// The final result of a DONE, or None when the caller's `until` sends the loop back to work.
    pub(super) async fn finish(
        &self,
        state: &mut RunState,
        observation: &Observation,
        output_schema: Option<&RecordSchema>,
        until: Option<&UntilCheck>,
    ) -> Step<Option<RunResult>> {
        // A task to carry out is not done by reading. Over HTTP nothing can be carried out, and what scripts build on
        // the page was never there to see.
        let acted = state.history.iter().any(|entry| {
            entry.outcome == StepOutcome::Executed
                && !matches!(entry.operation, None | Some(Operation::Read | Operation::Scroll | Operation::ScrollUp))
        });
        if !acted && output_schema.is_none() && !self.asks_to_be_told(state, observation).await? {
            // What was read did not do it, so it answers nothing yet.
            state.notes = Notes::default();
            let reason = "The task asks for something to be carried out, and no step has done it.";
            if self.page.over_http() {
                return Err(self.page.needs_browser(reason.into()).into());
            }
            self.recover(state, observation, reason, Status::Stuck, Some((Operation::Done, None))).await?;
            return Ok(None);
        }
        // Jev can say DONE on what it never read, or on a page its last action changed, and an answer comes only from
        // what it read.
        let unread = state.read_documents.get(&observation.document_key) != Some(&false);
        if output_schema.is_none()
            && (state.notes.facts().is_empty() || (!observation.document_key.is_empty() && unread))
        {
            let capture = self.capture().await?;
            self.read(state, capture, observation, state.compose).await?;
        }
        // Ending on a failed page with nothing read is that failure, not a finished task.
        if state.notes.facts().is_empty()
            && let Some(failure) = state.site_failure.as_ref().filter(|failure| failure.url == observation.url)
        {
            return Err(failure.stop().into());
        }
        // DONE on nothing read, when the task wants to be told something, is a step short of the answer.
        if output_schema.is_none() && state.notes.facts().is_empty() && self.asks_to_be_told(state, observation).await?
        {
            let reason = "Nothing read so far answers the task, so it is not done.";
            self.recover(state, observation, reason, Status::Stuck, Some((Operation::Done, None))).await?;
            return Ok(None);
        }
        // A form filled and left unsent is an ending of its own when that is where the task said to stop and ask.
        if let Some(unsent) = unsent_fact(&state.history, observation)
            && self.stops_to_confirm(state, observation).await?
        {
            return Err(Stop::new(Status::NeedsConfirmation, unsent).into());
        }
        let result = Box::pin(self.conclude(state, output_schema)).await?;
        let reached = match until {
            Some(until) => until(self.raw().map_or_else(|| observation.url.clone(), |raw| raw.url)).await,
            None => true,
        };
        if reached {
            return Ok(Some(result));
        }
        let reason = "The caller's until condition failed on this page.";
        state.history.push(crate::policy::HistoryEntry {
            effect: Some(reason.to_string()),
            ..crate::policy::HistoryEntry::new(Some(Operation::Done), None, StepOutcome::Failed, false)
        });
        self.record_failure(state, observation, Operation::Done, reason, None, None, None, Decider::Code).await?;
        Box::pin(self.count_progress(state, false, observation, DONE, Status::Stuck)).await?;
        Ok(None)
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
        Ok(extract(&*self.jev, task, &capture, schema, &options).await?)
    }

    pub(super) async fn conclude(&self, state: &mut RunState, output_schema: Option<&RecordSchema>) -> Step<RunResult> {
        let mut evidence: Vec<Evidence> = state.notes.facts().iter().filter_map(|fact| fact.evidence.clone()).collect();
        let extraction = match output_schema {
            Some(schema) => {
                let (notes, task, ledger) = (state.notes.clone(), state.task.clone(), state.ledger.clone());
                Some(self.extraction(&task, &notes, schema, &ledger).await?)
            }
            None => None,
        };
        let answered = if state.compose {
            let (notes, task, ledger) = (state.notes.clone(), state.task.clone(), state.ledger.clone());
            match self.trace.phase("verdict", verdict(&*self.jev, &task, &notes, Some(&ledger))).await? {
                Some(chosen) => Some(chosen),
                None => Some(self.trace.phase("compose", compose(&*self.llm, &task, &notes, Some(&ledger))).await?),
            }
        } else {
            draft_answer(&state.notes)
        };
        let mut answer_text = None;
        let mut citations = Vec::new();
        if let Some(composed) = answered {
            let (text, cited) = self.public_answer(&composed);
            answer_text = Some(text);
            citations = cited;
        }
        let mut data = Value::Null;
        let mut status = Status::Complete;
        if let Some(extraction) = extraction {
            data = extraction.data.unwrap_or(Value::Null);
            evidence.extend(extraction.evidence);
            if extraction.problem.is_some() {
                status = Status::Unverified;
            }
        }
        // The answer's notes and each schema field cite independently; the same words on the same page are one
        // citation.
        let mut cited: IndexMap<(String, String), Evidence> = IndexMap::new();
        for item in evidence {
            cited.entry((item.url.clone(), item.quote.clone())).or_insert(item);
        }
        let outcome = Outcome {
            answer: answer_text,
            data,
            evidence: cited.into_values().collect(),
            citations,
            ..Outcome::default()
        };
        if let Some(observed) = self.observed() {
            self.observe_if_changed(&observed).await?;
        }
        let result = self.result(Some(state), &state.ledger.clone(), status, outcome);
        Ok(self.ending_frame(result).await)
    }
}
