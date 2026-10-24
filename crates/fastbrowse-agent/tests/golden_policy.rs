//! policy.py against the Jev requests and decisions Python recorded.

mod common;

use common::*;
use fastbrowse_agent::policy::{RelevanceCache, StepContext, build_request, decide, fits};
use fastbrowse_core::config::Config;
use fastbrowse_core::page::Observation;
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::json::{Style, write};
use fastbrowse_core::telemetry::Trace;
use serde_json::{Map, Value, json};

#[test]
fn decisions_ask_and_choose_as_python_does() {
    let golden = golden("policy.json");
    let cases = golden["cases"].as_array().unwrap();
    assert!(cases.len() > 50);
    for case in cases {
        let jev = ReplayJev::new(case);
        let ledger = ledger(&case["ledger"]);
        let cache = case["cache"].as_bool().unwrap().then(RelevanceCache::default);
        let mut outcomes = Vec::new();
        for step in case["steps"].as_array().unwrap() {
            let observation: Observation = parse(&step["observation"]);
            let context: StepContext = parse(&step["context"]);
            let config: Config = parse(&step["config"]);
            let trace = Trace::collecting();
            let result =
                block_on(decide(&jev, &observation, &context, &config, ledger.as_ref(), cache.as_ref(), &trace));
            let result = match result {
                Ok(decision) => json!({"decision": decision.dump()}),
                Err(error) => error_outcome(&error),
            };
            outcomes.push(json!({"result": result, "trace": trace.events()}));
        }
        jev.finish();
        assert_outcome(case, Value::Array(outcomes));
        assert_eq!(ledger_state(ledger.as_ref()), case["ledger_after"], "{}: ledger", case["name"]);
    }
}

#[test]
fn requests_are_built_as_python_builds_them() {
    let golden = golden("policy.json");
    for case in golden["build_request"].as_array().unwrap() {
        let observation: Observation = parse(&case["observation"]);
        let context: StepContext = parse(&case["context"]);
        let config: Config = parse(&case["config"]);
        let request =
            build_request(&observation, &observation.controls, &context, &config, case["compact"].as_bool().unwrap());
        assert_eq!(write(&request.state, Style::HTTPX), case["state"].as_str().unwrap());
        let questions: Map<String, Value> = request.questions.iter().map(|(k, q)| (k.clone(), q.dump())).collect();
        assert_eq!(write(&Value::Object(questions), Style::HTTPX), case["questions"].as_str().unwrap());
        assert_eq!(json!(fits(&request, &config)), case["fits"]);
    }
}
