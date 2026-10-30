//! Jev's per-step decision: one batched request that picks the operation and its target.
//!
//! Question wording is adapted from browser-use/jev-ultrafast (MIT) `questions.py` and `model.py`, where it was
//! live-bench proven. The batch asks for an operation, its possible targets, and an optional login check. Choices
//! beyond Jev's option limit use a group choice followed by a separate element request. A relevance filter shortlists
//! dense pages before the action choice so late controls can still be offered.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex, MutexGuard};

use fastbrowse_core::config::Config;
use fastbrowse_core::decision::{
    Answer, ChoiceAnswer, ChoiceQuestion, DecisionClient, DecisionError, Evaluation, Images, NoulAnswer, NoulQuestion,
    Question,
};
use fastbrowse_core::models::{CostComponent, CostLine, Operation, StepOutcome, TARGETED, UNTRUSTED};
use fastbrowse_core::navigation::navigation_urls;
use fastbrowse_core::page::{Control, Observation, cut_text, cut_with, pager_link};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::json::{Style, dumped_len, dumps, write};
use fastbrowse_core::py::{lax, sha256_hex, text};
use fastbrowse_core::telemetry::{Ledger, Trace};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::batches::evaluate_batches;
use crate::error::{AgentError, Result};

const NEXT_ACTION_RULES: &str = "Advance the user's task from the current page using one operation.\nWhen a subgoal is supplied, take its next action first; it describes the current obstacle.\nUse current field values and the recent history.\nPrioritize unread_requirements when choosing the next target to research.\nPreserve authentication unless the task requires a session change.\nFill a search query before submitting. Clicking only focuses; fill can open an editor and type.\nDo not repeat satisfied steps. Fill required fields before submitting. An action whose\neffect is \"nothing visible changed\" did nothing: take another way, not the same action.\nWhen a control offers choices or a confirmation, use its observed behaviour to determine whether entering\na value commits it or whether selecting or confirming is needed to reach the requested state.\nIf changing a form's mode changes its fields or clears their values, set the requested mode before filling\nthe affected fields.\nIf a form asks for extra values the task does not need, change its mode before inventing those values.\nSet the filters the task asks for that this form already offers before submitting; setting one afterwards\nsubmits twice. A filter the page only shows once there are results is set there, after submitting.\nOnce the task's fields, mode and the filters offered here are set, submit if the form requires submission\nto apply them.\nA matching result alone does not prove a filter was set.\nDo not toggle a checkbox, switch or radio already in the requested state.\nElements marked offscreen can be targeted directly; do not scroll just to reach them.\npage.text is what the viewport shows; page.offscreen_text counts the characters of page text outside it.\nA read takes in the whole page, so do not scroll through a page's text to find what a read would.\nA link's href shows where it leads; use it to tell site navigation from content links.\nA matching link is not an opened result.\nA wizard or multi-step form can keep one URL for every step, so browser history has no entry for the step before\nthis one: when the page itself offers a Back or Previous control, prefer it over the back operation to move\nwithin the form.";
const TARGET_RULES: &str = "Choose the best observed target if the next operation is the one this question names.\nUse the task, unread_requirements, field values, nearby text and recent actions. Another question chooses the operation.\nDo not choose a field that already contains the requested value. Choose only an offered element.\nDo not toggle a checkbox, switch or radio already in the requested state.\nWhile the form's mode is not the one the task needs, the control that sets the mode is the target, not the\nform's submit.\nElements marked offscreen can be targeted directly. A link's href shows where it leads.\nElements carry a `context`: the card, row or section each one belongs to. When the task or subgoal names\none of those, choose the element whose context matches it.";
const DESTINATION_RULES: &str = "Another question picks the element to drag; choose the element it should be dropped onto. Prefer the drop\ntarget, drop zone, slot, list, folder, bucket or column the task names, not the element being dragged.";
const RELEVANCE_RULES: &str = "Could the task's next few actions, or reading what it needs, act on or rely on the element in each question?\nJudge by its label, role, context and href. Site chrome, footers, ads, social links and unrelated navigation\nare not relevant.";

fn untrusted(rules: &str) -> String {
    format!("{UNTRUSTED}\n{rules}")
}

pub static NEXT_ACTION: LazyLock<String> = LazyLock::new(|| untrusted(NEXT_ACTION_RULES));
pub static TARGET: LazyLock<String> = LazyLock::new(|| untrusted(TARGET_RULES));
pub static DESTINATION: LazyLock<String> = LazyLock::new(|| untrusted(DESTINATION_RULES));
pub static RELEVANCE: LazyLock<String> = LazyLock::new(|| untrusted(RELEVANCE_RULES));
pub const GROUP: &str = "Too many elements to list at once. Choose the group that contains the best target if the next\noperation is the one this question names. A later question picks the element inside the group.";

pub fn operation_label(operation: Operation) -> &'static str {
    match operation {
        Operation::Navigate => "Open another HTTP(S) address supplied by the caller for this task.",
        Operation::Click => "Click an element, button, link, menu option, autocomplete suggestion or calendar day.",
        Operation::Hover => "Hover over an element to reveal content the page shows only under the pointer.",
        Operation::Drag => "Drag one element and drop it onto another, to move, reorder or file it.",
        Operation::Fill => "Enter or replace text in an editable field.",
        Operation::Select => "Select a value in an observed dropdown.",
        Operation::Enter => "Press Enter in a text field to submit or search for what it already contains.",
        Operation::Upload => "Attach one of the task's files to a file input.",
        Operation::Escape => "Press Escape to close a menu, popup or overlay.",
        Operation::Scroll => "Scroll down the page to reveal content below the viewport.",
        Operation::ScrollUp => "Scroll up the page to return to content above the viewport.",
        Operation::Back => "Go back to the previous page.",
        Operation::SwitchTab => "Switch to another open tab.",
        Operation::Dialog => "Respond to the open browser dialog.",
        Operation::Read => {
            "Read this page's content, the whole page including text outside the viewport, to extract information the task needs."
        }
        Operation::Done => "Every requirement is satisfied.",
        Operation::Escalate => "No offered operation can make progress.",
    }
}

str_enum! {
    pub enum Reduction {
        None = "none",
        Relevance = "relevance",
        OnscreenOnly = "onscreen_only",
        /// Only the elements that fit offered; the rest are counted as omitted for a scroll to reach.
        Capped = "capped",
        /// The oldest actions dropped and long texts cut until the request fits Jev's documented
        /// limit; the state counts the dropped actions and each cut text ends saying how much was left out.
        Trimmed = "trimmed",
    }
}

str_enum! {
    pub enum ReadAssessment {
        Absent = "absent",
        Editing = "editing",
        Evidence = "evidence",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryEntry {
    /// None for a move code made before Jev's first step, such as opening a shortcut address.
    #[serde(deserialize_with = "lax::nullable")]
    pub operation: Option<Operation>,
    #[serde(deserialize_with = "lax::nullable")]
    pub target: Option<String>,
    pub outcome: StepOutcome,
    #[serde(deserialize_with = "lax::boolean")]
    pub page_changed: bool,
    #[serde(default)]
    pub note: Option<String>,
    /// Entered value, redacted before storage; secrets are represented only by a marker.
    #[serde(default)]
    pub text: Option<String>,
    /// What the action visibly did: the address, controls shown or removed, and values before and after.
    #[serde(default)]
    pub effect: Option<String>,
    /// The read added facts or satisfied a requirement, rather than just visiting the content.
    #[serde(default, deserialize_with = "lax::boolean")]
    pub read_progress: bool,
    /// True for a click that chose an option or ticked a box, a value set and kept in the record as a typed one is;
    /// unset otherwise, so the actions a model is shown do not each carry it.
    #[serde(default, deserialize_with = "lax::opt_boolean")]
    pub setting: Option<bool>,
}

impl HistoryEntry {
    pub fn new(operation: Option<Operation>, target: Option<&str>, outcome: StepOutcome, page_changed: bool) -> Self {
        HistoryEntry {
            operation,
            target: target.map(str::to_string),
            outcome,
            page_changed,
            note: None,
            text: None,
            effect: None,
            read_progress: false,
            setting: None,
        }
    }
}

/// A requirement as Jev sees it. The notes cite requirements by id, which Jev cannot match to a requirement it sees
/// only as text: told nothing but the notes, it judged a page whose answer it had read still worth reading and scrolled
/// instead of finishing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequirementState {
    pub id: String,
    pub text: String,
    /// The notes hold evidence citing this requirement.
    pub answered_in_notes: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepContext {
    pub task: String,
    #[serde(deserialize_with = "lax::nullable")]
    pub subgoal: Option<String>,
    pub requirements: Vec<RequirementState>,
    pub notes: String,
    pub history: Vec<HistoryEntry>,
    #[serde(deserialize_with = "lax::boolean")]
    pub check_login: bool,
    /// Asked apart from `check_login`: credentials mean a sign-in wall is work to do, but nothing a caller can supply
    /// passes a CAPTCHA, so a page is worth checking for one whether or not a secret is held for it.
    #[serde(deserialize_with = "lax::boolean")]
    pub check_bot: bool,
    #[serde(deserialize_with = "lax::boolean")]
    pub has_attachments: bool,
    /// Names of stored secrets the current origin may receive; a fill can type one without Jev seeing it.
    pub secrets: Vec<String>,
    /// None while planning; an empty list means no information remains to collect.
    #[serde(default)]
    pub unread_requirements: Option<Vec<String>>,
    #[serde(default)]
    pub start_url: Option<String>,
    #[serde(default)]
    pub start_landing_url: Option<String>,
    /// Recent reasons, diagnoses and subgoals, separate from actions actually taken.
    #[serde(default)]
    pub recovery_memory: String,
    /// The caller's background, which Jev reads beside the task and never as a requirement.
    #[serde(default)]
    pub background: Option<String>,
    /// Why Jev is asked again on this page: a stall or an unsure pick, and the action withheld for it.
    #[serde(default)]
    pub problem: Option<String>,
    /// Page operations not offered this time, because the problem is that they changed nothing.
    #[serde(default)]
    pub withheld: Vec<Operation>,
    /// Fields the task gives no value for: filling one ends the run needing input.
    #[serde(default)]
    pub no_value_fields: Vec<String>,
    /// What code knows about this page that bears on reading it, such as that it changed after it was read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub page_facts: Vec<String>,
    /// This page was read and nothing has changed it since, so the notes hold what a read takes from it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub page_read: bool,
    /// The page's own address when a site failure that can clear stands on it, offered to navigate to as a reload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reload: Option<String>,
    /// The viewport as a PNG, given to a route that takes images.
    #[serde(skip)]
    pub screenshot: Option<Vec<u8>>,
    /// Earlier actions left out of `history` to fit Jev's input limit; the state tells Jev how many.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_actions: usize,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

impl StepContext {
    /// A context with every optional field at its default.
    pub fn new(task: &str) -> StepContext {
        StepContext {
            task: task.into(),
            subgoal: None,
            requirements: Vec::new(),
            notes: String::new(),
            history: Vec::new(),
            check_login: false,
            check_bot: false,
            has_attachments: false,
            secrets: Vec::new(),
            unread_requirements: None,
            start_url: None,
            start_landing_url: None,
            recovery_memory: String::new(),
            background: None,
            omitted_actions: 0,
            problem: None,
            withheld: Vec::new(),
            no_value_fields: Vec::new(),
            page_facts: Vec::new(),
            page_read: false,
            reload: None,
            screenshot: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub operation: Operation,
    #[serde(default)]
    pub url: Option<String>,
    pub target: Option<Control>,
    pub tab_id: Option<String>,
    pub operation_confidence: f64,
    pub target_confidence: Option<f64>,
    pub login_required: Option<f64>,
    pub offered_controls: i64,
    pub reduction: Reduction,
    pub cost: Vec<CostLine>,
    pub input_tokens: i64,
    #[serde(default)]
    pub bot_check: Option<f64>,
    /// Jev's probability that the page reports the site failed to deliver what was asked.
    #[serde(default)]
    pub site_failure: Option<f64>,
    #[serde(default = "absent")]
    pub read_assessment: ReadAssessment,
    /// Recovery's action on this decision's page. Its confidence scores the action Jev chose instead, so it says
    /// nothing about this one.
    #[serde(default)]
    pub directed: bool,
    /// Drop target of a drag: the control the element Jev chose to drag is released onto.
    #[serde(default)]
    pub destination: Option<Control>,
}

fn absent() -> ReadAssessment {
    ReadAssessment::Absent
}

impl Decision {
    /// Weakest link: the action is only as certain as its least certain choice.
    pub fn confidence(&self) -> f64 {
        match self.target_confidence {
            None => self.operation_confidence,
            Some(target) => self.operation_confidence.min(target),
        }
    }
}

/// `_Request`: one step's state and questions, and what each choice's options stand for.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub state: Value,
    /// The images the state refers to, keyed by the reference written in it.
    pub images: Images,
    pub questions: IndexMap<String, Question>,
    pub targets: IndexMap<Operation, Vec<Control>>,
    pub groups: IndexMap<Operation, Vec<Vec<Control>>>,
    /// Controls a drag may be released onto; the destination question is keyed by their ids.
    pub destinations: Vec<Control>,
    pub destination_groups: Vec<Vec<Control>>,
    pub navigation_groups: Vec<Vec<String>>,
}

/// Completed relevance scores for one document and exact task context, including cancelled passes.
///
/// Shared behind a lock so the batches of one pass can each store what they were billed for as they come back.
#[derive(Debug, Default)]
pub struct RelevanceCache {
    inner: Mutex<CacheState>,
}

#[derive(Debug, Default)]
struct CacheState {
    context: Option<(String, String)>,
    answers: HashMap<String, NoulAnswer>,
}

impl RelevanceCache {
    fn lock(&self) -> MutexGuard<'_, CacheState> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Clears the scores when the document or the context changed; the scores kept for this one.
    fn prepare(&self, document: &str, state: &Value) -> HashMap<String, NoulAnswer> {
        let key = (document.to_string(), write(state, Style::DUMPS.sorted()));
        let mut cache = self.lock();
        if cache.context.as_ref() != Some(&key) {
            cache.context = Some(key);
            cache.answers.clear();
        }
        cache.answers.clone()
    }

    fn remember(&self, question: String, answer: NoulAnswer) {
        self.lock().answers.insert(question, answer);
    }
}

/// Ask Jev for the next action, reducing the observation when it will not fit.
pub async fn decide(
    jev: &dyn DecisionClient,
    observation: &Observation,
    context: &StepContext,
    config: &Config,
    ledger: Option<&Ledger>,
    relevance_cache: Option<&RelevanceCache>,
    trace: &Trace,
) -> Result<Decision> {
    let options = jev.profile().max_choice_options;
    let fitting = Fitting { config, options };
    let (mut observation, mut context) = (Cow::Borrowed(observation), Cow::Borrowed(context));
    let mut controls: Vec<Control> = observation.controls.clone();
    let mut reduction = Reduction::None;
    let mut cost: Vec<CostLine> = Vec::new();
    let mut tokens = 0;
    // A target is picked from at most `options` groups of at most `group_size`, so no more can be offered at once.
    let max_offered = fitting.group_size() * options;
    let mut shortlist_tried = controls.len() > max_offered;
    if shortlist_tried {
        let shortlisted =
            shortlist(jev, &observation, &controls, &context, fitting, ledger, relevance_cache, trace).await?;
        if let Some((kept, paid, used)) = shortlisted {
            (controls, cost, tokens) = (kept, paid, used);
            reduction = Reduction::Relevance;
        } else {
            let keep: HashSet<usize> = protected_first(&controls, max_offered);
            controls = controls.into_iter().enumerate().filter(|(i, _)| keep.contains(i)).map(|(_, c)| c).collect();
        }
    }
    let mut trimmed = false;
    loop {
        let request = build_request(&observation, &controls, &context, config, options);
        if fits(&request, config) {
            match evaluate(jev, &request, &controls, reduction, ledger).await {
                Ok(mut decision) => {
                    decision.cost = cost.iter().cloned().chain(decision.cost).collect();
                    decision.input_tokens += tokens;
                    return Ok(decision);
                }
                Err(AgentError::Jev(DecisionError::InputTooLarge(_))) => {}
                Err(AgentError::Jev(DecisionError::RetriesExhausted(error))) => {
                    let Some((smaller, reduced)) = shed(&request, &controls, config, reduction) else {
                        return Err(AgentError::Jev(DecisionError::RetriesExhausted(error)));
                    };
                    trace.event("decide_shed", || {
                        let mut fields = Map::new();
                        fields.insert("offered".into(), controls.len().into());
                        fields.insert("retry_with".into(), smaller.len().into());
                        fields.insert("reason".into(), error.message.clone().into());
                        fields
                    });
                    (controls, reduction) = (smaller, reduced);
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        // Long history and texts are cut when that alone makes room for every control offered; a page whose controls
        // are what overflow keeps its texts and has its controls shortlisted instead.
        if !trimmed {
            trimmed = true;
            if let Some((smaller, shorter)) = trim(&observation, &controls, &context, fitting) {
                (observation, context, reduction) = (Cow::Owned(smaller), Cow::Owned(shorter), Reduction::Trimmed);
                continue;
            }
        }
        if !shortlist_tried {
            shortlist_tried = true;
            let shortlisted =
                shortlist(jev, &observation, &controls, &context, fitting, ledger, relevance_cache, trace).await?;
            if let Some((kept, paid, used)) = shortlisted {
                (controls, cost, tokens) = (kept, paid, used);
                reduction = Reduction::Relevance;
                continue;
            }
        }
        if !matches!(reduction, Reduction::OnscreenOnly | Reduction::Capped) && controls.iter().any(|c| c.offscreen) {
            controls.retain(|c| !c.offscreen);
            reduction = Reduction::OnscreenOnly;
            continue;
        }
        // A page too dense even on screen is still worked rather than ending the run: the controls that fit are
        // offered, the rest counted as omitted, and a scroll brings them into the next step's view.
        if let Some(capped) = cap(&observation, &controls, &context, fitting) {
            (controls, reduction) = (capped, Reduction::Capped);
            continue;
        }
        // Not even an empty element list fits beside the texts, so they are cut to make room for the capped controls.
        let bare = (!fitting.fits(&observation, &[], &context)).then(|| trim(&observation, &[], &context, fitting));
        let Some((smaller, shorter)) = bare.flatten() else {
            return Err(AgentError::ObservationTooLarge(format!(
                "the state on {} exceeds Jev's input limits with no controls",
                observation.url
            )));
        };
        (observation, context) = (Cow::Owned(smaller), Cow::Owned(shorter));
    }
}

/// What decides whether a step request fits: the fitted config and Jev's documented limit on options per choice.
#[derive(Clone, Copy)]
struct Fitting<'a> {
    config: &'a Config,
    options: usize,
}

impl Fitting<'_> {
    fn group_size(&self) -> usize {
        (self.config.observation.group_size.max(0) as usize).min(self.options)
    }

    /// How many characters the request's state runs over Jev's limit; zero or less when it fits.
    fn excess(&self, observation: &Observation, controls: &[Control], context: &StepContext) -> i64 {
        let request = build_request(observation, controls, context, self.config, self.options);
        let written: Vec<String> = request.questions.values().map(Dump::dump_json).collect();
        text::len(&dumps(&request.state)) as i64 - self.config.tokens.input_chars(&written)
    }

    fn fits(&self, observation: &Observation, controls: &[Control], context: &StepContext) -> bool {
        self.excess(observation, controls, context) <= 0
    }
}

fn notes_marker(omitted_chars: usize) -> String {
    format!("\n[Notes cut: {omitted_chars} characters omitted to fit the request]")
}

fn recovery_marker(omitted_chars: usize) -> String {
    format!("\n[Recovery memory cut: {omitted_chars} characters omitted to fit the request]")
}

/// The observation and context cut until the request with `controls` fits, or None if no cut makes it fit.
///
/// When dropping actions alone makes room, the oldest go first and values set last, keeping the most recent that fit.
/// Otherwise the recovery memory, the page text and the notes are cut in turn, each only by what the request still
/// runs over, and actions are dropped only if those cuts are not enough.
fn trim(
    observation: &Observation,
    controls: &[Control],
    context: &StepContext,
    fitting: Fitting<'_>,
) -> Option<(Observation, StepContext)> {
    let (original, page_text) = (context, observation.viewport_text.as_str());
    let (mut observation, mut context) = (observation.clone(), context.clone());
    let history = std::mem::take(&mut context.history);
    // A value typed or a box ticked is still on the page however long ago it was set, so it is the last to go.
    let mut order: Vec<usize> = (0..history.len()).collect();
    order.sort_by_key(|i| history[*i].text.is_some() || history[*i].setting == Some(true));
    let without = |context: &StepContext, dropped: usize| {
        let gone: HashSet<usize> = order[..dropped].iter().copied().collect();
        let mut shorter = context.clone();
        shorter.history = (0..history.len()).filter(|i| !gone.contains(i)).map(|i| history[i].clone()).collect();
        shorter.omitted_actions += dropped;
        shorter
    };
    // The fewest actions to drop for the request to fit, or all of them when none is enough.
    let fewest = |observation: &Observation, context: &StepContext| {
        let (mut low, mut high) = (0, history.len());
        while low < high {
            let middle = (low + high) / 2;
            if fitting.fits(observation, controls, &without(context, middle)) {
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        low
    };
    if fitting.fits(&observation, controls, &without(&context, history.len())) {
        let dropped = fewest(&observation, &context);
        context = without(&context, dropped);
    } else {
        // The excess is counted in JSON-escaped characters, so each text is cut as it is written into the state.
        let room = |text: &str, excess: i64| (dumped_len(text) as i64 - excess).max(0) as usize;
        let full = without(&context, 0);
        let excess = fitting.excess(&observation, controls, &full);
        if excess > 0 {
            context.recovery_memory =
                cut_with(&context.recovery_memory, room(&context.recovery_memory, excess), true, recovery_marker);
        }
        let excess = fitting.excess(&observation, controls, &without(&context, 0));
        if excess > 0 {
            observation.viewport_text =
                cut_text(&observation.viewport_text, room(&observation.viewport_text, excess), true);
        }
        let excess = fitting.excess(&observation, controls, &without(&context, 0));
        if excess > 0 {
            context.notes = cut_with(&context.notes, room(&context.notes, excess), true, notes_marker);
        }
        let dropped = fewest(&observation, &context);
        context = without(&context, dropped);
    }
    let cut = context.history.len() < history.len()
        || context.recovery_memory != original.recovery_memory
        || context.notes != original.notes
        || observation.viewport_text != page_text;
    (cut && fitting.fits(&observation, controls, &context)).then_some((observation, context))
}

/// Protected controls, then the first others up to `limit` in all: document order when no relevance score is known.
fn protected_first(controls: &[Control], limit: usize) -> HashSet<usize> {
    let mut keep: HashSet<usize> = (0..controls.len()).filter(|i| protected(&controls[*i])).collect();
    let room = limit.saturating_sub(keep.len());
    let others: Vec<usize> = (0..controls.len()).filter(|i| !keep.contains(i)).take(room).collect();
    keep.extend(others);
    keep
}

/// Fewer controls to offer after the provider answered a large request with 503s until its retries ran out.
///
/// The gateway sheds large Jev requests. A Wikipedia article's step offered 160 controls, twice over (state and target
/// question), in 27k input tokens, and it failed six runs in six as unavailable; replayed, that same request was
/// answered four times in ten while every request under 10k tokens but two in 29 was. So a large request is asked
/// again once without its off-screen controls, as the fitting ladder drops them; past that the outage is reported. A
/// request already as small as a batch is not the reason, so its failure stays an outage.
fn shed(
    request: &Request,
    controls: &[Control],
    config: &Config,
    reduction: Reduction,
) -> Option<(Vec<Control>, Reduction)> {
    let ratio = config.tokens.chars_per_token;
    let state = text::len(&dumps(&request.state)) as f64 / ratio;
    let largest = request.questions.values().map(|q| text::len(&q.dump_json())).max().unwrap_or(0) as f64 / ratio;
    if state + largest <= config.tokens.batch_tokens as f64 {
        return None;
    }
    (!matches!(reduction, Reduction::OnscreenOnly | Reduction::Capped) && controls.iter().any(|c| c.offscreen))
        .then(|| (controls.iter().filter(|c| !c.offscreen).cloned().collect(), Reduction::OnscreenOnly))
}

/// The controls at `indices`, in document order.
fn keep_indices(controls: &[Control], indices: &HashSet<usize>) -> Vec<Control> {
    controls.iter().enumerate().filter(|(i, _)| indices.contains(i)).map(|(_, c)| c.clone()).collect()
}

/// The largest prefix, protected controls first, shorter than all of `controls` whose request fits; None if none does.
fn cap(
    observation: &Observation,
    controls: &[Control],
    context: &StepContext,
    fitting: Fitting<'_>,
) -> Option<Vec<Control>> {
    let mut order: Vec<usize> = (0..controls.len()).collect();
    order.sort_by_key(|i| !protected(&controls[*i]));
    let kept = |prefix: usize| keep_indices(controls, &order[..prefix.min(order.len())].iter().copied().collect());
    let fits_with = |prefix: usize| fitting.fits(observation, &kept(prefix), context);
    if controls.is_empty() || !fits_with(0) {
        return None;
    }
    let (mut low, mut high) = (0, controls.len() - 1);
    while low < high {
        let middle = (low + high).div_ceil(2);
        if fits_with(middle) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    Some(kept(low))
}

/// Only state the task has already set is kept unasked: counting every unchecked option of a long filter list would
/// fill the shortlist and push out the button that applies it.
fn protected(control: &Control) -> bool {
    pager_link(control)
        || control.value.as_deref().is_some_and(|value| !value.is_empty())
        || control.checked == Some(true)
        || control.selected == Some(true)
        || control.expanded == Some(true)
}

/// The shortlisted controls, the relevance pass's bill and its input tokens.
type Shortlist = (Vec<Control>, Vec<CostLine>, i64);

#[allow(clippy::too_many_arguments)]
async fn shortlist(
    jev: &dyn DecisionClient,
    observation: &Observation,
    controls: &[Control],
    context: &StepContext,
    fitting: Fitting<'_>,
    ledger: Option<&Ledger>,
    cache: Option<&RelevanceCache>,
    trace: &Trace,
) -> Result<Option<Shortlist>> {
    let config = fitting.config;
    let protected_set: HashSet<usize> = (0..controls.len()).filter(|i| protected(&controls[*i])).collect();
    let candidates: Vec<usize> = (0..controls.len()).filter(|i| !protected_set.contains(i)).collect();
    let state = json!({
        "rules": *RELEVANCE,
        "task": context.task,
        "subgoal": context.subgoal,
        "requirements": context.requirements,
        "unread_requirements": context.unread_requirements,
        "recent_actions": context.history.iter().map(Dump::dump_exclude_none).collect::<Vec<_>>(),
        "page": {"url": observation.url, "title": observation.title},
    });
    // The rubric sits once in the state; repeating it in every question would pack a third as many per request.
    let questions: IndexMap<String, (Question, String)> = candidates
        .iter()
        .map(|i| {
            let element = Value::Object(relevance_element(&controls[*i]));
            let question: Question =
                NoulQuestion::new(format!("Is this element relevant? {}", dumps(&element)), None, None).into();
            let written = question.dump_json();
            (format!("r{i}"), (question, written))
        })
        .collect();
    let cached = cache.map_or_else(HashMap::new, |cache| cache.prepare(&observation.document_key, &state));
    let mut answers: IndexMap<String, Answer> = questions
        .iter()
        .filter_map(|(key, (_, written))| cached.get(written).map(|a| (key.clone(), Answer::Noul(a.clone()))))
        .collect();
    // A redraw cancels unfinished batches, but scores already paid for remain valid for an identical question and
    // context. Action selection still sees the fresh page; relevance never authorizes or chooses an action.
    let mut pending: IndexMap<String, Question> = IndexMap::new();
    let mut representatives: HashMap<&str, &str> = HashMap::new();
    let mut aliases: IndexMap<String, String> = IndexMap::new();
    // Twin controls can ask the same relevance question; one score suffices, but action targets stay distinct.
    for (key, (question, written)) in &questions {
        if !answers.contains_key(key) {
            let representative = *representatives.entry(written.as_str()).or_insert(key.as_str());
            aliases.insert(key.clone(), representative.to_string());
            if representative == key {
                pending.insert(key.clone(), question.clone());
            }
        }
    }
    let remember = |key: &str, answer: &Answer| {
        if let (Some(cache), Answer::Noul(noul), Some((_, written))) = (cache, answer, questions.get(key)) {
            cache.remember(written.clone(), noul.clone());
        }
    };
    let answered = evaluate_batches(jev, &state, &pending, &config.tokens, ledger, Some(&remember), true).await?;
    if let Some(answered) = &answered {
        answers.extend(answered.answers.iter().map(|(k, a)| (k.clone(), a.clone())));
    }
    for (key, representative) in &aliases {
        if let Some(answer) = answers.get(representative).cloned() {
            answers.insert(key.clone(), answer);
        }
    }
    if answers.is_empty() {
        return Ok(None);
    }
    let mut scores: HashMap<usize, f64> = HashMap::new();
    for (key, answer) in &answers {
        if let Answer::Noul(noul) = answer {
            let index = key.get(1..).and_then(|i| i.parse().ok()).ok_or_else(|| {
                AgentError::Invalid(format!("invalid literal for int() with base 10: {}", text::repr(&key[1..])))
            })?;
            scores.insert(index, noul.probability);
        }
    }
    // A failed answer says nothing about relevance, so keep unscored controls ahead of equally scored ones.
    let mut ranked = candidates.clone();
    ranked.sort_by(|a, b| {
        let (sa, sb) = (scores.get(a).copied().unwrap_or(1.0), scores.get(b).copied().unwrap_or(1.0));
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| scores.contains_key(a).cmp(&scores.contains_key(b)))
            .then_with(|| a.cmp(b))
    });
    let kept = |prefix: usize| {
        let mut indices = protected_set.clone();
        indices.extend(ranked[..prefix].iter().copied());
        keep_indices(controls, &indices)
    };
    let max_offered = fitting.group_size() * fitting.options;
    let (mut low, mut high) = (0, ranked.len().min(max_offered.saturating_sub(protected_set.len())));
    while low < high {
        let middle = (low + high).div_ceil(2);
        if fits(&build_request(observation, &kept(middle), context, config, fitting.options), config) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    trace.event("shortlist", || {
        let mut fields = Map::new();
        fields.insert("pool".into(), controls.len().into());
        fields.insert("offered".into(), (protected_set.len() + low).into());
        fields.insert("scored".into(), scores.len().into());
        fields.insert("requests".into(), answered.as_ref().map_or(0, |a| a.requests).into());
        fields.insert("reused".into(), (questions.len() - aliases.len()).into());
        fields.insert("duplicates".into(), (aliases.len() - pending.len()).into());
        fields
    });
    let (paid, used) = answered.map_or((Vec::new(), 0), |answered| (answered.cost, answered.input_tokens));
    Ok(Some((kept(low), paid, used)))
}

fn index_controls(controls: &[Control]) -> IndexMap<Operation, Vec<Control>> {
    let mut indexed: IndexMap<Operation, Vec<Control>> = IndexMap::new();
    for control in controls {
        for operation in &control.operations {
            indexed.entry(*operation).or_default().push(control.clone());
        }
    }
    indexed
}

fn offered_operations(
    observation: &Observation,
    indexed: &IndexMap<Operation, Vec<Control>>,
    context: &StepContext,
    urls: &[String],
) -> Vec<Operation> {
    if observation.dialog.is_some() {
        return vec![Operation::Dialog, Operation::Escalate];
    }
    Operation::ALL
        .iter()
        .copied()
        .filter(|operation| !context.withheld.contains(operation))
        .filter(|operation| match operation {
            Operation::Navigate => !urls.is_empty(),
            Operation::Click
            | Operation::Hover
            | Operation::Fill
            | Operation::Select
            | Operation::Enter
            | Operation::Drag => indexed.contains_key(operation),
            Operation::Upload => context.has_attachments && indexed.contains_key(operation),
            Operation::SwitchTab => observation.tabs.len() > 1,
            Operation::Back => observation.can_go_back,
            Operation::Escape
            | Operation::Scroll
            | Operation::ScrollUp
            | Operation::Read
            | Operation::Done
            | Operation::Escalate => true,
            Operation::Dialog => false,
        })
        .collect()
}

fn choice(instructions: impl Into<String>, criteria: impl IntoIterator<Item = (String, Value)>) -> ChoiceQuestion {
    ChoiceQuestion::new(instructions, criteria.into_iter().collect())
}

fn chunks<T: Clone>(items: &[T], size: usize) -> Vec<Vec<T>> {
    items.chunks(size.max(1)).map(<[T]>::to_vec).collect()
}

pub fn build_request(
    observation: &Observation,
    controls: &[Control],
    context: &StepContext,
    config: &Config,
    max_options: usize,
) -> Request {
    let indexed = index_controls(controls);
    // The caller's start address is held back so a working form is not reopened and emptied. A page with no controls
    // has no form to lose, and a run asked again because it is stuck may need to start over.
    let mut urls = navigation_urls(
        &context.task,
        &observation.url,
        context.start_url.as_deref(),
        context.start_landing_url.as_deref(),
        observation.controls.is_empty() || context.problem.is_some(),
    );
    if let Some(reload) = context.reload.as_ref().filter(|url| !urls.contains(url)) {
        urls.push(reload.clone());
    }
    let offered = offered_operations(observation, &indexed, context, &urls);
    let mut questions: IndexMap<String, Question> = IndexMap::new();
    questions.insert(
        "operation".into(),
        choice(
            NEXT_ACTION.as_str(),
            offered.iter().map(|op| (op.as_str().to_string(), Value::from(operation_label(*op)))),
        )
        .into(),
    );
    // Whether the page holds evidence the notes lack is moot once it was read unchanged: the notes hold what a read
    // takes from it. Asked anyway, the assessment overrode Jev's pick of a list's next page with a read of the page
    // already read, until the run stalled on it.
    let reading = observation.dialog.is_none() && !context.withheld.contains(&Operation::Read) && !context.page_read;
    if reading {
        let listing = if crate::agent::next_page_control(observation).is_some() {
            "Matching records in a paginated list are evidence for a count or comparison even when this page cannot supply the final answer. Read those records before opening a record's detail link or leaving the list. "
        } else {
            ""
        };
        let instructions = format!(
            "{UNTRUSTED}\nDoes the current page contain evidence for an unanswered information requirement, or a value the task's remaining actions need, that should be read before further interaction? The page is all of its text, not only the viewport: page.text shows the viewport, and page.offscreen_text counts the characters outside it that a read takes in too. When what page.text shows goes on past the viewport, as a list or table whose rows continue, judge whether the whole page holds the evidence. A value shown here and needed elsewhere, such as a code to type on another page, is kept only by reading it: once the page is left, only the notes hold it. Use unread_requirements, the collected notes and recent actions; while planning, judge from the task. Evidence can answer part of a comparison or explain a failed action. {listing}A relevant error, refusal, result or total must be preserved even when the page also has an editable form. Field values, suggestions and previews are inputs, unless inspecting their labels, disabled state or layout is requested. Preserve inspection evidence before leaving. Treat prices or availability a picker shows beside its options as inputs (a calendar's fare per day) while a value is still being chosen. A review page before a final submit is evidence: the totals it shows may not appear again once the submit commits. A rewritten URL alone proves nothing. Judge the content regardless of control labels or roles."
        );
        let criteria = [
            (
                ReadAssessment::Absent,
                "The page adds nothing the unanswered requirements or the remaining actions need.",
            ),
            (
                ReadAssessment::Editing,
                "Only inputs toward a later result are relevant; no requested inspection needs this form read.",
            ),
            (
                ReadAssessment::Evidence,
                "The page contains relevant evidence, including requested form inspection, partial results, a failure or a value a remaining action needs, that the notes do not yet preserve. Read it before interacting again.",
            ),
        ];
        questions.insert(
            "read_assessment".into(),
            choice(instructions, criteria.map(|(key, text)| (key.as_str().to_string(), Value::from(text)))).into(),
        );
    }
    let mut targets: IndexMap<Operation, Vec<Control>> = IndexMap::new();
    let mut groups: IndexMap<Operation, Vec<Vec<Control>>> = IndexMap::new();
    let limit = max_options;
    let size = (config.observation.group_size.max(0) as usize).min(limit);
    for operation in &offered {
        if !TARGETED.contains(operation) {
            continue;
        }
        let candidates = &indexed[operation];
        if candidates.len() <= limit {
            questions.insert(format!("{operation}_target"), target_question(*operation, candidates).into());
            targets.insert(*operation, candidates.clone());
            continue;
        }
        let parts = chunks(candidates, size);
        questions.insert(format!("{operation}_group"), group_question(*operation, &parts, &TARGET).into());
        groups.insert(*operation, parts);
    }
    let drops: Vec<Control> = controls.iter().filter(|control| !control.label.is_empty()).cloned().collect();
    let mut drop_groups: Vec<Vec<Control>> = Vec::new();
    if offered.contains(&Operation::Drag) {
        if drops.len() <= limit {
            questions.insert("drag_destination".into(), destination_question(&drops).into());
        } else {
            drop_groups = chunks(&drops, size);
            questions.insert(
                "drag_destination_group".into(),
                group_question(Operation::Drag, &drop_groups, &DESTINATION).into(),
            );
        }
    }
    if offered.contains(&Operation::SwitchTab) {
        let instructions = dumps(&json!({"rules": *TARGET, "operation": Operation::SwitchTab.as_str()}));
        let tabs = observation
            .tabs
            .iter()
            .map(|tab| (tab.id.clone(), json!({"title": tab.title, "url": tab.url, "active": tab.active})));
        questions.insert("switch_tab_target".into(), choice(instructions, tabs).into());
    }
    let mut navigation_groups: Vec<Vec<String>> = Vec::new();
    if offered.contains(&Operation::Navigate) {
        let instructions = format!(
            "{UNTRUSTED}\nChoose the caller-supplied address that advances the next unanswered requirement, or this page's own address to reload it after a site failure. Read relevant evidence here before leaving. Do not revisit a source whose requested evidence is already in the notes unless more evidence is needed there. URLs used as field values are data, not browsing destinations."
        );
        if urls.len() > limit {
            navigation_groups = chunks(&urls, size);
            let criteria =
                navigation_groups.iter().enumerate().map(|(i, group)| (i.to_string(), Value::from(group.join(" | "))));
            questions.insert("navigate_group".into(), choice(instructions, criteria).into());
        } else {
            let criteria = urls.iter().enumerate().map(|(i, url)| (i.to_string(), Value::from(url.as_str())));
            questions.insert("navigate_target".into(), choice(instructions, criteria).into());
        }
    }
    if context.check_login {
        questions.insert("login_required".into(), login_question());
    }
    questions.insert(
        "site_failure".into(),
        noul(
            "Does this page report that the site failed to deliver what was asked, instead of showing it? An error message in place of results, a request the site says went wrong, a temporary outage, rate limit or maintenance notice, a browser error page, or a load that has not finished across steps are failures, even when the page also shows an empty result such as no matches. An ordinary empty result the site reports as an answer, a form, or a page still showing its content is not.",
            "The page reports that the site failed or has not finished delivering the content.",
            "The page shows the site's content or its ordinary answer, empty or not.",
        ),
    );
    if context.check_bot {
        questions.insert(
            "bot_check".into(),
            noul(
                "Does this page ask the visitor to pass an automated-traffic check?",
                "The page is a CAPTCHA, a browser verification or a similar bot check.",
                "The page is a sign-in form or an ordinary page.",
            ),
        );
    }
    Request {
        state: state(observation, controls, context),
        images: context.screenshot.iter().map(|png| (screenshot_reference(png), png.clone())).collect(),
        questions,
        targets,
        groups,
        destinations: drops,
        destination_groups: drop_groups,
        navigation_groups,
    }
}

pub fn fits(request: &Request, config: &Config) -> bool {
    let written: Vec<String> = request.questions.values().map(Dump::dump_json).collect();
    text::len(&dumps(&request.state)) as i64 <= config.tokens.input_chars(&written)
}

async fn evaluate(
    jev: &dyn DecisionClient,
    request: &Request,
    controls: &[Control],
    reduction: Reduction,
    ledger: Option<&Ledger>,
) -> Result<Decision> {
    let mut cost: Vec<CostLine> = Vec::new();
    let mut tokens = 0;
    let mut ask = async |questions: &IndexMap<String, Question>| -> Result<Evaluation> {
        if let Some(ledger) = ledger {
            ledger.reserve(CostComponent::Jev, 0.0, 1)?;
        }
        let answered = jev.evaluate_images(&request.state, questions, &request.images).await?;
        if let Some(ledger) = ledger {
            ledger.record([answered.cost.clone()])?;
        }
        cost.push(answered.cost.clone());
        tokens += answered.input_tokens;
        Ok(answered)
    };

    let evaluation = ask(&request.questions).await?;
    let operation_answer = choice_answer(&evaluation, "operation")?;
    let mut operation = Operation::parse(&operation_answer.choice).ok_or_else(|| {
        AgentError::Invalid(format!("{} is not a valid Operation", text::repr(&operation_answer.choice)))
    })?;
    let mut operation_confidence = operation_answer.confidence;
    let mut target: Option<Control> = None;
    let mut tab_id: Option<String> = None;
    let mut url: Option<String> = None;
    let mut target_confidence: Option<f64> = None;
    if let Some(candidates) = request.targets.get(&operation) {
        let target_answer = choice_answer(&evaluation, &format!("{operation}_target"))?;
        target = Some(find_control(candidates, &target_answer.choice)?);
        target_confidence = Some(target_answer.confidence);
    } else if let Some(groups) = request.groups.get(&operation) {
        let group_answer = choice_answer(&evaluation, &format!("{operation}_group"))?;
        let group = &groups[group_index(&group_answer.choice, groups.len())?];
        let key = format!("{operation}_target");
        let questions = IndexMap::from([(key.clone(), target_question(operation, group).into())]);
        let inner = ask(&questions).await?;
        let target_answer = choice_answer(&inner, &key)?;
        target = Some(find_control(group, &target_answer.choice)?);
        // The element was only ever chosen from inside the group, so a doubtful group is a doubtful target.
        target_confidence = Some(group_answer.confidence * target_answer.confidence);
    } else if operation == Operation::SwitchTab {
        let target_answer = choice_answer(&evaluation, "switch_tab_target")?;
        tab_id = Some(target_answer.choice.clone());
        target_confidence = Some(target_answer.confidence);
    } else if operation == Operation::Navigate {
        let mut group_confidence = 1.0;
        let (target_answer, question) = if request.navigation_groups.is_empty() {
            let question = match request.questions.get("navigate_target") {
                Some(Question::Choice(question)) => question.clone(),
                _ => return Err(AgentError::Invalid("navigation requires caller-supplied choices".into())),
            };
            (choice_answer(&evaluation, "navigate_target")?, question)
        } else {
            let group_answer = choice_answer(&evaluation, "navigate_group")?;
            let group = &request.navigation_groups[group_index(&group_answer.choice, request.navigation_groups.len())?];
            group_confidence = group_answer.confidence;
            let question = choice(
                format!("{UNTRUSTED}\nChoose the caller-supplied address for the next unanswered requirement."),
                group.iter().enumerate().map(|(i, url)| (i.to_string(), Value::from(url.as_str()))),
            );
            let questions = IndexMap::from([("navigate_target".to_string(), question.clone().into())]);
            let inner = ask(&questions).await?;
            (choice_answer(&inner, "navigate_target")?, question)
        };
        let chosen = question
            .criteria
            .get(&target_answer.choice)
            .ok_or_else(|| AgentError::Invalid(text::repr(&target_answer.choice)))?;
        let Some(chosen) = chosen.as_str() else {
            return Err(AgentError::Invalid("navigation requires an address".into()));
        };
        url = Some(chosen.to_string());
        target_confidence = Some(group_confidence * target_answer.confidence);
    }
    let mut destination: Option<Control> = None;
    if operation == Operation::Drag {
        let destination_confidence = if request.destination_groups.is_empty() {
            let destination_answer = choice_answer(&evaluation, "drag_destination")?;
            destination = Some(find_control(&request.destinations, &destination_answer.choice)?);
            destination_answer.confidence
        } else {
            let group_answer = choice_answer(&evaluation, "drag_destination_group")?;
            let group =
                &request.destination_groups[group_index(&group_answer.choice, request.destination_groups.len())?];
            let questions = IndexMap::from([("drag_destination".to_string(), destination_question(group).into())]);
            let inner = ask(&questions).await?;
            let destination_answer = choice_answer(&inner, "drag_destination")?;
            destination = Some(find_control(group, &destination_answer.choice)?);
            group_answer.confidence * destination_answer.confidence
        };
        // A drag is only as certain as the weaker of its two ends.
        target_confidence = Some(target_confidence.map_or(destination_confidence, |t| t.min(destination_confidence)));
    }
    let (read_assessment, read_confidence) = if request.questions.contains_key("read_assessment") {
        let answer = choice_answer(&evaluation, "read_assessment")?;
        let assessment = ReadAssessment::parse(&answer.choice).ok_or_else(|| {
            AgentError::Invalid(format!("{} is not a valid ReadAssessment", text::repr(&answer.choice)))
        })?;
        (assessment, answer.confidence)
    } else {
        (ReadAssessment::Absent, 0.0)
    };
    // Jev judging the page evidence to read is its decision to READ, before finishing too: the answer is written from
    // the notes, so a DONE on evidence left unread answers without it. Only ESCALATE stands, as it claims nothing. The
    // assessment is not asked of a page already read, so this cannot repeat.
    if read_assessment == ReadAssessment::Evidence && operation != Operation::Escalate {
        (operation, target, url, tab_id, target_confidence, destination) =
            (Operation::Read, None, None, None, None, None);
        operation_confidence = read_confidence;
    }
    Ok(Decision {
        operation,
        url,
        target,
        tab_id,
        operation_confidence,
        target_confidence,
        login_required: noul_probability(&evaluation, "login_required"),
        offered_controls: controls.len() as i64,
        reduction,
        cost,
        input_tokens: tokens,
        bot_check: noul_probability(&evaluation, "bot_check"),
        site_failure: noul_probability(&evaluation, "site_failure"),
        read_assessment,
        directed: false,
        destination,
    })
}

/// `int(choice)` used as an index into `count` groups.
fn group_index(choice: &str, count: usize) -> Result<usize> {
    let index = lax::int_of(&Value::from(choice))
        .map_err(|_| AgentError::Invalid(format!("invalid literal for int() with base 10: {}", text::repr(choice))))?;
    let index = if index < 0 { index + count as i64 } else { index };
    usize::try_from(index)
        .ok()
        .filter(|i| *i < count)
        .ok_or_else(|| AgentError::Invalid("list index out of range".into()))
}

fn state(observation: &Observation, controls: &[Control], context: &StepContext) -> Value {
    let mut state = Map::new();
    state.insert("task".into(), context.task.clone().into());
    // "Next month" and "the next Monday" are relative to today, which only the done check was told: shown which month
    // a date picker was on, Jev clicked Next through two years of months looking for next month.
    state.insert("date".into(), observation.today().into());
    state.insert("subgoal".into(), json!(context.subgoal));
    state.insert(
        "page".into(),
        json!({
            "url": observation.url,
            "title": observation.title,
            "text": observation.viewport_text,
            "offscreen_text": observation.offscreen_text,
            "response_status": observation.response_status,
        }),
    );
    state.insert("requirements".into(), json!(context.requirements));
    state.insert("unread_requirements".into(), json!(context.unread_requirements));
    state.insert("notes".into(), context.notes.clone().into());
    state.insert("recent_actions".into(), Value::Array(context.history.iter().map(Dump::dump_exclude_none).collect()));
    if context.omitted_actions > 0 {
        state.insert("omitted_actions".into(), context.omitted_actions.into());
    }
    state.insert("elements".into(), Value::Array(controls.iter().map(|c| Value::Object(element(c))).collect()));
    if let Some(background) = context.background.as_deref().map(text::strip).filter(|b| !b.is_empty()) {
        state.insert("background".into(), background.into());
    }
    if !context.recovery_memory.is_empty() {
        state.insert("recovery_memory".into(), context.recovery_memory.clone().into());
    }
    if let Some(problem) = &context.problem {
        state.insert("problem".into(), problem.clone().into());
    }
    if !context.no_value_fields.is_empty() {
        state.insert("fields_the_task_gives_no_value".into(), json!(context.no_value_fields));
    }
    if !context.page_facts.is_empty() {
        state.insert("page_facts".into(), json!(context.page_facts));
    }
    if !context.secrets.is_empty() {
        state.insert("stored_secrets".into(), json!(context.secrets));
    }
    let omitted = observation.controls.len() - controls.len();
    if omitted > 0 {
        state.insert("omitted_elements".into(), omitted.into());
    }
    if let Some(dialog) = &observation.dialog {
        state.insert("dialog".into(), dialog.dump_exclude_none());
    }
    if observation.tabs.len() > 1 {
        state.insert("tabs".into(), Value::Array(observation.tabs.iter().map(Dump::dump_exclude_none).collect()));
    }
    if let Some(png) = &context.screenshot {
        state.insert("viewport_screenshot".into(), screenshot_reference(png).into());
    }
    Value::Object(state)
}

/// Jev reads an image wherever its reference appears, even inside page text, so the reference is one no page could
/// contain by chance: the image's own digest.
pub(crate) fn screenshot_reference(png: &[u8]) -> String {
    format!("screenshot-{}", sha256_hex(png))
}

fn sorted_operations(control: &Control) -> Value {
    // A `BTreeSet` iterates by value, which is `sorted(control.operations)`.
    Value::Array(control.operations.iter().map(|op| Value::from(op.as_str())).collect())
}

fn relevance_element(control: &Control) -> Map<String, Value> {
    let mut element = Map::new();
    element.insert("label".into(), control.label.clone().into());
    element.insert("role".into(), control.role.clone().into());
    element.insert("operations".into(), sorted_operations(control));
    if let Some(context) = &control.context {
        element.insert("context".into(), context.clone().into());
    }
    if let Some(href) = &control.href {
        element.insert("href".into(), href.clone().into());
    }
    if control.offscreen {
        element.insert("offscreen".into(), true.into());
    }
    element
}

/// A control as the step's state lists it.
pub fn element(control: &Control) -> Map<String, Value> {
    let mut element = Map::new();
    element.insert("id".into(), control.id.clone().into());
    element.insert("label".into(), control.label.clone().into());
    element.insert("role".into(), control.role.clone().into());
    element.insert("operations".into(), sorted_operations(control));
    let optional = [
        ("context", control.context.clone().map(Value::from)),
        ("value", control.value.clone().map(Value::from)),
        ("href", control.href.clone().map(Value::from)),
        ("checked", control.checked.map(Value::from)),
        ("selected", control.selected.map(Value::from)),
        ("expanded", control.expanded.map(Value::from)),
        ("input_type", control.input_type.clone().map(Value::from)),
        ("input_name", control.input_name.clone().map(Value::from)),
        ("autocomplete", control.autocomplete.clone().map(Value::from)),
    ];
    for (key, value) in optional {
        if let Some(value) = value {
            element.insert(key.into(), value);
        }
    }
    if !control.options.is_empty() {
        element.insert("options".into(), json!(control.options));
    }
    if control.offscreen {
        element.insert("offscreen".into(), true.into());
    }
    for (key, marked) in [("required", control.required), ("invalid", control.invalid)] {
        if marked == Some(true) {
            element.insert(key.into(), true.into());
        }
    }
    element
}

fn group_question(operation: Operation, groups: &[Vec<Control>], rules: &str) -> ChoiceQuestion {
    choice(
        dumps(&json!({"rules": [rules, GROUP], "operation": operation.as_str()})),
        groups.iter().enumerate().map(|(i, group)| {
            let labels: Vec<&str> = group.iter().map(|c| c.label.as_str()).collect();
            (i.to_string(), Value::from(labels.join(" | ")))
        }),
    )
}

fn destination_question(candidates: &[Control]) -> ChoiceQuestion {
    choice(
        dumps(&json!({"rules": *DESTINATION, "operation": Operation::Drag.as_str()})),
        candidates.iter().map(|control| (control.id.clone(), Value::Object(relevance_element(control)))),
    )
}

fn target_question(operation: Operation, candidates: &[Control]) -> ChoiceQuestion {
    // Sending only the label and pointing Jev at the shared state for the rest is two thirds smaller on a dense page,
    // and it was tried. It bought no measured latency, because the request was never the slow part, and a criterion
    // the model has to go and look up is a worse criterion: the choice is what this whole design rests on, so it gets
    // the attributes in front of it.
    choice(
        dumps(&json!({"rules": *TARGET, "operation": operation.as_str()})),
        candidates.iter().map(|c| (c.id.clone(), Value::Object(target_element(c, operation)))),
    )
}

/// Ported from browser-use/jev-ultrafast (MIT), snapshot.js: name opening a field separately from typing in it so a
/// picker is a useful click target even when its value is already filled.
fn target_element(control: &Control, operation: Operation) -> Map<String, Value> {
    let mut element = element(control);
    if control.operations.contains(&Operation::Fill) {
        let label = element["label"].as_str().unwrap_or_default().to_string();
        match operation {
            Operation::Click => {
                element.insert("label".into(), format!("Open {label}").into());
            }
            Operation::Enter => {
                element.insert("label".into(), format!("Press Enter in {label}").into());
            }
            _ => {}
        }
    }
    element
}

/// Whether a sign-in wall blocks the task, asked of a fresh page and of the page a run gave up on as signed out.
pub(crate) fn login_question() -> Question {
    noul(
        "Does a sign-in or verification wall block the task in state, with no credentials given in the task to pass it? Unrelated sign-in links and login-to-post boxes do not block reading public or empty lists. A prompt to sign in before replying, commenting or posting gates only writing: when page.text shows what the task or unread_requirements ask to find, the page is readable and the prompt does not block it. If the task asks to inspect access restrictions, observing the wall advances that task without signing in.",
        "A sign-in, verification or access wall blocks the task and the task gives no way through it.",
        "The task can progress without signing in, or the task supplies the credentials to sign in.",
    )
}

fn noul(instructions: &str, yes: &str, no: &str) -> Question {
    NoulQuestion::new(format!("{UNTRUSTED}\n{instructions}"), Some(yes), Some(no)).into()
}

fn choice_answer(evaluation: &Evaluation, key: &str) -> Result<ChoiceAnswer> {
    match evaluation.answers.get(key) {
        Some(Answer::Choice(answer)) => Ok(answer.clone()),
        _ => Err(AgentError::Invalid(format!("Jev returned no choice for {}", text::repr(key)))),
    }
}

fn noul_probability(evaluation: &Evaluation, key: &str) -> Option<f64> {
    evaluation.answers.get(key).and_then(Answer::probability)
}

fn find_control(candidates: &[Control], control_id: &str) -> Result<Control> {
    candidates
        .iter()
        .find(|control| control.id == control_id)
        .cloned()
        .ok_or_else(|| AgentError::Invalid(format!("Jev chose {}, which was not offered", text::repr(control_id))))
}

#[cfg(test)]
mod tests {
    //! tests/test_policy.py's cases that are about more than the bytes sent: the rest replay Python's requests in
    //! tests/golden_policy.rs.
    use std::future::Future;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Waker};

    use fastbrowse_core::BoxFuture;
    use fastbrowse_core::config::ObservationLimits;
    use fastbrowse_core::decision::NoulTag;
    use fastbrowse_core::decision::{ChoiceTag, DecisionProfile, MAX_CHOICE_OPTIONS};
    use fastbrowse_core::models::{CostBasis, Limits};
    use fastbrowse_core::py::datetime::DateTime;

    use super::*;
    use crate::support::testing::block_on;

    fn grouped() -> Config {
        Config { observation: ObservationLimits { group_size: 4 }, ..Config::default() }
    }

    fn button(i: usize) -> Control {
        Control::new(&format!("b{i}"), "button", &format!("Button {i}"), [Operation::Click])
    }

    fn observation(controls: Vec<Control>) -> Observation {
        serde_json::from_value(json!({
            "url": "https://example.test/", "title": "t", "page_key": "k", "captured_at": "2026-09-26T20:00:00Z",
            "controls": controls.iter().map(Dump::dump).collect::<Vec<_>>(),
            "viewport_text": "", "tabs": [],
        }))
        .unwrap()
    }

    /// Picks `pick` for every choice it names, else the first option; scores relevance high for `relevant` labels
    /// and holds back every relevance batch without `r0` until `retry` is set.
    struct Fake {
        pick: HashMap<&'static str, String>,
        relevant: &'static str,
        interrupt: bool,
        retry: AtomicBool,
        options: usize,
        requests: Mutex<Vec<IndexMap<String, Question>>>,
        states: Mutex<Vec<Value>>,
    }

    fn fake(pick: &[(&'static str, &str)], relevant: &'static str) -> Fake {
        Fake {
            pick: pick.iter().map(|(k, v)| (*k, v.to_string())).collect(),
            relevant,
            interrupt: false,
            retry: AtomicBool::new(false),
            options: MAX_CHOICE_OPTIONS,
            requests: Mutex::new(Vec::new()),
            states: Mutex::new(Vec::new()),
        }
    }

    impl DecisionClient for Fake {
        fn profile(&self) -> DecisionProfile {
            DecisionProfile { max_choice_options: self.options, ..DecisionProfile::jev() }
        }

        fn evaluate<'a>(
            &'a self,
            state: &'a Value,
            questions: &'a IndexMap<String, Question>,
        ) -> BoxFuture<'a, std::result::Result<Evaluation, DecisionError>> {
            self.requests.lock().unwrap().push(questions.clone());
            self.states.lock().unwrap().push(state.clone());
            let relevance = !questions.contains_key("operation");
            let hold =
                relevance && self.interrupt && !questions.contains_key("r0") && !self.retry.load(Ordering::SeqCst);
            Box::pin(async move {
                if hold {
                    std::future::pending::<()>().await;
                }
                let answers = questions
                    .iter()
                    .map(|(key, question)| {
                        let answer = match question {
                            Question::Choice(question) => {
                                let first = question.criteria.keys().next().cloned().unwrap_or_default();
                                let choice = self.pick.get(key.as_str()).cloned().unwrap_or(first);
                                Answer::Choice(ChoiceAnswer {
                                    kind: ChoiceTag,
                                    probabilities: question
                                        .criteria
                                        .keys()
                                        .map(|o| (o.clone(), if *o == choice { 1.0 } else { 0.0 }))
                                        .collect(),
                                    choice,
                                    confidence: 0.9,
                                })
                            }
                            Question::Noul(question) => {
                                let label = format!("\"{}\"", self.relevant);
                                let yes = relevance && question.instructions.contains(&label);
                                Answer::Noul(NoulAnswer { kind: NoulTag, probability: if yes { 0.9 } else { 0.1 } })
                            }
                            Question::Score(_) | Question::Box(_) => unreachable!("the policy asks no score or box"),
                        };
                        (key.clone(), answer)
                    })
                    .collect();
                Ok(Evaluation {
                    model: "test".into(),
                    answers,
                    input_tokens: 10,
                    cost: CostLine::new(CostComponent::Jev, CostBasis::Estimated, Some(0.0)),
                    requests: 1,
                })
            })
        }
    }

    fn relevance_requests(jev: &Fake) -> Vec<IndexMap<String, Question>> {
        let scored = |key: &String| key.strip_prefix('r').is_some_and(|i| i.parse::<usize>().is_ok());
        jev.requests.lock().unwrap().iter().filter(|q| q.keys().all(scored)).cloned().collect()
    }

    #[test]
    fn test_picks_target_for_chosen_operation() {
        let jev = fake(&[("operation", "click"), ("click_target", "b3")], "");
        let mut context = StepContext::new("press button 3");
        context.history = vec![HistoryEntry::new(Some(Operation::Scroll), None, StepOutcome::Executed, true)];
        let page = observation((0..5).map(button).collect());
        let decision =
            block_on(decide(&jev, &page, &context, &Config::default(), None, None, &Trace::disabled())).unwrap();
        assert_eq!((decision.operation, decision.target.map(|t| t.id)), (Operation::Click, Some("b3".to_string())));
        assert_eq!(decision.login_required, None);
    }

    #[test]
    fn test_too_many_candidates_go_group_then_element() {
        let config = grouped();
        let jev =
            Fake { options: 10, ..fake(&[("operation", "click"), ("click_group", "2"), ("click_target", "b9")], "") };
        let page = observation((0..12).map(button).collect());
        let context = StepContext::new("press button 3");
        let decision = block_on(decide(&jev, &page, &context, &config, None, None, &Trace::disabled())).unwrap();
        assert_eq!(decision.target.as_ref().unwrap().id, "b9");
        let requests = jev.requests.lock().unwrap();
        let Question::Choice(inner) = &requests[1]["click_target"] else { panic!("a choice") };
        assert_eq!(inner.criteria.keys().collect::<Vec<_>>(), ["b8", "b9", "b10", "b11"]);
        assert_eq!(decision.cost.len(), 2);
        assert!((decision.confidence() - 0.81).abs() < 1e-9);
    }

    #[test]
    fn test_each_request_including_groups_needs_budget() {
        let config = grouped();
        let jev = Fake { options: 10, ..fake(&[("operation", "click"), ("click_group", "0")], "") };
        let ledger = Ledger::new(Limits { max_jev_calls: Some(1), ..Limits::default() });
        let page = observation((0..12).map(button).collect());
        let context = StepContext::new("press button 3");
        let result = block_on(decide(&jev, &page, &context, &config, Some(&ledger), None, &Trace::disabled()));
        assert!(matches!(result, Err(AgentError::Budget(_))));
        assert_eq!((jev.requests.lock().unwrap().len(), ledger.state().jev_calls), (1, 1));
    }

    #[test]
    fn test_redraw_keeps_answered_batches_and_retries_unfinished_batches_in_parallel() {
        // Two options of two controls offer four at most, so five are shortlisted first.
        let mut config = Config { observation: ObservationLimits { group_size: 2 }, ..Config::default() };
        config.tokens.batch_tokens = 1;
        let cache = RelevanceCache::default();
        let mut jev = Fake { options: 2, ..fake(&[("operation", "click")], "Button 0") };
        jev.interrupt = true;
        let page = observation((0..5).map(button).collect());
        let context = StepContext::new("press button 3");
        let trace = Trace::disabled();
        {
            let mut pending = Box::pin(decide(&jev, &page, &context, &config, None, Some(&cache), &trace));
            assert!(pending.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
            assert_eq!(jev.requests.lock().unwrap().len(), 5);
            // Dropped where a redraw cancels the step.
        }
        jev.retry.store(true, Ordering::SeqCst);
        let before = jev.requests.lock().unwrap().len();
        // The new indices and element ids belong to the redraw, so reuse is by question rather than old target id.
        let mut fresh = page.clone();
        fresh.page_key = "redrawn".into();
        fresh.controls =
            page.controls.iter().rev().map(|c| Control { id: format!("fresh-{}", c.id), ..c.clone() }).collect();
        let result = block_on(decide(&jev, &fresh, &context, &config, None, Some(&cache), &trace)).unwrap();
        let retries: Vec<_> = relevance_requests(&jev).into_iter().skip(before).collect();
        assert_eq!(retries.len(), 4);
        assert!(!retries.iter().flat_map(|r| r.values()).any(|q| match q {
            Question::Noul(q) => q.instructions.contains("\"Button 0\""),
            _ => false,
        }));
        assert!(result.target.unwrap().id.starts_with("fresh-"));
    }

    /// A request no larger than Jev's documented limit in tokens, at 1.5 characters a token.
    fn tight() -> Config {
        let mut config = Config::default();
        (config.tokens.state_plus_largest_question, config.tokens.state_plus_all_questions) = (4000, 8000);
        config
    }

    #[test]
    fn test_a_long_history_loses_its_oldest_actions_but_not_a_value_set() {
        let jev = fake(&[("operation", "click")], "");
        let mut context = StepContext::new("press button 3");
        let mut typed = HistoryEntry::new(Some(Operation::Fill), Some("Name"), StepOutcome::Executed, true);
        typed.text = Some("Ada".into());
        context.history = std::iter::once(typed)
            .chain((0..200).map(|i| {
                HistoryEntry::new(Some(Operation::Click), Some(&format!("Next {i}")), StepOutcome::Executed, true)
            }))
            .collect();
        let page = observation((0..3).map(button).collect());
        let decision = block_on(decide(&jev, &page, &context, &tight(), None, None, &Trace::disabled())).unwrap();
        assert_eq!(decision.reduction, Reduction::Trimmed);
        let states = jev.states.lock().unwrap();
        let actions = states[0]["recent_actions"].as_array().unwrap();
        let omitted = states[0]["omitted_actions"].as_u64().unwrap() as usize;
        assert_eq!(actions.len() + omitted, 201);
        assert_eq!(actions[0]["text"], "Ada");
        assert_eq!(actions.last().unwrap()["target"], "Next 199");
        assert_eq!(states[0]["elements"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn test_a_long_page_text_is_cut_to_fit_and_says_so() {
        let jev = fake(&[("operation", "click")], "");
        let mut page = observation((0..3).map(button).collect());
        page.viewport_text = "word ".repeat(4000);
        let mut context = StepContext::new("press button 3");
        context.history = vec![HistoryEntry::new(Some(Operation::Scroll), None, StepOutcome::Executed, true)];
        let decision = block_on(decide(&jev, &page, &context, &tight(), None, None, &Trace::disabled())).unwrap();
        assert_eq!(decision.reduction, Reduction::Trimmed);
        let states = jev.states.lock().unwrap();
        let text = states[0]["page"]["text"].as_str().unwrap();
        assert!(text.starts_with("word word") && text.contains("characters omitted; read the page for the rest"));
        // The text overflowed, not the history, so the one action stays.
        assert_eq!(states[0]["recent_actions"].as_array().unwrap().len(), 1);
        assert!(states[0].get("omitted_actions").is_none());
        assert_eq!(states[0]["elements"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn test_a_long_page_text_is_whole_when_it_fits() {
        let jev = fake(&[("operation", "click")], "");
        let mut page = observation((0..3).map(button).collect());
        page.viewport_text = "word ".repeat(4000);
        let context = StepContext::new("press button 3");
        let decision =
            block_on(decide(&jev, &page, &context, &Config::default(), None, None, &Trace::disabled())).unwrap();
        assert_eq!(decision.reduction, Reduction::None);
        assert_eq!(jev.states.lock().unwrap()[0]["page"]["text"], page.viewport_text);
    }

    /// Shown only the viewport of a long list, Jev scrolled screen by screen and never read the row further down.
    #[test]
    fn test_jev_is_told_how_much_page_text_lies_outside_the_viewport() {
        let jev = fake(&[("operation", "read")], "");
        let mut page = observation((0..3).map(button).collect());
        page.offscreen_text = 38_000;
        let context = StepContext::new("Find the population listed for the last row");
        block_on(decide(&jev, &page, &context, &Config::default(), None, None, &Trace::disabled())).unwrap();
        let states = jev.states.lock().unwrap();
        assert_eq!(states[0]["page"]["offscreen_text"], 38_000);
        assert!(operation_label(Operation::Read).contains("the whole page including text outside the viewport"));
    }

    #[test]
    fn test_done_on_evidence_left_unread_reads_it_first() {
        let jev = fake(&[("operation", "done"), ("read_assessment", "evidence")], "");
        let page = observation((0..3).map(button).collect());
        let mut context = StepContext::new("Find which shop sells the grinder cheaper");
        let unread =
            block_on(decide(&jev, &page, &context, &Config::default(), None, None, &Trace::disabled())).unwrap();
        assert_eq!(unread.operation, Operation::Read);
        context.page_read = true;
        let read = block_on(decide(&jev, &page, &context, &Config::default(), None, None, &Trace::disabled())).unwrap();
        assert_eq!(read.operation, Operation::Done);
    }

    #[test]
    fn test_a_page_read_unchanged_keeps_jevs_pick_of_its_next_page() {
        let jev = fake(&[("operation", "click"), ("click_target", "b2"), ("read_assessment", "evidence")], "");
        let page = observation((0..3).map(button).collect());
        let mut context = StepContext::new("Count the quotes by one author across every page");
        let unread =
            block_on(decide(&jev, &page, &context, &Config::default(), None, None, &Trace::disabled())).unwrap();
        assert_eq!(unread.operation, Operation::Read);
        context.page_read = true;
        let read = block_on(decide(&jev, &page, &context, &Config::default(), None, None, &Trace::disabled())).unwrap();
        assert_eq!((read.operation, read.target.map(|c| c.id)), (Operation::Click, Some("b2".into())));
        assert!(!jev.requests.lock().unwrap()[1].contains_key("read_assessment"));
    }

    #[test]
    fn test_what_the_page_says_about_a_field_is_shown_to_jev() {
        let mut field = button(1);
        field.required = Some(true);
        field.invalid = Some(true);
        assert_eq!(element(&field)["required"], true);
        assert_eq!(element(&field)["invalid"], true);
        assert!(!element(&button(2)).contains_key("required"));
        assert!(!element(&button(2)).contains_key("invalid"));
    }

    #[test]
    fn test_the_choice_is_told_today_for_a_task_relative_to_it() {
        let mut page = observation(vec![]);
        page.captured_at = DateTime::new(2026, 9, 26, 20, 0, 0).utc();
        let context = StepContext::new("Select next month's first Friday");
        let state = build_request(&page, &[], &context, &Config::default(), MAX_CHOICE_OPTIONS).state;
        assert_eq!(state["date"], page.today());
    }

    #[test]
    fn test_jev_reads_the_callers_background_apart_from_the_task() {
        let page = observation(vec![]);
        let mut context = StepContext::new("Find the price");
        let state = build_request(&page, &[], &context, &Config::default(), MAX_CHOICE_OPTIONS).state;
        assert!(state.get("background").is_none());
        context.background = Some(" Asked by email from Ana. ".into());
        let state = build_request(&page, &[], &context, &Config::default(), MAX_CHOICE_OPTIONS).state;
        assert_eq!(
            (&state["task"], &state["background"]),
            (&json!("Find the price"), &json!("Asked by email from Ana."))
        );
    }

    #[test]
    fn test_the_step_state_refers_to_the_viewport_it_is_sent() {
        let page = observation(vec![]);
        let mut context = StepContext::new("Pick the red square");
        let request = build_request(&page, &[], &context, &Config::default(), MAX_CHOICE_OPTIONS);
        assert!(request.state.get("viewport_screenshot").is_none() && request.images.is_empty());
        context.screenshot = Some(b"png".to_vec());
        let request = build_request(&page, &[], &context, &Config::default(), MAX_CHOICE_OPTIONS);
        let reference = format!("screenshot-{}", sha256_hex(b"png"));
        assert_eq!(request.state["viewport_screenshot"], json!(reference));
        assert_eq!(request.images, Images::from([(reference, b"png".to_vec())]));
    }

    #[test]
    fn test_drag_is_not_offered_when_nothing_is_draggable() {
        let page = observation(vec![button(1)]);
        let request =
            build_request(&page, &[button(1)], &StepContext::new("t"), &Config::default(), MAX_CHOICE_OPTIONS);
        assert!(!request.questions.contains_key("drag_target"));
        assert!(!request.questions.contains_key("drag_destination"));
    }

    fn assert_send<T: Send>(_: &T) {}

    #[test]
    fn a_step_can_run_on_any_thread() {
        let jev = fake(&[], "");
        let page = observation(vec![]);
        let context = StepContext::new("t");
        let config = Config::default();
        let cache = RelevanceCache::default();
        let trace = Trace::disabled();
        assert_send(&decide(&jev, &page, &context, &config, None, Some(&cache), &trace));
    }
}
