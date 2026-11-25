//! Saved steps from an earlier run of the task, taken first in a run's action queue. A step runs only while it is
//! certain: the same page, exactly one live control it names, not a password field, and its text known (written down,
//! this run's input, or a secret by name). Each goes through the same gates as an action the driver names. At the
//! first step that is not certain, or that is refused or fails, the rest are dropped and the driver is told where the
//! saved steps stopped, so it carries on from the page in front of it.

use fastbrowse_core::models::Operation;
use fastbrowse_core::page::{Action, Control};
use fastbrowse_core::recipe::{self, Recipe, RecipePage, RecipeStep, RecipeTarget, RecipeText};
use fastbrowse_core::safety::is_password;
use indexmap::IndexMap;

use crate::policy::{Decision, Given, takes};

pub(crate) struct Replay {
    steps: Vec<RecipeStep>,
    next: usize,
    /// The step handed out for the action now being carried out.
    pending: Option<usize>,
    /// Why the saved steps stopped, until the driver is told.
    stopped: Option<String>,
    pub replayed: usize,
    pub fell_back: usize,
}

impl Replay {
    pub fn new(recipe: Recipe) -> Replay {
        Replay { steps: recipe.steps, next: 0, pending: None, stopped: None, replayed: 0, fell_back: 0 }
    }

    /// The next saved step as an action on this page, or `None` once the steps are done or have stopped.
    pub fn ahead(&mut self, url: &str, offered: &[Control], inputs: &IndexMap<String, String>) -> Option<Decision> {
        self.pending = None;
        let step = self.steps.get(self.next)?;
        let made = certain(step, url, offered, inputs);
        match made {
            Ok(decision) => {
                self.pending = Some(self.next);
                Some(decision)
            }
            Err(why) => {
                self.stop(why);
                None
            }
        }
    }

    /// The step handed out was carried out, or was not.
    pub fn settled(&mut self, executed: bool) {
        let Some(index) = self.pending.take() else { return };
        if executed {
            self.next = index + 1;
            self.replayed += 1;
            if self.next == self.steps.len() {
                self.stopped = Some(format!(
                    "the {} steps saved from an earlier run of this task were all taken; carry on from the page below.",
                    self.replayed
                ));
            }
        } else {
            self.stop("it did not take effect on the page".into());
        }
    }

    /// The step handed out was refused by a gate, which says why.
    pub fn refused(&mut self, reason: &str) {
        if self.pending.take().is_some() {
            self.stop(format!("it was refused: {reason}"));
        }
    }

    /// What the driver is told once, when the saved steps stopped short of their end.
    pub fn take_notice(&mut self) -> Option<String> {
        self.stopped.take()
    }

    fn stop(&mut self, why: String) {
        let total = self.steps.len();
        let step = &self.steps[self.next];
        let what = match &step.target {
            Some(target) => format!("{} {:?}", step.operation, target.label),
            None => step.operation.to_string(),
        };
        self.stopped = Some(format!(
            "{} of {total} steps saved from an earlier run of this task were taken; step {} ({what}) was not, because \
             {why}. The rest are dropped: carry on from the page below.",
            self.next,
            self.next + 1
        ));
        self.fell_back = total - self.next;
        self.next = total;
    }
}

/// `step` as an action on this page, or why it cannot be taken without the driver.
fn certain(
    step: &RecipeStep,
    url: &str,
    offered: &[Control],
    inputs: &IndexMap<String, String>,
) -> Result<Decision, String> {
    if !step.replayable() {
        return Err("the earlier run's text for it was not saved".into());
    }
    if step.read_first {
        return Err("the earlier run read the page before it".into());
    }
    if !RecipePage::of(url).is_some_and(|page| step.page.matches(&page)) {
        return Err("this is not the page it was taken on".into());
    }
    let target = match &step.target {
        Some(saved) => {
            let control = recipe::find(saved, offered, url).ok_or("its control is not on this page exactly once")?;
            // A key is pressed in any control, as the driver pressed it: checked against what the page lists, a
            // recipe that pressed Enter in a link stopped there on the page it was saved on.
            if !matches!(step.operation, Operation::Press | Operation::Enter) && !takes(control, step.operation) {
                return Err("its control no longer takes that action".into());
            }
            Some(control.clone())
        }
        None => None,
    };
    let given = match step.operation {
        Operation::Fill | Operation::Press => {
            let field = target.as_ref().ok_or("it names no field")?;
            if is_password(field) {
                return Err("it types into a password field".into());
            }
            Some(match step.text.as_ref() {
                Some(RecipeText::Literal(_)) if field.sensitive => return Err("it types into a sensitive field".into()),
                Some(RecipeText::Literal(text)) => Given::Text(text.clone()),
                Some(RecipeText::Input(name)) => {
                    Given::Text(inputs.get(name).ok_or_else(|| format!("this run has no input {name:?}"))?.clone())
                }
                Some(RecipeText::Secret(name)) => Given::Secret(name.clone()),
                Some(RecipeText::Live) | None => return Err("the earlier run chose its text from the page".into()),
            })
        }
        _ => None,
    };
    Ok(Decision { replayed: true, ..super::driver::decision(step.operation, target, None, given) })
}

/// Whether a step of this operation is worth keeping in a recipe: one that acted on the site. Reads, waits and DONE
/// are decided live on replay, so evidence, timing and completion are never taken from an earlier run.
pub(crate) fn recorded(operation: Operation) -> bool {
    !matches!(operation, Operation::Read | Operation::Wait | Operation::Done)
}

/// The recipe step an executed action becomes. Text is kept only when it is safe to write down: a stored secret by
/// its name, a caller input by its name, and other text as typed unless it went into a sensitive field or holds a
/// resolved secret. A fill whose text cannot be kept is recorded in its place and left to the agent on replay.
pub(crate) fn step_of(
    page_url: &str,
    decision: &Decision,
    action: &Action,
    source: Option<RecipeText>,
    reveals: impl Fn(&str) -> bool,
) -> Option<RecipeStep> {
    if reveals(page_url) {
        return None;
    }
    let page = RecipePage::of(page_url)?;
    let target = decision.target.as_ref();
    let saved_target = target.map(|target| RecipeTarget::of(target, page_url));
    if saved_target.as_ref().is_some_and(|target| {
        [
            Some(target.role.as_str()),
            Some(target.label.as_str()),
            target.context.as_deref(),
            target.href.as_deref(),
            target.input_type.as_deref(),
            target.input_name.as_deref(),
            target.frame_origin.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(&reveals)
    }) {
        // A masked target cannot match on replay, so the agent must choose this step live instead.
        return None;
    }
    let mut replay = fastbrowse_core::recipe::REPLAYABLE.contains(&decision.operation);
    let text = match decision.operation {
        Operation::Fill => match source {
            Some(RecipeText::Live) => {
                replay = false;
                Some(RecipeText::Live)
            }
            Some(source @ (RecipeText::Input(_) | RecipeText::Secret(_))) => Some(source),
            _ => {
                let typed = action.text.as_deref().unwrap_or_default();
                let sensitive = target.is_some_and(|t| t.sensitive || fastbrowse_core::safety::is_password(t));
                if action.secret || sensitive || reveals(typed) {
                    replay = false;
                    None
                } else {
                    Some(RecipeText::Literal(typed.to_string()))
                }
            }
        },
        Operation::Press => {
            let typed = action.text.as_deref().unwrap_or_default();
            if reveals(typed) {
                replay = false;
                None
            } else {
                Some(RecipeText::Literal(typed.to_string()))
            }
        }
        _ => None,
    };
    Some(RecipeStep {
        operation: decision.operation,
        page,
        target: saved_target,
        text,
        option: None,
        read_first: false,
        replay,
    })
}

/// Remove steps that became sensitive after they were recorded. A secret can be learned later in a run, so
/// checking only when each action is appended leaves earlier literal text and page metadata exposed.
pub(crate) fn safe_steps(steps: Vec<RecipeStep>, reveals: impl Fn(&str) -> bool) -> Vec<RecipeStep> {
    steps
        .into_iter()
        .filter(|step| {
            let page_values = [
                step.page.origin.as_str(),
                step.page.path.as_str(),
                step.page.query.as_deref().unwrap_or_default(),
                step.page.fragment.as_deref().unwrap_or_default(),
            ];
            if page_values.into_iter().any(&reveals) {
                return false;
            }
            if step.target.as_ref().is_some_and(|target| {
                [
                    target.role.as_str(),
                    target.label.as_str(),
                    target.context.as_deref().unwrap_or_default(),
                    target.href.as_deref().unwrap_or_default(),
                    target.input_type.as_deref().unwrap_or_default(),
                    target.input_name.as_deref().unwrap_or_default(),
                    target.frame_origin.as_deref().unwrap_or_default(),
                ]
                .into_iter()
                .any(&reveals)
            }) {
                return false;
            }
            if step.option.as_deref().is_some_and(&reveals) {
                return false;
            }
            // An input's or a secret's name is the caller's text too: an input named with an address would be
            // written to the recipe with the credential in it (#1381).
            match step.text.as_ref() {
                Some(RecipeText::Literal(text) | RecipeText::Input(text) | RecipeText::Secret(text)) => !reveals(text),
                Some(RecipeText::Live) | None => true,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tests::code_decision;
    use fastbrowse_core::page::Control;

    const URL: &str = "https://shop.test/p/pico";

    fn button(id: &str, label: &str) -> Control {
        Control::new(id, "button", label, [Operation::Click])
    }

    #[test]
    fn page_derived_values_and_password_literals_are_not_replayed() {
        let ordinary = Control::new("name", "textbox", "Name", [Operation::Fill]);
        let (decision, action) = fill(ordinary, "a value read on an earlier page", false);
        let live = step_of(URL, &decision, &action, Some(RecipeText::Live), |_| false).unwrap();
        assert!(!live.replayable());
        let select =
            RecipeStep { operation: Operation::Select, option: Some("formerly cheapest".into()), ..live.clone() };
        assert!(!select.replayable());
        let password = Control {
            input_type: Some("password".into()),
            ..Control::new("p", "textbox", "Password", [Operation::Fill])
        };
        let (decision, action) = fill(password, "must not be recorded", false);
        let step = step_of(URL, &decision, &action, None, |_| false).unwrap();
        assert!(step.text.is_none());
        assert!(!step.replayable());
    }

    #[test]
    fn live_select_choices_are_not_saved_as_recipe_values() {
        let target = Control::new("shipping", "combobox", "Shipping", [Operation::Select]);
        let decision = code_decision(Operation::Select, Some(target.clone()));
        let action = Action {
            target_id: Some(target.id),
            text: Some("current cheapest".into()),
            ..Action::new(Operation::Select)
        };
        let step = step_of(URL, &decision, &action, None, |_| false).unwrap();
        assert!(step.option.is_none());
        assert!(!step.replayable());
    }

    fn fill(target: Control, text: &str, secret: bool) -> (Decision, Action) {
        let decision = code_decision(Operation::Fill, Some(target.clone()));
        let action =
            Action { target_id: Some(target.id), text: Some(text.into()), secret, ..Action::new(Operation::Fill) };
        (decision, action)
    }

    #[test]
    fn a_recipe_omits_steps_whose_page_or_target_reveals_a_secret() {
        let reveals = |text: &str| text.contains("hunter2");
        let action = Action::new(Operation::Click);
        let decision = code_decision(Operation::Click, Some(button("b", "Continue")));
        for url in [
            "https://hunter2.test/",
            "https://shop.test/hunter2",
            "https://shop.test/?value=hunter2",
            "https://shop.test/#hunter2",
        ] {
            assert!(step_of(url, &decision, &action, None, reveals).is_none(), "{url}");
        }
        for field in 0..7 {
            let mut target = button("b", "Continue");
            match field {
                0 => target.role = "hunter2".into(),
                1 => target.label = "Signed in as hunter2".into(),
                2 => target.context = Some("hunter2".into()),
                3 => target.href = Some("/next?value=hunter2".into()),
                4 => target.input_type = Some("hunter2".into()),
                5 => target.input_name = Some("hunter2".into()),
                _ => target.frame_origin = Some("https://hunter2.test".into()),
            }
            let decision = code_decision(Operation::Click, Some(target));
            assert!(step_of(URL, &decision, &action, None, reveals).is_none(), "field {field}");
        }
    }

    #[test]
    fn steps_recorded_before_a_secret_is_learned_are_filtered_at_save() {
        let action = Action::new(Operation::Click);
        let decision = code_decision(Operation::Click, Some(button("b", "Continue")));
        let step = step_of(URL, &decision, &action, None, |_| false).unwrap();
        let kept = safe_steps(vec![step.clone()], |_| false);
        assert_eq!(kept, vec![step.clone()]);
        let removed = safe_steps(vec![step], |text| text.contains("shop"));
        assert!(removed.is_empty());
    }

    /// An input's or a secret's name is the caller's text: one named with an address would be saved with its
    /// credential (#1381).
    #[test]
    fn a_recipe_drops_a_step_whose_input_or_secret_name_reveals_a_credential() {
        let action = Action::new(Operation::Click);
        let decision = code_decision(Operation::Click, Some(button("b", "Continue")));
        let step = step_of(URL, &decision, &action, None, |_| false).unwrap();
        let reveals = |text: &str| text.contains("n4me-pw");
        for text in [
            RecipeText::Input("https://ops:n4me-pw@h.test/".into()),
            RecipeText::Secret("https://ops:n4me-pw@h.test/".into()),
        ] {
            let named = RecipeStep { text: Some(text.clone()), ..step.clone() };
            assert!(safe_steps(vec![named], reveals).is_empty(), "{text:?}");
        }
        let plain = RecipeStep { text: Some(RecipeText::Input("email".into())), ..step };
        assert_eq!(safe_steps(vec![plain.clone()], reveals), vec![plain]);
    }

    #[test]
    fn a_recipe_keeps_secret_names_and_never_their_values() {
        let email = Control::new("e", "textbox", "Email", [Operation::Fill]);
        let password = Control { sensitive: true, ..Control::new("p", "textbox", "Password", [Operation::Fill]) };
        let never = |_: &str| false;

        let (decision, action) = fill(email.clone(), "ada@example.com", false);
        let typed = step_of(URL, &decision, &action, None, never).unwrap();
        assert_eq!(typed.text, Some(RecipeText::Literal("ada@example.com".into())));

        let (decision, action) = fill(password.clone(), "hunter2", true);
        let named = step_of(URL, &decision, &action, Some(RecipeText::Secret("password".into())), never).unwrap();
        assert_eq!((named.text, named.replay), (Some(RecipeText::Secret("password".into())), true));

        // A sensitive field typed from anything but a named secret, or text holding a resolved secret, is not kept.
        let (decision, action) = fill(password, "hunter2", false);
        let unnamed = step_of(URL, &decision, &action, None, never).unwrap();
        assert_eq!((unnamed.text, unnamed.replay), (None, false));
        let (decision, action) = fill(email, "my key is hunter2", false);
        let revealing = step_of(URL, &decision, &action, None, |text| text.contains("hunter2")).unwrap();
        assert_eq!((&revealing.text, revealing.replay), (&None, false));
        assert!(!serde_json::to_string(&revealing).unwrap().contains("hunter2"));
    }
}
