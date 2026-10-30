//! Verification against every call the Python implementation's tests made, as `tests/golden/verification*.json` hold
//! them.

mod common;

use common::capture::*;
use common::*;
use fastbrowse_agent::memory::Notes;
use fastbrowse_agent::planner::Plan;
use fastbrowse_agent::policy::HistoryEntry;
use fastbrowse_agent::retrieval::{Annotation, ComposedAnswer, ScalarField};
use fastbrowse_agent::verification::{self, *};
use fastbrowse_core::config::{Thresholds, TokenBudget};
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

fn field(field: &Value) -> ScalarField {
    let annotation = match field["annotation"].as_str() {
        Some("int") => Annotation::Int,
        Some("float") => Annotation::Float,
        Some("Decimal") => Annotation::Decimal,
        Some("date") => Annotation::Date,
        Some("bool") => Annotation::Bool,
        Some("str") => Annotation::Str,
        _ => Annotation::Other,
    };
    let bounds = field["metadata"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let (key, bound) = item.as_object()?.iter().next()?;
            bound.as_f64().map(|bound| (key.clone(), bound))
        })
        .collect();
    ScalarField {
        annotation,
        title: field["title"].as_str().map(str::to_string),
        description: field["description"].as_str().map(str::to_string),
        required: field["required"].as_bool().unwrap_or(true),
        default: field.get("default").cloned().unwrap_or(Value::Null),
        bounds,
    }
}

fn record_schema(value: &Value) -> Result<RecordSchema, String> {
    let schema = &value["$schema"];
    let model = output_model(&schema["json_schema"]).map_err(|error| format!("unsupported schema: {error}"))?;
    let fields = schema["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| (item["name"].as_str().unwrap_or_default().to_string(), field(item)))
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
fn replay(case: &Value) -> Result<(), String> {
    let args = &case["args"];
    let name = case["function"].as_str().unwrap_or_default();
    let ledger_param =
        args.as_object().and_then(|a| a.iter().find(|(_, v)| v.get("$ledger").is_some()).map(|(k, _)| k.clone()));
    let ledger: Option<Ledger> = ledger_param.as_ref().map(|key| ledger_arg(&args[key]));
    let ledgers: Vec<&Ledger> = ledger.iter().collect();
    let jev = KeyedJev::new(case, ledgers.clone());
    let llm = KeyedLLM::new(case, ledgers);
    let trace = Trace::collecting();
    let tokens: TokenBudget = opt(args, "tokens").unwrap_or_default();
    let notes: Option<Notes> = args.get("notes").filter(|n| n.get("$notes").is_some()).map(notes_arg);
    let notes_ref = || notes.as_ref().expect("a notes argument");
    let history: Vec<HistoryEntry> = opt(args, "history").unwrap_or_default();
    let visited = strings(args.get("visited").unwrap_or(&Value::Null));
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
        "check_done" => {
            let draft: Option<ComposedAnswer> = opt(args, "draft");
            let options = DoneOptions {
                tokens: tokens.clone(),
                history: &history,
                visited: &visited,
                changed_since_read: false,
                ledger: None,
                trace: trace.clone(),
            };
            outcome(block_on(check_done(
                &jev,
                args["task"].as_str().unwrap(),
                &parse::<Plan>(&args["plan"]),
                &parse::<Observation>(&args["observation"]),
                notes_ref(),
                &parse::<Thresholds>(&args["thresholds"]),
                draft.as_ref(),
                &options,
            )))
        }
        "extract" => {
            let schema = record_schema(&args["schema"])?;
            let options = ExtractOptions { notes: notes.as_ref(), tokens: tokens.clone(), ledger: ledger.as_ref() };
            let capture: Capture = parse(&args["capture"]);
            outcome(block_on(verification::extract(
                &jev,
                &llm,
                args["task"].as_str().unwrap(),
                &capture,
                &schema,
                &options,
            )))
        }
        other => return Err(format!("no replay for {other}")),
    };
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
        if actual != sorted(wanted.clone()) {
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
    if problems.is_empty() { Ok(()) } else { Err(problems.join("\n")) }
}

fn unordered_lines(mut ledger: Value) -> Value {
    if let Some(Value::Array(lines)) = ledger.get_mut("lines") {
        lines.sort_by_key(|line| line.to_string());
    }
    ledger
}

fn sorted(mut state: Value) -> Value {
    if let Some(Value::Array(owners)) = state.get_mut("owners") {
        owners.sort_by_key(|pair| pair[0].as_str().unwrap_or_default().to_string());
    }
    state
}

fn clip(value: &Value) -> String {
    let text = value.to_string();
    match text.char_indices().nth(3000) {
        Some((at, _)) => format!("{}...", &text[..at]),
        None => text,
    }
}

fn panic_text(panic: &Box<dyn std::any::Any + Send>) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "panic".into())
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
    let mut cases = capture_cases("verification.json");
    cases.extend(capture_cases("verification_large.json"));
    let only = std::env::var("CASE").ok();
    let mut failures = Vec::new();
    let mut skipped = Vec::new();
    let mut passed = 0;
    for case in &cases {
        let name = case["name"].as_str().unwrap_or("?");
        if only.as_deref().is_some_and(|only| !name.contains(only)) {
            continue;
        }
        if let Some(reason) = skip_reason(case).or_else(|| rust_skip(case)) {
            skipped.push(format!("{name}: {reason}"));
            continue;
        }
        let replayed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| replay(case)));
        match replayed {
            Ok(Ok(())) => passed += 1,
            Ok(Err(problem)) => failures.push(format!("{name}\n{problem}")),
            Err(panic) => failures.push(format!("{name}\npanicked: {}", panic_text(&panic))),
        }
    }
    eprintln!("verification goldens: {passed} passed, {} failed, {} skipped", failures.len(), skipped.len());
    for skip in &skipped {
        eprintln!("skipped {skip}");
    }
    let shown: Vec<&String> = failures.iter().take(8).collect();
    assert!(
        failures.is_empty(),
        "{} of {} cases differ; first:\n\n{}",
        failures.len(),
        cases.len(),
        shown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n\n")
    );
}
