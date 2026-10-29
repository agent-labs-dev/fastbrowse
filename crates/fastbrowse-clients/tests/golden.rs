//! The requests the Python clients sent and what they returned, fixed in `tests/golden/cases.json`, replayed against
//! the Rust clients.
//!
//! Each case's scripted responses are handed back in order; every request must match Python's byte for byte (and so its
//! canonical form, which a replay is keyed on), and every result, ledger line, collected spend and trace record must
//! equal Python's. Time is paused, so an attempt deadline or a retry wait passes at once.

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use common::{Script, Step, answer_model, plan_model, result_model};
use fastbrowse_clients::environment::Settings;
use fastbrowse_clients::failover::FailoverDecisionClient;
use fastbrowse_clients::http::{Http, Response, TransportError};
use fastbrowse_clients::openai_compatible::{
    LLMOptions, OpenAICompatibleLLM, ReasoningEffort, gateway_provider_routing, openrouter_provider_routing,
};
use fastbrowse_clients::system_one::SystemOneClient;
use fastbrowse_clients::validation::{Lines, Paid, jev_spend};
use fastbrowse_clients::vercel::VercelGatewayJevClient;
use fastbrowse_core::decision::{DecisionClient, DecisionError, Question};
use fastbrowse_core::llm::{LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{Bytes, CostLine, LLMPurpose, Limits};
use fastbrowse_core::py::base64;
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::json::{Style, write};
use fastbrowse_core::telemetry::{Ledger, Trace};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

fn cases() -> Vec<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/cases.json");
    serde_json::from_str(&fs::read_to_string(path).expect("golden file")).expect("golden JSON")
}

fn step(value: &Value) -> Step {
    if value.get("stall").is_some() {
        return Step::Stall;
    }
    if value.get("cancel").is_some() {
        return Step::Cancel;
    }
    if let Some(name) = value.get("raise").and_then(Value::as_str) {
        return Step::Fail(match name {
            "ReadTimeout" => TransportError::Timeout("ReadTimeout"),
            "ConnectError" => TransportError::Transient("ConnectError"),
            "UnsupportedProtocol" => TransportError::Fatal("UnsupportedProtocol"),
            other => panic!("unknown failure {other}"),
        });
    }
    let mut response = Response::new(value["status"].as_u64().unwrap() as u16, value["body"].as_str().unwrap());
    for (name, header) in value["headers"].as_object().unwrap() {
        response = response.with_header(name, header.as_str().unwrap());
    }
    Step::Reply(response)
}

enum Client {
    Llm(Box<dyn LLMClient>),
    Jev(Box<dyn DecisionClient>),
}

fn jev(spec: &Value, http: &Http) -> Box<dyn DecisionClient> {
    match build(spec, http) {
        Client::Jev(client) => client,
        Client::Llm(_) => panic!("expected a Jev client"),
    }
}

fn build(spec: &Value, http: &Http) -> Client {
    let text = |key: &str, default: &str| spec.get(key).and_then(Value::as_str).unwrap_or(default).to_string();
    match spec["kind"].as_str().unwrap() {
        "llm" => {
            let models: HashMap<LLMPurpose, String> = spec["models"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(purpose, model)| (LLMPurpose::parse(purpose).unwrap(), model.as_str().unwrap().to_string()))
                .collect();
            let provider_routing = match spec.get("provider_routing") {
                None => Some(openrouter_provider_routing()),
                Some(Value::String(name)) if name == "openrouter" => Some(openrouter_provider_routing()),
                Some(Value::String(name)) if name == "gateway" => Some(gateway_provider_routing()),
                Some(Value::Null) => None,
                Some(other) => Some(other.clone()),
            };
            let options = LLMOptions {
                reasoning_effort: spec.get("reasoning").and_then(Value::as_str).and_then(ReasoningEffort::parse),
                purpose_reasoning: HashMap::new(),
                provider_routing,
                strict: spec.get("strict").and_then(Value::as_bool).unwrap_or(true),
                extra_body: spec.get("extra_body").and_then(Value::as_object).cloned().unwrap_or_default(),
                ..LLMOptions::default()
            };
            let base_url = text("base_url", "https://openrouter.ai/api/v1");
            Client::Llm(Box::new(
                OpenAICompatibleLLM::new(text("api_key", "key"), http.clone(), &base_url, models, options).unwrap(),
            ))
        }
        "typesafe" => Client::Jev(Box::new(SystemOneClient::new(
            text("api_key", "key"),
            http.clone(),
            &text("base_url", ""),
            &text("model", ""),
        ))),
        "gateway" => Client::Jev(Box::new(VercelGatewayJevClient::new(
            text("api_key", "key"),
            http.clone(),
            &text("base_url", ""),
        ))),
        "failover" => Client::Jev(Box::new(FailoverDecisionClient::new(
            jev(&spec["primary"], http),
            jev(&spec["backup"], http),
            http.clone(),
        ))),
        "settings" => {
            let env: Vec<(String, String)> = spec["env"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_string()))
                .collect();
            let settings = Settings::from_sources(&env, None).unwrap();
            if spec["which"] == "jev" {
                Client::Jev(settings.decision(http).unwrap())
            } else {
                Client::Llm(settings.llm(http).unwrap())
            }
        }
        other => panic!("unknown client {other}"),
    }
}

fn cost_dump(cost: &CostLine) -> Value {
    let mut dumped = cost.dump();
    dumped["seconds"] = Value::Null;
    dumped
}

fn llm_error(error: &LLMError) -> Value {
    let kind = match error {
        LLMError::Failed(_) => "LLMError",
        LLMError::RetriesExhausted(_) => "LLMRetriesExhausted",
        LLMError::Rejected(_) => "LLMRejected",
        LLMError::Budget(_) => "BudgetExceeded",
    };
    json!({"error": {"type": kind, "message": normalized(&error.to_string())}})
}

fn jev_error(error: &DecisionError) -> Value {
    let kind = match error {
        DecisionError::Failed(_) => "JevError",
        DecisionError::InputTooLarge(_) => "JevInputTooLarge",
        DecisionError::TransportFailed(_) => "JevTransportFailed",
        DecisionError::RetriesExhausted(_) => "JevRetriesExhausted",
        DecisionError::Rejected(_) => "JevRejected",
        DecisionError::Budget(_) => "BudgetExceeded",
    };
    let mut dumped = json!({"type": kind, "message": normalized(&error.to_string())});
    if let DecisionError::RetriesExhausted(exhausted) = error {
        dumped["requests"] = exhausted.requests.into();
        dumped["unaccounted_requests"] = exhausted.unaccounted_requests.into();
        dumped["answered"] =
            exhausted.answered.iter().flat_map(|part| part.answers.keys().cloned()).collect::<Vec<_>>().into();
        dumped["last"] = exhausted.last.clone().into();
    }
    json!({"error": dumped})
}

/// "in 1.5s" read as "in <t>s": Python's clock and a paused tokio clock disagree on how long a call took.
fn normalized(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(index) = rest.find("in ") {
        out.push_str(&rest[..index]);
        let after = &rest[index + 3..];
        let digits = after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len());
        let tail = &after[digits..];
        if digits > 0
            && tail.len() >= 3
            && tail.as_bytes()[0] == b'.'
            && tail.as_bytes()[1].is_ascii_digit()
            && tail.as_bytes()[2] == b's'
        {
            out.push_str("in <t>s");
            rest = &tail[3..];
        } else {
            out.push_str("in ");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn schema(name: &str) -> common::Model {
    match name {
        "Result" => result_model(),
        "Answer" => answer_model(),
        "Plan" => plan_model(),
        other => panic!("unknown schema {other}"),
    }
}

fn traced(events: Vec<Value>) -> Vec<Value> {
    events
        .into_iter()
        .filter_map(|event| {
            let pick = |keys: &[&str]| {
                let mut kept = Map::new();
                for key in keys {
                    kept.insert(key.to_string(), event[*key].clone());
                }
                Some(Value::Object(kept))
            };
            match event["event"].as_str()? {
                "call" => pick(&["event", "call", "outcome"]),
                "request_retry" => pick(&["event", "call", "attempt", "reason", "wait"]),
                "jev_failover" => {
                    Some(json!({"event": "jev_failover", "reason": normalized(event["reason"].as_str().unwrap())}))
                }
                "cancelled_request_cost" => Some(event),
                _ => None,
            }
        })
        .collect()
}

fn canonical(body: &[u8]) -> String {
    if body.is_empty() {
        return String::new();
    }
    write(&serde_json::from_slice(body).unwrap(), Style::HTTPX.sorted())
}

async fn replay(case: &Value) -> Value {
    let script = Script::new(case["responses"].as_array().unwrap().iter().map(step));
    let trace = Trace::collecting();
    let http = script.traced(&trace);
    let limits: Limits = serde_json::from_value(case.get("limits").cloned().unwrap_or(json!({}))).unwrap();
    let ledger = Arc::new(Ledger::new(limits));
    let client = build(&case["client"], &http);
    let (mut results, mut ledgers, mut spends) = (vec![], vec![], vec![]);
    for call in case["calls"].as_array().unwrap() {
        let paid = Paid::default();
        let cancel = call.get("cancel").and_then(Value::as_bool).unwrap_or(false);
        let outcome = match (&client, call["op"].as_str().unwrap()) {
            (Client::Llm(llm), "generate") => {
                let messages: Vec<Message> = call["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| Message {
                        role: serde_json::from_value(m["role"].clone()).unwrap(),
                        content: m["content"].as_str().unwrap().to_string(),
                        images: m
                            .get("images")
                            .and_then(Value::as_array)
                            .map(|images| {
                                images.iter().map(|i| Bytes(base64::decode(i.as_str().unwrap()).unwrap())).collect()
                            })
                            .unwrap_or_default(),
                    })
                    .collect();
                let model = schema(call["schema"].as_str().unwrap());
                let purpose = LLMPurpose::parse(call["purpose"].as_str().unwrap()).unwrap();
                let cap = call.get("max_output_tokens").and_then(Value::as_i64).unwrap_or(2000);
                let ledger_ref = (call.get("ledger") != Some(&Value::Bool(false))).then_some(&*ledger);
                let schema: &dyn ResponseSchema = &model;
                let generating = llm.generate_value(purpose, &messages, schema, cap, ledger_ref);
                let finished = async {
                    match generating.await {
                        Ok(generated) => json!({"ok": {"data": generated.data, "cost": cost_dump(&generated.cost)}}),
                        Err(error) => llm_error(&error),
                    }
                };
                cancellable(finished, cancel, &script).await
            }
            (Client::Jev(jev), "evaluate") => {
                let questions: IndexMap<String, Question> = serde_json::from_value(call["questions"].clone()).unwrap();
                let state = call["state"].clone();
                let evaluating = async {
                    match jev.evaluate(&state, &questions).await {
                        Ok(evaluation) => {
                            let mut dumped = evaluation.dump();
                            dumped["cost"]["seconds"] = Value::Null;
                            json!({"ok": dumped})
                        }
                        Err(error) => jev_error(&error),
                    }
                };
                match call.get("spend").and_then(Value::as_str) {
                    Some("ledger") => {
                        let scoped = jev_spend(Lines::Ledger(ledger.clone()), Some(ledger.clone()), evaluating);
                        cancellable(scoped, cancel, &script).await
                    }
                    Some("paid") => {
                        cancellable(jev_spend(Lines::Paid(paid.clone()), None, evaluating), cancel, &script).await
                    }
                    _ => cancellable(evaluating, cancel, &script).await,
                }
            }
            _ => panic!("call does not fit the client"),
        };
        results.push(outcome);
        let state = ledger.state();
        ledgers.push(json!({
            "lines": state.lines.iter().map(cost_dump).collect::<Vec<_>>(),
            "llm_calls": state.llm_calls,
            "jev_calls": state.jev_calls,
        }));
        drop(state);
        spends.push(Value::Array(paid.lines().iter().map(cost_dump).collect()));
    }
    let requests: Vec<Value> = script
        .requests()
        .iter()
        .map(|request| {
            let mut headers = Map::new();
            for name in [
                "authorization",
                "content-type",
                "ai-gateway-auth-method",
                "ai-gateway-protocol-version",
                "ai-model-id",
                "ai-evaluation-model-specification-version",
            ] {
                if let Some(value) = request.header(name) {
                    let value = if name == "authorization" { "Bearer ***" } else { value };
                    headers.insert(name.into(), value.into());
                }
            }
            json!({
                "method": request.method.as_str(),
                "url": request.url,
                "headers": headers,
                "body": String::from_utf8(request.body.clone()).unwrap(),
                "canonical": canonical(&request.body),
            })
        })
        .collect();
    assert_eq!(script.unused(), 0, "{}: scripted responses unused", case["name"]);
    json!({"requests": requests, "results": results, "ledgers": ledgers, "spends": spends, "trace": traced(trace.events())})
}

async fn cancellable(future: impl std::future::Future<Output = Value>, cancel: bool, script: &Script) -> Value {
    if !cancel {
        return future.await;
    }
    tokio::select! {
        outcome = future => outcome,
        _ = script.cancel.notified() => json!({"error": {"type": "CancelledError", "message": ""}}),
    }
}

#[tokio::test(start_paused = true)]
async fn every_case_sends_and_returns_what_python_did() {
    let cases = cases();
    assert!(cases.len() > 60, "{} cases", cases.len());
    let mut failures = Vec::new();
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let got = replay(case).await;
        for key in ["requests", "results", "ledgers", "spends", "trace"] {
            if got[key] != case[key] {
                failures.push(format!(
                    "{name}: {key} differ\n  python: {}\n  rust:   {}",
                    serde_json::to_string(&case[key]).unwrap(),
                    serde_json::to_string(&got[key]).unwrap()
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{} of {} cases differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
}
