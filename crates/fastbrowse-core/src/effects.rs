//! What an action visibly did, told back to the policy so that a no-op cannot pass for progress.

use indexmap::IndexMap;
use serde_json::json;

use crate::models::Operation;
use crate::page::{Control, Observation};
use crate::py::{json::dumps, sha256_hex, text};

/// Clicking one of these is choosing a value, so it should leave some value, selection or address changed.
pub const SETTING_ROLES: [&str; 6] = ["option", "checkbox", "radio", "switch", "menuitemradio", "menuitemcheckbox"];

/// What a control holds after a step changed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Setting {
    /// Its value; none when the page reports it emptied.
    Value(Option<String>),
    Checked(bool),
    Selected(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    pub summary: String,
    /// The address, or a surviving control's label, value, checked or selected state, changed.
    pub set_something: bool,
    /// Each surviving control whose value, checked or selected state changed, by its id on the page after the step,
    /// with what it holds now: what the step left in the page's fields, as the page reports them.
    pub set: Vec<(String, Setting)>,
    /// The page stayed and its content or controls changed beyond focus, losing no control.
    pub added: bool,
    /// Something a person would see differs: a control, the address or the text in the viewport.
    pub changed: bool,
    /// The ids of the controls the step showed on the page it stayed on. A step that loaded another document or
    /// address showed none: every control there is new, and the step's link to them is the address it went to.
    pub shown: Vec<String>,
}

/// Whether a field can be left: an editable control the browser reports holds keyboard focus. Focus a browser never
/// reported is not guessed, and a control's own operation list is not trusted to say it.
pub fn leavable(control: &Control) -> bool {
    control.focused == Some(true) && control.operations.contains(&Operation::Fill)
}

/// Identify a page state by what can be done on it, ignoring text that changes on its own (clocks, ads).
///
/// A field that can be left is marked, since leaving it is an operation the state offers; every other control keys as
/// it did before focus was observed, so a page whose focus was never reported keeps its state.
pub fn state_key(observation: &Observation) -> String {
    let mut controls: Vec<String> = observation
        .controls
        .iter()
        .map(|c| {
            let mut key = json!([c.role, c.label, c.context, c.value, c.checked, c.selected, c.expanded]);
            if leavable(c) {
                key.as_array_mut().expect("an array").push(true.into());
            }
            dumps(&key)
        })
        .collect();
    controls.sort();
    sha256_hex(dumps(&json!([observation.url, controls])).as_bytes())
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

fn counted(controls: usize) -> String {
    format!("{controls} control{}", if controls == 1 { "" } else { "s" })
}

fn dialog(name: &str) -> String {
    if name.is_empty() { "a dialog".to_string() } else { format!("the dialog {name:?}") }
}

fn listed(controls: &[&Control]) -> String {
    let count = counted(controls.len());
    let names: Vec<String> = controls.iter().map(|c| short(&Shown::Text(c.label.clone()))).collect();
    format!("{count}: {}", names.join(", "))
}

fn plain(text: &str) -> String {
    text::casefold(&text::squash(text))
}

/// Whether a field now shows `choice`, whole or as its first comma-separated part ("London" for "London, United
/// Kingdom"). A fragment of the choice is only what was typed on the way to it.
fn shows(control: &Control, choice: &str) -> bool {
    let mut shown = Vec::new();
    if let Some(value) = control.value.as_deref().filter(|v| !v.is_empty()) {
        shown.push(plain(value));
    }
    if control.role == "combobox" {
        shown.push(plain(&control.label));
    }
    let first = choice.split(',').next().unwrap_or(choice).trim();
    shown.iter().filter(|t| !t.is_empty()).any(|t| t.contains(choice) || t == first)
}

/// The text the page shows after a step and did not before, each piece once per time it was added, in the order the
/// page lists them. Of a piece the page already showed and shows once more, the last one listed is the one named.
///
/// The viewport's text and the text outside it are read together, so scrolling a piece into view adds nothing. A
/// piece that is the label of a control in `shown` is left to that control's own mention.
fn added_text(before: &Observation, after: &Observation, shown: &[&Control]) -> Vec<String> {
    fn pieces(observation: &Observation) -> impl Iterator<Item = &str> {
        [&observation.viewport_text, &observation.offscreen_text]
            .into_iter()
            .flat_map(|text| text.lines())
            .map(text::strip)
            .filter(|piece| !piece.is_empty())
    }
    let mut told: IndexMap<&str, usize> = IndexMap::new();
    for piece in pieces(before).chain(shown.iter().map(|c| text::strip(&c.label))) {
        *told.entry(piece).or_default() += 1;
    }
    pieces(after)
        .filter(|piece| match told.get_mut(piece) {
            Some(left) if *left > 0 => {
                *left -= 1;
                false
            }
            _ => true,
        })
        .map(str::to_string)
        .collect()
}

/// What changed from `before` to `after`. With `chosen`, the option clicked in between, a field showing it counts as
/// a value set: an autocomplete choice can leave the text that was typed exactly as it was. `waited` is a step that only
/// let time pass, a Wait.
pub fn effect(before: &Observation, after: &Observation, chosen: Option<&Control>, waited: bool) -> Effect {
    let replaced = before.document_key != after.document_key;
    // Each document starts its control ids again. Matching those ids after navigation invents edits to controls
    // that belonged to the page left behind, even when a reload kept the same address and field labels.
    let old: IndexMap<&str, &Control> =
        before.controls.iter().filter(|_| !replaced).map(|c| (c.id.as_str(), c)).collect();
    let new: IndexMap<&str, &Control> = after.controls.iter().map(|c| (c.id.as_str(), c)).collect();
    let mut parts = Vec::new();
    let navigated = before.url != after.url;
    if navigated {
        parts.push(format!("went to {}", short(&Shown::Text(after.url.clone()))));
    }
    // The status belongs to the document the page holds, so a step that loaded none, such as a fill on the page an
    // error left behind, did not meet it.
    if let Some(status) = after.response_status.filter(|status| *status >= 400 && (navigated || replaced)) {
        parts.push(format!("HTTP {status}: destination did not load successfully"));
    }
    let mut changes = Vec::new();
    let mut content_changed = before.viewport_text != after.viewport_text;
    let mut setting = false;
    let mut set = Vec::new();
    // A picker can open as an overlay with its own copy of the field, so the field the choice fills is a different
    // element before and after. A control unmatched by id is compared with the one earlier control sharing its role
    // and label, if exactly one did.
    let mut by_name: IndexMap<(&str, &str), Vec<&Control>> = IndexMap::new();
    for control in old.values() {
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
                content_changed = true;
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
        // A flag the page stopped reporting holds nothing it can be said to hold.
        let held = [
            (was.value != now.value).then(|| Setting::Value(now.value.clone())),
            now.checked.filter(|_| was.checked != now.checked).map(Setting::Checked),
            now.selected.filter(|_| was.selected != now.selected).map(Setting::Selected),
        ];
        set.extend(held.into_iter().flatten().map(|held| (now.id.clone(), held)));
        // Focus is told where both readings know it and the control is a field leaving is offered for. Entering or
        // leaving a field is what happened, so it is told and counts as a change, but it sets no value. A reading that
        // never reported focus invents no transition.
        if let (Some(a), Some(b)) = (was.focused, now.focused)
            && a != b
            && now.operations.contains(&Operation::Fill)
        {
            let name = short(&Shown::Text(was.label.clone()));
            changes.push(format!("{name} focused: {} -> {}", Shown::Flag(a).str(), Shown::Flag(b).str()));
        }
    }
    // A player's clock and paused state live outside its controls, and the policy sees only the current reading, so
    // a step can confirm playback started, stopped or was moved only from what each earlier step is told it changed.
    // A playing clock moves by itself, like a page's own clock, so its movement is told only for a step that started or
    // stopped the player, or for a Wait, which is the step that lets time pass. Players pair up by position within one
    // document; a new document's players are new.
    if !replaced && before.media.len() == after.media.len() {
        for (was, now) in before.media.iter().zip(&after.media) {
            let name = match now.label.as_ref().or(now.heading.as_ref()) {
                Some(named) => format!("{} {named}", now.kind),
                None => now.kind.clone(),
            };
            for (attribute, a, b) in
                [("paused", was.paused, now.paused), ("ended", was.ended, now.ended), ("muted", was.muted, now.muted)]
            {
                if a != b {
                    changes.push(format!("{name} {attribute}: {} -> {}", Shown::Flag(a).str(), Shown::Flag(b).str()));
                }
            }
            if was.elapsed_seconds != now.elapsed_seconds && (waited || was.paused || now.paused) {
                changes.push(format!("{name} elapsed: {} s -> {} s", was.elapsed_seconds, now.elapsed_seconds));
            }
        }
    }
    if !changes.is_empty() {
        parts.push(format!("changed {}", changes.join("; ")));
    }
    // A new document starts its control ids again, so it has no control a step showed or lost: every control there is
    // new, and the next step is shown them all. Listing both pages here put two whole pages, 920 controls on a
    // Wikipedia article, into every later request of the run.
    if replaced && !navigated {
        parts.push("loaded a new document at this address".to_string());
    }
    let shown: Vec<&Control> = if replaced {
        Vec::new()
    } else {
        new.iter().filter(|(key, _)| !old.contains_key(*key)).map(|(_, c)| *c).collect()
    };
    let hidden: Vec<&Control> = if replaced {
        Vec::new()
    } else {
        before.controls.iter().filter(|c| !new.contains_key(c.id.as_str())).collect()
    };
    let stayed_whole = !navigated && !replaced && hidden.is_empty();
    // A modal hides the page under it and gives it back on closing, so what went behind one or came back from behind
    // it is counted, not named: the page's controls are the current elements once it closes, and cannot be used while
    // it is open. Named, Google Flights' Stops filter put 145 controls into every later request, and 125 more on closing.
    // One modal giving way to another is neither: the page stays behind it, and the controls are named as they change.
    let opened = after.modal.as_ref().filter(|_| !replaced && before.modal.is_none());
    let closed = before.modal.as_ref().filter(|_| !replaced && after.modal.is_none());
    if let Some(name) = closed.filter(|_| !shown.is_empty()) {
        parts.push(format!("closed {}; {} came back from behind it", dialog(name), counted(shown.len())));
    } else if !shown.is_empty() {
        parts.push(format!("showed {}", listed(&shown)));
    }
    if let Some(name) = opened.filter(|_| !hidden.is_empty()) {
        parts.push(format!("opened {} over the page; {} went behind it", dialog(name), counted(hidden.len())));
    } else if !hidden.is_empty() {
        parts.push(format!("removed {}", listed(&hidden)));
    }
    // A step that stays on its page can answer in text alone, and that text is the only record that the step is what
    // put it there: a comment form that posted showed the new comment, the effect said only that the box emptied, and
    // the run posted the same comment twice more. Another document or address is all new, and the page text shows it.
    if !navigated && !replaced {
        let added = added_text(before, after, &shown);
        if !added.is_empty() {
            let quoted: Vec<String> = added.iter().map(|piece| format!("{piece:?}")).collect();
            parts.push(format!("added the text: {}", quoted.join(", ")));
        }
    }
    if let Some(chosen) = chosen.filter(|_| !replaced) {
        let choice = plain(&chosen.label);
        setting =
            setting || after.controls.iter().any(|c| !SETTING_ROLES.contains(&c.role.as_str()) && shows(c, &choice));
    }
    // A form that refuses a submit can say why in its text alone, as Sauce Demo's checkout shows "Error: First Name is
    // required" and changes no control.
    let changed = !parts.is_empty() || before.viewport_text != after.viewport_text;
    let summary = if !parts.is_empty() {
        parts.join("; ")
    } else if changed {
        "only the page text changed".to_string()
    } else {
        "nothing visible changed".to_string()
    };
    Effect {
        summary,
        set_something: navigated || setting,
        set,
        added: stayed_whole && (content_changed || !shown.is_empty()),
        changed,
        shown: if navigated || replaced { Vec::new() } else { shown.iter().map(|c| c.id.clone()).collect() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Operation;
    use crate::page::Media;
    use crate::py::datetime::DateTime;

    #[test]
    fn test_a_fragment_of_the_choice_does_not_count_as_showing_it() {
        let mut field = Control::new("f", "combobox", "City", [Operation::Fill]);
        field.value = Some("Lon".into());
        assert!(!shows(&field, "london, united kingdom"));
        field.value = Some("London".into());
        assert!(shows(&field, "london, united kingdom"));
        field.value = Some("London, United Kingdom".into());
        assert!(shows(&field, "london"));
    }

    fn observation(controls: Vec<Control>) -> Observation {
        Observation {
            url: "https://example.test/".into(),
            title: "t".into(),
            page_key: "k".into(),
            captured_at: DateTime::now_utc(),
            response_status: None,
            document_key: String::new(),
            controls,
            viewport_text: String::new(),
            offscreen_text: String::new(),
            visual_content: false,
            media: Vec::new(),
            tabs: vec![],
            dialog: None,
            inaccessible_frames: 0,
            can_go_back: false,
            modal: None,
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
    fn test_an_error_status_is_reported_only_by_the_step_that_loaded_it() {
        let mut before = observation(vec![control("p", "Postcode", "textbox")]);
        before.document_key = "error".into();
        before.response_status = Some(400);
        let mut after = observation(vec![with(&before.controls[0], |c| c.value = Some("SW1A 1AA".into()))]);
        after.document_key = "error".into();
        after.response_status = Some(400);
        assert!(!effect(&before, &after, None, false).summary.contains("HTTP 400"));
        before.document_key = "form".into();
        assert!(effect(&before, &after, None, false).summary.contains("HTTP 400"));
    }

    #[test]
    fn test_controls_a_step_showed_are_named_only_on_the_page_it_stayed_on() {
        let before = observation(vec![trigger()]);
        let after = observation([vec![trigger()], options()].concat());
        let ids: Vec<String> = options().iter().map(|c| c.id.clone()).collect();
        assert_eq!(effect(&before, &after, None, false).shown, ids);
        let replaced = Observation { document_key: "next".into(), ..after.clone() };
        assert!(effect(&before, &replaced, None, false).shown.is_empty());
        let moved = Observation { url: "https://example.test/next".into(), ..after };
        assert!(effect(&before, &moved, None, false).shown.is_empty());
    }

    #[test]
    fn test_a_modal_counts_the_page_behind_it_and_names_its_own_controls() {
        let page: Vec<Control> = (0..3).map(|i| control(&format!("p{i}"), &format!("Page {i}"), "link")).collect();
        let dialog = vec![control("close", "Close dialog", "button"), control("nonstop", "Nonstop only", "radio")];
        let open = Observation { modal: Some("Stops".into()), ..observation(dialog.clone()) };
        let opened = effect(&observation(page.clone()), &open, None, false);
        assert_eq!(
            opened.summary,
            "showed 2 controls: Close dialog, Nonstop only; opened the dialog \"Stops\" over the page; 3 controls went behind it"
        );
        let closed = effect(&open, &observation(page), None, false);
        assert_eq!(
            closed.summary,
            "closed the dialog \"Stops\"; 3 controls came back from behind it; removed 2 controls: Close dialog, Nonstop only"
        );
        let unnamed = Observation { modal: Some(String::new()), ..observation(dialog) };
        let shown = effect(&observation(vec![control("p0", "Page 0", "link")]), &unnamed, None, false);
        assert!(shown.summary.ends_with("opened a dialog over the page; 1 control went behind it"));
        // One modal giving way to another opens and closes nothing: the page stayed behind.
        let sort = vec![control("close", "Close dialog", "button"), control("price", "Price", "radio")];
        let renamed = effect(&open, &Observation { modal: Some("Sort".into()), ..observation(sort) }, None, false);
        assert_eq!(renamed.summary, "showed 1 control: Price; removed 1 control: Nonstop only");
    }

    #[test]
    fn test_a_menu_closing_without_a_new_value_set_nothing() {
        let before = observation([vec![trigger()], options()].concat());
        let after = observation(vec![with(&trigger(), |c| c.expanded = Some(false))]);
        let done = effect(&before, &after, None, false);
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
        let done = effect(&before, &after, None, false);
        assert!(done.set_something);
        assert!(done.summary.contains("Ticket type value: Round trip -> One way"));
        assert_eq!(done.set, [(trigger().id, Setting::Value(Some("One way".into())))]);
    }

    #[test]
    fn test_control_ids_do_not_survive_a_new_document_at_the_same_address() {
        let mut before = observation(vec![trigger()]);
        before.document_key = "first".into();
        let mut after = observation(vec![with(&trigger(), |c| c.value = Some("One way".into()))]);
        after.document_key = "second".into();
        let done = effect(&before, &after, None, false);
        assert!(!done.set_something, "a different document did not edit the old field");
        assert!(!done.added);
        assert!(done.changed);
        assert!(!done.summary.contains("changed Ticket type"), "{}", done.summary);
        assert_eq!(done.summary, "loaded a new document at this address");
        assert!(!effect(&before, &after, Some(&options()[0]), false).set_something);
        let mut away = after.clone();
        away.url = "https://example.test/next".into();
        assert_eq!(effect(&before, &away, None, false).summary, "went to https://example.test/next");
    }

    #[test]
    fn test_new_documents_do_not_match_fields_by_name() {
        let mut before = observation(vec![trigger()]);
        before.document_key = "first".into();
        let mut after = observation(vec![with(&trigger(), |c| {
            c.id = "another".into();
            c.value = Some("One way".into());
        })]);
        after.document_key = "second".into();
        let done = effect(&before, &after, None, false);
        assert!(!done.set_something);
        assert!(!done.summary.contains("changed Ticket type"), "{}", done.summary);
    }

    #[test]
    fn test_new_controls_and_a_new_address_are_reported() {
        let before = observation(vec![trigger()]);
        let mut after = observation([vec![trigger()], options()].concat());
        after.url = "https://example.test/b".into();
        let done = effect(&before, &after, None, false);
        assert!(done.set_something);
        assert!(done.summary.starts_with("went to https://example.test/b; showed 2 controls: One way, Round trip"));
        assert_eq!(effect(&before, &before, None, false).summary, "nothing visible changed");
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
        let done = effect(&observation(vec![editor, choice]), &observation(vec![field]), None, false);
        assert!(done.set_something);
        // The label differs only in spacing, which is not a change.
        assert!(done.summary.starts_with("changed Where from? value: Lond -> London;"), "{}", done.summary);
    }

    #[test]
    fn test_a_chosen_suggestion_that_leaves_the_typed_text_counts() {
        let editor = with(&control("editor", "Where from?", "combobox"), |c| c.value = Some("London".into()));
        let choice = control("london", "London, United Kingdom", "option");
        let field = with(&control("field", "Where from?", "combobox"), |c| c.value = Some("London".into()));
        let done = effect(&observation(vec![editor, choice.clone()]), &observation(vec![field]), Some(&choice), false);
        assert!(done.set_something);
    }

    #[test]
    fn test_an_option_no_field_shows_afterwards_set_nothing() {
        let after = observation(vec![with(&trigger(), |c| {
            c.label = "Ticket type. Round trip".into();
            c.expanded = Some(false);
        })]);
        assert!(!effect(&observation(options()), &after, Some(&options()[0]), false).set_something);
    }

    #[test]
    fn test_an_action_that_changed_only_the_text_says_so() {
        let form = observation(vec![Control::new("s", "button", "Continue", [Operation::Click])]);
        let refused = Observation { viewport_text: "Error: First Name is required".into(), ..form.clone() };
        let done = effect(&form, &refused, None, false);
        assert_eq!(done.summary, "added the text: \"Error: First Name is required\"");
        assert!(!done.set_something);
        assert!(done.added, "text-only changes remain observable without claiming added records");
    }

    #[test]
    fn text_only_appended_rows_remain_observable() {
        let before = Observation {
            viewport_text: "Record A".into(),
            ..observation(vec![Control::new("m", "button", "More", [Operation::Click])])
        };
        let after = Observation { viewport_text: "Record A\nRecord B".into(), ..before.clone() };
        let done = effect(&before, &after, before.controls.first(), false);
        assert!(done.changed);
        assert!(done.added);
        assert_eq!(done.summary, "added the text: \"Record B\"");
    }

    #[test]
    fn test_a_step_that_stays_on_its_page_names_the_text_it_added() {
        let reply = Control::new("r1", "button", "Reply", [Operation::Click]);
        let before = Observation {
            viewport_text: "Comments\nPost".into(),
            offscreen_text: "Serzath\nWorks great.\nReply".into(),
            ..observation(vec![with(&reply, |c| c.id = "r0".into())])
        };
        // The new comment's own Reply button is named as a control, and the older comment only moved out of view.
        let after = Observation {
            viewport_text: "Comments\nPost\ncjber\njust now\nVersion 2.4.1 is out.\nReply".into(),
            offscreen_text: "Serzath\nWorks great.\nReply".into(),
            controls: vec![reply.clone(), with(&reply, |c| c.id = "r0".into())],
            ..before.clone()
        };
        assert_eq!(
            effect(&before, &after, None, false).summary,
            "showed 1 control: Reply; added the text: \"cjber\", \"just now\", \"Version 2.4.1 is out.\""
        );
        // Text scrolled into view was on the page already.
        let scrolled = Observation {
            viewport_text: "Serzath\nWorks great.\nReply".into(),
            offscreen_text: "Comments\nPost".into(),
            ..before.clone()
        };
        assert_eq!(effect(&before, &scrolled, None, false).summary, "only the page text changed");
        // A second copy of a line already there is still added.
        let again =
            Observation { offscreen_text: "Serzath\nWorks great.\nReply\nWorks great.".into(), ..before.clone() };
        assert_eq!(effect(&before, &again, None, false).summary, "added the text: \"Works great.\"");
        // Another document is all new: the page text shows it, and the step's effect is the address.
        let moved = Observation { url: "https://example.test/next".into(), ..after.clone() };
        assert!(!effect(&before, &moved, None, false).summary.contains("added the text"));
        let replaced = Observation { document_key: "next".into(), ..after };
        assert!(!effect(&before, &replaced, None, false).summary.contains("added the text"));
    }

    fn field(focused: Option<bool>) -> Control {
        let mut c = Control::new("ref", "textbox", "Reference", [Operation::Fill, Operation::Enter]);
        c.value = Some("Alpha-17".into());
        c.focused = focused;
        c
    }

    #[test]
    fn test_leaving_a_field_is_told_and_is_a_new_state_but_sets_nothing() {
        let typing = observation(vec![field(Some(true))]);
        let left = observation(vec![field(Some(false))]);
        let done = effect(&typing, &left, None, false);
        assert_eq!(done.summary, "changed Reference focused: True -> False");
        assert!(done.changed && !done.set_something);
        assert!(!done.added, "leaving a field adds no controls or records");
        let entered = effect(&left, &typing, None, false);
        assert_eq!(entered.summary, "changed Reference focused: False -> True");
        assert!(!entered.set_something);
        assert_ne!(state_key(&typing), state_key(&left));
    }

    #[test]
    fn test_focus_a_reading_never_reported_is_no_transition() {
        let unknown = observation(vec![field(None)]);
        for known in [Some(false), Some(true)] {
            let known = observation(vec![field(known)]);
            assert_eq!(effect(&unknown, &known, None, false).summary, "nothing visible changed");
            assert_eq!(effect(&known, &unknown, None, false).summary, "nothing visible changed");
        }
        // An unfocused field offers nothing an unknown one does not, so the page state is the same one.
        assert_eq!(state_key(&unknown), state_key(&observation(vec![field(Some(false))])));
        // A button's focus offers no way to leave it, and is not told.
        let button = |focused| Control { focused, ..control("go", "Go", "button") };
        let done = effect(&observation(vec![button(Some(false))]), &observation(vec![button(Some(true))]), None, false);
        assert_eq!(done.summary, "nothing visible changed");
    }

    #[test]
    fn every_name_is_told_whole() {
        let long = "x".repeat(70);
        assert_eq!(short(&Shown::Text(long.clone())), long);
        assert_eq!(short(&Shown::Missing), "empty");
        let many: Vec<Control> = (0..6).map(|i| control(&format!("c{i}"), &format!("Item {i}"), "link")).collect();
        let done = effect(&observation(vec![]), &observation(many), None, false);
        assert_eq!(done.summary, "showed 6 controls: Item 0, Item 1, Item 2, Item 3, Item 4, Item 5");
    }

    fn player(paused: bool, elapsed_seconds: f64) -> Media {
        Media {
            kind: "audio".into(),
            label: None,
            heading: Some("Gymnopedie No. 1".into()),
            paused,
            ended: false,
            muted: false,
            elapsed_seconds,
            duration_seconds: Some(204.8),
        }
    }

    #[test]
    fn test_a_players_change_is_told_so_a_later_step_can_confirm_it() {
        let page = observation(vec![Control::new("p", "button", "Pause", [Operation::Click])]);
        let playing = Observation { media: vec![player(false, 12.0)], ..page.clone() };
        let later = Observation { media: vec![player(false, 15.5)], ..page.clone() };
        let paused = Observation { media: vec![player(true, 15.6)], ..page.clone() };
        let sought = Observation { media: vec![player(true, 90.0)], ..page.clone() };
        let resumed = Observation { media: vec![player(false, 16.0)], ..page.clone() };
        // A playing clock moves by itself: a step that left it playing changed nothing about it.
        assert_eq!(effect(&playing, &later, None, false).summary, "nothing visible changed");
        // A Wait is the step that lets time pass, so how far a playing clock moved is what it did.
        assert_eq!(
            effect(&playing, &later, None, true).summary,
            "changed audio Gymnopedie No. 1 elapsed: 12 s -> 15.5 s"
        );
        assert_eq!(effect(&paused, &paused, None, true).summary, "nothing visible changed");
        assert_eq!(
            effect(&later, &paused, None, false).summary,
            "changed audio Gymnopedie No. 1 paused: False -> True; audio Gymnopedie No. 1 elapsed: 15.5 s -> 15.6 s"
        );
        assert_eq!(
            effect(&paused, &resumed, None, false).summary,
            "changed audio Gymnopedie No. 1 paused: True -> False; audio Gymnopedie No. 1 elapsed: 15.6 s -> 16 s"
        );
        assert_eq!(
            effect(&paused, &sought, None, false).summary,
            "changed audio Gymnopedie No. 1 elapsed: 15.6 s -> 90 s"
        );
        assert_eq!(effect(&paused, &paused, None, false).summary, "nothing visible changed");
        let unnamed = |media: &Observation| Observation {
            media: media.media.iter().map(|m| Media { heading: None, ..m.clone() }).collect(),
            ..media.clone()
        };
        assert_eq!(
            effect(&unnamed(&later), &unnamed(&paused), None, false).summary,
            "changed audio paused: False -> True; audio elapsed: 15.5 s -> 15.6 s"
        );
    }
}
