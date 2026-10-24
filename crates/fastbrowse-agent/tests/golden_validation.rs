//! Response validation against what pydantic answered, valid and invalid, as `tests/golden/generate_validation.py`
//! recorded it. A re-ask carries pydantic's error text in its body, so the text has to match byte for byte.

mod common;

use common::*;
use fastbrowse_agent::planner::Plan;
use fastbrowse_agent::retrieval;
use fastbrowse_agent::shortcut::{Shortcut, StartPage};
use fastbrowse_agent::verification;
use fastbrowse_core::llm::{ResponseSchema, Typed};
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::schema::Schema;
use fastbrowse_core::validation::ModelSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// Golden cases whose Rust model is not ported yet, or is private to its module. Wire each once its type lands:
/// - `_AnswerChecks`, `_FinishOutputs`: private to `planner.rs`.
const PENDING: &[&str] = &["_AnswerChecks", "_FinishOutputs"];

fn check<T: Schema + DeserializeOwned + Serialize>(case: &Value) {
    check_schema(&Typed::<T>::default(), case);
}

fn check_schema(schema: &dyn ResponseSchema, case: &Value) {
    let input = case["input"].as_str().unwrap();
    assert_eq!(dumps(&schema.json_schema()), case["schema"].as_str().unwrap(), "{input}: schema");
    let result = match schema.validate_json(input) {
        Ok(data) => json!({ "ok": data }),
        Err(errors) => json!({ "errors": errors }),
    };
    assert_eq!(result, case["result"], "{}: {input}", case["model"]);
}

#[test]
fn responses_validate_as_pydantic_validates_them() {
    let mut checked = 0;
    for case in cases("validation.json") {
        match case["model"].as_str().unwrap() {
            "Plan" => check::<Plan>(&case),
            "Shortcut" => check::<Shortcut>(&case),
            "StartPage" => check::<StartPage>(&case),
            "LLMVerdict" => check::<verification::LLMVerdict>(&case),
            "_OutputIdentities" => {
                check_schema(&ModelSchema { ty: verification::identity_schema(["output_0"]) }, &case)
            }
            "_OutputAssessment" => {
                check_schema(&ModelSchema { ty: verification::assessment_schema("output_0") }, &case)
            }
            name if retrieval::response_schema(name).is_some() => {
                check_schema(&retrieval::response_schema(name).unwrap(), &case)
            }
            other => {
                assert!(PENDING.contains(&other), "a golden model with no check: {other}");
                continue;
            }
        }
        checked += 1;
    }
    assert!(checked > 20, "only {checked} cases checked");
}
