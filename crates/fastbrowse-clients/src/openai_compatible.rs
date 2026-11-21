//! Structured chat completions with one schema repair and complete usage
//! accounting.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use fastbrowse_core::BoxFuture;
use fastbrowse_core::llm::{Fault, FaultClass, Generation, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{CostBasis, CostComponent, CostLine, LLMPurpose, combined_basis};
use fastbrowse_core::py::{self, base64, text};
use fastbrowse_core::schema::{flat_optional_objects, nest_flat_objects, strict_schema};
use fastbrowse_core::telemetry::Ledger;
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::anthropic::{self, ANTHROPIC_VERSION};
use crate::http::{Http, Response, scrub};
use crate::providers::LLMProvider;
use crate::validation::{
    Attempts, LLM_ATTEMPT_SECONDS, RetryStop, Usage, body_excerpt, describe, dollars, errors_json, json_invalid,
    json_object, known_total, object_value, post_with_retry, retryable, token_count, with_discarded,
};

/// OpenRouter routing: an endpoint that ignores response_format would treat the schema as a hint, so none is routed
/// to. OpenRouter's default routing sent gemini-3.8-flash reads to Vertex at a 2.4s median where AI Studio answered the
/// same read in 1.3s; sorting by latency keeps fallbacks and follows the faster endpoint.
pub fn openrouter_provider_routing() -> Value {
    json!({"require_parameters": true, "sort": "latency"})
}

/// The OpenRouter routing a request for `model` sends: [`openrouter_provider_routing`], with the endpoints a model was
/// measured on pinned. The eval record proxy sends this too, through `fastbrowse.routing`, so a cassette holds answers
/// from the endpoints a run is routed to.
pub fn openrouter_routing_for(model: &str) -> Value {
    let mut routing = openrouter_provider_routing();
    pin_endpoints(&mut routing, model);
    routing
}

/// Sorting by latency sent DeepSeek V4.1 Flash composes to whichever of about 50 endpoints was fastest, and Together,
/// the one it chose most, changed words the model copied: the same compose request at temperature 0 named "Clerkwell
/// Coffee" or "GBP 9.99" in 3 to 6 of 100 replays there. Parasail, DeepInfra and InferenceNet wrote it correctly 100 of
/// 100 times each, Parasail at Together's 1.2 s median, so they are tried in that order, and Together is never a
/// fallback. OpenRouter tries an `order` before its `sort`. Another model keeps the latency sort: a pin is a
/// measurement of that model's endpoints, and each response names the provider that served it.
fn pin_endpoints(routing: &mut Value, model: &str) {
    if deepseek(model) {
        routing["order"] = json!(["parasail", "deepinfra", "inference-net"]);
        routing["ignore"] = json!(["together"]);
    }
}

/// DeepSeek is served by many OpenRouter providers, and most of them null every optional object (see
/// [`flat_optional_objects`]). At its default temperature it also repeated `"true"` to the output cap on
/// Together; at 0 it did not. Other models keep their schema as written and their default temperature, which Gemini 3 is
/// documented to need.
fn deepseek(model: &str) -> bool {
    model.starts_with("deepseek/")
}

/// The Vercel AI Gateway documents `ttft` for the lowest time to first token where OpenRouter calls that sort
/// `latency`. It has no `require_parameters`: a provider that cannot hold a reply to `response_format` still serves
/// the request, and the reply may not match the schema. DeepSeek V4.1 Flash failed schema validation in five of six
/// live runs through the gateway even with `only` set to providers that list structured outputs, answering `type` where
/// the schema says `kind`. The request therefore also states the schema in the prompt.
pub fn gateway_provider_routing() -> Value {
    json!({"sort": "ttft"})
}

/// The output cap a routed request sends. Every fastbrowse reply is a few thousand tokens at most, reasoning included,
/// and every endpoint of the default models lists 64k or more, so it cuts only a degenerate generation: one DeepSeek
/// compose call ran to 131,072 tokens over six minutes before it ended (#749). Another server gets no cap, since it may
/// refuse the field.
pub const ROUTED_OUTPUT_TOKENS: i64 = 32_768;

/// Request fields fastbrowse sets itself, and the output caps. An extra body that named one would replace the schema or
/// the messages, or change where a reply is cut, so the run would fail far from the setting that caused it.
pub const RESERVED_BODY_KEYS: [&str; 7] =
    ["max_completion_tokens", "max_tokens", "messages", "model", "provider", "reasoning", "response_format"];

/// The reserved keys `extra` names, sorted.
pub fn reserved_clashes(extra: &Map<String, Value>) -> Vec<&'static str> {
    RESERVED_BODY_KEYS.into_iter().filter(|key| extra.contains_key(*key)).collect()
}

/// The media type of a PNG or JPEG, the two image formats every client sends.
pub(crate) fn image_type(content: &[u8]) -> Result<&'static str, LLMError> {
    if content.starts_with(b"\x89PNG\r\n\x1a\n") {
        Ok("image/png")
    } else if content.starts_with(b"\xff\xd8\xff") {
        Ok("image/jpeg")
    } else {
        Err(LLMError::failed(FaultClass::Request, "Only PNG and JPEG images are supported"))
    }
}

fn image_url(content: &[u8]) -> Result<String, LLMError> {
    Ok(format!("data:{};base64,{}", image_type(content)?, base64::encode(content)))
}

/// A message on the Chat Completions wire. `breakpoints` says whether the endpoint takes `cache_control` on a part.
fn wire_message(message: &Message, breakpoints: bool) -> Result<Value, LLMError> {
    let cached = breakpoints && message.cache_through;
    if message.images.is_empty() && !cached {
        return Ok(json!({"role": message.role.as_str(), "content": message.content}));
    }
    let mut parts = vec![json!({"type": "text", "text": message.content})];
    for image in &message.images {
        parts.push(json!({"type": "image_url", "image_url": {"url": image_url(&image.0)?}}));
    }
    if cached && let Some(last) = parts.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
    Ok(json!({"role": message.role.as_str(), "content": parts}))
}

/// The schema stated in the prompt, for an endpoint that cannot be given it as `response_format` or a route that may not
/// hold the reply to it. JSON mode on such endpoints (DeepSeek's) also refuses a prompt that never says "json".
fn schema_instruction(schema: &Value) -> String {
    format!("Reply with one JSON object, and nothing else, that matches this JSON Schema:\n{schema}")
}

/// A reply without JSON mode can wrap its object in a Markdown fence; the fence is not the model's answer.
fn unfenced(content: &str) -> &str {
    let trimmed = content.trim();
    let Some(inner) = trimmed.strip_prefix("```").and_then(|rest| rest.strip_suffix("```")) else { return content };
    inner.strip_prefix("json").unwrap_or(inner).trim()
}

fn first_choice(payload: &Map<String, Value>) -> Result<Option<&Map<String, Value>>, String> {
    match payload.get("choices").and_then(Value::as_array) {
        Some(choices) if !choices.is_empty() => object_value(choices.first()).map(Some),
        _ => Ok(None),
    }
}

fn content(payload: &Map<String, Value>) -> Result<String, String> {
    let choice = first_choice(payload)?.ok_or("missing completion choices")?;
    let message = object_value(choice.get("message"))?;
    match message.get("content") {
        Some(Value::String(content)) => Ok(content.clone()),
        _ => Err("missing completion text (possibly a refusal)".into()),
    }
}

/// The model ran out of output tokens: what it wrote is cut off mid-value and says nothing of its schema.
fn truncated(payload: &Map<String, Value>) -> Result<bool, String> {
    Ok(first_choice(payload)?
        .is_some_and(|choice| choice.get("finish_reason").and_then(Value::as_str) == Some("length")))
}

/// A response the model or provider cut off, named as truncation so it is never read as a bad answer.
///
/// This is the routed output cap, the model's own output maximum or a provider that dropped the tail. A cut-off reply is
/// asked once more: the runaways measured were a provider's decoder writing whitespace inside the JSON
/// or one paragraph of reasoning repeated until the maximum, and the same request asked again answered.
fn truncation(purpose: LLMPurpose) -> LLMError {
    LLMError::failed(FaultClass::Truncated, format!("LLM response was cut off before its JSON ended ({purpose})"))
}

/// Never fails: usage we cannot read is an unknown cost, which a dollar cap treats as unaffordable.
fn cost(payload: &Map<String, Value>, purpose: LLMPurpose) -> CostLine {
    let read = || -> Result<CostLine, String> {
        let empty = Value::Object(Map::new());
        let usage = object_value(Some(payload.get("usage").unwrap_or(&empty)))?;
        let cost = usage.get("cost").filter(|cost| !cost.is_null());
        let zero = Value::from(0);
        Ok(CostLine {
            component: CostComponent::Llm,
            purpose: Some(purpose),
            decision_purpose: None,
            basis: if cost.is_none() { CostBasis::Unknown } else { CostBasis::Metered },
            dollars: cost.map(|cost| dollars(Some(cost))).transpose()?,
            input_tokens: token_count(Some(usage.get("prompt_tokens").unwrap_or(&zero)))?,
            output_tokens: token_count(Some(usage.get("completion_tokens").unwrap_or(&zero)))?,
            seconds: None,
            physical_requests: None,
            images: None,
            cache_read_tokens: cache_tokens(usage, "cached_tokens")?,
            cache_write_tokens: cache_tokens(usage, "cache_write_tokens")?,
            model: payload.get("model").and_then(Value::as_str).filter(|model| !model.is_empty()).map(str::to_string),
        })
    };
    read().unwrap_or_else(|_| unknown(purpose))
}

/// A prompt-cache count from `usage.prompt_tokens_details`, where OpenAI-compatible APIs report it; None when the
/// provider leaves it out.
fn cache_tokens(usage: &Map<String, Value>, key: &str) -> Result<Option<i64>, String> {
    let details = usage.get("prompt_tokens_details").and_then(Value::as_object);
    details
        .and_then(|details| details.get(key))
        .filter(|count| !count.is_null())
        .map(|count| token_count(Some(count)))
        .transpose()
}

fn unknown(purpose: LLMPurpose) -> CostLine {
    CostLine {
        purpose: Some(purpose),
        physical_requests: Some(1),
        ..CostLine::new(CostComponent::Llm, CostBasis::Unknown, None)
    }
}

fn total_cost(costs: &[CostLine], purpose: LLMPurpose) -> CostLine {
    let basis = combined_basis(costs);
    CostLine {
        component: CostComponent::Llm,
        purpose: Some(purpose),
        decision_purpose: None,
        basis,
        dollars: (basis != CostBasis::Unknown).then(|| py::sum(costs.iter().map(|line| line.dollars.unwrap_or(0.0)))),
        input_tokens: costs.iter().map(|line| line.input_tokens).sum(),
        output_tokens: costs.iter().map(|line| line.output_tokens).sum(),
        seconds: None,
        physical_requests: costs.iter().map(|line| line.physical_requests).sum(),
        images: None,
        cache_read_tokens: known_total(costs.iter().map(|line| line.cache_read_tokens)),
        cache_write_tokens: known_total(costs.iter().map(|line| line.cache_write_tokens)),
        model: CostLine::shared_model(costs),
    }
}

/// OpenRouter's fee on a request sent with the caller's own provider key, as a share of the listed price, billed beside
/// the provider's own charge. A bound without it would fall short of that pair. The gateway's bound carries it too,
/// which only raises it.
const BYOK_FEE: f64 = 0.05;

/// OpenRouter price keys charged once per request; `image` is charged per input image and every other key per token. A
/// key added later is read as per token, which can only raise the bound.
const PER_REQUEST_PRICES: [&str; 2] = ["request", "web_search"];

/// The context window, the room it leaves a prompt, highest token price, per-request fees and per-image price across a
/// model's endpoints.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Ceiling {
    context: i64,
    prompt: i64,
    per_token: f64,
    per_request: f64,
    per_image: f64,
}

/// Routing can send a request to any endpoint, so the largest of each is taken. None when a price cannot be read or is
/// negative, as OpenRouter lists a price set per request: no ceiling holds for those.
///
/// OpenRouter and the Vercel AI Gateway serve the same endpoints payload. The gateway also lists dearer token prices
/// per `service_tiers` entry, per `inference_regions` zone and a `peak_pricing.multiplier` for busy hours; the bound
/// takes the dearest of them.
fn ceiling(payload: &Map<String, Value>) -> Result<Option<Ceiling>, String> {
    let endpoints = match object_value(payload.get("data"))?.get("endpoints").and_then(Value::as_array) {
        Some(endpoints) if !endpoints.is_empty() => endpoints,
        _ => return Ok(None),
    };
    let mut found = Ceiling { context: 0, prompt: 0, per_token: 0.0, per_request: 0.0, per_image: 0.0 };
    for entry in endpoints {
        let endpoint = object_value(Some(entry))?;
        let context = token_count(endpoint.get("context_length"))?;
        found.context = found.context.max(context);
        // Holding the listed completion length back from the window keeps the bound at or below what fits, whatever
        // output cap the request sends.
        let output = match endpoint.get("max_completion_tokens") {
            None | Some(Value::Null) => 0,
            listed => token_count(listed)?,
        };
        found.prompt = found.prompt.max(if output < context { context - output } else { context });
        let (mut token, mut request, mut image, mut peak) = (0.0_f64, 0.0_f64, 0.0_f64, 1.0_f64);
        for (key, value) in object_value(endpoint.get("pricing"))? {
            match key.as_str() {
                "discount" => {} // a discount lowers the charge, and leaving it out keeps the bound above it
                "service_tiers" => {
                    for tier in object_value(Some(value))?.values() {
                        for price in object_value(Some(tier))?.values() {
                            token = token.max(dollars(Some(price))?);
                        }
                    }
                }
                "peak_pricing" => peak = peak.max(dollars(object_value(Some(value))?.get("multiplier"))?),
                // Each override is a whole price set for the condition it names: a window, such as weekends or a band
                // of hours, or a prompt length from `min_prompt_tokens` up.
                "overrides" => {
                    for set in value.as_array().ok_or("expected a list of price overrides")? {
                        for (key, price) in object_value(Some(set))? {
                            match key.as_str() {
                                "utc_days" | "utc_start" | "utc_end" | "min_prompt_tokens" => {}
                                key if PER_REQUEST_PRICES.contains(&key) => {
                                    request = request.max(dollars(Some(price))?)
                                }
                                "image" => image = image.max(dollars(Some(price))?),
                                _ => token = token.max(dollars(Some(price))?),
                            }
                        }
                    }
                }
                key if PER_REQUEST_PRICES.contains(&key) => request += dollars(Some(value))?,
                "image" => image = dollars(Some(value))?,
                _ => token = token.max(dollars(Some(value))?),
            }
        }
        for region in endpoint.get("inference_regions").and_then(Value::as_array).into_iter().flatten() {
            for price in object_value(object_value(Some(region))?.get("pricing"))?.values() {
                token = token.max(dollars(Some(price))?);
            }
        }
        found.per_token = found.per_token.max(token * peak);
        found.per_request = found.per_request.max(request * peak);
        found.per_image = found.per_image.max(image * peak);
    }
    Ok((found.context != 0).then_some(found))
}

fn images(body: &Map<String, Value>) -> usize {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else { return 0 };
    messages
        .iter()
        .filter_map(|message| message.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("image_url"))
        .count()
}

fn bound(cost: CostLine, ceiling: Ceiling, body: &Map<String, Value>) -> CostLine {
    // An image can bill prompt tokens beside its per-image fee, at a rate no listing publishes, so only a text request
    // is bounded by its own bytes; one that carries an image keeps the whole window.
    let tokens = if images(body) > 0 {
        ceiling.context
    } else {
        ceiling.context.min(text_bytes(body).saturating_add(output_cap(body).unwrap_or(ceiling.context)))
    };
    let dollars = (tokens as f64 * ceiling.per_token + ceiling.per_request + ceiling.per_image * images(body) as f64)
        * (1.0 + BYOK_FEE);
    CostLine { basis: CostBasis::Bounded, dollars: Some(dollars), ..cost }
}

/// The bytes of a request that carries no image. Every tokenizer a routed model uses maps each
/// token to at least one byte of text, so the prompt holds at most this many tokens. The JSON around each message and
/// the response schema are counted too, and they are longer than the few tokens a chat template adds for them.
fn text_bytes(body: &Map<String, Value>) -> i64 {
    i64::try_from(Value::Object(body.clone()).to_string().len()).unwrap_or(i64::MAX)
}

/// The output tokens the request let the model bill, hidden reasoning included, when it set a cap.
fn output_cap(body: &Map<String, Value>) -> Option<i64> {
    ["max_tokens", "max_completion_tokens"].iter().find_map(|key| body.get(*key).and_then(Value::as_i64))
}

/// How much hidden reasoning a model may spend before it answers, in OpenRouter's normalized terms.
///
/// `None` turns reasoning off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffort {
    None,
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::None => "none",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
        }
    }

    pub fn parse(value: &str) -> Option<ReasoningEffort> {
        [ReasoningEffort::None, ReasoningEffort::Low, ReasoningEffort::Medium, ReasoningEffort::High]
            .into_iter()
            .find(|e| e.as_str() == value)
    }
}

/// Where a request carries the reasoning effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningField {
    /// OpenRouter's `reasoning: {"effort": ...}`, which the gateway and many local servers also read.
    OpenRouter,
    /// OpenAI's top-level `reasoning_effort`, which the other OpenAI-compatible APIs copy.
    Effort,
}

impl ReasoningField {
    pub fn key(self) -> &'static str {
        match self {
            ReasoningField::OpenRouter => "reasoning",
            ReasoningField::Effort => "reasoning_effort",
        }
    }

    fn value(self, effort: ReasoningEffort) -> Value {
        match self {
            ReasoningField::OpenRouter => json!({"effort": effort.as_str()}),
            ReasoningField::Effort => effort.as_str().into(),
        }
    }
}

/// The request and response format a client speaks. Everything past the wire, retries, the repair and the ledger, is
/// shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    ChatCompletions,
    /// Anthropic's Messages API ([`crate::anthropic`]).
    AnthropicMessages,
}

/// How a request asks for output that matches the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// `response_format: json_schema`, which the endpoint decodes against.
    JsonSchema,
    /// `response_format: json_object` with the schema stated in the prompt, for an endpoint that takes no schema. The
    /// schema is not enforced, so the one repair is what holds the reply to it.
    JsonObject,
}

/// A `Some(value)` that is not JSON null: Python's `.get(key)` reads both as `None`.
fn present<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    map.get(key).filter(|value| !value.is_null())
}

pub struct OpenAICompatibleLLM {
    api_key: String,
    wire: Wire,
    auth_header: Option<&'static str>,
    strict: bool,
    output_format: OutputFormat,
    reasoning_field: ReasoningField,
    extra_body: Map<String, Value>,
    http: Http,
    base_url: String,
    models: HashMap<LLMPurpose, String>,
    reasoning_effort: Option<ReasoningEffort>,
    reasoning_default: Option<ReasoningEffort>,
    provider_routing: Option<Value>,
    cache_breakpoints: bool,
    ceilings: Mutex<HashMap<String, Ceiling>>,
    provider: LLMProvider,
}

/// Options past the endpoint and its models, defaulting as Python's keyword arguments do.
#[derive(Debug, Clone)]
pub struct LLMOptions {
    /// The effort the user set. It is sent as it is, for every purpose.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// The effort the model runs at when the user set none. A purpose may raise it, as the field reader does.
    pub reasoning_default: Option<ReasoningEffort>,
    pub reasoning_field: ReasoningField,
    pub provider_routing: Option<Value>,
    pub strict: bool,
    pub output_format: OutputFormat,
    /// The header that carries the key in place of `Authorization: Bearer`, such as Azure's `api-key`.
    pub auth_header: Option<&'static str>,
    /// Whether a cached prefix is marked with `cache_control`, which OpenRouter and the gateway pass to providers that
    /// cache only up to such a mark. A provider's own endpoint may refuse the field, and caches without it.
    pub cache_breakpoints: bool,
    pub extra_body: Map<String, Value>,
    /// The provider this endpoint is, named in each failure's cause.
    pub provider: LLMProvider,
}

impl Default for LLMOptions {
    fn default() -> Self {
        LLMOptions {
            reasoning_effort: None,
            reasoning_default: None,
            reasoning_field: ReasoningField::OpenRouter,
            provider_routing: Some(openrouter_provider_routing()),
            strict: true,
            output_format: OutputFormat::JsonSchema,
            auth_header: None,
            cache_breakpoints: true,
            extra_body: Map::new(),
            provider: LLMProvider::OpenRouter,
        }
    }
}

/// What a generation has been billed so far, recorded into the ledger if the generation is dropped before it settles,
/// as Python's `except CancelledError` records it.
struct Billing<'a> {
    llm: &'a OpenAICompatibleLLM,
    ledger: Option<&'a Ledger>,
    model: String,
    /// The body of the attempt in flight, which bounds what it can bill if it is dropped.
    sent: Option<Map<String, Value>>,
    costs: Vec<CostLine>,
    purpose: LLMPurpose,
    /// The attempt in flight, whose requests are unaccounted until it returns.
    in_flight: Option<Usage>,
    armed: bool,
}

impl Billing<'_> {
    /// Charge an attempt that returned nothing: a request without a response may still have been billed.
    fn charge_unanswered(&mut self, usage: &Usage) -> usize {
        let offset = self.costs.len();
        let usage = usage.snapshot();
        let requested = self.model.clone();
        self.costs.extend(
            (0..usage.unaccounted_requests)
                .map(|_| CostLine { model: Some(requested.clone()), ..unknown(self.purpose) }),
        );
        let answered = usage.requests - usage.unaccounted_requests;
        if answered > 0 {
            // Rejected responses can cost nothing while still consuming physical provider requests.
            self.costs.push(CostLine {
                purpose: Some(self.purpose),
                physical_requests: Some(answered),
                model: Some(self.model.clone()),
                ..CostLine::new(CostComponent::Llm, CostBasis::Metered, Some(0.0))
            });
        }
        self.in_flight = None;
        offset
    }
}

impl Drop for Billing<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(usage) = self.in_flight.take() {
            let offset = self.charge_unanswered(&usage);
            // Drop cannot wait on a price lookup, so only a ceiling already read bounds what was dropped: a capped call
            // reads it before it sends.
            if let Some(ceiling) = self.llm.cached_ceiling(&self.model) {
                for line in &mut self.costs[offset..] {
                    if line.dollars.is_none() {
                        *line = bound(line.clone(), ceiling, self.sent.as_ref().unwrap_or(&Map::new()));
                    }
                }
            }
        }
        if let Some(ledger) = self.ledger {
            // An unknown receipt must not replace cancellation with a new budget failure.
            ledger.state().lines.extend(std::mem::take(&mut self.costs));
        }
    }
}

impl OpenAICompatibleLLM {
    /// Whether every provider that serves the request holds its reply to `response_format`. A router told to
    /// `require_parameters` routes only to those; one without it (the AI Gateway) can pick a provider that drops the
    /// schema, so the prompt states the schema too and the model sees it either way. A request without routing goes to
    /// the one endpoint named, which decodes against the schema or refuses it.
    fn routes_only_to_schema_providers(&self) -> bool {
        self.provider_routing.as_ref().is_none_or(|routing| routing["require_parameters"] == true)
    }

    /// An empty `api_key` sends no Authorization header, which is what a keyless local server expects.
    pub fn new(
        api_key: impl Into<String>,
        http: Http,
        base_url: &str,
        models: HashMap<LLMPurpose, String>,
        options: LLMOptions,
    ) -> Result<OpenAICompatibleLLM, String> {
        let mut clashes = reserved_clashes(&options.extra_body);
        let key = options.reasoning_field.key();
        if options.extra_body.contains_key(key) && !clashes.contains(&key) {
            clashes.push(key);
        }
        if !clashes.is_empty() {
            return Err(format!("extra request fields replace ones fastbrowse sets: {}", clashes.join(", ")));
        }
        let extra_body = options.extra_body.clone();
        OpenAICompatibleLLM::with_wire(api_key, http, base_url, models, options, Wire::ChatCompletions, extra_body)
    }

    /// A client on another wire, whose caller has checked the extra body against that wire's own fields.
    pub(crate) fn with_wire(
        api_key: impl Into<String>,
        http: Http,
        base_url: &str,
        models: HashMap<LLMPurpose, String>,
        options: LLMOptions,
        wire: Wire,
        extra_body: Map<String, Value>,
    ) -> Result<OpenAICompatibleLLM, String> {
        Ok(OpenAICompatibleLLM {
            api_key: api_key.into(),
            wire,
            auth_header: options.auth_header,
            strict: options.strict,
            output_format: options.output_format,
            reasoning_field: options.reasoning_field,
            http: http.with_credential_header(options.auth_header).withholding(extra_body.keys().cloned()),
            extra_body,
            base_url: base_url.trim_end_matches('/').to_string(),
            models,
            reasoning_effort: options.reasoning_effort,
            reasoning_default: options.reasoning_default,
            provider_routing: options.provider_routing,
            cache_breakpoints: options.cache_breakpoints,
            ceilings: Mutex::new(HashMap::new()),
            provider: options.provider,
        })
    }

    fn headers(&self) -> Vec<(String, String)> {
        if self.wire == Wire::AnthropicMessages {
            let mut headers = vec![("anthropic-version".to_string(), ANTHROPIC_VERSION.to_string())];
            if !self.api_key.is_empty() {
                headers.push(("x-api-key".into(), self.api_key.clone()));
            }
            return headers;
        }
        match self.auth_header {
            _ if self.api_key.is_empty() => vec![],
            Some(header) => vec![(header.into(), self.api_key.clone())],
            None => vec![("Authorization".into(), format!("Bearer {}", self.api_key))],
        }
    }

    fn host(&self) -> Option<String> {
        url::Url::parse(&self.base_url).ok().and_then(|url| url.host_str().map(str::to_string))
    }

    fn cached_ceiling(&self, model: &str) -> Option<Ceiling> {
        self.ceilings.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(model).copied()
    }

    async fn request(
        &self,
        body: &Value,
        ledger: Option<&Ledger>,
        usage: &Usage,
    ) -> Result<Map<String, Value>, LLMError> {
        let started = Instant::now();
        let reserve = ledger.map(|ledger| move || ledger.reserve(CostComponent::Llm, 0.0, 1));
        let attempts = Attempts {
            call: format!("llm {}", body["model"].as_str().unwrap_or_default()),
            attempt_seconds: LLM_ATTEMPT_SECONDS,
            before_retry: reserve.as_ref().map(|reserve| reserve as _),
            start_attempt: 0,
            split_batch: false,
        };
        let path = match self.wire {
            Wire::ChatCompletions => "chat/completions",
            Wire::AnthropicMessages => "messages",
        };
        let url = format!("{}/{path}", self.base_url);
        let response = match post_with_retry(&self.http, &url, body, &self.headers(), &attempts, usage).await {
            Ok(response) => response,
            Err(RetryStop::Budget(error)) => return Err(LLMError::Budget(error)),
            Err(RetryStop::Fatal(name)) => {
                return Err(LLMError::failed(FaultClass::Transport, format!("LLM request could not be sent ({name})")));
            }
            Err(RetryStop::Split { .. }) => return Err(LLMError::failed(FaultClass::Request, "LLM request split")),
        };
        let history = || usage.lock().history(started.elapsed().as_secs_f64());
        let Some(response) = response else {
            let last = usage.lock().failures.last().cloned().unwrap_or_default();
            return Err(LLMError::RetriesExhausted(Fault::new(
                FaultClass::Transport,
                format!("LLM transport failed after {}; last: {last}", history()),
            )));
        };
        if retryable(&response) {
            return Err(LLMError::RetriesExhausted(Fault::new(
                FaultClass::Http(response.status),
                format!("LLM request failed after {}; last: {}", history(), describe(&response)),
            )));
        }
        if matches!(response.status, 401 | 403) {
            let host = self.host().unwrap_or_else(|| self.base_url.clone());
            return Err(LLMError::Rejected(Fault::new(
                FaultClass::Http(response.status),
                format!("LLM {} ({host} rejected the API key)", describe(&response)),
            )));
        }
        if !response.is_success() {
            return Err(LLMError::failed(FaultClass::Http(response.status), format!("LLM {}", describe(&response))));
        }
        let payload = json_object(&response).map_err(|_| {
            // A successful response can be billed even when its receipt cannot be parsed.
            usage.lock().unaccounted_requests += 1;
            LLMError::failed(
                FaultClass::Envelope,
                format!("Invalid LLM response; HTTP {}: {}", response.status, body_excerpt(&response)),
            )
        })?;
        Ok(match self.wire {
            Wire::ChatCompletions => payload,
            Wire::AnthropicMessages => anthropic::as_chat_completion(payload),
        })
    }

    async fn settled_cost(
        &self,
        payload: &Map<String, Value>,
        purpose: LLMPurpose,
        body: &Map<String, Value>,
    ) -> CostLine {
        let mut cost = self.receipt_cost(payload, purpose).await;
        if cost.model.is_none() {
            // The response named no model, so the one asked for is the best record of what served it.
            cost.model = body.get("model").and_then(Value::as_str).map(str::to_string);
        }
        if cost.dollars.is_some() { cost } else { self.bounded(cost, body).await }
    }

    async fn receipt_cost(&self, payload: &Map<String, Value>, purpose: LLMPurpose) -> CostLine {
        let cost = cost(payload, purpose);
        let generation_id = match payload.get("id") {
            Some(Value::String(id)) if !id.is_empty() => id.clone(),
            _ => return cost,
        };
        if cost.dollars.is_some() || !matches!(self.host().as_deref(), Some("openrouter.ai" | "ai-gateway.vercel.sh")) {
            return cost;
        }
        // Usage receipts can lag the completion; recover that exact charge without generating the text again.
        let url = format!("{}/generation?id={}", self.base_url, text::quote_plus(&generation_id));
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let Ok(response) = self.http.get(&url, &self.headers(), 2.0).await else { continue };
            match read_receipt(&response, &generation_id, &cost) {
                Receipt::Settled(settled) => return settled,
                Receipt::Distrusted => return cost,
                Receipt::Again => continue,
            }
        }
        cost
    }

    /// An unknown charge bounded from the model's listed prices, or left unknown when none can be read.
    ///
    /// A request that timed out, or a response without usage whose receipt never came, stopped every run with a dollar
    /// cap although it had spent a fraction of a cent. No call can bill more tokens than its context window holds,
    /// prompt and output together, nor more than its prompt's bytes and its output cap, so the smaller of the two at
    /// the highest price is a true ceiling. OpenRouter and the Vercel AI Gateway both list each
    /// model's endpoints with their windows and prices.
    async fn bounded(&self, cost: CostLine, body: &Map<String, Value>) -> CostLine {
        let Some(model) = body.get("model").and_then(Value::as_str) else { return cost };
        match self.listed_ceiling(model).await {
            Some(ceiling) => bound(cost, ceiling, body),
            None => cost,
        }
    }

    /// The window and prices OpenRouter or the Vercel AI Gateway list for `model`, read once and kept.
    async fn listed_ceiling(&self, model: &str) -> Option<Ceiling> {
        if !matches!(self.host().as_deref(), Some("openrouter.ai" | "ai-gateway.vercel.sh")) {
            return None;
        }
        if let Some(found) = self.cached_ceiling(model) {
            return Some(found);
        }
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let url = format!("{}/models/{model}/endpoints", self.base_url);
            let Ok(response) = self.http.get(&url, &self.headers(), 2.0).await else { continue };
            if !response.is_success() {
                continue;
            }
            return match json_object(&response).and_then(|payload| ceiling(&payload)) {
                Ok(Some(read)) => {
                    self.ceilings
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(model.to_string(), read);
                    Some(read)
                }
                Ok(None) | Err(_) => None,
            };
        }
        None
    }

    async fn generate(
        &self,
        purpose: LLMPurpose,
        messages: &[Message],
        schema: &dyn ResponseSchema,
        ledger: Option<&Ledger>,
    ) -> Result<Generation<Value>, LLMError> {
        let Some(model) = self.models.get(&purpose) else {
            return Err(LLMError::failed(FaultClass::Request, format!("No model configured for {purpose}")));
        };
        let mut wire_messages = messages
            .iter()
            .map(|message| wire_message(message, self.cache_breakpoints))
            .collect::<Result<Vec<_>, _>>()?;
        // Strict mode needs every property required and no defaults; a decoder without strict support (some local
        // vLLM builds) gets the model's own schema instead.
        let json_schema = if self.strict {
            strict_schema(&schema.json_schema())
                .map_err(|error| LLMError::failed(FaultClass::Request, error.to_string()))?
        } else {
            schema.json_schema()
        };
        if self.wire == Wire::AnthropicMessages {
            let body = anthropic::body(&self.extra_body, model, messages, &json_schema, self.reasoning_effort)?;
            return self.billed(body, purpose, schema, ledger, model).await;
        }
        let json_schema = if deepseek(model) { flat_optional_objects(&json_schema) } else { json_schema };
        let response_format = match self.output_format {
            OutputFormat::JsonSchema => {
                if !self.routes_only_to_schema_providers() {
                    wire_messages.push(json!({"role": "system", "content": schema_instruction(&json_schema)}));
                }
                json!({
                    "type": "json_schema",
                    "json_schema": {"name": schema.name(), "schema": json_schema, "strict": self.strict},
                })
            }
            OutputFormat::JsonObject => {
                wire_messages.push(json!({"role": "system", "content": schema_instruction(&json_schema)}));
                json!({"type": "json_object"})
            }
        };
        let mut body = self.extra_body.clone();
        body.insert("model".into(), model.clone().into());
        body.insert("messages".into(), wire_messages.into());
        body.insert("response_format".into(), response_format);
        // Only OpenRouter and the gateway route, and both take `max_tokens`.
        if let Some(routing) = &self.provider_routing {
            let mut routing = routing.clone();
            // Only OpenRouter's routing takes `require_parameters`, and the pins name OpenRouter's providers, so the
            // gateway's is left as it is.
            if routing.get("require_parameters").is_some() {
                pin_endpoints(&mut routing, model);
            }
            body.insert("provider".into(), routing);
            body.insert("max_tokens".into(), ROUTED_OUTPUT_TOKENS.into());
        }
        if deepseek(model) {
            body.entry("temperature").or_insert(0.0.into());
        }
        // Reading a field's value off a record is a judgment whether the text holds it, and at `low` GPT-6 Luna
        // answered null for a title that was there in 14% of 80 replays of one captured request, against 0 of 80 at
        // `medium`. Every other purpose passes at `low`, so only this one's default is raised; an effort the user
        // set is theirs.
        let default = match (purpose, self.reasoning_default) {
            (LLMPurpose::Groups, Some(ReasoningEffort::Low)) => Some(ReasoningEffort::Medium),
            (_, default) => default,
        };
        if let Some(effort) = self.reasoning_effort.or(default) {
            body.insert(self.reasoning_field.key().into(), self.reasoning_field.value(effort));
        }
        self.billed(body, purpose, schema, ledger, model).await
    }

    /// Run a generation's turns over a built request body, recording what it was billed however it ends.
    async fn billed(
        &self,
        mut body: Map<String, Value>,
        purpose: LLMPurpose,
        schema: &dyn ResponseSchema,
        ledger: Option<&Ledger>,
        model: &str,
    ) -> Result<Generation<Value>, LLMError> {
        if ledger.is_some_and(|ledger| ledger.limits.max_dollars.is_some()) {
            // A call dropped in flight is bounded only by a ceiling already read, since Drop cannot wait on a lookup.
            // Unbounded, it stopped a capped run whose first call to a model was cancelled before any listing was read
            // (#1291). The listing is read once per model, before anything is sent.
            self.listed_ceiling(model).await;
        }
        let mut billing = Billing {
            llm: self,
            ledger,
            model: model.to_string(),
            sent: None,
            costs: Vec::new(),
            purpose,
            in_flight: None,
            armed: true,
        };
        let outcome = self.turns(&mut body, purpose, schema, ledger, &mut billing).await;
        billing.armed = false;
        match outcome {
            Ok(data) => Ok(Generation { data: data.0, cost: data.1 }),
            Err(error) => {
                // Record what a failed generation was billed. A success hands its cost to the caller to record instead.
                if let Some(ledger) = ledger {
                    ledger.record(std::mem::take(&mut billing.costs))?;
                }
                Err(error)
            }
        }
    }

    async fn turns(
        &self,
        body: &mut Map<String, Value>,
        purpose: LLMPurpose,
        schema: &dyn ResponseSchema,
        ledger: Option<&Ledger>,
        billing: &mut Billing<'_>,
    ) -> Result<(Value, CostLine), LLMError> {
        let started = Instant::now();
        for attempt in 0..2 {
            if let Some(ledger) = ledger {
                ledger.reserve(CostComponent::Llm, 0.0, 1)?;
            }
            let usage = Usage::default();
            billing.in_flight = Some(usage.clone());
            billing.sent = Some(body.clone());
            let sent = Value::Object(body.clone());
            let payload = match self.request(&sent, ledger, &usage).await {
                Ok(payload) => payload,
                Err(error) => {
                    let offset = billing.charge_unanswered(&usage);
                    self.settle_unanswered(billing, offset, body).await;
                    return Err(error);
                }
            };
            billing.in_flight = None;
            let usage = usage.snapshot();
            // Recorded before the envelope is read: a generation we cannot parse was still billed, and dropping it
            // would let an unaccounted request pass a dollar cap. A request sent but never delivered carried the same
            // prompt, so it is charged as the answer was; charging it as unknown instead made every run with one slow
            // call stop at its dollar cap.
            //
            // The receipt lookup below may be dropped, and Drop cannot wait on a price, so the line it would leave is
            // bounded now from the ceiling a capped call read before it sent: per request, before the requests it stands
            // for are counted in.
            let unsettled = match (cost(&payload, purpose), self.cached_ceiling(&billing.model)) {
                (line, Some(ceiling)) if line.dollars.is_none() => bound(line, ceiling, body),
                (line, _) => line,
            };
            billing.costs.push(with_discarded(unsettled, &usage));
            let settled = with_discarded(self.settled_cost(&payload, purpose, body).await, &usage);
            *billing.costs.last_mut().expect("pushed above") = settled;
            if truncated(&payload).map_err(|error| LLMError::failed(FaultClass::Envelope, error))? {
                if attempt == 0 {
                    continue;
                }
                return Err(truncation(purpose));
            }
            let content = match content(&payload) {
                Ok(content) if self.output_format == OutputFormat::JsonObject => unfenced(&content).to_string(),
                Ok(content) => content,
                // An empty completion comes back intermittently (a dropped or refused generation); ask once more.
                Err(_) if attempt == 0 => continue,
                Err(error) => {
                    return Err(LLMError::failed(
                        FaultClass::Envelope,
                        format!("Invalid completion envelope: {}", text::prefix(&scrub(&error, &self.api_key), 400)),
                    ));
                }
            };
            let (detail, ends_mid_json) = match serde_json::from_str::<Value>(&content) {
                Err(error) => (
                    errors_json(vec![json_invalid(&content, &error)]),
                    error.classify() == serde_json::error::Category::Eof,
                ),
                Ok(value) => match schema.validate_json(&if deepseek(&billing.model) {
                    let (nested, dropped) = nest_flat_objects(&schema.json_schema(), value);
                    if !dropped.is_empty() {
                        self.http.trace.event("half_objects_dropped", || {
                            Map::from_iter([
                                ("purpose".into(), purpose.as_str().into()),
                                ("paths".into(), json!(dropped)),
                            ])
                        });
                    }
                    nested.to_string()
                } else {
                    content.clone()
                }) {
                    Ok(data) => {
                        let cost = CostLine {
                            seconds: Some(started.elapsed().as_secs_f64()),
                            ..total_cost(&billing.costs, purpose)
                        };
                        return Ok((data, cost));
                    }
                    Err(detail) => (detail, false),
                },
            };
            // Paths and reasons are enough to repair the schema. A path is the model's own key, which can be anything
            // it read, so it is scrubbed too.
            let detail = scrub(&detail, &self.api_key);
            if ends_mid_json {
                // A reply ending mid-JSON was cut off by the output cap, the model or the provider.
                if attempt == 0 {
                    continue;
                }
                return Err(truncation(purpose));
            }
            if attempt == 1 {
                return Err(LLMError::failed(
                    FaultClass::Schema,
                    format!("LLM schema validation failed after one retry: {detail}"),
                ));
            }
            if let Some(Value::Array(messages)) = body.get_mut("messages") {
                messages.push(json!({"role": "assistant", "content": content}));
                messages.push(json!({
                    "role": "user",
                    "content": format!("Correct the JSON to match the schema. Validation errors:\n{detail}"),
                }));
            }
        }
        Err(LLMError::failed(FaultClass::Schema, "unreachable"))
    }

    /// Price the requests of an attempt that returned nothing usable.
    async fn settle_unanswered(&self, billing: &mut Billing<'_>, start: usize, body: &Map<String, Value>) {
        // A request without a response is bounded.
        for index in start..billing.costs.len() {
            if billing.costs[index].dollars.is_none() {
                let line = billing.costs[index].clone();
                billing.costs[index] = self.bounded(line, body).await;
            }
        }
    }
}

enum Receipt {
    Settled(CostLine),
    /// The receipt names another generation or an unreadable BYOK flag: keep the unknown charge.
    Distrusted,
    /// Not ready or unreadable: ask again.
    Again,
}

fn read_receipt(response: &Response, generation_id: &str, cost: &CostLine) -> Receipt {
    if !response.is_success() {
        return Receipt::Again;
    }
    let Ok(payload) = json_object(response) else { return Receipt::Again };
    let Ok(receipt) = object_value(payload.get("data")) else { return Receipt::Again };
    if receipt.get("id").and_then(Value::as_str) != Some(generation_id) {
        return Receipt::Distrusted;
    }
    let Some(amount) = present(receipt, "total_cost") else { return Receipt::Again };
    let Ok(mut charge) = dollars(Some(amount)) else { return Receipt::Again };
    let mut basis = CostBasis::Metered;
    let byok = present(receipt, "is_byok");
    if byok.is_some_and(|byok| !byok.is_boolean()) {
        return Receipt::Distrusted;
    }
    let upstream_amount = present(receipt, "upstream_inference_cost");
    if byok == Some(&Value::Bool(true)) && upstream_amount.is_none() {
        return Receipt::Distrusted;
    }
    // BYOK inference is billed outside the gateway; a normal receipt already includes it.
    if byok != Some(&Value::Bool(false))
        && let Some(upstream_amount) = upstream_amount
    {
        let Ok(upstream) = dollars(Some(upstream_amount)) else { return Receipt::Again };
        if upstream != 0.0 {
            basis = CostBasis::Estimated;
            charge += upstream;
        }
    }
    Receipt::Settled(CostLine { basis, dollars: Some(charge), ..cost.clone() })
}

impl fastbrowse_core::llm::LLMClient for OpenAICompatibleLLM {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        Box::pin(async move {
            let mut fields = Map::new();
            fields.insert("schema".into(), schema.name().into());
            let call = format!("llm:{purpose}");
            let generated = self.generate(purpose, messages, schema, ledger);
            let generated = self.http.trace.call_span(&call, fields, generated).await;
            generated.map_err(|error| error.from_call(self.provider.as_str(), purpose))
        })
    }

    fn context_tokens(&self, purpose: LLMPurpose) -> BoxFuture<'_, Option<i64>> {
        Box::pin(async move {
            let model = self.models.get(&purpose)?;
            self.listed_ceiling(model).await.map(|ceiling| ceiling.prompt)
        })
    }
}
