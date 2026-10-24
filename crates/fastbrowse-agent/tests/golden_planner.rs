//! planner.py against the requests and results Python recorded.

mod common;

use common::*;
use fastbrowse_agent::planner::{Plan, answer_checks, make_plan, partition_plan};
use fastbrowse_core::models::Limits;
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::telemetry::Ledger;
use serde_json::{Value, json};

fn plan_outcome(plan: &Plan, cost: Value) -> Value {
    json!({"plan": plan.dump(), "cost": cost})
}

#[test]
fn plans_ask_what_python_asks() {
    for case in cases("planner.json") {
        let llm = ReplayLLM::new(&case);
        let task = case["task"].as_str().unwrap();
        let (outcome, ledger) = match case["function"].as_str().unwrap() {
            "make_plan" => {
                let ledger = ledger(&case["ledger"]);
                let start = case["start"].as_str();
                let normalize = case["normalize_checks"].as_bool().unwrap();
                let result = block_on(make_plan(&llm, task, start, ledger.as_ref(), normalize));
                let outcome = match result {
                    Ok(generated) => plan_outcome(&generated.data, generated.cost.dump()),
                    Err(error) => error_outcome(&error),
                };
                (outcome, ledger)
            }
            "partition_plan" => {
                let ledger = Ledger::new(Limits::default());
                let plan: Plan = parse(&case["plan"]);
                let independent = case["independent_fields"].as_bool().unwrap();
                let outcome = match block_on(partition_plan(&llm, task, &plan, &ledger, independent)) {
                    Ok(partition) => json!({
                        "plans": partition.plans.iter().map(Dump::dump).collect::<Vec<_>>(),
                        "targets": partition.targets.iter().map(|(k, t)| (k.clone(), t.dump())).collect::<serde_json::Map<_, _>>(),
                    }),
                    Err(error) => error_outcome(&error),
                };
                (outcome, Some(ledger))
            }
            "answer_checks" => {
                let plan: Plan = parse(&case["plan"]);
                let checked = block_on(answer_checks(&llm, task, &plan, None)).unwrap();
                (plan_outcome(&checked.data, checked.cost.map_or(Value::Null, |cost| cost.dump())), None)
            }
            other => panic!("no Rust function for {other}"),
        };
        llm.finish();
        assert_outcome(&case, outcome);
        assert_eq!(ledger_state(ledger.as_ref()), case["ledger_after"], "{}: ledger", case["name"]);
    }
}
