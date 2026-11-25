//! OpenAI models through the Responses API: a reply with commentary and a final answer is read as the final answer, which
//! Chat Completions cannot do because it joins the two, and the request, repair and cost line stay what every other LLM
//! client's are.

mod common;

use common::{Script, Step, answer_model, body, reply, settings};
use fastbrowse_clients::http::Request;
use fastbrowse_core::llm::{Generation, LLMError, Message, Role};
use fastbrowse_core::models::{Bytes, CostBasis, LLMPurpose};
use serde_json::{Value, json};

const OPENROUTER: [(&str, &str); 1] = [("OPENROUTER_API_KEY", "openrouter-secret")];
const OPENAI: [(&str, &str); 3] = [
    ("FASTBROWSE_LLM_PROVIDER", "openai"),
    ("OPENAI_API_KEY", "openai-secret"),
    ("FASTBROWSE_LLM_MODEL", "gpt-6-luna"),
];

fn message(phase: Option<&str>, text: &str) -> Value {
    let mut item = json!({
        "type": "message",
        "role": "assistant",
        "status": "completed",
        "content": [{"type": "output_text", "text": text, "annotations": []}],
    });
    if let Some(phase) = phase {
        item["phase"] = phase.into();
    }
    item
}

fn response(output: Vec<Value>) -> Step {
    reply(
        200,
        json!({
            "id": "gen-1",
            "object": "response",
            "model": "openai/gpt-6-luna",
            "status": "completed",
            "incomplete_details": null,
            "output": output,
            "usage": {
                "input_tokens": 89,
                "input_tokens_details": {"cached_tokens": 64},
                "output_tokens": 40,
                "output_tokens_details": {"reasoning_tokens": 22},
                "cost": 2.89e-05,
            },
        }),
    )
}

fn answer(text: &str) -> Step {
    response(vec![json!({"type": "reasoning", "summary": []}), message(Some("final_answer"), text)])
}

async fn generate(
    env: &[(&str, &str)],
    messages: &[Message],
    replies: Vec<Step>,
) -> (Vec<Request>, Result<Generation<Value>, LLMError>) {
    let script = Script::new(replies);
    let llm = settings(env).llm(&script.http()).unwrap();
    let result = llm.generate_value(LLMPurpose::Action, messages, &answer_model(), None).await;
    (script.requests(), result)
}

#[tokio::test]
async fn test_an_openai_model_through_openrouter_is_a_responses_request() {
    let messages = [
        Message::system("Plan."),
        Message::user("What is shown?").cached(),
        Message { images: vec![Bytes(b"\x89PNG\r\n\x1a\nrest".to_vec())], ..Message::user("And this?") },
    ];
    let (seen, result) = generate(&OPENROUTER, &messages, vec![answer(r#"{"answer":"ok"}"#)]).await;
    let result = result.unwrap();
    assert_eq!(result.data["answer"], "ok");
    assert_eq!(result.cost.dollars, Some(2.89e-05));
    assert_eq!(result.cost.basis, CostBasis::Metered);
    assert_eq!(
        (result.cost.input_tokens, result.cost.output_tokens, result.cost.cache_read_tokens),
        (89, 40, Some(64))
    );
    assert_eq!(result.cost.model.as_deref(), Some("openai/gpt-6-luna"));
    assert_eq!(seen.len(), 1, "the cost is on the reply, so no receipt lookup follows");
    let request = &seen[0];
    assert_eq!(request.url, "https://openrouter.ai/api/v1/responses");
    assert_eq!(request.header("authorization"), Some("Bearer openrouter-secret"));
    let sent = body(request);
    assert_eq!(sent["model"], "openai/gpt-6-luna");
    assert_eq!(
        sent["input"],
        json!([
            {"role": "system", "content": "Plan."},
            {"role": "user", "content": "What is shown?"},
            {"role": "user", "content": [
                {"type": "input_text", "text": "And this?"},
                {"type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgpyZXN0"},
            ]},
        ])
    );
    let format = &sent["text"]["format"];
    assert_eq!(
        (&format["type"], &format["name"], &format["strict"]),
        (&json!("json_schema"), &json!("Answer"), &json!(true))
    );
    assert_eq!(format["schema"]["additionalProperties"], false);
    assert_eq!(sent["reasoning"], json!({"effort": "low"}));
    assert_eq!(sent["max_output_tokens"], 32_768);
    assert_eq!(sent["provider"]["order"], json!(["openai"]));
    assert_eq!(sent["provider"]["require_parameters"], true);
    for absent in ["messages", "response_format", "max_tokens", "max_completion_tokens"] {
        assert!(sent.get(absent).is_none(), "sent {absent}");
    }
}

#[tokio::test]
async fn test_the_final_answer_is_read_and_the_commentary_before_it_is_not() {
    let reply = response(vec![
        message(Some("commentary"), "The homepage lists Travel as a category. I'll open it."),
        json!({"type": "reasoning", "summary": []}),
        message(Some("final_answer"), r#"{"answer":"travel"}"#),
    ]);
    let (_, result) = generate(&OPENROUTER, &[Message::user("?")], vec![reply]).await;
    assert_eq!(result.unwrap().data["answer"], "travel");
}

#[tokio::test]
async fn test_chat_completions_joining_the_two_messages_is_not_json() {
    // What a gateway makes of the reply above: the commentary and the answer as one string.
    let joined = "The homepage lists Travel as a category. I'll open it.\n\n{\"answer\":\"travel\"}";
    let completion = || {
        reply(
            200,
            json!({"choices": [{"message": {"content": joined}, "finish_reason": "stop"}], "usage": {"cost": 0.0}}),
        )
    };
    let env = [OPENROUTER.as_slice(), &[("FASTBROWSE_LLM_MODEL", "google/gemini-3.8-flash")]].concat();
    let (seen, result) = generate(&env, &[Message::user("?")], vec![completion(), completion()]).await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("schema validation failed after one retry"), "{error}");
    assert!(seen[0].url.ends_with("/chat/completions"));
}

#[tokio::test]
async fn test_a_reply_without_phases_is_its_one_message() {
    let (_, result) =
        generate(&OPENROUTER, &[Message::user("?")], vec![response(vec![message(None, r#"{"answer":"ok"}"#)])]).await;
    assert_eq!(result.unwrap().data["answer"], "ok");
}

#[tokio::test]
async fn test_a_reply_with_only_commentary_has_no_answer_and_is_asked_once_more() {
    let commentary = || response(vec![message(Some("commentary"), r#"{"answer":"not final"}"#)]);
    let (seen, result) =
        generate(&OPENROUTER, &[Message::user("?")], vec![commentary(), answer(r#"{"answer":"ok"}"#)]).await;
    assert_eq!(result.unwrap().data["answer"], "ok");
    assert_eq!(seen.len(), 2);
    let (_, result) = generate(&OPENROUTER, &[Message::user("?")], vec![commentary(), commentary()]).await;
    assert!(result.unwrap_err().to_string().contains("Invalid completion envelope"));
}

#[tokio::test]
async fn test_a_schema_miss_is_repaired_with_the_answer_replayed_as_final() {
    let (seen, result) =
        generate(&OPENROUTER, &[Message::user("?")], vec![answer(r#"{"other":1}"#), answer(r#"{"answer":"ok"}"#)])
            .await;
    assert_eq!(result.unwrap().data["answer"], "ok");
    let input = body(&seen[1])["input"].as_array().unwrap().clone();
    assert_eq!(input.len(), 3);
    assert_eq!(
        input[1],
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": r#"{"other":1}"#}], "phase": "final_answer"})
    );
    assert_eq!(input[2]["role"], "user");
    assert!(input[2]["content"].as_str().unwrap().starts_with("Correct the JSON to match the schema."));
    assert!(body(&seen[1]).get("messages").is_none());
}

#[tokio::test]
async fn test_an_assistant_turn_in_the_history_keeps_its_phase() {
    let history = [Message::user("?"), Message::new(Role::Assistant, r#"{"answer":"first"}"#), Message::user("again")];
    let (seen, _) = generate(&OPENROUTER, &history, vec![answer(r#"{"answer":"ok"}"#)]).await;
    let sent = body(&seen[0]);
    assert_eq!(sent["input"][1]["type"], "message");
    assert_eq!(sent["input"][1]["phase"], "final_answer");
    assert_eq!(sent["input"][1]["content"][0]["text"], r#"{"answer":"first"}"#);
}

#[tokio::test]
async fn test_a_reply_cut_at_the_output_cap_is_asked_once_more_then_reported_as_cut_off() {
    let cut = || {
        reply(
            200,
            json!({
                "id": "gen-1",
                "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"},
                "output": [message(Some("final_answer"), "{\"ans")],
                "usage": {"input_tokens": 1, "output_tokens": 1, "cost": 0.0},
            }),
        )
    };
    let (seen, result) = generate(&OPENROUTER, &[Message::user("?")], vec![cut(), cut()]).await;
    assert!(result.unwrap_err().to_string().contains("cut off before its JSON ended"));
    assert_eq!(seen.len(), 2);
}

#[tokio::test]
async fn test_a_model_from_another_provider_stays_on_chat_completions() {
    let env = [OPENROUTER.as_slice(), &[("FASTBROWSE_LLM_MODEL", "google/gemini-3.8-flash")]].concat();
    let completion = reply(
        200,
        json!({"choices": [{"message": {"content": "{\"answer\":\"ok\"}"}, "finish_reason": "stop"}], "usage": {"cost": 0.0}}),
    );
    let (seen, result) = generate(&env, &[Message::user("?")], vec![completion]).await;
    assert_eq!(result.unwrap().data["answer"], "ok");
    assert_eq!(seen[0].url, "https://openrouter.ai/api/v1/chat/completions");
}

#[tokio::test]
async fn test_openai_direct_is_a_responses_request_without_routing() {
    let (seen, result) = generate(&OPENAI, &[Message::user("?")], vec![answer(r#"{"answer":"ok"}"#)]).await;
    assert_eq!(result.unwrap().data["answer"], "ok");
    let request = &seen[0];
    assert_eq!(request.url, "https://api.openai.com/v1/responses");
    assert_eq!(request.header("authorization"), Some("Bearer openai-secret"));
    let sent = body(request);
    assert_eq!(sent["model"], "gpt-6-luna");
    for absent in ["provider", "max_output_tokens", "reasoning", "messages", "response_format"] {
        assert!(sent.get(absent).is_none(), "sent {absent}");
    }
    assert_eq!(sent["store"], false);
}

#[tokio::test]
async fn test_openai_direct_takes_the_effort_as_a_reasoning_object() {
    let env = [OPENAI.as_slice(), &[("FASTBROWSE_LLM_REASONING", "medium")]].concat();
    let (seen, _) = generate(&env, &[Message::user("?")], vec![answer(r#"{"answer":"ok"}"#)]).await;
    assert_eq!(body(&seen[0])["reasoning"], json!({"effort": "medium"}));
}

#[tokio::test]
async fn test_a_failed_reply_is_asked_once_more_then_reported_with_the_providers_error() {
    let failed = || {
        reply(
            200,
            json!({
                "id": "gen-1",
                "status": "failed",
                "error": {"code": "server_error", "message": "The model failed to generate a response."},
                "output": [],
                "usage": {"input_tokens": 1, "output_tokens": 0, "cost": 0.0},
            }),
        )
    };
    let (seen, result) = generate(&OPENROUTER, &[Message::user("?")], vec![failed(), failed()]).await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("the provider reported server_error"), "{error}");
    assert_eq!(seen.len(), 2);
}
