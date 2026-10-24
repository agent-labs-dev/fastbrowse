//! tests/test_serve_output.py: `output_schema` in a `run`. A JSON Schema goes in, `run_task` gets a model, and what
//! that model produced comes back under the caller's names.

mod common;

use common::{Recorder, result, run_params, runner, serving, settings};
use fastbrowse_cli::runner::{RunArgs, RunError};
use fastbrowse_core::models::{RunResult, Status};
use serde_json::{Value, json};

fn order_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "order-id": {"type": "string", "description": "The order number on the confirmation page"},
            "class": {"type": "string"},
            "lines": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {"sku": {"type": "string"}, "2-day": {"type": "boolean"}},
                    "required": ["sku", "2-day"],
                    "additionalProperties": false,
                },
            },
        },
        "required": ["order-id", "class", "lines"],
        "additionalProperties": false,
    })
}

#[tokio::test]
async fn test_a_run_without_an_output_schema_hands_run_task_none() {
    let recorder = Recorder::default();
    let mut client = serving(recorder.runner(), settings());
    client.result("run", Some(run_params("run-1", json!({})))).await;
    assert!(recorder.calls()[0].output_schema.is_none());
}

#[tokio::test]
async fn test_a_nested_schema_reaches_run_task_as_a_model() {
    let recorder = Recorder::default();
    let mut client = serving(recorder.runner(), settings());
    client.result("run", Some(run_params("run-1", json!({"output_schema": order_schema()})))).await;
    let model = recorder.calls()[0].output_schema.clone().unwrap();
    let order = json!({"order-id": "A-17", "class": "express", "lines": [{"sku": "kettle", "2-day": true}]});
    assert_eq!(model.validate(&order).unwrap(), order);
    assert!(
        model
            .fields()
            .iter()
            .any(|field| field.description.as_deref() == Some("The order number on the confirmation page"))
    );
    let mut short = order.clone();
    short["lines"] = json!([{"sku": "kettle"}]);
    assert!(model.validate(&short).is_err());
    let mut text = order.clone();
    text["lines"] = json!("kettle");
    assert!(model.validate(&text).is_err());
}

#[tokio::test]
async fn test_the_result_carries_the_data_under_the_callers_property_names() {
    let order = json!({"order-id": "A-17", "class": "express", "lines": [{"sku": "kettle", "2-day": true}]});
    let found = order.clone();
    // Fills the model as a run does, and dumps it under the caller's names.
    let filling = runner(move |args: RunArgs| {
        let found = found.clone();
        async move {
            let model = args.output_schema.unwrap();
            let data = model.validate(&found).map_err(|error| RunError::Failed(error.to_string()))?;
            Ok(RunResult { data, ..result(Status::Complete) })
        }
    });
    let mut client = serving(filling, settings());
    let result = client.result("run", Some(run_params("run-1", json!({"output_schema": order_schema()})))).await;
    assert_eq!(result["data"], order);
}

#[tokio::test]
async fn test_a_schema_the_server_cannot_honor_is_refused_before_a_run_starts() {
    for (schema, keyword, path) in [
        (
            json!({"type": "object", "properties": {"id": {"type": "string", "pattern": "^A-"}}}),
            "pattern",
            "#/properties/id/pattern",
        ),
        (
            json!({
                "type": "object",
                "properties": {"tree": {"$ref": "#/$defs/node"}},
                "$defs": {"node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/node"}}}},
            }),
            "$ref",
            "#/$defs/node/properties/next/$ref",
        ),
        (json!({"type": "array", "items": {"type": "string"}}), "type", "#/type"),
    ] {
        let recorder = Recorder::default();
        let mut client = serving(recorder.runner(), settings());
        let response = client.request("run", Some(run_params("run-1", json!({"output_schema": schema})))).await;
        let after = client.result("run", Some(run_params("run-2", json!({})))).await;
        assert_eq!(response["error"]["code"], -32004);
        let message = response["error"]["message"].as_str().unwrap();
        assert!(message.contains(&format!("\"{keyword}\"")) && message.contains(path), "{message}");
        // Nothing ran for the refused request, and the server is free for the next one.
        assert_eq!(recorder.calls().len(), 1);
        assert_eq!(after["status"], "complete");
    }
}

#[tokio::test]
async fn test_an_output_schema_that_is_not_an_object_is_invalid_params() {
    let recorder = Recorder::default();
    let mut client = serving(recorder.runner(), settings());
    let response = client.request("run", Some(run_params("run-1", json!({"output_schema": "order"})))).await;
    assert_eq!(response["error"]["code"], -32602);
    assert!(response["error"]["message"].as_str().unwrap().contains("output_schema"));
    assert!(recorder.calls().is_empty());
}
