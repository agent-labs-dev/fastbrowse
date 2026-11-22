//! The System One API (`POST /v1/systemone`): direct TypeSafe and OpenRouter
//! serve Jev over it, Telnyx its own decision models, and Ollama any it runs locally. AIMLAPI and Perplexity take the
//! same body at `/v1/decisions`. A host with no key is sent no Authorization header.

use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{
    DecisionClient, DecisionError, DecisionProfile, Evaluation, Images, JEV_REQUEST_BYTES, NO_IMAGES, Question,
    image_tokens, png_size, questions_without_images, referenced, without_images,
};
use fastbrowse_core::models::CostLine;
use fastbrowse_core::py::base64;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::http::Http;
use crate::validation::{
    AskError, Usage, asking_open, asking_split, invalid_response, json_object, object_value, parse_answers, post,
    reported_cost, timed, token_count, wire_questions,
};

pub const TYPESAFE_URL: &str = "https://api.typesafe.ai";
pub const OPENROUTER_URL: &str = "https://openrouter.ai/api";
pub const OPENROUTER_MODEL: &str = "jev-1.14";
/// The SDK base URL Telnyx documents; it serves only its own models, and rejects a request that names none.
pub const TELNYX_URL: &str = "https://api.telnyx.com/v2/ai/typesafe";
pub const TELNYX_MODEL: &str = "telnyx/decision-flash";
/// Ollama 0.35 and later, with no `/v1` suffix and no key.
pub const OLLAMA_URL: &str = "http://localhost:11434";
pub const AIMLAPI_URL: &str = "https://api.aimlapi.com";
/// Pinned rather than `typesafe/jev`, which AIMLAPI moves to each new Jev release.
pub const AIMLAPI_MODEL: &str = "typesafe/jev-1.14";
pub const PERPLEXITY_URL: &str = "https://api.perplexity.ai";
/// Pinned rather than `pplx-decider-latest`.
pub const PERPLEXITY_MODEL: &str = "pplx-decider-v1.2";
pub const SYSTEM_ONE_PATH: &str = "/v1/systemone";
/// TypeSafe's opt-in to Jev's long context, sent on each request that needs it.
pub const LONG_CONTEXT_HEADER: (&str, &str) = ("TypeSafe-Danger-Zone-Long-Context", "true");
/// TypeSafe's opt-in to image input, sent on each request that carries an image.
pub const IMAGES_HEADER: (&str, &str) = ("TypeSafe-Danger-Zone-Images", "true");
/// Where AIMLAPI and Perplexity take a System One body.
pub const DECISIONS_PATH: &str = "/v1/decisions";

pub struct SystemOneClient {
    api_key: String,
    http: Http,
    base_url: String,
    model: String,
    image_accounting: ImageAccounting,
    profile: DecisionProfile,
    path: &'static str,
    headers: Vec<(&'static str, &'static str)>,
}

impl SystemOneClient {
    pub fn new(api_key: impl Into<String>, http: Http, base_url: &str, model: &str) -> SystemOneClient {
        SystemOneClient {
            api_key: api_key.into(),
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            image_accounting: ImageAccounting::for_model(model),
            profile: DecisionProfile::for_model(model),
            path: SYSTEM_ONE_PATH,
            headers: Vec::new(),
        }
    }

    /// A route that documents Jev's long context, so a request is fitted to it rather than to 32k a question.
    pub fn long_context(mut self) -> SystemOneClient {
        self.profile = self.profile.long_context();
        self
    }

    /// OpenRouter, which serves each decision model at the context and price it lists for that model.
    pub fn openrouter(mut self) -> SystemOneClient {
        self.profile = DecisionProfile::on_openrouter(&self.model);
        self
    }

    pub(crate) fn openrouter_with_model(mut self, profile: DecisionProfile, model: &str) -> Self {
        self.profile = profile;
        self.image_accounting = ImageAccounting::for_model(model);
        self
    }

    /// A route that passes Jev image input through, sent with TypeSafe's images header.
    pub fn images(mut self) -> SystemOneClient {
        self.profile.images = true;
        self
    }

    /// TypeSafe's own API, which grants long context and image input only to a request carrying their headers.
    pub fn typesafe(mut self) -> SystemOneClient {
        self.headers.push(LONG_CONTEXT_HEADER);
        self.images().long_context()
    }

    /// The same body sent to `path` instead of `/v1/systemone`.
    pub fn at(mut self, path: &'static str) -> SystemOneClient {
        self.path = path;
        self
    }

    /// A reseller's own price, which it does not report: its calls are an unknown cost, never the model's list price.
    pub fn unpriced(mut self) -> SystemOneClient {
        self.profile.dollars_per_input_token = None;
        self
    }

    async fn ask(
        &self,
        state: &Value,
        questions: IndexMap<String, Question>,
        images: &Images,
        start_attempt: usize,
        split_batch: bool,
    ) -> Result<Evaluation, AskError> {
        // The span covers the parse as well as the request: a 200 whose body is not a valid answer is a failed call.
        let asked = self.request(state, &questions, images, start_attempt, split_batch);
        self.http.trace.call_span("jev", crate::validation::jev_trace_fields(), asked).await
    }

    async fn request(
        &self,
        state: &Value,
        questions: &IndexMap<String, Question>,
        images: &Images,
        start_attempt: usize,
        split_batch: bool,
    ) -> Result<Evaluation, AskError> {
        let started = Instant::now();
        let sent = Usage::default();
        // Another model reads an image only as a top-level item of the state: an option's picture is left out, and its
        // option is told by its text.
        // TypeSafe refuses an image nothing in the request refers to, and a batch of a split request asks only some of
        // the questions that refer to them.
        let (state, stripped, images) = if images.is_empty() || self.image_accounting == ImageAccounting::Jev {
            (state.clone(), None, referenced(images, state, questions))
        } else {
            let state = media_as_items(state, images);
            let questions = questions_without_images(questions, images);
            let images = state_images(&state, images).map(|(key, png)| (key.clone(), png.clone())).collect();
            (state, Some(questions), images)
        };
        let images = &images;
        let asked = stripped.as_ref().unwrap_or(questions);
        let mut body = json!({"model": self.model, "state": state, "questions": wire_questions(asked, false)});
        let mut headers = self.headers.clone();
        if !images.is_empty() {
            let assets: Map<String, Value> = images
                .iter()
                .map(|(key, png)| {
                    (key.clone(), json!({"type": "image", "format": "png", "data_base64": base64::encode(png)}))
                })
                .collect();
            body["assets"] = Value::Object(assets);
            headers.push(IMAGES_HEADER);
        }
        // A body past TypeSafe's limit is refused with 413 whatever is resent, so it is refused here, before a
        // round trip that uploads it: a page whose cards outgrew it sent each step's decision 28 times (#1301).
        let bytes = serde_json::to_vec(&body).map_or(0, |bytes| bytes.len());
        if bytes > JEV_REQUEST_BYTES {
            return Err(DecisionError::input_too_large(format!(
                "Jev input too large; the request body is {bytes} bytes, over the limit of {JEV_REQUEST_BYTES}"
            ))
            .into());
        }
        let url = format!("{}{}", self.base_url, self.path);
        let price = self.profile.dollars_per_input_token;
        let image_context = (!images.is_empty()).then_some(self.profile.max_total_tokens);
        let response = post(
            &self.http,
            &url,
            &self.api_key,
            &body,
            &headers,
            &sent,
            start_attempt,
            split_batch,
            price,
            image_context,
        )
        .await?;
        let sent = sent.snapshot();
        let parsed = (|| {
            let payload = json_object(&response)?;
            let usage = object_value(payload.get("usage"))?;
            let tokens = token_count(usage.get("input_tokens"))?;
            let output_tokens = match usage.get("output_tokens") {
                None => 0,
                value => token_count(value)?,
            };
            let cost = reported_cost(usage.get("cost"), tokens, output_tokens, price)?;
            let model = match payload.get("model") {
                None => self.model.clone(),
                Some(Value::String(model)) => model.clone(),
                Some(_) => return Err("invalid model name".to_string()),
            };
            Ok(Evaluation {
                requests: sent.requests,
                model: model.clone(),
                answers: parse_answers(payload.get("answers"), questions, false, None)?,
                input_tokens: tokens,
                // Counted before `timed`, so each resent request is charged the images it carried too.
                cost: timed(
                    CostLine { images: (!images.is_empty()).then(|| images.len() as i64), model: Some(model), ..cost },
                    &sent,
                    started,
                ),
            })
        })();
        parsed.map_err(|detail| invalid_response(&response, "Invalid Jev response", &detail))
    }
}

impl SystemOneClient {
    async fn answer(
        &self,
        state: &Value,
        questions: &IndexMap<String, Question>,
        images: &Images,
    ) -> Result<Evaluation, DecisionError> {
        let price = self.profile.dollars_per_input_token;
        let ask = |batch, attempt, split| -> BoxFuture<'_, Result<Evaluation, AskError>> {
            Box::pin(self.ask(state, batch, images, attempt, split))
        };
        asking_open(questions, move |asked| async move { asking_split(asked, &ask, price).await }, &self.model, price)
            .await
    }
}

impl DecisionClient for SystemOneClient {
    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(self.answer(state, questions, &NO_IMAGES))
    }

    fn evaluate_images<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
        images: &'a Images,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async move {
            if self.profile.images {
                self.answer(state, questions, images).await
            } else {
                self.answer(&without_images(state, images), &questions_without_images(questions, images), &NO_IMAGES)
                    .await
            }
        })
    }

    fn profile(&self) -> DecisionProfile {
        self.profile.clone()
    }

    /// Reserve context for the pictures each route sends, using its own documented tokenization where available.
    fn image_tokens(&self, state: &Value, questions: &IndexMap<String, Question>, images: &Images) -> i64 {
        if !self.profile.images || images.is_empty() {
            return 0;
        }
        if self.image_accounting == ImageAccounting::Jev {
            return image_tokens(images, state, questions);
        }
        let state = media_as_items(state, images);
        state_images(&state, images)
            .map(|(_, png)| png)
            .filter_map(|png| png_size(png))
            .map(|(width, height)| self.image_accounting.tokens(width, height, questions.len()))
            .sum()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImageAccounting {
    Jev,
    GeminiDecision,
    LiquidPerQuestion,
    LiquidPerRequest,
    PerplexityDecider,
    Clef,
    Undocumented,
}

impl ImageAccounting {
    fn for_model(model: &str) -> Self {
        let (vendor, model) = model.trim_start_matches('~').split_once('/').unwrap_or(("", model));
        match (vendor, model) {
            ("" | "typesafe" | "typesafe-ai", _) if model.starts_with("jev-") => Self::Jev,
            ("google", "gemini-decision-1") => Self::GeminiDecision,
            ("liquid", "d1" | "d1-ultra") => Self::LiquidPerQuestion,
            ("liquid", "d1.2" | "d1.2-ultra") => Self::LiquidPerRequest,
            (
                "" | "perplexity",
                "pplx-decider-latest"
                | "pplx-decider-flash-latest"
                | "pplx-decider-v1-27b"
                | "pplx-decider-v1.1-27b"
                | "pplx-decider-v1.2"
                | "pplx-decider-flash-v1.2"
                | "pplx-decider-v1.3"
                | "pplx-decider-flash-v1.3"
                | "pplx-decider-v1.3:free"
                | "pplx-decider-flash-v1.3:free",
            ) => Self::PerplexityDecider,
            ("cloudflare", "clef" | "clef-flash" | "clef-omni") => Self::Clef,
            _ => Self::Undocumented,
        }
    }

    fn tokens(self, width: u64, height: u64, questions: usize) -> i64 {
        match self {
            // Google's Decisions API links to the default media resolution budget of 1,120 image tokens.
            // https://aistudio.google.com/docs/media-resolution#token-counts
            Self::GeminiDecision => 1120,
            // Liquid bills 1.5 tokens per 32-pixel patch, rounded up. OpenRouter documents that the original
            // d1 and d1-ultra count them per question. The d1.2 models count them once per request.
            // https://openrouter.ai/docs/guides/community/multimodal-decisions#limits
            // https://docs.liquid.ai/lfm/models/d1#image-token-usage
            Self::LiquidPerQuestion | Self::LiquidPerRequest => {
                let patches = width.div_ceil(32) * height.div_ceil(32);
                let tokens = (3 * patches).div_ceil(2) as i64;
                if self == Self::LiquidPerQuestion { tokens * questions as i64 } else { tokens }
            }
            // Perplexity documents about 1,000 tokens per megapixel.
            // https://docs.perplexity.ai/docs/decisions/decisions
            Self::PerplexityDecider => (width * height).div_ceil(1000) as i64,
            // Cloudflare scales to an area between 256 squared and about 1 megapixel, rounds both sides to
            // multiples of 32, and adds 3 markers to its patch count, capped at 1,024.
            // https://developers.cloudflare.com/workers-ai/models/clef-flash/
            Self::Clef => {
                let area = (width * height) as f64;
                let scale = (area.clamp(65_536.0, 1_048_576.0) / area).sqrt();
                let patches = |side: u64| (side as f64 * scale / 32.0).round().max(1.0) as i64;
                (patches(width) * patches(height) + 3).min(1024)
            }
            // Jev counts references before this point. For other models without a published calculation,
            // no local estimate is available. The response's reported usage still includes the pictures.
            Self::Jev | Self::Undocumented => 0,
        }
    }
}

/// Move an object's top-level images into separate state items. Strip nested references because OpenRouter refuses
/// them for these routes. The key beside each moved image still names what it shows.
fn media_as_items(state: &Value, images: &Images) -> Value {
    let Value::Object(object) = state else {
        return match state {
            Value::Array(items) => items.iter().map(|item| without_images(item, images)).collect(),
            _ => state.clone(),
        };
    };
    let mut items = Vec::new();
    let mut text = Map::new();
    for (key, value) in object {
        let value = if value.as_str().is_some_and(|reference| images.contains_key(reference)) {
            let pointer = format!("image item {} after this object", items.len() + 1);
            items.push(value.clone());
            Value::String(pointer)
        } else {
            without_images(value, images)
        };
        text.insert(key.clone(), value);
    }
    if items.is_empty() {
        return Value::Object(text);
    }
    std::iter::once(Value::Object(text)).chain(items).collect()
}

/// The exact top-level references the non-Jev wire resolves to pictures. A repeated item counts at each occurrence.
fn state_images<'a, 'b>(state: &'a Value, images: &'b Images) -> impl Iterator<Item = (&'b String, &'b Vec<u8>)> {
    let items = state.as_array().map_or_else(|| std::slice::from_ref(state), Vec::as_slice);
    items.iter().filter_map(|item| images.get_key_value(item.as_str()?))
}
