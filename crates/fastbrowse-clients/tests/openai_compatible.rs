//! tests/clients/test_openai_compatible.py and test_attempt_deadline.py.
//!
//! Python shortens its retry and deadline constants by monkeypatching them; here they stay Python's and the clock is
//! paused, so a full retry ladder passes at once. Python cancels a task and lets an abandoned
//! request run to its deadline so its receipt can be read; here the future is dropped, and a request in flight is
//! charged as unknown at once. The tests that depend on that wait say how the Rust behaviour differs.

mod common;

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::{Model, Script, Step, answering, reply, result_model, text};
use fastbrowse_clients::http::{Http, Response, TransportError};
use fastbrowse_clients::openai_compatible::{LLMOptions, OpenAICompatibleLLM};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, Role};
use fastbrowse_core::models::{Bytes, CostBasis, LLMPurpose, Limits, MaybeUnavailable};
use fastbrowse_core::py::base64;
use fastbrowse_core::schema::{Field, ModelTy, Ty, json_schema, strict_schema};
use fastbrowse_core::telemetry::{Ledger, Trace};
use serde_json::{Map, Value, json};
use tokio::sync::Notify;

const OPENROUTER: &str = "https://openrouter.ai/api/v1";

fn client(http: Http, base_url: &str, purpose: LLMPurpose, model: &str) -> OpenAICompatibleLLM {
    keyed("key", http, base_url, purpose, model)
}

fn keyed(key: &str, http: Http, base_url: &str, purpose: LLMPurpose, model: &str) -> OpenAICompatibleLLM {
    let models = HashMap::from([(purpose, model.to_string())]);
    OpenAICompatibleLLM::new(key, http, base_url, models, LLMOptions::default()).unwrap()
}

async fn generate(
    llm: &OpenAICompatibleLLM,
    purpose: LLMPurpose,
    messages: &[Message],
    cap: i64,
    ledger: Option<&Ledger>,
) -> Result<Generation<Value>, LLMError> {
    let schema = result_model();
    llm.generate_value(purpose, messages, &schema, cap, ledger).await
}

fn completion(content: Value, usage: Value) -> Step {
    reply(200, json!({"choices": [{"message": {"content": content}}], "usage": usage}))
}

fn truncated(content: &str) -> Step {
    reply(
        200,
        json!({"choices": [{"message": {"content": content}, "finish_reason": "length"}], "usage": {"completion_tokens": 9}}),
    )
}

fn stopped(content: &str, written: Option<i64>) -> Step {
    let mut body = json!({"choices": [{"message": {"content": content}, "finish_reason": "stop"}]});
    if let Some(written) = written {
        body["usage"] = json!({"completion_tokens": written});
    }
    reply(200, body)
}

fn caps(script: &Script) -> Vec<Value> {
    script.bodies().iter().map(|body| body["max_tokens"].clone()).collect()
}

fn capped(max_dollars: Option<f64>) -> Ledger {
    Ledger::new(Limits { max_dollars, ..Limits::default() })
}

/// Runs `future` until `signal` fires, then drops it, which is how a Rust caller cancels. True when it was cancelled.
async fn cancelled<F: Future>(future: F, signal: &Notify) -> bool {
    tokio::select! {
        _ = future => false,
        _ = signal.notified() => true,
    }
}

fn ok(response: Response) -> BoxFuture<'static, Result<Response, TransportError>> {
    Box::pin(async move { Ok(response) })
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-12 + 1e-6 * b.abs()
}

#[tokio::test]
async fn test_schema_repair_keeps_images_and_accounts_for_both_calls() {
    let png = b"\x89PNG\r\n\x1a\nimage".to_vec();
    let jpeg = b"\xff\xd8\xffimage".to_vec();
    let usage = json!({"prompt_tokens": 20, "completion_tokens": 5, "cost": 0.001});
    let script = Script::new([
        completion(json!(r#"{"count":"wrong"}"#), usage.clone()),
        completion(json!(r#"{"count":3}"#), usage),
    ]);
    let llm = client(script.http(), "https://llm.test/v1/", LLMPurpose::Read, "reader");
    let message =
        Message { role: Role::User, content: "Count".into(), images: vec![Bytes(png.clone()), Bytes(jpeg.clone())] };
    let result = generate(&llm, LLMPurpose::Read, &[message], 42, None).await.unwrap();
    assert_eq!(result.data["count"], 3);
    assert_eq!(result.cost.basis, CostBasis::Metered);
    assert_eq!(result.cost.dollars, Some(0.002));
    assert_eq!((result.cost.input_tokens, result.cost.output_tokens), (40, 10));
    assert_eq!(result.cost.purpose, Some(LLMPurpose::Read));
    for request in script.requests() {
        assert_eq!(request.url, "https://llm.test/v1/chat/completions");
        assert_eq!(request.header("authorization"), Some("Bearer key"));
    }
    let bodies = script.bodies();
    let body = &bodies[0];
    assert_eq!((body["model"].clone(), body["max_tokens"].clone()), (json!("reader"), json!(42)));
    // Strict output requires every property and no defaults; a docstring is the field's only guidance.
    let json_schema = &body["response_format"]["json_schema"];
    let schema = &json_schema["schema"];
    let properties = schema["properties"].as_object().unwrap();
    assert_eq!(json_schema["strict"], true);
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["required"], json!(properties.keys().collect::<Vec<_>>()));
    assert!(properties.values().all(|field| field.get("default").is_none()));
    assert_eq!(properties["tags"]["description"], "What was counted.");
    assert_eq!(body["provider"], json!({"require_parameters": true, "sort": "latency"}));
    assert_eq!(
        body["messages"][0],
        json!({"role": "user", "content": [
            {"type": "text", "text": "Count"},
            {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", base64::encode(&png))}},
            {"type": "image_url", "image_url": {"url": format!("data:image/jpeg;base64,{}", base64::encode(&jpeg))}},
        ]})
    );
    let retry = bodies[1]["messages"].as_array().unwrap();
    assert_eq!(retry.len(), 3);
    let last = retry[2].to_string();
    assert!(last.contains("Validation errors") && last.contains("count"));
}

#[tokio::test]
async fn test_unknown_price_retains_all_token_counts() {
    for (first, second) in [(Value::Null, Value::Null), (json!(0.01), Value::Null), (Value::Null, json!(0.02))] {
        let script = Script::new([
            completion(json!("not json"), json!({"prompt_tokens": 7, "completion_tokens": 2, "cost": first})),
            completion(json!(r#"{"count":0}"#), json!({"prompt_tokens": 7, "completion_tokens": 2, "cost": second})),
        ]);
        let llm = client(script.http(), "https://llm.test", LLMPurpose::Plan, "planner");
        let result = generate(&llm, LLMPurpose::Plan, &[], 2000, None).await.unwrap();
        assert_eq!((result.cost.basis, result.cost.dollars), (CostBasis::Unknown, None));
        assert_eq!((result.cost.input_tokens, result.cost.output_tokens), (14, 4));
    }
}

#[tokio::test]
async fn test_exactly_one_retry_then_llm_error() {
    let key = "sk-secret-key";
    // A rejected property is named in the error's path, and the model can name one after anything it read.
    let content = format!(r#"{{"{key}":1}}"#);
    let script = Script::new([completion(json!(content), Value::Null), completion(json!(content), Value::Null)]);
    let llm = keyed(key, script.http(), "https://llm.test", LLMPurpose::Plan, "planner");
    let error = generate(&llm, LLMPurpose::Plan, &[], 2000, None).await.unwrap_err();
    assert!(error.to_string().contains("after one retry"), "{error}");
    assert_eq!(script.requests().len(), 2);
    assert!(!error.to_string().contains(key));
}

#[tokio::test]
async fn test_invalid_image_is_not_silently_dropped() {
    let script = Script::new([]);
    let llm = client(script.http(), "https://llm.test", LLMPurpose::Plan, "planner");
    let message = Message { role: Role::User, content: "See image".into(), images: vec![Bytes(b"GIF89a".to_vec())] };
    let error = generate(&llm, LLMPurpose::Plan, &[message], 2000, None).await.unwrap_err();
    assert!(error.to_string().contains("PNG and JPEG"));
    assert!(script.requests().is_empty(), "an unsupported image must fail before sending");
}

#[tokio::test(start_paused = true)]
async fn test_an_overloaded_model_is_retried_then_reported_without_a_schema_repair() {
    // A 429 is a transport condition: repeat it, then report it rather than asking to fix the JSON. Python shortens the
    // ladder to three requests; the full one is six.
    let rate_limited = format!("rate limited{}", "x".repeat(2000));
    let script = Script::new((0..6).map(|_| text(429, &rate_limited)));
    let llm = client(script.http(), "https://llm.test", LLMPurpose::Plan, "planner");
    let error = generate(&llm, LLMPurpose::Plan, &[], 2000, None).await.unwrap_err();
    assert!(matches!(error, LLMError::RetriesExhausted(_)));
    assert!(error.to_string().contains("429") && error.to_string().len() < 500);
    assert_eq!(script.requests().len(), 6);
}

#[tokio::test]
async fn test_a_retry_reserves_its_own_call_and_keeps_an_unreadable_attempts_cost() {
    // A re-ask is a second billable generation: it must spend budget and never lose its cost.
    let unreadable = || reply(200, json!({"choices": [], "usage": {"cost": 0.02, "prompt_tokens": "invalid"}}));
    let script = Script::new([unreadable(), unreadable(), completion(json!(r#"{"count":1}"#), json!({"cost": 0.01}))]);
    let llm = client(script.http(), "https://llm.test/v1/", LLMPurpose::Read, "reader");
    let messages = [Message::user("Count")];
    let ledger = Ledger::new(Limits { max_llm_calls: Some(1), ..Limits::default() });
    let error = generate(&llm, LLMPurpose::Read, &messages, 2000, Some(&ledger)).await.unwrap_err();
    assert!(matches!(error, LLMError::Budget(_)));
    assert_eq!(script.requests().len(), 1);
    let result =
        generate(&llm, LLMPurpose::Read, &messages, 2000, Some(&Ledger::new(Limits::default()))).await.unwrap();
    // The first attempt's usage could not be read, so the total cannot claim to be metered.
    assert_eq!(script.requests().len(), 3);
    assert_eq!(result.data["count"], 1);
    assert_eq!((result.cost.basis, result.cost.dollars), (CostBasis::Unknown, None));
}

#[tokio::test(start_paused = true)]
async fn test_an_error_status_retry_does_not_add_llm_cost() {
    let script = Script::new([text(503, ""), completion(json!(r#"{"count":1}"#), json!({"cost": 0.01}))]);
    let ledger = Ledger::new(Limits { max_llm_calls: Some(2), ..Limits::default() });
    let llm = client(script.http(), "https://llm.test", LLMPurpose::Read, "reader");
    let result = generate(&llm, LLMPurpose::Read, &[], 2000, Some(&ledger)).await.unwrap();
    ledger.record([result.cost.clone()]).unwrap();
    assert_eq!(ledger.state().llm_calls, 2);
    assert_eq!((result.cost.basis, result.cost.dollars), (CostBasis::Metered, Some(0.01)));
    assert!(!ledger.breakdown().has_unknown());
}

#[tokio::test]
async fn test_a_response_truncated_at_the_callers_cap_is_reported_without_raising_it() {
    // The cap is the caller's setting: the same prompt under it would be cut off again, and a larger one is a ceiling
    // nobody asked for.
    let script = Script::new([truncated(r#"{"count":""#), stopped(r#"{"count":5}"#, None)]);
    let llm = client(script.http(), "https://llm.test", LLMPurpose::Read, "reader");
    let error = generate(&llm, LLMPurpose::Read, &[], 100, None).await.unwrap_err();
    assert!(error.to_string().contains("truncated at the 100 token output cap"), "{error}");
    assert!(!error.to_string().contains("schema"));
    assert_eq!(caps(&script), [json!(100)]);
}

#[tokio::test]
async fn test_a_response_cut_off_with_no_cap_sends_no_cap_and_is_reported_once() {
    let script = Script::new([truncated(r#"{"count":""#), stopped(r#"{"count":5}"#, None)]);
    let llm = client(script.http(), "https://llm.test", LLMPurpose::Verify, "verifier");
    let error = generate(&llm, LLMPurpose::Verify, &[], 0, None).await.unwrap_err();
    assert!(error.to_string().contains("no output cap set (verify)"), "{error}");
    assert_eq!(script.requests().len(), 1);
    assert!(script.bodies()[0].get("max_tokens").is_none());
}

#[tokio::test]
async fn test_a_response_that_ran_out_of_tokens_before_any_text_is_truncation() {
    // A reasoning model can spend the whole cap before it writes; that is not an empty or refused completion.
    let empty = || reply(200, json!({"choices": [{"message": {"content": null}, "finish_reason": "length"}]}));
    let script = Script::new([empty(), empty()]);
    let llm = client(script.http(), "https://llm.test", LLMPurpose::Read, "reader");
    let error = generate(&llm, LLMPurpose::Read, &[], 2000, None).await.unwrap_err();
    assert!(error.to_string().contains("truncated"));
    assert_eq!(script.requests().len(), 1);
}

#[tokio::test]
async fn test_json_that_ends_mid_value_is_truncation_even_when_the_provider_says_it_stopped() {
    let script = Script::new([stopped(r#"{"count":"cut off mid-str"#, None), stopped(r#"{"count":2}"#, None)]);
    let llm = client(script.http(), "https://llm.test", LLMPurpose::Read, "reader");
    let error = generate(&llm, LLMPurpose::Read, &[], 100, None).await.unwrap_err();
    assert!(error.to_string().contains("truncated at the 100 token output cap"), "{error}");
    assert_eq!(caps(&script), [json!(100)]);
}

#[tokio::test]
async fn test_json_that_ends_mid_value_short_of_the_cap_is_a_provider_fault_not_truncation() {
    let script = Script::new([stopped(r#"{"count":"cut off"#, Some(7)), stopped(r#"{"count":"cut off"#, Some(7))]);
    let llm = client(script.http(), "https://llm.test", LLMPurpose::Read, "reader");
    let error = generate(&llm, LLMPurpose::Read, &[], 100, None).await.unwrap_err();
    assert!(matches!(error, LLMError::RetriesExhausted(_)));
    assert!(error.to_string().contains("ended mid-JSON at 7 of 100"), "{error}");
    assert!(error.is_unavailable());
    assert_eq!(caps(&script), [json!(100), json!(100)]);
}

#[tokio::test]
async fn test_a_short_reply_after_another_retry_is_still_asked_for_again() {
    // After a schema repair.
    let replies = [(r#"{"count":"five"}"#, 7), (r#"{"count":"cut off"#, 7), (r#"{"count":5}"#, 7)];
    let script = Script::new(replies.map(|(content, written)| stopped(content, Some(written))));
    let llm = client(script.http(), "https://llm.test", LLMPurpose::Read, "reader");
    let result = generate(&llm, LLMPurpose::Read, &[], 100, None).await.unwrap();
    assert_eq!(result.data["count"], 5);
    assert_eq!(script.requests().len(), 3);
}

#[tokio::test]
async fn test_complete_json_marked_truncated_never_becomes_a_citable_read() {
    let script = Script::new([truncated(r#"{"count":5}"#), truncated(r#"{"count":5}"#)]);
    let llm = client(script.http(), "https://llm.test", LLMPurpose::Read, "reader");
    let error = generate(&llm, LLMPurpose::Read, &[], 100, None).await.unwrap_err().to_string();
    assert!(error.contains("truncated") && error.ends_with("(read)"), "{error}");
    assert_eq!(caps(&script), [json!(100)]);
}

#[test]
fn test_a_strict_schema_states_the_array_bounds_validation_enforces() {
    // A model is held only to rules it was told: the bound validation checks goes out in the schema.
    let bounded = ModelTy::frozen(
        "Bounded",
        vec![Field::default("items", Ty::list(Ty::Str).constrained("maxItems", 2), json!([]))],
    )
    .ty();
    let strict = strict_schema(&json_schema(&bounded)).unwrap().to_string();
    assert!(strict.contains("\"maxItems\":2") && !strict.contains("default"), "{strict}");
}

#[test]
fn test_a_strict_schema_refuses_a_map_with_free_keys() {
    // Gemini through the Vercel AI Gateway returns such a map empty where OpenRouter fills it.
    let keyed = ModelTy::frozen("Keyed", vec![Field::required("entries", Ty::map(Ty::Int))]).ty();
    let error = strict_schema(&json_schema(&keyed)).unwrap_err();
    assert!(error.to_string().contains("free keys"));
}

#[tokio::test(start_paused = true)]
async fn test_cancelled_generation_records_dispatched_costs_and_paid_retries_once() {
    for paid_retry in [false, true] {
        for max_dollars in [None, Some(1.0)] {
            let mut steps = vec![];
            if paid_retry {
                steps.push(completion(json!(r#"{"count":"wrong"}"#), json!({"cost": 0.01})));
            }
            steps.push(Step::Cancel);
            let script = Script::new(steps);
            let ledger = capped(max_dollars);
            let llm = client(script.http(), "https://llm.test", LLMPurpose::Plan, "planner");
            assert!(cancelled(generate(&llm, LLMPurpose::Plan, &[], 2000, Some(&ledger)), &script.cancel).await);
            let lines = ledger.state().lines.clone();
            assert_eq!(lines.len(), if paid_retry { 2 } else { 1 });
            assert_eq!((lines.last().unwrap().basis, lines.last().unwrap().dollars), (CostBasis::Unknown, None));
            if paid_retry {
                assert_eq!(lines[0].dollars, Some(0.01));
            }
            if max_dollars.is_some() {
                assert!(ledger.check(0.0).unwrap_err().to_string().contains("reported no cost"));
            }
        }
    }
}

#[tokio::test]
async fn test_cancelled_generation_before_dispatch_does_not_wait_or_record_spend() {
    // Python stops the call inside a patched `_send`; a Rust future dropped before it is polled has sent nothing.
    let script = Script::new([]);
    let ledger = capped(Some(0.25));
    let llm = client(script.http(), "https://llm.test", LLMPurpose::Plan, "planner");
    drop(generate(&llm, LLMPurpose::Plan, &[], 2000, Some(&ledger)));
    assert!(script.requests().is_empty());
    assert!(ledger.state().lines.is_empty());
    ledger.check(0.0).unwrap();
}

#[tokio::test(start_paused = true)]
async fn test_abandoned_generation_is_charged_for_its_request_in_flight() {
    // Python waits for the abandoned response and records its metered receipt. Drop cannot wait, so the request in
    // flight is charged as unknown, which a dollar cap then refuses to enforce past.
    for purpose in [LLMPurpose::Plan, LLMPurpose::FieldText] {
        let script = Script::new([Step::Cancel]);
        let ledger = capped(Some(0.25));
        let llm = client(script.http(), "https://llm.test", purpose, "model");
        assert!(cancelled(generate(&llm, purpose, &[], 2000, Some(&ledger)), &script.cancel).await);
        assert_eq!(ledger.state().llm_calls, 1);
        let lines = ledger.state().lines.clone();
        assert_eq!(lines.len(), 1);
        assert_eq!((lines[0].basis, lines[0].dollars), (CostBasis::Unknown, None));
        assert!(ledger.check(0.0).is_err());
    }
}

fn without_cost(id: &str) -> Value {
    json!({"id": id, "choices": [{"message": {"content": r#"{"count":3}"#}}], "usage": {"prompt_tokens": 7, "completion_tokens": 2}})
}

/// A transport answering completions with `completion` and every GET with `receipt`.
fn receipts(completion: Value, receipt: Value) -> Http {
    answering(move |request| {
        let body = if request.method.as_str() == "GET" { receipt.clone() } else { completion.clone() };
        ok(Response::json(200, &body))
    })
}

#[tokio::test]
async fn test_missing_field_cost_recovers_matching_provider_receipt() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let http = answering(move |request| {
        record.lock().unwrap().push(request.url.clone());
        if request.method.as_str() == "GET" {
            return ok(Response::json(200, &json!({"data": {"id": "generation-1", "total_cost": 0.001}})));
        }
        ok(Response::json(200, &without_cost("generation-1")))
    });
    let ledger = capped(Some(0.25));
    let llm = client(http, OPENROUTER, LLMPurpose::FieldText, "writer");
    let result = generate(&llm, LLMPurpose::FieldText, &[], 2000, Some(&ledger)).await.unwrap();
    ledger.record([result.cost.clone()]).unwrap();
    assert_eq!(seen.lock().unwrap()[1], "https://openrouter.ai/api/v1/generation?id=generation-1");
    assert_eq!((result.cost.dollars, result.cost.basis), (Some(0.001), CostBasis::Metered));
    assert_eq!(result.cost.input_tokens, 7);
}

#[tokio::test(start_paused = true)]
async fn test_untrusted_or_missing_receipt_keeps_spend_guard() {
    for receipt in [
        json!({"id": "another-generation", "total_cost": 0.0}),
        json!({"id": "generation-1", "total_cost": -1}),
        json!({"id": "generation-1", "total_cost": null}),
    ] {
        let llm = client(
            receipts(without_cost("generation-1"), json!({"data": receipt})),
            OPENROUTER,
            LLMPurpose::FieldText,
            "writer",
        );
        let ledger = capped(Some(0.25));
        let result = generate(&llm, LLMPurpose::FieldText, &[], 2000, Some(&ledger)).await.unwrap();
        assert!(ledger.record([result.cost]).unwrap_err().to_string().contains("cannot be enforced"));
    }
}

#[tokio::test(start_paused = true)]
async fn test_cancelled_receipt_lookup_retains_unknown_generation_charge() {
    let script = Script::new([reply(200, without_cost("generation-1")), Step::Cancel]);
    let ledger = capped(Some(0.25));
    let llm = client(script.http(), OPENROUTER, LLMPurpose::FieldText, "writer");
    assert!(cancelled(generate(&llm, LLMPurpose::FieldText, &[], 2000, Some(&ledger)), &script.cancel).await);
    let lines = ledger.state().lines.clone();
    assert_eq!(lines.len(), 1);
    assert_eq!((lines[0].basis, lines[0].input_tokens), (CostBasis::Unknown, 7));
    assert!(ledger.check(0.0).unwrap_err().to_string().contains("cannot be enforced"));
}

#[tokio::test]
async fn test_gateway_byok_receipt_includes_estimated_provider_spend() {
    for base_url in ["https://ai-gateway.vercel.sh/v1", OPENROUTER] {
        let receipt = json!({"data": {"id": "generation-byok", "is_byok": true, "total_cost": 0.001, "upstream_inference_cost": 0.3}});
        let llm = client(receipts(without_cost("generation-byok"), receipt), base_url, LLMPurpose::FieldText, "writer");
        let result = generate(&llm, LLMPurpose::FieldText, &[], 2000, None).await.unwrap();
        assert_eq!(result.cost.basis, CostBasis::Estimated);
        assert!(approx(result.cost.dollars.unwrap(), 0.301));
        assert!(capped(Some(0.25)).record([result.cost]).is_err());
    }
}

#[tokio::test(start_paused = true)]
async fn test_byok_receipt_without_valid_upstream_keeps_unknown_cost() {
    for upstream in [None, Some(Value::Null), Some(json!(-1)), Some(json!("invalid"))] {
        for base_url in ["https://ai-gateway.vercel.sh/v1", OPENROUTER] {
            let mut receipt = json!({"id": "byok", "is_byok": true, "total_cost": 0});
            if let Some(upstream) = &upstream {
                receipt["upstream_inference_cost"] = upstream.clone();
            }
            let completion = json!({"id": "byok", "choices": [{"message": {"content": r#"{"count":3}"#}}]});
            let llm = client(receipts(completion, json!({"data": receipt})), base_url, LLMPurpose::FieldText, "writer");
            let result = generate(&llm, LLMPurpose::FieldText, &[], 2000, None).await.unwrap();
            assert_eq!(result.cost.basis, CostBasis::Unknown, "{upstream:?} {base_url}");
            assert!(capped(Some(0.25)).record([result.cost]).unwrap_err().to_string().contains("cannot be enforced"));
        }
    }
}

#[tokio::test]
async fn test_receipt_does_not_double_count_normal_provider_spend() {
    for base_url in ["https://ai-gateway.vercel.sh/v1", OPENROUTER] {
        for (byok, upstream, basis, amount) in [
            (Some(json!(false)), json!(0.3), CostBasis::Metered, Some(0.001)),
            (Some(json!(true)), json!(0), CostBasis::Metered, Some(0.001)),
            (None, json!(0.3), CostBasis::Estimated, Some(0.301)),
            (Some(json!("true")), json!(0.3), CostBasis::Unknown, None),
        ] {
            let mut receipt = json!({"id": "generation", "total_cost": 0.001, "upstream_inference_cost": upstream});
            if let Some(byok) = &byok {
                receipt["is_byok"] = byok.clone();
            }
            let completion = json!({"id": "generation", "choices": [{"message": {"content": r#"{"count":3}"#}}]});
            // A distrusted receipt on OpenRouter goes on to a price lookup; this one has no price list, so it stays
            // unknown as the Python test's handler leaves it.
            let llm = client(receipts(completion, json!({"data": receipt})), base_url, LLMPurpose::FieldText, "writer");
            let result = generate(&llm, LLMPurpose::FieldText, &[], 2000, None).await.unwrap();
            assert_eq!(result.cost.basis, basis, "{byok:?} {base_url}");
            match amount {
                None => assert_eq!(result.cost.dollars, None),
                Some(amount) => assert!(approx(result.cost.dollars.unwrap(), amount)),
            }
        }
    }
}

fn endpoints() -> Value {
    json!({"data": {"id": "writer", "endpoints": [
        {"context_length": 1000, "pricing": {"prompt": "0.000001", "completion": "0.000004", "request": "0"}},
        {"context_length": 2000, "pricing": {"prompt": "0.000002", "completion": "0.000003", "image": "0.01", "discount": 0.5}},
    ]}})
}

/// The largest window at the highest token price, the image fee once, and the BYOK fee on top.
const CEILING: f64 = (2000.0 * 0.000004 + 0.01) * 1.05;

#[tokio::test(start_paused = true)]
async fn test_requests_without_a_response_are_bounded_so_a_capped_run_can_go_on() {
    // Every attempt timed out, so no cost was reported; the run must hear the failure, not a stopped budget. Python
    // shortens the ladder to two requests; all six are bounded here, from one price lookup.
    let looked_up = Arc::new(Mutex::new(Vec::new()));
    let record = looked_up.clone();
    let http = answering(move |request| {
        if request.method.as_str() == "GET" {
            record.lock().unwrap().push(url::Url::parse(&request.url).unwrap().path().to_string());
            return ok(Response::json(200, &endpoints()));
        }
        Box::pin(async { Err(TransportError::Timeout("ReadTimeout")) })
    });
    let image =
        Message { role: Role::User, content: "Write".into(), images: vec![Bytes(b"\x89PNG\r\n\x1a\n0".to_vec())] };
    let ledger = capped(Some(28.0));
    let llm = client(http, OPENROUTER, LLMPurpose::FieldText, "writer");
    let error = generate(&llm, LLMPurpose::FieldText, &[image], 2000, Some(&ledger)).await.unwrap_err();
    assert!(matches!(error, LLMError::RetriesExhausted(_)), "{error}");
    assert_eq!(*looked_up.lock().unwrap(), ["/api/v1/models/writer/endpoints"]);
    let lines = ledger.state().lines.clone();
    assert_eq!(lines.len(), 6);
    assert!(lines.iter().all(|line| line.basis == CostBasis::Bounded));
    assert!(approx(ledger.breakdown().known_dollars(), 6.0 * CEILING));
    ledger.check(0.0).unwrap();
}

#[tokio::test(start_paused = true)]
async fn test_a_response_without_cost_or_receipt_is_bounded() {
    let http = answering(|request| {
        if request.url.ends_with("/endpoints") {
            return ok(Response::json(200, &endpoints()));
        }
        if request.method.as_str() == "GET" {
            return ok(Response::new(404, ""));
        }
        ok(Response::json(200, &without_cost("generation-1")))
    });
    let ledger = capped(Some(28.0));
    let llm = client(http, OPENROUTER, LLMPurpose::Plan, "writer");
    let result = generate(&llm, LLMPurpose::Plan, &[], 2000, Some(&ledger)).await.unwrap();
    ledger.record([result.cost.clone()]).unwrap();
    assert_eq!(result.cost.basis, CostBasis::Bounded);
    assert!(approx(result.cost.dollars.unwrap(), 2000.0 * 0.000004 * 1.05));
    assert_eq!(result.cost.input_tokens, 7);
}

#[tokio::test(start_paused = true)]
async fn test_a_price_with_no_ceiling_keeps_the_spend_guard() {
    for price in [json!("-1"), json!("free"), Value::Null] {
        let lookups = Arc::new(AtomicUsize::new(0));
        let counted = lookups.clone();
        let http = answering(move |request| {
            if request.method.as_str() == "GET" {
                counted.fetch_add(1, Ordering::SeqCst);
                let endpoint = json!({"context_length": 1000, "pricing": {"prompt": "0.000001", "completion": price}});
                return ok(Response::json(200, &json!({"data": {"endpoints": [endpoint]}})));
            }
            Box::pin(async { Err(TransportError::Timeout("ReadTimeout")) })
        });
        let ledger = capped(Some(28.0));
        let llm = client(http, OPENROUTER, LLMPurpose::Plan, "writer");
        let error = generate(&llm, LLMPurpose::Plan, &[], 2000, Some(&ledger)).await.unwrap_err();
        assert!(matches!(error, LLMError::Budget(_)) && error.to_string().contains("cannot be enforced"), "{error}");
        assert!(ledger.breakdown().has_unknown());
        // No ceiling is cached from a price that cannot hold one, so each unanswered request asks again.
        assert_eq!(lookups.load(Ordering::SeqCst), 6);
    }
}

#[test]
fn test_extra_fields_cannot_replace_the_ones_the_client_sets() {
    let extra: Map<String, Value> =
        serde_json::from_value(json!({"response_format": {}, "model": "x", "top_k": 1})).unwrap();
    let options = LLMOptions { extra_body: extra, ..LLMOptions::default() };
    let error =
        OpenAICompatibleLLM::new("", Script::new([]).http(), "http://127.0.0.1:8000/v1", HashMap::new(), options)
            .err()
            .unwrap();
    assert!(error.contains("model, response_format"), "{error}");
}

#[tokio::test]
async fn test_a_keyless_client_sends_no_authorization_and_scrubs_nothing() {
    let reply = || completion(json!(r#"{"count": 1, "local": 2}"#), Value::Null);
    let script = Script::new([reply(), reply()]);
    let options = LLMOptions { provider_routing: None, ..LLMOptions::default() };
    let models = HashMap::from([(LLMPurpose::Read, "qwen".to_string())]);
    let llm = OpenAICompatibleLLM::new("", script.http(), "http://127.0.0.1:8000/v1", models, options).unwrap();
    let error = generate(&llm, LLMPurpose::Read, &[Message::user("?")], 2000, None).await.unwrap_err().to_string();
    let requests = script.requests();
    assert!(!requests.is_empty() && requests.iter().all(|request| request.header("authorization").is_none()));
    // No placeholder key stands in for a missing one, so nothing in an error is mistaken for it and blanked.
    assert!(error.contains("local") && !error.contains("[api key]"), "{error}");
}

#[tokio::test(start_paused = true)]
async fn test_trickling_response_is_bounded_by_attempt_deadline() {
    // test_attempt_deadline.py: a body that keeps trickling resets a read timeout, so the attempt has an elapsed
    // deadline too. A transport that never finishes stands in for the trickle; the deadline drops it.
    let script = Script::new([Step::Stall]);
    let error = script.http().post_json("https://provider.test", &json!({}), &[], 0.05).await.unwrap_err();
    assert_eq!(error, TransportError::Timeout("TimeoutError"));
    assert_eq!(script.requests().len(), 1);
}

#[tokio::test]
async fn test_an_optional_object_goes_out_as_nullable_leaves_and_comes_back_whole() {
    // On 12 of 21 DeepSeek endpoints the decoder answered null for every optional object, so no read claim was cited.
    let cite = ModelTy::frozen("Cite", vec![Field::required("first", Ty::Str), Field::required("last", Ty::Str)]).ty();
    let tally =
        ModelTy::frozen("Tally", vec![Field::required("span", cite.clone()), Field::required("prefix", Ty::Str)]).ty();
    let claim = ModelTy::frozen(
        "Claim",
        vec![
            Field::required("cite", Ty::optional(cite)).doc("The blocks the claim reads."),
            Field::required("field", Ty::optional(tally)),
        ],
    )
    .ty();
    let reply = json!({"cite__first": "main/:5", "cite__last": "main/:6", "field__span__first": null,
        "field__span__last": null, "field__prefix": null});
    let script = Script::new([completion(json!(reply.to_string()), json!({}))]);
    let llm = client(script.http(), "https://llm.test/v1/", LLMPurpose::Read, "deepseek/deepseek-v4.1-flash-1017");
    let message = Message { role: Role::User, content: "Read".into(), images: vec![] };
    let result = llm.generate_value(LLMPurpose::Read, &[message], &Model(claim), 0, None).await.unwrap();
    assert_eq!(result.data, json!({"cite": {"first": "main/:5", "last": "main/:6"}, "field": null}));
    let body = &script.bodies()[0];
    let schema = &body["response_format"]["json_schema"]["schema"];
    let names: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
    assert_eq!(names, ["cite__first", "cite__last", "field__span__first", "field__span__last", "field__prefix"]);
    assert_eq!(schema["required"], json!(names));
    assert_eq!(schema["properties"]["cite__first"]["anyOf"][1], json!({"type": "null"}));
    assert_eq!(schema["properties"]["cite__first"]["description"], "The blocks the claim reads.");
    assert_eq!(body["temperature"], 0.0);
}

#[tokio::test]
async fn test_a_half_answered_optional_object_comes_back_as_none_rather_than_failing_validation() {
    // DeepSeek answered a comparison's operator without its limit, and a tally field without its span: the original
    // schema allows no object there, but not half of one, so both runs failed validation twice and stopped.
    let cite = ModelTy::frozen("Cite", vec![Field::required("first", Ty::Str), Field::required("last", Ty::Str)]).ty();
    let tally =
        ModelTy::frozen("Tally", vec![Field::required("span", cite.clone()), Field::required("prefix", Ty::Str)]).ty();
    let claim = ModelTy::frozen(
        "Claim",
        vec![Field::required("cite", Ty::optional(cite)), Field::required("field", Ty::optional(tally))],
    )
    .ty();
    let reply = json!({"cite__first": "main/:5", "cite__last": null, "field__span__first": null,
        "field__span__last": null, "field__prefix": "Total"});
    let script = Script::new([completion(json!(reply.to_string()), json!({}))]);
    let trace = Trace::collecting();
    let models = HashMap::from([(LLMPurpose::Read, "deepseek/deepseek-v4.1-flash-1017".to_string())]);
    let llm =
        OpenAICompatibleLLM::new("key", script.traced(&trace), "https://llm.test/v1/", models, LLMOptions::default())
            .unwrap();
    let message = Message { role: Role::User, content: "Read".into(), images: vec![] };
    let result = llm.generate_value(LLMPurpose::Read, &[message], &Model(claim), 0, None).await.unwrap();
    assert_eq!(result.data, json!({"cite": null, "field": null}));
    assert_eq!(script.requests().len(), 1);
    // What the reply lost is in the trace, not only in the result.
    let dropped: Vec<Value> = trace.events().into_iter().filter(|e| e["event"] == "half_objects_dropped").collect();
    assert_eq!(dropped, [json!({"event": "half_objects_dropped", "purpose": "read", "paths": ["cite", "field"]})]);
}
