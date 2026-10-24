//! Port of `src/fastbrowse/jev.py`: the contract for Jev (TypeSafe System One), typed questions in and calibrated
//! answers out, and the client trait the TypeSafe, OpenRouter and Vercel clients implement.

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::BoxFuture;
use crate::models::{CostLine, MaybeUnavailable};
use crate::py::lax;
use crate::schema::{Field, ModelTy, Schema, Ty};

pub const JEV_MODEL: &str = "jev-1.13.0";
pub const MAX_CHOICE_OPTIONS: usize = 255;
/// Input price; output is free.
pub const JEV_DOLLARS_PER_INPUT_TOKEN: f64 = 0.042 / 1_000_000.0;

literal_tag!(ChoiceTag = "choice");
literal_tag!(NoulTag = "noul");
literal_tag!(ScoreTag = "score");

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

/// `ChoiceQuestion | NoulQuestion | ScoreQuestion`, discriminated by `type`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Question {
    Choice(ChoiceQuestion),
    Noul(NoulQuestion),
    Score(ScoreQuestion),
}

/// Reads the `type` member first, as a pydantic discriminated union does, so an error names the right model.
fn by_type<'de, D: Deserializer<'de>, T>(
    d: D,
    choice: impl FnOnce(Value) -> Result<T, serde_json::Error>,
    noul: impl FnOnce(Value) -> Result<T, serde_json::Error>,
    score: impl FnOnce(Value) -> Result<T, serde_json::Error>,
) -> Result<T, D::Error> {
    let value = Value::deserialize(d)?;
    let parsed = match value.get("type").and_then(Value::as_str) {
        Some("choice") => choice(value),
        Some("noul") => noul(value),
        Some("score") => score(value),
        Some(_) => {
            return Err(serde::de::Error::custom(
                "Input tag does not match any of the expected tags: 'choice', 'noul', 'score'",
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

/// `ChoiceAnswer | NoulAnswer | ScoreAnswer`, discriminated by `type`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Answer {
    Choice(ChoiceAnswer),
    Noul(NoulAnswer),
    Score(ScoreAnswer),
}

impl<'de> Deserialize<'de> for Answer {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        by_type(
            d,
            |v| serde_json::from_value(v).map(Answer::Choice),
            |v| serde_json::from_value(v).map(Answer::Noul),
            |v| serde_json::from_value(v).map(Answer::Score),
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
pub enum JevError {
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
}

impl MaybeUnavailable for JevError {
    fn is_unavailable(&self) -> bool {
        matches!(self, JevError::TransportFailed(_) | JevError::RetriesExhausted(_))
    }
}

pub trait JevClient: Send + Sync {
    /// Answer every question against one state. Fails with `InputTooLarge` or another `JevError`.
    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, JevError>>;
}
