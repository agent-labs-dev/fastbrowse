//! Replays fixed recordings of the Python implementation the core replaced, taken while its tests ran: each case is one
//! outermost call
//! of a retrieval or verification function, with the arguments it was given, every model request it made and the
//! answer it got, and what it returned and left behind.
//!
//! Requests are matched by their bytes, not by their order: Python runs audits side by side, and a request sent in
//! another order by Rust is still the same request. Two requests with the same body take their answers in the order
//! Python got them.
//!
//! `FASTBROWSE_UPDATE_GOLDEN=1` re-records the cases after an intended behaviour change: recorded answers are kept, a
//! new Jev request reuses this case's earlier answer to each question key (a key never answered gets no and a choice
//! of synthesis, none or the first option, at no confidence), and the diff of the file is the behaviour change.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use fastbrowse_agent::memory::{Notes, NotesState};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{DecisionClient, DecisionError, Evaluation, Question, RetriesExhausted};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{BudgetResource, BudgetStop, CostLine, LLMPurpose, Limits};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::json::{Style, dumps, write};
use fastbrowse_core::telemetry::{BudgetExceeded, Ledger, Trace};
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

/// Whether this run re-records the cases instead of checking them.
pub fn updating() -> bool {
    std::env::var_os("FASTBROWSE_UPDATE_GOLDEN").is_some()
}

/// Writes cases back as `capture_cases` reads them, one compact object per line inside a list. A case that did not
/// change keeps its recorded line, so the diff holds only what did.
pub fn write_cases(name: &str, cases: &[Value]) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(name);
    let recorded = fs::read_to_string(&path).unwrap_or_default();
    let mut before: HashMap<String, VecDeque<&str>> = HashMap::new();
    for line in recorded.lines().map(|line| line.trim_end_matches(',')) {
        if let Ok(case) = serde_json::from_str::<Value>(line) {
            before.entry(case.to_string()).or_default().push_back(line);
        }
    }
    let lines: Vec<String> = cases
        .iter()
        .map(|case| match before.get_mut(&case.to_string()).and_then(VecDeque::pop_front) {
            Some(line) => line.to_string(),
            None => serde_json::to_string(case).expect("case JSON"),
        })
        .collect();
    fs::write(&path, format!("[\n{}\n]\n", lines.join(",\n"))).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
}

/// The calls a case made while re-recording, Jev and LLM requests in the order Rust sent them.
pub type CallLog = Arc<Mutex<Vec<Value>>>;

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

/// `case` with what Rust returned, left behind, traced and asked.
pub fn rerecorded(
    case: &Value,
    result: Value,
    notes: Option<&Notes>,
    ledger: Option<(&str, &Ledger)>,
    trace: &Trace,
    log: CallLog,
) -> Value {
    let mut case = case.clone();
    case["outcome"] = result;
    case["calls"] = Value::Array(log.lock().unwrap().clone());
    let events = trace.events();
    let order = |events: &[Value]| {
        let mut written: Vec<String> = events.iter().map(Value::to_string).collect();
        written.sort();
        written
    };
    if order(&events) != order(case["trace"].as_array().map(Vec::as_slice).unwrap_or_default()) {
        case["trace"] = Value::Array(events);
    }
    if let Some(notes) = notes
        && let Some(expected) = case["notes_after"].get("notes").cloned()
    {
        let before = sorted_owners(case["args"]["notes"]["$notes"].clone());
        let wanted = if expected.get("$same").is_some() { before.clone() } else { sorted_owners(expected) };
        let actual = notes_json(notes);
        if actual != wanted {
            case["notes_after"]["notes"] = if actual == before { json!({"$same": true}) } else { actual };
        }
    }
    if let Some((key, ledger)) = ledger
        && !case["ledger_after"][key].is_null()
    {
        let written = ledger_json(ledger);
        let recorded = &case["ledger_after"][key];
        let wrapped = recorded.get("$ledger").is_some();
        // Calls sent side by side bill in whichever order they finish, so the lines are compared as a multiset.
        if unordered_lines(written.clone()) != unordered_lines(recorded.get("$ledger").unwrap_or(recorded).clone()) {
            case["ledger_after"][key] = if wrapped { json!({"$ledger": written}) } else { written };
        }
    }
    case
}

pub fn unordered_lines(mut ledger: Value) -> Value {
    if let Some(Value::Array(lines)) = ledger.get_mut("lines") {
        lines.sort_by_key(|line| line.to_string());
    }
    ledger
}

/// Replays every case in `files`, or re-records them under `FASTBROWSE_UPDATE_GOLDEN=1`, and fails on any that
/// differ. `CASE` narrows the run to the cases whose name holds it.
pub fn replay_files(
    label: &str,
    files: &[&str],
    skip: impl Fn(&Value) -> Option<String>,
    replay: impl Fn(&Value, bool) -> Result<Option<Value>, String>,
) {
    let update = updating();
    let only = std::env::var("CASE").ok();
    let (mut failures, mut skipped, mut passed, mut total) = (Vec::new(), Vec::new(), 0, 0);
    for file in files {
        let mut written = Vec::new();
        for case in capture_cases(file) {
            total += 1;
            let name = case["name"].as_str().unwrap_or("?").to_string();
            if only.as_deref().is_some_and(|only| !name.contains(only)) {
                written.push(case);
                continue;
            }
            if let Some(reason) = skip_reason(&case).or_else(|| skip(&case)) {
                skipped.push(format!("{name}: {reason}"));
                written.push(case);
                continue;
            }
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| replay(&case, update))) {
                Ok(Ok(updated)) => {
                    passed += 1;
                    written.push(updated.unwrap_or(case));
                }
                Ok(Err(problem)) => {
                    failures.push(format!("{name}\n{problem}"));
                    written.push(case);
                }
                Err(panic) => {
                    failures.push(format!("{name}\npanicked: {}", panic_text(&panic)));
                    written.push(case);
                }
            }
        }
        if update {
            write_cases(file, &written);
        }
    }
    eprintln!("{label} goldens: {passed} passed, {} failed, {} skipped", failures.len(), skipped.len());
    for skip in &skipped {
        eprintln!("skipped {skip}");
    }
    let shown: Vec<&String> = failures.iter().take(8).collect();
    assert!(
        failures.is_empty(),
        "{} of {total} cases differ; first:\n\n{}",
        failures.len(),
        shown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n\n")
    );
}

pub fn panic_text(panic: &Box<dyn std::any::Any + Send>) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "panic".into())
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
    kind: &'static str,
    by_body: Mutex<HashMap<String, VecDeque<Value>>>,
    bodies: Vec<String>,
    recorded: Vec<Value>,
    log: Option<CallLog>,
}

impl Answers {
    fn new(case: &Value, kind: &'static str, log: Option<CallLog>, body: impl Fn(&Value) -> String) -> Answers {
        let mut by_body: HashMap<String, VecDeque<Value>> = HashMap::new();
        let mut bodies = Vec::new();
        let mut recorded = Vec::new();
        for call in case["calls"].as_array().into_iter().flatten().filter(|call| call.get(kind).is_some()) {
            let key = body(&call[kind]);
            bodies.push(key.clone());
            recorded.push(call.clone());
            by_body.entry(key).or_default().push_back(call.clone());
        }
        let name = case["name"].as_str().unwrap_or("?").to_string();
        Answers { name, kind, by_body: Mutex::new(by_body), bodies, recorded, log }
    }

    /// The recorded call for `key`; re-recording, `fresh` answers one never recorded and every call is logged with
    /// `request` as Rust wrote it.
    fn take(&self, key: &str, request: Value, fresh: impl FnOnce(&[Value]) -> Option<Value>) -> Value {
        let mut by_body = self.by_body.lock().unwrap();
        let found = by_body.get_mut(key).and_then(VecDeque::pop_front);
        if let Some(log) = &self.log {
            let mut call = found.or_else(|| fresh(&self.recorded)).unwrap_or_else(|| self.missing(key));
            let mut logged = serde_json::Map::new();
            logged.insert(self.kind.into(), request);
            for (field, value) in call.as_object_mut().expect("a call").iter() {
                if field != self.kind {
                    logged.insert(field.clone(), value.clone());
                }
            }
            call = Value::Object(logged);
            log.lock().unwrap().push(call.clone());
            return call;
        }
        found.unwrap_or_else(|| self.missing(key))
    }

    fn missing(&self, key: &str) -> ! {
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
        if self.log.is_some() {
            return;
        }
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
    pub fn new(case: &Value, ledgers: Vec<&'a Ledger>, log: Option<CallLog>) -> KeyedJev<'a> {
        let answers = Answers::new(case, "jev", log, |request| {
            jev_body(request["state"].as_str().unwrap_or_default(), request["questions"].as_str().unwrap_or_default())
        });
        KeyedJev { answers, ledgers }
    }

    pub fn finish(&self) {
        self.answers.finish();
    }
}

impl DecisionClient for KeyedJev<'_> {
    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        fastbrowse_core::decision::DecisionProfile::jev()
    }

    fn evaluate<'b>(
        &'b self,
        state: &'b Value,
        questions: &'b IndexMap<String, Question>,
    ) -> BoxFuture<'b, Result<Evaluation, DecisionError>> {
        let written: serde_json::Map<String, Value> =
            questions.iter().map(|(key, question)| (key.clone(), question.dump())).collect();
        let (state, written) = (write(state, Style::HTTPX), write(&Value::Object(written), Style::HTTPX));
        let key = jev_body(&state, &written);
        let request = json!({"state": state, "questions": written});
        let call = self.answers.take(&key, request, |recorded| Some(fresh_jev(questions, recorded)));
        apply_effect(&call, &self.ledgers);
        let response = call["response"].clone();
        Box::pin(async move { jev_response(&response) })
    }
}

/// A call answering `questions` with this case's earlier answer to each key, or no at no confidence.
fn fresh_jev(questions: &IndexMap<String, Question>, recorded: &[Value]) -> Value {
    let earlier =
        |key: &str| recorded.iter().rev().find_map(|call| call["response"]["evaluation"]["answers"].get(key).cloned());
    let answers: serde_json::Map<String, Value> = questions
        .iter()
        .map(|(key, question)| {
            let answer = earlier(key).unwrap_or_else(|| match question {
                Question::Choice(choice) => {
                    let pick = ["synthesis", "none"]
                        .into_iter()
                        .find(|option| choice.criteria.contains_key(*option))
                        .map(str::to_string)
                        .or_else(|| choice.criteria.keys().next().cloned())
                        .unwrap_or_default();
                    json!({"type": "choice", "choice": pick, "probabilities": {pick.clone(): 0.0}, "confidence": 0.0})
                }
                _ => json!({"type": "noul", "probability": 0.0}),
            });
            (key.clone(), answer)
        })
        .collect();
    let cost = json!({"component": "jev", "basis": "estimated", "dollars": 0.0001, "purpose": null,
                      "input_tokens": 0, "output_tokens": 0, "seconds": null});
    json!({"response": {"evaluation": {"model": "test", "answers": answers, "input_tokens": 10, "cost": cost,
                                       "requests": 1}}})
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

fn llm_body(purpose: &str, messages: &str, name: &str, schema: &str, cap: i64) -> String {
    format!("{purpose}\n{messages}\n{name}\n{schema}\n{cap}")
}

pub struct KeyedLLM<'a> {
    answers: Answers,
    ledgers: Vec<&'a Ledger>,
}

impl<'a> KeyedLLM<'a> {
    pub fn new(case: &Value, ledgers: Vec<&'a Ledger>, log: Option<CallLog>) -> KeyedLLM<'a> {
        let answers = Answers::new(case, "llm", log, |request| {
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
        let written = write(&Value::Array(messages.iter().map(Dump::dump).collect()), Style::HTTPX);
        let schema_json = dumps(&schema.json_schema());
        let key = llm_body(purpose.as_str(), &written, &schema.name(), &schema_json, max_output_tokens);
        let request = json!({"purpose": purpose.as_str(), "messages": written, "schema_name": schema.name(),
                             "schema": schema_json, "max_output_tokens": max_output_tokens});
        let call = self.answers.take(&key, request, |_| None);
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
