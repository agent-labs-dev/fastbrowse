//! Replays the fixed recordings in `tests/golden`: each request a function makes is compared byte for byte with the one
//! the Python implementation the core replaced made at that point, and answered with what its fake answered.

#![allow(dead_code)]

pub mod capture;

use std::fs;
use std::future::Future;
use std::path::Path;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use fastbrowse_agent::AgentError;
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{
    DecisionClient, DecisionError, DecisionProfile, Evaluation, Question, RetriesExhausted,
};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{CostComponent, CostLine, LLMPurpose, Limits};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::json::{Style, dumps, write};
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub fn golden(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(name);
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).expect("golden JSON")
}

pub fn cases(name: &str) -> Vec<Value> {
    match golden(name) {
        Value::Array(cases) => cases,
        other => panic!("expected a list of cases, got {other}"),
    }
}

pub fn parse<T: DeserializeOwned>(value: &Value) -> T {
    serde_json::from_value(value.clone()).unwrap_or_else(|e| panic!("{e}: {value}"))
}

/// Runs a future whose fakes answer on their first poll.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
    }
}

/// The recorded calls of one kind, in the order Python made them.
struct Script {
    name: String,
    calls: Vec<Value>,
    next: Mutex<usize>,
}

impl Script {
    fn new(case: &Value, kind: &str) -> Script {
        let calls = case["calls"]
            .as_array()
            .map_or_else(Vec::new, |calls| calls.iter().filter(|call| call.get(kind).is_some()).cloned().collect());
        Script { name: case["name"].as_str().unwrap_or("?").to_string(), calls, next: Mutex::new(0) }
    }

    fn take(&self) -> (usize, Value) {
        let mut next = self.next.lock().unwrap();
        let index = *next;
        let call =
            self.calls.get(index).unwrap_or_else(|| panic!("{}: a request Python never made (#{index})", self.name));
        *next += 1;
        (index, call.clone())
    }

    fn finish(&self) {
        let made = *self.next.lock().unwrap();
        assert_eq!(made, self.calls.len(), "{}: Python made {} requests, Rust {made}", self.name, self.calls.len());
    }
}

/// Replays recorded Jev answers. Under `FASTBROWSE_UPDATE_GOLDEN=1` it answers from the recording without comparing
/// the requests, and keeps each request as written now in `asked` for the test to record.
pub struct ReplayJev(Script, DecisionProfile, pub Mutex<Vec<(String, String)>>);

/// Jev's profile, with the fewer options per choice a case sets to reach group questions with few controls.
pub fn case_profile(case: &Value) -> DecisionProfile {
    let mut profile = DecisionProfile::jev();
    if let Some(options) = case["max_choice_options"].as_u64() {
        profile.max_choice_options = options as usize;
    }
    profile
}

impl ReplayJev {
    pub fn new(case: &Value) -> ReplayJev {
        ReplayJev(Script::new(case, "jev"), case_profile(case), Mutex::new(Vec::new()))
    }

    pub fn finish(&self) {
        self.0.finish();
    }
}

impl DecisionClient for ReplayJev {
    fn profile(&self) -> DecisionProfile {
        self.1.clone()
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        let (index, call) = self.0.take();
        let name = &self.0.name;
        let written: serde_json::Map<String, Value> =
            questions.iter().map(|(key, question)| (key.clone(), question.dump())).collect();
        let (state, questions) = (write(state, Style::HTTPX), write(&Value::Object(written), Style::HTTPX));
        if std::env::var_os("FASTBROWSE_UPDATE_GOLDEN").is_some() {
            self.2.lock().unwrap().push((state, questions));
        } else {
            assert_eq!(state, call["jev"]["state"].as_str().unwrap(), "{name}: Jev state #{index}");
            assert_eq!(questions, call["jev"]["questions"].as_str().unwrap(), "{name}: Jev questions #{index}");
        }
        let response = call["response"].clone();
        Box::pin(async move { jev_response(&response) })
    }
}

fn jev_response(response: &Value) -> Result<Evaluation, DecisionError> {
    if let Some(evaluation) = response.get("evaluation") {
        return Ok(parse(evaluation));
    }
    let message = response["message"].as_str().unwrap_or_default().to_string();
    Err(match response["error"].as_str() {
        Some("input_too_large") => DecisionError::InputTooLarge(message),
        Some("retries_exhausted") => DecisionError::RetriesExhausted(Box::new(RetriesExhausted {
            message,
            seconds: response["seconds"].as_f64().unwrap_or_default(),
            unaccounted_requests: response["unaccounted_requests"].as_i64().unwrap_or_default(),
            requests: response["requests"].as_i64().unwrap_or_default(),
            answered: Vec::new(),
            last: String::new(),
        })),
        _ => DecisionError::Failed(message),
    })
}

pub struct ReplayLLM(Script);

impl ReplayLLM {
    pub fn new(case: &Value) -> ReplayLLM {
        ReplayLLM(Script::new(case, "llm"))
    }

    pub fn finish(&self) {
        self.0.finish();
    }
}

impl LLMClient for ReplayLLM {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        Box::pin(async move {
            // Reserved as the real client reserves, so a budget stop is seen where Python saw it.
            if let Some(ledger) = ledger {
                ledger.reserve(CostComponent::Llm, 0.0, 1)?;
            }
            let (index, call) = self.0.take();
            let name = &self.0.name;
            let request = &call["llm"];
            assert_eq!(purpose.as_str(), request["purpose"], "{name}: LLM purpose #{index}");
            let written = Value::Array(messages.iter().map(Dump::dump).collect());
            assert_eq!(
                write(&written, Style::HTTPX),
                request["messages"].as_str().unwrap(),
                "{name}: messages #{index}"
            );
            assert_eq!(schema.name(), request["schema_name"].as_str().unwrap(), "{name}: schema name #{index}");
            assert_eq!(dumps(&schema.json_schema()), request["schema"].as_str().unwrap(), "{name}: schema #{index}");
            let data = schema.validate_json(&dumps(&call["response"]["data"])).map_err(LLMError::Failed)?;
            let cost: CostLine = parse(&call["response"]["cost"]);
            Ok(Generation { data, cost })
        })
    }
}

pub fn ledger(limits: &Value) -> Option<Ledger> {
    (!limits.is_null()).then(|| Ledger::new(parse::<Limits>(limits)))
}

pub fn ledger_state(ledger: Option<&Ledger>) -> Value {
    match ledger {
        None => Value::Null,
        Some(ledger) => {
            let state = ledger.state();
            json!({
                "lines": state.lines.iter().map(Dump::dump).collect::<Vec<_>>(),
                "jev_calls": state.jev_calls,
                "llm_calls": state.llm_calls,
            })
        }
    }
}

pub fn error_outcome(error: &AgentError) -> Value {
    let kind = match error {
        AgentError::Budget(_) => "budget",
        AgentError::Jev(DecisionError::RetriesExhausted(_)) => "jev_retries_exhausted",
        AgentError::Jev(DecisionError::InputTooLarge(_)) => "jev_input_too_large",
        AgentError::Jev(_) => "jev",
        AgentError::Llm(_) => "llm",
        AgentError::ObservationTooLarge(_) => "ObservationTooLarge",
        AgentError::NotesTooLarge(_) => "NotesTooLarge",
        AgentError::Invalid(_) => "ValueError",
    };
    json!({"error": kind, "message": error.to_string()})
}

/// Compares what Rust returned with what Python returned, naming the case.
pub fn assert_outcome(case: &Value, actual: Value) {
    assert_eq!(actual, case["outcome"], "{}: outcome", case["name"]);
}
