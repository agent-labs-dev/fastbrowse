//! Replaying a recipe: each observation is offered the next recipe steps, and the first whose page and control are
//! here is dispatched as a code decision, through the same gates as any other. A page the recipe does not describe is
//! decided by the agent, as a run without a recipe would be.

use fastbrowse_core::models::Operation;
use fastbrowse_core::page::{Action, Control};
use fastbrowse_core::recipe::{self, Recipe, RecipePage, RecipeStep, RecipeTarget, RecipeText};

use super::helpers::code_decision;
use crate::policy::Decision;

/// How many recipe steps ahead a page may match, so a recorded step this run no longer needs (a cookie banner that
/// is not shown, a step that cannot be replayed) does not end the replay.
const LOOKAHEAD: usize = 3;
/// Agent decisions in a row, with no recipe step matching, before the rest of the run is left to the agent.
const MISSES: u32 = 4;

pub(crate) struct Replay {
    steps: Vec<RecipeStep>,
    next: usize,
    /// The step handed out for the decision now being carried out.
    pending: Option<usize>,
    misses: u32,
    /// Whether the next step has already been looked for and was not on the page. Only then may a later step be
    /// replayed past it: a page still loading lacks the next step's control too, and skipping it there left an
    /// "Add to cart" out of the order.
    looked: bool,
    abandoned: bool,
    pub replayed: usize,
    pub fell_back: usize,
}

impl Replay {
    pub fn new(recipe: Recipe) -> Replay {
        Replay {
            steps: recipe.steps,
            next: 0,
            pending: None,
            misses: 0,
            looked: false,
            abandoned: false,
            replayed: 0,
            fell_back: 0,
        }
    }

    pub fn active(&self) -> bool {
        !self.abandoned && self.next < self.steps.len()
    }

    /// The decision for the first of the next recipe steps this page offers, or None for the agent to decide.
    pub fn decision(&mut self, url: &str, offered: &[Control]) -> Option<Decision> {
        if self.pending.take().is_some() {
            // The step handed out last was not carried out: a gate stopped it, or another decision replaced it.
            self.miss();
        }
        if !self.active() {
            return None;
        }
        let Some(page) = RecipePage::of(url) else {
            self.fell_back += 1;
            self.miss();
            return None;
        };
        let end = if self.looked || !self.steps[self.next].replayable() {
            (self.next + LOOKAHEAD).min(self.steps.len())
        } else {
            self.next + 1
        };
        for index in self.next..end {
            let step = &self.steps[index];
            if !step.replayable() || !step.page.matches(&page) {
                continue;
            }
            let target = match &step.target {
                Some(target) => match recipe::find(target, offered, url) {
                    Some(control) if control.operations.contains(&step.operation) => Some(control.clone()),
                    Some(_) => continue,
                    None => continue,
                },
                None => None,
            };
            self.pending = Some(index);
            let mut decision = code_decision(step.operation, target);
            decision.replayed = true;
            if step.read_first {
                decision.shows_answer = Some(1.0);
            }
            return Some(decision);
        }
        self.looked = true;
        self.fell_back += 1;
        self.miss();
        None
    }

    /// An action the agent decided was carried out. When it is one of the next recipe steps, the replay goes on
    /// after it, so the step is not taken twice.
    pub fn followed(&mut self, url: &str, decision: &Decision) {
        let Some(page) = RecipePage::of(url) else { return };
        if !self.active() {
            return;
        }
        let end = (self.next + LOOKAHEAD).min(self.steps.len());
        let took = (self.next..end).find(|&index| {
            let step = &self.steps[index];
            step.operation == decision.operation
                && step.page.matches(&page)
                && match (&step.target, &decision.target) {
                    (Some(recorded), Some(control)) => recorded.matches(control, url),
                    (None, None) => true,
                    _ => false,
                }
        });
        if let Some(index) = took {
            self.advance(index);
        }
    }

    /// The text a replayed fill types, from the step being carried out.
    pub fn text(&self) -> Option<&RecipeText> {
        self.pending.and_then(|index| self.steps[index].text.as_ref())
    }

    /// The option a replayed select chooses, from the step being carried out.
    pub fn option(&self) -> Option<&str> {
        self.pending.and_then(|index| self.steps[index].option.as_deref())
    }

    /// The decision handed out was carried out, or was not.
    pub fn settled(&mut self, executed: bool) {
        let Some(index) = self.pending.take() else { return };
        if executed {
            self.advance(index);
            self.replayed += 1;
        } else {
            self.miss();
        }
    }

    /// The decision handed out was replaced before it was carried out.
    pub fn replaced(&mut self) {
        if self.pending.take().is_some() {
            self.miss();
        }
    }

    fn advance(&mut self, index: usize) {
        self.next = index + 1;
        self.misses = 0;
        self.looked = false;
    }

    fn miss(&mut self) {
        self.misses += 1;
        if self.misses >= MISSES {
            self.abandoned = true;
        }
    }
}

/// Whether a step of this operation is worth keeping in a recipe: one that acted on the site. Reads and DONE are
/// decided live on replay, so evidence and completion are never taken from an earlier run.
pub(crate) fn recorded(operation: Operation) -> bool {
    !matches!(operation, Operation::Read | Operation::Done)
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
    let page = RecipePage::of(page_url)?;
    let target = decision.target.as_ref();
    let mut replay = true;
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
        _ => None,
    };
    let option = (decision.operation == Operation::Select).then(|| action.text.clone()).flatten();
    if option.as_deref().is_some_and(&reveals) {
        return None;
    }
    Some(RecipeStep {
        operation: decision.operation,
        page,
        target: target.map(|t| RecipeTarget::of(t, page_url)),
        text,
        option,
        read_first: decision.shows_answer.is_some_and(|probability| probability > fastbrowse_core::decision::EVEN_ODDS),
        replay,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastbrowse_core::recipe::RecipeTarget;

    const URL: &str = "https://shop.test/p/pico";

    fn button(id: &str, label: &str) -> Control {
        Control::new(id, "button", label, [Operation::Click])
    }

    fn click(label: &str, path: &str) -> RecipeStep {
        RecipeStep {
            operation: Operation::Click,
            page: RecipePage::of(&format!("https://shop.test{path}")).unwrap(),
            target: Some(RecipeTarget::of(&button("old:1", label), URL)),
            text: None,
            option: None,
            read_first: false,
            replay: true,
        }
    }

    fn replay(steps: Vec<RecipeStep>) -> Replay {
        Replay::new(Recipe { version: 1, task: "t".into(), start: None, steps })
    }

    #[test]
    fn a_matching_step_is_a_replayed_decision_on_the_live_control() {
        let mut replay = replay(vec![click("Add to cart", "/p/pico")]);
        let page = [button("new:4", "Add to cart")];
        let decision = replay.decision(URL, &page).unwrap();
        assert!(decision.replayed);
        assert_eq!(decision.target.unwrap().id, "new:4");
        replay.settled(true);
        assert_eq!((replay.replayed, replay.active()), (1, false));
    }

    #[test]
    fn a_step_not_on_the_page_is_skipped_only_once_it_was_looked_for() {
        let mut replay = replay(vec![click("Accept cookies", "/p/pico"), click("Add to cart", "/p/pico")]);
        let page = [button("n:1", "Add to cart")];
        // The page may still be loading the next step's control, so the agent decides this one.
        assert!(replay.decision(URL, &page).is_none());
        let decision = replay.decision(URL, &page).unwrap();
        assert_eq!(decision.target.unwrap().label, "Add to cart");
        replay.settled(true);
        assert!(!replay.active());
    }

    #[test]
    fn a_recipe_step_the_agent_took_itself_is_not_replayed_again() {
        let mut replay = replay(vec![click("Add to cart", "/p/pico"), click("Cart", "/p/pico")]);
        let add = button("n:1", "Add to cart");
        replay.followed(URL, &code_decision(Operation::Click, Some(add.clone())));
        let decision = replay.decision(URL, &[add, button("n:2", "Cart")]).unwrap();
        assert_eq!(decision.target.unwrap().label, "Cart");
    }

    #[test]
    fn another_page_or_a_missing_control_falls_back_and_enough_misses_abandon() {
        let mut replay = replay(vec![click("Add to cart", "/p/pico")]);
        assert!(replay.decision("https://shop.test/p/other", &[button("n:1", "Add to cart")]).is_none());
        for _ in 1..MISSES {
            assert!(replay.decision(URL, &[button("n:1", "Checkout")]).is_none());
        }
        assert!(!replay.active());
        assert!(replay.decision(URL, &[button("n:1", "Add to cart")]).is_none());
        assert_eq!(replay.fell_back, MISSES as usize);
    }

    #[test]
    fn a_step_not_carried_out_is_offered_again() {
        let mut replay = replay(vec![click("Add to cart", "/p/pico")]);
        let page = [button("n:1", "Add to cart")];
        assert!(replay.decision(URL, &page).is_some());
        replay.settled(false);
        assert!(replay.decision(URL, &page).is_some());
        // Offered again is not a step the agent decided.
        assert_eq!(replay.fell_back, 0);
    }

    #[test]
    fn an_operation_the_live_control_does_not_offer_is_left_to_jev() {
        let mut replay = replay(vec![click("Add to cart", "/p/pico")]);
        let control = Control::new("new:4", "button", "Add to cart", [Operation::Hover]);
        assert!(replay.decision(URL, &[control]).is_none());
    }

    #[test]
    fn page_derived_values_and_password_literals_are_not_replayed() {
        let ordinary = Control::new("name", "textbox", "Name", [Operation::Fill]);
        let (decision, action) = fill(ordinary, "a value read on an earlier page", false);
        let live = step_of(URL, &decision, &action, Some(RecipeText::Live), |_| false).unwrap();
        assert!(!live.replayable());
        let password = Control {
            input_type: Some("password".into()),
            ..Control::new("p", "textbox", "Password", [Operation::Fill])
        };
        let (decision, action) = fill(password, "must not be recorded", false);
        let step = step_of(URL, &decision, &action, None, |_| false).unwrap();
        assert!(step.text.is_none());
        assert!(!step.replayable());
    }

    fn fill(target: Control, text: &str, secret: bool) -> (Decision, Action) {
        let decision = code_decision(Operation::Fill, Some(target.clone()));
        let action =
            Action { target_id: Some(target.id), text: Some(text.into()), secret, ..Action::new(Operation::Fill) };
        (decision, action)
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
