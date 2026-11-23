//! The page as the chat driver reads it: one line per control, `[id] role "label" = "value" states`, then the page's
//! visible text. A `*` before an id marks a control that was not on the page the driver last looked at, so it sees
//! what its last action showed: a date field's popup appears as new ids, where a fresh per-step question saw only the
//! controls that exist and matched the task's words to the wrong calendar's Next (#1538).

use std::collections::HashSet;

use fastbrowse_core::page::{Control, Observation};

/// `observation` as lines, with `seen` the control ids of the page the driver looked at before.
pub(super) fn render(observation: &Observation, seen: &HashSet<String>) -> String {
    let mut out = format!("Page: {} {}\n", q(&observation.title), observation.url);
    if let Some(modal) = observation.modal.as_deref().filter(|modal| !modal.is_empty()) {
        out += &format!("A modal is open: {}. Controls behind it are not listed.\n", q(modal));
    }
    if let Some(dialog) = &observation.dialog {
        out += &format!("A {} dialog is open: {}\n", dialog.kind, q(&dialog.message));
    }
    if observation.tabs.len() > 1 {
        out += "Tabs:\n";
        for tab in &observation.tabs {
            let active = if tab.active { " (this tab)" } else { "" };
            out += &format!("  tab {} {} {}{active}\n", tab.id, q(&tab.title), tab.url);
        }
    }
    // A popup a page appends to `<body>` comes last in reading order, under the section that ends the page: listed
    // among the page's controls, a datepicker's popup read as the inline calendar beside it (#1538).
    let mut layers: Vec<u32> = observation.controls.iter().filter_map(|control| control.layer).collect();
    layers.sort_unstable();
    layers.dedup();
    for (heading, layer) in std::iter::once(("Controls:".to_string(), None))
        .chain(layers.into_iter().map(|layer| ("Controls in a layer drawn over the page:".to_string(), Some(layer))))
    {
        out += &heading;
        out.push('\n');
        for control in observation.controls.iter().filter(|control| control.layer == layer) {
            out += &line(control, !seen.is_empty() && !seen.contains(&control.id));
            out.push('\n');
        }
    }
    out += "Visible text:\n";
    out += &observation.viewport_text;
    if !observation.offscreen_text.is_empty() {
        out += "\nText outside the viewport:\n";
        out += &observation.offscreen_text;
    }
    if observation.inaccessible_frames > 0 {
        out += &format!("\n{} frames could not be read.", observation.inaccessible_frames);
    }
    out
}

/// One control: what it is, what it holds, and the states that decide what acting on it does.
pub(super) fn line(control: &Control, new: bool) -> String {
    let mut out = format!("{}[{}] {} {}", if new { "*" } else { "" }, control.id, control.role, q(&control.label));
    if let Some(value) = control.value.as_deref().filter(|value| !value.is_empty()) {
        out += &format!(" = {}", q(value));
    }
    if let Some(context) = &control.context {
        out += &format!(" in {}", q(context));
    }
    let mut states = Vec::new();
    for (on, name) in [
        (control.checked, "checked"),
        (control.selected, "selected"),
        (control.focused, "focused"),
        (control.required, "required"),
        (control.invalid, "invalid"),
    ] {
        if on == Some(true) {
            states.push(name.to_string());
        }
    }
    match control.expanded {
        Some(true) => states.push("expanded".into()),
        Some(false) => states.push("collapsed".into()),
        None => {}
    }
    // A readonly date field is opened by a click, not typed in: told only that it can be clicked, a chooser saw no
    // reason to open it before acting on the calendar beside it.
    if control.read_only {
        states.push("readonly".into());
    }
    if control.offscreen {
        states.push("offscreen".into());
    }
    if control.already_read == Some(true) {
        states.push("leads to a page already read".into());
    }
    if let Some(message) = &control.validation_message {
        states.push(format!("refused: {}", q(message)));
    }
    if let Some(description) = &control.description {
        states.push(format!("described {}", q(description)));
    }
    if let (Some(position), Some(count)) = (control.item_position, control.item_count) {
        states.push(format!("item {position} of {count}"));
    }
    if !states.is_empty() {
        out += &format!(" ({})", states.join(", "));
    }
    let operations: Vec<String> = control.operations.iter().map(|operation| operation.to_string()).collect();
    out += &format!(" can {}", operations.join("/"));
    if !control.options.is_empty() {
        let options: Vec<String> = control.options.iter().map(|option| q(option)).collect();
        out += &format!(" options {}", options.join(" | "));
    }
    if let Some(href) = &control.href {
        out += &format!(" -> {href}");
    }
    out
}

/// `value` quoted as JSON writes a string, which escapes the quotes and line breaks a label can hold.
fn q(value: &str) -> String {
    serde_json::to_string(value).expect("a string serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastbrowse_core::models::Operation;

    #[test]
    fn a_control_new_since_the_last_look_is_starred() {
        let date =
            Control { value: Some("11/23/2026".into()), ..Control::new("7", "textbox", "Date", [Operation::Fill]) };
        let next = Control::new("9", "link", "Next", [Operation::Click]);
        assert_eq!(line(&date, false), r#"[7] textbox "Date" = "11/23/2026" can fill"#);
        assert_eq!(line(&next, true), r#"*[9] link "Next" can click"#);
    }

    #[test]
    fn a_layers_controls_are_listed_under_their_own_heading() {
        let field = Control::new("1", "textbox", "Date", [Operation::Click]);
        let day = Control { layer: Some(1), ..Control::new("2", "link", "4", [Operation::Click]) };
        let observation: Observation = serde_json::from_value(serde_json::json!({
            "url": "https://example.test/", "title": "t", "page_key": "k", "document_key": "d",
            "captured_at": "2026-11-18T00:00:00Z", "viewport_text": "", "tabs": [],
            "controls": [day, field]
        }))
        .unwrap();
        let text = render(&observation, &HashSet::new());
        let page = text.find(r#"[1] textbox "Date""#).unwrap();
        let layer = text.find("Controls in a layer drawn over the page:\n[2] link \"4\"").unwrap();
        assert!(page < layer, "{text}");
    }

    #[test]
    fn states_and_options_are_shown() {
        let select = Control {
            options: vec!["Jan".into(), "Feb".into()],
            expanded: Some(false),
            required: Some(true),
            ..Control::new("3", "combobox", "Month", [Operation::Select])
        };
        assert_eq!(
            line(&select, false),
            r#"[3] combobox "Month" (required, collapsed) can select options "Jan" | "Feb""#
        );
    }
}
