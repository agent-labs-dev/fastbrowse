//! Port of `src/fastbrowse/effects.py`: what an action visibly did, told back to the policy so that a no-op cannot
//! pass for progress.

use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::page::{BlockKind, Capture, Control, Observation};
use crate::py::{json::dumps, sha256_hex, text};

/// Clicking one of these is choosing a value, so it should leave some value, selection or address changed.
pub const SETTING_ROLES: [&str; 6] = ["option", "checkbox", "radio", "switch", "menuitemradio", "menuitemcheckbox"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    pub summary: String,
    /// The address, or a surviving control's label, value, checked or selected state, changed.
    pub set_something: bool,
}

/// Identify a page state by what can be done on it, ignoring text that changes on its own (clocks, ads).
pub fn state_key(observation: &Observation) -> String {
    let mut controls: Vec<String> = observation
        .controls
        .iter()
        .map(|c| dumps(&json!([c.role, c.label, c.context, c.value, c.checked, c.selected, c.expanded])))
        .collect();
    controls.sort();
    sha256_hex(dumps(&json!([observation.url, controls])).as_bytes())
}

/// Identify which step or content state a capture shows: its page state and the shape of its text.
///
/// A wizard's Review and Submit steps can share an address and their Back and Next buttons, so `state_key` takes one
/// for the other. Each block's kind and the headings over it tell them apart, while a block's own words are left
/// out: a countdown or a corrected name changes them on every capture of the same step.
pub fn step_key(observation: &Observation, capture: &Capture) -> String {
    let shape: Vec<Value> = capture
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Observation)
        .map(|block| json!([block.kind, block.frame_id, block.heading_path]))
        .collect();
    let headings: Vec<&str> = capture
        .blocks
        .iter()
        .filter(|block| block.kind == BlockKind::Heading)
        .map(|block| capture.block_text(block))
        .collect();
    sha256_hex(dumps(&json!([state_key(observation), shape, headings])).as_bytes())
}

/// A control attribute as `str()` shows it.
#[derive(Debug, Clone, PartialEq)]
enum Shown {
    Missing,
    Text(String),
    Flag(bool),
}

impl Shown {
    fn of_text(value: &Option<String>) -> Shown {
        value.clone().map_or(Shown::Missing, Shown::Text)
    }
    fn of_flag(value: Option<bool>) -> Shown {
        value.map_or(Shown::Missing, Shown::Flag)
    }
    fn str(&self) -> String {
        match self {
            Shown::Missing => "None".into(),
            Shown::Text(text) => text.clone(),
            Shown::Flag(true) => "True".into(),
            Shown::Flag(false) => "False".into(),
        }
    }
}

fn short(value: &Shown) -> String {
    let squashed = if *value == Shown::Missing { String::new() } else { text::squash(&value.str()) };
    if squashed.is_empty() { "empty".to_string() } else { squashed }
}

fn listed(controls: &[&Control]) -> String {
    let count = format!("{} control{}", controls.len(), if controls.len() == 1 { "" } else { "s" });
    let names: Vec<String> = controls.iter().map(|c| short(&Shown::Text(c.label.clone()))).collect();
    format!("{count}: {}", names.join(", "))
}

fn plain(text: &str) -> String {
    text::casefold(&text::squash(text))
}

/// Whether a field now shows `choice`, whole or as the start of it ("London" for "London, United Kingdom").
fn shows(control: &Control, choice: &str) -> bool {
    let mut shown = Vec::new();
    if let Some(value) = control.value.as_deref().filter(|v| !v.is_empty()) {
        shown.push(plain(value));
    }
    if control.role == "combobox" {
        shown.push(plain(&control.label));
    }
    shown.iter().filter(|t| !t.is_empty()).any(|t| choice.contains(t.as_str()) || choice.starts_with(t.as_str()))
}

/// What changed from `before` to `after`. With `chosen`, the option clicked in between, a field showing it counts as
/// a value set: an autocomplete choice can leave the text that was typed exactly as it was.
pub fn effect(before: &Observation, after: &Observation, chosen: Option<&Control>) -> Effect {
    let old: IndexMap<&str, &Control> = before.controls.iter().map(|c| (c.id.as_str(), c)).collect();
    let new: IndexMap<&str, &Control> = after.controls.iter().map(|c| (c.id.as_str(), c)).collect();
    let mut parts = Vec::new();
    let navigated = before.url != after.url;
    if navigated {
        parts.push(format!("went to {}", short(&Shown::Text(after.url.clone()))));
    }
    if let Some(status) = after.response_status.filter(|status| *status >= 400) {
        parts.push(format!("HTTP {status}: destination did not load successfully"));
    }
    let mut changes = Vec::new();
    let mut setting = false;
    // A picker can open as an overlay with its own copy of the field, so the field the choice fills is a different
    // element before and after. A control unmatched by id is compared with the one earlier control sharing its role
    // and label, if exactly one did.
    let mut by_name: IndexMap<(&str, &str), Vec<&Control>> = IndexMap::new();
    for control in &before.controls {
        by_name.entry((control.role.as_str(), text::strip(&control.label))).or_default().push(control);
    }
    for (key, now) in &new {
        let was = match old.get(key) {
            Some(was) => *was,
            None => match by_name.get(&(now.role.as_str(), text::strip(&now.label))).map(Vec::as_slice) {
                Some([namesake]) if !new.contains_key(namesake.id.as_str()) => *namesake,
                _ => continue,
            },
        };
        let attributes = [
            ("label", Shown::Text(was.label.clone()), Shown::Text(now.label.clone())),
            ("value", Shown::of_text(&was.value), Shown::of_text(&now.value)),
            ("checked", Shown::of_flag(was.checked), Shown::of_flag(now.checked)),
            ("selected", Shown::of_flag(was.selected), Shown::of_flag(now.selected)),
            ("expanded", Shown::of_flag(was.expanded), Shown::of_flag(now.expanded)),
        ];
        for (name, a, b) in attributes {
            let differs = if name == "label" { plain(&was.label) != plain(&now.label) } else { a != b };
            if differs {
                changes.push(format!(
                    "{} {name}: {} -> {}",
                    short(&Shown::Text(was.label.clone())),
                    short(&a),
                    short(&b)
                ));
                // A menu opening or closing is not a value set.
                setting = setting || name != "expanded";
            }
        }
    }
    if !changes.is_empty() {
        parts.push(format!("changed {}", changes.join("; ")));
    }
    let shown: Vec<&Control> = new.iter().filter(|(key, _)| !old.contains_key(*key)).map(|(_, c)| *c).collect();
    let hidden: Vec<&Control> = old.iter().filter(|(key, _)| !new.contains_key(*key)).map(|(_, c)| *c).collect();
    if !shown.is_empty() {
        parts.push(format!("showed {}", listed(&shown)));
    }
    if !hidden.is_empty() {
        parts.push(format!("removed {}", listed(&hidden)));
    }
    if let Some(chosen) = chosen {
        let choice = plain(&chosen.label);
        setting =
            setting || after.controls.iter().any(|c| !SETTING_ROLES.contains(&c.role.as_str()) && shows(c, &choice));
    }
    // A form that refuses a submit can say why in its text alone, as Sauce Demo's checkout shows "Error: First Name is
    // required" and changes no control.
    let summary = if !parts.is_empty() {
        parts.join("; ")
    } else if before.viewport_text != after.viewport_text {
        "only the page text changed".to_string()
    } else {
        "nothing visible changed".to_string()
    };
    Effect { summary, set_something: navigated || setting }
}

#[cfg(test)]
mod tests {
    //! tests/test_effects.py.
    use super::*;
    use crate::models::Operation;
    use crate::py::datetime::DateTime;

    fn observation(controls: Vec<Control>) -> Observation {
        Observation {
            url: "https://example.test/".into(),
            title: "t".into(),
            page_key: "k".into(),
            captured_at: DateTime::now_utc(),
            response_status: None,
            document_key: String::new(),
            controls,
            omitted_controls: 0,
            viewport_text: String::new(),
            visual_content: false,
            tabs: vec![],
            dialog: None,
            inaccessible_frames: 0,
            can_go_back: false,
            history_entry: None,
        }
    }

    fn control(id: &str, label: &str, role: &str) -> Control {
        Control::new(id, role, label, [Operation::Click])
    }

    fn trigger() -> Control {
        let mut c = control("trip", "Ticket type", "combobox");
        c.value = Some("Round trip".into());
        c.expanded = Some(true);
        c
    }

    fn options() -> Vec<Control> {
        vec![control("one", "One way", "option"), control("round", "Round trip", "option")]
    }

    fn with<F: FnOnce(&mut Control)>(c: &Control, change: F) -> Control {
        let mut c = c.clone();
        change(&mut c);
        c
    }

    #[test]
    fn test_a_menu_closing_without_a_new_value_set_nothing() {
        let before = observation([vec![trigger()], options()].concat());
        let after = observation(vec![with(&trigger(), |c| c.expanded = Some(false))]);
        let done = effect(&before, &after, None);
        assert!(!done.set_something);
        assert!(done.summary.contains("Ticket type expanded: True -> False"));
        assert!(done.summary.contains("removed 2 controls: One way, Round trip"));
    }

    #[test]
    fn test_a_value_transition_is_named_and_counts() {
        let before = observation([vec![trigger()], options()].concat());
        let after = observation(vec![with(&trigger(), |c| {
            c.value = Some("One way".into());
            c.expanded = Some(false);
        })]);
        let done = effect(&before, &after, None);
        assert!(done.set_something);
        assert!(done.summary.contains("Ticket type value: Round trip -> One way"));
    }

    #[test]
    fn test_new_controls_and_a_new_address_are_reported() {
        let before = observation(vec![trigger()]);
        let mut after = observation([vec![trigger()], options()].concat());
        after.url = "https://example.test/b".into();
        let done = effect(&before, &after, None);
        assert!(done.set_something);
        assert!(done.summary.starts_with("went to https://example.test/b; showed 2 controls: One way, Round trip"));
        assert_eq!(effect(&before, &before, None).summary, "nothing visible changed");
    }

    #[test]
    fn test_a_page_state_ignores_text_but_not_values() {
        let page = observation(vec![trigger()]);
        let mut ticking = page.clone();
        ticking.viewport_text = "12:01".into();
        assert_eq!(state_key(&page), state_key(&ticking));
        assert_ne!(
            state_key(&page),
            state_key(&observation(vec![with(&trigger(), |c| c.value = Some("One way".into()))]))
        );
    }

    #[test]
    fn test_a_value_set_on_the_field_an_overlay_stood_in_for_counts() {
        let editor = with(&control("editor", "Where from? ", "combobox"), |c| c.value = Some("Lond".into()));
        let choice = control("london", "London, United Kingdom", "option");
        let field = with(&control("field", "Where from?", "combobox"), |c| c.value = Some("London".into()));
        let done = effect(&observation(vec![editor, choice]), &observation(vec![field]), None);
        assert!(done.set_something);
        // The label differs only in spacing, which is not a change.
        assert!(done.summary.starts_with("changed Where from? value: Lond -> London;"), "{}", done.summary);
    }

    #[test]
    fn test_a_chosen_suggestion_that_leaves_the_typed_text_counts() {
        let editor = with(&control("editor", "Where from?", "combobox"), |c| c.value = Some("London".into()));
        let choice = control("london", "London, United Kingdom", "option");
        let field = with(&control("field", "Where from?", "combobox"), |c| c.value = Some("London".into()));
        let done = effect(&observation(vec![editor, choice.clone()]), &observation(vec![field]), Some(&choice));
        assert!(done.set_something);
    }

    #[test]
    fn test_an_option_no_field_shows_afterwards_set_nothing() {
        let after = observation(vec![with(&trigger(), |c| {
            c.label = "Ticket type. Round trip".into();
            c.expanded = Some(false);
        })]);
        assert!(!effect(&observation(options()), &after, Some(&options()[0])).set_something);
    }

    #[test]
    fn test_an_action_that_changed_only_the_text_says_so() {
        let form = observation(vec![Control::new("s", "button", "Continue", [Operation::Click])]);
        let refused = Observation { viewport_text: "Error: First Name is required".into(), ..form.clone() };
        let done = effect(&form, &refused, None);
        assert_eq!(done.summary, "only the page text changed");
        assert!(!done.set_something);
    }

    #[test]
    fn every_name_is_told_whole() {
        let long = "x".repeat(70);
        assert_eq!(short(&Shown::Text(long.clone())), long);
        assert_eq!(short(&Shown::Missing), "empty");
        let many: Vec<Control> = (0..6).map(|i| control(&format!("c{i}"), &format!("Item {i}"), "link")).collect();
        let done = effect(&observation(vec![]), &observation(many), None);
        assert_eq!(done.summary, "showed 6 controls: Item 0, Item 1, Item 2, Item 3, Item 4, Item 5");
    }
}
