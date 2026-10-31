//! Retrieval against every call the Python implementation's tests made, as `tests/golden/retrieval*.json` hold them.
//! `FASTBROWSE_UPDATE_GOLDEN=1` re-records them (see `common::capture`).

mod common;

use common::capture::*;
use common::*;
use fastbrowse_agent::memory::Notes;
use fastbrowse_agent::retrieval::*;
use fastbrowse_core::decision::{ChoiceAnswer, MAX_CHOICE_OPTIONS};
use fastbrowse_core::page::{Block, Capture};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::telemetry::{Ledger, Trace};
use serde_json::{Value, json};

fn strings(value: &Value) -> Vec<String> {
    match value {
        // A test that passed one id as a string had Python iterate its characters.
        Value::String(text) => text.chars().map(String::from).collect(),
        Value::Null => Vec::new(),
        other => parse(other),
    }
}

fn opt<T: serde::de::DeserializeOwned>(args: &Value, key: &str) -> Option<T> {
    args.get(key).filter(|value| !value.is_null()).map(parse)
}

fn field(value: &Value) -> ScalarField {
    scalar_field(&value["$field"])
}

fn outcome<T: Dump>(result: fastbrowse_agent::error::Result<T>) -> Value {
    match result {
        Ok(value) => json!({"result": value.dump()}),
        Err(error) => error_outcome(&error),
    }
}

/// Runs one case and returns every way Rust differed from the recording, or with `update` the case re-recorded.
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
    let notes: Option<Notes> = args.get("notes").filter(|n| n.get("$notes").is_some()).map(notes_arg);
    let capture = || parse::<Capture>(&args["capture"]);
    let result: Value = match name {
        "leading_quotes" => json!({"result": page_quotes(&capture()).dump()}),
        "read_candidates" => {
            let blocks: Option<Vec<Block>> = opt(args, "blocks");
            json!({"result": read_candidates(&capture(), blocks.as_deref()).dump()})
        }
        "field_question" => {
            let candidates: Vec<Candidate> = parse(&args["candidates"]);
            let record_fields = strings(args.get("record_fields").unwrap_or(&Value::Null));
            outcome(field_question(
                &field(&args["field"]),
                &candidates,
                args.get("name").and_then(Value::as_str),
                args.get("task").and_then(Value::as_str),
                &record_fields,
                MAX_CHOICE_OPTIONS,
            ))
        }
        "field_candidates" => match field_candidates(&capture(), &field(&args["field"])) {
            Ok(found) => json!({"result": found.dump()}),
            Err(unsupported) => json!({"result": unsupported.dump()}),
        },
        "field_candidates_from_notes" => {
            let capture: Option<Capture> = opt(args, "capture");
            match field_candidates_from_notes(notes.as_ref().unwrap(), &field(&args["field"]), capture.as_ref()) {
                Ok(found) => json!({"result": found.dump()}),
                Err(unsupported) => json!({"result": unsupported.dump()}),
            }
        }
        "copy_field" => {
            let answer: ChoiceAnswer = parse(&args["answer"]);
            let candidates: Vec<Candidate> = parse(&args["candidates"]);
            json!({"result": copy_field(&answer, &candidates).map(|(v, e)| json!([v.dump(), e.dump()]))})
        }
        "merge_candidates" => {
            let groups: Vec<Vec<Candidate>> = parse(&args["groups"]);
            let groups: Vec<&[Candidate]> = groups.iter().map(Vec::as_slice).collect();
            json!({"result": merge_candidates(&groups).dump()})
        }
        "choose_candidate" => {
            let candidates: Vec<Candidate> = parse(&args["candidates"]);
            let record_fields = strings(args.get("record_fields").unwrap_or(&Value::Null));
            let chosen = block_on(choose_candidate(
                &jev,
                &args["state"],
                &field(&args["field"]),
                &candidates,
                args["name"].as_str().unwrap(),
                args["task"].as_str().unwrap(),
                &record_fields,
                ledger.as_ref(),
            ));
            match chosen {
                Ok(found) => json!({"result": found.map(|(v, e)| json!([v.dump(), e.dump()]))}),
                Err(error) => error_outcome(&error),
            }
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
    if result != case["outcome"] {
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
    let events = trace.events();
    let mut written: Vec<String> = events.iter().map(|e| e.to_string()).collect();
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
    if text.len() > 3000 {
        format!("{}...", &text[..text.char_indices().nth(3000).map_or(text.len(), |(i, _)| i)])
    } else {
        text
    }
}

#[test]
fn every_recorded_call_makes_the_same_requests_and_decisions() {
    replay_files("retrieval", &["retrieval.json", "retrieval_large.json"], |_| None, replay);
}
