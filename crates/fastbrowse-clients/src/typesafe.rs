//! Port of `clients/typesafe.py`: TypeSafe System One transport for OpenRouter and the direct API.

use fastbrowse_core::BoxFuture;
use fastbrowse_core::jev::{DecisionProfile, Evaluation, JEV_MODEL, JevClient, JevError, Question};
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
pub const OPENROUTER_MODEL: &str = "jev-1.13";

pub struct TypeSafeJevClient {
    api_key: String,
    http: Http,
    base_url: String,
    model: String,
    profile: DecisionProfile,
}

impl TypeSafeJevClient {
    pub fn new(api_key: impl Into<String>, http: Http, base_url: &str, model: &str) -> TypeSafeJevClient {
        TypeSafeJevClient {
            api_key: api_key.into(),
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            profile: DecisionProfile::for_model(model),
        }
    }

    /// The direct API with its default model.
    pub fn direct(api_key: impl Into<String>, http: Http) -> TypeSafeJevClient {
        TypeSafeJevClient::new(api_key, http, TYPESAFE_URL, JEV_MODEL)
    }

    async fn ask(
        &self,
        state: &Value,
        questions: IndexMap<String, Question>,
        start_attempt: usize,
        split_batch: bool,
    ) -> Result<Evaluation, AskError> {
        // The span covers the parse as well as the request: a 200 whose body is not a valid answer is a failed call.
        self.http.trace.call_span("jev", Map::new(), self.request(state, &questions, start_attempt, split_batch)).await
    }

    async fn request(
        &self,
        state: &Value,
        questions: &IndexMap<String, Question>,
        start_attempt: usize,
        split_batch: bool,
    ) -> Result<Evaluation, AskError> {
        let started = Instant::now();
        let sent = Usage::default();
        let body = json!({"model": self.model, "state": state, "questions": wire_questions(questions, false)});
        let url = format!("{}/v1/systemone", self.base_url);
        let price = self.profile.dollars_per_input_token;
        let response =
            post(&self.http, &url, &self.api_key, &body, &[], &sent, start_attempt, split_batch, price).await?;
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

impl JevClient for TypeSafeJevClient {
    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, JevError>> {
        let price = self.profile.dollars_per_input_token;
        Box::pin(asking_open(
            questions,
            move |asked| async move {
                asking_split(asked, &|batch, attempt, split| Box::pin(self.ask(state, batch, attempt, split)), price)
                    .await
            },
            &self.model,
            price,
        ))
    }

    fn profile(&self) -> DecisionProfile {
        self.profile.clone()
    }
}
