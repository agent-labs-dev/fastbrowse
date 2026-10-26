//! A caller's own Python objects in the run: its Jev and LLM clients, its HTTP client, its access check and its
//! secrets, each behind the Rust trait the run calls. `fastbrowse.native` wraps each object into the plain function
//! called here, which takes and returns JSON, so the conversions to the caller's pydantic models stay in Python.
//!
//! A domain error crosses back as data and becomes the Rust error of the same kind. Anything else a client raised is
//! that client failing, as an error the core already handles: a call the run began and then stopped waiting on, such
//! as a plan begun before a browser that failed, must not end the run.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};

use fastbrowse_browser::session::{AccessRefused, CheckAccess};
use fastbrowse_cli::embed::{HostCall, HostResolve};
use fastbrowse_cli::runner::Cancel;
use fastbrowse_clients::http::{Request, Response, Transport, TransportError};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::jev::{Evaluation, JevClient, JevError, Question, RetriesExhausted};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{BudgetStop, CostComponent, CostLine, LLMPurpose, SecretRef, SecretResolver};
use fastbrowse_core::protocol::Base64Bytes;
use fastbrowse_core::telemetry::{BudgetExceeded, Ledger};
use indexmap::IndexMap;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

/// Whether a callback failed, which stops the run where it is.
pub struct Failure {
    pub cancel: Cancel,
    failed: AtomicBool,
}

impl Failure {
    pub fn new(cancel: Cancel) -> Failure {
        Failure { cancel, failed: AtomicBool::new(false) }
    }

    pub fn fail(&self) {
        self.failed.store(true, Ordering::SeqCst);
        self.cancel.trip();
    }

    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }
}

#[derive(Deserialize)]
struct Fault {
    kind: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    budget: Option<BudgetStop>,
    #[serde(default)]
    seconds: f64,
    #[serde(default)]
    unaccounted_requests: i64,
    #[serde(default)]
    requests: i64,
    #[serde(default)]
    answered: Vec<Evaluation>,
    #[serde(default)]
    last: String,
}

impl Fault {
    fn budget(&self) -> Option<BudgetExceeded> {
        let budget = self.budget.clone()?;
        Some(BudgetExceeded { reason: self.message.clone(), budget })
    }
}

/// `{"ok": value}` or `{"error": fault}`, as the wrappers in `fastbrowse.native` answer.
fn reply<T: for<'de> Deserialize<'de>>(text: &str) -> Result<Result<T, Fault>, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "lowercase")]
    enum Reply<T> {
        Ok(T),
        Error(Fault),
    }
    match serde_json::from_str::<Reply<T>>(text) {
        Ok(Reply::Ok(value)) => Ok(Ok(value)),
        Ok(Reply::Error(fault)) => Ok(Err(fault)),
        Err(error) => Err(format!("the client's answer cannot be used: {error}")),
    }
}

/// The caller's `JevClient`.
pub struct HostJev {
    pub call: HostCall<(String, String), String>,
}

impl JevClient for HostJev {
    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, JevError>> {
        Box::pin(async move {
            let questions = serde_json::to_string(questions).map_err(|error| JevError::Failed(error.to_string()))?;
            let answered = match (self.call)((state.to_string(), questions)).await {
                Ok(text) => reply::<Evaluation>(&text),
                Err(message) => Err(message),
            };
            match answered {
                Ok(Ok(evaluation)) => Ok(evaluation),
                Ok(Err(fault)) => Err(jev_error(fault)),
                Err(message) => Err(JevError::Failed(message)),
            }
        })
    }
}

fn jev_error(fault: Fault) -> JevError {
    if let Some(budget) = fault.budget() {
        return JevError::Budget(budget);
    }
    match fault.kind.as_str() {
        "input_too_large" => JevError::InputTooLarge(fault.message),
        "transport_failed" => JevError::TransportFailed(fault.message),
        "retries_exhausted" => JevError::RetriesExhausted(Box::new(RetriesExhausted {
            message: fault.message,
            seconds: fault.seconds,
            unaccounted_requests: fault.unaccounted_requests,
            requests: fault.requests,
            answered: fault.answered,
            last: fault.last,
        })),
        _ => JevError::Failed(fault.message),
    }
}

/// What an LLM call hands the caller's client besides its arguments.
pub struct LlmCall {
    pub purpose: String,
    pub messages: String,
    pub name: String,
    pub schema: String,
    pub max_output_tokens: i64,
    pub context: Context,
}

/// The caller's `LLMClient`.
pub struct HostLlm {
    pub call: HostCall<LlmCall, String>,
}

#[derive(Deserialize)]
struct Generated {
    data: Value,
    cost: CostLine,
}

type Ask = (Value, mpsc::Sender<Value>);

/// The run's ledger and the response schema, for the length of one LLM call. The client reaches them from the event
/// loop's thread through `op`, and the call that holds them answers from the run's, since both are only borrowed.
#[pyclass(frozen)]
pub struct Context {
    asks: Arc<Mutex<Option<UnboundedSender<Ask>>>>,
    #[pyo3(get)]
    ledger: bool,
}

#[pymethods]
impl Context {
    /// One request as JSON, answered as JSON. Raises once the call it belongs to has ended.
    fn op(&self, py: Python<'_>, request: &str) -> PyResult<String> {
        let request: Value =
            serde_json::from_str(request).map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
        let (answer, answered) = mpsc::channel();
        let sent = match &*self.asks.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
            Some(asks) => asks.send((request, answer)).is_ok(),
            None => false,
        };
        let ended = || PyRuntimeError::new_err("the LLM call this ledger belongs to has ended");
        if !sent {
            return Err(ended());
        }
        py.detach(move || answered.recv()).map(|value| value.to_string()).map_err(|_| ended())
    }
}

/// Closes a [`Context`] when the call that answers it ends, however it ends.
struct Closing(Arc<Mutex<Option<UnboundedSender<Ask>>>>);

impl Drop for Closing {
    fn drop(&mut self) {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
    }
}

fn budget_reply(outcome: Result<(), BudgetExceeded>) -> Value {
    match outcome {
        Ok(()) => Value::Null,
        Err(error) => json!({"budget": {"reason": error.reason, "budget": error.budget}}),
    }
}

fn answer(ask: &Value, ledger: Option<&Ledger>, schema: &dyn ResponseSchema) -> Value {
    let number = |key: &str| ask.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    let op = ask.get("op").and_then(Value::as_str).unwrap_or_default();
    if op == "validate" {
        let text = ask.get("text").and_then(Value::as_str).unwrap_or_default();
        return match schema.validate_json(text) {
            Ok(value) => json!({"ok": value}),
            Err(message) => json!({"error": message}),
        };
    }
    let Some(ledger) = ledger else { return json!({"error": "this call has no ledger"}) };
    match op {
        "reserve" => {
            let component = ask.get("component").and_then(Value::as_str).and_then(CostComponent::parse);
            let Some(component) = component else { return json!({"error": "no such cost component"}) };
            let calls = ask.get("calls").and_then(Value::as_i64).unwrap_or(1);
            budget_reply(ledger.reserve(component, number("estimate_dollars"), calls))
        }
        "check" => budget_reply(ledger.check(number("extra_dollars"))),
        "check_spend" => budget_reply(ledger.check_spend(number("extra_dollars"))),
        "record" => match serde_json::from_value::<Vec<CostLine>>(ask.get("lines").cloned().unwrap_or_default()) {
            Ok(lines) => budget_reply(ledger.record(lines)),
            Err(error) => json!({"error": error.to_string()}),
        },
        "capped" => json!(ledger.capped()),
        "limits" => json!(ledger.limits),
        "lines" => json!(ledger.state().lines),
        "breakdown" => json!(ledger.breakdown()),
        _ => json!({"error": format!("no such ledger operation: {op}")}),
    }
}

fn messages_json(messages: &[Message]) -> String {
    let messages: Vec<Value> = messages
        .iter()
        .map(|message| {
            // JPEG bytes have no UTF-8 form, so images cross as base64 and the wrapper decodes them.
            let images: Vec<Value> = message.images.iter().map(|image| json!(Base64Bytes(image.0.clone()))).collect();
            json!({"role": message.role, "content": message.content, "images": images})
        })
        .collect();
    Value::Array(messages).to_string()
}

impl LLMClient for HostLlm {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        max_output_tokens: i64,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        Box::pin(async move {
            let (asks, mut asked) = unbounded_channel::<Ask>();
            let slot = Arc::new(Mutex::new(Some(asks)));
            let _closing = Closing(slot.clone());
            let call = (self.call)(LlmCall {
                purpose: purpose.as_str().to_string(),
                messages: messages_json(messages),
                name: schema.name(),
                schema: schema.json_schema().to_string(),
                max_output_tokens,
                context: Context { asks: slot, ledger: ledger.is_some() },
            });
            tokio::pin!(call);
            let answered = loop {
                tokio::select! {
                    done = &mut call => break done,
                    Some((ask, back)) = asked.recv() => {
                        let _ = back.send(answer(&ask, ledger, schema));
                    }
                }
            };
            let answered = match answered {
                Ok(text) => reply::<Generated>(&text),
                Err(message) => Err(message),
            };
            match answered {
                Ok(Ok(generated)) => {
                    let data = schema.validate_json(&generated.data.to_string()).map_err(LLMError::Failed)?;
                    Ok(Generation { data, cost: generated.cost })
                }
                Ok(Err(fault)) => Err(match fault.budget() {
                    Some(budget) => LLMError::Budget(budget),
                    None if fault.kind == "retries_exhausted" => LLMError::RetriesExhausted(fault.message),
                    None => LLMError::Failed(fault.message),
                }),
                Err(message) => Err(LLMError::Failed(message)),
            }
        })
    }
}

/// What the caller's HTTP client answered: a response, or the httpx exception it raised, by kind and type name.
pub enum Sent {
    Response(Response),
    Failed(String, String),
}

/// The caller's `httpx.AsyncClient`, for every request the run's clients make.
pub struct HostTransport {
    pub call: HostCall<Request, Sent>,
}

/// An exception's type name as the `'static` name a `TransportError` carries. A client raises a few kinds, so each
/// name is kept once for the life of the process.
fn interned(name: String) -> &'static str {
    static NAMES: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut names = NAMES.get_or_init(Mutex::default).lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(known) = names.get(name.as_str()) {
        return known;
    }
    let leaked: &'static str = Box::leak(name.into_boxed_str());
    names.insert(leaked);
    leaked
}

impl Transport for HostTransport {
    fn send(&self, request: Request) -> BoxFuture<'static, Result<Response, TransportError>> {
        let call = self.call.clone();
        Box::pin(async move {
            match call(request).await {
                Ok(Sent::Response(response)) => Ok(response),
                Ok(Sent::Failed(kind, name)) => {
                    let name = interned(name);
                    Err(match kind.as_str() {
                        "timeout" => TransportError::Timeout(name),
                        "transient" => TransportError::Transient(name),
                        _ => TransportError::Fatal(name),
                    })
                }
                Err(_) => Err(TransportError::Fatal("Exception")),
            }
        })
    }
}

/// The caller's `check_access`. Its wrapper answers None, or the type name of what it raised.
pub fn check_access(call: HostCall<(), Option<String>>) -> CheckAccess {
    Arc::new(move || {
        let call = call.clone();
        Box::pin(async move {
            match call(()).await {
                Ok(None) => Ok(()),
                Ok(Some(kind)) => Err(Box::new(AccessRefused(kind)) as _),
                Err(_) => Err(Box::new(AccessRefused("Exception".into())) as _),
            }
        })
    })
}

/// The caller's `SecretResolver`: the refs it made available, and its `resolve`.
pub struct HostSecrets {
    pub refs: Vec<SecretRef>,
    pub resolve: HostResolve,
    pub failure: Arc<Failure>,
}

impl SecretResolver for HostSecrets {
    fn available(&self) -> Vec<SecretRef> {
        self.refs.clone()
    }

    fn resolve<'a>(&'a self, name: &'a str, origin: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            (self.resolve)(name.to_string(), origin.to_string()).await.unwrap_or_else(|_| {
                self.failure.fail();
                None
            })
        })
    }
}
