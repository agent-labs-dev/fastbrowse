//! memory.py and comparison.py: the calls Python made on `Notes`, made again here, each result compared.

mod common;

use common::*;
use fastbrowse_agent::AgentError;
use fastbrowse_agent::comparison::{NumericComparison, QuotedField, complete_comparison};
use fastbrowse_agent::memory::{Fact, Notes, RenderOptions, Tally, evidence_id, fact_id, shows};
use fastbrowse_core::models::Evidence;
use fastbrowse_core::page::Capture;
use fastbrowse_core::py::dump::Dump;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

fn strings(value: &Value) -> Vec<String> {
    value.as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
}

fn error(error: AgentError) -> Value {
    let kind = match error {
        AgentError::NotesTooLarge(_) => "NotesTooLarge",
        AgentError::Invalid(message) if message.starts_with('\'') => "KeyError",
        _ => "ValueError",
    };
    json!({"error": kind})
}

fn snapshot(notes: &Notes, requirement_ids: &[String]) -> Value {
    let keys: Vec<String> = notes.facts_by_id().keys().cloned().collect();
    let per_fact: Map<String, Value> = keys
        .iter()
        .map(|key| {
            (
                key.clone(),
                json!({
                    "current": notes.current(key),
                    "derived": notes.derived(key),
                    "requirements": notes.fact_requirements(key),
                }),
            )
        })
        .collect();
    let requirements: Map<String, Value> = requirement_ids
        .iter()
        .map(|rid| {
            (
                rid.clone(),
                json!({
                    "supporting": notes.supporting(rid).into_iter().map(|(key, _)| key).collect::<Vec<_>>(),
                    "evidenced": notes.evidenced(rid),
                    "read_for": notes.read_for(rid).iter().map(evidence_id).collect::<Vec<_>>(),
                    "supporting_evidence": notes.supporting_evidence(rid).iter().map(evidence_id).collect::<Vec<_>>(),
                    "comparison_records": notes.comparison_records(Some(rid)),
                    "has_untallied_records": notes.has_untallied_records(rid),
                }),
            )
        })
        .collect();
    let tallies = notes.tallies();
    let options = RenderOptions { preserve_collections: true, ..RenderOptions::default() };
    json!({
        "facts": notes.facts_by_id().iter().map(|(key, fact)| json!([key, fact.dump()])).collect::<Vec<_>>(),
        "per_fact": per_fact,
        "requirements": requirements,
        "evidence": notes.evidence().keys().collect::<Vec<_>>(),
        "usable": notes.usable_evidence().keys().collect::<Vec<_>>(),
        "current": notes.current_evidence(None).keys().collect::<Vec<_>>(),
        "comparison_records": notes.comparison_records(None),
        "tallies": tallies.iter().map(Dump::dump).collect::<Vec<_>>(),
        "expanded": notes.expand_evidence_ids(keys.iter().rev().map(String::as_str)),
        "rendered": notes.render_with_ids(1_000_000, &options).unwrap().text,
        "navigation": notes.render_for_navigation(),
    })
}

fn options(value: &Value) -> RenderOptions {
    let flag = |name: &str| value.get(name).and_then(Value::as_bool).unwrap_or(false);
    RenderOptions {
        preserve_requirements: flag("preserve_requirements"),
        preserve_collections: flag("preserve_collections"),
        source_only: flag("source_only"),
        json_encoded: flag("json_encoded"),
        labels: value.get("labels").map(parse::<IndexMap<String, String>>),
    }
}

fn run(notes: &mut Notes, op: &Value) -> Value {
    let text = |name: &str| op[name].as_str().unwrap();
    match text("op") {
        "add" => json!(notes.add(parse::<Fact>(&op["fact"]))),
        "add_tally" => match notes.add_tally(parse::<Tally>(&op["tally"])) {
            Ok(fact) => json!({"id": fact_id(&fact), "fact": fact.dump()}),
            Err(e) => error(e),
        },
        "complete_tallies" => {
            notes.complete_tallies(text("requirement_id"));
            Value::Null
        }
        "add_continuation" => {
            notes.add_continuation(text("requirement_id"), text("record_id"));
            Value::Null
        }
        "supersede" => {
            notes.supersede(text("requirement_id"), text("url"), text("sha256"), text("text"));
            Value::Null
        }
        "unevidence" => {
            let ids = strings(&op["requirement_ids"]);
            notes.unevidence(ids.iter().map(String::as_str));
            Value::Null
        }
        "retain_for" => {
            let (ids, facts) = (strings(&op["requirement_ids"]), strings(&op["fact_ids"]));
            json!(notes.retain_for(ids.iter().map(String::as_str), facts.iter().map(String::as_str)))
        }
        "remember_capture" => {
            notes.remember_capture(&parse::<Capture>(&op["capture"]));
            Value::Null
        }
        "complete_comparison" => {
            json!(complete_comparison(notes, text("requirement_id"), &parse::<NumericComparison>(&op["comparison"])))
        }
        "render_with_ids" => match notes.render_with_ids(op["max_chars"].as_i64().unwrap(), &options(&op["options"])) {
            Ok(rendered) => json!({"text": rendered.text, "evidence_ids": rendered.evidence_ids}),
            Err(e) => error(e),
        },
        "render_for_navigation" => json!(notes.render_for_navigation()),
        "current_evidence" => {
            let capture = (!op["capture"].is_null()).then(|| parse::<Capture>(&op["capture"]));
            json!(notes.current_evidence(capture.as_ref()).keys().collect::<Vec<_>>())
        }
        "captured_page" => json!(notes.captured_page(&parse::<Evidence>(&op["evidence"])).map(|page| page.dump())),
        "for_requirements" => {
            let (ids, evidence) = (strings(&op["requirement_ids"]), strings(&op["evidence_ids"]));
            let scoped = notes.for_requirements(ids.iter().map(String::as_str), evidence.iter().map(String::as_str));
            snapshot(&scoped, &ids)
        }
        "snapshot" => snapshot(notes, &strings(&op["requirement_ids"])),
        other => panic!("no Rust call for {other}"),
    }
}

/// `FASTBROWSE_UPDATE_GOLDEN=1` records each call's result as it is now, after an intended behaviour change.
#[test]
fn notes_answer_as_python_answers() {
    let mut golden = golden("memory.json");
    let update = capture::updating();
    let cases = golden["cases"].as_array_mut().unwrap();
    assert!(cases.len() > 30);
    for case in cases.iter_mut() {
        let name = case["name"].clone();
        let ops = case["ops"].as_array_mut().unwrap();
        let facts: Vec<Fact> = parse(&ops[0]["facts"]);
        let mut notes = Notes::new(facts);
        for (index, op) in ops.iter_mut().enumerate().skip(1) {
            let result = run(&mut notes, op);
            if update {
                op["result"] = result;
            } else {
                assert_eq!(result, op["result"], "{name}: #{index} {}", op["op"]);
            }
        }
    }
    if update {
        let mut out = Vec::new();
        let mut writer =
            serde_json::Serializer::with_formatter(&mut out, serde_json::ser::PrettyFormatter::with_indent(b" "));
        serde::Serialize::serialize(&golden, &mut writer).unwrap();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/memory.json");
        std::fs::write(path, capture::ascii(&String::from_utf8(out).unwrap()) + "\n").unwrap();
    }
}

#[test]
fn quoted_fields_and_whole_word_quotes_match_python() {
    let golden = golden("memory.json");
    for case in golden["quoted_fields"].as_array().unwrap() {
        let field = QuotedField::new(case["prefix"].as_str().unwrap(), case["suffix"].as_str().unwrap());
        assert_eq!(json!(field.extract(case["quote"].as_str().unwrap())), case["value"], "{case}");
    }
    for case in golden["shows"].as_array().unwrap() {
        let shown = shows(case["text"].as_str().unwrap(), case["quote"].as_str().unwrap());
        assert_eq!(json!(shown), case["shows"], "{case}");
    }
}
