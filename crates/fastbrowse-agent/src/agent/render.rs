//! The page as the chat driver reads it: one line per control, `[id] role "label" = "value" states`, then the page's
//! visible text. A `*` before an id marks a control that was not on the page the driver last looked at, so it sees
//! what its last action showed: a date field's popup appears as new ids, where a fresh per-step question saw only the
//! controls that exist and matched the task's words to the wrong calendar's Next (#1538).

use std::collections::HashSet;

use fastbrowse_core::page::{BlockKind, Capture, Control, Observation};

/// `observation` as lines, with `seen` the control ids of the page the driver looked at before. `text` is the page's
/// captured text, the text a quote is checked against; without it the observation's visible text goes instead.
pub(super) fn render(observation: &Observation, seen: &HashSet<String>, text: Option<&str>) -> String {
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
    // The driver reads the text its quotes are checked against. Shown the visible text, a table came as one cell a
    // line with its icons left out, so the flag marking the Best Picture winner never reached it and every quote it
    // took of the row failed against the captured "| Argo | 7 | 3 | [flag icon] |".
    if let Some(text) = text {
        out += "Page text:\n";
        out += text;
    } else {
        out += "Visible text:\n";
        out += &observation.viewport_text;
        if !observation.offscreen_text.is_empty() {
            out += "\nText outside the viewport:\n";
            out += &observation.offscreen_text;
        }
    }
    if observation.inaccessible_frames > 0 {
        out += &format!("\n{} frames could not be read.", observation.inaccessible_frames);
    }
    out
}

/// What the driver reads of `capture`: every block but the capture's own observations and its title. Observations
/// describe elements the control list already gives, links and fields with their values, or images the driver was never
/// shown; with them a Google Flights page sent its ten city images' 200-character addresses every turn, and a run's
/// input doubled. The title is on the page's first line already.
///
/// A heading starts `# `: given the page's lines unmarked, a run asked for a new window's heading answered with its
/// title, "Example of a new window", over the `h1` "Example of a new window page for Automation Testing Practice".
pub(super) fn page_text(capture: &Capture) -> String {
    let blocks =
        capture.blocks.iter().filter(|block| !matches!(block.kind, BlockKind::Observation | BlockKind::DocumentTitle));
    blocks
        .map(|block| match block.kind {
            BlockKind::Heading => format!("# {}", capture.block_text(block)),
            _ => capture.block_text(block).to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
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
    // A native date input keeps only its ISO form: told nothing of it, the driver typed "11/30/2026" into a date
    // range's fields, saw both fills refused, and ended stuck pressing keys at a picker the page never drew.
    if let Some(form) = control.input_type.as_deref().and_then(fastbrowse_core::page::native_date_form) {
        states.push(format!("takes {form}"));
    }
    // A drag on a drawn surface follows a path drawn from a screenshot; told nothing of it, the driver could not tell
    // a canvas from a slider handle that needs a control to drop on.
    if control.drawn == Some(true) {
        states.push("drawn".into());
    }
    if control.offscreen {
        states.push("offscreen".into());
    }
    if control.already_read == Some(true) {
        states.push("leads to a page already read".into());
    }
    // Shown only an href, a run checking a page followed its store button off the site and ended at that store's
    // security check (#1137).
    if control.other_site == Some(true) {
        states.push("leads to another site".into());
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
    fn a_native_date_input_names_the_form_it_keeps() {
        let date =
            Control { input_type: Some("date".into()), ..Control::new("7", "textbox", "Start", [Operation::Fill]) };
        assert_eq!(line(&date, false), r#"[7] textbox "Start" (takes YYYY-MM-DD, such as 2026-11-16) can fill"#);
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
        let text = render(&observation, &HashSet::new(), None);
        let page = text.find(r#"[1] textbox "Date""#).unwrap();
        let layer = text.find("Controls in a layer drawn over the page:\n[2] link \"4\"").unwrap();
        assert!(page < layer, "{text}");
    }

    #[test]
    fn the_page_text_is_the_captured_text_quotes_are_checked_against() {
        let observation: Observation = serde_json::from_value(serde_json::json!({
            "url": "https://example.test/", "title": "t", "page_key": "k", "document_key": "d",
            "captured_at": "2026-11-18T00:00:00Z", "viewport_text": "Argo\n7\n3", "tabs": [], "controls": []
        }))
        .unwrap();
        let text = render(&observation, &HashSet::new(), Some("| Argo | 7 | 3 | [flag icon] |"));
        assert!(text.ends_with("Page text:\n| Argo | 7 | 3 | [flag icon] |"), "{text}");
        assert!(!text.contains("Visible text"), "{text}");
    }

    #[test]
    fn the_page_text_leaves_out_what_the_controls_and_images_describe() {
        let capture = crate::retrieval::testing::capture(&[
            (BlockKind::Table, "| Argo | 7 | 3 | [flag icon] |"),
            (BlockKind::Observation, "Where to?: [empty]"),
        ]);
        assert_eq!(page_text(&capture), "| Argo | 7 | 3 | [flag icon] |");
    }

    #[test]
    fn a_heading_is_marked_and_the_title_left_to_the_page_line() {
        let capture = crate::retrieval::testing::capture(&[
            (BlockKind::Heading, "Example of a new window page for Automation Testing Practice"),
            (BlockKind::Paragraph, "Body"),
            (BlockKind::DocumentTitle, "Example of a new window"),
        ]);
        assert_eq!(page_text(&capture), "# Example of a new window page for Automation Testing Practice\n\nBody");
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
