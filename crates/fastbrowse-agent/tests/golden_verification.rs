//! Verification against every call the Python implementation's tests made, as `tests/golden/verification*.json` hold
//! them. `FASTBROWSE_UPDATE_GOLDEN=1` re-records them (see `common::capture`).

mod common;

use common::capture::*;
use common::*;
use fastbrowse_agent::memory::Notes;
use fastbrowse_agent::verification::{self, *};
use fastbrowse_core::config::TokenBudget;
use fastbrowse_core::output_schema::output_model;
use fastbrowse_core::page::{Capture, Observation};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::telemetry::{Ledger, Trace};
use serde_json::{Map, Value, json};

fn strings(value: &Value) -> Vec<String> {
    match value {
        Value::Null => Vec::new(),
        other => parse(other),
    }
}

fn opt<T: serde::de::DeserializeOwned>(args: &Value, key: &str) -> Option<T> {
    args.get(key).filter(|value| !value.is_null()).map(parse)
}

fn record_schema(value: &Value) -> Result<RecordSchema, String> {
    let schema = &value["$schema"];
    let model = output_model(&schema["json_schema"]).map_err(|error| format!("unsupported schema: {error}"))?;
    let fields = schema["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| (item["name"].as_str().unwrap_or_default().to_string(), scalar_field(item)))
        .collect();
    Ok(RecordSchema { fields, model })
}

fn outcome<T: Dump>(result: fastbrowse_agent::error::Result<T>) -> Value {
    match result {
        Ok(value) => json!({"result": value.dump()}),
        Err(error) => error_outcome(&error),
    }
}

/// Runs one case and returns every way Rust differed from Python.
fn replay(case: &Value, update: bool) -> Result<Option<Value>, String> {
    let args = &case["args"];
    let name = case["function"].as_str().unwrap_or_default();
    let ledger_param =
        args.as_object().and_then(|a| a.iter().find(|(_, v)| v.get("$ledger").is_some()).map(|(k, _)| k.clone()));
    let ledger: Option<Ledger> = ledger_param.as_ref().map(|key| ledger_arg(&args[key]));
    let ledgers: Vec<&Ledger> = ledger.iter().collect();
    let log: Option<CallLog> = update.then(CallLog::default);
    let jev = KeyedJev::new(case, ledgers.clone(), log.clone());
    let llm = KeyedLLM::new(case, ledgers, log.clone());
    let trace = Trace::collecting();
    let tokens: TokenBudget = opt(args, "tokens").unwrap_or_default();
    let notes: Option<Notes> = args.get("notes").filter(|n| n.get("$notes").is_some()).map(notes_arg);
    let notes_ref = || notes.as_ref().expect("a notes argument");
    let result: Value = match name {
        "page_state" => {
            let questions = strings(args.get("questions").unwrap_or(&Value::Null));
            let context: Option<Map<String, Value>> = opt(args, "context");
            outcome(page_state(
                &parse::<Observation>(&args["observation"]),
                notes_ref(),
                &tokens,
                &questions,
                context.as_ref(),
            ))
        }
        "extract" => {
            let schema = record_schema(&args["schema"])?;
            let options = ExtractOptions { notes: notes.as_ref(), tokens: tokens.clone(), ledger: ledger.as_ref() };
            let capture: Capture = parse(&args["capture"]);
            outcome(block_on(verification::extract(&jev, args["task"].as_str().unwrap(), &capture, &schema, &options)))
        }
        other => return Err(format!("no replay for {other}")),
    };
    if let Some(log) = log {
        return Ok(Some(rerecorded(
            case,
            result,
            notes.as_ref(),
            ledger_param.as_deref().zip(ledger.as_ref()),
            &trace,
            log,
        )));
    }
    let mut problems = Vec::new();
    let mut expected_outcome = case["outcome"].clone();
    if let Some(schema) = expected_outcome.pointer_mut("/result/$schema").and_then(Value::as_object_mut) {
        // A model class is recorded with its fields too; its schema is what a request carries.
        schema.retain(|key, _| key == "json_schema");
    }
    if result != expected_outcome {
        problems.push(format!("outcome\n  rust:   {}\n  python: {}", clip(&result), clip(&case["outcome"])));
    }
    if let Some(notes) = &notes
        && case["notes_after"].get("notes").is_some()
    {
        let expected = &case["notes_after"]["notes"];
        let wanted = if expected.get("$same").is_some() { args["notes"]["$notes"].clone() } else { expected.clone() };
        let actual = notes_json(notes);
        if actual != sorted_owners(wanted.clone()) {
            problems.push(format!("notes\n  rust:   {}\n  python: {}", clip(&actual), clip(&wanted)));
        }
    }
    if let (Some(key), Some(ledger)) = (&ledger_param, &ledger) {
        let expected = &case["ledger_after"][key];
        let expected = expected.get("$ledger").unwrap_or(expected);
        // Calls sent side by side bill in whichever order they finish, so the lines are compared as a multiset.
        let actual = unordered_lines(ledger_json(ledger));
        let expected = unordered_lines(expected.clone());
        if !expected.is_null() && actual != expected {
            problems.push(format!("ledger\n  rust:   {}\n  python: {}", clip(&actual), clip(&expected)));
        }
    }
    let mut written: Vec<String> = trace.events().iter().map(|e| e.to_string()).collect();
    let mut wanted: Vec<String> = case["trace"].as_array().into_iter().flatten().map(|e| e.to_string()).collect();
    written.sort();
    wanted.sort();
    if written != wanted {
        problems.push(format!("trace\n  rust:   {written:?}\n  python: {wanted:?}"));
    }
    let finished = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        jev.finish();
        llm.finish();
    }));
    if let Err(panic) = finished {
        problems.push(panic_text(&panic));
    }
    if problems.is_empty() { Ok(None) } else { Err(problems.join("\n")) }
}

fn clip(value: &Value) -> String {
    let text = value.to_string();
    match text.char_indices().nth(3000) {
        Some((at, _)) => format!("{}...", &text[..at]),
        None => text,
    }
}

/// Cases Rust cannot reproduce, with why.
fn rust_skip(case: &Value) -> Option<String> {
    let outcome = &case["outcome"];
    // pydantic's str(ValidationError) for a hand-written model is not what an output schema's validation writes.
    if case["function"] == "extract"
        && outcome["result"]["problem"].as_str().is_some_and(|problem| problem.contains("validation error"))
    {
        return Some("pydantic's ValidationError text for a hand-written model".into());
    }
    None
}

#[test]
fn every_recorded_call_makes_the_same_requests_and_decisions() {
    replay_files("verification", &["verification.json", "verification_large.json"], rust_skip, replay);
}
