//! The Responses API, which is where an OpenAI model's answer is told apart from its commentary.
//!
//! Newer OpenAI models write several assistant messages in one reply, each with a `phase` of `commentary` or
//! `final_answer`, and decode only the final answer against the schema. Chat Completions cannot represent several
//! output items, so a gateway joins their text: the commentary landed before the JSON, or the JSON twice, in 62 of 624
//! GPT-6 Luna replies. The Responses reply keeps the items apart, so the answer is the one the model marked final.
//!
//! The request and reply are translated at the edge, as [`crate::anthropic`] does for Messages, and the retries, the
//! one schema repair and the ledger stay [`crate::openai_compatible::OpenAICompatibleLLM`]'s.

use fastbrowse_core::llm::{LLMError, Message, Role};
use fastbrowse_core::py::base64;
use serde_json::{Map, Value, json};

use crate::openai_compatible::{ReasoningEffort, image_type};

/// The phase of every assistant turn fastbrowse replays: the history holds the JSON a model answered with, which is
/// its final answer. The API asks for the phase back on each assistant item, and a model that loses it stops early.
const FINAL_ANSWER: &str = "final_answer";

/// A user or system turn. A text turn is a plain string, as the Chat Completions wire writes it.
fn input_message(message: &Message) -> Result<Value, LLMError> {
    if message.images.is_empty() {
        return Ok(json!({"role": message.role.as_str(), "content": message.content}));
    }
    let mut parts = vec![json!({"type": "input_text", "text": message.content})];
    for image in &message.images {
        let url = format!("data:{};base64,{}", image_type(&image.0)?, base64::encode(&image.0));
        parts.push(json!({"type": "input_image", "image_url": url}));
    }
    Ok(json!({"role": message.role.as_str(), "content": parts}))
}

/// An assistant turn replayed as the final answer it was.
pub(crate) fn assistant_item(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text}],
        "phase": FINAL_ANSWER,
    })
}

/// A user turn the repair appends.
pub(crate) fn user_item(text: &str) -> Value {
    json!({"role": "user", "content": text})
}

/// The Responses request for one generation. `max_output_tokens` is set by the caller that routes, as `max_tokens` is
/// on the other wires. Prompt caching needs no mark on OpenAI's models, so a message's cache breakpoint is not sent.
pub(crate) fn body(
    extra: &Map<String, Value>,
    model: &str,
    messages: &[Message],
    name: &str,
    schema: &Value,
    strict: bool,
    effort: Option<ReasoningEffort>,
) -> Result<Map<String, Value>, LLMError> {
    let mut input = Vec::new();
    for message in messages {
        input.push(match message.role {
            Role::Assistant => assistant_item(&message.content),
            _ => input_message(message)?,
        });
    }
    let mut body = extra.clone();
    body.insert("model".into(), model.into());
    body.insert("input".into(), input.into());
    body.insert(
        "text".into(),
        json!({"format": {"type": "json_schema", "name": name, "schema": schema, "strict": strict}}),
    );
    // The reply is the caller's to keep or not; a stored response would outlive the run on the provider.
    body.insert("store".into(), false.into());
    if let Some(effort) = effort {
        body.insert("reasoning".into(), json!({"effort": effort.as_str()}));
    }
    Ok(body)
}

/// The text of an output message, whose content parts are one string.
fn message_text(item: &Value) -> Option<String> {
    let parts = item.get("content")?.as_array()?;
    let texts: Vec<&str> = parts
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect();
    (!texts.is_empty()).then(|| texts.concat())
}

/// The reply's answer: the last assistant message that is not commentary. A model writes its commentary before its
/// final answer, so that message is the final one, and a reply without phases has the one message. A refusal or a reply
/// with no message has no answer, which the caller asks for again and then reports.
fn answer(payload: &Map<String, Value>) -> Option<String> {
    payload
        .get("output")?
        .as_array()?
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
        .rfind(|item| item.get("phase").and_then(Value::as_str) != Some("commentary"))
        .and_then(message_text)
}

/// Why the reply stopped, in Chat Completions' words.
fn finish_reason(payload: &Map<String, Value>) -> &'static str {
    let reason = payload.get("incomplete_details").and_then(|details| details.get("reason")).and_then(Value::as_str);
    match reason {
        Some("max_output_tokens") => "length",
        Some("content_filter") => "content_filter",
        _ => "stop",
    }
}

/// A Responses reply as the chat completion it stands for, with the usage the cost line reads: input and output tokens,
/// the cached and cache-written prompt tokens, and the cost OpenRouter reports.
pub(crate) fn as_chat_completion(payload: Map<String, Value>) -> Map<String, Value> {
    let mut message = json!({"role": "assistant"});
    if let Some(text) = answer(&payload) {
        message["content"] = text.into();
    }
    let mut completion = json!({"choices": [{"message": message, "finish_reason": finish_reason(&payload)}]});
    for key in ["id", "model", "error"] {
        if let Some(value) = payload.get(key) {
            completion[key] = value.clone();
        }
    }
    if let Some(Value::Object(usage)) = payload.get("usage") {
        let mut read = Map::new();
        for (native, compatible) in [
            ("input_tokens", "prompt_tokens"),
            ("output_tokens", "completion_tokens"),
            ("cost", "cost"),
            ("is_byok", "is_byok"),
        ] {
            if let Some(value) = usage.get(native) {
                read.insert(compatible.into(), value.clone());
            }
        }
        let mut details = Map::new();
        if let Some(Value::Object(native)) = usage.get("input_tokens_details") {
            for key in ["cached_tokens", "cache_write_tokens"] {
                if let Some(count) = native.get(key).filter(|count| !count.is_null()) {
                    details.insert(key.into(), count.clone());
                }
            }
        }
        read.insert("prompt_tokens_details".into(), details.into());
        completion["usage"] = read.into();
    }
    match completion {
        Value::Object(map) => map,
        _ => unreachable!("built as an object"),
    }
}
