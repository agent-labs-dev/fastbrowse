//! Port of `clients/failover.py`: keep a run on its backup after the chosen Jev provider exhausts its retries or refuses
//! its key, or the LLM's provider refuses its key.

use std::sync::atomic::{AtomicBool, Ordering};

use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{
    DecisionClient, DecisionError, DecisionProfile, Evaluation, Question, RetriesExhausted,
};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::LLMPurpose;
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::http::Http;
use crate::validation::{JevSpend, Lines, Paid, estimated_cost, exhausted, jev_spend, merged_cost};

pub struct FailoverDecisionClient {
    primary: Box<dyn DecisionClient>,
    backup: Box<dyn DecisionClient>,
    on_backup: AtomicBool,
    http: Http,
}

/// Records what the providers were paid into the enclosing scope when the call is dropped.
struct PaidOnDrop {
    paid: Paid,
    parent: Option<JevSpend>,
    armed: bool,
}

impl Drop for PaidOnDrop {
    fn drop(&mut self) {
        if self.armed
            && let Some(parent) = &self.parent
        {
            parent.record(self.paid.lines());
        }
    }
}

impl FailoverDecisionClient {
    /// `http` carries the trace the switch is written to.
    pub fn new(
        primary: Box<dyn DecisionClient>,
        backup: Box<dyn DecisionClient>,
        http: Http,
    ) -> FailoverDecisionClient {
        FailoverDecisionClient { primary, backup, on_backup: AtomicBool::new(false), http }
    }

    async fn evaluate_paid(
        &self,
        state: &Value,
        questions: &IndexMap<String, Question>,
    ) -> Result<Evaluation, DecisionError> {
        let paid = Paid::default();
        let mut guard = PaidOnDrop { paid: paid.clone(), parent: JevSpend::current(), armed: true };
        let result = jev_spend(Lines::Paid(paid.clone()), None, self.switching(state, questions)).await;
        guard.armed = false;
        let mut result = match result {
            Ok(result) => result,
            Err(error) => {
                if let Some(parent) = &guard.parent {
                    parent.record(paid.lines());
                }
                return Err(error);
            }
        };
        if !paid.is_empty() {
            let mut lines = paid.lines();
            lines.push(result.cost.clone());
            result.cost = fastbrowse_core::models::CostLine { seconds: result.cost.seconds, ..merged_cost(&lines) };
        }
        Ok(result)
    }

    async fn switching(
        &self,
        state: &Value,
        questions: &IndexMap<String, Question>,
    ) -> Result<Evaluation, DecisionError> {
        let on_backup = self.on_backup.load(Ordering::SeqCst);
        let client = if on_backup { &self.backup } else { &self.primary };
        let error = match client.evaluate(state, questions).await {
            Err(DecisionError::RetriesExhausted(error)) if !on_backup => error,
            // A revoked OPENROUTER_API_KEY failed every run at its first Jev call with a working gateway key beside it.
            Err(DecisionError::Rejected(reason)) if !on_backup => Box::new(RetriesExhausted {
                message: reason.clone(),
                seconds: 0.0,
                unaccounted_requests: 0,
                requests: 1,
                answered: Vec::new(),
                last: reason,
            }),
            other => return other,
        };
        // Calls already in flight may fail together; assigning the same backup without awaiting cannot flap.
        self.on_backup.store(true, Ordering::SeqCst);
        log::warn!("jev: {}; moving this run to the backup provider", error.message);
        self.http.trace.event("jev_failover", || {
            let mut fields = Map::new();
            fields.insert("reason".into(), error.message.clone().into());
            fields
        });
        let held: IndexMap<String, _> = error.answered.iter().flat_map(|result| result.answers.clone()).collect();
        let remaining: IndexMap<String, Question> =
            questions.iter().filter(|(key, _)| !held.contains_key(*key)).map(|(k, q)| (k.clone(), q.clone())).collect();
        let result = match self.backup.evaluate(state, &remaining).await {
            Ok(result) => result,
            Err(DecisionError::RetriesExhausted(backup_error)) => {
                let requests = error.requests + backup_error.requests;
                let seconds = error.seconds + backup_error.seconds;
                let mut answered = error.answered.clone();
                answered.extend(backup_error.answered.iter().cloned());
                return Err(DecisionError::RetriesExhausted(Box::new(RetriesExhausted {
                    message: exhausted(requests, seconds, &backup_error.last),
                    seconds,
                    unaccounted_requests: error.unaccounted_requests + backup_error.unaccounted_requests,
                    requests,
                    answered,
                    last: backup_error.last.clone(),
                })));
            }
            Err(other) => return Err(other),
        };
        let mut cost = result.cost.clone();
        if error.unaccounted_requests > 0 {
            // Only unanswered requests may be billed. Estimate their inputs from the answered request.
            // They went to the primary, so they are priced as its.
            let price = self.primary.profile().dollars_per_input_token;
            let discarded = estimated_cost(price, result.input_tokens * error.unaccounted_requests, 0);
            cost = fastbrowse_core::models::CostLine { seconds: cost.seconds, ..merged_cost(&[cost, discarded]) };
        }
        let mut answers = held;
        answers.extend(result.answers);
        Ok(Evaluation {
            model: result.model,
            answers,
            input_tokens: result.input_tokens + error.answered.iter().map(|part| part.input_tokens).sum::<i64>(),
            requests: result.requests + error.requests,
            cost: fastbrowse_core::models::CostLine {
                seconds: Some(error.seconds + cost.seconds.unwrap_or(0.0)),
                ..cost
            },
        })
    }
}

impl DecisionClient for FailoverDecisionClient {
    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(self.evaluate_paid(state, questions))
    }

    /// The primary's. Every default backup serves Jev as well; one with less context than the primary answers an
    /// oversized request as too large, which the step answers with a smaller state.
    fn profile(&self) -> DecisionProfile {
        self.primary.profile()
    }
}

/// The LLM on its backup provider for the rest of a run once the primary refuses its key.
///
/// A revoked OPENROUTER_API_KEY failed every run at its first call even with a working AI_GATEWAY_API_KEY beside it,
/// which serves the same models. A refused key is the one failure a retry cannot fix, so only it switches.
pub struct FailoverLLMClient {
    primary: Box<dyn LLMClient>,
    backup: Box<dyn LLMClient>,
    on_backup: AtomicBool,
    http: Http,
}

impl FailoverLLMClient {
    /// `primary` is OpenRouter's and `backup` the gateway's, the one pair whose keys serve the same models.
    pub fn new(primary: Box<dyn LLMClient>, backup: Box<dyn LLMClient>, http: Http) -> FailoverLLMClient {
        FailoverLLMClient { primary, backup, on_backup: AtomicBool::new(false), http }
    }
}

impl LLMClient for FailoverLLMClient {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        max_output_tokens: i64,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        Box::pin(async move {
            if !self.on_backup.load(Ordering::SeqCst) {
                match self.primary.generate_value(purpose, messages, schema, max_output_tokens, ledger).await {
                    Err(LLMError::Rejected(reason)) => {
                        // Calls already in flight may fail together; storing the same flag again cannot flap.
                        self.on_backup.store(true, Ordering::SeqCst);
                        log::warn!(
                            "llm: OPENROUTER_API_KEY was refused ({reason}); moving this run to AI_GATEWAY_API_KEY"
                        );
                        self.http.trace.event("llm_failover", || {
                            let mut fields = Map::new();
                            fields.insert("reason".into(), reason.clone().into());
                            fields
                        });
                    }
                    other => return other,
                }
            }
            self.backup.generate_value(purpose, messages, schema, max_output_tokens, ledger).await
        })
    }
}
