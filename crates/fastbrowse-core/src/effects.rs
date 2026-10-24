//! Port of `src/fastbrowse/effects.py`: what an action visibly did, told back to the policy so that a no-op cannot
//! pass for progress.

use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::page::{BlockKind, Capture, Control, Observation};
use crate::py::{json::dumps, sha256_hex, text};

const SHOWN: usize = 4;
const TEXT: usize = 60;

/// Clicking one of these is choosing a value, so it should leave some value, selection or address changed.
pub const SETTING_ROLES: [&str; 6] = ["option", "checkbox", "radio", "switch", "menuitemradio", "menuitemcheckbox"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    pub summary: String,
    /// The address, or a surviving control's label, value, checked or selected state, changed.
    pub set_something: bool,
}

/// `(frame_id, frame_origin, role, label, context)`.
pub type ControlKey = (Option<String>, Option<String>, String, String, Option<String>);
/// `(value, checked, selected)`.
pub type ControlValue = (Option<String>, Option<bool>, Option<bool>);

/// Committed control values before and after an action on the same document.
#[derive(Debug, Clone, PartialEq)]
pub struct Move {
    pub document: String,
    pub values_before: IndexMap<ControlKey, ControlValue>,
    pub values_after: IndexMap<ControlKey, ControlValue>,
}

impl Move {
    /// Whether a control on the page both before and after now holds another value.
    ///
    /// A wizard's Next swaps one step's fields for the next step's, whose values are the ones typed there before:
    /// nothing was set, and counting it as a setting put back tripped the stall recovery on the pass after a
    /// correction.
    pub fn set_a_value(&self) -> bool {
        self.values_before.iter().any(|(key, before)| self.values_after.get(key).is_some_and(|after| after != before))
    }
}

fn key_of(c: &Control) -> ControlKey {
    (c.frame_id.clone(), c.frame_origin.clone(), c.role.clone(), c.label.clone(), c.context.clone())
}

fn control_values(observation: &Observation) -> IndexMap<ControlKey, ControlValue> {
    let mut controls: IndexMap<ControlKey, Vec<&Control>> = IndexMap::new();
    for c in &observation.controls {
        controls.entry(key_of(c)).or_default().push(c);
    }
    // A redraw replaces node ids. Exact names can survive it, but unnamed twins cannot stand in for each other.
    controls
        .into_iter()
        .filter_map(|(key, items)| match items.as_slice() {
            [c] if c.value.is_some() || c.checked.is_some() || c.selected.is_some() => {
                Some((key, (c.value.clone(), c.checked, c.selected)))
            }
            _ => None,
        })
        .collect()
}

pub fn r#move(before: &Observation, after: &Observation) -> Option<Move> {
    if before.document_key.is_empty() || before.document_key != after.document_key {
        return None;
    }
    Some(Move {
        document: before.document_key.clone(),
        values_before: control_values(before),
        values_after: control_values(after),
    })
}

fn repr_option(value: &Option<String>) -> String {
    value.as_deref().map_or_else(|| "None".to_string(), text::repr)
}

/// `repr(key)`, which is what Python sorts the keys by.
fn repr_key(key: &ControlKey) -> String {
    format!(
        "({}, {}, {}, {}, {})",
        repr_option(&key.0),
        repr_option(&key.1),
        text::repr(&key.2),
        text::repr(&key.3),
        repr_option(&key.4)
    )
}

/// Name a restored value only when the document and every other committed value also return.
pub fn reversal(later: &Move, earlier: &Move) -> Option<String> {
    if later.document != earlier.document || later.values_after != earlier.values_before {
        return None;
    }
    let mut keys: Vec<&ControlKey> = earlier
        .values_before
        .keys()
        .filter(|key| earlier.values_after.contains_key(*key) && later.values_before.contains_key(*key))
        .collect();
    keys.sort_by_cached_key(|key| repr_key(key));
    for key in keys {
        let value = &later.values_after[key];
        if &earlier.values_after[key] != value && &later.values_before[key] != value {
            let mut held = Vec::new();
            if let Some(v) = &value.0 {
                held.push(format!("value={}", short(&Shown::Text(v.clone()))));
            }
            if let Some(v) = value.1 {
                held.push(format!("checked={}", short(&Shown::Flag(v))));
            }
            if let Some(v) = value.2 {
                held.push(format!("selected={}", short(&Shown::Flag(v))));
            }
            return Some(format!("{} keeps returning to {}", control_name(key), held.join(", ")));
        }
    }
    None
}

fn control_name(key: &ControlKey) -> String {
    match key.4.as_deref() {
        Some(context) if !context.is_empty() => format!("{} ({context})", key.3),
        _ => key.3.clone(),
    }
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
    let shown = if squashed.is_empty() { "empty".to_string() } else { squashed };
    if text::len(&shown) <= TEXT { shown } else { format!("{}\u{2026}", text::prefix(&shown, TEXT - 1)) }
}

fn listed(controls: &[&Control]) -> String {
    let count = format!("{} control{}", controls.len(), if controls.len() == 1 { "" } else { "s" });
    let names: Vec<String> = controls.iter().take(SHOWN).map(|c| short(&Shown::Text(c.label.clone()))).collect();
    let more = if controls.len() > SHOWN { format!(" and {} more", controls.len() - SHOWN) } else { String::new() };
    format!("{count}: {}{more}", names.join(", "))
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
        let more = if changes.len() > SHOWN { format!(" and {} more", changes.len() - SHOWN) } else { String::new() };
        parts.push(format!("changed {}{more}", changes[..changes.len().min(SHOWN)].join("; ")));
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
    let mut summary = if parts.is_empty() { "nothing visible changed".to_string() } else { parts.join("; ") };
    // A submit a form refuses moves nothing or reopens a picker, and says why only in the fields it marks: Search on a
    // round trip with no return date reopened the date picker, and the run clicked Done and Search in turn.
    if !(navigated || setting) {
        let blocking: Vec<&Control> = after.controls.iter().filter(|c| c.blocking == Some(true)).collect();
        if !blocking.is_empty() {
            summary.push_str(&format!("; fields the form still needs: {}", listed(&blocking)));
        }
    }
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
            tabs: vec![],
            dialog: None,
            inaccessible_frames: 0,
            can_go_back: false,
            history_entry: None,
        }
    }

    fn with_doc(mut obs: Observation, document: &str) -> Observation {
        obs.document_key = document.into();
        obs
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
    fn test_reversals_do_not_match_a_control_from_another_document_or_context() {
        for scope in ["document_key", "context", "frame_id", "frame_origin"] {
            let mut toggle = control("filter", "Direct only", "checkbox");
            toggle.checked = Some(false);
            toggle.context = Some("Outbound".into());
            let off = with_doc(observation(vec![toggle.clone()]), "doc");
            let mut on = off.clone();
            on.controls = vec![with(&toggle, |c| c.checked = Some(true))];
            let earlier = r#move(&off, &on).unwrap();
            let changed: Vec<Observation> = [&on, &off]
                .into_iter()
                .map(|obs| {
                    let mut obs = obs.clone();
                    match scope {
                        "document_key" => obs.document_key = "another".into(),
                        "context" => obs.controls[0].context = Some("another".into()),
                        "frame_id" => obs.controls[0].frame_id = Some("another".into()),
                        _ => obs.controls[0].frame_origin = Some("another".into()),
                    }
                    obs
                })
                .collect();
            let later = r#move(&changed[0], &changed[1]).unwrap();
            assert_eq!(reversal(&later, &earlier), None, "{scope}");
            assert!(r#move(&on, &with_doc(off.clone(), "new")).is_none());
        }
    }

    #[test]
    fn test_reversal_requires_every_committed_value_to_return() {
        type Set = fn(&mut Control, bool);
        let cases: [(&str, Set, &str); 3] = [
            ("checked", |c, after| c.checked = Some(after), "False"),
            ("selected", |c, after| c.selected = Some(after), "False"),
            ("value", |c, after| c.value = Some(if after { "Name" } else { "Price" }.into()), "Price"),
        ];
        for (attribute, set, before) in cases {
            for other_returns in [false, true] {
                let setting = with(&control("setting", "Setting", "button"), |c| set(c, false));
                let other = with(&control("other", "Other", "button"), |c| c.value = Some("original".into()));
                let original = with_doc(observation(vec![setting.clone(), other.clone()]), "doc");
                let mut changed = original.clone();
                changed.controls = vec![with(&setting, |c| set(c, true)), other.clone()];
                let mut returned = original.clone();
                returned.controls = vec![
                    setting.clone(),
                    if other_returns { other.clone() } else { with(&other, |c| c.value = Some("new".into())) },
                ];
                let earlier = r#move(&original, &changed).unwrap();
                let later = r#move(&changed, &returned).unwrap();
                let expected = other_returns.then(|| format!("Setting keeps returning to {attribute}={before}"));
                assert_eq!(reversal(&later, &earlier), expected, "{attribute} {other_returns}");
            }
        }
    }

    #[test]
    fn test_reopening_controls_with_new_results_is_not_a_value_reversal() {
        for value in ["Round trip", "One way"] {
            let panel = with_doc(observation(vec![trigger(), control("done", "Done", "button")]), "doc");
            let form = with_doc(
                observation(vec![
                    with(&trigger(), |c| c.expanded = Some(false)),
                    control("search", "Search", "button"),
                ]),
                "doc",
            );
            let earlier = r#move(&panel, &form).unwrap();
            let mut reopened = panel.clone();
            reopened.controls = vec![
                with(&trigger(), |c| c.value = Some(value.into())),
                panel.controls[1].clone(),
                control("result", "New result", "button"),
            ];
            let later = r#move(&form, &reopened).unwrap();
            assert_eq!(reversal(&later, &earlier), None);
        }
    }

    #[test]
    fn test_a_wizard_step_swapping_its_fields_sets_no_value() {
        let search = with(&control("search", "Search", "textbox"), |c| c.value = Some(String::new()));
        let first = with(&control("name", "First Name", "textbox"), |c| c.value = Some("Priya".into()));
        let city = with(&control("city", "City", "textbox"), |c| c.value = Some("Manchester".into()));
        let name = with_doc(observation(vec![search.clone(), first.clone()]), "wizard");
        let next = with_doc(observation(vec![search.clone(), city]), "wizard");
        let searched = with_doc(observation(vec![with(&search, |c| c.value = Some("forms".into())), first]), "wizard");
        assert!(!r#move(&name, &next).unwrap().set_a_value());
        assert!(r#move(&name, &searched).unwrap().set_a_value());
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
    fn test_an_action_that_set_nothing_names_the_fields_the_form_still_needs() {
        let search = Control::new("s", "button", "Search", [Operation::Click]);
        let needed = with(&Control::new("r", "textbox", "Return", [Operation::Fill]), |c| c.blocking = Some(true));
        let form = observation(vec![search.clone(), needed.clone()]);
        assert_eq!(
            effect(&form, &form, None).summary,
            "nothing visible changed; fields the form still needs: 1 control: Return"
        );
        let filled = observation(vec![
            search,
            with(&needed, |c| {
                c.value = Some("Oct 23".into());
                c.blocking = Some(false);
            }),
        ]);
        assert!(!effect(&form, &filled, None).summary.contains("still needs"));
    }

    #[test]
    fn a_long_name_is_cut_with_an_ellipsis() {
        let long = "x".repeat(70);
        assert_eq!(short(&Shown::Text(long)), format!("{}\u{2026}", "x".repeat(59)));
        assert_eq!(short(&Shown::Missing), "empty");
    }
}
