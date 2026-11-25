//! What a step is: the decision the driver's action becomes, the history and form state it is told of, and the
//! controls and operations a page offers it.

use fastbrowse_core::decision::{Answer, ChoiceQuestion, Evaluation, Question};
use fastbrowse_core::models::{Operation, StepOutcome, UNTRUSTED};
use fastbrowse_core::page::{Control, Observation, pager_link};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::{lax, sha256_hex, text};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// The keys of the questions a step request asks Jev, which it answers under the same keys.
pub(crate) mod keys {
    pub const STEP: &str = "step";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryEntry {
    /// None for a move code made before Jev's first step.
    #[serde(deserialize_with = "lax::nullable")]
    pub operation: Option<Operation>,
    #[serde(deserialize_with = "lax::nullable")]
    pub target: Option<String>,
    /// The named control the drag was dispatched to. The effect records what the page showed afterward.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
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
    /// What a fill or select replaced: the value the field held when the step began, masked. Kept out of what models
    /// are shown; a save's authorization questions state it beside the value typed.
    #[serde(skip)]
    pub before: Option<String>,
    /// The task step a click placed inside a drawn surface was made for. Kept apart from `target`, which actions are
    /// withheld and counted by.
    #[serde(skip)]
    pub for_step: Option<String>,
    /// The card of the pictured control the step acted on, as its option showed it. Such a control is named only by
    /// its place, so without it an editor's Bold and Save read as "button (2 of 5)" and "button (5 of 5)": the run was
    /// judged not to have made the text bold, and pressed Bold again.
    #[serde(skip)]
    pub look: Option<Look>,
    /// The form the step submitted, kept out of what models are shown but for a question about another submission.
    #[serde(skip)]
    pub submitted: Option<Submitted>,
    /// The commit the step sent, if it sent one: a run that posted two replies and then stopped reported both as typed
    /// but unsent. Kept out of what models are shown: it is the run's own record.
    #[serde(skip)]
    pub commit: Option<Commit>,
    /// The form or field group the step's target belongs to, as the page reports it: a commit sends the fields of its
    /// own form, and no others.
    #[serde(skip)]
    pub form: Option<Form>,
    /// The step was sent and made a change the site keeps: an irreversible commit by the authorization classification,
    /// or a reversible change such as a saved setting. Kept out of what models are shown, and the only steps the
    /// coverage check is told of.
    #[serde(skip)]
    pub committed: bool,
    /// The document that supplied the action, kept private so form changes cannot cross document instances.
    #[serde(skip)]
    pub document_key: Option<String>,
    /// The id of the control the action targeted, which a field is matched by when its label changes as it is typed in.
    #[serde(skip)]
    pub control_id: Option<String>,
    /// The form instance the action targeted, kept private so same-label fields cannot cross form instances.
    #[serde(skip)]
    pub form_id: Option<String>,
    #[serde(skip)]
    /// The frame `form_id` is local to: two frames can each hold a form with the same id.
    pub frame_id: Option<String>,
    /// Form instances that appeared after this action, kept private so an unrelated click is not called the opener.
    #[serde(skip)]
    pub opened_forms: Vec<(Option<String>, String)>,
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

/// The form a step submitted, and what its other fields held as it went.
#[derive(Clone, Debug, PartialEq)]
pub struct Submitted {
    /// The top-level document containing the form.
    document: (String, String),
    frame_id: Option<String>,
    frame_document_key: Option<String>,
    form_id: String,
    held: Vec<String>,
}

impl Submitted {
    /// The form `operation` on `target` submits in `observation`; None for a step that submits no form. A field
    /// Enter submits from is among what the form held.
    pub fn of(operation: Operation, target: &Control, observation: &Observation) -> Option<Submitted> {
        let form_id = target.form_id.clone().filter(|_| fastbrowse_core::safety::submits(operation, target))?;
        let own = target
            .value
            .as_deref()
            .filter(|value| !value.is_empty() && !target.sensitive && target.role != "button")
            .map(|value| format!("{} holds {}", text::repr(&target.label), text::repr(value)));
        // A masked field is said to hold a value without the value, so a form whose only entry was a password is not
        // told as empty.
        let masked = observation.controls.iter().filter(|c| {
            c.sensitive
                && c.value.as_deref().is_some_and(|value| !value.is_empty())
                && (&c.frame_id, &c.form_id) == (&target.frame_id, &target.form_id)
        });
        let held = own
            .into_iter()
            .chain(fastbrowse_core::safety::form_held(target, &observation.controls))
            .chain(masked.map(|c| format!("{} holds a masked value", text::repr(&c.label))))
            .collect();
        Some(Submitted {
            document: (observation.url.clone(), observation.document_key.clone()),
            frame_id: target.frame_id.clone(),
            frame_document_key: target.frame_document_key.clone(),
            form_id,
            held,
        })
    }

    /// Which form this was beside the one `target` submits, and what it held.
    fn told(&self, target: Option<&Control>, observation: &Observation) -> String {
        // Back can reload the same form with fresh ids. Calling it different led to a duplicate submission.
        let same_document = !observation.document_key.is_empty() && observation.document_key == self.document.1;
        let which = if same_document {
            match target.and_then(|target| target.form_id.as_ref().map(|id| (target, id))) {
                Some((target, id)) if target.frame_id == self.frame_id => {
                    let frame_reloaded = self.frame_id.is_some()
                        && match (&self.frame_document_key, &target.frame_document_key) {
                            (Some(previous), Some(current)) => previous != current,
                            // Older observations lack the receiving document, so a changed id could be a reload.
                            _ => id != &self.form_id,
                        };
                    if frame_reloaded {
                        "an earlier form whose relationship to the current form is unknown"
                    } else if id == &self.form_id {
                        "the same form the action now asked about submits"
                    } else {
                        "a different form from the one the action now asked about submits"
                    }
                }
                Some(_) => "a different form from the one the action now asked about submits",
                None => "a form from this document",
            }
        } else if observation.url != self.document.0 {
            "a form submitted from another address"
        } else if observation.document_key.is_empty() || self.document.1.is_empty() {
            "a form submitted at this address; its relation to the current form is unknown"
        } else {
            "a form from an earlier document at this address; its relation to the current form is unknown"
        };
        // The observation can omit fields in the submitted form, so no recorded values is not proof of emptiness.
        match self.held.is_empty() {
            true => format!("{which}; the observation recorded no field values"),
            false => format!("{which}; in it, {}", self.held.join("; ")),
        }
    }
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

impl HistoryEntry {
    pub fn new(operation: Option<Operation>, target: Option<&str>, outcome: StepOutcome, page_changed: bool) -> Self {
        HistoryEntry {
            operation,
            target: target.map(str::to_string),
            destination: None,
            outcome,
            page_changed,
            note: None,
            text: None,
            effect: None,
            read_progress: false,
            setting: None,
            added: false,
            set: Vec::new(),
            before: None,
            for_step: None,
            look: None,
            submitted: None,
            commit: None,
            form: None,
            committed: false,
            document_key: None,
            control_id: None,
            form_id: None,
            frame_id: None,
            opened_forms: Vec::new(),
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

    /// The entry as shown to a question about submitting `target`'s form: a submission says which form it sent and
    /// what that held. Shown only "click Post" with the page unchanged after the run had pressed the empty top-level
    /// Post, Jev judged a thread's reply already posted, 0.6 to 0.4, and refused it; told the Post sent another,
    /// empty form, it judged the reply asked for (#881).
    pub fn shown_beside(&self, target: Option<&Control>, observation: &Observation) -> Value {
        let mut shown = self.shown();
        if let Some(submitted) = &self.submitted {
            shown["form"] = submitted.told(target, observation).into();
        }
        shown
    }

    /// The entry as the coverage check is told of a step the site keeps: what was done, not what it showed. An
    /// effect names controls and counts the answer does not state, and a count there read as a missing fact (#1467).
    pub fn kept(&self) -> Value {
        let shown = self.shown();
        let mut kept = serde_json::Map::new();
        for field in ["operation", "target", "outcome"] {
            if let Some(value) = shown.get(field) {
                kept.insert(field.into(), value.clone());
            }
        }
        Value::Object(kept)
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

    pub(crate) fn has_unperformed_actions(history: &[HistoryEntry]) -> bool {
        history.iter().any(|entry| entry.acts() && entry.outcome != StepOutcome::Executed)
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
pub struct Decision {
    pub operation: Operation,
    #[serde(default)]
    pub url: Option<String>,
    pub target: Option<Control>,
    pub tab_id: Option<String>,
    pub operation_confidence: f64,
    /// Drop target of a drag: the control the element Jev chose to drag is released onto.
    #[serde(default)]
    pub destination: Option<Control>,
    /// A recipe step carried out again: no model chose it in this run, so no model's confidence vouches for it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replayed: bool,
    /// A step the run's own code chose rather than Jev, such as opening an unread next page of a whole-list task.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub by_code: bool,
    /// The value the chat driver gave for a fill, select, key press or wait, which the action uses instead of asking for
    /// one.
    #[serde(skip)]
    pub given: Option<Given>,
}

/// A value the chat driver supplies with its action: text it wrote, a secret it names and never sees, or how long a
/// wait may last.
#[derive(Debug, Clone, PartialEq)]
pub enum Given {
    Text(String),
    Secret(String),
    Seconds(f64),
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
            Decider::Llm
        }
    }

    pub fn confidence(&self) -> f64 {
        self.operation_confidence
    }
}

/// Whether `control` takes `operation`. A field that takes a fill also takes a key; other keyboard targets are
/// offered by the observation, which knows whether the control can receive focus.
pub fn takes(control: &Control, operation: Operation) -> bool {
    control.operations.contains(&operation)
        || (operation == Operation::Press && control.operations.contains(&Operation::Fill))
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
        ("context", control.context.clone().filter(|_| shared).map(Value::from)),
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
        ("role_description", control.role_description.clone().map(Value::from)),
        ("description", control.description.clone().map(Value::from)),
        ("item_position", control.item_position.map(Value::from)),
        ("item_count", control.item_count.map(Value::from)),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn button(i: usize) -> Control {
        Control::new(&format!("b{i}"), "button", &format!("Button {i}"), [Operation::Click])
    }

    fn field(id: &str, focused: Option<bool>) -> Control {
        Control { focused, ..Control::new(id, "textbox", id, [Operation::Fill, Operation::Enter]) }
    }

    fn observation(controls: Vec<Control>) -> Observation {
        serde_json::from_value(json!({
            "url": "https://example.test/", "title": "t", "page_key": "k", "captured_at": "2026-09-26T20:00:00Z",
            "controls": controls.iter().map(Dump::dump).collect::<Vec<_>>(),
            "viewport_text": "", "tabs": [],
        }))
        .unwrap()
    }

    #[test]
    fn submitted_form_relationship_is_unknown_after_iframe_reload() {
        let mut target = button(0);
        target.frame_id = Some("frame".into());
        target.form_id = Some("first-document:1".into());
        target.frame_document_key = Some("first-document".into());
        let mut outer = observation(vec![target.clone()]);
        outer.document_key = "outer-document".into();
        let submitted = Submitted::of(Operation::Click, &target, &outer).unwrap();

        assert!(submitted.told(Some(&target), &outer).contains("the same form"));
        target.form_id = Some("first-document:2".into());
        assert!(submitted.told(Some(&target), &outer).contains("a different form"));

        target.form_id = Some("second-document:1".into());
        target.frame_document_key = Some("second-document".into());
        outer.controls = vec![target.clone()];
        let evidence = submitted.told(Some(&target), &outer);
        assert!(evidence.contains("unknown"), "{evidence}");
    }

    #[test]
    fn test_an_activation_only_role_takes_no_press() {
        assert!(!takes(&button(1), Operation::Press));
        let native_select = Control::new("native", "combobox", "Choose", [Operation::Select]);
        assert!(!takes(&native_select, Operation::Press));
        let unfocusable = Control::new("cell", "gridcell", "Cell", [Operation::Click]);
        assert!(!takes(&unfocusable, Operation::Press));
    }

    #[test]
    fn test_the_decision_sees_the_focused_control_and_no_unfocused_one() {
        assert_eq!(element(&field("reference", Some(true)))["focused"], json!(true));
        for unfocused in [None, Some(false)] {
            assert!(!element(&field("reference", unfocused)).contains_key("focused"));
        }
    }
}
