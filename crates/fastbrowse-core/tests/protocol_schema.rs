//! `protocol/schema.json`, the document every client's types are generated from, is what `protocol::schema()` writes.
//! `FASTBROWSE_UPDATE_GOLDEN=1` rewrites it, and the diff is the change to the wire.

use std::fs;
use std::path::Path;

use fastbrowse_core::protocol::schema;

#[test]
fn the_committed_protocol_schema_is_the_one_the_types_describe() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/schema.json");
    let written = serde_json::to_string_pretty(&schema()).unwrap() + "\n";
    if std::env::var("FASTBROWSE_UPDATE_GOLDEN").as_deref() == Ok("1") {
        fs::write(&path, &written).unwrap();
    }
    let committed = fs::read_to_string(&path).unwrap_or_default();
    assert!(committed == written, "protocol/schema.json is stale: run with FASTBROWSE_UPDATE_GOLDEN=1");
}

fn definition(name: &str) -> serde_json::Value {
    schema()["$defs"][name].clone()
}

#[test]
fn the_result_is_described_as_it_is_written_and_not_as_it_is_read() {
    let result = definition("RunResult");
    let properties = result["properties"].as_object().unwrap();
    // `final_frame` is in no JSON result, so a type that listed it would promise a field nobody gets.
    assert!(!properties.contains_key("final_frame"));
    // The server writes every field, defaults included, so a client never has to handle a missing one.
    let required: Vec<&str> = result["required"].as_array().unwrap().iter().map(|key| key.as_str().unwrap()).collect();
    assert_eq!(required, properties.keys().map(String::as_str).collect::<Vec<_>>());
}

#[test]
fn a_frame_is_base64_text() {
    let frame = &definition("StepEvent")["properties"]["frame"];
    assert_eq!(frame["anyOf"], serde_json::json!([{"type": "string"}, {"type": "null"}]));
    assert_eq!(definition("RunFrame")["properties"]["frame"]["type"], "string");
}

#[test]
fn what_the_client_sends_keeps_its_defaults_optional() {
    assert_eq!(definition("RunParams")["required"], serde_json::json!(["run_id", "task"]));
    assert_eq!(definition("RunParams")["additionalProperties"], false);
}

#[test]
fn the_version_the_codes_and_the_method_names_are_beside_the_models() {
    let constants = &schema()["constants"];
    assert_eq!(constants["PROTOCOL_VERSION"], 1);
    assert_eq!(constants["ErrorCode"]["CANCELLED"], -32002);
    assert_eq!(constants["ErrorCode"]["UNSUPPORTED_SCHEMA"], -32004);
    assert_eq!(
        constants["Method"],
        serde_json::json!({"INITIALIZE": "initialize", "RUN": "run", "RUN_CANCEL": "run/cancel", "SHUTDOWN": "shutdown"})
    );
    assert_eq!(constants["ServerMethod"]["SECRETS_RESOLVE"], "secrets/resolve");
}
