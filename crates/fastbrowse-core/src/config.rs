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
    /// The browser's hard pool bound; late controls beyond it are still cut in DOM order.
    #[serde(deserialize_with = "lax::int_gt0")]
    pub max_controls: i64,
    /// Long footers must leave most of the control budget for what is on screen.
    #[serde(deserialize_with = "lax::int_ge0")]
    pub max_offscreen_controls: i64,
    /// Jev's step sees at most this many controls; on a denser page a relevance filter picks which. Jev 1.13 loses
    /// accuracy on large state full of irrelevant detail, and a DOM-order cut dropped the control a task needed.
    #[serde(deserialize_with = "lax::int_gt0")]
    pub max_offered_controls: i64,
    /// Keep navigation's page excerpt small; the reader captures the whole page when it needs more.
    #[serde(deserialize_with = "lax::int_gt0")]
    pub viewport_text_chars: i64,
    /// Navigation, field writing and recovery share a short memory; verdicts use the token budget instead.
    #[serde(deserialize_with = "lax::int_gt0")]
    pub working_notes_chars: i64,
    /// Keep effects for six recent actions; older effects describe page states that have already changed.
    #[serde(deserialize_with = "lax::int_ge0")]
    pub history_entries: i64,
    /// Actions before the recent ones, shown without their effects. With only the last six, a form filled in eight
    /// steps lost its first field from view, and Jev typed the origin again instead of searching.
    #[serde(deserialize_with = "lax::int_ge0")]
    pub earlier_history_entries: i64,
    /// Thirty controls keep the second choice small when selection goes group -> element.
    #[serde(deserialize_with = "lax::int_gt1")]
    pub group_size: i64,
    /// Leave headroom under Jev's 255-option ceiling for policy choices.
    #[serde(deserialize_with = "choice_options")]
    pub max_choice_options: i64,
}

fn choice_options<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let value = lax::int_gt1(d)?;
    if value > 255 {
        return Err(serde::de::Error::custom("Input should be less than or equal to 255"));
    }
    Ok(value)
}

impl Default for ObservationLimits {
    fn default() -> Self {
        ObservationLimits {
            max_controls: 320,
            max_offscreen_controls: 120,
            max_offered_controls: 160,
            viewport_text_chars: 6000,
            working_notes_chars: 6000,
            history_entries: 6,
            earlier_history_entries: 14,
            group_size: 30,
            max_choice_options: 240,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TokenBudget {
    /// Conservative target under Jev's 32k limit, since tokens are estimated locally.
    #[serde(deserialize_with = "lax::int_gt0")]
    pub state_plus_largest_question: i64,
    /// Conservative target under Jev's 64k limit.
    #[serde(deserialize_with = "lax::int_gt0")]
    pub state_plus_all_questions: i64,
    /// Target for one request among many independent questions. The gateway sheds large requests with 503s (a 26k
    /// token request failed five of eight times, 4k never), so a batch is split small and sent in parallel.
    #[serde(deserialize_with = "lax::int_gt0")]
    pub batch_tokens: i64,
    /// Local estimate for prompt budgeting; Jev also has a separate dense JSON safety bound.
    #[serde(deserialize_with = "lax::float_gt0")]
    pub chars_per_token: f64,
    /// A page of records needs room for each value and its verbatim quote in the reader's JSON.
    #[serde(deserialize_with = "lax::int_gt0")]
    pub read_output_tokens: i64,
    /// A long answer repeats citation ids for every claim, so it needs more room than a planning response.
    #[serde(deserialize_with = "lax::int_gt0")]
    pub compose_output_tokens: i64,
}

impl Default for TokenBudget {
    fn default() -> Self {
        TokenBudget {
            state_plus_largest_question: 24_000,
            state_plus_all_questions: 48_000,
            batch_tokens: 8000,
            chars_per_token: 3.0,
            read_output_tokens: 8000,
            compose_output_tokens: 8000,
        }
    }
}

impl TokenBudget {
    /// Characters of state that fit beside `questions`, measured as Python measures them, in code points.
    pub fn input_chars<S: AsRef<str>>(&self, questions: &[S], jev: bool) -> i64 {
        let sizes: Vec<f64> = questions.iter().map(|q| text::len(q.as_ref()) as f64).collect();
        let mut largest = self.state_plus_largest_question as f64 * self.chars_per_token;
        let mut total = self.state_plus_all_questions as f64 * self.chars_per_token;
        if jev {
            // Dense JSON measured 1.84 characters per token, so the prose estimate admitted oversized requests.
            largest = largest.min(48_000.0);
            total = total.min(96_000.0);
        }
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
    fn the_jev_budget_is_capped_for_dense_json() {
        let tokens = TokenBudget::default();
        assert_eq!(tokens.input_chars::<&str>(&[], false), 72_000);
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
}
