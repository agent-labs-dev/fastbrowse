//! verification.py against every call Python's own tests made, as `tests/golden/golden_capture.py` recorded them.

mod common;

use std::sync::Mutex;

use common::capture::*;
use common::*;
use fastbrowse_agent::memory::Notes;
use fastbrowse_agent::planner::Plan;
use fastbrowse_agent::policy::HistoryEntry;
use fastbrowse_agent::retrieval::{Annotation, AnswerCorrection, ComposedAnswer, ScalarField};
use fastbrowse_agent::verification::{self, *};
use fastbrowse_core::config::{Config, Thresholds, TokenBudget};
use fastbrowse_core::llm::{LLMClient, Message};
use fastbrowse_core::output_schema::output_model;
use fastbrowse_core::page::{Capture, Observation};
use fastbrowse_core::py::base64;
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::schema::json_schema;
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

fn flag(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
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

fn screenshots(value: &Value) -> Vec<Vec<u8>> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| base64::decode(item["$bytes"].as_str().unwrap_or_default()).expect("base64"))
        .collect()
}

fn outcome<T: Dump>(result: fastbrowse_agent::error::Result<T>) -> Value {
    match result {
        Ok(value) => json!({"result": value.dump()}),
        Err(error) => error_outcome(&error),
    }
}

fn schema_outcome(ty: &fastbrowse_core::schema::Ty) -> Value {
    json!({"result": {"$schema": {"json_schema": json_schema(ty)}}})
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
    let llm_arg: Option<&dyn LLMClient> = args.get("llm").filter(|l| !l.is_null()).map(|_| &llm as &dyn LLMClient);
    let trace = Trace::collecting();
    let tokens: TokenBudget = opt(args, "tokens").unwrap_or_default();
    let notes: Option<Notes> = args.get("notes").filter(|n| n.get("$notes").is_some()).map(notes_arg);
    let missing: Mutex<Vec<String>> = Mutex::new(opt(args, "missing_outputs").unwrap_or_default());
    let corrections: Mutex<Vec<AnswerCorrection>> = Mutex::new(opt(args, "corrections").unwrap_or_default());
    let verified_fields: Mutex<Vec<ComposedAnswer>> = Mutex::new(opt(args, "verified_fields").unwrap_or_default());
    let cache: OutputAuditCache = Mutex::default();
    let has = |key: &str| args.get(key).is_some_and(|v| !v.is_null());
    let check_options = || CheckOptions {
        tokens: tokens.clone(),
        ledger: ledger.as_ref(),
        missing_outputs: has("missing_outputs").then_some(&missing),
        corrections: has("corrections").then_some(&corrections),
        allow_scalar_jev: flag(args, "allow_scalar_jev", false),
        task: args.get("task").and_then(Value::as_str).unwrap_or("").to_string(),
        audit_cache: has("audit_cache").then_some(&cache),
        bounded: flag(args, "bounded", false),
        verified_outputs: None,
        independent_fields: flag(args, "independent_fields", true),
        today: opt(args, "today"),
        trace: trace.clone(),
        transaction_evidence_ids: strings(args.get("transaction_evidence_ids").unwrap_or(&Value::Null)),
        answer_checks: strings(args.get("answer_checks").unwrap_or(&Value::Null)),
        verified_fields: has("verified_fields").then_some(&verified_fields),
    };
    let notes_ref = || notes.as_ref().expect("a notes argument");
    let history: Vec<HistoryEntry> = opt(args, "history").unwrap_or_default();
    let visited = strings(args.get("visited").unwrap_or(&Value::Null));
    let doubted = strings(args.get("doubted").unwrap_or(&Value::Null));
    let invented = strings(args.get("invented").unwrap_or(&Value::Null));
    let verify_options = || VerifyOptions {
        doubted: &doubted,
        invented: &invented,
        visited: &visited,
        config: opt::<Config>(args, "config").unwrap_or_default(),
        ledger: ledger.as_ref(),
    };
    let result: Value = match name {
        "visual_only_state" => json!({"result": visual_only_state(&parse::<Observation>(&args["observation"]))}),
        "verify_cache_key" => json!({"result": verify_cache_key(&parse::<Vec<Message>>(&args["messages"]))}),
        "_identity_schema" => {
            let keys = strings(&args["keys"]);
            schema_outcome(&identity_schema(keys.iter().map(String::as_str)))
        }
        "_assessment_schema" => schema_outcome(&assessment_schema(args["key"].as_str().unwrap())),
        "_grounding" => json!({"result": grounding(notes_ref(), &parse::<Plan>(&args["plan"]), &invented)}),
        "_output_context" => {
            let composed: ComposedAnswer = parse(&args["composed"]);
            json!({"result": output_context(&composed, notes_ref()).map(|c| c.dump())})
        }
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
        "verify_messages" => outcome(verify_messages(
            args["task"].as_str().unwrap(),
            &parse::<Plan>(&args["plan"]),
            &parse::<Observation>(&args["observation"]),
            &screenshots(&args["screenshots"]),
            notes_ref(),
            &history,
            &verify_options(),
        )),
        "llm_verify" => {
            let found = block_on(llm_verify(
                &llm,
                args["task"].as_str().unwrap(),
                &parse::<Plan>(&args["plan"]),
                &parse::<Observation>(&args["observation"]),
                &screenshots(&args["screenshots"]),
                notes_ref(),
                &history,
                &verify_options(),
            ));
            match found {
                Ok(generation) => json!({"result": {"data": generation.data.dump(), "cost": generation.cost.dump()}}),
                Err(error) => error_outcome(&error),
            }
        }
        "check_done" => {
            let draft: Option<ComposedAnswer> = opt(args, "draft");
            let options =
                DoneOptions { tokens: tokens.clone(), history: &history, visited: &visited, trace: trace.clone() };
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
        "check_answer_outputs" => {
            let composed: ComposedAnswer = parse(&args["composed"]);
            let options = check_options();
            outcome(block_on(check_answer_outputs(
                &jev,
                llm_arg,
                &composed,
                notes_ref(),
                &strings(&args["checks"]),
                &options,
            )))
        }
        "check_claims" => {
            let composed: ComposedAnswer = parse(&args["composed"]);
            let options = check_options();
            outcome(block_on(check_claims(
                &jev,
                &composed,
                notes_ref(),
                &parse::<Thresholds>(&args["thresholds"]),
                llm_arg,
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
        if !expected.is_null() && !shares_ledger(case) && actual != expected {
            problems.push(format!("ledger\n  rust:   {}\n  python: {}", clip(&actual), clip(&expected)));
        }
    }
    let mutated = &case["mutated"];
    let collected: [(&str, Value); 3] = [
        ("missing_outputs", json!(missing.lock().unwrap().clone())),
        ("corrections", corrections.lock().unwrap().dump()),
        ("verified_fields", verified_fields.lock().unwrap().dump()),
    ];
    for (key, actual) in collected {
        if let Some(expected) = mutated.get(key)
            && actual != *expected
        {
            problems.push(format!("{key}\n  rust:   {}\n  python: {}", clip(&actual), clip(expected)));
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

/// The agent bills the done check and the verifier into the same ledger while `check_claims` runs beside them, and a
/// fake that waits lets those receipts land inside a recorded call's ledger effect. Python's ledger after such a call
/// holds lines from work outside it, and overlapping effects count one receipt twice, so neither ledger is a fixed
/// answer there; the requests and the outcome still are.
fn shares_ledger(case: &Value) -> bool {
    let name = case["name"].as_str().unwrap_or_default();
    case["function"] == "check_claims"
        && (name.starts_with("tests/test_agent.py::") || name.starts_with("tests/test_finish_overlap.py::"))
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
