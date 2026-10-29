//! Retrieval against every call the Python implementation's tests made, as `tests/golden/retrieval*.json` hold them.

mod common;

use common::capture::*;
use common::*;
use fastbrowse_agent::memory::Notes;
use fastbrowse_agent::planner::{Plan, Requirement};
use fastbrowse_agent::retrieval::{self, *};
use fastbrowse_core::config::TokenBudget;
use fastbrowse_core::decision::{ChoiceAnswer, MAX_CHOICE_OPTIONS};
use fastbrowse_core::page::{Block, Capture};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::telemetry::{Ledger, Trace};
use indexmap::IndexMap;
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

fn flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn field(value: &Value) -> ScalarField {
    let field = &value["$field"];
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

fn fields(value: &Value) -> IndexMap<String, ScalarField> {
    value.as_object().into_iter().flatten().map(|(name, item)| (name.clone(), field(item))).collect()
}

fn outcome<T: Dump>(result: fastbrowse_agent::error::Result<T>) -> Value {
    match result {
        Ok(value) => json!({"result": value.dump()}),
        Err(error) => error_outcome(&error),
    }
}

fn read_outcome(outcome: &ReadOutcome) -> Value {
    let mut dumped = outcome.dump();
    dumped["collection_facts"] = Value::Array(outcome.collection_facts.iter().map(Dump::dump).collect());
    dumped
}

fn proposed(found: &Proposed) -> Value {
    Value::Object(
        found.iter().map(|(name, (written, evidence))| (name.clone(), json!([written, evidence.dump()]))).collect(),
    )
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
    let mut notes: Option<Notes> = args.get("notes").filter(|n| n.get("$notes").is_some()).map(notes_arg);
    let capture = || parse::<Capture>(&args["capture"]);
    let result: Value = match name {
        "chunk" => outcome(retrieval::chunk(
            &capture(),
            args["max_chars"].as_i64().unwrap(),
            args.get("overlap_blocks").and_then(Value::as_i64).unwrap_or(1),
        )),
        "leading_quotes" => json!({"result": leading_quotes(&capture(), args["max_chars"].as_i64().unwrap()).dump()}),
        "_read_message" => {
            let part: Chunk = parse(&args["part"]);
            let message = read_message_for(
                &capture(),
                &part,
                args["question"].as_str().unwrap(),
                &strings(&args["requirement_ids"]),
                args.get("evidence").and_then(Value::as_str).unwrap_or(""),
            );
            json!({"result": message.dump()})
        }
        "read_candidates" => {
            let blocks: Option<Vec<Block>> = opt(args, "blocks");
            json!({"result": read_candidates(&capture(), blocks.as_deref()).dump()})
        }
        "subject_words" => json!({"result": subject_words(args["text"].as_str().unwrap())}),
        "names_subject" => {
            json!({"result": names_subject(args["text"].as_str().unwrap(), args["subject"].as_str().unwrap())})
        }
        "_counted_collections" => {
            let requirements: Vec<Requirement> = opt(args, "requirements").unwrap_or_default();
            let targets: Option<Vec<Requirement>> = opt(args, "targets");
            let facts = counted_collections_for(
                &capture(),
                &strings(&args["requirement_ids"]),
                args["read_end"].as_i64().unwrap(),
                &requirements,
                targets.as_deref(),
            );
            json!({"result": facts.dump()})
        }
        "read_tallies" => {
            let readers: Vec<TallyReader> = parse(&args["readers"]);
            match read_tallies(&capture(), &readers, &strings(&args["requirement_ids"])) {
                Ok(found) => json!({"result": found.as_ref().map(read_outcome)}),
                Err(error) => error_outcome(&error),
            }
        }
        "filter_tallies" => {
            let readers: Vec<TallyReader> = parse(&args["readers"]);
            let ids = strings(&args["requirement_ids"]);
            match block_on(filter_tallies(&jev, &capture(), &readers, &ids, &tokens, ledger.as_ref(), &trace)) {
                Ok(found) => json!({"result": found.as_ref().map(read_outcome)}),
                Err(error) => error_outcome(&error),
            }
        }
        "merge_records" => {
            let outcome: ReadOutcome = parse(&args["self"]);
            match outcome.merge_records(notes.as_mut().unwrap()) {
                Ok(()) => json!({"result": null}),
                Err(error) => error_outcome(&error),
            }
        }
        "assemble_answer" => {
            let claims: Vec<Claim> = parse(&args["claims"]);
            let requirements: Vec<Requirement> = parse(&args["requirements"]);
            let dropped = args.get("dropped_claims").and_then(Value::as_i64).unwrap_or(0);
            json!({"result": assemble_answer(&claims, notes.as_ref().unwrap(), &requirements, dropped).dump()})
        }
        "retain_fields" => {
            let answer: ComposedAnswer = parse(&args["answer"]);
            let previous: ComposedAnswer = parse(&args["previous"]);
            json!({"result": retain_fields(answer, &previous, notes.as_ref().unwrap()).dump()})
        }
        "partial_answer" => {
            json!({"result": partial_answer(notes.as_ref().unwrap()).dump()})
        }
        "draft_answer" => {
            let plan: Plan = parse(&args["plan"]);
            json!({"result": draft_answer(&plan, notes.as_ref().unwrap()).map(|a| a.dump())})
        }
        "transaction_check_question" => {
            let composed: ComposedAnswer = parse(&args["composed"]);
            let question =
                transaction_check_question(&composed, notes.as_ref().unwrap(), &strings(&args["ids"]), &tokens);
            json!({"result": question.map(|q| q.dump())})
        }
        "claim_check_questions" => {
            let composed: ComposedAnswer = parse(&args["composed"]);
            let check = args.get("check_omission").and_then(Value::as_bool).unwrap_or(true);
            let checks = strings(args.get("answer_checks").unwrap_or(&Value::Null));
            outcome(claim_check_questions(&composed, notes.as_ref().unwrap(), &tokens, check, &checks))
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
        "propose_text_fields" => {
            let found = block_on(propose_text_fields(
                &llm,
                args["task"].as_str().unwrap(),
                &capture(),
                &fields(&args["fields"]),
                args.get("max_chars").and_then(Value::as_i64).unwrap_or(retrieval::READ_CHUNK_CHARS),
                &tokens,
                ledger.as_ref(),
            ));
            match found {
                Ok((found, costs)) => json!({"result": [proposed(&found), costs.dump()]}),
                Err(error) => error_outcome(&error),
            }
        }
        "propose_text_fields_from_notes" => {
            let capture: Option<Capture> = opt(args, "capture");
            let found = block_on(propose_text_fields_from_notes(
                &llm,
                args["task"].as_str().unwrap(),
                notes.as_ref().unwrap(),
                &fields(&args["fields"]),
                capture.as_ref(),
                &tokens,
                ledger.as_ref(),
            ));
            match found {
                Ok(found) => json!({"result": proposed(&found)}),
                Err(error) => error_outcome(&error),
            }
        }
        "compose" => {
            let plan: Plan = parse(&args["plan"]);
            let mut options = ComposeOptions::new();
            options.tokens = tokens.clone();
            options.ledger = ledger.as_ref();
            options.transaction_evidence_ids = strings(args.get("transaction_evidence_ids").unwrap_or(&Value::Null));
            options.corrections = opt(args, "corrections").unwrap_or_default();
            options.preserve_collections = flag(args, "preserve_collections");
            options.field_outputs = flag(args, "field_outputs");
            options.verified = opt(args, "verified");
            options.independent_fields = args.get("independent_fields").and_then(Value::as_bool).unwrap_or(true);
            let composed =
                block_on(compose(&llm, args["task"].as_str().unwrap(), &plan, notes.as_ref().unwrap(), &options));
            match composed {
                Ok(generation) => json!({"result": {"data": generation.data.dump(), "cost": generation.cost.dump()}}),
                Err(error) => error_outcome(&error),
            }
        }
        "read" => {
            let options = ReadOptions {
                max_chars: opt(args, "max_chars"),
                tokens: tokens.clone(),
                ledger: ledger.as_ref(),
                jev: args
                    .get("jev")
                    .filter(|j| !j.is_null())
                    .map(|_| &jev as &dyn fastbrowse_core::decision::DecisionClient),
                requirements: opt(args, "requirements").unwrap_or_default(),
                notice: args.get("notice").and_then(Value::as_str).unwrap_or("").to_string(),
                continuing: strings(args.get("continuing").unwrap_or(&Value::Null)),
                incomplete: strings(args.get("incomplete").unwrap_or(&Value::Null)),
                records_only: flag(args, "records_only"),
                require_all_evidence: flag(args, "require_all_evidence"),
                revalidate: flag(args, "revalidate"),
                recovering_outputs: flag(args, "recovering_outputs"),
                preserve_collections: flag(args, "preserve_collections"),
                field_outputs: flag(args, "field_outputs"),
                targets: opt(args, "targets"),
                skip_order_shortcuts: strings(args.get("_skip_order_shortcuts").unwrap_or(&Value::Null)),
                trace: trace.clone(),
            };
            let capture = capture();
            let ids = strings(&args["requirement_ids"]);
            let found = block_on(read(
                &llm,
                &capture,
                args["question"].as_str().unwrap(),
                &ids,
                notes.as_mut().unwrap(),
                &options,
            ));
            match found {
                Ok(found) => json!({"result": read_outcome(&found)}),
                Err(error) => error_outcome(&error),
            }
        }
        other => return Err(format!("no replay for {other}")),
    };
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
        if actual != sorted(wanted.clone()) {
            problems.push(format!("notes\n  rust:   {}\n  python: {}", clip(&actual), clip(&wanted)));
        }
    }
    if let (Some(key), Some(ledger)) = (&ledger_param, &ledger) {
        let expected = &case["ledger_after"][key];
        let expected = expected.get("$ledger").unwrap_or(expected);
        let actual = ledger_json(ledger);
        if !expected.is_null() && actual != *expected {
            problems.push(format!("ledger\n  rust:   {}\n  python: {}", clip(&actual), clip(expected)));
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
    if problems.is_empty() { Ok(()) } else { Err(problems.join("\n")) }
}

fn sorted(mut state: Value) -> Value {
    if let Some(Value::Array(owners)) = state.get_mut("owners") {
        owners.sort_by_key(|pair| pair[0].as_str().unwrap_or_default().to_string());
    }
    state
}

fn clip(value: &Value) -> String {
    let text = value.to_string();
    if text.len() > 3000 {
        format!("{}...", &text[..text.char_indices().nth(3000).map_or(text.len(), |(i, _)| i)])
    } else {
        text
    }
}

fn panic_text(panic: &Box<dyn std::any::Any + Send>) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "panic".into())
}

/// Cases whose Python outcome changes from one process to the next, so no fixed answer can match it.
fn python_only(name: &str) -> Option<String> {
    // read() builds `inherited` as a set of fact ids and appends it to a claim's basis in set order, which follows
    // PYTHONHASHSEED. Rust keeps first-seen order; Python's would be deterministic with `dict.fromkeys`.
    name.contains("test_partitioned_read_keeps_records_from_context_in_derived_answer")
        .then(|| "Python orders inherited records by string hash, which varies per process".into())
}

#[test]
fn every_recorded_call_makes_the_same_requests_and_decisions() {
    let mut cases = capture_cases("retrieval.json");
    cases.extend(capture_cases("retrieval_large.json"));
    let only = std::env::var("CASE").ok();
    let mut failures = Vec::new();
    let mut skipped = Vec::new();
    let mut passed = 0;
    for case in &cases {
        let name = case["name"].as_str().unwrap_or("?");
        if only.as_deref().is_some_and(|only| !name.contains(only)) {
            continue;
        }
        if let Some(reason) = skip_reason(case).or_else(|| python_only(name)) {
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
    eprintln!("retrieval goldens: {passed} passed, {} failed, {} skipped", failures.len(), skipped.len());
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
