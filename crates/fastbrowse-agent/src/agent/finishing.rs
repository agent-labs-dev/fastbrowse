//! How a run ends: Jev says DONE, and the answer is what Jev read.

use fastbrowse_core::models::{Decider, Evidence, Operation, RunResult, Status, StepOutcome, UntilCheck};
use fastbrowse_core::page::Observation;
use indexmap::IndexMap;
use serde_json::Value;

use super::recovery::DONE;
use super::state::{RunState, Step};
use super::{Agent, Outcome};
use crate::memory::Notes;
use crate::retrieval::{compose, draft_answer};
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
        // Jev can say DONE on what it never read, and an answer comes only from what it read.
        if output_schema.is_none() && state.notes.facts().is_empty() {
            let capture = self.capture().await?;
            self.read(state, capture, observation).await?;
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
            Some(self.trace.phase("compose", compose(&*self.llm, &task, &notes, Some(&ledger))).await?)
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
