//! Port of `src/fastbrowse/config.py`: tunable policy. Defaults are starting points to be recalibrated from recorded
//! decision packets.

use serde::{Deserialize, Serialize};

use crate::py::{lax, text};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Thresholds {
    /// Operation/target confidence under this waits for the plan, then goes to LLM recovery if the step acts.
    #[serde(deserialize_with = "lax::float")]
    pub recover_below: f64,
    #[serde(deserialize_with = "lax::float")]
    pub login_required_above: f64,
    /// Jev's agreement, on a page it also judged a wall, that the wall is a bot check and not a sign-in.
    #[serde(deserialize_with = "lax::float")]
    pub bot_check_above: f64,
    #[serde(deserialize_with = "lax::float")]
    pub irreversible_above: f64,
    /// Jev's agreement that the task asks for a commit, above which an authorized run takes it.
    #[serde(deserialize_with = "lax::float")]
    pub requested_above: f64,
    /// A requirement's "not satisfied" answer above this keeps the run working.
    #[serde(deserialize_with = "lax::float")]
    pub claim_problem_above: f64,
    /// Jev's agreement that the task or notes hold a field's value, under which a NEEDS_INPUT stop stands.
    #[serde(deserialize_with = "lax::float")]
    pub value_stated_above: f64,
    /// Jev's doubt that the read facts already answer the task, from which the LLM composer writes it instead.
    #[serde(deserialize_with = "lax::float")]
    pub rewrite_from: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            recover_below: 0.55,
            login_required_above: 0.70,
            bot_check_above: 0.70,
            irreversible_above: 0.50,
            requested_above: 0.50,
            claim_problem_above: 0.70,
            value_stated_above: 0.50,
            rewrite_from: 0.30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ObservationLimits {
    /// Thirty controls keep the second choice small when selection goes group -> element.
    #[serde(deserialize_with = "lax::int_gt1")]
    pub group_size: i64,
}

impl Default for ObservationLimits {
    fn default() -> Self {
        ObservationLimits { group_size: 30 }
    }
}

/// Characters of a decision request's dense JSON counted as one token. Typesafe publishes no tokenizer, so a request's
/// tokens are estimated from its length, and that estimate is the one uncertain number in fitting a request: dense JSON
/// measured 1.84 characters a token, and pages vary, so a request is fitted at 1.5.
pub const JSON_CHARS_PER_TOKEN: f64 = 1.5;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TokenBudget {
    /// Tokens a decision request may spend on its state and largest question: Jev's documented 32k, brought within
    /// another model's limit by `DecisionProfile::fit`.
    #[serde(deserialize_with = "lax::int_gt0")]
    pub state_plus_largest_question: i64,
    /// Tokens a decision request may spend on its state and every question: Jev's documented 64k.
    #[serde(deserialize_with = "lax::int_gt0")]
    pub state_plus_all_questions: i64,
    /// Target for one request among many independent questions. The gateway sheds large requests with 503s (a 26k
    /// token request failed five of eight times, 4k never), so a batch is split small and sent in parallel.
    #[serde(deserialize_with = "lax::int_gt0")]
    pub batch_tokens: i64,
    /// Local estimate for prose prompts; a decision request's dense JSON is counted at `JSON_CHARS_PER_TOKEN`.
    #[serde(deserialize_with = "lax::float_gt0")]
    pub chars_per_token: f64,
    /// Characters a decision request may spend on its state plus largest question and on its state plus every
    /// question, set by `DecisionProfile::fit` from the budget above and the route's limits. Jev with long context
    /// takes a state far past what the budget allows an LLM prompt.
    #[serde(skip)]
    pub decision_chars: Option<(i64, i64)>,
    /// The context window, in tokens, of the LLM that reads and composes, as its provider lists it. A reader or
    /// composer prompt is fitted to it at `chars_per_token`; with none known, the page and notes go whole.
    #[serde(skip)]
    pub llm_context_tokens: Option<i64>,
}

impl Default for TokenBudget {
    fn default() -> Self {
        TokenBudget {
            state_plus_largest_question: 32_000,
            state_plus_all_questions: 64_000,
            batch_tokens: 8000,
            chars_per_token: 3.0,
            decision_chars: None,
            llm_context_tokens: None,
        }
    }
}

impl TokenBudget {
    /// Characters of decision state that fit beside `questions` within Jev's limits, measured as Python measures them,
    /// in code points.
    pub fn input_chars<S: AsRef<str>>(&self, questions: &[S]) -> i64 {
        let sizes: Vec<f64> = questions.iter().map(|q| text::len(q.as_ref()) as f64).collect();
        let (largest, total) = self.decision_chars.map_or_else(
            || {
                (
                    self.state_plus_largest_question as f64 * JSON_CHARS_PER_TOKEN,
                    self.state_plus_all_questions as f64 * JSON_CHARS_PER_TOKEN,
                )
            },
            |(most, all)| (most as f64, all as f64),
        );
        let biggest = sizes.iter().copied().fold(0.0_f64, f64::max);
        let room = (largest - biggest).min(total - crate::py::sum(sizes.iter().copied()));
        // `int()` truncates toward zero before the floor at zero.
        (room.trunc() as i64).max(0)
    }

    /// Characters an LLM prompt may hold: its model's context window where the provider lists one, and no bound
    /// where it does not.
    pub fn llm_chars(&self) -> i64 {
        self.llm_context_tokens.map_or(i64::MAX, |tokens| (tokens as f64 * self.chars_per_token) as i64)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StallRules {
    #[serde(deserialize_with = "lax::int_gt0")]
    pub unchanged_actions: i64,
    #[serde(deserialize_with = "lax::int_ge0")]
    pub max_recoveries: i64,
}

impl Default for StallRules {
    fn default() -> Self {
        StallRules { unchanged_actions: 3, max_recoveries: 2 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub thresholds: Thresholds,
    pub observation: ObservationLimits,
    pub tokens: TokenBudget,
    pub stall: StallRules,
    /// Refuse cookie consent on the platforms DuckDuckGo's autoconsent knows, before the banner paints.
    #[serde(deserialize_with = "lax::boolean")]
    pub refuse_cookie_banners: bool,
    /// Send a PNG of the page with every step event. Off by default: it costs a screenshot round trip per step,
    /// which a caller with nowhere to put the image would pay for nothing.
    #[serde(deserialize_with = "lax::boolean")]
    pub step_frames: bool,
    /// Uploads move as bytes over the DevTools socket; larger files return NEEDS_INPUT with a reason.
    #[serde(deserialize_with = "lax::int_gt0")]
    pub max_upload_bytes: i64,
    /// Largest download kept as an artifact. A body over it is refused and reported, never silently dropped.
    /// Downloads arrive whole over the DevTools socket, so this bounds a run's memory as much as a file's size.
    #[serde(deserialize_with = "lax::int_gt0")]
    pub max_download_bytes: i64,
    /// Next pages of a list code opens and reads by itself in one run; further pages are left to Jev's choice.
    /// Walking a list is the one thing the loop does without asking a model each time, so a catalogue of fifty pages
    /// would otherwise spend the caller's paging allowance before anything noticed.
    #[serde(deserialize_with = "lax::int_ge0")]
    pub max_pages: i64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            thresholds: Thresholds::default(),
            observation: ObservationLimits::default(),
            tokens: TokenBudget::default(),
            stall: StallRules::default(),
            refuse_cookie_banners: true,
            step_frames: false,
            max_upload_bytes: 25 * 1024 * 1024,
            max_download_bytes: 512 * 1024 * 1024,
            max_pages: 12,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decision_request_is_counted_as_dense_json() {
        let tokens = TokenBudget::default();
        assert_eq!(tokens.input_chars::<&str>(&[]), 48_000);
        assert_eq!(tokens.input_chars(&["abc", "\u{6771}\u{4eac}"]), 47_997);
    }

    #[test]
    fn an_llm_prompt_is_bounded_only_by_its_models_listed_context() {
        let tokens = TokenBudget::default();
        assert_eq!(tokens.llm_chars(), i64::MAX);
        assert_eq!(TokenBudget { llm_context_tokens: Some(1000), ..tokens }.llm_chars(), 3000);
    }

    #[test]
    fn a_partial_config_keeps_every_other_default() {
        let config: Config = serde_json::from_str(r#"{"stall": {"max_recoveries": 4}, "max_pages": "3"}"#).unwrap();
        assert_eq!(config.stall.max_recoveries, 4);
        assert_eq!(config.stall.unchanged_actions, 3);
        assert_eq!(config.max_pages, 3);
        assert!(serde_json::from_str::<Config>(r#"{"observation": {"group_size": 1}}"#).is_err());
    }
}
