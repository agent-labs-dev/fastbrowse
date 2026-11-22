//! The contract for a decision model, Jev (TypeSafe System One) first: typed questions in and calibrated answers out,
//! each model's [`DecisionProfile`], and the client trait every provider's client implements.

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::BoxFuture;
use crate::config::{Config, JSON_CHARS_PER_TOKEN, Thresholds};
use crate::llm::FaultClass;
use crate::models::{CostComponent, CostLine, FailureKind, JevPurpose, MaybeUnavailable};
use crate::py::lax;
use crate::schema::{Field, ModelTy, Schema, Ty};
use crate::telemetry::{BudgetExceeded, Ledger};

pub const JEV_MODEL: &str = "jev-1.14.0";
pub const MAX_CHOICE_OPTIONS: usize = 255;
/// Input price; output is free.
pub const JEV_DOLLARS_PER_INPUT_TOKEN: f64 = 0.042 / 1_000_000.0;
/// Jev's context for a request and for each question with TypeSafe's long-context header, which OpenRouter and AIMLAPI
/// also document as Jev's context on their routes.
pub const JEV_LONG_CONTEXT_TOKENS: i64 = 262_144;

/// Characters of a decision request counted as one token when a route's larger context is fitted to, where
/// `JSON_CHARS_PER_TOKEN` is how a request of Jev's ordinary size is estimated.
///
/// On its documented 262,144 tokens, `fit` multiplies the budget by `JSON_CHARS_PER_TOKEN` and the state may reach
/// 393,216 characters. A present-day page's text carries base64 and hex that tokenize far denser than that: measured
/// against Jev 1.14 on 2026-11-11 through OpenRouter, one request each over states shaped like `policy::state`,
/// base64 took 1.19 characters a token and hex 1.34, against 2.05 to 2.28 for dense checkout JSON and 2.14 for a cart
/// summary. A base64 state built to 393,216 characters was refused with `max_tokens_exceeded` — the failure in
/// [#959](https://github.com/agent-labs-dev/fastbrowse/issues/959) — while the same content at 288,358 was answered.
/// So a route serving 262k is fitted at 1.1, below every ratio measured, and one serving Jev's ordinary 32k keeps 1.5.
pub const JEV_LONG_CONTEXT_CHARS_PER_TOKEN: f64 = 1.1;

/// Images a decision request carries, keyed by the reference its state writes in each one's place.
pub type Images = IndexMap<String, Vec<u8>>;
/// A request with no images.
pub static NO_IMAGES: std::sync::LazyLock<Images> = std::sync::LazyLock::new(Images::new);

/// The longest side of a Jev image, in pixels.
pub const JEV_IMAGE_SIDE: u32 = 1568;

/// TypeSafe's limit on a request body: the JSON, its base64 image text and their framing together, in bytes. A body
/// past it is refused with 413 and answers nothing, so a request is fitted to it before it is sent.
pub const JEV_REQUEST_BYTES: usize = 32 * 1024 * 1024;

/// The bytes an image adds to a request body sent as JSON: its reference, the asset object around it, and its PNG in
/// base64, which is four characters for every three bytes.
pub fn asset_bytes(reference: &str, png: &[u8]) -> usize {
    let framing = serde_json::json!({reference: {"type": "image", "format": "png", "data_base64": ""}}).to_string();
    // The braces of the map holding the assets are counted once per body, so each asset adds a comma in their place.
    framing.len() - 1 + png.len().div_ceil(3) * 4
}

/// Whether TypeSafe takes `png` as a Jev image: a PNG 280 to 1568 pixels a side and 78,400 to 2,458,624 in all, of at
/// most 16 MiB. An image outside these is refused with the whole request, never resized.
pub fn jev_takes_png(png: &[u8]) -> bool {
    let Some((width, height)) = png_size(png).filter(|_| png.len() <= 16 * 1024 * 1024) else {
        return false;
    };
    (280..=JEV_IMAGE_SIDE as u64).contains(&width)
        && (280..=JEV_IMAGE_SIDE as u64).contains(&height)
        && (78_400..=2_458_624).contains(&(width * height))
}

/// A PNG's width and height in pixels, read from its header.
pub fn png_size(png: &[u8]) -> Option<(u64, u64)> {
    if png.len() < 24 || !png.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let side = |at: usize| u32::from_be_bytes([png[at], png[at + 1], png[at + 2], png[at + 3]]) as u64;
    Some((side(16), side(20)))
}

/// The input tokens Jev counts for one PNG: two for each 28-pixel tile it covers, rounded up on each side, and 13 for
/// the image itself. TypeSafe publishes no image rate and says each image counts toward the context, so this was
/// measured against Jev 1.14 through OpenRouter on 2026-11-22, reading `usage.input_tokens` against the same request
/// without the image: 280x280 added 213, 560x560 813, 1568x882 3,597 and four at 800x600 5,156, each exactly this rule.
/// A step that pictured eight cards was refused with `max_tokens_exceeded` while its text fitted, because nothing
/// counted the pictures ([#1303](https://github.com/agent-labs-dev/fastbrowse/issues/1303)).
pub fn jev_image_tokens(png: &[u8]) -> i64 {
    png_size(png).map_or(0, |(width, height)| (2 * width.div_ceil(28) * height.div_ceil(28) + 13) as i64)
}

/// The image tokens a request carries: each image Jev is sent, once for every place the state or a question refers to
/// it, since Jev counts a picture again for each place it is used.
pub fn image_tokens(images: &Images, state: &Value, questions: &IndexMap<String, Question>) -> i64 {
    if images.is_empty() {
        return 0;
    }
    let written = format!("{state}{}", serde_json::to_string(questions).unwrap_or_default());
    images.iter().map(|(key, png)| written.matches(key.as_str()).count() as i64 * jev_image_tokens(png)).sum()
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

/// `questions` with every image reference left out of each Choice option's criteria, for a route that takes no image
/// or reads none nested in a question. The options themselves stay, described by their text.
pub fn questions_without_images(questions: &IndexMap<String, Question>, images: &Images) -> IndexMap<String, Question> {
    let strip = |question: &Question| match question {
        Question::Choice(choice) => Question::Choice(ChoiceQuestion {
            criteria: choice.criteria.iter().map(|(key, value)| (key.clone(), without_images(value, images))).collect(),
            ..choice.clone()
        }),
        other => other.clone(),
    };
    questions.iter().map(|(key, question)| (key.clone(), strip(question))).collect()
}

/// The images `state` or `questions` refer to. TypeSafe refuses a request with an asset nothing refers to, and a
/// request split into batches carries only some of its questions.
pub fn referenced(images: &Images, state: &Value, questions: &IndexMap<String, Question>) -> Images {
    if images.is_empty() {
        return Images::new();
    }
    let written = format!("{state}{}", serde_json::to_string(questions).unwrap_or_default());
    images.iter().filter(|(key, _)| written.contains(key.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()
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
        let edges = ModelTy::frozen(
            "Edges",
            vec![
                Field::required("top", Ty::Float),
                Field::required("bottom", Ty::Float),
                Field::required("left", Ty::Float),
                Field::required("right", Ty::Float),
            ],
        )
        .ty();
        let boxed = ModelTy::frozen(
            "BoxAnswer",
            vec![
                Field::default("type", BoxTag::ty(), "box"),
                Field::required("box", edges),
                Field::default("confidence", Ty::optional(Ty::Float.le(1).ge(0)), Value::Null),
            ],
        )
        .ty();
        let answer = Ty::Alias(
            "Answer",
            Box::new(Ty::Tagged {
                property: "type",
                variants: vec![("choice", choice), ("noul", noul), ("score", score), ("box", boxed)],
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
    pub cause: JevCause,
}

/// How a Jev call failed, named without anything it carried: the class where the error was made, the provider and
/// purpose where the call was. A Jev failure named only `Jev.Failed` said neither which provider failed nor how (#1372).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JevCause {
    /// None for a failure no call made, such as a question with no options.
    pub class: Option<FaultClass>,
    /// The decision source, as `FASTBROWSE_DECISION_SOURCE` spells it.
    pub provider: Option<String>,
    pub purpose: Option<JevPurpose>,
}

impl JevCause {
    /// `provider/purpose/class`, with `unknown` for a part no one set, or None for a failure no call made.
    fn named(&self) -> Option<String> {
        let class = self.class?;
        let provider = self.provider.as_deref().unwrap_or("unknown");
        let purpose = self.purpose.map_or("unknown", |purpose| purpose.as_str());
        Some(format!("{provider}/{purpose}/{class}"))
    }
}

/// A failed Jev call's message, which can quote the provider's answer, and its content-free cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JevFault {
    pub message: String,
    pub cause: JevCause,
}

impl std::fmt::Display for JevFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl JevFault {
    fn new(message: impl Into<String>) -> JevFault {
        JevFault { message: message.into(), cause: JevCause::default() }
    }
}

/// Transport or validation failure; the request may be retried only if it caused no browser action.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DecisionError {
    #[error("{0}")]
    Failed(JevFault),
    /// The provider rejected the request for exceeding its input token limits.
    #[error("{0}")]
    InputTooLarge(JevFault),
    /// Every attempt failed in transport. Not a provider verdict, so it does not move a run to the backup.
    #[error("{0}")]
    TransportFailed(JevFault),
    #[error("{}", .0.message)]
    RetriesExhausted(Box<RetriesExhausted>),
    /// The provider refused the key (HTTP 401 or 403), which no retry on that provider can change.
    #[error("{0}")]
    Rejected(JevFault),
    /// A request would break a limit, so it was not sent. Python raises `BudgetExceeded` through `evaluate` itself.
    #[error(transparent)]
    Budget(#[from] BudgetExceeded),
    /// The page redrew under the decision that would have sent this request, so it was not sent.
    #[error("the page redrew, so the decision it was for is discarded")]
    Superseded,
}

impl DecisionError {
    pub fn failed(message: impl Into<String>) -> DecisionError {
        DecisionError::Failed(JevFault::new(message))
    }
    pub fn input_too_large(message: impl Into<String>) -> DecisionError {
        DecisionError::InputTooLarge(JevFault::new(message))
    }
    pub fn transport_failed(message: impl Into<String>) -> DecisionError {
        DecisionError::TransportFailed(JevFault::new(message))
    }
    pub fn rejected(message: impl Into<String>) -> DecisionError {
        DecisionError::Rejected(JevFault::new(message))
    }

    fn cause_mut(&mut self) -> Option<&mut JevCause> {
        match self {
            DecisionError::Failed(fault)
            | DecisionError::InputTooLarge(fault)
            | DecisionError::TransportFailed(fault)
            | DecisionError::Rejected(fault) => Some(&mut fault.cause),
            DecisionError::RetriesExhausted(exhausted) => Some(&mut exhausted.cause),
            DecisionError::Budget(_) | DecisionError::Superseded => None,
        }
    }

    fn cause(&self) -> Option<&JevCause> {
        match self {
            DecisionError::Failed(fault)
            | DecisionError::InputTooLarge(fault)
            | DecisionError::TransportFailed(fault)
            | DecisionError::Rejected(fault) => Some(&fault.cause),
            DecisionError::RetriesExhausted(exhausted) => Some(&exhausted.cause),
            DecisionError::Budget(_) | DecisionError::Superseded => None,
        }
    }

    /// This error, classed where it was made.
    pub fn classed(mut self, class: FaultClass) -> DecisionError {
        if let Some(cause) = self.cause_mut() {
            cause.class = Some(class);
        }
        self
    }

    /// This error with the provider and purpose of the call that met it, where no one closer to the call set them.
    pub fn from_call(mut self, provider: &str, purpose: Option<JevPurpose>) -> DecisionError {
        if let Some(cause) = self.cause_mut() {
            cause.provider.get_or_insert_with(|| provider.to_owned());
            if cause.purpose.is_none() {
                cause.purpose = purpose;
            }
        }
        self
    }
}

/// The variant, then the cause of a failed call: a Jev failure's message is the provider's, so it is never part of it.
impl FailureKind for DecisionError {
    fn failure_kind(&self) -> String {
        let variant = match self {
            DecisionError::Failed(_) => "Failed",
            DecisionError::InputTooLarge(_) => "InputTooLarge",
            DecisionError::TransportFailed(_) => "TransportFailed",
            DecisionError::RetriesExhausted(_) => "RetriesExhausted",
            DecisionError::Rejected(_) => "Rejected",
            DecisionError::Budget(exceeded) => return exceeded.failure_kind(),
            DecisionError::Superseded => "Superseded",
        };
        match self.cause().and_then(JevCause::named) {
            Some(cause) => format!("{variant}/{cause}"),
            None => variant.into(),
        }
    }
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

    /// Cached profile for use when OpenRouter's catalog is unavailable or has no entry for the model.
    pub fn on_openrouter(model: &str) -> DecisionProfile {
        let bare = model.strip_prefix("typesafe/").or_else(|| model.strip_prefix("~typesafe/")).unwrap_or(model);
        if bare == "jev" || bare.starts_with("jev-") {
            return DecisionProfile { images: true, ..DecisionProfile::for_model(model).long_context() };
        }
        let Some((_, context, dollars_per_million, images)) =
            OPENROUTER_DECISION_MODELS.iter().find(|(id, ..)| *id == model)
        else {
            return DecisionProfile::unknown();
        };
        DecisionProfile::listed_on_openrouter(model, *context, Some(dollars_per_million / 1_000_000.0), *images)
    }

    fn listed_on_openrouter(model: &str, context: i64, price: Option<f64>, images: bool) -> DecisionProfile {
        DecisionProfile {
            max_largest_tokens: context,
            max_total_tokens: context,
            // OpenRouter caps no decision body below TypeSafe's own limit, which it passes Jev requests through at.
            max_request_bytes: Some(JEV_REQUEST_BYTES as i64),
            dollars_per_input_token: price,
            images,
            ..DecisionProfile::for_model(model)
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
            // A request of the ordinary size keeps the ratio it is estimated at; the longer a route is fitted to, the
            // denser the content that fills it can be, and past Jev's default context that is what overran the limit.
            let fitted = |budget: i64, limit: i64| {
                ((budget as f64 * scale).min(limit as f64) * JEV_LONG_CONTEXT_CHARS_PER_TOKEN) as i64
            };
            (fitted(given.state_plus_largest_question, largest), fitted(given.state_plus_all_questions, total))
        });
        config
    }

    /// The input price of `input_tokens`, or `None` where the model has none.
    pub fn input_dollars(&self, input_tokens: f64) -> Option<f64> {
        self.dollars_per_input_token.map(|price| input_tokens * price)
    }
}

/// OpenRouter's model list from `GET /api/v1/models?output_modalities=decisions`.
#[derive(Deserialize)]
pub struct OpenRouterDecisionCatalog {
    data: Vec<OpenRouterModel>,
}

#[derive(Deserialize)]
struct OpenRouterModel {
    id: String,
    canonical_slug: String,
    alias_target: Option<OpenRouterAliasTarget>,
    context_length: Option<i64>,
    architecture: OpenRouterArchitecture,
    pricing: OpenRouterPricing,
}

#[derive(Deserialize)]
struct OpenRouterAliasTarget {
    slug: String,
}

#[derive(Deserialize)]
struct OpenRouterArchitecture {
    input_modalities: Vec<String>,
    output_modalities: Vec<String>,
}

#[derive(Deserialize)]
struct OpenRouterPricing {
    prompt: Option<String>,
}

impl OpenRouterDecisionCatalog {
    /// Return the concrete model and the limits and price of the requested route. An alias can move, so its
    /// context must come from its own listing before a request is fitted.
    pub fn resolve(&self, model: &str) -> Option<(&str, DecisionProfile)> {
        let entry = self
            .data
            .iter()
            .find(|entry| entry.id == model)
            .or_else(|| self.data.iter().find(|entry| entry.canonical_slug == model))?;
        if !entry.architecture.output_modalities.iter().any(|modality| modality == "decisions") {
            return None;
        }
        let context = entry.context_length.filter(|&tokens| tokens > 0)?;
        let concrete = entry.alias_target.as_ref().map_or(entry.id.as_str(), |target| target.slug.as_str());
        let price = entry
            .pricing
            .prompt
            .as_ref()
            .and_then(|price| price.parse::<f64>().ok())
            .filter(|price| price.is_finite() && *price >= 0.0);
        let images = entry.architecture.input_modalities.iter().any(|modality| modality == "image");
        Some((concrete, DecisionProfile::listed_on_openrouter(concrete, context, price, images)))
    }
}

/// Each decision model OpenRouter lists, as `GET /api/v1/models?output_modalities=decisions` returned it on
/// 2026-11-22: its id, context length in tokens, dollars per million input tokens, and whether it takes image input.
/// Jev is profiled as Jev. Respan's span models (no context listed, conversation spans only) and Clairic (extractions,
/// not decisions) answer no fastbrowse request, so they are left out and fall to the unknown profile.
pub const OPENROUTER_DECISION_MODELS: &[(&str, i64, f64, bool)] = &[
    ("baseten/bison-1", 32768, 0.1, true),
    ("baseten/elephant-1", 16384, 0.035, true),
    ("baseten/groundhog-1", 16384, 0.035, true),
    ("baseten/joey-1", 16384, 0.01, true),
    ("baseten/mammoth-1", 16384, 0.1, true),
    ("cloudflare/clef", 65537, 0.192, true),
    ("cloudflare/clef-flash", 24576, 0.019, true),
    ("cloudflare/clef-omni", 65536, 0.15, true),
    ("google/gemini-decision-1", 262144, 0.1, true),
    ("heygen/heygen-decision-1", 262144, 0.07, true),
    ("heygen/heygen-decision-1-turbo", 262144, 0.042, true),
    ("jaredpalmer/kev-4b", 8192, 0.042, false),
    ("liquid/d1-ultra", 32768, 0.042, true),
    ("liquid/d1.1", 65536, 0.02, true),
    ("liquid/d1.2", 65536, 0.02, true),
    ("liquid/d1.2-ultra", 65536, 0.042, true),
    ("~liquid/d1-latest", 65536, 0.02, true),
    ("~liquid/d1-ultra-latest", 65536, 0.042, true),
    ("microsoft/microsoft-decision-1", 32768, 0.042, false),
    ("microsoft/microsoft-decision-1.1", 32768, 0.042, true),
    ("morph/reflex", 65536, 0.042, true),
    ("nace-ai/drex-max", 262144, 0.06, true),
    ("nace-ai/drex-max-finance", 262144, 0.06, true),
    ("nace-ai/drex-v1.5", 131072, 0.04, false),
    ("nace-ai/drex-v2.0", 131072, 0.06, false),
    ("openai/gpt-6-luna-decisions", 1050000, 0.1, true),
    ("parallel/decisions-fast", 32000, 0.0, false),
    ("perplexity/pplx-decider-flash-v1.2", 262144, 0.01, true),
    ("perplexity/pplx-decider-flash-v1.3:free", 262144, 0.0, true),
    ("perplexity/pplx-decider-v1.2", 262144, 0.02, true),
    ("perplexity/pplx-decider-v1.3:free", 262144, 0.0, true),
    ("~perplexity/pplx-decider-flash-latest", 262144, 0.01, true),
    ("~perplexity/pplx-decider-latest", 262144, 0.02, true),
    ("respan/span-02", 131072, 0.01, true),
    ("scaledown/classify-1", 1000000, 0.04, true),
    ("togethercomputer/tev1-4b-experimental", 32768, 0.042, false),
    ("upstage/solar-decide", 524288, 0.05, false),
    ("upstage/solar-decide-flash", 524288, 0.05, false),
];

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
        Box::pin(async move {
            self.evaluate(&without_images(state, images), &questions_without_images(questions, images)).await
        })
    }

    /// The model this client asks, whose limits fit every request sent to it.
    fn profile(&self) -> DecisionProfile;

    /// The image tokens the model is sent for this request: none on a route that takes no image, otherwise every
    /// reference the request makes. A client that sends a model fewer pictures than the request names says so here,
    /// so the text is not cut for pictures that never go out.
    fn image_tokens(&self, _state: &Value, _questions: &IndexMap<String, Question>, _images: &Images) -> i64 {
        0
    }

    /// The ledger counter a caller reserves before a call for `purpose`, or `None` when the client that answers it
    /// reserves each of its own requests (an LLM, whose repairs and re-asks only it can count).
    fn reserves(&self, _purpose: JevPurpose) -> Option<CostComponent> {
        Some(CostComponent::Jev)
    }
}

/// Reserve `calls` decision calls for `purpose` under the counter `client` charges them to.
pub fn reserve(
    ledger: &Ledger,
    client: &dyn DecisionClient,
    purpose: JevPurpose,
    estimate_dollars: f64,
    calls: i64,
) -> Result<(), BudgetExceeded> {
    match client.reserves(purpose) {
        Some(component) => ledger.reserve(component, estimate_dollars, calls),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TokenBudget;
    use serde_json::json;

    #[test]
    fn a_jev_failure_is_named_by_its_cause_and_never_its_message() {
        let failed = DecisionError::failed("HTTP 500: sk-secret")
            .classed(FaultClass::Http(500))
            .from_call("openrouter", Some(JevPurpose::Verify))
            .from_call("gateway", Some(JevPurpose::Read));
        assert_eq!(failed.failure_kind(), "Failed/openrouter/verify/http-500");
        // A failure no call made, such as a question with no options, keeps the bare variant.
        assert_eq!(
            DecisionError::rejected("q offers no options").from_call("openrouter", None).failure_kind(),
            "Rejected"
        );
        let unstamped = DecisionError::transport_failed("timed out").classed(FaultClass::Transport);
        assert_eq!(unstamped.failure_kind(), "TransportFailed/unknown/unknown/transport");
    }

    #[test]
    fn jev_leaves_the_default_config_as_it_is() {
        assert_eq!(DecisionProfile::jev().fit(None), Config::default());
    }

    #[test]
    fn long_context_fits_a_decision_request_to_it_and_leaves_the_llm_budget() {
        let long = DecisionProfile::jev().long_context();
        let fitted = long.fit(None);
        assert_eq!(fitted.tokens.decision_chars, Some((288_358, 288_358)));
        assert_eq!(fitted.tokens, TokenBudget { decision_chars: fitted.tokens.decision_chars, ..Default::default() });
        // A caller's smaller budget stays as much smaller on the longer route.
        let mut given = Config::default();
        given.tokens.state_plus_largest_question = 2500;
        assert_eq!(long.fit(Some(given)).tokens.decision_chars.unwrap().0, 22_528);
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
    fn jev_counts_an_image_at_two_tokens_a_tile_as_measured() {
        // Measured against Jev 1.14 through OpenRouter on 2026-11-22: the input tokens each image added.
        for ((width, height), tokens) in [((280, 280), 213), ((560, 560), 813), ((1568, 882), 3597), ((800, 600), 1289)]
        {
            assert_eq!(jev_image_tokens(&png(width, height)), tokens, "{width}x{height}");
        }
        assert_eq!(jev_image_tokens(b"not a png"), 0);
        let images = Images::from([("ref-1".to_string(), png(280, 280))]);
        let state = json!({"screenshot": "ref-1", "cards": ["ref-1"]});
        assert_eq!(image_tokens(&images, &state, &IndexMap::new()), 2 * 213, "counted for each place it is used");
    }

    #[test]
    fn pictures_take_their_tokens_from_the_room_left_for_text() {
        let fitted = DecisionProfile::jev().long_context().fit(None).tokens;
        let room = fitted.input_chars(&[""]);
        assert!((room - fitted.beside_images(3597).input_chars(&[""]) - 3957).abs() <= 1);
    }

    #[test]
    fn openrouter_fits_each_model_to_the_context_it_lists() {
        let fitted = |model: &str| DecisionProfile::on_openrouter(model).fit(None).tokens;
        let microsoft = fitted("microsoft/microsoft-decision-1.1");
        assert_eq!((microsoft.state_plus_largest_question, microsoft.decision_chars), (32_000, Some((36_044, 36_044))));
        assert_eq!(fitted("cloudflare/clef-flash").state_plus_largest_question, 24_576);
        assert_eq!(DecisionProfile::on_openrouter("jev-1.14").max_largest_tokens, JEV_LONG_CONTEXT_TOKENS);
        assert_eq!(DecisionProfile::on_openrouter("google/gemini-decision-1").max_largest_tokens, 262_144);
        assert_eq!(DecisionProfile::on_openrouter("vendor/unlisted"), DecisionProfile::unknown());
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
    fn a_route_without_images_is_asked_each_option_by_its_text_and_a_request_carries_only_what_it_names() {
        let images = Images::from([("ref-1".to_string(), png(300, 300)), ("ref-2".to_string(), png(300, 300))]);
        let pick: Question = ChoiceQuestion::new(
            "Pick",
            IndexMap::from([
                ("a".to_string(), json!({"label": "Add", "looks_like": "ref-1"})),
                ("b".to_string(), json!({"label": "Add"})),
            ]),
        )
        .into();
        let questions = IndexMap::from([("click_target".to_string(), pick)]);
        let Question::Choice(stripped) = &questions_without_images(&questions, &images)["click_target"] else {
            panic!("a choice")
        };
        assert_eq!(stripped.criteria["a"], json!({"label": "Add"}));
        assert_eq!(
            referenced(&images, &json!({"text": "none here"}), &questions).keys().collect::<Vec<_>>(),
            ["ref-1"]
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
        given.thresholds.bot_check_above = 0.4;
        let fitted = DecisionProfile::unknown().fit(Some(given));
        assert_eq!(fitted.thresholds.bot_check_above, 0.4);
        assert_eq!(fitted.observation.group_size, 26);
    }
}
