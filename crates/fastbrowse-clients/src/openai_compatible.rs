//! Port of `clients/openai_compatible.py`: structured chat completions with one schema repair and complete usage
//! accounting.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use fastbrowse_core::BoxFuture;
use fastbrowse_core::llm::{Generation, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{CostBasis, CostComponent, CostLine, LLMPurpose, combined_basis};
use fastbrowse_core::py::{self, base64, text};
use fastbrowse_core::schema::strict_schema;
use fastbrowse_core::telemetry::Ledger;
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::anthropic::{self, ANTHROPIC_VERSION};
use crate::http::{Http, Response};
use crate::validation::{
    Attempts, LLM_ATTEMPT_SECONDS, RetryStop, Usage, body_excerpt, describe, dollars, errors_json, json_invalid,
    json_object, object_value, post_with_retry, retryable, token_count, with_discarded,
};

/// How much larger the output cap is on the one retry after a response ran into it.
const TRUNCATION_RETRY_FACTOR: i64 = 4;

/// OpenRouter routing: an endpoint that ignores response_format would treat the schema as a hint, so none is routed
/// to. OpenRouter's default routing sent gemini-3.8-flash reads to Vertex at a 2.4s median where AI Studio answered the
/// same read in 1.3s; sorting by latency keeps fallbacks and follows the faster endpoint.
pub fn openrouter_provider_routing() -> Value {
    json!({"require_parameters": true, "sort": "latency"})
}

/// The Vercel AI Gateway documents `ttft` for the lowest time to first token where OpenRouter calls that sort
/// `latency`, and takes no `require_parameters`: the gateway matches the schema itself.
pub fn gateway_provider_routing() -> Value {
    json!({"sort": "ttft"})
}

/// Request fields fastbrowse sets itself. An extra body that named one would replace the schema, the messages or the
/// output cap the truncation retry grows, so the run would fail far from the setting that caused it.
pub const RESERVED_BODY_KEYS: [&str; 6] =
    ["max_tokens", "messages", "model", "provider", "reasoning", "response_format"];

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
        Err(LLMError::Failed("Only PNG and JPEG images are supported".into()))
    }
}

fn image_url(content: &[u8]) -> Result<String, LLMError> {
    Ok(format!("data:{};base64,{}", image_type(content)?, base64::encode(content)))
}

fn wire_message(message: &Message) -> Result<Value, LLMError> {
    if message.images.is_empty() {
        return Ok(json!({"role": message.role.as_str(), "content": message.content}));
    }
    let mut parts = vec![json!({"type": "text", "text": message.content})];
    for image in &message.images {
        parts.push(json!({"type": "image_url", "image_url": {"url": image_url(&image.0)?}}));
    }
    Ok(json!({"role": message.role.as_str(), "content": parts}))
}

/// The schema stated for an endpoint that cannot be given it as `response_format`. JSON mode on such endpoints (DeepSeek's)
/// also refuses a prompt that never says "json".
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

/// Output tokens of JSON that ended mid-value short of the cap, or None where the cap may have cut it.
///
/// Some providers report a response that hit the cap as an ordinary stop, so JSON ending mid-value is read as
/// truncation unless usage shows the model stopped short: then the provider dropped the tail, and more room would not
/// help.
fn short_of_cap(payload: &Map<String, Value>, cap: i64) -> Option<i64> {
    let empty = Value::Object(Map::new());
    let usage = object_value(Some(payload.get("usage").unwrap_or(&empty))).ok()?;
    let written = token_count(usage.get("completion_tokens")).ok()?;
    (0 < written && written < cap).then_some(written)
}

/// Make room for a response that ran into the output cap, or report it as truncated when it did so again.
///
/// Repairing the JSON would send the same prompt under the same cap and be cut off again, so the one retry is spent on
/// more room instead, and a second truncation is reported as what it is.
fn grow_cap(
    body: &mut Map<String, Value>,
    field: &str,
    attempt: usize,
    cap: i64,
    purpose: LLMPurpose,
) -> Result<(), LLMError> {
    if attempt == 1 {
        let ceiling = body.get(field).map_or_else(|| "no cap".to_string(), |value| value.to_string());
        return Err(LLMError::Failed(format!("LLM response truncated at the {ceiling} token output cap ({purpose})")));
    }
    // No cap was sent, so there is nothing to grow: the model stopped on its own, and asking again under a larger
    // ceiling would only spend the one retry on the same reply. The caller's repair turn is the next move.
    if cap <= 0 && body.get(field).is_none() {
        return Err(LLMError::Failed(format!(
            "LLM response was cut off with no output cap set ({purpose}); it ended before the JSON did"
        )));
    }
    body.insert(field.into(), (cap * TRUNCATION_RETRY_FACTOR).into());
    Ok(())
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
            basis: if cost.is_none() { CostBasis::Unknown } else { CostBasis::Metered },
            dollars: cost.map(|cost| dollars(Some(cost))).transpose()?,
            input_tokens: token_count(Some(usage.get("prompt_tokens").unwrap_or(&zero)))?,
            output_tokens: token_count(Some(usage.get("completion_tokens").unwrap_or(&zero)))?,
            seconds: None,
        })
    };
    read().unwrap_or_else(|_| unknown(purpose))
}

fn unknown(purpose: LLMPurpose) -> CostLine {
    CostLine { purpose: Some(purpose), ..CostLine::new(CostComponent::Llm, CostBasis::Unknown, None) }
}

fn total_cost(costs: &[CostLine], purpose: LLMPurpose) -> CostLine {
    let basis = combined_basis(costs);
    CostLine {
        component: CostComponent::Llm,
        purpose: Some(purpose),
        basis,
        dollars: (basis != CostBasis::Unknown).then(|| py::sum(costs.iter().map(|line| line.dollars.unwrap_or(0.0)))),
        input_tokens: costs.iter().map(|line| line.input_tokens).sum(),
        output_tokens: costs.iter().map(|line| line.output_tokens).sum(),
        seconds: None,
    }
}

/// OpenRouter's fee on a request sent with the caller's own provider key, as a share of the listed price, billed beside
/// the provider's own charge. A bound without it would fall short of that pair.
const BYOK_FEE: f64 = 0.05;

/// OpenRouter price keys charged once per request; `image` is charged per input image and every other key per token. A
/// key added later is read as per token, which can only raise the bound.
const PER_REQUEST_PRICES: [&str; 2] = ["request", "web_search"];

/// The context window, highest token price, per-request fees and per-image price across a model's endpoints.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Ceiling {
    context: i64,
    per_token: f64,
    per_request: f64,
    per_image: f64,
}

/// Routing can send a request to any endpoint, so the largest of each is taken. None when a price cannot be read or is
/// negative, as OpenRouter lists a price set per request: no ceiling holds for those.
fn ceiling(payload: &Map<String, Value>) -> Result<Option<Ceiling>, String> {
    let endpoints = match object_value(payload.get("data"))?.get("endpoints").and_then(Value::as_array) {
        Some(endpoints) if !endpoints.is_empty() => endpoints,
        _ => return Ok(None),
    };
    let mut found = Ceiling { context: 0, per_token: 0.0, per_request: 0.0, per_image: 0.0 };
    for entry in endpoints {
        let endpoint = object_value(Some(entry))?;
        found.context = found.context.max(token_count(endpoint.get("context_length"))?);
        let (mut request, mut image) = (0.0, 0.0);
        for (key, value) in object_value(endpoint.get("pricing"))? {
            if key == "discount" {
                continue; // a discount lowers the charge, and leaving it out keeps the bound above it
            }
            let price = dollars(Some(value))?;
            if PER_REQUEST_PRICES.contains(&key.as_str()) {
                request += price;
            } else if key == "image" {
                image = price;
            } else {
                found.per_token = found.per_token.max(price);
            }
        }
        found.per_request = found.per_request.max(request);
        found.per_image = found.per_image.max(image);
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
    let dollars =
        (ceiling.context as f64 * ceiling.per_token + ceiling.per_request + ceiling.per_image * images(body) as f64)
            * (1.0 + BYOK_FEE);
    CostLine { basis: CostBasis::Bounded, dollars: Some(dollars), ..cost }
}

/// How much hidden reasoning a model may spend before it answers, in OpenRouter's normalized terms.
///
/// `None` turns reasoning off. A model that reasons at every effort and does not scale it to `max_tokens`, such as
/// DeepSeek V4.1 Flash, spends a verdict's whole 400-token cap thinking and answers nothing, so it needs this.
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
    output_cap_field: &'static str,
    reasoning_field: ReasoningField,
    extra_body: Map<String, Value>,
    http: Http,
    base_url: String,
    models: HashMap<LLMPurpose, String>,
    reasoning_effort: Option<ReasoningEffort>,
    purpose_reasoning: HashMap<LLMPurpose, ReasoningEffort>,
    provider_routing: Option<Value>,
    ceilings: Mutex<HashMap<String, Ceiling>>,
}

/// Options past the endpoint and its models, defaulting as Python's keyword arguments do.
#[derive(Debug, Clone)]
pub struct LLMOptions {
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Efforts for single purposes, over `reasoning_effort`.
    pub purpose_reasoning: HashMap<LLMPurpose, ReasoningEffort>,
    pub reasoning_field: ReasoningField,
    pub provider_routing: Option<Value>,
    pub strict: bool,
    pub output_format: OutputFormat,
    /// `max_tokens`, or `max_completion_tokens` where an API has deprecated the other for its reasoning models.
    pub output_cap_field: &'static str,
    /// The header that carries the key in place of `Authorization: Bearer`, such as Azure's `api-key`.
    pub auth_header: Option<&'static str>,
    pub extra_body: Map<String, Value>,
}

impl Default for LLMOptions {
    fn default() -> Self {
        LLMOptions {
            reasoning_effort: None,
            purpose_reasoning: HashMap::new(),
            reasoning_field: ReasoningField::OpenRouter,
            provider_routing: Some(openrouter_provider_routing()),
            strict: true,
            output_format: OutputFormat::JsonSchema,
            output_cap_field: "max_tokens",
            auth_header: None,
            extra_body: Map::new(),
        }
    }
}

/// What a generation has been billed so far, recorded into the ledger if the generation is dropped before it settles,
/// as Python's `except CancelledError` records it.
struct Billing<'a> {
    llm: &'a OpenAICompatibleLLM,
    ledger: Option<&'a Ledger>,
    model: String,
    body_images: usize,
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
        let unaccounted = usage.lock().unaccounted_requests;
        self.costs.extend((0..unaccounted).map(|_| unknown(self.purpose)));
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
            // Drop cannot wait on a price lookup, so only a ceiling already read bounds what was dropped.
            let cached = self.llm.cached_ceiling(&self.model);
            if let Some(ceiling) = cached {
                for line in &mut self.costs[offset..] {
                    if line.dollars.is_none() {
                        *line = bound(line.clone(), ceiling, &json_images(self.body_images));
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

/// A body holding `count` images, for bounding a request whose body is gone.
fn json_images(count: usize) -> Map<String, Value> {
    let part = json!({"type": "image_url"});
    let mut body = Map::new();
    body.insert("messages".into(), json!([{"content": vec![part; count]}]));
    body
}

impl OpenAICompatibleLLM {
    /// An empty `api_key` sends no Authorization header, which is what a keyless local server expects.
    pub fn new(
        api_key: impl Into<String>,
        http: Http,
        base_url: &str,
        models: HashMap<LLMPurpose, String>,
        options: LLMOptions,
    ) -> Result<OpenAICompatibleLLM, String> {
        let mut clashes = reserved_clashes(&options.extra_body);
        for key in [options.output_cap_field, options.reasoning_field.key()] {
            if options.extra_body.contains_key(key) && !clashes.contains(&key) {
                clashes.push(key);
            }
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
            output_cap_field: options.output_cap_field,
            reasoning_field: options.reasoning_field,
            extra_body,
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            models,
            reasoning_effort: options.reasoning_effort,
            purpose_reasoning: options.purpose_reasoning,
            provider_routing: options.provider_routing,
            ceilings: Mutex::new(HashMap::new()),
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

    fn scrubbed(&self, text: &str) -> String {
        if self.api_key.is_empty() { text.to_string() } else { text.replace(&self.api_key, "[api key]") }
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
                return Err(LLMError::Failed(format!("LLM request could not be sent ({name})")));
            }
            Err(RetryStop::Split { .. }) => return Err(LLMError::Failed("LLM request split".into())),
        };
        let history = || usage.lock().history(started.elapsed().as_secs_f64());
        let Some(response) = response else {
            let last = usage.lock().failures.last().cloned().unwrap_or_default();
            return Err(LLMError::RetriesExhausted(format!("LLM transport failed after {}; last: {last}", history())));
        };
        if retryable(&response) {
            return Err(LLMError::RetriesExhausted(format!(
                "LLM request failed after {}; last: {}",
                history(),
                describe(&response)
            )));
        }
        if !response.is_success() {
            return Err(LLMError::Failed(format!("LLM {}", describe(&response))));
        }
        let payload = json_object(&response).map_err(|_| {
            LLMError::Failed(format!("Invalid LLM response; HTTP {}: {}", response.status, body_excerpt(&response)))
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
        let cost = self.receipt_cost(payload, purpose).await;
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
    /// prompt and output together, so that window at the highest price is a true ceiling where the output cap would
    /// not be: some routes do not count reasoning against it.
    async fn bounded(&self, cost: CostLine, body: &Map<String, Value>) -> CostLine {
        let Some(model) = body.get("model").and_then(Value::as_str) else { return cost };
        if self.host().as_deref() != Some("openrouter.ai") {
            return cost;
        }
        let mut found = self.cached_ceiling(model);
        let tries = if found.is_some() { 0 } else { 3 };
        for attempt in 0..tries {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let url = format!("{}/models/{model}/endpoints", self.base_url);
            let Ok(response) = self.http.get(&url, &self.headers(), 2.0).await else { continue };
            if !response.is_success() {
                continue;
            }
            match json_object(&response).and_then(|payload| ceiling(&payload)) {
                Ok(Some(read)) => {
                    self.ceilings
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(model.to_string(), read);
                    found = Some(read);
                    break;
                }
                Ok(None) | Err(_) => return cost,
            }
        }
        match found {
            Some(ceiling) => bound(cost, ceiling, body),
            None => cost,
        }
    }

    async fn generate(
        &self,
        purpose: LLMPurpose,
        messages: &[Message],
        schema: &dyn ResponseSchema,
        max_output_tokens: i64,
        ledger: Option<&Ledger>,
    ) -> Result<Generation<Value>, LLMError> {
        let Some(model) = self.models.get(&purpose) else {
            return Err(LLMError::Failed(format!("No model configured for {purpose}")));
        };
        let mut wire_messages = messages.iter().map(wire_message).collect::<Result<Vec<_>, _>>()?;
        // Strict mode needs every property required and no defaults; a decoder without strict support (some local
        // vLLM builds) gets the model's own schema instead.
        let json_schema = if self.strict {
            strict_schema(&schema.json_schema()).map_err(|error| LLMError::Failed(error.to_string()))?
        } else {
            schema.json_schema()
        };
        if self.wire == Wire::AnthropicMessages {
            let body = anthropic::body(
                &self.extra_body,
                model,
                messages,
                &json_schema,
                max_output_tokens,
                self.purpose_reasoning.get(&purpose).copied().or(self.reasoning_effort),
            )?;
            return self.billed(body, purpose, schema, max_output_tokens, ledger, model).await;
        }
        let response_format = match self.output_format {
            OutputFormat::JsonSchema => json!({
                "type": "json_schema",
                "json_schema": {"name": schema.name(), "schema": json_schema, "strict": self.strict},
            }),
            OutputFormat::JsonObject => {
                wire_messages.push(json!({"role": "system", "content": schema_instruction(&json_schema)}));
                json!({"type": "json_object"})
            }
        };
        let mut body = self.extra_body.clone();
        body.insert("model".into(), model.clone().into());
        // Zero is the caller saying no ceiling: fastbrowse never caps, truncates or crops an output token, so the
        // field is left off the request entirely rather than sent as a limit of zero.
        if max_output_tokens > 0 {
            body.insert(self.output_cap_field.into(), max_output_tokens.into());
        } else {
            body.remove(self.output_cap_field);
        }
        body.insert("messages".into(), wire_messages.into());
        body.insert("response_format".into(), response_format);
        if let Some(routing) = &self.provider_routing {
            body.insert("provider".into(), routing.clone());
        }
        if let Some(effort) = self.purpose_reasoning.get(&purpose).copied().or(self.reasoning_effort) {
            body.insert(self.reasoning_field.key().into(), self.reasoning_field.value(effort));
        }
        self.billed(body, purpose, schema, max_output_tokens, ledger, model).await
    }

    /// Run a generation's turns over a built request body, recording what it was billed however it ends.
    async fn billed(
        &self,
        mut body: Map<String, Value>,
        purpose: LLMPurpose,
        schema: &dyn ResponseSchema,
        max_output_tokens: i64,
        ledger: Option<&Ledger>,
        model: &str,
    ) -> Result<Generation<Value>, LLMError> {
        let mut billing = Billing {
            llm: self,
            ledger,
            model: model.to_string(),
            body_images: images(&body),
            costs: Vec::new(),
            purpose,
            in_flight: None,
            armed: true,
        };
        let outcome = self.turns(&mut body, purpose, schema, max_output_tokens, ledger, &mut billing).await;
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
        max_output_tokens: i64,
        ledger: Option<&Ledger>,
        billing: &mut Billing<'_>,
    ) -> Result<(Value, CostLine), LLMError> {
        let started = Instant::now();
        let mut short_retried = false;
        for turn in 0..3 {
            // A reply the provider cut short is asked for again without spending the one retry the rest share.
            let attempt = turn - usize::from(short_retried);
            if let Some(ledger) = ledger {
                ledger.reserve(CostComponent::Llm, 0.0, 1)?;
            }
            let usage = Usage::default();
            billing.in_flight = Some(usage.clone());
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
            billing.costs.push(with_discarded(cost(&payload, purpose), &usage));
            let settled = with_discarded(self.settled_cost(&payload, purpose, body).await, &usage);
            *billing.costs.last_mut().expect("pushed above") = settled;
            if truncated(&payload).map_err(LLMError::Failed)? {
                grow_cap(body, self.output_cap_field, attempt, max_output_tokens, purpose)?;
                continue;
            }
            let content = match content(&payload) {
                Ok(content) if self.output_format == OutputFormat::JsonObject => unfenced(&content).to_string(),
                Ok(content) => content,
                // An empty completion comes back intermittently (a dropped or refused generation); ask once more.
                Err(_) if attempt == 0 => continue,
                Err(error) => {
                    return Err(LLMError::Failed(format!(
                        "Invalid completion envelope: {}",
                        text::prefix(&self.scrubbed(&error), 400)
                    )));
                }
            };
            let (detail, ends_mid_json) = match serde_json::from_str::<Value>(&content) {
                Err(error) => (
                    errors_json(vec![json_invalid(&content, &error)]),
                    error.classify() == serde_json::error::Category::Eof,
                ),
                Ok(_) => match schema.validate_json(&content) {
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
            let detail = self.scrubbed(&detail);
            if ends_mid_json {
                // With no ceiling sent there is no cap to compare against, and `short_of_cap` answers None, which
                // grows a cap that is not there: the reply ended mid-JSON on its own and is reported, not retried
                // under a ceiling nobody asked for.
                let cap = body.get(self.output_cap_field).and_then(Value::as_i64).unwrap_or(max_output_tokens);
                if cap <= 0 {
                    return Err(LLMError::Failed(format!(
                        "LLM response ended mid-JSON with no output cap set ({purpose}); it wrote {written} tokens",
                        written = payload
                            .get("usage")
                            .and_then(|usage| usage.get("completion_tokens"))
                            .and_then(Value::as_i64)
                            .unwrap_or_default()
                    )));
                }
                match short_of_cap(&payload, cap) {
                    None => grow_cap(body, self.output_cap_field, attempt, max_output_tokens, purpose)?,
                    Some(written) if short_retried => {
                        return Err(LLMError::RetriesExhausted(format!(
                            "LLM response ended mid-JSON at {written} of {cap} output tokens twice"
                        )));
                    }
                    Some(_) => short_retried = true,
                }
                continue;
            }
            if attempt == 1 {
                return Err(LLMError::Failed(format!(
                    "LLM schema validation failed after one retry: {}",
                    text::prefix(&detail, 1000)
                )));
            }
            if let Some(Value::Array(messages)) = body.get_mut("messages") {
                messages.push(json!({"role": "assistant", "content": content}));
                messages.push(json!({
                    "role": "user",
                    "content": format!("Correct the JSON to match the schema. Validation errors:\n{detail}"),
                }));
            }
        }
        Err(LLMError::Failed("unreachable".into()))
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
        max_output_tokens: i64,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        Box::pin(async move {
            let mut fields = Map::new();
            fields.insert("schema".into(), schema.name().into());
            let call = format!("llm:{purpose}");
            self.http
                .trace
                .call_span(&call, fields, self.generate(purpose, messages, schema, max_output_tokens, ledger))
                .await
        })
    }
}
