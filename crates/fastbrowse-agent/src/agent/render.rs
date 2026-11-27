//! The page as the chat driver reads it: one line per control, `[id] role "label" = "value" states`, then the page's
//! visible text. A `*` before an id marks a control that was not on the page the driver last looked at, so it sees
//! what its last action showed: a date field's popup appears as new ids, where a fresh per-step question saw only the
//! controls that exist and matched the task's words to the wrong calendar's Next (#1538).

use std::collections::HashSet;

use fastbrowse_core::page::{BlockKind, Capture, Control, Observation};

/// `observation` as lines, with `seen` the control ids of the page the driver looked at before. `text` is the page's
/// captured text, the text a quote is checked against; without it the observation's visible text goes instead.
pub(super) fn render(observation: &Observation, seen: &HashSet<String>, text: Option<&str>) -> String {
    render_view(observation, seen, text, None)
}

/// The lines of the page text a viewport window shows, as indexes into the text's lines, both included.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Window {
    pub first: usize,
    pub last: usize,
}

/// Where in `page_text(capture)` the viewport is, with the screen above and below it. A scroll moves the page by one
/// viewport, so the viewport and one screen each side hold what the driver reaches by one scroll, and the rest is
/// named and read by `read_text`. The browser reports which blocks it drew inside the viewport when it captured the
/// page, so the place is the span of those blocks' lines. A capture that reports none has no place, and the page goes
/// whole; a window that reaches both ends of the text is the whole page.
pub(super) fn window_of(capture: &Capture) -> Option<Window> {
    let page = crate::retrieval::text::Page::new(capture);
    let mut line = 0;
    let mut span: Option<(usize, usize)> = None;
    for block in
        capture.blocks.iter().filter(|block| !matches!(block.kind, BlockKind::Observation | BlockKind::DocumentTitle))
    {
        let lines = page.slice(block.start, block.end).split('\n').count();
        if block.in_viewport {
            span = Some((span.map_or(line, |(first, _)| first), line + lines - 1));
        }
        // Blocks are joined by a blank line.
        line += lines + 1;
    }
    let (first, last) = span?;
    let total = line - 1;
    let screen = last - first + 1;
    let window = Window { first: first.saturating_sub(screen), last: (last + screen).min(total - 1) };
    (window.first > 0 || window.last + 1 < total).then_some(window)
}

/// Whether a windowed page lists `control`: it is in the viewport or the screen each side, as the window's text is.
/// Listed by the viewport alone, a login form one screen down was named in the text with no ids to type into, and the
/// driver spent a turn on `read_controls` to find them (practice-login).
pub(super) fn in_window(control: &Control) -> bool {
    control.screen.abs() <= 1
}

/// `render`, with the page text and controls cut to `window` when one is given: the text and controls of the window,
/// and a line naming what is left out and the tools that read it.
pub(super) fn render_view(
    observation: &Observation,
    seen: &HashSet<String>,
    text: Option<&str>,
    window: Option<Window>,
) -> String {
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
            if window.is_some() && !in_window(control) {
                continue;
            }
            out += &line(control, !seen.is_empty() && !seen.contains(&control.id));
            out.push('\n');
        }
    }
    let outside = observation.controls.iter().filter(|control| !in_window(control)).count();
    // Told only how many were left out, the driver passed a control's id as `from` and was refused until it scrolled.
    if window.is_some() && outside > 0 {
        out += &format!(
            "{outside} more controls are further than a screen from the viewport: read_controls(from, count) lists \
             them in reading order, from the `from`th of them (1 to {outside}), and scroll brings them into view.\n"
        );
    }
    // The driver reads the text its quotes are checked against. Shown the visible text, a table came as one cell a
    // line with its icons left out, so the flag marking the Best Picture winner never reached it and every quote it
    // took of the row failed against the captured "| Argo | 7 | 3 | [flag icon] |".
    if let (Some(text), Some(window)) = (text, window) {
        let lines: Vec<&str> = text.lines().collect();
        out += &format!(
            "Page text, lines {} to {} of {}: the viewport and the screen above and below it:\n",
            window.first + 1,
            window.last + 1,
            lines.len()
        );
        out += &lines[window.first..=window.last].join("\n");
        let (above, below) = (window.first, lines.len() - 1 - window.last);
        let left = match (above, below) {
            (0, below) => format!("The last {below} lines are not shown"),
            (above, 0) => format!("The first {above} lines are not shown"),
            (above, below) => format!("The first {above} lines and the last {below} are not shown"),
        };
        out += &format!(
            "\n({left}: read_text(null, from, lines, null) reads any lines of the page, read_text(null, 1, lines, text) lists the lines holding text under their numbers, and scroll moves the viewport.)"
        );
    } else if let Some(text) = text {
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
///
/// Blocks are cut through one offset table: `Capture::block_text` walks the text from its start for each block, and a
/// fetched page of 30000 rows spent four minutes in this function before the driver's first turn.
pub(super) fn page_text(capture: &Capture) -> String {
    let page = crate::retrieval::text::Page::new(capture);
    let blocks =
        capture.blocks.iter().filter(|block| !matches!(block.kind, BlockKind::Observation | BlockKind::DocumentTitle));
    blocks
        .map(|block| match block.kind {
            BlockKind::Heading => format!("# {}", page.slice(block.start, block.end)),
            _ => page.slice(block.start, block.end).to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `quote` without the `# ` that `page_text` puts before a heading, as the page itself has it, or None when it has none.
/// The marking is code's own, so a quote copied with it is the page's words all the same: a run read "# 15.5.20. 421
/// Misdirected Request" off the page it was shown, quoted it so, and was refused four times with the section on screen.
pub(super) fn unmarked(quote: &str) -> Option<String> {
    let plain: Vec<&str> = quote.split('\n').map(|line| line.strip_prefix("# ").unwrap_or(line)).collect();
    let plain = plain.join("\n");
    (plain != quote).then_some(plain)
}

/// The evidence `quote` has in `capture`: the words as written, else as the page has them without code's heading marks.
pub(super) fn quoted_in(capture: &Capture, quote: &str) -> Option<fastbrowse_core::models::Evidence> {
    crate::retrieval::quoted(capture, quote)
        .or_else(|| unmarked(quote).and_then(|plain| crate::retrieval::quoted(capture, &plain)))
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
        // A disabled Save left out of the list read as no Save: the driver pressed Enter in a bio field to submit it
        // and typed a newline into the bio (#1561).
        (control.disabled, "disabled"),
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
    // The list's own pager: only a link the page marks is read_pages' to follow, so the model is told which ones.
    if fastbrowse_core::page::pager_link(control) {
        states.push("leads to the next page of a list".into());
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
    // A disabled control offers nothing, and its line says so by naming no operation.
    if !control.operations.is_empty() {
        let operations: Vec<String> = control.operations.iter().map(|operation| operation.to_string()).collect();
        out += &format!(" can {}", operations.join("/"));
    }
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

    /// `count` one-line blocks named by their number, the `flagged` ones drawn inside the viewport.
    fn capture_of(count: usize, flagged: &[usize], names: &[(usize, &str)]) -> Capture {
        let parts: Vec<String> = (0..count)
            .map(|n| {
                names.iter().find(|(at, _)| *at == n).map_or_else(|| format!("line {n}"), |(_, name)| name.to_string())
            })
            .collect();
        let parts: Vec<(BlockKind, &str)> = parts.iter().map(|part| (BlockKind::Paragraph, part.as_str())).collect();
        let mut capture = crate::retrieval::testing::capture(&parts);
        for at in flagged {
            capture.blocks[*at].in_viewport = true;
        }
        capture
    }

    #[test]
    fn the_window_is_the_viewport_and_one_screen_each_side() {
        // Blocks are joined by a blank line, so block n is line 2n: the viewport is lines 16 to 20.
        let window = window_of(&capture_of(20, &[8, 9, 10], &[])).unwrap();
        assert_eq!(window, Window { first: 11, last: 25 });
    }

    #[test]
    fn a_repeated_label_is_placed_by_the_browsers_report_not_by_its_text() {
        let capture = capture_of(60, &[30, 31, 32], &[(0, "Add"), (30, "Add")]);
        assert_eq!(window_of(&capture), Some(Window { first: 55, last: 69 }));
    }

    #[test]
    fn a_page_the_window_covers_or_cannot_place_goes_whole() {
        assert_eq!(window_of(&capture_of(3, &[0, 1, 2], &[])), None, "the window covers the page");
        assert_eq!(window_of(&capture_of(20, &[], &[])), None, "no block reports the viewport");
    }

    #[test]
    fn a_window_names_the_lines_and_controls_it_leaves_out() {
        let near = Control::new("1", "link", "Near", [Operation::Click]);
        let below = Control { offscreen: true, screen: 1, ..Control::new("3", "link", "Below", [Operation::Click]) };
        let far = Control { offscreen: true, screen: 2, ..Control::new("2", "link", "Far", [Operation::Click]) };
        let observation: Observation = serde_json::from_value(serde_json::json!({
            "url": "https://example.test/", "title": "t", "page_key": "k", "document_key": "d",
            "captured_at": "2026-11-18T00:00:00Z", "viewport_text": "line 9", "tabs": [],
            "controls": [near, below, far]
        }))
        .unwrap();
        let capture = capture_of(20, &[8], &[]);
        let text = page_text(&capture);
        let shown = render_view(&observation, &HashSet::new(), Some(&text), window_of(&capture));
        assert!(shown.contains("Page text, lines 16 to 18 of 39"), "{shown}");
        assert!(
            shown.contains("The first 15 lines and the last 21 are not shown: read_text(null, from, lines, null)"),
            "{shown}"
        );
        assert!(
            shown.contains("1 more controls are further than a screen from the viewport: read_controls(from, count)"),
            "{shown}"
        );
        // The screen below is one scroll away, as the window's text is, so its controls are listed with it.
        assert!(shown.contains("Near") && shown.contains("Below") && !shown.contains("Far"), "{shown}");
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
    fn a_quote_copied_with_a_headings_mark_is_the_pages_words() {
        let capture = crate::retrieval::testing::capture(&[
            (BlockKind::Heading, "15.5.20. 421 Misdirected Request"),
            (BlockKind::Paragraph, "The 421 status code indicates."),
        ]);
        let shown = page_text(&capture);
        assert!(shown.starts_with("# 15.5.20."), "{shown}");
        let evidence = quoted_in(&capture, "# 15.5.20. 421 Misdirected Request").expect("the heading as shown");
        assert_eq!(evidence.quote, "15.5.20. 421 Misdirected Request");
        assert!(quoted_in(&capture, "# 15.5.21. 422").is_none(), "words the page lacks stay refused");
        assert_eq!(unmarked("The 421 status\n# 15.5.20. x"), Some("The 421 status\n15.5.20. x".into()));
        assert_eq!(unmarked("a # b"), None);
    }

    #[test]
    fn a_links_fragment_is_shown() {
        let link = Control {
            href: Some("/rfc/rfc9110.html#section-15.5.20".into()),
            ..Control::new("1", "link", "421", [Operation::Click])
        };
        assert!(line(&link, false).ends_with("-> /rfc/rfc9110.html#section-15.5.20"));
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

    #[test]
    fn a_disabled_control_is_listed_as_disabled_offering_nothing() {
        let save = Control { disabled: Some(true), ..Control::new("5", "button", "Save", []) };
        assert_eq!(line(&save, false), r#"[5] button "Save" (disabled)"#);
    }
}
