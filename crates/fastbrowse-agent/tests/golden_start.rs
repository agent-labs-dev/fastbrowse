//! The start proposal and the scripted model clients, against what was asked, accepted and answered.

mod common;

use common::*;
use fastbrowse_agent::scripted::{ScriptedJev, steps};
use fastbrowse_agent::start::{accept_start, propose_start};
use fastbrowse_core::decision::{DecisionClient, Question};
use fastbrowse_core::models::Limits;
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde_json::{Value, json};

#[test]
fn proposals_ask_what_they_asked() {
    let golden = golden("start.json");
    for case in golden["proposals"].as_array().unwrap() {
        let llm = ReplayLLM::new(case);
        let ledger = Ledger::new(Limits::default());
        let generated = block_on(propose_start(&llm, case["task"].as_str().unwrap(), Some(&ledger))).unwrap();
        llm.finish();
        assert_outcome(case, json!({"url": generated.data.url, "cost": generated.cost.dump()}));
        assert_eq!(ledger_state(Some(&ledger)), case["ledger_after"], "{}: ledger", case["name"]);
    }
}

#[test]
fn proposals_are_accepted() {
    let golden = golden("start.json");
    for case in golden["accept"].as_array().unwrap() {
        assert_eq!(json!(accept_start(case["proposed"].as_str())), case["accept_start"], "{case}");
    }
}

#[test]
fn scripts_read_and_answer() {
    let golden = golden("start.json");
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
