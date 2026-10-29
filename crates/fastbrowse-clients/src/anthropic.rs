//! The Claude Messages API, for the structured outputs Anthropic's OpenAI-compatible endpoint ignores.
//!
//! The request and response are translated at the edge, and everything between (retries, the one schema repair, the
//! truncation retry and the ledger) is [`OpenAICompatibleLLM`]'s: a Messages reply is read as the chat completion it
//! stands for, so the two clients cannot drift apart in how they bill or give up.

use std::collections::HashMap;

use fastbrowse_core::BoxFuture;
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema, Role};
use fastbrowse_core::models::LLMPurpose;
use fastbrowse_core::py::base64;
use fastbrowse_core::telemetry::Ledger;
use serde_json::{Map, Value, json};

use crate::http::Http;
use crate::openai_compatible::{LLMOptions, OpenAICompatibleLLM, ReasoningEffort, Wire, image_type};

/// The API version every request names; the Messages API refuses a request without one.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Request fields the Messages body sets itself, which an extra body may not replace.
pub const RESERVED_MESSAGES_KEYS: [&str; 5] = ["max_tokens", "messages", "model", "output_config", "system"];

/// Keywords structured outputs answers HTTP 400 to. The schema is still validated in full against the reply, so a
/// value they would have excluded is caught there and repaired, as any other schema miss is.
const UNSUPPORTED_KEYWORDS: [&str; 12] = [
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "maxItems",
    "uniqueItems",
    "minProperties",
    "maxProperties",
    "contains",
];

/// A schema structured outputs accepts: unsupported constraints dropped, `minItems` kept only at 0 or 1, and a
/// fixed-length tuple read as a list of any of its members.
pub fn messages_schema(schema: &Value) -> Value {
    match schema {
        Value::Array(items) => Value::Array(items.iter().map(messages_schema).collect()),
        Value::Object(map) => {
            let mut result = Map::new();
            for (key, value) in map {
                let names = key == "properties" || key == "$defs";
                if !names && UNSUPPORTED_KEYWORDS.contains(&key.as_str()) {
                    continue;
                }
                if key == "minItems" && value.as_i64().is_some_and(|count| count > 1) {
                    continue;
                }
                if key == "prefixItems" {
                    let members = value.as_array().cloned().unwrap_or_default();
                    result.insert("items".into(), json!({"anyOf": messages_schema(&Value::Array(members))}));
                    continue;
                }
                let translated = match value {
                    Value::Object(entries) if names => Value::Object(
                        entries.iter().map(|(name, item)| (name.clone(), messages_schema(item))).collect(),
                    ),
                    other => messages_schema(other),
                };
                result.insert(key.clone(), translated);
            }
            Value::Object(result)
        }
        other => other.clone(),
    }
}

fn content(message: &Message) -> Result<Value, LLMError> {
    if message.images.is_empty() {
        return Ok(message.content.clone().into());
    }
    let mut blocks = Vec::new();
    for image in &message.images {
        let media_type = image_type(&image.0)?;
        blocks.push(json!({
            "type": "image",
            "source": {"type": "base64", "media_type": media_type, "data": base64::encode(&image.0)},
        }));
    }
    blocks.push(json!({"type": "text", "text": message.content}));
    Ok(Value::Array(blocks))
}

/// The Messages request for one generation. System messages become the top-level `system`, as the API takes them.
pub(crate) fn body(
    extra: &Map<String, Value>,
    model: &str,
    messages: &[Message],
    schema: &Value,
    max_output_tokens: i64,
    effort: Option<ReasoningEffort>,
) -> Result<Map<String, Value>, LLMError> {
    let system: Vec<&str> =
        messages.iter().filter(|message| message.role == Role::System).map(|m| m.content.as_str()).collect();
    let mut turns = Vec::new();
    for message in messages.iter().filter(|message| message.role != Role::System) {
        turns.push(json!({"role": message.role.as_str(), "content": content(message)?}));
    }
    let mut output_config = json!({"format": {"type": "json_schema", "schema": messages_schema(schema)}});
    if let Some(effort) = effort {
        output_config["effort"] = effort.as_str().into();
    }
    let mut body = extra.clone();
    body.insert("model".into(), model.into());
    // Zero is the caller saying no ceiling; the provider's own default applies. fastbrowse never caps output tokens.
    if max_output_tokens > 0 {
        body.insert("max_tokens".into(), max_output_tokens.into());
    } else {
        body.remove("max_tokens");
    }
    if !system.is_empty() {
        body.insert("system".into(), system.join("\n\n").into());
    }
    body.insert("messages".into(), turns.into());
    body.insert("output_config".into(), output_config);
    Ok(body)
}

fn tokens(usage: &Map<String, Value>, key: &str) -> i64 {
    usage.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// A Messages reply as the chat completion it stands for: the text blocks as the message, `max_tokens` as a `length`
/// stop, and every input token, cached or not, as prompt tokens. A refusal carries no message, so it is asked again
/// and then reported, as an empty completion is.
pub(crate) fn as_chat_completion(payload: Map<String, Value>) -> Map<String, Value> {
    let stop_reason = payload.get("stop_reason").and_then(Value::as_str).unwrap_or_default().to_string();
    let texts: Vec<&str> = payload
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    let mut message = json!({"role": "assistant"});
    if stop_reason != "refusal" && !texts.is_empty() {
        message["content"] = texts.concat().into();
    }
    let finish_reason = if stop_reason == "max_tokens" { "length" } else { "stop" };
    let mut completion = json!({"choices": [{"message": message, "finish_reason": finish_reason}]});
    if let Some(id) = payload.get("id") {
        completion["id"] = id.clone();
    }
    if let Some(Value::Object(usage)) = payload.get("usage") {
        let prompt = ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"]
            .iter()
            .map(|key| tokens(usage, key))
            .sum::<i64>();
        completion["usage"] = json!({"prompt_tokens": prompt, "completion_tokens": tokens(usage, "output_tokens")});
    }
    match completion {
        Value::Object(map) => map,
        _ => unreachable!("built as an object"),
    }
}

/// Claude through the Messages API with structured outputs.
pub struct AnthropicLLM(OpenAICompatibleLLM);

impl AnthropicLLM {
    /// `effort` goes to `output_config.effort`; None leaves each model's own default. A Claude model cannot have its
    /// thinking turned off on every model, so `ReasoningEffort::None` is refused here rather than per request.
    pub fn new(
        api_key: impl Into<String>,
        http: Http,
        base_url: &str,
        models: HashMap<LLMPurpose, String>,
        effort: Option<ReasoningEffort>,
        purpose_effort: HashMap<LLMPurpose, ReasoningEffort>,
        extra_body: Map<String, Value>,
    ) -> Result<AnthropicLLM, String> {
        if effort == Some(ReasoningEffort::None) || purpose_effort.values().any(|e| *e == ReasoningEffort::None) {
            return Err("Claude takes a reasoning effort of low, medium or high, not none".into());
        }
        let clashes: Vec<&str> =
            RESERVED_MESSAGES_KEYS.into_iter().filter(|key| extra_body.contains_key(*key)).collect();
        if !clashes.is_empty() {
            return Err(format!("extra request fields replace ones fastbrowse sets: {}", clashes.join(", ")));
        }
        let options = LLMOptions {
            reasoning_effort: effort,
            purpose_reasoning: purpose_effort,
            provider_routing: None,
            ..LLMOptions::default()
        };
        OpenAICompatibleLLM::with_wire(api_key, http, base_url, models, options, Wire::AnthropicMessages, extra_body)
            .map(AnthropicLLM)
    }
}

impl LLMClient for AnthropicLLM {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        max_output_tokens: i64,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        self.0.generate_value(purpose, messages, schema, max_output_tokens, ledger)
    }
}
