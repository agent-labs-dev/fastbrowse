//! FASTBROWSE_LLM_PROVIDER=anthropic: Claude through the Messages API, with the schema as structured output, the key
//! as `x-api-key`, and the reply read through the same retries, repair and accounting as every other LLM.

mod common;

use common::{Script, Step, answer_model};
use fastbrowse_clients::anthropic::messages_schema;
use fastbrowse_clients::environment::{ConfigurationError, Settings};
use fastbrowse_clients::http::{Request, Response};
use fastbrowse_core::llm::{Generation, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{Bytes, CostBasis, LLMPurpose};
use serde_json::{Value, json};

const ENV: [(&str, &str); 2] = [("FASTBROWSE_LLM_PROVIDER", "anthropic"), ("ANTHROPIC_API_KEY", "anthropic-secret")];

fn settings(env: &[(&str, &str)]) -> Settings {
    Settings::from_sources(env, None).unwrap()
}

fn config_error<T>(built: Result<T, ConfigurationError>) -> String {
    match built {
        Ok(_) => panic!("expected a configuration error"),
        Err(error) => error.to_string(),
    }
}

fn reply(content: Value, stop_reason: &str) -> Step {
    Step::Reply(Response::json(
        200,
        &json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": content,
            "stop_reason": stop_reason,
            "usage": {"input_tokens": 3, "cache_read_input_tokens": 4, "output_tokens": 2},
        }),
    ))
}

fn answer(text: &str) -> Step {
    reply(json!([{"type": "thinking", "thinking": ""}, {"type": "text", "text": text}]), "end_turn")
}

async fn generate(
    env: &[(&str, &str)],
    messages: &[Message],
    replies: Vec<Step>,
) -> (Vec<Request>, Result<Generation<Value>, LLMError>) {
    let script = Script::new(replies);
    let llm = settings(env).llm(&script.http()).unwrap();
    let result = llm.generate_value(LLMPurpose::Read, messages, &answer_model(), None).await;
    (script.requests(), result)
}

fn body(request: &Request) -> Value {
    serde_json::from_slice(&request.body).unwrap()
}

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";

#[tokio::test]
async fn test_a_generation_is_a_messages_request_with_structured_output() {
    let messages =
        [Message::system("Plan."), Message { images: vec![Bytes(PNG.to_vec())], ..Message::user("What is shown?") }];
    let (seen, result) = generate(&ENV, &messages, vec![answer(r#"{"answer":"ok"}"#)]).await;
    assert_eq!(result.unwrap().data["answer"], "ok");
    let request = &seen[0];
    assert_eq!(request.url, "https://api.anthropic.com/v1/messages");
    assert_eq!(request.header("x-api-key"), Some("anthropic-secret"));
    assert_eq!(request.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(request.header("authorization"), None);
    let sent = body(request);
    assert_eq!(sent["model"], "claude-sonnet-5-5");
    assert_eq!(sent["max_tokens"], 128_000, "the documented output maximum the API requires");
    assert_eq!(sent["system"], "Plan.");
    assert_eq!(
        sent["messages"],
        json!([{"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgpyZXN0"}},
            {"type": "text", "text": "What is shown?"},
        ]}])
    );
    let format = &sent["output_config"]["format"];
    assert_eq!(format["type"], "json_schema");
    assert_eq!(format["schema"]["additionalProperties"], false);
    for absent in ["response_format", "provider", "reasoning", "reasoning_effort", "max_completion_tokens"] {
        assert!(sent.get(absent).is_none(), "sent {absent}");
    }
    assert!(sent["output_config"].get("effort").is_none(), "an unset effort leaves the model's own default");
}

#[tokio::test]
async fn test_the_key_goes_to_anthropic_alone() {
    let env = [
        ENV.as_slice(),
        &[
            ("OPENROUTER_API_KEY", "openrouter-secret"),
            ("AI_GATEWAY_API_KEY", "gateway-secret"),
            ("FASTBROWSE_LLM_API_KEY", "local-secret"),
            ("OPENAI_API_KEY", "openai-secret"),
        ],
    ]
    .concat();
    let (seen, _) = generate(&env, &[Message::user("?")], vec![answer(r#"{"answer":"ok"}"#)]).await;
    let sent = format!("{:?}{}", seen[0].headers, String::from_utf8_lossy(&seen[0].body));
    for other in ["openrouter-secret", "gateway-secret", "local-secret", "openai-secret"] {
        assert!(!sent.contains(other), "carried {other}");
    }
}

#[tokio::test]
async fn test_an_error_that_echoes_the_key_is_scrubbed() {
    let refused = Step::Reply(Response::json(401, &json!({"error": {"message": "bad key anthropic-secret"}})));
    let (_, result) = generate(&ENV, &[Message::user("?")], vec![refused]).await;
    let error = result.unwrap_err().to_string();
    assert!(!error.contains("anthropic-secret") && error.contains("[api key]"), "{error}");
}

#[tokio::test]
async fn test_cost_counts_cached_input_and_stays_unknown_without_a_price() {
    let (seen, result) = generate(&ENV, &[Message::user("?")], vec![answer(r#"{"answer":"ok"}"#)]).await;
    // No receipt or price lookup: those belong to OpenRouter and the gateway.
    assert_eq!(seen.len(), 1);
    let cost = result.unwrap().cost;
    assert_eq!((cost.basis, cost.dollars), (CostBasis::Unknown, None));
    assert_eq!((cost.input_tokens, cost.output_tokens), (7, 2));
}

#[tokio::test]
async fn test_a_reply_cut_at_max_tokens_is_reported_as_cut_off() {
    let cut = reply(json!([{"type": "text", "text": "{\"ans"}]), "max_tokens");
    let (seen, result) = generate(&ENV, &[Message::user("?")], vec![cut, answer(r#"{"answer":"ok"}"#)]).await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("cut off before its JSON ended"), "{error}");
    assert_eq!(seen.len(), 1);
}

#[tokio::test]
async fn test_a_schema_miss_is_repaired_once_in_the_same_conversation() {
    let (seen, result) =
        generate(&ENV, &[Message::user("?")], vec![answer(r#"{"other":1}"#), answer(r#"{"answer":"ok"}"#)]).await;
    assert_eq!(result.unwrap().data["answer"], "ok");
    let messages = body(&seen[1])["messages"].as_array().unwrap().clone();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1], json!({"role": "assistant", "content": r#"{"other":1}"#}));
    assert_eq!(messages[2]["role"], "user");
}

#[tokio::test]
async fn test_a_refusal_is_asked_again_and_then_reported() {
    let refusal = || reply(json!([{"type": "text", "text": "I can't help with that."}]), "refusal");
    let (seen, result) = generate(&ENV, &[Message::user("?")], vec![refusal(), refusal()]).await;
    assert_eq!(seen.len(), 2);
    let error = result.unwrap_err().to_string();
    assert!(error.contains("refusal"), "{error}");
}

#[tokio::test]
async fn test_the_reasoning_setting_becomes_the_effort() {
    let env = [ENV.as_slice(), &[("FASTBROWSE_LLM_REASONING", "high")]].concat();
    let (seen, _) = generate(&env, &[Message::user("?")], vec![answer(r#"{"answer":"ok"}"#)]).await;
    assert_eq!(body(&seen[0])["output_config"]["effort"], "high");
    let off = [ENV.as_slice(), &[("FASTBROWSE_LLM_REASONING", "none")]].concat();
    let error = config_error(settings(&off).llm(&Script::default().http()));
    assert!(error.contains("not none"), "{error}");
}

#[test]
fn test_an_extra_body_cannot_replace_the_messages_fields() {
    let env = [ENV.as_slice(), &[("FASTBROWSE_LLM_EXTRA_BODY", r#"{"system": "x", "output_config": {}}"#)]].concat();
    let error = config_error(settings(&env).llm(&Script::default().http()));
    assert!(error.contains("output_config") && error.contains("system"), "{error}");
}

#[test]
fn test_defaults_and_pins() {
    let providers = settings(&ENV).providers();
    assert!(providers.contains("LLM claude-haiku-5-5, claude-sonnet-5-5 via Anthropic"), "{providers}");
    let pinned = settings(&[ENV.as_slice(), &[("FASTBROWSE_LLM_MODEL", "claude-opus-5-5")]].concat());
    assert!(pinned.providers().contains("LLM claude-opus-5-5 via Anthropic"), "{}", pinned.providers());
}

#[test]
fn test_the_schema_sent_drops_what_structured_outputs_refuses() {
    let schema = json!({
        "type": "object",
        "properties": {
            "minimum": {"type": "integer", "minimum": 1, "maximum": 5},
            "name": {"type": "string", "minLength": 1, "maxLength": 9},
            "one": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 3},
            "pair": {"type": "array", "prefixItems": [{"type": "string"}, {"type": "integer"}], "minItems": 2},
        },
        "required": ["minimum", "name", "one", "pair"],
        "additionalProperties": false,
    });
    assert_eq!(
        messages_schema(&schema),
        json!({
            "type": "object",
            "properties": {
                // A property may be named like a keyword; only keywords are dropped.
                "minimum": {"type": "integer"},
                "name": {"type": "string"},
                "one": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                "pair": {"type": "array", "items": {"anyOf": [{"type": "string"}, {"type": "integer"}]}},
            },
            "required": ["minimum", "name", "one", "pair"],
            "additionalProperties": false,
        })
    );
    // The model's own schema still judges the reply, so a dropped constraint is enforced there.
    assert!(answer_model().validate_json(r#"{"answer": 1}"#).is_err());
}
