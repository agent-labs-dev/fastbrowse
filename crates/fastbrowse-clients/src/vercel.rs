//! Port of `clients/vercel.py`: the Vercel AI Gateway v4 evaluation transport.
//!
//! Integrator's observed response, with choice, noul (wire boolean), and score:
//!
//! ```json
//! {"answers":{"green":{"type":"boolean","probability":0.02},
//! "col":{"type":"choice","choice":"blue","probabilities":{"blue":1,"red":0}},
//! "sc":{"type":"score","score":1.99,"probabilities":{"0":0,"1":0,"2":1}}},
//! "rounding":{"probabilityDecimals":2,"scoreDecimals":2},
//! "usage":{"inputTokens":389,"outputTokens":61},"warnings":[],
//! "providerMetadata":{"typesafe":{"confidence":{"col":1,"sc":0.99}},
//! "gateway":{"cost":"0.000016338"}}}
//! ```
//!
//! Noul questions use wire type boolean; score criteria are arrays. Rounded probabilities may be JSON integers. This
//! implementation makes no discovery calls.

use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{DecisionClient, DecisionError, Evaluation, JEV_DOLLARS_PER_INPUT_TOKEN, Question};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::http::Http;
use crate::validation::{
    AskError, Usage, asking_open, asking_split, invalid_response, json_object, object_value, parse_answers, post,
    reported_cost, timed, token_count, wire_questions,
};

pub const GATEWAY_URL: &str = "https://ai-gateway.vercel.sh";
const MODEL: &str = "typesafe-ai/jev";
const PRICE: Option<f64> = Some(JEV_DOLLARS_PER_INPUT_TOKEN);

pub struct VercelGatewayJevClient {
    api_key: String,
    http: Http,
    base_url: String,
}

impl VercelGatewayJevClient {
    pub fn new(api_key: impl Into<String>, http: Http, base_url: &str) -> VercelGatewayJevClient {
        VercelGatewayJevClient { api_key: api_key.into(), http, base_url: base_url.trim_end_matches('/').to_string() }
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
        let body = json!({"state": state, "questions": wire_questions(questions, true)});
        let url = format!("{}/v4/ai/evaluation-model", self.base_url);
        let headers = [
            ("ai-gateway-auth-method", "api-key"),
            ("ai-gateway-protocol-version", "0.0.1"),
            ("ai-model-id", MODEL),
            ("ai-evaluation-model-specification-version", "4"),
        ];
        let response =
            post(&self.http, &url, &self.api_key, &body, &headers, &sent, start_attempt, split_batch, PRICE).await?;
        let sent = sent.snapshot();
        let empty = Value::Object(Map::new());
        let parsed = (|| {
            let payload = json_object(&response)?;
            let usage = object_value(payload.get("usage"))?;
            let tokens = token_count(usage.get("inputTokens"))?;
            let output_tokens = match usage.get("outputTokens") {
                None => 0,
                value => token_count(value)?,
            };
            let metadata = object_value(Some(payload.get("providerMetadata").unwrap_or(&empty)))?;
            let typesafe = object_value(Some(metadata.get("typesafe").unwrap_or(&empty)))?;
            let confidence = object_value(Some(typesafe.get("confidence").unwrap_or(&empty)))?;
            let cost_value = object_value(Some(metadata.get("gateway").unwrap_or(&empty)))?.get("cost");
            let cost = reported_cost(cost_value, tokens, output_tokens, PRICE)?;
            Ok::<_, String>(Evaluation {
                requests: sent.requests,
                model: MODEL.to_string(),
                answers: parse_answers(payload.get("answers"), questions, true, Some(confidence))?,
                input_tokens: tokens,
                cost: timed(cost, &sent, started),
            })
        })();
        parsed.map_err(|detail| invalid_response(&response, "Invalid gateway response", &detail))
    }
}

impl DecisionClient for VercelGatewayJevClient {
    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(asking_open(
            questions,
            move |asked| async move {
                asking_split(asked, &|batch, attempt, split| Box::pin(self.ask(state, batch, attempt, split)), PRICE)
                    .await
            },
            MODEL,
            PRICE,
        ))
    }
}
