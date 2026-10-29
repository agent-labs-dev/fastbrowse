//! OpenAI's Decisions API (`POST /v1/decisions`, <https://developers.openai.com/api/docs/guides/decisions>): the
//! same three judgments as System One under other names and shapes.
//!
//! | System One | Decisions |
//! |:--|:--|
//! | `state` | `input`, a string or a user message with an `input_json` part |
//! | `questions` keyed by id | `questions`, an array whose items carry their id as `name` |
//! | `noul` with `true`/`false` descriptions | `predicate`, whose descriptions go in its instructions |
//! | `choice` criteria `{key: description}` | `choices` `[{value, description}]` |
//! | `score` criteria `[description]` | `levels` `[{label, description}]`, lowest first |
//!
//! Answers come back as an array by `name`, with probabilities as `[{value, probability}]`. They are rewritten into
//! System One's answer shape and checked by the same validation, so every client agrees on what a valid answer is. A
//! `refusal` answer is a failed request. The response carries no cost, so it is priced from `usage.input_tokens`.

use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{DecisionClient, DecisionError, DecisionProfile, Evaluation, Question};
use fastbrowse_core::py::json::dumps;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::http::Http;
use crate::validation::{
    AskError, Usage, asking_open, asking_split, estimated_cost, invalid_response, json_object, object_value,
    parse_answers, post, timed, token_count,
};

pub const OPENAI_URL: &str = "https://api.openai.com";
pub const OPENAI_DECISIONS_MODEL: &str = "gpt-6-luna";

pub struct OpenAIDecisionsClient {
    api_key: String,
    http: Http,
    base_url: String,
    model: String,
    profile: DecisionProfile,
}

/// A criterion's description as text: the field takes a string, and fastbrowse's criteria may be objects.
fn description(key: &str, criterion: &Value) -> String {
    match criterion {
        Value::String(text) => text.clone(),
        Value::Null => key.to_string(),
        other => dumps(other),
    }
}

/// The questions as the Decisions API takes them.
pub fn wire_questions(questions: &IndexMap<String, Question>) -> Value {
    let wire: Vec<Value> = questions
        .iter()
        .map(|(name, question)| match question {
            Question::Noul(q) => {
                let mut instructions = q.instructions.clone();
                for (answer, meaning) in [("Yes", &q.r#true), ("No", &q.r#false)] {
                    if let Some(meaning) = meaning {
                        instructions.push_str(&format!("\n{answer} means: {meaning}"));
                    }
                }
                json!({"type": "predicate", "name": name, "instructions": instructions})
            }
            Question::Choice(q) => {
                let choices: Vec<Value> = q
                    .criteria
                    .iter()
                    .map(|(key, criterion)| json!({"value": key, "description": description(key, criterion)}))
                    .collect();
                json!({"type": "choice", "name": name, "instructions": q.instructions, "choices": choices})
            }
            Question::Score(q) => {
                let levels: Vec<Value> = q
                    .criteria
                    .iter()
                    .enumerate()
                    .map(|(i, level)| json!({"label": i.to_string(), "description": level}))
                    .collect();
                json!({"type": "score", "name": name, "instructions": q.instructions, "levels": levels})
            }
        })
        .collect();
    Value::Array(wire)
}

/// `[{value, probability}]` keyed by `field` of each item, as System One's `{value: probability}`.
fn keyed_probabilities(value: Option<&Value>, field: &str) -> Result<Value, String> {
    let Some(Value::Array(items)) = value else { return Err("probabilities are not a list".into()) };
    let mut keyed = Map::new();
    for item in items {
        let item = object_value(Some(item))?;
        let key = match item.get(field) {
            Some(Value::String(key)) => key.clone(),
            Some(Value::Number(key)) => key.to_string(),
            _ => return Err(format!("a probability without its {field}")),
        };
        keyed.insert(key, item.get("probability").cloned().unwrap_or(Value::Null));
    }
    Ok(Value::Object(keyed))
}

/// The answers array rewritten as System One's answers object, for `parse_answers` to check.
pub fn system_one_answers(value: Option<&Value>) -> Result<Value, String> {
    let Some(Value::Array(items)) = value else { return Err("answers are not a list".into()) };
    let mut answers = Map::new();
    for item in items {
        let item = object_value(Some(item))?;
        let Some(Value::String(name)) = item.get("name") else { return Err("an answer without its name".into()) };
        let answer = match item.get("type").and_then(Value::as_str) {
            Some("predicate") => json!({"type": "noul", "noul": item.get("probability")}),
            Some("choice") => json!({
                "type": "choice",
                "choice": item.get("choice"),
                "probabilities": keyed_probabilities(item.get("probabilities"), "value")?,
                "confidence": item.get("confidence"),
            }),
            Some("score") => json!({
                "type": "score",
                "score": item.get("score"),
                "probabilities": keyed_probabilities(item.get("probabilities"), "label")?,
                "confidence": item.get("confidence"),
            }),
            Some("refusal") => return Err(format!("the model refused {name}")),
            _ => return Err(format!("incorrect answer type for {name}")),
        };
        if answers.insert(name.clone(), answer).is_some() {
            return Err(format!("two answers for {name}"));
        }
    }
    Ok(Value::Object(answers))
}

/// The state as `input`: text as it is, anything else as the user's JSON.
fn input(state: &Value) -> Value {
    match state {
        Value::String(text) => Value::String(text.clone()),
        other => json!([{"role": "user", "content": [{"type": "input_json", "data": other}]}]),
    }
}

impl OpenAIDecisionsClient {
    pub fn new(api_key: impl Into<String>, http: Http, base_url: &str, model: &str) -> OpenAIDecisionsClient {
        OpenAIDecisionsClient {
            api_key: api_key.into(),
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            profile: DecisionProfile::for_model(model),
        }
    }

    /// A reseller's own price, which it does not report: its calls are an unknown cost, never the model's list price.
    pub fn unpriced(mut self) -> OpenAIDecisionsClient {
        self.profile.dollars_per_input_token = None;
        self
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
        let body = json!({"model": self.model, "input": input(state), "questions": wire_questions(questions)});
        let url = format!("{}/v1/decisions", self.base_url);
        let price = self.profile.dollars_per_input_token;
        let response =
            post(&self.http, &url, &self.api_key, &body, &[], &sent, start_attempt, split_batch, price).await?;
        let sent = sent.snapshot();
        let parsed = (|| {
            let payload = json_object(&response)?;
            let usage = object_value(payload.get("usage"))?;
            let tokens = token_count(usage.get("input_tokens"))?;
            let model = match payload.get("model") {
                None => self.model.clone(),
                Some(Value::String(model)) => model.clone(),
                Some(_) => return Err("invalid model name".to_string()),
            };
            let answers = system_one_answers(payload.get("answers"))?;
            Ok(Evaluation {
                requests: sent.requests,
                model,
                answers: parse_answers(Some(&answers), questions, false, None)?,
                input_tokens: tokens,
                // Cached input is billed at a discount the response does not state, so the call is estimated at list price.
                cost: timed(estimated_cost(price, tokens, 0), &sent, started),
            })
        })();
        parsed.map_err(|detail| invalid_response(&response, "Invalid decisions response", &detail))
    }
}

impl DecisionClient for OpenAIDecisionsClient {
    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
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
