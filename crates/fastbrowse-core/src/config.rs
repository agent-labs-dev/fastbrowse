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
    /// Minimum confidence for an action code has classified as irreversible (authorization also required).
    #[serde(deserialize_with = "lax::float")]
    pub sensitive_act_from: f64,
    #[serde(deserialize_with = "lax::float")]
    pub login_required_above: f64,
    /// Jev's agreement, on a page it also judged a wall, that the wall is a bot check and not a sign-in.
    #[serde(deserialize_with = "lax::float")]
    pub bot_check_above: f64,
    #[serde(deserialize_with = "lax::float")]
    pub irreversible_above: f64,
    #[serde(deserialize_with = "lax::float")]
    pub done_accept_from: f64,
    /// With every action requirement confirmed, the holistic done answer that accepts without the LLM verifier.
    /// Jev scores a right page 0.49 to 0.91 as done and a near miss 0.04 or less.
    #[serde(deserialize_with = "lax::float")]
    pub done_confirmed_from: f64,
    /// A requirement's "not satisfied" answer under which it counts as confirmed; near misses scored 0.65 or more.
    #[serde(deserialize_with = "lax::float")]
    pub requirement_confirmed_below: f64,
    /// Any "yes = something is wrong" answer check above this rejects the claim.
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
            sensitive_act_from: 0.90,
            login_required_above: 0.70,
            bot_check_above: 0.70,
            irreversible_above: 0.50,
            done_accept_from: 0.85,
            done_confirmed_from: 0.50,
            requirement_confirmed_below: 0.30,
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
    /// The reader's output ceiling; 0 sends none, so a page of records and its verbatim quotes is never cut short.
    #[serde(deserialize_with = "lax::int_ge0")]
    pub read_output_tokens: i64,
    /// The composer's output ceiling; 0 sends none, so a long answer citing every claim is never cut short.
    #[serde(deserialize_with = "lax::int_ge0")]
    pub compose_output_tokens: i64,
}

impl Default for TokenBudget {
    fn default() -> Self {
        TokenBudget {
            state_plus_largest_question: 32_000,
            state_plus_all_questions: 64_000,
            batch_tokens: 8000,
            chars_per_token: 3.0,
            // Zero means no ceiling: fastbrowse does not cap, truncate or crop an output token; the model's own
            // maximum and the answer schema are the only bounds.
            read_output_tokens: 0,
            compose_output_tokens: 0,
        }
    }
}

impl TokenBudget {
    /// Characters of state that fit beside `questions`, measured as Python measures them, in code points.
    pub fn input_chars<S: AsRef<str>>(&self, questions: &[S], jev: bool) -> i64 {
        let sizes: Vec<f64> = questions.iter().map(|q| text::len(q.as_ref()) as f64).collect();
        let ratio = if jev { JSON_CHARS_PER_TOKEN } else { self.chars_per_token };
        let largest = self.state_plus_largest_question as f64 * ratio;
        let total = self.state_plus_all_questions as f64 * ratio;
        let biggest = sizes.iter().copied().fold(0.0_f64, f64::max);
        let room = (largest - biggest).min(total - crate::py::sum(sizes.iter().copied()));
        // `int()` truncates toward zero before the floor at zero.
        (room.trunc() as i64).max(0)
    }

    /// Room for notes after other content, using the same conservative input targets for LLM prompts.
    pub fn remaining_chars<S: AsRef<str>>(&self, context: &str, questions: &[S], jev: bool) -> i64 {
        (self.input_chars(questions, jev) - text::len(context) as i64).max(0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StallRules {
    #[serde(deserialize_with = "lax::int_gt0")]
    pub unchanged_actions: i64,
    #[serde(deserialize_with = "lax::int_ge0")]
    pub max_recoveries: i64,
    /// Reads of one page state, for one set of open requirements, that may add no fact before the run has to act
    /// instead. A page that rewrites its own text on every observation otherwise mints a new read key each time.
    #[serde(deserialize_with = "lax::int_gt0")]
    pub barren_reads: i64,
}

impl Default for StallRules {
    fn default() -> Self {
        StallRules { unchanged_actions: 3, max_recoveries: 2, barren_reads: 2 }
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
        assert_eq!(tokens.input_chars::<&str>(&[], false), 96_000);
        assert_eq!(tokens.input_chars::<&str>(&[], true), 48_000);
        assert_eq!(tokens.input_chars(&["abc", "\u{6771}\u{4eac}"], true), 47_997);
        assert_eq!(tokens.remaining_chars("x".repeat(50_000).as_str(), &["a"], true), 0);
    }

    #[test]
    fn a_partial_config_keeps_every_other_default() {
        let config: Config = serde_json::from_str(r#"{"stall": {"max_recoveries": 4}, "max_pages": "3"}"#).unwrap();
        assert_eq!(config.stall.max_recoveries, 4);
        assert_eq!(config.stall.unchanged_actions, 3);
        assert_eq!(config.max_pages, 3);
        assert!(serde_json::from_str::<Config>(r#"{"observation": {"group_size": 1}}"#).is_err());
    }

    #[test]
    fn a_default_config_reads_back_with_no_output_ceiling() {
        let config: Config = serde_json::from_value(serde_json::to_value(Config::default()).unwrap()).unwrap();
        assert_eq!((config.tokens.read_output_tokens, config.tokens.compose_output_tokens), (0, 0));
        assert!(serde_json::from_str::<Config>(r#"{"tokens": {"read_output_tokens": -1}}"#).is_err());
    }
}
