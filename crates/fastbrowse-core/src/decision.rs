//! The contract for a decision model, first ported from `src/fastbrowse/jev.py` for Jev (TypeSafe System One): typed
//! questions in and calibrated answers out, each model's [`DecisionProfile`], and the client trait every provider's
//! client implements.

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::BoxFuture;
use crate::config::{Config, JSON_CHARS_PER_TOKEN, Thresholds};
use crate::models::{CostLine, MaybeUnavailable};
use crate::py::lax;
use crate::schema::{Field, ModelTy, Schema, Ty};
use crate::telemetry::BudgetExceeded;

pub const JEV_MODEL: &str = "jev-1.14.0";
pub const MAX_CHOICE_OPTIONS: usize = 255;
/// Input price; output is free.
pub const JEV_DOLLARS_PER_INPUT_TOKEN: f64 = 0.042 / 1_000_000.0;
/// Jev's context for a request and for each question with TypeSafe's long-context header, which OpenRouter and AIMLAPI
/// also document as Jev's context on their routes.
pub const JEV_LONG_CONTEXT_TOKENS: i64 = 262_144;

/// Images a decision request carries, keyed by the reference its state writes in each one's place.
pub type Images = IndexMap<String, Vec<u8>>;
/// A request with no images.
pub static NO_IMAGES: std::sync::LazyLock<Images> = std::sync::LazyLock::new(Images::new);

/// The longest side of a Jev image, in pixels.
pub const JEV_IMAGE_SIDE: u32 = 1568;

/// Whether TypeSafe takes `png` as a Jev image: a PNG 280 to 1568 pixels a side and 78,400 to 2,458,624 in all, of at
/// most 16 MiB. An image outside these is refused with the whole request, never resized.
pub fn jev_takes_png(png: &[u8]) -> bool {
    if png.len() < 24 || !png.starts_with(b"\x89PNG\r\n\x1a\n") || png.len() > 16 * 1024 * 1024 {
        return false;
    }
    let side = |at: usize| u32::from_be_bytes([png[at], png[at + 1], png[at + 2], png[at + 3]]) as u64;
    let (width, height) = (side(16), side(20));
    (280..=JEV_IMAGE_SIDE as u64).contains(&width)
        && (280..=JEV_IMAGE_SIDE as u64).contains(&height)
        && (78_400..=2_458_624).contains(&(width * height))
}

/// `state` with every value that is an image reference left out, for a route that takes no image.
pub fn without_images(state: &Value, images: &Images) -> Value {
    let reference = |value: &Value| value.as_str().is_some_and(|text| images.contains_key(text));
    match state {
        Value::Object(map) => Value::Object(
            map.iter().filter(|(_, v)| !reference(v)).map(|(k, v)| (k.clone(), without_images(v, images))).collect(),
        ),
        Value::Array(items) => {
            Value::Array(items.iter().filter(|v| !reference(v)).map(|v| without_images(v, images)).collect())
        }
        other => other.clone(),
    }
}

literal_tag!(ChoiceTag = "choice");
literal_tag!(NoulTag = "noul");
literal_tag!(ScoreTag = "score");
literal_tag!(BoxTag = "box");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChoiceQuestion {
    #[serde(default, rename = "type")]
    pub kind: ChoiceTag,
    pub instructions: String,
    /// Option key -> description (string, object with what/not_for/examples, or null).
    pub criteria: IndexMap<String, Value>,
}

impl ChoiceQuestion {
    pub fn new(instructions: impl Into<String>, criteria: IndexMap<String, Value>) -> Self {
        ChoiceQuestion { kind: ChoiceTag, instructions: instructions.into(), criteria }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoulQuestion {
    #[serde(default, rename = "type")]
    pub kind: NoulTag,
    pub instructions: String,
    #[serde(default)]
    pub r#true: Option<String>,
    #[serde(default)]
    pub r#false: Option<String>,
}

impl NoulQuestion {
    pub fn new(instructions: impl Into<String>, yes: Option<&str>, no: Option<&str>) -> Self {
        NoulQuestion {
            kind: NoulTag,
            instructions: instructions.into(),
            r#true: yes.map(str::to_string),
            r#false: no.map(str::to_string),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScoreQuestion {
    #[serde(default, rename = "type")]
    pub kind: ScoreTag,
    pub instructions: String,
    #[serde(deserialize_with = "score_criteria")]
    pub criteria: Vec<String>,
}

fn score_criteria<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let criteria = Vec::<String>::deserialize(d)?;
    match criteria.len() {
        0..2 => Err(serde::de::Error::custom("Tuple should have at least 2 items after validation")),
        11.. => Err(serde::de::Error::custom("Tuple should have at most 10 items after validation")),
        _ => Ok(criteria),
    }
}

/// Where something is in an image the state refers to. It takes no criteria and always answers with a box, so it is
/// asked beside a NOUL on whether the thing is there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoxQuestion {
    #[serde(default, rename = "type")]
    pub kind: BoxTag,
    pub instructions: String,
}

/// `ChoiceQuestion | NoulQuestion | ScoreQuestion | BoxQuestion`, discriminated by `type`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Question {
    Choice(ChoiceQuestion),
    Noul(NoulQuestion),
    Score(ScoreQuestion),
    Box(BoxQuestion),
}

/// Reads the `type` member first, as a pydantic discriminated union does, so an error names the right model.
fn by_type<'de, D: Deserializer<'de>, T>(
    d: D,
    choice: impl FnOnce(Value) -> Result<T, serde_json::Error>,
    noul: impl FnOnce(Value) -> Result<T, serde_json::Error>,
    score: impl FnOnce(Value) -> Result<T, serde_json::Error>,
    boxed: impl FnOnce(Value) -> Result<T, serde_json::Error>,
) -> Result<T, D::Error> {
    let value = Value::deserialize(d)?;
    let parsed = match value.get("type").and_then(Value::as_str) {
        Some("choice") => choice(value),
        Some("noul") => noul(value),
        Some("score") => score(value),
        Some("box") => boxed(value),
        Some(_) => {
            return Err(serde::de::Error::custom(
                "Input tag does not match any of the expected tags: 'choice', 'noul', 'score', 'box'",
            ));
        }
        None => return Err(serde::de::Error::custom("Unable to extract tag using discriminator 'type'")),
    };
    parsed.map_err(serde::de::Error::custom)
}

impl<'de> Deserialize<'de> for Question {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        by_type(
            d,
            |v| serde_json::from_value(v).map(Question::Choice),
            |v| serde_json::from_value(v).map(Question::Noul),
            |v| serde_json::from_value(v).map(Question::Score),
            |v| serde_json::from_value(v).map(Question::Box),
        )
    }
}

impl From<ChoiceQuestion> for Question {
    fn from(q: ChoiceQuestion) -> Self {
        Question::Choice(q)
    }
}
impl From<NoulQuestion> for Question {
    fn from(q: NoulQuestion) -> Self {
        Question::Noul(q)
    }
}
impl From<ScoreQuestion> for Question {
    fn from(q: ScoreQuestion) -> Self {
        Question::Score(q)
    }
}
impl From<BoxQuestion> for Question {
    fn from(q: BoxQuestion) -> Self {
        Question::Box(q)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChoiceAnswer {
    #[serde(default, rename = "type")]
    pub kind: ChoiceTag,
    pub choice: String,
    pub probabilities: IndexMap<String, f64>,
    #[serde(deserialize_with = "lax::float_unit")]
    pub confidence: f64,
}

/// Jev's own answer to a noul: yes above even odds. A question answered this way has no bar of ours to tune.
pub const EVEN_ODDS: f64 = 0.5;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoulAnswer {
    #[serde(default, rename = "type")]
    pub kind: NoulTag,
    #[serde(deserialize_with = "lax::float_unit")]
    pub probability: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScoreAnswer {
    #[serde(default, rename = "type")]
    pub kind: ScoreTag,
    /// Probability-weighted mean over the levels, so fractional (e.g. 1.3).
    #[serde(deserialize_with = "lax::float")]
    pub score: f64,
    pub probabilities: IndexMap<String, f64>,
    #[serde(deserialize_with = "lax::float_unit")]
    pub confidence: f64,
}

/// The edges of a box, each a fraction of the image from its top-left corner.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edges {
    pub top: f64,
    pub bottom: f64,
    pub left: f64,
    pub right: f64,
}

impl Edges {
    /// The centre, as fractions of the image's width and height.
    pub fn centre(&self) -> (f64, f64) {
        ((self.left + self.right) / 2.0, (self.top + self.bottom) / 2.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoxAnswer {
    #[serde(default, rename = "type")]
    pub kind: BoxTag,
    #[serde(rename = "box")]
    pub edges: Edges,
    /// TypeSafe documents this as null when the model gives no confidence for the box.
    #[serde(default, deserialize_with = "lax::opt_float_unit")]
    pub confidence: Option<f64>,
}

/// `ChoiceAnswer | NoulAnswer | ScoreAnswer | BoxAnswer`, discriminated by `type`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Answer {
    Choice(ChoiceAnswer),
    Noul(NoulAnswer),
    Score(ScoreAnswer),
    Box(BoxAnswer),
}

impl<'de> Deserialize<'de> for Answer {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        by_type(
            d,
            |v| serde_json::from_value(v).map(Answer::Choice),
            |v| serde_json::from_value(v).map(Answer::Noul),
            |v| serde_json::from_value(v).map(Answer::Score),
            |v| serde_json::from_value(v).map(Answer::Box),
        )
    }
}

impl Answer {
    /// The NOUL probability, or `None` for another kind of answer.
    pub fn probability(&self) -> Option<f64> {
        match self {
            Answer::Noul(answer) => Some(answer.probability),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evaluation {
    pub model: String,
    pub answers: IndexMap<String, Answer>,
    #[serde(deserialize_with = "lax::int_ge0")]
    pub input_tokens: i64,
    pub cost: CostLine,
    /// HTTP requests, including retries, made to obtain these answers.
    #[serde(default = "one", deserialize_with = "lax::int_ge0")]
    pub requests: i64,
}

fn one() -> i64 {
    1
}

impl Schema for Evaluation {
    fn ty() -> Ty {
        let probabilities = || Field::required("probabilities", Ty::map(Ty::Float));
        let confidence = || Field::required("confidence", Ty::Float.le(1).ge(0));
        let choice = ModelTy::frozen(
            "ChoiceAnswer",
            vec![
                Field::default("type", ChoiceTag::ty(), "choice"),
                Field::required("choice", Ty::Str),
                probabilities(),
                confidence(),
            ],
        )
        .ty();
        let noul = ModelTy::frozen(
            "NoulAnswer",
            vec![Field::default("type", NoulTag::ty(), "noul"), Field::required("probability", Ty::Float.le(1).ge(0))],
        )
        .ty();
        let score = ModelTy::frozen(
            "ScoreAnswer",
            vec![
                Field::default("type", ScoreTag::ty(), "score"),
                Field::required("score", Ty::Float)
                    .doc("Probability-weighted mean over the levels, so fractional (e.g. 1.3)."),
                probabilities(),
                confidence(),
            ],
        )
        .ty();
        let answer = Ty::Alias(
            "Answer",
            Box::new(Ty::Tagged {
                property: "type",
                variants: vec![("choice", choice), ("noul", noul), ("score", score)],
            }),
        );
        ModelTy::frozen(
            "Evaluation",
            vec![
                Field::required("model", Ty::Str),
                Field::required("answers", Ty::map(answer)),
                Field::required("input_tokens", Ty::Int.ge(0)),
                Field::required("cost", <CostLine as Schema>::ty()),
                Field::default("requests", Ty::Int.ge(0), 1)
                    .doc("HTTP requests, including retries, made to obtain these answers."),
            ],
        )
        .ty()
    }
}

/// `JevRetriesExhausted`: a retryable HTTP status outlasted the provider's retry budget.
#[derive(Debug, Clone, PartialEq)]
pub struct RetriesExhausted {
    pub message: String,
    pub seconds: f64,
    pub unaccounted_requests: i64,
    pub requests: i64,
    /// Singles answered before a split batch ran out of retries; paid for, and kept by a failover.
    pub answered: Vec<Evaluation>,
    /// The last failed response, so a caller that sent more requests can restate the count around it.
    pub last: String,
}

/// Transport or validation failure; the request may be retried only if it caused no browser action.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DecisionError {
    #[error("{0}")]
    Failed(String),
    /// The provider rejected the request for exceeding its input token limits.
    #[error("{0}")]
    InputTooLarge(String),
    /// Every attempt failed in transport. Not a provider verdict, so it does not move a run to the backup.
    #[error("{0}")]
    TransportFailed(String),
    #[error("{}", .0.message)]
    RetriesExhausted(Box<RetriesExhausted>),
    /// The provider refused the key (HTTP 401 or 403), which no retry on that provider can change.
    #[error("{0}")]
    Rejected(String),
    /// A request would break a limit, so it was not sent. Python raises `BudgetExceeded` through `evaluate` itself.
    #[error(transparent)]
    Budget(#[from] BudgetExceeded),
}

impl MaybeUnavailable for DecisionError {
    fn is_unavailable(&self) -> bool {
        matches!(self, DecisionError::TransportFailed(_) | DecisionError::RetriesExhausted(_))
    }
}

/// What one decision model accepts and costs, and the thresholds measured on it.
///
/// A limit is the model's, so it bounds a caller's [`Config`] too; thresholds are a starting point, so they apply only
/// when the caller brings none.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionProfile {
    /// Most options one Choice may offer.
    pub max_choice_options: usize,
    /// Most levels one Score may have.
    pub max_score_levels: usize,
    /// Most questions one request may carry; `None` where the provider documents no cap.
    pub max_questions: Option<usize>,
    /// Input tokens allowed for the state plus its largest question.
    pub max_largest_tokens: i64,
    /// Input tokens allowed for the state plus every question.
    pub max_total_tokens: i64,
    /// Largest request body, where the server caps bytes rather than tokens.
    pub max_request_bytes: Option<i64>,
    /// List price; `None` where none is published, so an unmetered call is an unknown cost.
    pub dollars_per_input_token: Option<f64>,
    /// Whether this route is sent image input: TypeSafe's own API and OpenRouter are, a backup is not.
    pub images: bool,
    /// Thresholds tuned on the evals against this model; `None` when it was never measured.
    pub thresholds: Option<Thresholds>,
}

impl DecisionProfile {
    /// Jev 1.13 and 1.14, at the limits and price TypeSafe documents for 1.13, which 1.14 keeps.
    pub fn jev() -> DecisionProfile {
        DecisionProfile {
            max_choice_options: MAX_CHOICE_OPTIONS,
            max_score_levels: 10,
            max_questions: None,
            max_largest_tokens: 32_000,
            max_total_tokens: 64_000,
            max_request_bytes: None,
            dollars_per_input_token: Some(JEV_DOLLARS_PER_INPUT_TOKEN),
            images: false,
            thresholds: Some(Thresholds::default()),
        }
    }

    /// A model fastbrowse has no record of: the tightest limits any documented System One host sets, no price, and no
    /// thresholds of its own.
    pub fn unknown() -> DecisionProfile {
        DecisionProfile {
            max_choice_options: 26,
            max_score_levels: 10,
            max_questions: Some(64),
            max_largest_tokens: 32_000,
            max_total_tokens: 64_000,
            max_request_bytes: Some(64 * 1024),
            dollars_per_input_token: None,
            images: false,
            thresholds: None,
        }
    }

    fn perplexity(dollars_per_million: f64) -> DecisionProfile {
        DecisionProfile {
            max_choice_options: 255,
            max_score_levels: 10,
            max_questions: Some(128),
            max_largest_tokens: 256_000,
            max_total_tokens: 256_000,
            max_request_bytes: Some(32 * 1024 * 1024),
            dollars_per_input_token: Some(dollars_per_million / 1_000_000.0),
            images: false,
            thresholds: None,
        }
    }

    /// The profile of a model id as a provider names it.
    pub fn for_model(model: &str) -> DecisionProfile {
        let bare = model.rsplit('/').next().unwrap_or(model);
        if bare == "jev" || bare.starts_with("jev-1.13") || bare.starts_with("jev-1.14") {
            return DecisionProfile::jev();
        }
        // Another Jev release keeps the family's limits and price, but nothing was tuned on it.
        if bare.starts_with("jev-") {
            return DecisionProfile { thresholds: None, ..DecisionProfile::jev() };
        }
        // Perplexity and AIMLAPI also take a model under its maker's prefix.
        let name = ["perplexity/", "openai/"].iter().find_map(|prefix| model.strip_prefix(prefix)).unwrap_or(model);
        match name {
            // Telnyx documents 64 questions a request and 64 options a choice or levels a score, and no price.
            "telnyx/decision-flash" | "telnyx/decision-pro" => DecisionProfile {
                max_choice_options: 64,
                max_score_levels: 64,
                max_request_bytes: None,
                ..DecisionProfile::unknown()
            },
            // OpenAI documents 400 questions a request, 2 to 10 levels, $0.10 per million input tokens and image input;
            // Pydantic AI's profile caps a choice at 255 options. No token limit is stated, so a request keeps Jev's
            // default budget.
            "gpt-6-luna" => DecisionProfile {
                max_choice_options: 255,
                max_score_levels: 10,
                max_questions: Some(400),
                max_largest_tokens: 32_000,
                max_total_tokens: 64_000,
                max_request_bytes: None,
                dollars_per_input_token: Some(0.10 / 1_000_000.0),
                images: false,
                thresholds: None,
            },
            // Perplexity documents 128 questions a request, 255 options, 10 levels, inputs under 256K tokens, image input
            // and these prices; its legacy names bill as v1.2.
            "pplx-decider-v1.2" | "pplx-decider-latest" | "pplx-decider-v1.1-27b" | "pplx-decider-v1-27b" => {
                DecisionProfile::perplexity(0.02)
            }
            "pplx-decider-flash-v1.2" | "pplx-decider-flash-latest" => DecisionProfile::perplexity(0.01),
            // AIMLAPI lists a 32,768-token context and image input, and no other limit.
            "liquid/d1" => DecisionProfile { max_total_tokens: 32_768, ..DecisionProfile::unknown() },
            // Pydantic AI's Ollama profile: 26 options, 26 levels, 64 questions and 64 KiB.
            "nimble" | "tev1" => DecisionProfile { max_score_levels: 26, ..DecisionProfile::unknown() },
            _ => DecisionProfile::unknown(),
        }
    }

    /// This model at Jev's long context, for a route that serves it.
    pub fn long_context(self) -> DecisionProfile {
        DecisionProfile {
            max_largest_tokens: JEV_LONG_CONTEXT_TOKENS,
            max_total_tokens: JEV_LONG_CONTEXT_TOKENS,
            ..self
        }
    }

    /// Whether the thresholds a run would start from were measured on this model.
    pub fn validated(&self) -> bool {
        self.thresholds.is_some()
    }

    /// The configuration a run uses against this model: the caller's, or this model's thresholds when there is none,
    /// with every limit brought within the model's.
    pub fn fit(&self, config: Option<Config>) -> Config {
        let mut config = config
            .unwrap_or_else(|| Config { thresholds: self.thresholds.clone().unwrap_or_default(), ..Config::default() });
        let given = config.tokens.clone();
        let observation = &mut config.observation;
        observation.group_size = observation.group_size.min(self.max_choice_options.max(2) as i64);
        let mut total = self.max_total_tokens;
        if let Some(bytes) = self.max_request_bytes {
            total = total.min((bytes as f64 / JSON_CHARS_PER_TOKEN) as i64);
        }
        let largest = self.max_largest_tokens.min(total);
        let tokens = &mut config.tokens;
        tokens.state_plus_largest_question = tokens.state_plus_largest_question.min(largest).max(1);
        tokens.state_plus_all_questions = tokens.state_plus_all_questions.min(total).max(1);
        // A budget is written against Jev's default context, so a route that documents more scales it to its own;
        // an LLM prompt keeps the budget as given.
        let scale = self.max_largest_tokens as f64 / DecisionProfile::jev().max_largest_tokens as f64;
        tokens.decision_chars = (scale > 1.0).then(|| {
            let fitted =
                |budget: i64, limit: i64| ((budget as f64 * scale).min(limit as f64) * JSON_CHARS_PER_TOKEN) as i64;
            (fitted(given.state_plus_largest_question, largest), fitted(given.state_plus_all_questions, total))
        });
        config
    }

    /// The input price of `input_tokens`, or `None` where the model has none.
    pub fn input_dollars(&self, input_tokens: f64) -> Option<f64> {
        self.dollars_per_input_token.map(|price| input_tokens * price)
    }
}

pub trait DecisionClient: Send + Sync {
    /// Answer every question against one state. Fails with `InputTooLarge` or another `DecisionError`.
    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>>;

    /// `evaluate` against a state that refers to `images` by their keys. A route that takes no image answers from the
    /// rest of the state.
    fn evaluate_images<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
        images: &'a Images,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async move { self.evaluate(&without_images(state, images), questions).await })
    }

    /// The model this client asks, whose limits fit every request sent to it.
    fn profile(&self) -> DecisionProfile;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TokenBudget;
    use serde_json::json;

    #[test]
    fn jev_leaves_the_default_config_as_it_is() {
        assert_eq!(DecisionProfile::jev().fit(None), Config::default());
    }

    #[test]
    fn long_context_fits_a_decision_request_to_it_and_leaves_the_llm_budget() {
        let long = DecisionProfile::jev().long_context();
        let fitted = long.fit(None);
        assert_eq!(fitted.tokens.decision_chars, Some((393_216, 393_216)));
        assert_eq!(fitted.tokens, TokenBudget { decision_chars: fitted.tokens.decision_chars, ..Default::default() });
        // A caller's smaller budget stays as much smaller on the longer route.
        let mut given = Config::default();
        given.tokens.state_plus_largest_question = 2500;
        assert_eq!(long.fit(Some(given)).tokens.decision_chars.unwrap().0, 30_720);
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend(width.to_be_bytes());
        png.extend(height.to_be_bytes());
        png
    }

    #[test]
    fn a_png_is_sent_only_within_typesafes_image_limits() {
        assert!(jev_takes_png(&png(1280, 900)));
        assert!(jev_takes_png(&png(280, 280)) && jev_takes_png(&png(1568, 1568)));
        assert!(!jev_takes_png(&png(2560, 1800)), "a HiDPI viewport");
        assert!(!jev_takes_png(&png(1280, 279)));
        assert!(!jev_takes_png(b"\xff\xd8\xff\xe0 a JPEG is not asked about here"));
    }

    #[test]
    fn a_route_without_images_is_asked_the_rest_of_the_state() {
        let images = Images::from([("ref-1".to_string(), png(300, 300))]);
        let state = json!({"page": {"text": "ref-1 is text here"}, "screenshot": "ref-1", "frames": ["ref-1", "kept"]});
        assert_eq!(
            without_images(&state, &images),
            json!({"page": {"text": "ref-1 is text here"}, "frames": ["kept"]})
        );
    }

    #[test]
    fn a_pinned_or_routed_jev_is_jev() {
        for model in
            ["jev-1.13", "jev-1.14", "jev-1.14.0", "typesafe/jev-1.14", "typesafe-ai/jev", "typesafe-ai/jev-1.14.0"]
        {
            assert!(DecisionProfile::for_model(model).validated(), "{model}");
        }
        assert!(!DecisionProfile::for_model("nimble").validated());
        assert_eq!(DecisionProfile::for_model("telnyx/decision-flash").max_choice_options, 64);
        assert_eq!(DecisionProfile::for_model("nimble").max_score_levels, 26);
        assert_eq!(DecisionProfile::for_model("openai/gpt-6-luna").max_questions, Some(400));
        let flash = DecisionProfile::for_model("perplexity/pplx-decider-flash-latest");
        assert_eq!((flash.max_questions, flash.dollars_per_input_token), (Some(128), Some(0.01 / 1_000_000.0)));
        assert_eq!(DecisionProfile::for_model("gpt-6-luna").fit(None), DecisionProfile::jev().fit(None));
        let later = DecisionProfile::for_model("typesafe/jev-1.15");
        assert_eq!((later.validated(), later.max_choice_options), (false, MAX_CHOICE_OPTIONS));
    }

    #[test]
    fn a_small_model_bounds_the_choices_offered_and_the_request() {
        let config = DecisionProfile::unknown().fit(None);
        assert_eq!(config.observation.group_size, 26);
        // 64 KiB of dense JSON, counted as the request is fitted.
        assert_eq!(config.tokens.state_plus_all_questions, 43_690);
        assert_eq!(config.tokens.state_plus_largest_question, 32_000);
    }

    #[test]
    fn a_callers_thresholds_stand_and_its_limits_are_bounded() {
        let mut given = Config::default();
        given.thresholds.recover_below = 0.4;
        let fitted = DecisionProfile::unknown().fit(Some(given));
        assert_eq!(fitted.thresholds.recover_below, 0.4);
        assert_eq!(fitted.observation.group_size, 26);
    }
}
