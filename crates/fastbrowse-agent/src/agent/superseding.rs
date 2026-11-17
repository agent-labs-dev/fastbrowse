//! The run's clients, which send nothing for a decision the page redrew under ([`fastbrowse_core::superseding`]).

use std::sync::Arc;

use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{DecisionClient, DecisionError, DecisionProfile, Evaluation, Images, Question};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{CostComponent, JevPurpose, LLMPurpose};
use fastbrowse_core::superseding::superseded;
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde_json::Value;

/// The run's decision client, sending nothing for a superseded decision.
pub(super) struct Decisions(pub(super) Arc<dyn DecisionClient>);

impl DecisionClient for Decisions {
    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async move {
            if superseded() {
                return Err(DecisionError::Superseded);
            }
            self.0.evaluate(state, questions).await
        })
    }

    fn evaluate_images<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
        images: &'a Images,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async move {
            if superseded() {
                return Err(DecisionError::Superseded);
            }
            self.0.evaluate_images(state, questions, images).await
        })
    }

    fn profile(&self) -> DecisionProfile {
        self.0.profile()
    }

    fn reserves(&self, purpose: JevPurpose) -> Option<CostComponent> {
        self.0.reserves(purpose)
    }
}

/// The run's LLM client, sending nothing for a superseded decision.
pub(super) struct Generations(pub(super) Arc<dyn LLMClient>);

impl LLMClient for Generations {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        Box::pin(async move {
            if superseded() {
                return Err(LLMError::Superseded);
            }
            self.0.generate_value(purpose, messages, schema, ledger).await
        })
    }

    fn context_tokens(&self, purpose: LLMPurpose) -> BoxFuture<'_, Option<i64>> {
        self.0.context_tokens(purpose)
    }
}
