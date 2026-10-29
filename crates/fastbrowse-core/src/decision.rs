//! The contract for a decision model, first ported from `src/fastbrowse/jev.py` for Jev (TypeSafe System One): typed
//! questions in and calibrated answers out, each model's [`DecisionProfile`], and the client trait every provider's
//! client implements.

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::BoxFuture;
use crate::config::{Config, Thresholds};
use crate::models::{CostLine, MaybeUnavailable};
use crate::py::lax;
use crate::schema::{Field, ModelTy, Schema, Ty};
use crate::telemetry::BudgetExceeded;

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
    /// Whether the model may be sent image input.
    pub images: bool,
    /// Thresholds tuned on the evals against this model; `None` when it was never measured.
    pub thresholds: Option<Thresholds>,
}

/// Share of a provider's token limit a request is budgeted to, because tokens are estimated locally.
const TOKEN_HEADROOM: f64 = 0.75;

impl DecisionProfile {
    /// Jev 1.13, whose limits and price TypeSafe documents and on which every default threshold was tuned.
    pub fn jev() -> DecisionProfile {
        DecisionProfile {
            max_choice_options: MAX_CHOICE_OPTIONS,
            max_score_levels: 10,
            max_questions: None,
            max_largest_tokens: 32_000,
            max_total_tokens: 64_000,
            max_request_bytes: None,
            dollars_per_input_token: Some(JEV_DOLLARS_PER_INPUT_TOKEN),
            images: true,
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

    /// The profile of a model id as a provider names it.
    pub fn for_model(model: &str) -> DecisionProfile {
        let bare = model.rsplit('/').next().unwrap_or(model);
        if bare == "jev" || bare.starts_with("jev-1.13") {
            return DecisionProfile::jev();
        }
        // Another Jev release keeps the family's limits and price, but nothing was tuned on it.
        if bare.starts_with("jev-") {
            return DecisionProfile { thresholds: None, ..DecisionProfile::jev() };
        }
        match model {
            // Telnyx documents 64 questions a request and 64 options a choice or levels a score, and no price.
            "telnyx/decision-flash" | "telnyx/decision-pro" => DecisionProfile {
                max_choice_options: 64,
                max_score_levels: 64,
                max_request_bytes: None,
                ..DecisionProfile::unknown()
            },
            // OpenAI documents 400 questions a request, 2 to 10 levels, $0.10 per million input tokens and image input;
            // Pydantic AI's profile caps a choice at 255 options. No token limit is stated, so the configured budget
            // stands.
            "gpt-6-luna" | "openai/gpt-6-luna" => DecisionProfile {
                max_choice_options: 255,
                max_score_levels: 10,
                max_questions: Some(400),
                max_largest_tokens: 1_000_000,
                max_total_tokens: 1_000_000,
                max_request_bytes: None,
                dollars_per_input_token: Some(0.10 / 1_000_000.0),
                images: true,
                thresholds: None,
            },
            // Pydantic AI's Ollama profile: 26 options, 26 levels, 64 questions and 64 KiB.
            "nimble" | "tev1" => DecisionProfile { max_score_levels: 26, ..DecisionProfile::unknown() },
            _ => DecisionProfile::unknown(),
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
        let options = self.max_choice_options.max(2) as i64;
        let observation = &mut config.observation;
        observation.max_choice_options = observation.max_choice_options.min(options);
        observation.group_size = observation.group_size.min(observation.max_choice_options);
        // A group choice offers at most `max_choice_options` groups of `group_size`, so a denser shortlist cannot be
        // asked at all.
        observation.max_offered_controls =
            observation.max_offered_controls.min(observation.group_size * observation.max_choice_options);
        let mut total = self.max_total_tokens as f64;
        if let Some(bytes) = self.max_request_bytes {
            total = total.min(bytes as f64 / config.tokens.chars_per_token);
        }
        let largest = (self.max_largest_tokens as f64).min(total);
        let tokens = &mut config.tokens;
        tokens.state_plus_largest_question =
            tokens.state_plus_largest_question.min((largest * TOKEN_HEADROOM) as i64).max(1);
        tokens.state_plus_all_questions = tokens.state_plus_all_questions.min((total * TOKEN_HEADROOM) as i64).max(1);
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

    /// The model this client asks. A client that does not say is taken to ask Jev.
    fn profile(&self) -> DecisionProfile {
        DecisionProfile::jev()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jev_leaves_the_default_config_as_it_is() {
        assert_eq!(DecisionProfile::jev().fit(None), Config::default());
    }

    #[test]
    fn a_pinned_or_routed_jev_is_jev() {
        for model in ["jev-1.13", "jev-1.13.0", "typesafe/jev-1.13", "typesafe-ai/jev"] {
            assert!(DecisionProfile::for_model(model).validated(), "{model}");
        }
        assert!(!DecisionProfile::for_model("nimble").validated());
        assert_eq!(DecisionProfile::for_model("telnyx/decision-flash").max_choice_options, 64);
        assert_eq!(DecisionProfile::for_model("nimble").max_score_levels, 26);
        assert_eq!(DecisionProfile::for_model("gpt-6-luna").fit(None), Config::default());
        let later = DecisionProfile::for_model("typesafe/jev-1.14");
        assert_eq!((later.validated(), later.max_choice_options), (false, MAX_CHOICE_OPTIONS));
    }

    #[test]
    fn a_small_model_bounds_the_choices_offered_and_the_request() {
        let config = DecisionProfile::unknown().fit(None);
        assert_eq!(config.observation.max_choice_options, 26);
        assert_eq!(config.observation.group_size, 26);
        assert_eq!(config.observation.max_offered_controls, 160);
        // 64 KiB at three characters a token, budgeted to three quarters.
        assert_eq!(config.tokens.state_plus_all_questions, 16_384);
        assert_eq!(config.tokens.state_plus_largest_question, 16_384);
    }

    #[test]
    fn a_callers_thresholds_stand_and_its_limits_are_bounded() {
        let mut given = Config::default();
        given.thresholds.recover_below = 0.4;
        let fitted = DecisionProfile::unknown().fit(Some(given));
        assert_eq!(fitted.thresholds.recover_below, 0.4);
        assert_eq!(fitted.observation.max_choice_options, 26);
    }
}
