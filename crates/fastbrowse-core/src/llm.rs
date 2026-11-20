//! The contract for the injected LLM, structured generation for a named purpose.
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
use crate::models::{Bytes, CostLine, FailureKind, LLMPurpose, MaybeUnavailable};
use crate::schema::{Schema, Ty, json_schema};
use crate::telemetry::{BudgetExceeded, Ledger};

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
    /// Marks the end of a prompt prefix later calls repeat, so a provider that caches only at marked points (Bedrock,
    /// Anthropic) can cache it. A provider that caches on its own ignores the mark.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cache_through: bool,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Message {
        Message { role, content: content.into(), images: Vec::new(), cache_through: false }
    }
    /// This message, marked as the end of a prefix to cache.
    pub fn cached(self) -> Message {
        Message { cache_through: true, ..self }
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

/// What kind of failure a model call met, named without anything it carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultClass {
    /// The provider answered this HTTP status.
    Http(u16),
    /// No answer came: a connection, timeout or proxy failure.
    Transport,
    /// The answer did not match the schema it was asked for.
    Schema,
    /// The answer was cut off before its JSON ended.
    Truncated,
    /// The answer held no completion the client could read.
    Envelope,
    /// The request could not be made as asked: no model for the purpose, an image type or schema the wire cannot
    /// carry, or a context window that cannot hold the request.
    Request,
    /// An embedder's own client failed and said no more.
    Client,
}

impl std::fmt::Display for FaultClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FaultClass::Http(status) => write!(f, "http-{status}"),
            FaultClass::Transport => f.write_str("transport"),
            FaultClass::Schema => f.write_str("schema"),
            FaultClass::Truncated => f.write_str("truncated"),
            FaultClass::Envelope => f.write_str("envelope"),
            FaultClass::Request => f.write_str("request"),
            FaultClass::Client => f.write_str("client"),
        }
    }
}

/// A failed model call: its message, and the provider, purpose and class of the failure, which hold no content.
///
/// The hosted worker logs only `RunResult::failure_kind`, never the message, which can quote a response; a failure
/// named only `Llm.Failed` said neither which provider failed nor how (#1372). The class is set where the error is
/// made, the provider and purpose by the client that made the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    pub message: String,
    pub class: FaultClass,
    /// The provider `FASTBROWSE_LLM_PROVIDER` names, as `LLMProvider::as_str` spells it.
    pub provider: Option<String>,
    pub purpose: Option<LLMPurpose>,
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl Fault {
    pub fn new(class: FaultClass, message: impl Into<String>) -> Fault {
        Fault { message: message.into(), class, provider: None, purpose: None }
    }

    /// `provider/purpose/class`, with `unknown` for a part no one set.
    pub fn cause(&self) -> String {
        let purpose = self.purpose.map(|purpose| purpose.as_str().to_owned());
        let provider = self.provider.as_deref().unwrap_or("unknown");
        format!("{provider}/{}/{}", purpose.as_deref().unwrap_or("unknown"), self.class)
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LLMError {
    /// The model failed to return data matching the schema, or the transport failed.
    #[error("{}", .0.message)]
    Failed(Fault),
    /// A retryable HTTP status or a transport failure outlasted the provider's retry budget.
    #[error("{}", .0.message)]
    RetriesExhausted(Fault),
    /// The provider refused the key (HTTP 401 or 403), which no retry on that provider can change.
    #[error("{}", .0.message)]
    Rejected(Fault),
    /// The client's reservation, or the receipt it recorded, broke a limit.
    #[error(transparent)]
    Budget(#[from] BudgetExceeded),
    /// The page redrew under the decision that would have sent this request, so it was not sent.
    #[error("the page redrew, so the decision it was for is discarded")]
    Superseded,
}

impl LLMError {
    pub fn failed(class: FaultClass, message: impl Into<String>) -> LLMError {
        LLMError::Failed(Fault::new(class, message))
    }

    /// An answer that did not match its schema, the error every `ResponseSchema::validate_json` failure becomes.
    pub fn schema(message: impl Into<String>) -> LLMError {
        LLMError::failed(FaultClass::Schema, message)
    }

    /// The fault this error carries, if it is one a model call met.
    pub fn fault(&self) -> Option<&Fault> {
        match self {
            LLMError::Failed(fault) | LLMError::RetriesExhausted(fault) | LLMError::Rejected(fault) => Some(fault),
            LLMError::Budget(_) | LLMError::Superseded => None,
        }
    }

    /// The error with the provider and purpose of the call it came from, where the maker did not set them.
    pub fn from_call(mut self, provider: &str, purpose: LLMPurpose) -> LLMError {
        if let LLMError::Failed(fault) | LLMError::RetriesExhausted(fault) | LLMError::Rejected(fault) = &mut self {
            fault.provider.get_or_insert_with(|| provider.to_owned());
            fault.purpose.get_or_insert(purpose);
        }
        self
    }
}

impl FailureKind for LLMError {
    fn failure_kind(&self) -> String {
        match self {
            LLMError::Failed(fault) => format!("Failed/{}", fault.cause()),
            LLMError::RetriesExhausted(fault) => format!("RetriesExhausted/{}", fault.cause()),
            LLMError::Rejected(fault) => format!("Rejected/{}", fault.cause()),
            LLMError::Budget(exceeded) => exceeded.failure_kind(),
            LLMError::Superseded => "Superseded".into(),
        }
    }
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
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>>;

    /// The tokens a prompt may hold for the model that answers `purpose`: its context window less the output it
    /// reserves, where its provider lists them. A prompt is fitted to it; with none known, a prompt goes whole and the
    /// provider is the one to refuse it.
    fn context_tokens(&self, _purpose: LLMPurpose) -> BoxFuture<'_, Option<i64>> {
        Box::pin(async { None })
    }
}

/// `generate` with the response type named, as Python's generic method has it.
pub trait LLMClientExt: LLMClient {
    fn generate<'a, T>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        ledger: Option<&'a Ledger>,
    ) -> impl Future<Output = Result<Generation<T>, LLMError>> + Send + 'a
    where
        T: Schema + DeserializeOwned + Serialize + Send + 'a,
        Self: Sized,
    {
        async move {
            let schema = Typed::<T>::default();
            let generated = self.generate_value(purpose, messages, &schema, ledger).await?;
            let data = serde_json::from_value(generated.data).map_err(|e| {
                LLMError::Failed(Fault { purpose: Some(purpose), ..Fault::new(FaultClass::Schema, e.to_string()) })
            })?;
            Ok(Generation { data, cost: generated.cost })
        }
    }
}

impl<C: LLMClient + ?Sized> LLMClientExt for C {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::py::dump::Dump;

    /// A client that hands back data without validating it against the schema it was given.
    struct Unvalidated;

    impl LLMClient for Unvalidated {
        fn generate_value<'a>(
            &'a self,
            _purpose: LLMPurpose,
            _messages: &'a [Message],
            _schema: &'a dyn ResponseSchema,
            _ledger: Option<&'a Ledger>,
        ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
            let cost = CostLine::new(crate::models::CostComponent::Llm, crate::models::CostBasis::Metered, Some(0.0));
            Box::pin(async move { Ok(Generation { data: serde_json::json!({}), cost }) })
        }
    }

    #[test]
    fn data_that_is_not_the_type_asked_for_names_the_purpose_and_no_provider() {
        // A client validates its answer against this schema, so only one that skipped it reaches here: no provider
        // answered wrong, and none is named.
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let generated = runtime.block_on(Unvalidated.generate::<crate::models::Link>(LLMPurpose::Compose, &[], None));
        let Err(LLMError::Failed(fault)) = generated else { panic!("expected a failed generation") };
        assert_eq!(fault.cause(), "unknown/compose/schema");
    }

    #[test]
    fn a_message_dumps_as_pydantic_does() {
        assert_eq!(Message::user("hi").dump_json(), r#"{"role":"user","content":"hi","images":[]}"#);
    }
}
