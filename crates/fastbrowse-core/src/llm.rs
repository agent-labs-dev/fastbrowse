//! Port of `src/fastbrowse/llm.py`: the contract for the injected LLM, structured generation for a named purpose.
//!
//! Python's `generate` is generic over the pydantic model it fills. A generic method would keep the trait from
//! being a `dyn` object, which a failover between providers needs, so the trait takes the response model as a
//! [`ResponseSchema`] and returns its validated JSON, and [`LLMClientExt::generate`] puts the type back.

use std::future::Future;
use std::marker::PhantomData;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::BoxFuture;
use crate::models::{Bytes, CostLine, LLMPurpose, MaybeUnavailable};
use crate::schema::{Schema, Ty, json_schema};
use crate::telemetry::{BudgetExceeded, Ledger};

/// Planning and field responses are short; readers and composers pass their larger configured caps.
pub const DEFAULT_MAX_OUTPUT_TOKENS: i64 = 2000;

literal_enum! {
    pub enum Role {
        System = "system",
        User = "user",
        Assistant = "assistant",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// JPEG or PNG bytes; clients that cannot see images must fail with `LLMError`, never drop them silently.
    #[serde(default)]
    pub images: Vec<Bytes>,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Message {
        Message { role, content: content.into(), images: Vec::new() }
    }
    pub fn system(content: impl Into<String>) -> Message {
        Message::new(Role::System, content)
    }
    pub fn user(content: impl Into<String>) -> Message {
        Message::new(Role::User, content)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Generation<T> {
    pub data: T,
    pub cost: CostLine,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LLMError {
    /// The model failed to return data matching the schema, or the transport failed.
    #[error("{0}")]
    Failed(String),
    /// A retryable HTTP status or a transport failure outlasted the provider's retry budget.
    #[error("{0}")]
    RetriesExhausted(String),
    /// The provider refused the key (HTTP 401 or 403), which no retry on that provider can change.
    #[error("{0}")]
    Rejected(String),
    /// The client's reservation, or the receipt it recorded, broke a limit.
    #[error(transparent)]
    Budget(#[from] BudgetExceeded),
}

impl MaybeUnavailable for LLMError {
    fn is_unavailable(&self) -> bool {
        matches!(self, LLMError::RetriesExhausted(_))
    }
}

/// The response model a request asks for: its name and schema on the wire, and the validation its answer must pass.
pub trait ResponseSchema: Send + Sync {
    /// `schema.__name__`, the `json_schema.name` of the request.
    fn name(&self) -> String;

    /// `schema.model_json_schema()`.
    fn json_schema(&self) -> Value;

    /// `schema.model_validate_json(text)`, as the model's `model_dump(mode="json")`.
    fn validate_json(&self, text: &str) -> Result<Value, String>;
}

/// A Rust type as a response model.
pub struct Typed<T>(PhantomData<fn() -> T>);

impl<T> Default for Typed<T> {
    fn default() -> Self {
        Typed(PhantomData)
    }
}

impl<T: Schema + DeserializeOwned + Serialize> ResponseSchema for Typed<T> {
    fn name(&self) -> String {
        match T::ty() {
            Ty::Model(model) => model.name.clone(),
            _ => std::any::type_name::<T>().to_string(),
        }
    }

    fn json_schema(&self) -> Value {
        json_schema(&T::ty())
    }

    fn validate_json(&self, text: &str) -> Result<Value, String> {
        crate::validation::typed_json::<T>(text)
    }
}

pub trait LLMClient: Send + Sync {
    /// Return `schema`-validated data. `purpose` lets callers route models and attribute cost.
    ///
    /// The client reserves against `ledger` once per HTTP request it makes, because only it knows how many a repair or
    /// a re-ask costs; a caller that reserved instead would undercount them. A dropped future must stop owned
    /// requests and record their settled or unknown charges before it is gone, so no paid request outlives its cost
    /// result.
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        max_output_tokens: i64,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>>;
}

/// `generate` with the response type named, as Python's generic method has it.
pub trait LLMClientExt: LLMClient {
    fn generate<'a, T>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        max_output_tokens: i64,
        ledger: Option<&'a Ledger>,
    ) -> impl Future<Output = Result<Generation<T>, LLMError>> + Send + 'a
    where
        T: Schema + DeserializeOwned + Serialize + Send + 'a,
        Self: Sized,
    {
        async move {
            let schema = Typed::<T>::default();
            let generated = self.generate_value(purpose, messages, &schema, max_output_tokens, ledger).await?;
            let data = serde_json::from_value(generated.data).map_err(|e| LLMError::Failed(e.to_string()))?;
            Ok(Generation { data, cost: generated.cost })
        }
    }
}

impl<C: LLMClient + ?Sized> LLMClientExt for C {}

/// The generation, its receipt already in `ledger`.
///
/// A successful generation leaves its receipt to the caller. A call run beside another returned into a task that
/// could be dropped before its caller recorded it: a verifier that finished while the audit beside it failed was
/// paid for and never billed. Recorded where it was made, a finished call is a billed call, whatever happens to its
/// sibling. A receipt that breaks a limit is kept and the limit reported.
pub async fn recorded<T, F>(generating: F, ledger: Option<&Ledger>) -> Result<Generation<T>, LLMError>
where
    F: Future<Output = Result<Generation<T>, LLMError>>,
{
    let generated = generating.await?;
    if let Some(ledger) = ledger {
        ledger.record([generated.cost.clone()])?;
    }
    Ok(generated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CostBasis, CostComponent, Limits};
    use crate::py::dump::Dump;
    use crate::testing::block_on;

    fn line(dollars: f64) -> CostLine {
        CostLine::new(CostComponent::Llm, CostBasis::Metered, Some(dollars))
    }

    #[test]
    fn a_recorded_generation_is_billed_where_it_finished() {
        let ledger = Ledger::new(Limits::default());
        let generated = block_on(recorded(async { Ok(Generation { data: 1, cost: line(0.5) }) }, Some(&ledger)));
        assert_eq!(generated.unwrap().data, 1);
        assert_eq!(ledger.breakdown().known_dollars(), 0.5);
    }

    #[test]
    fn a_receipt_over_the_cap_is_kept_and_reported() {
        let ledger = Ledger::new(Limits { max_dollars: Some(0.1), ..Limits::default() });
        let generated = block_on(recorded(async { Ok(Generation { data: (), cost: line(0.5) }) }, Some(&ledger)));
        assert!(matches!(generated, Err(LLMError::Budget(_))));
        assert_eq!(ledger.breakdown().lines.len(), 1);
    }

    #[test]
    fn a_message_dumps_as_pydantic_does() {
        assert_eq!(Message::user("hi").dump_json(), r#"{"role":"user","content":"hi","images":[]}"#);
    }
}
