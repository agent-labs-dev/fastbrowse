//! The exceptions the ported modules raise, as one error a caller can match on.
//!
//! Python lets each raise travel up through whatever called it; the loop then tells a budget stop from an outage by
//! its class. Here every module returns this one type, so a budget stop reached inside an LLM client stays a budget
//! stop and is never mistaken for a failed generation.

use fastbrowse_core::jev::JevError;
use fastbrowse_core::llm::LLMError;
use fastbrowse_core::models::MaybeUnavailable;
use fastbrowse_core::telemetry::BudgetExceeded;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Jev(#[from] JevError),
    /// Never `LLMError::Budget`: that one arrives as [`AgentError::Budget`].
    #[error(transparent)]
    Llm(LLMError),
    #[error(transparent)]
    Budget(#[from] BudgetExceeded),
    /// `policy.ObservationTooLarge`: the page's state cannot fit Jev's limits even with no controls offered.
    #[error("{0}")]
    ObservationTooLarge(String),
    /// `memory.NotesTooLarge`: a verdict cannot fit its requirement evidence without losing facts.
    #[error("{0}")]
    NotesTooLarge(String),
    /// Python's `ValueError`: an answer or an argument the code cannot use.
    #[error("{0}")]
    Invalid(String),
}

impl From<LLMError> for AgentError {
    fn from(error: LLMError) -> Self {
        match error {
            LLMError::Budget(budget) => AgentError::Budget(budget),
            other => AgentError::Llm(other),
        }
    }
}

impl MaybeUnavailable for AgentError {
    fn is_unavailable(&self) -> bool {
        match self {
            AgentError::Jev(error) => error.is_unavailable(),
            AgentError::Llm(error) => error.is_unavailable(),
            _ => false,
        }
    }
}

pub type Result<T, E = AgentError> = std::result::Result<T, E>;
