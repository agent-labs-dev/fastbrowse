//! The System One API (`POST /v1/systemone`), first ported from `clients/typesafe.py`: direct TypeSafe and OpenRouter
//! serve Jev over it, Telnyx its own decision models, and Ollama any it runs locally. AIMLAPI and Perplexity take the
//! same body at `/v1/decisions`. A host with no key is sent no Authorization header.

use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{
    DecisionClient, DecisionError, DecisionProfile, Evaluation, Images, NO_IMAGES, Question, without_images,
};
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
        self.http.trace.call_span("jev", Map::new(), asked).await
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
        let mut body = json!({"model": self.model, "state": state, "questions": wire_questions(questions, false)});
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
        let url = format!("{}{}", self.base_url, self.path);
        let price = self.profile.dollars_per_input_token;
        let response =
            post(&self.http, &url, &self.api_key, &body, &headers, &sent, start_attempt, split_batch, price).await?;
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
                model,
                answers: parse_answers(payload.get("answers"), questions, false, None)?,
                input_tokens: tokens,
                cost: timed(cost, &sent, started),
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
                self.answer(&without_images(state, images), questions, &NO_IMAGES).await
            }
        })
    }

    fn profile(&self) -> DecisionProfile {
        self.profile.clone()
    }
}
