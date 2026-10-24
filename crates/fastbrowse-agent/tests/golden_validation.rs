//! Response validation against what pydantic answered, valid and invalid, as `tests/golden/generate_validation.py`
//! recorded it. A re-ask carries pydantic's error text in its body, so the text has to match byte for byte.

mod common;

use common::*;
use fastbrowse_agent::planner::Plan;
use fastbrowse_agent::shortcut::{Shortcut, StartPage};
use fastbrowse_core::llm::{ResponseSchema, Typed};
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::schema::Schema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// Golden cases whose Rust model is not ported yet, or is private to its module. Wire each once its type lands:
/// - `_AnswerChecks`, `_FinishOutputs`: private to `planner.rs`.
/// - TODO(retrieval): `_ReadResponse`, `_CollectionResponse`, `_RecordsResponse`, `_TextProposals`, `_ListOrder`,
///   `_AnswerDraft`, `_FieldAnswerDraft` (a `ModelSchema` built as `compose` builds it).
/// - TODO(verification): `LLMVerdict`, `_OutputIdentities` (`_identity_schema(["output_0"])`), `_OutputAssessment`
///   (`_assessment_schema("output_0")`).
const PENDING: &[&str] = &[
    "_AnswerChecks",
    "_FinishOutputs",
    "_ReadResponse",
    "_CollectionResponse",
    "_RecordsResponse",
    "_TextProposals",
    "_ListOrder",
    "_AnswerDraft",
    "_FieldAnswerDraft",
    "LLMVerdict",
    "_OutputIdentities",
    "_OutputAssessment",
];

fn check<T: Schema + DeserializeOwned + Serialize>(case: &Value) {
    let schema = Typed::<T>::default();
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
            other => {
                assert!(PENDING.contains(&other), "a golden model with no check: {other}");
                continue;
            }
        }
        checked += 1;
    }
    assert!(checked > 20, "only {checked} cases checked");
}
