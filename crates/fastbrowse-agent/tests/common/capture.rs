//! Replays what `tests/golden/golden_capture.py` recorded while Python's own tests ran: each case is one outermost call
//! of a retrieval or verification function, with the arguments it was given, every model request it made and the
//! answer it got, and what it returned and left behind.
//!
//! Requests are matched by their bytes, not by their order: Python runs audits side by side, and a request sent in
//! another order by Rust is still the same request. Two requests with the same body take their answers in the order
//! Python got them.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

use fastbrowse_agent::memory::{Notes, NotesState};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::jev::{Evaluation, JevClient, JevError, Question, RetriesExhausted};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{BudgetResource, BudgetStop, CostLine, LLMPurpose, Limits};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::json::{Style, dumps, write};
use fastbrowse_core::telemetry::{BudgetExceeded, Ledger};
use indexmap::IndexMap;
use serde_json::{Value, json};

use super::parse;

/// Every case in a capture file, one JSON object per line inside a list.
pub fn capture_cases(name: &str) -> Vec<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(name);
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    match serde_json::from_str(&text).expect("capture JSON") {
        Value::Array(cases) => cases,
        other => panic!("expected a list of cases, got {other}"),
    }
}

/// Why a case cannot be replayed in Rust, or None.
pub fn skip_reason(case: &Value) -> Option<String> {
    if let Some(reason) = case.get("skip").and_then(Value::as_str) {
        return Some(reason.to_string());
    }
    if case.get("unsupported").and_then(Value::as_bool) == Some(true) {
        return Some("an argument Rust cannot rebuild".into());
    }
    None
}

pub fn notes_arg(value: &Value) -> Notes {
    let state: NotesState = parse(&value["$notes"]);
    Notes::from_state(state)
}

/// A notes state as JSON, owners sorted as Rust keeps them.
pub fn notes_json(notes: &Notes) -> Value {
    serde_json::to_value(notes.state()).expect("notes state")
}

fn sorted_owners(mut state: Value) -> Value {
    if let Some(Value::Array(owners)) = state.get_mut("owners") {
        owners.sort_by_key(|pair| pair[0].as_str().unwrap_or_default().to_string());
    }
    state
}

/// Compares the notes a call left with what Python's left; `$same` means the call did not change them.
pub fn assert_notes(case: &Value, param: &str, notes: &Notes) {
    let expected = &case["notes_after"][param];
    let wanted = if expected.get("$same").is_some() { case["args"][param]["$notes"].clone() } else { expected.clone() };
    assert_eq!(notes_json(notes), sorted_owners(wanted), "{}: notes {param}", case["name"]);
}

pub fn ledger_arg(value: &Value) -> Ledger {
    let state = &value["$ledger"];
    let ledger = Ledger::new(parse::<Limits>(&state["limits"]));
    {
        let mut held = ledger.state();
        held.lines = parse(&state["lines"]);
        held.steps = state["steps"].as_i64().unwrap_or(0);
        held.jev_calls = state["jev_calls"].as_i64().unwrap_or(0);
        held.llm_calls = state["llm_calls"].as_i64().unwrap_or(0);
    }
    ledger
}

pub fn ledger_json(ledger: &Ledger) -> Value {
    let state = ledger.state();
    json!({
        "limits": ledger.limits.dump(),
        "lines": state.lines.iter().map(Dump::dump).collect::<Vec<_>>(),
        "steps": state.steps,
        "jev_calls": state.jev_calls,
        "llm_calls": state.llm_calls,
    })
}

pub fn assert_ledger(case: &Value, param: &str, ledger: &Ledger) {
    let expected = &case["ledger_after"][param];
    let expected = expected.get("$ledger").unwrap_or(expected);
    assert_eq!(ledger_json(ledger), *expected, "{}: ledger {param}", case["name"]);
}

/// Trace events compared as a multiset: concurrent calls record in whichever order they finish.
pub fn assert_trace(case: &Value, events: &[Value]) {
    let sort = |events: &[Value]| {
        let mut written: Vec<String> = events.iter().map(dumps).collect();
        written.sort();
        written
    };
    let expected: Vec<Value> = case["trace"].as_array().cloned().unwrap_or_default();
    assert_eq!(sort(events), sort(&expected), "{}: trace", case["name"]);
}

struct Answers {
    name: String,
    by_body: Mutex<HashMap<String, VecDeque<Value>>>,
    bodies: Vec<String>,
}

impl Answers {
    fn new(case: &Value, kind: &str, body: impl Fn(&Value) -> String) -> Answers {
        let mut by_body: HashMap<String, VecDeque<Value>> = HashMap::new();
        let mut bodies = Vec::new();
        for call in case["calls"].as_array().into_iter().flatten().filter(|call| call.get(kind).is_some()) {
            let key = body(&call[kind]);
            bodies.push(key.clone());
            by_body.entry(key).or_default().push_back(call.clone());
        }
        Answers { name: case["name"].as_str().unwrap_or("?").to_string(), by_body: Mutex::new(by_body), bodies }
    }

    fn take(&self, key: &str) -> Value {
        let mut by_body = self.by_body.lock().unwrap();
        if let Some(call) = by_body.get_mut(key).and_then(VecDeque::pop_front) {
            return call;
        }
        let nearest = self.bodies.iter().max_by_key(|body| common_prefix(body, key)).cloned().unwrap_or_default();
        let at = common_prefix(&nearest, key);
        let around = |text: &str| text.chars().skip(at.saturating_sub(80)).take(240).collect::<String>();
        panic!(
            "{}: a request Python never made ({} recorded)\n  first difference at {at}\n  rust:   {}\n  python: {}",
            self.name,
            self.bodies.len(),
            around(key),
            around(&nearest)
        );
    }

    fn finish(&self) {
        let left: usize = self.by_body.lock().unwrap().values().map(VecDeque::len).sum();
        assert_eq!(left, 0, "{}: Python made {left} requests Rust did not", self.name);
    }
}

fn common_prefix(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

/// What a test's fake did to the ledgers it was handed, applied again here.
fn apply_effect(call: &Value, ledgers: &[&Ledger]) {
    for (effect, ledger) in call["ledger_effect"].as_array().into_iter().flatten().zip(ledgers) {
        let lines: Vec<CostLine> = parse(&effect["lines"]);
        let mut state = ledger.state();
        state.lines.extend(lines);
        state.jev_calls += effect["jev_calls"].as_i64().unwrap_or(0);
        state.llm_calls += effect["llm_calls"].as_i64().unwrap_or(0);
    }
}

fn jev_body(state: &str, questions: &str) -> String {
    format!("{state}\n{questions}")
}

pub struct KeyedJev<'a> {
    answers: Answers,
    ledgers: Vec<&'a Ledger>,
}

impl<'a> KeyedJev<'a> {
    pub fn new(case: &Value, ledgers: Vec<&'a Ledger>) -> KeyedJev<'a> {
        let answers = Answers::new(case, "jev", |request| {
            jev_body(request["state"].as_str().unwrap_or_default(), request["questions"].as_str().unwrap_or_default())
        });
        KeyedJev { answers, ledgers }
    }

    pub fn finish(&self) {
        self.answers.finish();
    }
}

impl JevClient for KeyedJev<'_> {
    fn evaluate<'b>(
        &'b self,
        state: &'b Value,
        questions: &'b IndexMap<String, Question>,
    ) -> BoxFuture<'b, Result<Evaluation, JevError>> {
        let written: serde_json::Map<String, Value> =
            questions.iter().map(|(key, question)| (key.clone(), question.dump())).collect();
        let key = jev_body(&write(state, Style::HTTPX), &write(&Value::Object(written), Style::HTTPX));
        let call = self.answers.take(&key);
        apply_effect(&call, &self.ledgers);
        let response = call["response"].clone();
        Box::pin(async move { jev_response(&response) })
    }
}

fn jev_response(response: &Value) -> Result<Evaluation, JevError> {
    if let Some(evaluation) = response.get("evaluation") {
        return Ok(parse(evaluation));
    }
    let message = response["message"].as_str().unwrap_or_default().to_string();
    Err(match response["error"].as_str() {
        Some("input_too_large") => JevError::InputTooLarge(message),
        Some("retries_exhausted") => JevError::RetriesExhausted(Box::new(RetriesExhausted {
            message,
            seconds: response["seconds"].as_f64().unwrap_or_default(),
            unaccounted_requests: response["unaccounted_requests"].as_i64().unwrap_or_default(),
            requests: response["requests"].as_i64().unwrap_or_default(),
            answered: Vec::new(),
            last: String::new(),
        })),
        _ => JevError::Failed(message),
    })
}

fn llm_body(purpose: &str, messages: &str, name: &str, schema: &str, cap: i64) -> String {
    format!("{purpose}\n{messages}\n{name}\n{schema}\n{cap}")
}

pub struct KeyedLLM<'a> {
    answers: Answers,
    ledgers: Vec<&'a Ledger>,
}

impl<'a> KeyedLLM<'a> {
    pub fn new(case: &Value, ledgers: Vec<&'a Ledger>) -> KeyedLLM<'a> {
        let answers = Answers::new(case, "llm", |request| {
            llm_body(
                request["purpose"].as_str().unwrap_or_default(),
                request["messages"].as_str().unwrap_or_default(),
                request["schema_name"].as_str().unwrap_or_default(),
                request["schema"].as_str().unwrap_or_default(),
                request["max_output_tokens"].as_i64().unwrap_or_default(),
            )
        });
        KeyedLLM { answers, ledgers }
    }

    pub fn finish(&self) {
        self.answers.finish();
    }
}

impl LLMClient for KeyedLLM<'_> {
    fn generate_value<'b>(
        &'b self,
        purpose: LLMPurpose,
        messages: &'b [Message],
        schema: &'b dyn ResponseSchema,
        max_output_tokens: i64,
        _ledger: Option<&'b Ledger>,
    ) -> BoxFuture<'b, Result<Generation<Value>, LLMError>> {
        let written = Value::Array(messages.iter().map(Dump::dump).collect());
        let key = llm_body(
            purpose.as_str(),
            &write(&written, Style::HTTPX),
            &schema.name(),
            &dumps(&schema.json_schema()),
            max_output_tokens,
        );
        let call = self.answers.take(&key);
        apply_effect(&call, &self.ledgers);
        Box::pin(async move {
            let response = &call["response"];
            let message = response["message"].as_str().unwrap_or_default().to_string();
            match response["error"].as_str() {
                Some("llm_retries_exhausted") => return Err(LLMError::RetriesExhausted(message)),
                Some("budget") => return Err(LLMError::Budget(budget(&message))),
                Some(_) => return Err(LLMError::Failed(message)),
                None => {}
            }
            let data = schema.validate_json(&dumps(&response["data"])).map_err(LLMError::Failed)?;
            Ok(Generation { data, cost: parse(&response["cost"]) })
        })
    }
}

fn budget(message: &str) -> BudgetExceeded {
    BudgetExceeded { reason: message.to_string(), budget: BudgetStop { resource: BudgetResource::Dollars, limit: 0.0 } }
}
