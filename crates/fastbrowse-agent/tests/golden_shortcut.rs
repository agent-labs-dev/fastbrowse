//! shortcut.py and scripted.py against what Python asked, accepted and answered.

mod common;

use common::*;
use fastbrowse_agent::scripted::{ScriptedJev, steps};
use fastbrowse_agent::shortcut::{accept, accept_start, propose_shortcut, propose_start};
use fastbrowse_core::decision::{DecisionClient, Question};
use fastbrowse_core::models::Limits;
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde_json::{Value, json};

#[test]
fn proposals_ask_what_python_asks() {
    let golden = golden("shortcut.json");
    for case in golden["proposals"].as_array().unwrap() {
        let llm = ReplayLLM::new(case);
        let ledger = Ledger::new(Limits::default());
        let task = case["task"].as_str().unwrap();
        let (url, cost) = if case["function"] == "propose_shortcut" {
            let generated = block_on(propose_shortcut(&llm, task, case["start"].as_str().unwrap(), Some(&ledger)));
            let generated = generated.unwrap();
            (generated.data.url, generated.cost)
        } else {
            let generated = block_on(propose_start(&llm, task, Some(&ledger))).unwrap();
            (generated.data.url, generated.cost)
        };
        llm.finish();
        assert_outcome(case, json!({"url": url, "cost": cost.dump()}));
        assert_eq!(ledger_state(Some(&ledger)), case["ledger_after"], "{}: ledger", case["name"]);
    }
}

#[test]
fn proposals_are_accepted_as_python_accepts_them() {
    let golden = golden("shortcut.json");
    for case in golden["accept"].as_array().unwrap() {
        let proposed = case["proposed"].as_str();
        assert_eq!(json!(accept(proposed, case["start"].as_str().unwrap())), case["accept"], "{case}");
        assert_eq!(json!(accept_start(proposed)), case["accept_start"], "{case}");
    }
}

#[test]
fn scripts_read_and_answer_as_python_does() {
    let golden = golden("shortcut.json");
    for case in golden["steps"].as_array().unwrap() {
        let parsed = match steps(case["task"].as_str().unwrap()) {
            Ok(found) => Value::Array(found.iter().map(Dump::dump).collect()),
            Err(_) => json!({"error": "ValueError"}),
        };
        assert_eq!(parsed, case["steps"], "{case}");
    }
    let jev = ScriptedJev::new(steps("click Add one; fill Token = secret:token").unwrap());
    for exchange in golden["scripted_jev"].as_array().unwrap() {
        let questions: IndexMap<String, Question> = parse(&exchange["questions"]);
        let evaluation = block_on(jev.evaluate(&json!({}), &questions)).unwrap();
        assert_eq!(evaluation.dump(), exchange["evaluation"], "{exchange}");
    }
}
