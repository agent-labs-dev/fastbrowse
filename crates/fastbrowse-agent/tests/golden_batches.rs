//! batches.py against the requests and results Python recorded.

mod common;

use std::sync::Mutex;

use common::*;
use fastbrowse_agent::batches::evaluate_batches;
use fastbrowse_core::config::TokenBudget;
use fastbrowse_core::decision::{Answer, Question};
use fastbrowse_core::py::dump::Dump;
use indexmap::IndexMap;
use serde_json::json;

#[test]
fn batches_send_what_python_sends() {
    for case in cases("batches.json") {
        let jev = ReplayJev::new(&case);
        let questions: IndexMap<String, Question> = parse(&case["questions"]);
        let tokens: TokenBudget = parse(&case["tokens"]);
        let ledger = ledger(&case["ledger"]);
        let seen = Mutex::new(Vec::<String>::new());
        let record = |key: &str, _: &Answer| seen.lock().unwrap().push(key.to_string());
        let result = block_on(evaluate_batches(
            &jev,
            &case["state"],
            &questions,
            &tokens,
            ledger.as_ref(),
            Some(&record),
            case["allow_failed_batches"].as_bool().unwrap(),
        ));
        jev.finish();
        let outcome = match result {
            Err(error) => error_outcome(&error),
            Ok(answered) => json!({
                "answered": answered.map(|answered| json!({
                    "answers": answered.answers.iter().map(|(k, a)| (k.clone(), a.dump())).collect::<serde_json::Map<_, _>>(),
                    "cost": answered.cost.iter().map(Dump::dump).collect::<Vec<_>>(),
                    "input_tokens": answered.input_tokens,
                    "requests": answered.requests,
                })),
                "on_answer": seen.into_inner().unwrap(),
            }),
        };
        assert_outcome(&case, outcome);
        assert_eq!(ledger_state(ledger.as_ref()), case["ledger_after"], "{}: ledger", case["name"]);
    }
}
