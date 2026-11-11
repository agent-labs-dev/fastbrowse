//! Match saved steps to live controls. When the next saved step's page and control are certain (the same address,
//! exactly one live control it names, no read the original run made first), Jev is offered that control alone and
//! no screenshot. Any other step is Jev's to choose, and a choice that agrees with a saved step may reuse its value.
//! Missing or ambiguous controls leave the run to its ordinary decisions.

use fastbrowse_core::models::Operation;
use fastbrowse_core::page::{Action, Control};
use fastbrowse_core::recipe::{self, Recipe, RecipePage, RecipeStep, RecipeTarget, RecipeText};
use fastbrowse_core::safety::is_password;

use crate::policy::{Decision, takes};

/// A live Jev decision may match a later saved step when the current page needs a different order.
pub(crate) struct Replay {
    steps: Vec<RecipeStep>,
    next: usize,
    /// The step handed out for the decision now being carried out.
    pending: Option<usize>,
    /// Saved steps offered ahead that Jev declined or that failed, so Jev decides them from then on.
    failed: Vec<usize>,
    pub replayed: usize,
    pub fell_back: usize,
}

impl Replay {
    pub fn new(recipe: Recipe) -> Replay {
        Replay { steps: recipe.steps, next: 0, pending: None, failed: Vec::new(), replayed: 0, fell_back: 0 }
    }

    pub fn active(&self) -> bool {
        self.next < self.steps.len()
    }

    /// The next saved step as the decision to offer Jev alone, when nothing about its target is left to choose: it
    /// is replayable, it is on the page it was saved on, the original run read nothing first, exactly one live
    /// control is the one it names, and a fill has a saved value this field may take. Jev still judges the page and
    /// may take another action; acting still checks authorization, freshness and secrets.
    pub fn ahead(&mut self, url: &str, offered: &[Control], missing: impl Fn(&Control) -> bool) -> Option<Decision> {
        self.pending.take();
        let index = self.next;
        let step = self.steps.get(index)?;
        if self.failed.contains(&index) || !step.replayable() || step.read_first {
            return None;
        }
        if !step.page.matches(&RecipePage::of(url)?) {
            return None;
        }
        let target = match &step.target {
            Some(target) => {
                let control = recipe::find(target, offered, url)?;
                if !takes(control, step.operation) {
                    return None;
                }
                Some(control.clone())
            }
            None if step.operation.is_targeted() => return None,
            None => None,
        };
        self.pending = Some(index);
        if matches!(step.operation, Operation::Fill | Operation::Press)
            && target.as_ref().and_then(|target| self.text_for(target, missing(target))).is_none()
        {
            self.pending.take();
            return None;
        }
        Some(Decision {
            operation: step.operation,
            url: None,
            target_confidence: target.as_ref().map(|_| 1.0),
            target,
            tab_id: None,
            operation_confidence: 1.0,
            login_required: None,
            offered_controls: offered.len() as i64,
            reduction: crate::policy::Reduction::None,
            cost: Vec::new(),
            input_tokens: 0,
            bot_check: None,
            shows_answer: None,
            site_failure: None,
            unobtainable: None,
            destination: None,
            replayed: true,
            by_code: false,
            offered: Vec::new(),
            plan: Vec::new(),
            ahead: Default::default(),
        })
    }

    /// Reads and completion stay live without consuming a saved browser action.
    pub fn confirm(&mut self, url: &str, offered: &[Control], decision: &mut Decision, missing: bool) {
        if !recorded(decision.operation) {
            return;
        }
        if self.matching_step(url, offered, decision).is_some() {
            if matches!(decision.operation, Operation::Fill | Operation::Press)
                && decision.target.as_ref().and_then(|target| self.text_for(target, missing)).is_none()
            {
                self.pending.take();
                self.fell_back += 1;
                return;
            }
            decision.replayed = true;
        }
    }

    /// Match the live choice against remaining saved steps, even when earlier controls are still offered.
    fn matching_step(&mut self, url: &str, offered: &[Control], chosen: &Decision) -> Option<usize> {
        self.pending.take();
        if !self.active() {
            return None;
        }
        let Some(page) = RecipePage::of(url) else {
            self.fell_back += 1;
            return None;
        };
        for index in self.next..self.steps.len() {
            let step = &self.steps[index];
            if !step.replayable() || !step.page.matches(&page) || step.operation != chosen.operation {
                continue;
            }
            let target = match &step.target {
                Some(target) => match recipe::find(target, offered, url) {
                    Some(control) if takes(control, step.operation) => Some(control),
                    Some(_) => continue,
                    None => continue,
                },
                None => None,
            };
            if target.as_ref().map(|control| &control.id) != chosen.target.as_ref().map(|control| &control.id) {
                continue;
            }
            self.pending = Some(index);
            return Some(index);
        }
        self.fell_back += 1;
        None
    }

    /// An action the agent decided was carried out. When it is one of the next recipe steps, the replay goes on
    /// after it, so the step is not taken twice.
    pub fn followed(&mut self, url: &str, decision: &Decision) {
        let Some(page) = RecipePage::of(url) else { return };
        if !self.active() {
            return;
        }
        let took = (self.next..self.steps.len()).find(|&index| {
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

    pub fn text_for(&self, target: &Control, missing: bool) -> Option<&RecipeText> {
        if missing || is_password(target) {
            return None;
        }
        match self.text()? {
            RecipeText::Literal(_) if target.sensitive => None,
            RecipeText::Live => None,
            source => Some(source),
        }
    }

    /// The decision handed out was carried out, or was not.
    pub fn settled(&mut self, executed: bool) {
        let Some(index) = self.pending.take() else { return };
        if executed {
            self.advance(index);
            self.replayed += 1;
        } else {
            self.failed.push(index);
        }
    }

    /// Jev did not take the step offered ahead: it is Jev's to decide among the whole page from then on.
    pub fn declined(&mut self) {
        if let Some(index) = self.pending.take() {
            self.failed.push(index);
        }
    }

    /// The decision handed out was replaced before it was carried out.
    pub fn replaced(&mut self) {
        self.pending.take();
    }

    fn advance(&mut self, index: usize) {
        self.next = index + 1;
    }
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
        read_first: decision.shows_answer.is_some_and(|probability| probability > fastbrowse_core::decision::EVEN_ODDS),
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
            match step.text.as_ref() {
                Some(RecipeText::Literal(value)) => !reveals(value),
                _ => true,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tests::code_decision;
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
    fn the_next_saved_step_is_offered_alone_on_its_page() {
        let mut replay = replay(vec![click("Add to cart", "/p/pico"), click("Cart", "/p/pico")]);
        let page = [button("n:1", "Cart"), button("n:2", "Add to cart")];
        let decision = replay.ahead(URL, &page, |_| false).unwrap();
        assert!(decision.replayed);
        assert_eq!(decision.target.as_ref().map(|target| target.id.as_str()), Some("n:2"));
        replay.settled(true);
        let decision = replay.ahead(URL, &page, |_| false).unwrap();
        assert_eq!(decision.target.as_ref().map(|target| target.id.as_str()), Some("n:1"));
    }

    #[test]
    fn a_step_is_not_offered_ahead_where_its_page_or_control_is_uncertain() {
        let saved = click("Add to cart", "/p/pico");
        let twins = [button("n:1", "Add to cart"), button("n:2", "Add to cart")];
        assert!(replay(vec![saved.clone()]).ahead(URL, &twins, |_| false).is_none());
        let page = [button("n:1", "Add to cart")];
        assert!(replay(vec![saved.clone()]).ahead("https://shop.test/p/other", &page, |_| false).is_none());
        let read_first = RecipeStep { read_first: true, ..saved.clone() };
        assert!(replay(vec![read_first]).ahead(URL, &page, |_| false).is_none());
        assert!(replay(vec![saved]).ahead(URL, &page, |_| false).is_some());
    }

    #[test]
    fn a_fill_is_offered_ahead_only_with_a_saved_value_the_field_may_take() {
        let email = Control::new("email", "textbox", "Email", [Operation::Fill]);
        let (decision, action) = fill(email.clone(), "ada@example.test", false);
        let saved = step_of(URL, &decision, &action, None, |_| false).unwrap();
        let mut ahead = replay(vec![saved.clone()]);
        assert!(ahead.ahead(URL, std::slice::from_ref(&email), |_| false).is_some());
        assert_eq!(ahead.text(), Some(&RecipeText::Literal("ada@example.test".into())));
        assert!(replay(vec![saved.clone()]).ahead(URL, std::slice::from_ref(&email), |_| true).is_none());
        let live = RecipeStep { text: Some(RecipeText::Live), ..saved };
        assert!(replay(vec![live]).ahead(URL, &[email], |_| false).is_none());
    }

    #[test]
    fn a_saved_key_press_replays_on_a_field_that_takes_a_fill() {
        let reply = Control::new("reply", "textbox", "Reply", [Operation::Fill]);
        let saved = RecipeStep {
            operation: Operation::Press,
            page: RecipePage::of(URL).unwrap(),
            target: Some(RecipeTarget::of(&reply, URL)),
            text: Some(RecipeText::Literal("Control+End".into())),
            option: None,
            read_first: false,
            replay: true,
        };
        let mut ahead = replay(vec![saved.clone()]);
        assert_eq!(
            ahead.ahead(URL, std::slice::from_ref(&reply), |_| false).map(|d| d.operation),
            Some(Operation::Press)
        );
        let mut live = replay(vec![saved]);
        let mut press = code_decision(Operation::Press, Some(reply.clone()));
        live.confirm(URL, &[reply], &mut press, false);
        assert!(press.replayed);
    }

    #[test]
    fn a_step_jev_declined_or_that_failed_is_left_to_jev() {
        let page = [button("n:1", "Add to cart")];
        let mut declined = replay(vec![click("Add to cart", "/p/pico")]);
        declined.ahead(URL, &page, |_| false).unwrap();
        declined.declined();
        assert!(declined.ahead(URL, &page, |_| false).is_none());
        let mut failed = replay(vec![click("Add to cart", "/p/pico")]);
        failed.ahead(URL, &page, |_| false).unwrap();
        failed.settled(false);
        assert!(failed.ahead(URL, &page, |_| false).is_none());
        assert!(failed.active());
    }

    #[test]
    fn live_reads_do_not_consume_the_next_saved_action() {
        let mut replay = replay(vec![click("Continue", "/p/pico")]);
        let page = [button("new:4", "Continue")];
        for _ in 0..8 {
            let mut read = code_decision(Operation::Read, None);
            replay.confirm(URL, &page, &mut read, false);
            assert!(!read.replayed);
        }
        let mut click = code_decision(Operation::Click, Some(page[0].clone()));
        replay.confirm(URL, &page, &mut click, false);
        assert!(click.replayed);
        replay.settled(true);
        assert_eq!(replay.replayed, 1);
        assert!(!replay.active());
    }

    #[test]
    fn a_saved_literal_is_not_reused_when_the_live_field_becomes_sensitive() {
        let mut email = Control::new("email", "textbox", "Email", [Operation::Fill]);
        let (decision, action) = fill(email.clone(), "ada@example.test", false);
        let saved = step_of(URL, &decision, &action, None, |_| false).unwrap();
        email.sensitive = true;
        let (mut decision, _) = fill(email.clone(), "", false);
        let mut replay = replay(vec![saved]);
        replay.confirm(URL, &[email], &mut decision, false);
        assert!(!decision.replayed);
        assert!(replay.text().is_none());
        replay.settled(true);
        assert_eq!(replay.replayed, 0);
    }

    #[test]
    fn a_missing_field_uses_a_live_value_without_counting_reuse() {
        let email = Control::new("email", "textbox", "Email", [Operation::Fill]);
        let (mut decision, action) = fill(email.clone(), "ada@example.test", false);
        let saved = step_of(URL, &decision, &action, None, |_| false).unwrap();
        let mut replay = replay(vec![saved]);
        replay.confirm(URL, &[email], &mut decision, true);
        assert!(!decision.replayed);
        assert!(replay.text().is_none());
        replay.followed(URL, &decision);
        assert!(!replay.active());
        assert_eq!((replay.replayed, replay.fell_back), (0, 1));
    }

    #[test]
    fn a_live_fill_can_reuse_a_later_value_after_an_unneeded_scroll() {
        let email = Control::new("email", "textbox", "Email", [Operation::Fill]);
        let (mut decision, action) = fill(email.clone(), "ada@example.test", false);
        let saved = step_of(URL, &decision, &action, None, |_| false).unwrap();
        let scroll = RecipeStep { operation: Operation::Scroll, target: None, text: None, ..saved.clone() };
        let mut replay = replay(vec![scroll, saved]);
        replay.confirm(URL, &[email], &mut decision, false);
        assert!(decision.replayed);
        assert_eq!(replay.text(), Some(&RecipeText::Literal("ada@example.test".into())));
        replay.settled(true);
        assert!(!replay.active());
    }

    #[test]
    fn a_live_fill_matches_its_value_when_an_earlier_field_is_also_offered() {
        let name = Control::new("name", "textbox", "Name", [Operation::Fill]);
        let email = Control::new("email", "textbox", "Email", [Operation::Fill]);
        let (name_decision, name_action) = fill(name.clone(), "Ada", false);
        let (mut decision, email_action) = fill(email.clone(), "ada@example.test", false);
        let mut replay = replay(vec![
            step_of(URL, &name_decision, &name_action, None, |_| false).unwrap(),
            step_of(URL, &decision, &email_action, None, |_| false).unwrap(),
        ]);
        replay.confirm(URL, &[name, email], &mut decision, false);
        assert!(decision.replayed);
        assert_eq!(replay.text(), Some(&RecipeText::Literal("ada@example.test".into())));
    }

    #[test]
    fn a_matching_step_is_a_replayed_decision_on_the_live_control() {
        let mut replay = replay(vec![click("Add to cart", "/p/pico")]);
        let page = [button("new:4", "Add to cart")];
        let decision =
            replay.matching_step(URL, &page, &code_decision(Operation::Click, Some(page[0].clone()))).unwrap();
        assert_eq!(decision, 0);
        replay.settled(true);
        assert_eq!((replay.replayed, replay.active()), (1, false));
    }

    #[test]
    fn a_missing_step_does_not_block_a_later_replayable_step() {
        let mut steps: Vec<_> = (0..8).map(|index| click(&format!("Absent control {index}"), "/p/pico")).collect();
        steps.push(click("Add to cart", "/p/pico"));
        let mut replay = replay(steps);
        let page = [button("n:1", "Add to cart")];
        let decision =
            replay.matching_step(URL, &page, &code_decision(Operation::Click, Some(page[0].clone()))).unwrap();
        assert_eq!(decision, 8);
        replay.settled(true);
        assert!(!replay.active());
    }

    #[test]
    fn a_recipe_step_the_agent_took_itself_is_not_replayed_again() {
        let mut replay = replay(vec![click("Add to cart", "/p/pico"), click("Cart", "/p/pico")]);
        let add = button("n:1", "Add to cart");
        replay.followed(URL, &code_decision(Operation::Click, Some(add.clone())));
        let decision = replay
            .matching_step(
                URL,
                &[add, button("n:2", "Cart")],
                &code_decision(Operation::Click, Some(button("n:2", "Cart"))),
            )
            .unwrap();
        assert_eq!(decision, 1);
    }

    #[test]
    fn missing_pages_do_not_abandon_replay() {
        let mut replay = replay(vec![click("Add to cart", "/p/pico")]);
        assert!(
            replay
                .matching_step(
                    "https://shop.test/p/other",
                    &[button("n:1", "Add to cart")],
                    &code_decision(Operation::Click, Some(button("n:1", "Add to cart")))
                )
                .is_none()
        );
        for _ in 1..8 {
            assert!(
                replay
                    .matching_step(
                        URL,
                        &[button("n:1", "Checkout")],
                        &code_decision(Operation::Click, Some(button("n:1", "Checkout")))
                    )
                    .is_none()
            );
        }
        assert!(replay.active());
        assert!(
            replay
                .matching_step(
                    URL,
                    &[button("n:1", "Add to cart")],
                    &code_decision(Operation::Click, Some(button("n:1", "Add to cart")))
                )
                .is_some()
        );
        assert_eq!(replay.fell_back, 8);
    }

    #[test]
    fn a_step_not_carried_out_is_offered_again() {
        let mut replay = replay(vec![click("Add to cart", "/p/pico")]);
        let page = [button("n:1", "Add to cart")];
        assert!(replay.matching_step(URL, &page, &code_decision(Operation::Click, Some(page[0].clone()))).is_some());
        replay.settled(false);
        assert!(replay.matching_step(URL, &page, &code_decision(Operation::Click, Some(page[0].clone()))).is_some());
        // Offered again is not a step the agent decided.
        assert_eq!(replay.fell_back, 0);
    }

    #[test]
    fn an_operation_the_live_control_does_not_offer_is_left_to_jev() {
        let mut replay = replay(vec![click("Add to cart", "/p/pico")]);
        let control = Control::new("new:4", "button", "Add to cart", [Operation::Hover]);
        assert!(
            replay
                .matching_step(
                    URL,
                    std::slice::from_ref(&control),
                    &code_decision(Operation::Click, Some(control.clone()))
                )
                .is_none()
        );
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
