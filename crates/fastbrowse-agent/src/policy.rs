//! Jev's per-step decision: one batched request that picks the operation and its target.
//!
//! Question wording is adapted from browser-use/jev-ultrafast (MIT) `questions.py` and `model.py`, where it was
//! live-bench proven. The batch asks for an operation, its possible targets, and an optional login check. Choices
//! beyond Jev's option limit use a group choice followed by a separate element request. A relevance filter shortlists
//! dense pages before the action choice so late controls can still be offered.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use fastbrowse_clients::validation::jev_purpose;
use fastbrowse_core::config::Config;
use fastbrowse_core::decision::{
    Answer, ChoiceAnswer, ChoiceQuestion, DecisionClient, DecisionError, EVEN_ODDS, Evaluation, Images,
    JEV_REQUEST_BYTES, NoulQuestion, Question, asset_bytes, jev_image_tokens, referenced,
};
use fastbrowse_core::effects::leavable;
use fastbrowse_core::keys::role_keys;
use fastbrowse_core::models::{CostLine, JevPurpose, Operation, StepOutcome, TARGETED, UNTRUSTED};
use fastbrowse_core::navigation::navigation_urls;
use fastbrowse_core::page::{Control, Observation, cut_text, cut_with, pager_link};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::json::{dumped_len, dumps};
use fastbrowse_core::py::{lax, sha256_hex, text};
use fastbrowse_core::telemetry::{Ledger, Trace};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::batches::evaluate_batches;
use crate::error::{AgentError, Result};
use crate::retrieval::text::{SpanKind, spans};

/// The keys of the questions a step request asks Jev, which it answers under the same keys.
pub(crate) mod keys {
    use fastbrowse_core::models::Operation;

    pub const OPERATION: &str = "operation";
    pub const DRAG_DESTINATION: &str = "drag_destination";
    pub const DRAG_DESTINATION_GROUP: &str = "drag_destination_group";
    pub const SWITCH_TAB_TARGET: &str = "switch_tab_target";
    pub const NAVIGATE_TARGET: &str = "navigate_target";
    pub const NAVIGATE_GROUP: &str = "navigate_group";
    pub const LOGIN_REQUIRED: &str = "login_required";
    pub const BOT_CHECK: &str = "bot_check";
    pub const SHOWS_ANSWER: &str = "shows_answer";
    pub const UNOBTAINABLE: &str = "unobtainable";
    pub const INSIDE_DRAWN: &str = "inside_drawn";
    pub const STEP: &str = "step";

    /// The question that picks `operation`'s target.
    pub fn target(operation: Operation) -> String {
        format!("{operation}_target")
    }

    /// The question that picks the group holding `operation`'s target.
    pub fn group(operation: Operation) -> String {
        format!("{operation}_group")
    }
}

const NEXT_ACTION_RULES: &str = "Advance the user's task from the current page using one operation.\nWhen a subgoal is supplied, take its next action first; it describes the current obstacle.\nUse current field values and the recent history.\nPreserve authentication unless the task requires a session change.\nWhen navigate names a page that holds the item the task names, open it rather than searching the site for that item.\nFill a search query before submitting. Clicking only focuses; fill can open an editor and type.\nDo not repeat satisfied steps. Fill required fields before submitting. An action whose\neffect is \"nothing visible changed\" did nothing: take another way, not the same action.\nWhen a control offers choices or a confirmation, use its observed behaviour to determine whether entering\na value commits it or whether selecting or confirming is needed to reach the requested state.\nIf changing a form's mode changes its fields or clears their values, set the requested mode before filling\nthe affected fields.\nIf a form asks for extra values the task does not need, change its mode before inventing those values.\nSet the filters the task asks for that this form already offers before submitting; setting one afterwards\nsubmits twice. A filter the page only shows once there are results is set there, after submitting.\nOnce the task's fields, mode and the filters offered here are set, submit if the form requires submission\nto apply them.\nA matching result alone does not prove a filter was set.\nDo not toggle a checkbox, switch or radio already in the requested state.\nElements marked offscreen can be targeted directly; do not scroll just to reach them.\npage.text is what the viewport shows; page.offscreen_text is the page's text outside it.\nA read takes in the whole page, so do not scroll through a page's text to find what a read would.\nA link's href shows where it leads; use it to tell site navigation from content links. A link marked next_page\nis the one the page itself names as its next page.\nA matching link is not an opened result.\nA list the page shows in part is not read yet: while it offers more of a list the task covers, a next page or a load\nmore, take it, not one record's own page.\nA wizard or multi-step form can keep one URL for every step, so browser history has no entry for the step before\nthis one: when the page itself offers a Back or Previous control, prefer it over the back operation to move\nwithin the form.\nWhen the task asks for an action, such as a click, and then for what that action shows, the answer is only what\nthe page shows after that action is among the recent actions; what the page already showed before it is not the\nanswer, however close it looks.";
const TARGET_RULES: &str = "Choose the best observed target if the next operation is the one this question names.\nUse the task, field values, nearby text and recent actions. Another question chooses the operation.\nDo not choose a field that already contains the requested value. Choose only an offered element.\nDo not toggle a checkbox, switch or radio already in the requested state.\nWhile the form's mode is not the one the task needs, the control that sets the mode is the target, not the\nform's submit.\nElements marked offscreen can be targeted directly. A link's href shows where it leads, already_read marks one to a page this run\nhas read, and next_page marks the one the page itself names as its next page.\nEach option is the element in state.elements with its id, whose `context` is the card, row or section it belongs\nto. When the task names one, choose by context; a link in one record's context opens it and shows no more of the\nlist.";
const DESTINATION_RULES: &str = "Another question picks the element to drag; choose the element it should be dropped onto. Prefer the drop\ntarget, drop zone, slot, list, folder, bucket or column the task names, not the element being dragged.";
const RELEVANCE_RULES: &str = "Could carrying out any part of the task or current subgoal that is still needed, or reading what it needs, act on or rely on the element in each question?\nUse recent actions to judge which task steps are already complete.\nJudge by its label, role, context and href. An already_read link leads to a page this run has read;\nconsider whether visiting it again would help the task. Site chrome, footers, ads, social links and unrelated navigation\nare not relevant.";

fn untrusted(rules: &str) -> String {
    format!("{UNTRUSTED}\n{rules}")
}

/// What `other_site` on an element means, told only on a page where a link carries it: a run checking one page on a
/// Pages preview followed its store button to another site and ended at that site's security check, with nothing
/// saying the button left the site the task was about (#1137).
pub const OTHER_SITE: &str = "An element marked other_site is a link to a different site: following it leaves the site the browser opened for this task. Follow one only when the task asks for that link or for what that other site holds.";

/// `rules` with `OTHER_SITE` added when one of `controls` carries the mark.
fn told_sites(rules: &str, controls: &[Control]) -> String {
    match controls.iter().any(|control| control.other_site == Some(true)) {
        true => format!("{rules}\n{OTHER_SITE}"),
        false => rules.to_string(),
    }
}

pub static NEXT_ACTION: LazyLock<String> = LazyLock::new(|| untrusted(NEXT_ACTION_RULES));
pub static TARGET: LazyLock<String> = LazyLock::new(|| untrusted(TARGET_RULES));
pub static DESTINATION: LazyLock<String> = LazyLock::new(|| untrusted(DESTINATION_RULES));
pub static RELEVANCE: LazyLock<String> = LazyLock::new(|| untrusted(RELEVANCE_RULES));
pub const GROUP: &str = "Too many elements to list at once. Choose the group that contains the best target if the next\noperation is the one this question names. A later question picks the element inside the group.";

/// The Press operation where a control's ARIA role defines keys, such as a slider stepped by its arrows.
const PRESS_ROLE_KEYS: &str = "Press a key on a control: one its role supports, such as an arrow that steps a slider or moves through tabs, a menu, a list or a grid, or the key or key chord the task gives in an editable field.";

pub fn operation_label(operation: Operation) -> &'static str {
    match operation {
        Operation::Search => {
            "Search the web through the configured search provider, without opening a browser search page."
        }
        Operation::Navigate => {
            "Open another HTTP(S) address: one the caller supplied for this task, or a page a site the task names is known to serve for an item the task names."
        }
        Operation::Click => "Click an element, button, link, menu option, autocomplete suggestion or calendar day.",
        Operation::RightClick => {
            "Right-click an element to open its context menu of commands, such as rename or compress."
        }
        Operation::DoubleClick => "Double-click an element to open it, such as a file, folder or list item.",
        Operation::Hover => "Hover over an element to reveal content the page shows only under the pointer.",
        Operation::Drag => "Drag one element and drop it onto another, to move, reorder or file it.",
        Operation::Fill => "Put the value the task needs into an editable field, typing or replacing its text.",
        Operation::Press => "Press the key or key chord the task gives in an editable field.",
        Operation::Blur => {
            "Move keyboard focus out of the focused editable field without activating or submitting anything, so the page runs what it does when a field is left."
        }
        Operation::Select => "Select a value in an observed dropdown.",
        Operation::Enter => "Press Enter in a text field to submit or search for what it already contains.",
        Operation::Upload => "Attach one of the task's files to a file input.",
        Operation::Escape => "Press Escape to close a menu, popup or overlay.",
        Operation::Scroll => "Scroll down the page to reveal content below the viewport.",
        Operation::ScrollUp => "Scroll up the page to return to content above the viewport.",
        Operation::Back => "Go back to the previous page.",
        Operation::SwitchTab => "Switch to another open tab.",
        Operation::Dialog => "Respond to the open browser dialog.",
        Operation::Wait => {
            "Wait without acting while the page changes by itself, as a clock or a live status line does."
        }
        Operation::Read => {
            "Read this page's content, the whole page including text outside the viewport, to extract information the task needs. What an action the task asks for will show is not on the page until that action is among the recent actions."
        }
        // The action condition stops done before the task's own click on Leaflet's quick-start page, whose standalone
        // popup is open on load: done 0.30 to 0.15 and click 0.32 to 0.62 on the captured first step, with the
        // NEXT_ACTION_RULES sentence on an action's answer and the matching clause on Read (#1202).
        // Defined only for a written answer, done lost to a link on the very page a task asked to open: on PyPI's
        // httpx page Jev split 0.51 done against 0.42 for the Homepage link and sometimes left for GitHub (#964).
        Operation::Done => {
            "The task is done: what it asks is carried out. A task that asks for an action first, such as a click, is not carried out until that action is among the recent actions, whatever the page already shows. A task that only asks to open or reach a page is carried out once the current page is that page. When it asks for a written answer, notes.selected_answer gives every requested fact and value, with no unrelated text or unresolved requirement; a value present only in retained_notes is not in that answer. When composition_pending or structured_output_pending is true, the retained sources cover every requested item and the answer or structured result will be generated and checked before delivery."
        }
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryEntry {
    /// None for a move code made before Jev's first step.
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
    /// The read added facts, rather than just visiting the content.
    #[serde(default, deserialize_with = "lax::boolean")]
    pub read_progress: bool,
    /// True for a click that chose an option or ticked a box, a value set and kept in the record as a typed one is;
    /// unset otherwise, so the actions a model is shown do not each carry it.
    #[serde(default, deserialize_with = "lax::opt_boolean")]
    pub setting: Option<bool>,
    /// The action added to the page without leaving it or taking anything away.
    #[serde(skip)]
    pub added: bool,
    /// Each field a click on an option, checkbox, radio or switch changed, described, with what the page reads it as
    /// holding after the click. Kept out of what models are shown: it is the run's own record of what it changed.
    #[serde(skip)]
    pub set: Vec<FieldChange>,
    /// The task step a click placed inside a drawn surface was made for. Kept apart from `target`, which actions are
    /// withheld and counted by.
    #[serde(skip)]
    pub for_step: Option<String>,
    /// The card of the pictured control the step acted on, as its option showed it. Such a control is named only by
    /// its place, so without it an editor's Bold and Save read as "button (2 of 5)" and "button (5 of 5)": the run was
    /// judged not to have made the text bold, and pressed Bold again.
    #[serde(skip)]
    pub look: Option<Look>,
    /// The commit the step sent, if it sent one: a run that posted two replies and then stopped reported both as typed
    /// but unsent. Kept out of what models are shown: it is the run's own record.
    #[serde(skip)]
    pub commit: Option<Commit>,
    /// The form or field group the step's target belongs to, as the page reports it: a commit sends the fields of its
    /// own form, and no others.
    #[serde(skip)]
    pub form: Option<Form>,
}

/// The scope of a form or field group. A local form id can be reused in other frames or documents.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Form {
    pub(crate) document: String,
    pub(crate) frame: Option<String>,
    pub(crate) id: String,
}

impl Form {
    pub(crate) fn of(observation: &Observation, control: &Control) -> Option<Self> {
        Some(Self {
            document: observation.document_key.clone(),
            frame: control.frame_id.clone(),
            id: control.form_id.clone()?,
        })
    }
}

/// A field changed by a click, with the value and form the page reports for that field.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldChange {
    pub(crate) label: String,
    pub(crate) value: String,
    pub(crate) form: Option<Form>,
    pub(crate) was_target: bool,
}

/// A commit a step sent, and whether the run saw the page take it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Commit {
    /// Sent, and the run ended before the next page showed whether it was taken.
    Sent,
    /// The page took it, as the run counts its commits.
    Taken,
}

/// What the place a step acted on looked like: the card the decision saw and, for a click placed in pixels, the same
/// place after it. A click inside a drawn surface changes nothing but its picture, and the card from before shows the
/// map's Library pin unselected: shown that card alone, the step after the right click put done at 0.2 and clicked the
/// map again; shown the pin turned from red to blue, it put done at 0.9.
#[derive(Clone, Debug, PartialEq)]
pub struct Look {
    pub before: Vec<u8>,
    pub after: Option<Vec<u8>>,
}

impl Look {
    /// The card of the target `decision` chose, among the `looks` its question showed.
    pub fn of(decision: &Decision, looks: &IndexMap<String, Vec<u8>>) -> Option<Look> {
        let target = decision.target.as_ref()?;
        looks.get(&target.id).map(|card| Look { before: card.clone(), after: None })
    }
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
            added: false,
            set: Vec::new(),
            for_step: None,
            look: None,
            commit: None,
            form: None,
        }
    }

    /// The entry as models are shown it. A drawn surface names no part of itself, so a click placed inside one names
    /// the task step it was for: told only that "button (2 of 2)" was clicked, the step after the click that opened
    /// a Leaflet triangle's popup clicked the circle beside it, and the run never answered.
    pub fn shown(&self) -> Value {
        let mut shown = self.dump_exclude_none();
        if let (Some(step), Some(target)) = (&self.for_step, &self.target) {
            shown["target"] = format!("{target}, for the task step {step:?}").into();
        }
        shown
    }

    /// The entry as shown with its control's card, for a question that sends the card among its images.
    pub fn seen(&self) -> Value {
        let mut shown = self.shown();
        match &self.look {
            Some(Look { before, after: None }) => shown["looks_like"] = screenshot_reference(before).into(),
            Some(Look { before, after: Some(after) }) => {
                shown["looked_like_before"] = screenshot_reference(before).into();
                shown["looks_like_after"] = screenshot_reference(after).into();
            }
            None => {}
        }
        shown
    }

    /// What `seen` adds to an entry, told once in a question that is sent the entries' images. Not told, a Leaflet
    /// run that had clicked the blue triangle, its card beside "button (2 of 2)" and "I am a polygon." on the page,
    /// was judged three times not to have clicked it.
    pub const LOOKS: &str = "A recent action's `looks_like` is an image of the element it acted on, as the page drew \
                             it before the action and outlined in magenta: it shows which element the action's target \
                             names. `looked_like_before` and `looks_like_after` are the place a click inside a drawn \
                             surface acted on, before and after the click.";

    /// `instructions` with `LOOKS` added when an entry of `history` has a card.
    pub fn told(instructions: &str, history: &[HistoryEntry]) -> String {
        match history.iter().any(|entry| entry.look.is_some()) {
            true => format!("{instructions}\n{}", Self::LOOKS),
            false => instructions.to_string(),
        }
    }

    /// The images `seen` refers to.
    pub fn looks(history: &[HistoryEntry]) -> impl Iterator<Item = &Vec<u8>> {
        history
            .iter()
            .filter_map(|entry| entry.look.as_ref())
            .flat_map(|look| std::iter::once(&look.before).chain(&look.after))
    }

    /// The step was carried out and acts on the page: a read, scroll, hover or wait only shows what is there, and DONE
    /// ends the run.
    pub fn acted(&self) -> bool {
        self.outcome == StepOutcome::Executed && self.acts()
    }

    /// The step would act on the page, whether or not it was carried out.
    pub fn acts(&self) -> bool {
        !matches!(
            self.operation,
            None | Some(
                Operation::Read
                    | Operation::Scroll
                    | Operation::ScrollUp
                    | Operation::Hover
                    | Operation::Wait
                    | Operation::Done
            )
        )
    }

    /// The step was carried out and could have made or changed something on the site: closing an overlay and moving
    /// between pages or tabs act on the page but only change what is shown.
    pub fn changed_site(&self) -> bool {
        self.acted()
            && !matches!(
                self.operation,
                Some(
                    Operation::Escape
                        | Operation::Back
                        | Operation::SwitchTab
                        | Operation::Navigate
                        | Operation::Search
                )
            )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepContext {
    pub task: String,
    /// The caller's literal task for URL extraction: model context can mention the starting address without asking
    /// to navigate back to it. Older embedders that supply only `task` keep that source.
    #[serde(skip)]
    pub literal_task: Option<String>,
    #[serde(default)]
    pub web_search: bool,
    #[serde(deserialize_with = "lax::nullable")]
    pub subgoal: Option<String>,
    /// The dates the task names, worked out against each page's capture date.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dates: Vec<crate::dates::DateReference>,
    pub notes: String,
    /// The notes hold a fact standing as the task's answer. Told nothing but the notes, Jev judged a page whose answer
    /// it had read still worth reading and scrolled instead of finishing.
    #[serde(default)]
    pub answered_in_notes: bool,
    pub history: Vec<HistoryEntry>,
    #[serde(deserialize_with = "lax::boolean")]
    pub check_login: bool,
    /// A page Jev has not seen, so it is asked whether this shows what the task asks to be told.
    pub check_answer: bool,
    /// Asked apart from `check_login`: credentials mean a sign-in wall is work to do, but nothing a caller can supply
    /// passes a CAPTCHA, so a page is worth checking for one whether or not a secret is held for it.
    #[serde(deserialize_with = "lax::boolean")]
    pub check_bot: bool,
    /// The run has not yet asked, in this document, whether the task needs an action inside a drawn surface.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub check_surface: bool,
    #[serde(deserialize_with = "lax::boolean")]
    pub has_attachments: bool,
    /// Names of stored secrets the current origin may receive; a fill can type one without Jev seeing it.
    pub secrets: Vec<String>,
    #[serde(default)]
    pub start_url: Option<String>,
    #[serde(default)]
    pub start_landing_url: Option<String>,
    /// Pages the site is known to serve for the items the task names, offered as places to navigate (#983).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_pages: Vec<String>,
    /// The caller's background, which Jev reads beside the task and never as part of the task.
    #[serde(default)]
    pub background: Option<String>,
    /// Why Jev is asked again on this page: a stall, and the action withheld for it.
    #[serde(default)]
    pub problem: Option<String>,
    /// Page operations not offered this time, because the problem is that they changed nothing.
    #[serde(default)]
    pub withheld: Vec<Operation>,
    /// Derived actions have no entry in a control's operation list to remove during recovery; a navigation's target is
    /// its address.
    #[serde(skip)]
    pub withheld_targets: Vec<(Operation, String)>,
    /// Fields the task gives no value for: filling one ends the run needing input.
    #[serde(default)]
    pub no_value_fields: Vec<String>,
    /// What code knows about this page that bears on reading it, such as that it changed after it was read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub page_facts: Vec<String>,
    /// The page's own address when a site failure that can clear stands on it, offered to navigate to as a reload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reload: Option<String>,
    /// The viewport as a PNG, given to a route that takes images.
    #[serde(skip)]
    pub screenshot: Option<Vec<u8>>,
    /// The card of each `pictured` control in view, as a PNG cut from the viewport, keyed by control id. A target
    /// option shows it beside its text.
    #[serde(skip)]
    pub looks: IndexMap<String, Vec<u8>>,
    /// The follow-ups each action would need, asked beside the decision. Questions the provider cannot take at speed
    /// in one request stay follow-ups.
    #[serde(skip)]
    pub ahead: Vec<Ahead>,
    /// Ask Jev, beside the next action, which other controls the task needs used on this page, so they run without a
    /// decision of their own while the page stays as it was.
    #[serde(skip)]
    pub plan_ahead: bool,
    #[serde(skip)]
    pub list_read: ListRead,
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
            literal_task: None,
            web_search: false,
            subgoal: None,
            dates: Vec::new(),
            notes: String::new(),
            history: Vec::new(),
            check_login: false,
            check_answer: false,
            check_bot: false,
            check_surface: false,
            has_attachments: false,
            secrets: Vec::new(),
            answered_in_notes: false,
            start_url: None,
            start_landing_url: None,
            known_pages: Vec::new(),
            background: None,
            omitted_actions: 0,
            problem: None,
            withheld: Vec::new(),
            withheld_targets: Vec::new(),
            no_value_fields: Vec::new(),
            page_facts: Vec::new(),
            reload: None,
            screenshot: None,
            looks: IndexMap::new(),
            ahead: Vec::new(),
            plan_ahead: false,
            list_read: ListRead::None,
        }
    }
}

/// A question that follows one action on one control, asked before Jev has picked that action.
#[derive(Debug, Clone, PartialEq)]
pub struct Ahead {
    pub operation: Operation,
    pub control: String,
    /// What the follow-up calls this question.
    pub name: String,
    pub question: Question,
}

impl Ahead {
    fn key(&self) -> String {
        format!("{}:{}:{}", self.operation, self.control, self.name)
    }
}

/// A field Jev said the task needs filled on this page besides the chosen fill, filled after it while the page stays as
/// it was. Only fills are planned: a fill sets one field to a value judged now and stands whatever the chosen action
/// is, while a press changes the page in ways known only after it, and a second control doing the same job undoes the
/// first (#902).
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub target: Control,
    /// Jev's probability that the task needs it.
    pub probability: f64,
    /// Jev's answers to the questions asked ahead about this action, by what the follow-up calls them.
    pub ahead: IndexMap<String, Answer>,
}

const PLAN_PREFIX: &str = "plan:";

/// A field as a sentence names it: its role, label, the context it sits in, and the input name and autocomplete that
/// tell two fields labelled alike apart. A label alone can be the role again: an unlabelled search box reads as
/// 'textbox', and a value writer told only that typed the task's whole answer into it (#1339).
pub fn described(control: &Control) -> String {
    let context = control.context.as_deref().map(|c| format!(" in {}", text::repr(c))).unwrap_or_default();
    let named = [("named", &control.input_name), ("autocomplete", &control.autocomplete)]
        .into_iter()
        .filter_map(|(key, value)| value.as_deref().map(|value| format!(", {key} {}", text::repr(value))))
        .collect::<String>();
    format!("a {} labelled {}{context}{named}", control.role, text::repr(&control.label))
}

fn plan_question(control: &Control) -> Question {
    // A target candidate's state element leaves out its input name and autocomplete, and two fields labelled alike
    // are told apart by them.
    let instructions = format!(
        "{UNTRUSTED}\nElement {} is {}. Besides the one next action, does what the task still needs on this page \
         include that this run fill it, before anything moves on from this page? Yes only when the task itself gives \
         or calls for a value for it now.",
        control.id,
        described(control),
    );
    noul(
        &instructions,
        "The task needs this field filled here, and it does not hold that value yet.",
        "It is not needed, already holds the value, or belongs to another page, view or step.",
    )
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
    /// Whether this page shows what the task asks to be told, so it is read before an action leaves it.
    pub shows_answer: Option<f64>,
    /// Jev's probability that the page reports the site failed to deliver what was asked.
    #[serde(default)]
    pub site_failure: Option<f64>,
    /// Jev's probability that the page says an item the task requires cannot be bought, with no way here to get it.
    #[serde(default)]
    pub unobtainable: Option<f64>,
    /// Jev's probability that the task needs an action on something shown inside a drawn surface in view, which the
    /// run cannot act inside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inside_drawn: Option<f64>,
    /// Drop target of a drag: the control the element Jev chose to drag is released onto.
    #[serde(default)]
    pub destination: Option<Control>,
    /// A recipe step carried out again: no model chose it in this run, so no model's confidence vouches for it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replayed: bool,
    /// A step the run's own code chose rather than Jev, such as opening an unread next page of a whole-list task.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub by_code: bool,
    /// The controls Jev chose among, kept only when `Config.step_choices` asks for them.
    #[serde(skip)]
    pub offered: Vec<Control>,
    /// Jev's answers to the questions asked ahead about this action, by what the follow-up calls them.
    #[serde(skip)]
    pub ahead: IndexMap<String, Answer>,
    /// Other controls to use on this page after this action, in page order, while the page stays as it was.
    #[serde(skip)]
    pub plan: Vec<Planned>,
    #[serde(skip)]
    pub read_scope: ReadScope,
    /// The task step Jev said an action on a pictured control is for, kept for its history entry.
    #[serde(skip)]
    pub step: Option<String>,
}

/// A list read keeps page judgments while leaving target selection to an ordinary decision.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ListRead {
    #[default]
    None,
    Offer,
    Continue,
}

/// These are planner choices, carried out through the existing public Read operation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReadScope {
    #[default]
    Page,
    List,
    Return,
}

impl Decision {
    /// Who chose this step: a replayed recipe, the run's own code, or Jev.
    pub fn decider(&self) -> fastbrowse_core::models::Decider {
        use fastbrowse_core::models::Decider;
        if self.replayed {
            Decider::Recipe
        } else if self.by_code {
            Decider::Code
        } else {
            Decider::Jev
        }
    }

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
    /// The images the state or a target option refers to, keyed by the reference written in it.
    pub images: Images,
    /// The card of each pictured control, which its target option shows, again when a group's pick asks for it.
    pub looks: IndexMap<String, Vec<u8>>,
    /// The task steps the step question offers, by its options' order.
    pub steps: Vec<String>,
    pub questions: IndexMap<String, Question>,
    pub targets: IndexMap<Operation, Vec<Control>>,
    pub groups: IndexMap<Operation, Vec<Vec<Control>>>,
    /// Controls a drag may be released onto; the destination question is keyed by their ids.
    pub destinations: Vec<Control>,
    pub destination_groups: Vec<Vec<Control>>,
    pub navigation_groups: Vec<Vec<String>>,
    /// The rules a target is chosen by, again when a group's pick asks for its element.
    pub target_rules: String,
}

/// Ask Jev for the next action, reducing the observation when it will not fit.
pub async fn decide(
    jev: &dyn DecisionClient,
    observation: &Observation,
    context: &StepContext,
    config: &Config,
    ledger: Option<&Ledger>,
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
    let mut shortlist_tried = context.list_read != ListRead::Continue && controls.len() > max_offered;
    if shortlist_tried {
        let shortlisted = shortlist(jev, &observation, &controls, &context, fitting, ledger, trace).await?;
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
        // Pictures are given up before text: a body over Jev's limit is refused whatever its controls, so cutting
        // them first resent the same pictures and was refused again (#1301). The oldest card goes first, one at a
        // time, until the body fits.
        if request_bytes(&request) > JEV_REQUEST_BYTES && shows_pictures(&context) {
            drop_a_picture(context.to_mut());
            continue;
        }
        if step_room(&request, config) >= 0 {
            // Jev counts every picture's tokens, for each place it is used, on top of the text: a step whose text fits
            // but whose pictures do not was sent anyway and refused with `max_tokens_exceeded` (#1303). The oldest
            // card goes first, as for the body limit, until the first request sent fits.
            if shows_pictures(&context) && !pictures_fit(&request, config) {
                drop_a_picture(context.to_mut());
                continue;
            }
            match evaluate(jev, &request, &controls, reduction, ledger).await {
                Ok(mut decision) => {
                    decision.cost = cost.iter().cloned().chain(decision.cost).collect();
                    decision.input_tokens += tokens;
                    if config.step_choices {
                        decision.offered = controls;
                    }
                    return Ok(decision);
                }
                // Jev counts a picture again for each place it is used, and as it decodes it rather than as it was
                // sent, so a body that fits can still be refused. Its pictures are the part a text cut cannot shrink:
                // the cards go, then the screenshot, before any text or control.
                Err(AgentError::Jev(DecisionError::InputTooLarge(_))) if shows_pictures(&context) => {
                    drop_pictures(context.to_mut());
                    continue;
                }
                Err(AgentError::Jev(DecisionError::InputTooLarge(_))) => {}
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
            let shortlisted = shortlist(jev, &observation, &controls, &context, fitting, ledger, trace).await?;
            if let Some((kept, paid, used)) = shortlisted {
                (controls, cost, tokens) = (kept, paid, used);
                reduction = Reduction::Relevance;
                continue;
            }
        }
        if let Some(onscreen) = onscreen_rung(&controls, reduction) {
            (controls, reduction) = (onscreen, Reduction::OnscreenOnly);
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
        -step_room(&request, self.config)
    }

    fn fits(&self, observation: &Observation, controls: &[Control], context: &StepContext) -> bool {
        self.excess(observation, controls, context) <= 0
    }
}

fn fact_marker(omitted_chars: usize) -> String {
    format!("\n[Page fact cut: {omitted_chars} characters omitted to fit Jev's input limit]")
}

fn notes_marker(omitted_chars: usize) -> String {
    format!("\n[Notes cut: {omitted_chars} characters omitted to fit the request]")
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
    let original_offscreen = observation.offscreen_text.as_str();
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
        // A retained source can be larger than this page, so fit its facts before discarding the current text.
        for index in (0..context.page_facts.len()).rev() {
            let excess = fitting.excess(&observation, controls, &without(&context, 0));
            if excess <= 0 {
                break;
            }
            let fact = &context.page_facts[index];
            context.page_facts[index] = cut_with(fact, room(fact, excess), true, fact_marker);
        }
        let excess = fitting.excess(&observation, controls, &without(&context, 0));
        if excess > 0 {
            observation.offscreen_text =
                cut_text(&observation.offscreen_text, room(&observation.offscreen_text, excess), true);
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
    let cut = context.page_facts != original.page_facts
        || context.history.len() < history.len()
        || context.notes != original.notes
        || observation.viewport_text != page_text
        || observation.offscreen_text != original_offscreen;
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

/// The fitting ladder's on-screen rung: `controls` without the off-screen ones, or None when it has no more to drop.
fn onscreen_rung(controls: &[Control], reduction: Reduction) -> Option<Vec<Control>> {
    (!matches!(reduction, Reduction::OnscreenOnly | Reduction::Capped) && controls.iter().any(|c| c.offscreen))
        .then(|| controls.iter().filter(|c| !c.offscreen).cloned().collect())
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
    trace: &Trace,
) -> Result<Option<Shortlist>> {
    let config = fitting.config;
    let protected_set: HashSet<usize> = (0..controls.len()).filter(|i| protected(&controls[*i])).collect();
    let candidates: Vec<usize> = (0..controls.len()).filter(|i| !protected_set.contains(i)).collect();
    let state = json!({
        "rules": told_sites(&RELEVANCE, controls),
        "task": context.task,
        "subgoal": context.subgoal,
        // Scrolling changes visibility, not completed task steps. Other actions can make an unchanged control
        // no longer useful, so their history must invalidate its score before shortlist selection.
        "recent_actions": context.history.iter()
            .filter(|entry| !matches!(entry.operation, Some(Operation::Scroll | Operation::ScrollUp)))
            .map(HistoryEntry::shown).collect::<Vec<_>>(),
        "page": {"url": observation.url, "title": observation.title},
    });
    // The rubric sits once in the state; repeating it in every question would pack a third as many per request.
    let questions: IndexMap<String, (Question, String)> = candidates
        .iter()
        .map(|i| {
            // A scroll made thousands of unchanged links ask again. Visibility belongs in the action choice,
            // but read state can change which links survive this shortlist.
            let mut element = relevance_element(&controls[*i]);
            element.remove("offscreen");
            let element = Value::Object(element);
            let question: Question =
                NoulQuestion::new(format!("Is this element relevant? {}", dumps(&element)), None, None).into();
            let written = question.dump_json();
            (format!("r{i}"), (question, written))
        })
        .collect();
    // A redraw cancels unfinished batches, but the run's `Known` answers keep the scores already paid for, by the exact
    // state and question. Action selection still sees the fresh page; relevance never authorizes or chooses an action.
    let mut pending: IndexMap<String, Question> = IndexMap::new();
    let mut representatives: HashMap<&str, &str> = HashMap::new();
    let mut aliases: IndexMap<String, String> = IndexMap::new();
    // Twin controls can ask the same relevance question; one score suffices, but action targets stay distinct.
    for (key, (question, written)) in &questions {
        let representative = *representatives.entry(written.as_str()).or_insert(key.as_str());
        aliases.insert(key.clone(), representative.to_string());
        if representative == key {
            pending.insert(key.clone(), question.clone());
        }
    }
    let answered = evaluate_batches(JevPurpose::Relevance, jev, &state, &pending, &config.tokens, ledger, true).await?;
    let mut answers: IndexMap<String, Answer> = answered.as_ref().map(|a| a.answers.clone()).unwrap_or_default();
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
        fields.insert("reused".into(), answered.as_ref().map_or(0, |a| a.known).into());
        fields.insert("duplicates".into(), (aliases.len() - pending.len()).into());
        fields
    });
    let (paid, used) = answered.map_or((Vec::new(), 0), |answered| (answered.cost, answered.input_tokens));
    Ok(Some((kept(low), paid, used)))
}

/// Whether `control` takes `operation`. A field that takes a fill also takes a key; other keyboard targets are
/// offered by the observation, which knows whether the control can receive focus.
pub fn takes(control: &Control, operation: Operation) -> bool {
    control.operations.contains(&operation)
        || (operation == Operation::Press && control.operations.contains(&Operation::Fill))
}

/// The controls each operation can target. Leaving a field is offered from what the browser reports, never from a
/// control's own list.
fn index_controls(controls: &[Control], context: &StepContext) -> IndexMap<Operation, Vec<Control>> {
    let mut indexed: IndexMap<Operation, Vec<Control>> = IndexMap::new();
    for control in controls {
        for operation in control.operations.iter().filter(|operation| **operation != Operation::Blur) {
            indexed.entry(*operation).or_default().push(control.clone());
        }
        if leavable(control)
            && !context
                .withheld_targets
                .iter()
                .any(|(operation, id)| *operation == Operation::Blur && id == &control.id)
        {
            indexed.entry(Operation::Blur).or_default().push(control.clone());
        }
        if takes(control, Operation::Press)
            && !context
                .withheld_targets
                .iter()
                .any(|(operation, id)| *operation == Operation::Press && id == &control.id)
        {
            indexed.entry(Operation::Press).or_default().push(control.clone());
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
        return vec![Operation::Dialog];
    }
    Operation::ALL
        .iter()
        .copied()
        .filter(|operation| !context.withheld.contains(operation))
        .filter(|operation| match operation {
            Operation::Search => context.web_search,
            Operation::Navigate => !urls.is_empty(),
            Operation::Click
            | Operation::RightClick
            | Operation::DoubleClick
            | Operation::Hover
            | Operation::Fill
            | Operation::Blur
            | Operation::Select
            | Operation::Enter
            | Operation::Press
            | Operation::Drag => indexed.contains_key(operation),
            Operation::Upload => context.has_attachments && indexed.contains_key(operation),
            Operation::SwitchTab => observation.tabs.len() > 1,
            Operation::Back => observation.can_go_back,
            Operation::Escape
            | Operation::Scroll
            | Operation::ScrollUp
            | Operation::Wait
            | Operation::Read
            | Operation::Done => true,
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
    let indexed = index_controls(controls, context);
    // The caller's start address is held back so a working form is not reopened and emptied. A page with no controls
    // has no form to lose, and a run asked again because it is stuck may need to start over.
    let mut urls = navigation_urls(
        context.literal_task.as_deref().unwrap_or(&context.task),
        &observation.url,
        context.start_url.as_deref(),
        context.start_landing_url.as_deref(),
        observation.controls.is_empty() || context.problem.is_some(),
    );
    for known in &context.known_pages {
        if !urls.contains(known) && !fastbrowse_core::navigation::same_address(known, &observation.url) {
            urls.push(known.clone());
        }
    }
    if let Some(reload) = context.reload.as_ref().filter(|url| !urls.contains(url)) {
        urls.push(reload.clone());
    }
    urls.retain(|url| {
        !context.withheld_targets.iter().any(|(operation, withheld)| {
            *operation == Operation::Navigate && fastbrowse_core::navigation::same_address(url, withheld)
        })
    });
    let offered = if context.list_read == ListRead::Continue {
        Vec::new()
    } else {
        offered_operations(observation, &indexed, context, &urls)
    };
    let mut questions: IndexMap<String, Question> = IndexMap::new();
    let target_rules = told_sites(&TARGET, controls);
    let target_rules = target_rules.as_str();
    questions.insert(
        keys::OPERATION.into(),
        choice(
            told_sites(&HistoryEntry::told(&NEXT_ACTION, &context.history), controls),
            offered.iter().map(|op| {
                // The operation is chosen apart from its target, and a choice of one address is answered without Jev
                // at all, so the operation names where navigate goes: told only "another address", Jev searched the
                // site for the item whose page was on offer, with one known page and with two.
                let known: Vec<&str> =
                    urls.iter().filter(|url| context.known_pages.contains(url)).map(String::as_str).collect();
                // Naming a known page as one held keeps Jev to it more firmly than naming it as the address.
                let label = match (op, urls.as_slice()) {
                    (Operation::Navigate, _) if !known.is_empty() => {
                        format!(
                            "{} Pages known to hold an item the task names: {}",
                            operation_label(*op),
                            known.join(", ")
                        )
                    }
                    (Operation::Scroll, _) if indexed.contains_key(op) => {
                        "Scroll the chosen vertical area down, or horizontal area right, to reveal more content.".into()
                    }
                    (Operation::ScrollUp, _) if indexed.contains_key(op) => {
                        "Scroll the chosen vertical area up, or horizontal area left, to reveal content in that direction."
                            .into()
                    }
                    (Operation::Navigate, [only]) => format!("{} The address: {only}", operation_label(*op)),
                    // Only a page with a control whose role defines keys is told so, so a page of plain fields asks
                    // what it always has.
                    (Operation::Press, _)
                        if indexed
                            .get(op)
                            .is_some_and(|controls| controls.iter().any(|c| !role_keys(&c.role).is_empty())) =>
                    {
                        PRESS_ROLE_KEYS.into()
                    }
                    _ => operation_label(*op).to_string(),
                };
                (op.as_str().to_string(), Value::from(label))
            }),
        )
        .into(),
    );
    if context.list_read != ListRead::None {
        let Question::Choice(operation) = questions.get_mut(keys::OPERATION).expect("operation question") else {
            unreachable!("operation is a choice");
        };
        if context.list_read == ListRead::Continue {
            operation.instructions = format!(
                "{UNTRUSTED}\nThe run is reading a list the task covers, one page at a time. Choose whether to read this page as another page of that same list, or hand it back for an ordinary action decision. Hand it back if it is a different list, an item detail page, a wall, an error, or needs an action before its records can be read. Reading does not declare the task complete."
            );
            operation.criteria.insert("stop_list".into(), "Return to an ordinary decision on this page.".into());
        }
        if !context.withheld.contains(&Operation::Read)
            && (context.list_read == ListRead::Continue || offered.contains(&Operation::Read))
        {
            operation.criteria.insert(
                "read_list".into(),
                "Read this page and continue reading this same list through its observed next-page links, within the pages the task covers. Keep checking each page before reading it.".into(),
            );
        }
    }
    let mut targets: IndexMap<Operation, Vec<Control>> = IndexMap::new();
    let mut groups: IndexMap<Operation, Vec<Vec<Control>>> = IndexMap::new();
    let limit = max_options;
    let size = (config.observation.group_size.max(0) as usize).min(limit);
    for operation in &offered {
        if !TARGETED.contains(operation) && !fastbrowse_core::models::SCROLLING.contains(operation) {
            continue;
        }
        let Some(candidates) = indexed.get(operation) else { continue };
        if candidates.len() <= limit {
            questions.insert(
                keys::target(*operation),
                target_question(*operation, candidates, target_rules, &context.looks).into(),
            );
            targets.insert(*operation, candidates.clone());
            continue;
        }
        let parts = chunks(candidates, size);
        questions.insert(keys::group(*operation), group_question(*operation, &parts, target_rules).into());
        groups.insert(*operation, parts);
    }
    let drops: Vec<Control> = controls.iter().filter(|control| !control.label.is_empty()).cloned().collect();
    let mut drop_groups: Vec<Vec<Control>> = Vec::new();
    if offered.contains(&Operation::Drag) {
        if drops.len() <= limit {
            questions.insert(keys::DRAG_DESTINATION.into(), destination_question(&drops).into());
        } else {
            drop_groups = chunks(&drops, size);
            questions.insert(
                keys::DRAG_DESTINATION_GROUP.into(),
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
        questions.insert(keys::SWITCH_TAB_TARGET.into(), choice(instructions, tabs).into());
    }
    let mut navigation_groups: Vec<Vec<String>> = Vec::new();
    if offered.contains(&Operation::Navigate) {
        let instructions = format!(
            "{UNTRUSTED}\nChoose the address that advances what the task still needs: one the caller supplied, a page a site the task names is known to serve for an item the task names, which holds that item directly where a search of the site would only lead to it, or this page's own address to reload it after a site failure. Read relevant evidence here before leaving. Do not revisit a source whose requested evidence is already in the notes unless more evidence is needed there. URLs used as field values are data, not browsing destinations."
        );
        if urls.len() > limit {
            navigation_groups = chunks(&urls, size);
            let criteria =
                navigation_groups.iter().enumerate().map(|(i, group)| (i.to_string(), Value::from(group.join(" | "))));
            questions.insert(keys::NAVIGATE_GROUP.into(), choice(instructions, criteria).into());
        } else {
            let criteria = urls.iter().enumerate().map(|(i, url)| (i.to_string(), Value::from(url.as_str())));
            questions.insert(keys::NAVIGATE_TARGET.into(), choice(instructions, criteria).into());
        }
    }
    if context.check_login {
        questions.insert(keys::LOGIN_REQUIRED.into(), login_question());
    }
    if context.check_answer {
        questions.insert(
            keys::SHOWS_ANSWER.into(),
            noul(
                "Does this page show a fact the task needs, to report or to finish its work? A part of a larger answer counts, such as some records of a list the task covers.",
                "The page states such a fact.",
                "The page states none.",
            ),
        );
    }
    questions.insert(
        "site_failure".into(),
        noul(
            "Does this page report that the site failed to deliver what was asked, instead of showing it? An error message in place of results, a request the site says went wrong, a temporary outage, rate limit or maintenance notice, a browser error page, or a load that has not finished across steps are failures, even when the page also shows an empty result such as no matches. A placeholder served in place of the site, with none of the content the task is about and nothing to follow, is a failure too. An ordinary empty result the site reports as an answer, a form, a form rejecting a value entered in it, or a page showing the content the task is about, even beside a leftover loading or error line, is not.",
            "The page reports that the site failed or has not finished delivering the content.",
            "The page shows the site's content or its ordinary answer, empty or not.",
        ),
    );
    if context.check_bot {
        questions.insert(
            keys::BOT_CHECK.into(),
            noul(
                "Does this page ask the visitor to pass an automated-traffic check, or refuse this visitor access outright, such as Access Denied or Forbidden, without offering a way to sign in?",
                "The page is a CAPTCHA, a browser verification, a similar bot check, or a refusal of access with no sign-in.",
                "The page is a sign-in form or an ordinary page.",
            ),
        );
        // Asked once per page state, as the bot check is: a run that read "Sold out" on its first step otherwise kept
        // looking for 74 more (#960).
        questions.insert(
            keys::UNOBTAINABLE.into(),
            noul(
                "Does this page say that an item the task requires buying, ordering or adding to a basket cannot be bought, such as an item marked sold out or no longer sold, with no way offered on this site to get it?",
                "The page says an item the task requires cannot be bought here, and offers no way to get it.",
                "The page says no such thing, or the task does not require buying anything.",
            ),
        );
    }
    // Only the task says whether what it needs is inside a drawn surface, where the run cannot act, or among the
    // page's elements (#1084). Asked once per document that shows one, whenever one comes into view: a streamed view
    // shows a spinner before its picture, in the same document. The line drawn is whether the action has to land on
    // one place in the surface: asked instead whether the surface was "pressed as a whole", with only a streamed
    // screen's field and button as examples, an OpenLayers marker was put at 0.45 to 0.58, and clicked at the canvas
    // centre half the time; asked this way it is put at 0.97, and a canvas switch stays under 0.25.
    if context.check_surface && !drawn_surfaces(controls).is_empty() {
        questions.insert(
            keys::INSIDE_DRAWN.into(),
            noul(
                "An element marked `drawn` is a drawn surface: it shows its content as pixels, with no elements inside it. Does the task need an action that has to land on one particular thing drawn inside such a surface, such as a marker, pin or shape on a map, a bar of a chart, or a field or button in a streamed screen? An action that does the same wherever it lands on the surface is not such an action, and neither is reading what it shows.",
                "The task needs an action on one particular thing drawn inside a surface.",
                "The task needs only this page's elements, an action that does the same anywhere on a surface, or what a surface shows read.",
            ),
        );
    }
    if context.plan_ahead {
        for control in controls {
            if offered.contains(&Operation::Fill) && control.operations.contains(&Operation::Fill) {
                questions.insert(format!("{PLAN_PREFIX}{}", control.id), plan_question(control));
            }
        }
    }
    // A pictured control is named only by its place, so its history entry names the task step it was for, as a click
    // inside a drawn surface does: told only that "button (2 of 2)" was clicked, a done check after the click that
    // opened a Leaflet triangle's popup put the task at 0.75 carried, and at 0.97 told it was for "Click the blue
    // triangle drawn on the map."
    let steps = if context.looks.is_empty() { Vec::new() } else { task_steps(&context.task) };
    if let Some(question) = step_question(&steps, PICTURED_STEP) {
        questions.insert(keys::STEP.into(), question);
    }
    let state = state(observation, controls, context);
    let images: Images = context
        .screenshot
        .iter()
        .chain(context.looks.values())
        .chain(HistoryEntry::looks(&context.history))
        .map(|png| (screenshot_reference(png), png.clone()))
        .collect();
    let imaged = !referenced(&images, &state, &questions).is_empty();
    // Images still pay for every question's input. Pre-asking values for every field on a pictured page enlarged
    // decisions even when the chosen action used none of them; ask the chosen action's follow-ups afterwards.
    if !imaged {
        let written: usize = questions.iter().map(|(key, question)| key.len() + question.dump_json().len()).sum();
        let mut room = LONG_CONTEXT_BYTES.saturating_sub(dumps(&state).len() + written);
        let mut ahead: Vec<(usize, &Ahead)> = context
            .ahead
            .iter()
            .filter(|ahead| offered.contains(&ahead.operation))
            .filter(|ahead| controls.iter().any(|control| control.id == ahead.control))
            .map(|ahead| (ahead.key().len() + ahead.question.dump_json().len(), ahead))
            .collect();
        // The smallest first, so a form's long value questions never push out what its buttons would commit.
        ahead.sort_by_key(|(size, _)| *size);
        for (size, ahead) in ahead {
            if size > room {
                break;
            }
            room -= size;
            questions.insert(ahead.key(), ahead.question.clone());
        }
    }
    Request {
        state,
        images,
        looks: context.looks.clone(),
        steps,
        questions,
        targets,
        groups,
        destinations: drops,
        destination_groups: drop_groups,
        navigation_groups,
        target_rules: target_rules.to_string(),
    }
}

/// OpenRouter's documented point past which a Jev request runs in TypeSafe's long-context mode, which TypeSafe says is
/// slower and less accurate. An image puts a request there too. Questions asked ahead save a round trip only below it.
const LONG_CONTEXT_BYTES: usize = 32_000;

/// Whether the request's text fits Jev's input limit; its pictures are counted by `pictures_fit`.
pub fn fits(request: &Request, config: &Config) -> bool {
    room(text::len(&dumps(&request.state)) as i64, &request.questions, config) >= 0
}

/// Room in the tightest text request, including the target choices Jev may ask after choosing a group.
fn step_room(request: &Request, config: &Config) -> i64 {
    let state_chars = text::len(&dumps(&request.state)) as i64;
    let initial = room(state_chars, &request.questions, config);
    request.groups.iter().fold(initial, |room_left, (&operation, groups)| {
        groups.iter().fold(room_left, |room_left, group| {
            room_left.min(room(state_chars, &group_target_question(request, operation, group), config))
        })
    })
}

/// Whether the request, and each target question asked after a group is chosen, fits Jev's input limit with the tokens
/// of every picture it refers to, once for each use. A group's question shows its members' cards, which the first
/// request does not.
fn pictures_fit(request: &Request, config: &Config) -> bool {
    let state = request.state.to_string();
    let state_chars = text::len(&dumps(&request.state)) as i64;
    let asked = |questions: &IndexMap<String, Question>| {
        let written = format!("{state}{}", serde_json::to_string(questions).unwrap_or_default());
        let tokens: i64 = request
            .images
            .iter()
            .map(|(key, png)| written.matches(key.as_str()).count() as i64 * jev_image_tokens(png))
            .sum();
        room(state_chars, questions, config) >= config.tokens.decision_chars_of(tokens)
    };
    asked(&request.questions)
        && request.groups.iter().all(|(&operation, groups)| {
            groups.iter().all(|group| asked(&group_target_question(request, operation, group)))
        })
}

/// The characters of Jev's input limit `state` leaves unused beside `questions`.
fn room(state_chars: i64, questions: &IndexMap<String, Question>, config: &Config) -> i64 {
    let written: Vec<String> = questions.values().map(Dump::dump_json).collect();
    config.tokens.input_chars(&written) - state_chars
}

/// The question that picks a target from `group` once Jev has chosen it for `operation`.
fn group_target_question(request: &Request, operation: Operation, group: &[Control]) -> IndexMap<String, Question> {
    let question = target_question(operation, group, &request.target_rules, &request.looks);
    IndexMap::from([(keys::target(operation), question.into())])
}

/// The bytes the request's body takes as the Jev client sends it: its state and questions as JSON, and each picture
/// they refer to as a base64 asset.
fn request_bytes(request: &Request) -> usize {
    let questions = serde_json::to_string(&request.questions).map_or(0, |written| written.len());
    let pictures = referenced(&request.images, &request.state, &request.questions);
    request.state.to_string().len() + questions + pictures.iter().map(|(key, png)| asset_bytes(key, png)).sum::<usize>()
}

fn shows_pictures(context: &StepContext) -> bool {
    context.screenshot.is_some()
        || !context.looks.is_empty()
        || context.history.iter().any(|entry| entry.look.is_some())
}

/// Leaves out the picture a decision needs least: a recent action's card from the oldest, then a target's card, then
/// the screenshot.
fn drop_a_picture(context: &mut StepContext) {
    if let Some(entry) = context.history.iter_mut().find(|entry| entry.look.is_some()) {
        entry.look = None;
    } else if context.looks.pop().is_none() {
        context.screenshot = None;
    }
}

/// Leaves out a kind of picture, in the order of `drop_a_picture`: every recent action's card, then every target's
/// card, then the screenshot.
fn drop_pictures(context: &mut StepContext) {
    if context.history.iter().any(|entry| entry.look.is_some()) {
        context.history.iter_mut().for_each(|entry| entry.look = None);
    } else if !context.looks.is_empty() {
        context.looks.clear();
    } else {
        context.screenshot = None;
    }
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
            fastbrowse_core::decision::reserve(ledger, jev, JevPurpose::Action, 0.0, 1)?;
        }
        let answered =
            jev_purpose(JevPurpose::Action, jev.evaluate_images(&request.state, questions, &request.images)).await?;
        if let Some(ledger) = ledger {
            ledger.record([answered.cost.clone()])?;
        }
        cost.push(answered.cost.clone());
        tokens += answered.input_tokens;
        Ok(answered)
    };

    let evaluation = ask(&request.questions).await?;
    let operation_answer = choice_answer(&evaluation, keys::OPERATION)?;
    let read_scope = match operation_answer.choice.as_str() {
        "read_list" => ReadScope::List,
        "stop_list" => ReadScope::Return,
        _ => ReadScope::Page,
    };
    let operation = (read_scope != ReadScope::Page)
        .then_some(Operation::Read)
        .or_else(|| Operation::parse(&operation_answer.choice))
        .ok_or_else(|| {
            AgentError::Invalid(format!("{} is not a valid Operation", text::repr(&operation_answer.choice)))
        })?;
    let operation_confidence = operation_answer.confidence;
    let mut target: Option<Control> = None;
    let mut tab_id: Option<String> = None;
    let mut url: Option<String> = None;
    let mut target_confidence: Option<f64> = None;
    if let Some(candidates) = request.targets.get(&operation) {
        let target_answer = choice_answer(&evaluation, &keys::target(operation))?;
        target = Some(find_control(candidates, destination_pick(candidates, &target_answer))?);
        target_confidence = Some(target_answer.confidence);
    } else if let Some(groups) = request.groups.get(&operation) {
        let group_answer = choice_answer(&evaluation, &keys::group(operation))?;
        let group = &groups[group_index(&group_answer.choice, groups.len())?];
        let key = keys::target(operation);
        let inner = ask(&group_target_question(request, operation, group)).await?;
        let target_answer = choice_answer(&inner, &key)?;
        target = Some(find_control(group, &target_answer.choice)?);
        // The element was only ever chosen from inside the group, so a doubtful group is a doubtful target.
        target_confidence = Some(group_answer.confidence * target_answer.confidence);
    } else if operation == Operation::SwitchTab {
        let target_answer = choice_answer(&evaluation, keys::SWITCH_TAB_TARGET)?;
        tab_id = Some(target_answer.choice.clone());
        target_confidence = Some(target_answer.confidence);
    } else if operation == Operation::Navigate {
        let mut group_confidence = 1.0;
        let (target_answer, question) = if request.navigation_groups.is_empty() {
            let question = match request.questions.get(keys::NAVIGATE_TARGET) {
                Some(Question::Choice(question)) => question.clone(),
                _ => return Err(AgentError::Invalid("navigation requires caller-supplied choices".into())),
            };
            (choice_answer(&evaluation, keys::NAVIGATE_TARGET)?, question)
        } else {
            let group_answer = choice_answer(&evaluation, keys::NAVIGATE_GROUP)?;
            let group = &request.navigation_groups[group_index(&group_answer.choice, request.navigation_groups.len())?];
            group_confidence = group_answer.confidence;
            let question = choice(
                format!("{UNTRUSTED}\nChoose the caller-supplied address for what the task still needs."),
                group.iter().enumerate().map(|(i, url)| (i.to_string(), Value::from(url.as_str()))),
            );
            let questions = IndexMap::from([(keys::NAVIGATE_TARGET.to_string(), question.clone().into())]);
            let inner = ask(&questions).await?;
            (choice_answer(&inner, keys::NAVIGATE_TARGET)?, question)
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
            let destination_answer = choice_answer(&evaluation, keys::DRAG_DESTINATION)?;
            destination = Some(find_control(&request.destinations, &destination_answer.choice)?);
            destination_answer.confidence
        } else {
            let group_answer = choice_answer(&evaluation, keys::DRAG_DESTINATION_GROUP)?;
            let group =
                &request.destination_groups[group_index(&group_answer.choice, request.destination_groups.len())?];
            let questions = IndexMap::from([(keys::DRAG_DESTINATION.to_string(), destination_question(group).into())]);
            let inner = ask(&questions).await?;
            let destination_answer = choice_answer(&inner, keys::DRAG_DESTINATION)?;
            destination = Some(find_control(group, &destination_answer.choice)?);
            group_answer.confidence * destination_answer.confidence
        };
        // A drag is only as certain as the weaker of its two ends.
        target_confidence = Some(target_confidence.map_or(destination_confidence, |t| t.min(destination_confidence)));
    }
    let prefix = target.as_ref().map(|target| format!("{operation}:{}:", target.id));
    let ahead: IndexMap<String, Answer> = prefix
        .iter()
        .flat_map(|prefix| {
            let asked = request.questions.keys().filter(|key| key.starts_with(prefix.as_str()));
            asked.filter_map(|key| Some((key[prefix.len()..].to_string(), evaluation.answers.get(key)?.clone())))
        })
        .collect();
    let chosen = target.as_ref().map(|target| target.id.clone());
    // A plan follows a chosen fill only: after a press the fields may be gone, or filled after what they belong before.
    // It fills the rest of the chosen field's form, which one submit sends: a field of another form is sent by that
    // form's own submit. On CurseForge a reply box's fill planned the page's top-level comment box with the same reply,
    // and that box's Post put it on the page as a new comment (#881).
    let form = target.as_ref().map(|target| &target.form_id);
    let plan: Vec<Planned> = controls
        .iter()
        .filter(|_| operation == Operation::Fill)
        .filter(|control| Some(&control.id) != chosen.as_ref() && Some(&control.form_id) == form)
        .filter_map(|control| {
            let probability = noul_probability(&evaluation, &format!("{PLAN_PREFIX}{}", control.id))?;
            (probability >= EVEN_ODDS).then(|| {
                let prefix = format!("{}:{}:", Operation::Fill, control.id);
                let ahead = evaluation
                    .answers
                    .iter()
                    .filter_map(|(key, answer)| Some((key.strip_prefix(prefix.as_str())?.to_string(), answer.clone())))
                    .collect();
                Planned { target: control.clone(), probability, ahead }
            })
        })
        .collect();
    Ok(Decision {
        read_scope,
        plan,
        operation,
        url,
        target,
        tab_id,
        operation_confidence,
        target_confidence,
        login_required: noul_probability(&evaluation, keys::LOGIN_REQUIRED),
        offered_controls: controls.len() as i64,
        reduction,
        cost,
        input_tokens: tokens,
        bot_check: noul_probability(&evaluation, keys::BOT_CHECK),
        shows_answer: noul_probability(&evaluation, keys::SHOWS_ANSWER),
        site_failure: noul_probability(&evaluation, "site_failure"),
        unobtainable: noul_probability(&evaluation, keys::UNOBTAINABLE),
        inside_drawn: noul_probability(&evaluation, keys::INSIDE_DRAWN),
        destination,
        replayed: false,
        by_code: false,
        offered: Vec::new(),
        ahead,
        step: chosen_step(&evaluation, &request.steps),
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
    // "Next month" and "the next Monday" are relative to today: shown only which month a date picker was on, Jev
    // clicked Next through two years of months looking for next month. Code works each date out, since Jev does not
    // count days.
    state.insert("date".into(), crate::dates::dated(observation.captured_at, &context.dates).into());
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
    // A player's state is in the element, not the text: without it a run could not tell playing from paused.
    if !observation.media.is_empty() {
        state["page"]["media"] = json!(observation.media);
    }
    state.insert("answered_in_notes".into(), context.answered_in_notes.into());
    state.insert("notes".into(), context.notes.clone().into());
    state.insert("recent_actions".into(), Value::Array(context.history.iter().map(HistoryEntry::seen).collect()));
    if context.omitted_actions > 0 {
        state.insert("omitted_actions".into(), context.omitted_actions.into());
    }
    // The operation question also needs a control's context and available actions: without them a covered click
    // made Escape close the task's popup even though the target question chose a control inside it (#884).
    let elements = controls.iter().map(|control| Value::Object(element(control)));
    state.insert("elements".into(), Value::Array(elements.collect()));
    if let Some(background) = context.background.as_deref().map(text::strip).filter(|b| !b.is_empty()) {
        state.insert("background".into(), background.into());
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
    if let Some(context) = context_of(control) {
        element.insert("context".into(), context.into());
    }
    if let Some(href) = &control.href {
        element.insert("href".into(), href.clone().into());
    }
    if pager_link(control) {
        element.insert("next_page".into(), true.into());
    }
    if control.already_read == Some(true) {
        element.insert("already_read".into(), true.into());
    }
    if control.other_site == Some(true) {
        element.insert("other_site".into(), true.into());
    }
    if control.offscreen {
        element.insert("offscreen".into(), true.into());
    }
    element
}

/// Where a control belongs, as a model deciding a step reads it: the step that showed it, then its card, row or
/// section. A popup a page attaches at the end of its body has only its own heading around it, such as "November
/// 2026", and without the step that opened it a run chose an always-visible calendar's dates over it (#884).
fn context_of(control: &Control) -> Option<String> {
    match (&control.shown_by, &control.context) {
        (Some(step), Some(context)) if !context.is_empty() => Some(format!("shown by {step}; {context}")),
        (Some(step), _) => Some(format!("shown by {step}")),
        (None, context) => context.clone(),
    }
}

/// A control as the step's state lists it.
pub fn element(control: &Control) -> Map<String, Value> {
    control_description(control, true)
}

/// A control's description; `shared` adds the context and operations every question reads from the step's state, which
/// a target option refers to by id rather than repeating.
fn control_description(control: &Control, shared: bool) -> Map<String, Value> {
    let mut element = Map::new();
    element.insert("id".into(), control.id.clone().into());
    element.insert("label".into(), control.label.clone().into());
    element.insert("role".into(), control.role.clone().into());
    if shared {
        element.insert("operations".into(), sorted_operations(control));
    }
    let optional = [
        ("context", context_of(control).filter(|_| shared).map(Value::from)),
        ("value", control.value.clone().map(Value::from)),
        // An unfocused control offers nothing an unknown one does not, and every control but one is unfocused.
        ("focused", control.focused.filter(|focused| *focused).map(Value::from)),
        ("href", control.href.clone().map(Value::from)),
        ("next_page", pager_link(control).then_some(true.into())),
        ("already_read", control.already_read.filter(|read| *read).map(Value::from)),
        ("other_site", control.other_site.filter(|other| *other).map(Value::from)),
        ("drawn", control.drawn.filter(|drawn| *drawn).map(Value::from)),
        ("checked", control.checked.map(Value::from)),
        ("selected", control.selected.map(Value::from)),
        ("expanded", control.expanded.map(Value::from)),
        ("input_type", control.input_type.clone().map(Value::from)),
        ("input_name", control.input_name.clone().map(Value::from)),
        ("autocomplete", control.autocomplete.clone().map(Value::from)),
        ("validation_message", control.validation_message.clone().map(Value::from)),
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

/// What a target option's `looks_like` is, told to Jev once in the question rather than in every option.
/// The task's sentences and clauses, which a click that names nothing on the page is recorded as made for.
pub fn task_steps(task: &str) -> Vec<String> {
    spans(task, SpanKind::Str).into_iter().map(|(_, _, step)| step).collect()
}

/// The question choosing which of `steps` an action is for; none when the task is one step.
pub fn step_question(steps: &[String], instructions: &str) -> Option<Question> {
    (steps.len() > 1).then(|| {
        let criteria = steps.iter().enumerate().map(|(index, step)| (index.to_string(), step.clone().into())).collect();
        ChoiceQuestion::new(format!("{UNTRUSTED}\n{instructions}"), criteria).into()
    })
}

/// The step `evaluation` chose among `steps`, or the only one.
pub fn chosen_step(evaluation: &Evaluation, steps: &[String]) -> Option<String> {
    match evaluation.answers.get(keys::STEP) {
        Some(Answer::Choice(picked)) => picked.choice.parse::<usize>().ok().and_then(|index| steps.get(index).cloned()),
        _ => steps.first().cloned(),
    }
}

const PICTURED_STEP: &str = "If the next action is on an element whose option shows `looks_like`, which sentence or clause \
                             of `task` is that action for? Choose the task's own wording.";
const LOOKS_LIKE: &str = "An option's `looks_like` is an image of the card or row that holds that element as the page \
                          draws it, outlined in magenta, for a task that names it by what its picture shows.";

fn target_question(
    operation: Operation,
    candidates: &[Control],
    rules: &str,
    looks: &IndexMap<String, Vec<u8>>,
) -> ChoiceQuestion {
    // An option keeps what tells it apart as a target (label, role, href, value, state), and refers by id to the
    // context and operations the shared state carries for the operation question. Repeated in every option, they
    // added 45% to a step request's bytes and 18% to its cost (#1286).
    let shown = candidates.iter().any(|c| looks.contains_key(&c.id));
    let rules = if shown { json!([rules, LOOKS_LIKE]) } else { json!(rules) };
    choice(
        dumps(&json!({"rules": rules, "operation": operation.as_str()})),
        candidates.iter().map(|c| {
            let mut element = target_element(c, operation);
            if let Some(png) = looks.get(&c.id) {
                element.insert("looks_like".into(), screenshot_reference(png).into());
            }
            (c.id.clone(), Value::Object(element))
        }),
    )
}

/// Ported from browser-use/jev-ultrafast (MIT), snapshot.js: name opening a field separately from typing in it so a
/// picker is a useful click target even when its value is already filled.
fn target_element(control: &Control, operation: Operation) -> Map<String, Value> {
    let mut element = control_description(control, false);
    if operation == Operation::Press && !role_keys(&control.role).is_empty() {
        let label = element["label"].as_str().unwrap_or_default().to_string();
        let given = if control.operations.contains(&Operation::Fill) { ", or a task-supplied key," } else { "" };
        element
            .insert("label".into(), format!("Press a key its {} role supports{given} in {label}", control.role).into());
    } else if control.operations.contains(&Operation::Fill) {
        let label = element["label"].as_str().unwrap_or_default().to_string();
        match operation {
            Operation::Click => {
                element.insert("label".into(), format!("Open {label}").into());
            }
            Operation::Enter => {
                element.insert("label".into(), format!("Press Enter in {label}").into());
            }
            Operation::Press => {
                element.insert("label".into(), format!("Press a task-supplied key in {label}").into());
            }
            _ => {}
        }
    }
    element
}

/// Whether a sign-in wall blocks the task, asked of a fresh page and of the page a run gave up on as signed out.
///
/// It asks what the page's main content asks of its visitor. Asked whether a sign-in form took the place of the
/// page's content, Jev read a page that is only a sign-in, such as an OAuth login, as showing its content: Wago's
/// scored about 0.3 from its text (#855).
pub(crate) fn login_question() -> Question {
    noul(
        "Is signing in what this page asks of its visitor: is its main content a sign-in or account verification form or sign-in options, so the page cannot be read or used without signing in?",
        "The page's main content asks the visitor to sign in or verify an account.",
        "The page's main content is something else, with or without a sign-in link or box beside it, or it refuses access with no way to sign in.",
    )
}

/// The drawn surfaces in view that the page takes input on, as snapshot.js marks them: an image, canvas or video
/// whose content has no page elements to act on.
pub fn drawn_surfaces(controls: &[Control]) -> Vec<&Control> {
    controls.iter().filter(|control| control.drawn == Some(true) && !control.offscreen).collect()
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

/// Jev's pick, with links to one address counted as one destination: a shop's two next-page links split their vote,
/// and a product beat both while they held the most between them.
fn destination_pick<'a>(candidates: &'a [Control], answer: &'a ChoiceAnswer) -> &'a str {
    let chance = |control: &Control| answer.probabilities.get(&control.id).copied().unwrap_or_default();
    let mut votes: HashMap<&str, f64> = HashMap::new();
    for control in candidates {
        if let Some(href) = control.href.as_deref() {
            *votes.entry(href).or_default() += chance(control);
        }
    }
    let total = |control: &Control| control.href.as_deref().map_or(chance(control), |href| votes[href]);
    let chosen = candidates.iter().find(|control| control.id == answer.choice).map_or(0.0, total);
    let best = candidates.iter().max_by(|a, b| total(a).total_cmp(&total(b)));
    best.filter(|best| total(best) > chosen).map_or(&answer.choice, |best| &best.id)
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
    //! Cases about more than the bytes sent; tests/golden_policy.rs replays the recorded requests.
    use fastbrowse_core::models::CostComponent;
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
    use crate::batches::{Known, knowing};
    use crate::support::testing::block_on;
    use fastbrowse_core::decision::NoulAnswer;
    use std::sync::{Arc, Mutex};

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
        exhaust_once: AtomicBool,
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
            exhaust_once: AtomicBool::new(false),
        }
    }

    impl DecisionClient for Fake {
        fn profile(&self) -> DecisionProfile {
            DecisionProfile { max_choice_options: self.options, ..DecisionProfile::jev() }
        }

        fn evaluate_images<'a>(
            &'a self,
            state: &'a Value,
            questions: &'a IndexMap<String, Question>,
            _images: &'a Images,
        ) -> BoxFuture<'a, std::result::Result<Evaluation, DecisionError>> {
            self.evaluate(state, questions)
        }

        fn evaluate<'a>(
            &'a self,
            state: &'a Value,
            questions: &'a IndexMap<String, Question>,
        ) -> BoxFuture<'a, std::result::Result<Evaluation, DecisionError>> {
            self.requests.lock().unwrap().push(questions.clone());
            self.states.lock().unwrap().push(state.clone());
            let exhaust = self.exhaust_once.swap(false, Ordering::SeqCst);
            let relevance = !questions.contains_key("operation");
            let hold =
                relevance && self.interrupt && !questions.contains_key("r0") && !self.retry.load(Ordering::SeqCst);
            Box::pin(async move {
                if exhaust {
                    return Err(DecisionError::RetriesExhausted(Box::new(
                        fastbrowse_core::decision::RetriesExhausted {
                            message: "provider retries exhausted".into(),
                            seconds: 1.0,
                            unaccounted_requests: 0,
                            requests: 1,
                            answered: Vec::new(),
                            last: "503".into(),
                            cause: Default::default(),
                        },
                    )));
                }
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
                                let yes = match self.pick.get(key.as_str()) {
                                    Some(picked) => picked == "yes",
                                    None => relevance && question.instructions.contains(&label),
                                };
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

    fn link(id: &str) -> Control {
        Control { href: Some(format!("/{id}")), ..Control::new(id, "link", id, [Operation::Click]) }
    }

    #[test]
    fn test_control_context_and_actions_are_sent_once_in_shared_state() {
        let carded = |i| Control {
            context: Some(format!("Card {i}")),
            shown_by: Some("clicking Booking date".into()),
            ..button(i)
        };
        let controls: Vec<Control> = (1..=3).map(carded).chain([link("away")]).collect();
        let page = observation(controls.clone());
        let context = StepContext::new("t");
        let targeted = build_request(&page, &controls, &context, &Config::default(), MAX_CHOICE_OPTIONS);
        let criteria: Value = serde_json::from_str(&targeted.questions["click_target"].dump_json()).unwrap();
        // A target option names its element by id; its context and operations are read from the shared state, which
        // the operation question needs them in too (#1286).
        assert_eq!(criteria["criteria"]["b1"], json!({"id": "b1", "label": "Button 1", "role": "button"}));
        assert_eq!(criteria["criteria"]["away"]["href"], "/away");
        assert!(criteria["instructions"].as_str().unwrap().contains("state.elements"));
        let shown = &targeted.state["elements"][0];
        assert_eq!(shown["context"], "shown by clicking Booking date; Card 1");
        assert_eq!(shown["operations"], json!(["click"]));
        assert_eq!(targeted.state["elements"][3]["href"], "/away");
        // Grouped targets retain the same facts in shared state.
        let grouped = build_request(&page, &controls, &context, &Config::default(), 2);
        assert!(!grouped.questions.contains_key("click_target"));
        assert_eq!(grouped.state["elements"][0]["context"], shown["context"]);
        assert_eq!(grouped.state["elements"][0]["operations"], json!(["click"]));
    }

    #[test]
    fn test_a_list_continuation_respects_a_withheld_read() {
        let page = observation(vec![link("next")]);
        let mut context = StepContext { list_read: ListRead::Continue, ..StepContext::new("read every page") };
        for withheld in [false, true] {
            context.withheld = if withheld { vec![Operation::Read] } else { Vec::new() };
            let request = build_request(&page, &page.controls, &context, &Config::default(), MAX_CHOICE_OPTIONS);
            let Question::Choice(operation) = &request.questions[keys::OPERATION] else { unreachable!() };
            assert_eq!(operation.criteria.contains_key("read_list"), !withheld);
            assert!(operation.criteria.contains_key("stop_list"));
            assert!(!request.questions.contains_key("click_target"));
        }
    }

    #[test]
    fn test_a_plan_is_asked_only_when_wanted_and_only_of_fields() {
        let controls = vec![field("name", None), button(1), link("away")];
        let page = observation(controls.clone());
        let plain = build_request(&page, &controls, &StepContext::new("t"), &Config::default(), MAX_CHOICE_OPTIONS);
        assert!(!plain.questions.keys().any(|key| key.starts_with(PLAN_PREFIX)));
        let context = StepContext { plan_ahead: true, ..StepContext::new("t") };
        let planned = build_request(&page, &controls, &context, &Config::default(), MAX_CHOICE_OPTIONS);
        let asked: Vec<_> = planned.questions.keys().filter(|key| key.starts_with(PLAN_PREFIX)).collect();
        assert_eq!(asked, ["plan:name"]);
        let pictured = StepContext { screenshot: Some(vec![0]), ..context };
        let planned = build_request(&page, &controls, &pictured, &Config::default(), MAX_CHOICE_OPTIONS);
        assert!(planned.questions.contains_key("plan:name"), "images keep field planning, with values asked later");
    }

    #[test]
    fn test_unobtainable_is_asked_on_a_fresh_page_and_read_into_the_decision() {
        let page = observation(vec![button(1)]);
        let task = "Add a Raspberry Pi Pico 2 W to the basket";
        let repeat =
            build_request(&page, &page.controls, &StepContext::new(task), &Config::default(), MAX_CHOICE_OPTIONS);
        assert!(!repeat.questions.contains_key(keys::UNOBTAINABLE), "a page state already judged is not asked again");
        let context = StepContext { check_bot: true, ..StepContext::new(task) };
        let fresh = build_request(&page, &page.controls, &context, &Config::default(), MAX_CHOICE_OPTIONS);
        assert!(fresh.questions.contains_key(keys::UNOBTAINABLE));
        let jev = fake(&[("operation", "click"), ("click_target", "b1"), (keys::UNOBTAINABLE, "yes")], "");
        let decision = block_on(decide(&jev, &page, &context, &Config::default(), None, &Trace::disabled())).unwrap();
        assert_eq!(decision.unobtainable, Some(0.9));
    }

    /// A page that takes input on a drawn surface asks whether the task needs an action inside one (#1084); a page
    /// with none, with the surface out of view, or in a document already asked about, does not.
    #[test]
    fn test_inside_drawn_is_asked_of_a_page_with_a_drawn_surface_in_view() {
        let view = Control { drawn: Some(true), ..button(1) };
        let page = observation(vec![button(2), view.clone()]);
        let context = StepContext { check_surface: true, ..StepContext::new("Type tomsmith into the Username field") };
        let config = Config::default();
        let asked = |controls: Vec<Control>, context: &StepContext| {
            let page = observation(controls);
            build_request(&page, &page.controls, context, &config, MAX_CHOICE_OPTIONS)
                .questions
                .contains_key(keys::INSIDE_DRAWN)
        };
        assert!(asked(vec![button(2), view.clone()], &context));
        assert!(
            !asked(vec![view.clone()], &StepContext::new("t")),
            "a document already asked about is not asked again"
        );
        assert!(!asked(vec![button(1)], &context));
        assert!(!asked(vec![Control { offscreen: true, ..view }], &context));
        let jev = fake(&[("operation", "click"), ("click_target", "b1"), (keys::INSIDE_DRAWN, "yes")], "");
        let decision = block_on(decide(&jev, &page, &context, &config, None, &Trace::disabled())).unwrap();
        assert_eq!(decision.inside_drawn, Some(0.9));
    }

    #[test]
    fn test_the_plan_keeps_page_order_and_leaves_out_the_chosen_field() {
        let answers = |operation: &'static str, target: (&'static str, &'static str)| {
            fake(
                &[
                    ("operation", operation),
                    target,
                    ("plan:first", "yes"),
                    ("plan:middle", "no"),
                    ("plan:last", "yes"),
                    ("plan:zip", "yes"),
                ],
                "",
            )
        };
        let page = observation(vec![
            field("first", None),
            field("middle", None),
            field("last", None),
            field("zip", None),
            button(1),
        ]);
        let context = StepContext { plan_ahead: true, ..StepContext::new("check out as Ada Lovelace, 12345") };
        let jev = answers("fill", ("fill_target", "first"));
        let decision = block_on(decide(&jev, &page, &context, &Config::default(), None, &Trace::disabled())).unwrap();
        assert_eq!(decision.target.map(|t| t.id), Some("first".into()));
        let plan: Vec<_> = decision.plan.iter().map(|p| p.target.id.as_str()).collect();
        assert_eq!(plan, ["last", "zip"]);
        assert_eq!(jev.requests.lock().unwrap().len(), 1, "the plan is asked in the decision's own request");

        let jev = answers("click", ("click_target", "b1"));
        let decision = block_on(decide(&jev, &page, &context, &Config::default(), None, &Trace::disabled())).unwrap();
        assert!(decision.plan.is_empty(), "fields are never filled after a press");
    }

    #[test]
    fn test_the_plan_fills_only_the_chosen_fields_form() {
        // A comment page's own box and a reply box opened under one comment are two forms, each with its own Post.
        let in_form = |id: &str, form: &str| Control { form_id: Some(form.into()), ..field(id, None) };
        let page = observation(vec![in_form("page_box", "top"), in_form("reply_box", "reply"), field("search", None)]);
        let jev = fake(
            &[
                ("operation", "fill"),
                ("fill_target", "reply_box"),
                ("plan:page_box", "yes"),
                ("plan:reply_box", "yes"),
                ("plan:search", "yes"),
            ],
            "",
        );
        let context = StepContext { plan_ahead: true, ..StepContext::new("reply to Ada: thanks") };
        let decision = block_on(decide(&jev, &page, &context, &Config::default(), None, &Trace::disabled())).unwrap();
        assert!(decision.plan.is_empty(), "{:?}", decision.plan);
    }

    #[test]
    fn test_links_to_one_address_are_one_destination() {
        let link = |id: &str, href: &str| {
            let mut control = Control::new(id, "link", id, [Operation::Click]);
            control.href = Some(href.to_string());
            control
        };
        let controls = [link("next", "/2"), link("next again", "/2"), link("item", "/item")];
        let probabilities = [("next".to_string(), 0.3), ("next again".to_string(), 0.3), ("item".to_string(), 0.4)];
        let answer = ChoiceAnswer {
            kind: Default::default(),
            choice: "item".into(),
            probabilities: probabilities.into_iter().collect(),
            confidence: 0.4,
        };
        assert_ne!(destination_pick(&controls, &answer), "item");
    }

    fn field(id: &str, focused: Option<bool>) -> Control {
        Control { focused, ..Control::new(id, "textbox", id, [Operation::Fill, Operation::Enter]) }
    }

    #[test]
    fn test_only_a_focused_editable_field_can_be_left() {
        let focused_button = Control { focused: Some(true), ..button(1) };
        // A control naming the operation itself is not trusted to hold focus the browser never reported.
        let custom = Control::new("custom", "textbox", "custom", [Operation::Fill, Operation::Blur]);
        let controls = vec![
            field("left", Some(false)),
            field("unknown", None),
            field("typing", Some(true)),
            focused_button,
            custom,
        ];
        let page = observation(controls.clone());
        let request = build_request(&page, &controls, &StepContext::new("t"), &Config::default(), MAX_CHOICE_OPTIONS);
        let Question::Choice(operation) = &request.questions["operation"] else { panic!("a choice") };
        assert_eq!(
            operation.criteria["blur"],
            json!(
                "Move keyboard focus out of the focused editable field without activating or submitting anything, so the page runs what it does when a field is left."
            )
        );
        let Question::Choice(target) = &request.questions["blur_target"] else { panic!("a choice") };
        assert_eq!(target.criteria.keys().collect::<Vec<_>>(), ["typing"]);
        assert_eq!(target.criteria["typing"]["focused"], json!(true));

        for unfocused in [vec![field("left", Some(false))], vec![field("unknown", None)], vec![button(1)]] {
            let page = observation(unfocused.clone());
            let request =
                build_request(&page, &unfocused, &StepContext::new("t"), &Config::default(), MAX_CHOICE_OPTIONS);
            let Question::Choice(operation) = &request.questions["operation"] else { panic!("a choice") };
            assert!(!operation.criteria.contains_key("blur"));
            assert!(!request.questions.contains_key("blur_target"));
        }
    }

    #[test]
    fn test_a_control_whose_role_defines_keys_is_offered_a_press() {
        let slider = Control::new("volume", "slider", "Volume", [Operation::Click, Operation::Press]);
        let controls = vec![slider.clone(), field("name", None), button(1)];
        let page = observation(controls.clone());
        let request = build_request(&page, &controls, &StepContext::new("t"), &Config::default(), MAX_CHOICE_OPTIONS);
        let Question::Choice(operation) = &request.questions["operation"] else { panic!("a choice") };
        assert_eq!(operation.criteria["press"], json!(PRESS_ROLE_KEYS));
        let Question::Choice(target) = &request.questions["press_target"] else { panic!("a choice") };
        assert_eq!(target.criteria.keys().collect::<Vec<_>>(), ["volume", "name"]);
        assert_eq!(target.criteria["volume"]["label"], json!("Press a key its slider role supports in Volume"));
        assert_eq!(target.criteria["name"]["label"], json!("Press a task-supplied key in name"));
        let spin = Control::new("qty", "spinbutton", "Quantity", [Operation::Fill]);
        assert_eq!(
            target_element(&spin, Operation::Press)["label"],
            json!("Press a key its spinbutton role supports, or a task-supplied key, in Quantity")
        );
        // An activation-only role adds no press, and a page of plain fields is asked what it always was.
        assert!(!takes(&button(1), Operation::Press));
        let native_select = Control::new("native", "combobox", "Choose", [Operation::Select]);
        assert!(!takes(&native_select, Operation::Press));
        let unfocusable = Control::new("cell", "gridcell", "Cell", [Operation::Click]);
        assert!(!takes(&unfocusable, Operation::Press));
        let plain = vec![field("name", None), button(1)];
        let page = observation(plain.clone());
        let request = build_request(&page, &plain, &StepContext::new("t"), &Config::default(), MAX_CHOICE_OPTIONS);
        let Question::Choice(operation) = &request.questions["operation"] else { panic!("a choice") };
        assert_eq!(operation.criteria["press"], json!(operation_label(Operation::Press)));
    }

    #[test]
    fn test_jev_picks_the_focused_field_to_leave() {
        let jev = fake(&[("operation", "blur"), ("blur_target", "typing")], "");
        let page = observation(vec![field("left", Some(false)), field("typing", Some(true))]);
        let context = StepContext::new("leave the field");
        let decision = block_on(decide(&jev, &page, &context, &Config::default(), None, &Trace::disabled())).unwrap();
        assert_eq!((decision.operation, decision.target.map(|t| t.id)), (Operation::Blur, Some("typing".into())));
    }

    #[test]
    fn test_the_decision_sees_the_focused_control_and_no_unfocused_one() {
        assert_eq!(element(&field("reference", Some(true)))["focused"], json!(true));
        for unfocused in [None, Some(false)] {
            assert!(!element(&field("reference", unfocused)).contains_key("focused"));
        }
    }

    #[test]
    fn scrolling_can_choose_a_container_without_requiring_one_on_older_observations() {
        let context = StepContext::new("Read the rest of the side panel");
        let jev = fake(&[("operation", "scroll"), ("scroll_target", "panel")], "");
        let page =
            observation(vec![Control::new("panel", "region", "Vertical scroll area: Side panel", [Operation::Scroll])]);
        let decision = block_on(decide(&jev, &page, &context, &Config::default(), None, &Trace::disabled())).unwrap();
        assert_eq!(decision.target.unwrap().id, "panel");
        let page = observation(vec![]);
        let decision = block_on(decide(&jev, &page, &context, &Config::default(), None, &Trace::disabled())).unwrap();
        assert_eq!(decision.operation, Operation::Scroll);
        assert!(decision.target.is_none());
    }

    #[test]
    fn test_picks_target_for_chosen_operation() {
        let jev = fake(&[("operation", "click"), ("click_target", "b3")], "");
        let mut context = StepContext::new("press button 3");
        context.history = vec![HistoryEntry::new(Some(Operation::Scroll), None, StepOutcome::Executed, true)];
        let page = observation((0..5).map(button).collect());
        let decision = block_on(decide(&jev, &page, &context, &Config::default(), None, &Trace::disabled())).unwrap();
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
        let decision = block_on(decide(&jev, &page, &context, &config, None, &Trace::disabled())).unwrap();
        assert_eq!(decision.target.as_ref().unwrap().id, "b9");
        let requests = jev.requests.lock().unwrap();
        let Question::Choice(inner) = &requests[1]["click_target"] else { panic!("a choice") };
        assert_eq!(inner.criteria.keys().collect::<Vec<_>>(), ["b8", "b9", "b10", "b11"]);
        assert_eq!(decision.cost.len(), 2);
        assert!((decision.confidence() - 0.81).abs() < 1e-9);
    }

    #[test]
    fn test_rules_tell_jev_what_other_site_means_only_when_a_link_carries_it() {
        let mut page = observation((0..3).map(button).collect());
        page.controls[1].role = "link".into();
        page.controls[1].href = Some("https://store.test/item".into());
        let context = StepContext::new("check that the page's sections open");
        let plain = build_request(&page, &page.controls, &context, &Config::default(), MAX_CHOICE_OPTIONS);
        page.controls[1].other_site = Some(true);
        let marked = build_request(&page, &page.controls, &context, &Config::default(), MAX_CHOICE_OPTIONS);
        let instructions = |request: &Request, key: &str| match &request.questions[key] {
            Question::Choice(question) => question.instructions.clone(),
            _ => unreachable!("{key} is a choice"),
        };
        assert!(!instructions(&plain, keys::OPERATION).contains(OTHER_SITE));
        assert!(instructions(&marked, keys::OPERATION).contains(OTHER_SITE));
        assert!(instructions(&marked, &keys::target(Operation::Click)).contains(OTHER_SITE));
        assert_eq!(marked.state["elements"][1]["other_site"], json!(true));
        assert!(OTHER_SITE.contains("leaves the site the browser opened for this task"));
    }

    #[test]
    fn test_a_step_on_a_pictured_control_shows_its_card_in_the_recent_actions() {
        let page = observation((0..3).map(button).collect());
        let mut context = StepContext::new("make the text bold, then save");
        let mut bold = HistoryEntry::new(Some(Operation::Click), Some("button (2 of 5)"), StepOutcome::Executed, true);
        bold.look = Some(Look { before: b"bold".to_vec(), after: None });
        let mut pin = HistoryEntry::new(Some(Operation::Click), Some("canvas"), StepOutcome::Executed, true);
        pin.look = Some(Look { before: b"red pin".to_vec(), after: Some(b"blue pin".to_vec()) });
        context.history = vec![bold, pin];
        let request = build_request(&page, &page.controls, &context, &Config::default(), MAX_CHOICE_OPTIONS);
        let card = screenshot_reference(b"bold");
        assert_eq!(request.state["recent_actions"][0]["looks_like"], json!(card));
        assert_eq!(request.images.get(&card), Some(&b"bold".to_vec()));
        // A click placed in pixels shows its place before and after.
        let (red, blue) = (screenshot_reference(b"red pin"), screenshot_reference(b"blue pin"));
        assert_eq!(request.state["recent_actions"][1]["looked_like_before"], json!(red));
        assert_eq!(request.state["recent_actions"][1]["looks_like_after"], json!(blue));
        assert!(request.images.contains_key(&red) && request.images.contains_key(&blue));
        // Jev is told what the cards are, and only when there are any.
        let Question::Choice(operation) = &request.questions[keys::OPERATION] else { unreachable!() };
        assert!(operation.instructions.contains(HistoryEntry::LOOKS));
        context.history.iter_mut().for_each(|entry| entry.look = None);
        let request = build_request(&page, &page.controls, &context, &Config::default(), MAX_CHOICE_OPTIONS);
        let Question::Choice(operation) = &request.questions[keys::OPERATION] else { unreachable!() };
        assert!(!operation.instructions.contains(HistoryEntry::LOOKS));
        // Questions that send no image name no card.
        assert!(context.history[0].shown().get("looks_like").is_none());
    }

    #[test]
    fn test_a_pictured_target_option_shows_its_card_and_a_group_follow_up_sends_only_its_own() {
        let config = grouped();
        let jev =
            Fake { options: 10, ..fake(&[("operation", "click"), ("click_group", "2"), ("click_target", "b9")], "") };
        let page = observation((0..12).map(button).collect());
        let mut context = StepContext::new("add the red kettle");
        context.looks = IndexMap::from([("b1".to_string(), b"one".to_vec()), ("b9".to_string(), b"nine".to_vec())]);
        let decision = block_on(decide(&jev, &page, &context, &config, None, &Trace::disabled())).unwrap();
        assert_eq!(decision.target.as_ref().unwrap().id, "b9");
        let nine = screenshot_reference(b"nine");
        let requests = jev.requests.lock().unwrap();
        // The group question names no card; the follow-up shows the one card among its options.
        let states = jev.states.lock().unwrap();
        let images = Images::from([(nine.clone(), b"nine".to_vec()), (screenshot_reference(b"one"), b"one".to_vec())]);
        assert!(referenced(&images, &states[0], &requests[0]).is_empty());
        assert_eq!(referenced(&images, &states[1], &requests[1]).keys().collect::<Vec<_>>(), [&nine]);
        let Question::Choice(inner) = &requests[1]["click_target"] else { panic!("a choice") };
        assert_eq!(inner.criteria["b9"]["looks_like"], json!(nine));
        assert!(inner.criteria["b8"].get("looks_like").is_none());
        assert!(inner.instructions.contains(LOOKS_LIKE));

        // Within the option limit the card goes on the target option of the first request.
        let page = observation((0..3).map(button).collect());
        let request = build_request(&page, &page.controls, &context, &Config::default(), MAX_CHOICE_OPTIONS);
        let Question::Choice(target) = &request.questions["click_target"] else { panic!("a choice") };
        assert_eq!(target.criteria["b1"]["looks_like"], json!(screenshot_reference(b"one")));
        assert!(!referenced(&request.images, &request.state, &request.questions).is_empty());
    }

    #[test]
    fn test_an_action_on_a_pictured_control_says_which_task_step_it_is_for() {
        let jev = fake(&[("operation", "click"), ("click_target", "b1"), (keys::STEP, "0")], "");
        let page = observation((0..3).map(button).collect());
        let mut context =
            StepContext::new("Click the blue triangle drawn on the map. What does the popup that opens say?");
        context.looks = IndexMap::from([("b1".to_string(), b"one".to_vec())]);
        let decision = block_on(decide(&jev, &page, &context, &Config::default(), None, &Trace::disabled())).unwrap();
        assert!(decision.step.unwrap().starts_with("Click the blue triangle"));
        // With no pictured control in view, an option's label names it, and no step is asked.
        context.looks.clear();
        let request = build_request(&page, &page.controls, &context, &Config::default(), MAX_CHOICE_OPTIONS);
        assert!(!request.questions.contains_key(keys::STEP));
    }

    #[test]
    fn retries_exhausted_does_not_retry_with_hidden_offscreen_controls() {
        let jev = fake(&[("operation", "click")], "");
        jev.exhaust_once.store(true, Ordering::SeqCst);
        let mut page = observation(vec![button(0), button(1)]);
        page.controls[1].offscreen = true;
        let mut config = Config::default();
        config.tokens.batch_tokens = 1;
        let result =
            block_on(decide(&jev, &page, &StepContext::new("press a button"), &config, None, &Trace::disabled()));
        assert!(
            matches!(result, Err(AgentError::Jev(DecisionError::RetriesExhausted(error))) if error.message == "provider retries exhausted")
        );
        assert_eq!(jev.requests.lock().unwrap().len(), 1);
        assert!(
            jev.states.lock().unwrap()[0]["elements"].as_array().unwrap().iter().any(|element| element["id"] == "b1")
        );
    }

    #[test]
    fn test_each_request_including_groups_needs_budget() {
        let config = grouped();
        let jev = Fake { options: 10, ..fake(&[("operation", "click"), ("click_group", "0")], "") };
        let ledger = Ledger::new(Limits { max_jev_calls: Some(1), ..Limits::default() });
        let page = observation((0..12).map(button).collect());
        let context = StepContext::new("press button 3");
        let result = block_on(decide(&jev, &page, &context, &config, Some(&ledger), &Trace::disabled()));
        assert!(matches!(result, Err(AgentError::Budget(_))));
        assert_eq!((jev.requests.lock().unwrap().len(), ledger.state().jev_calls), (1, 1));
    }

    #[test]
    fn test_redraw_keeps_answered_batches_and_retries_unfinished_batches_in_parallel() {
        // Two options of two controls offer four at most, so five are shortlisted first.
        let mut config = Config { observation: ObservationLimits { group_size: 2 }, ..Config::default() };
        config.tokens.batch_tokens = 1;
        let known = Arc::new(Known::default());
        let mut jev = Fake { options: 2, ..fake(&[("operation", "click")], "Button 0") };
        jev.interrupt = true;
        let page = observation((0..5).map(button).collect());
        let context = StepContext::new("press button 3");
        let trace = Trace::disabled();
        {
            let mut pending = Box::pin(knowing(known.clone(), decide(&jev, &page, &context, &config, None, &trace)));
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
        let result = block_on(knowing(known, decide(&jev, &fresh, &context, &config, None, &trace))).unwrap();
        let retries: Vec<_> = relevance_requests(&jev).into_iter().skip(before).collect();
        assert_eq!(retries.len(), 4);
        assert!(!retries.iter().flat_map(|r| r.values()).any(|q| match q {
            Question::Noul(q) => q.instructions.contains("\"Button 0\""),
            _ => false,
        }));
        assert!(result.target.unwrap().id.starts_with("fresh-"));
    }

    #[test]
    fn task_relevance_survives_a_scroll_but_changed_controls_are_judged_again() {
        let config = Config { observation: ObservationLimits { group_size: 2 }, ..Config::default() };
        let jev = Fake { options: 2, ..fake(&[("operation", "click")], "Button 0") };
        let known = Arc::new(Known::default());
        let mut page = observation((0..5).map(button).collect());
        page.controls[1].role = "link".into();
        page.controls[1].href = Some("https://example.test/result".into());
        let mut context = StepContext::new("press button 3");
        let trace = Trace::disabled();
        block_on(knowing(known.clone(), decide(&jev, &page, &context, &config, None, &trace))).unwrap();
        let before = relevance_requests(&jev).len();
        assert!(before > 0);
        assert!(RELEVANCE.contains("any part of the task"));
        context.history.push(HistoryEntry::new(Some(Operation::Scroll), None, StepOutcome::Executed, true));
        context.answered_in_notes = true;
        page.controls.iter_mut().for_each(|control| {
            control.offscreen = true;
        });
        block_on(knowing(known.clone(), decide(&jev, &page, &context, &config, None, &trace))).unwrap();
        assert_eq!(relevance_requests(&jev).len(), before, "scrolling does not change task relevance");
        page.controls[0].label = "Changed button".into();
        block_on(knowing(known.clone(), decide(&jev, &page, &context, &config, None, &trace))).unwrap();
        let requests = relevance_requests(&jev);
        assert_eq!(requests.len(), before + 1);
        assert_eq!(requests.last().unwrap().len(), 1, "only the changed control needs a new judgment");
        page.controls[1].already_read = Some(true);
        block_on(knowing(known.clone(), decide(&jev, &page, &context, &config, None, &trace))).unwrap();
        let requests = relevance_requests(&jev);
        assert_eq!(requests.len(), before + 2, "a newly read link needs its relevance judged again");
        assert_eq!(requests.last().unwrap().len(), 1);
        let question = requests.last().unwrap().values().next().unwrap().dump_json();
        assert!(question.contains("already_read"), "shortlisting must see read state before omitting controls");
        context.history.push(HistoryEntry::new(Some(Operation::Click), Some("Button 0"), StepOutcome::Executed, false));
        let before_action = relevance_requests(&jev).len();
        block_on(knowing(known.clone(), decide(&jev, &page, &context, &config, None, &trace))).unwrap();
        assert!(relevance_requests(&jev).len() > before_action, "completed actions can change what is still useful");
        context.subgoal = Some("Use the form's submit button to apply its values".into());
        let before_subgoal = relevance_requests(&jev).len();
        block_on(knowing(known, decide(&jev, &page, &context, &config, None, &trace))).unwrap();
        assert!(relevance_requests(&jev).len() > before_subgoal, "recovery can change which controls are useful");
        assert!(jev.states.lock().unwrap().iter().any(|state| {
            state.get("rules").and_then(Value::as_str) == Some(RELEVANCE.as_str())
                && state.get("subgoal").and_then(Value::as_str) == context.subgoal.as_deref()
        }));
    }

    /// A recent action's card for each of `count` clicks, each `bytes` long and told apart by its first byte.
    fn carded_history(count: u8, bytes: usize) -> Vec<HistoryEntry> {
        (0..count)
            .map(|i| {
                let mut entry = HistoryEntry::new(Some(Operation::Click), Some("canvas"), StepOutcome::Executed, false);
                let mut card = vec![0u8; bytes];
                card[0] = i;
                entry.look = Some(Look { before: card, after: None });
                entry
            })
            .collect()
    }

    /// Sixteen viewport-sized cards are about 47 MiB as base64, past Jev's 32 MiB body limit: the oldest are left out
    /// before the request is sent, the newest and the screenshot kept, and one request is asked (#1301).
    #[test]
    fn test_a_step_with_more_cards_than_the_body_takes_leaves_out_the_oldest() {
        let jev = fake(&[("operation", "click")], "");
        let mut context = StepContext::new("press button 1");
        context.screenshot = Some(vec![7u8; 1024 * 1024]);
        context.history = carded_history(16, 2_200_000);
        let page = observation((0..3).map(button).collect());
        block_on(decide(&jev, &page, &context, &Config::default(), None, &Trace::disabled())).unwrap();
        let states = jev.states.lock().unwrap();
        let carded: Vec<bool> = states[0]["recent_actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry.get("looks_like").is_some())
            .collect();
        let kept = carded.iter().filter(|carded| **carded).count();
        assert!((1..16).contains(&kept), "{carded:?}");
        assert!(carded[16 - kept..].iter().all(|carded| *carded), "the newest cards are kept: {carded:?}");
        assert!(states[0].get("viewport_screenshot").is_some());
        let pictures = 1024 * 1024 + kept * 2_200_000;
        assert!(pictures.div_ceil(3) * 4 <= JEV_REQUEST_BYTES, "{kept} cards do not fit");
    }

    /// Refuses as too large any request that carries more than one picture, as Jev refuses a body past its limit.
    struct Refusing {
        inner: Fake,
        pictures: Mutex<Vec<usize>>,
    }

    impl DecisionClient for Refusing {
        fn profile(&self) -> DecisionProfile {
            self.inner.profile()
        }

        fn evaluate_images<'a>(
            &'a self,
            state: &'a Value,
            questions: &'a IndexMap<String, Question>,
            images: &'a Images,
        ) -> BoxFuture<'a, std::result::Result<Evaluation, DecisionError>> {
            let sent = referenced(images, state, questions).len();
            self.pictures.lock().unwrap().push(sent);
            if sent > 1 {
                return Box::pin(async { Err(DecisionError::input_too_large("over the body limit")) });
            }
            self.inner.evaluate(state, questions)
        }

        fn evaluate<'a>(
            &'a self,
            state: &'a Value,
            questions: &'a IndexMap<String, Question>,
        ) -> BoxFuture<'a, std::result::Result<Evaluation, DecisionError>> {
            self.inner.evaluate(state, questions)
        }
    }

    /// A step Jev refuses as too large is asked again with fewer pictures, never with the same ones: the cards go
    /// before any text or control is cut. Sent the same pictures each time, one step was refused 28 times (#1301).
    #[test]
    fn test_a_step_refused_as_too_large_is_never_resent_with_the_same_pictures() {
        let jev = Refusing { inner: fake(&[("operation", "click")], ""), pictures: Mutex::new(Vec::new()) };
        let mut context = StepContext::new("press button 1");
        context.screenshot = Some(vec![7u8; 1000]);
        context.history = carded_history(3, 1000);
        let page = observation((0..3).map(button).collect());
        let decision = block_on(decide(&jev, &page, &context, &Config::default(), None, &Trace::disabled())).unwrap();
        assert_eq!(*jev.pictures.lock().unwrap(), vec![4, 1]);
        assert_eq!(decision.reduction, Reduction::None, "no text or control was cut for the pictures");
    }

    /// Refuses as too large a request past Jev's 32,000 tokens for its state and largest question as Jev counts them:
    /// its text at 1.5 characters a token and each picture at its measured count for every place it is used.
    struct Counting {
        inner: Fake,
        refused: Mutex<usize>,
    }

    impl DecisionClient for Counting {
        fn profile(&self) -> DecisionProfile {
            self.inner.profile()
        }

        fn evaluate_images<'a>(
            &'a self,
            state: &'a Value,
            questions: &'a IndexMap<String, Question>,
            images: &'a Images,
        ) -> BoxFuture<'a, std::result::Result<Evaluation, DecisionError>> {
            let written = format!("{state}{}", serde_json::to_string(questions).unwrap());
            let pictures: i64 = images
                .iter()
                .map(|(key, png)| written.matches(key.as_str()).count() as i64 * jev_image_tokens(png))
                .sum();
            let largest = questions.values().map(|question| text::len(&question.dump_json())).max().unwrap_or(0);
            let text = (text::len(&dumps(state)) + largest) as f64 / fastbrowse_core::config::JSON_CHARS_PER_TOKEN;
            if text as i64 + pictures > 32_000 {
                *self.refused.lock().unwrap() += 1;
                return Box::pin(async { Err(DecisionError::input_too_large("max_tokens_exceeded")) });
            }
            self.inner.evaluate(state, questions)
        }

        fn evaluate<'a>(
            &'a self,
            state: &'a Value,
            questions: &'a IndexMap<String, Question>,
        ) -> BoxFuture<'a, std::result::Result<Evaluation, DecisionError>> {
            self.inner.evaluate(state, questions)
        }
    }

    /// Eight viewport-sized cards are about 29,000 tokens as Jev counts them, under the body limit in bytes but past
    /// its input limit beside a page's text: the oldest are left out before the first request is sent, so Jev is never
    /// sent one it refuses (#1303).
    #[test]
    fn test_a_step_whose_cards_overrun_jevs_input_limit_leaves_out_the_oldest_before_asking() {
        let jev = Counting { inner: fake(&[("operation", "click")], ""), refused: Mutex::new(0) };
        let mut context = StepContext::new("press button 1");
        context.history = (0..8u8)
            .map(|i| {
                let mut card = Vec::new();
                image::RgbaImage::from_pixel(1568, 882, image::Rgba([i, 0, 0, 255]))
                    .write_to(&mut std::io::Cursor::new(&mut card), image::ImageFormat::Png)
                    .unwrap();
                let mut entry = HistoryEntry::new(Some(Operation::Click), Some("canvas"), StepOutcome::Executed, false);
                entry.look = Some(Look { before: card, after: None });
                entry
            })
            .collect();
        let mut page = observation((0..3).map(button).collect());
        page.viewport_text = "Plain page text. ".repeat(1200);
        block_on(decide(&jev, &page, &context, &Config::default(), None, &Trace::disabled())).unwrap();
        assert_eq!(*jev.refused.lock().unwrap(), 0, "a request Jev counts past its limit was sent");
        let states = jev.inner.states.lock().unwrap();
        let carded: Vec<bool> = states[0]["recent_actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry.get("looks_like").is_some())
            .collect();
        let kept = carded.iter().filter(|carded| **carded).count();
        assert!((1..8).contains(&kept), "{carded:?}");
        assert!(carded[8 - kept..].iter().all(|carded| *carded), "the newest cards are kept: {carded:?}");
    }

    /// A group's follow-up shows its members' cards, which the request that chose the group did not: with the run's
    /// cards filling most of Jev's input limit, the follow-up is fitted before the first request is sent, rather than
    /// sent and refused.
    #[test]
    fn test_a_group_follow_up_whose_cards_overrun_jevs_input_limit_is_fitted_before_asking() {
        let card = |shade: u8| {
            let mut png = Vec::new();
            image::RgbaImage::from_pixel(1568, 882, image::Rgba([shade, 0, 0, 255]))
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            png
        };
        let inner =
            Fake { options: 10, ..fake(&[("operation", "click"), ("click_group", "2"), ("click_target", "b9")], "") };
        let jev = Counting { inner, refused: Mutex::new(0) };
        let mut context = StepContext::new("add the red kettle");
        context.history = (0..7u8)
            .map(|i| {
                let mut entry = HistoryEntry::new(Some(Operation::Click), Some("canvas"), StepOutcome::Executed, false);
                entry.look = Some(Look { before: card(i), after: None });
                entry
            })
            .collect();
        context.looks = (8..11).map(|i| (format!("b{i}"), card(200 + i as u8))).collect();
        let mut page = observation((0..12).map(button).collect());
        page.viewport_text = "Plain page text. ".repeat(300);
        let decision = block_on(decide(&jev, &page, &context, &grouped(), None, &Trace::disabled())).unwrap();
        assert_eq!(decision.target.as_ref().unwrap().id, "b9");
        assert_eq!(*jev.refused.lock().unwrap(), 0, "a follow-up Jev counts past its limit was sent");
    }

    /// A request no larger than Jev's documented limit in tokens, at 1.5 characters a token.
    fn tight() -> Config {
        let mut config = Config::default();
        (config.tokens.state_plus_largest_question, config.tokens.state_plus_all_questions) = (4000, 8000);
        config
    }

    #[test]
    fn test_a_large_prior_page_fact_fits_jevs_limit_without_erasing_the_current_page() {
        let jev = fake(&[("operation", "click")], "");
        let mut context = StepContext::new("press button 3");
        context.check_login = true;
        context.page_facts = vec![
            "No stored secrets are available for this page's origin.".into(),
            format!("Previously observed source: {}", "catalogue data ".repeat(5000)),
        ];
        let mut page = observation((0..3).map(button).collect());
        page.viewport_text = "Current account sign-in page".into();
        let decision = block_on(decide(&jev, &page, &context, &tight(), None, &Trace::disabled())).unwrap();
        assert_eq!(decision.reduction, Reduction::Trimmed);
        let states = jev.states.lock().unwrap();
        assert_eq!(states[0]["page"]["text"], "Current account sign-in page");
        assert!(states[0]["page_facts"].to_string().contains("Jev's input limit"));
        assert_eq!(states[0]["page_facts"][0], "No stored secrets are available for this page's origin.");
        assert_eq!(states[0]["elements"].as_array().unwrap().len(), 3);
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
        let decision = block_on(decide(&jev, &page, &context, &tight(), None, &Trace::disabled())).unwrap();
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
        let decision = block_on(decide(&jev, &page, &context, &tight(), None, &Trace::disabled())).unwrap();
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
        let decision = block_on(decide(&jev, &page, &context, &Config::default(), None, &Trace::disabled())).unwrap();
        assert_eq!(decision.reduction, Reduction::None);
        assert_eq!(jev.states.lock().unwrap()[0]["page"]["text"], page.viewport_text);
    }

    /// Shown only the viewport of a long list, Jev scrolled screen by screen and never read the row further down.
    #[test]
    fn test_jev_is_told_how_much_page_text_lies_outside_the_viewport() {
        let jev = fake(&[("operation", "read")], "");
        let mut page = observation((0..3).map(button).collect());
        page.offscreen_text = "Mongolia".into();
        let context = StepContext::new("Find the population listed for the last row");
        block_on(decide(&jev, &page, &context, &Config::default(), None, &Trace::disabled())).unwrap();
        let states = jev.states.lock().unwrap();
        assert_eq!(states[0]["page"]["offscreen_text"], "Mongolia");
        assert!(operation_label(Operation::Read).contains("the whole page including text outside the viewport"));
    }

    /// On the httpx page a task asked it to open, Jev clicked on to GitHub about half the time, told what done means
    /// only for a written answer (#964).
    #[test]
    fn test_done_says_a_task_to_open_a_page_is_carried_out_on_that_page() {
        assert!(operation_label(Operation::Done).contains(
            "A task that only asks to open or reach a page is carried out once the current page is that page."
        ));
    }

    /// A media QA run pressed Play and Pause to its step limit, shown no player's state; a page without one adds nothing.
    #[test]
    fn test_jev_is_shown_each_players_state() {
        let jev = fake(&[("operation", "read")], "");
        let mut page = observation(vec![button(1)]);
        let context = StepContext::new("Confirm the lesson audio plays");
        block_on(decide(&jev, &page, &context, &Config::default(), None, &Trace::disabled())).unwrap();
        page.media = vec![
            serde_json::from_value(json!({"kind": "audio", "label": "Lesson one", "paused": false,
            "ended": false, "muted": false, "elapsed_seconds": 12.0, "duration_seconds": 30.0}))
            .unwrap(),
        ];
        block_on(decide(&jev, &page, &context, &Config::default(), None, &Trace::disabled())).unwrap();
        let states = jev.states.lock().unwrap();
        assert!(states[0]["page"].get("media").is_none());
        assert_eq!(states[1]["page"]["media"][0]["paused"], false);
        assert_eq!(states[1]["page"]["media"][0]["elapsed_seconds"], 12.0);
    }

    #[test]
    fn test_what_the_page_says_about_a_field_is_shown_to_jev() {
        let mut field = button(1);
        field.required = Some(true);
        field.invalid = Some(true);
        field.validation_message = Some("Please fill in this field.".into());
        assert_eq!(element(&field)["required"], true);
        assert_eq!(element(&field)["invalid"], true);
        assert_eq!(element(&field)["validation_message"], "Please fill in this field.");
        assert!(!element(&button(2)).contains_key("required"));
        assert!(!element(&button(2)).contains_key("invalid"));
    }

    #[test]
    fn test_the_choice_is_told_today_for_a_task_relative_to_it() {
        let mut page = observation(vec![]);
        page.captured_at = DateTime::new(2026, 9, 26, 20, 0, 0).utc();
        let friday = crate::dates::DateReference {
            text: "next month's first Friday".into(),
            year: None,
            month: None,
            day: None,
            weekday: Some("friday".into()),
            nth: Some(1),
            days_from_today: None,
            months_from_now: Some(1),
        };
        let context = StepContext { dates: vec![friday], ..StepContext::new("Select next month's first Friday") };
        let state = build_request(&page, &[], &context, &Config::default(), MAX_CHOICE_OPTIONS).state;
        assert_eq!(
            state["date"],
            "2026-09-26 (Saturday)\nThe dates the task names, worked out from today:\nnext month's first Friday: 2026-10-02 (Friday)"
        );
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
    fn test_follow_ups_go_with_the_decision_only_while_it_stays_out_of_long_context() {
        let control = button(1);
        let ahead = |content: &str| Ahead {
            operation: Operation::Click,
            control: control.id.clone(),
            name: "irreversible".into(),
            question: noul(content, "yes", "no"),
        };
        let page = observation(vec![control.clone()]);
        let controls = std::slice::from_ref(&control);
        let asked = |context: &StepContext| {
            let request = build_request(&page, controls, context, &Config::default(), MAX_CHOICE_OPTIONS);
            request.questions.keys().any(|key| key.ends_with(":irreversible"))
        };

        let mut context = StepContext::new("Find the price");
        context.ahead = vec![ahead("Is this irreversible?")];
        assert!(asked(&context));

        context.ahead = vec![ahead(&"x".repeat(LONG_CONTEXT_BYTES))];
        assert!(!asked(&context));

        context.ahead = vec![ahead(&"x".repeat(LONG_CONTEXT_BYTES)), ahead("Is this irreversible?")];
        assert!(asked(&context));

        context.ahead = vec![ahead("Is this irreversible?")];
        context.screenshot = Some(vec![0]);
        assert!(!asked(&context));
        context.plan_ahead = true;
        assert!(!asked(&context), "an image does not make speculative follow-ups free");
        context.screenshot = None;
        context.looks.insert(control.id.clone(), vec![0]);
        assert!(!asked(&context), "a target card still leaves values as follow-ups");
        context.looks.clear();
        let mut previous = HistoryEntry::new(Some(Operation::Click), Some("button"), StepOutcome::Executed, true);
        previous.look = Some(Look { before: vec![0], after: None });
        context.history.push(previous);
        assert!(!asked(&context), "a pictured history entry still leaves values as follow-ups");
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
        let trace = Trace::disabled();
        assert_send(&decide(&jev, &page, &context, &config, None, &trace));
    }
}
