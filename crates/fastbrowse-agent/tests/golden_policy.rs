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
    let mut golden = golden("policy.json");
    let updating = capture::updating();
    let cases = golden["cases"].as_array_mut().unwrap();
    assert!(cases.len() > 50);
    for case in cases.iter_mut() {
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
        if updating {
            let asked = jev.2.into_inner().unwrap();
            let calls = case["calls"].as_array_mut().unwrap().iter_mut().filter(|call| call.get("jev").is_some());
            for (call, (state, questions)) in calls.zip(asked) {
                call["jev"] = json!({"state": state, "questions": questions});
            }
            case["outcome"] = Value::Array(outcomes);
            case["ledger_after"] = ledger_state(ledger.as_ref());
            continue;
        }
        assert_outcome(case, Value::Array(outcomes));
        assert_eq!(ledger_state(ledger.as_ref()), case["ledger_after"], "{}: ledger", case["name"]);
    }
    if updating {
        save("policy.json", &golden);
    }
}

#[test]
fn requests_are_built_as_python_builds_them() {
    let mut golden = golden("policy.json");
    let updating = capture::updating();
    for case in golden["build_request"].as_array_mut().unwrap() {
        let observation: Observation = parse(&case["observation"]);
        let context: StepContext = parse(&case["context"]);
        let config: Config = parse(&case["config"]);
        let options = case_profile(case).max_choice_options;
        let compact = case["compact"].as_bool().unwrap();
        let request = build_request(&observation, &observation.controls, &context, &config, options, compact);
        let questions: Map<String, Value> = request.questions.iter().map(|(k, q)| (k.clone(), q.dump())).collect();
        let built = json!({
            "state": write(&request.state, Style::HTTPX),
            "questions": write(&Value::Object(questions), Style::HTTPX),
            "fits": fits(&request, &config),
        });
        for key in ["state", "questions", "fits"] {
            if updating {
                case[key] = built[key].clone();
            } else {
                assert_eq!(built[key], case[key], "{key}");
            }
        }
    }
    if updating {
        save("policy.json", &golden);
    }
}
