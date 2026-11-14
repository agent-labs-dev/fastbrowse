//! A window's accessibility tree, as deskpilot's `elements` reports it, read as an `Observation` and a `Capture`.
//!
//! What a control is and what it can do come from the app's own AT-SPI roles and states, as a browser page's come
//! from its ARIA roles and HTML elements: nothing is inferred from labels, pixels or layout.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;

use fastbrowse_core::models::Operation;
use fastbrowse_core::page::{Block, BlockKind, Capture, Control, Observation, Tab};
use fastbrowse_core::py::datetime::DateTime;
use fastbrowse_core::py::{sha256_hex, text};
use fastbrowse_core::safety::origin_of;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Element {
    pub id: String,
    pub role: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub states: Vec<String>,
    #[serde(default)]
    pub text: Option<String>,
    /// Where the element's top left corner is, in window pixels.
    #[serde(default)]
    pub x: i64,
    #[serde(default)]
    pub y: i64,
    #[serde(default)]
    pub children: Vec<Element>,
    /// On the root only: the snapshot its ids belong to. A peek, which binds nothing, has none.
    #[serde(default)]
    pub snapshot: Option<String>,
}

impl Element {
    fn has(&self, state: &str) -> bool {
        self.states.iter().any(|s| s == state)
    }

    /// Whether the app says this element is on screen now. `visible` alone means only that it would be were its
    /// parent shown: the items of a closed menu are visible and not showing.
    fn showing(&self) -> bool {
        self.has("showing")
    }
}

/// The tree with each element's children in reading order: top to bottom, then left to right, by where the app says
/// each one is. A toolkit lists children in its own order, which need not be the one they are read in: GTK's grid lists
/// them last attached first, so a form read bottom up. A web page's order is its author's, which is reading order.
pub fn in_reading_order(mut element: Element) -> Element {
    element.children = element.children.into_iter().map(in_reading_order).collect();
    element.children.sort_by_key(|child| (child.y, child.x));
    element
}

/// The window an observation is of, as deskpilot's `windows` describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub address: String,
    pub class: String,
    pub title: String,
}

/// The characters of an app class kept as written in a `desktop://` address: those a host name may hold.
const CLASS_KEPT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'.').remove(b'-').remove(b'_');

impl Window {
    /// `desktop://<app class>/`: the app is the origin, so a secret, an allowed origin and the irreversible-action
    /// gate are scoped to one app as a website's are to one site. The title is the page's title, not its address:
    /// it changes as the app's state does.
    pub fn url(&self) -> String {
        format!("desktop://{}/", utf8_percent_encode(&self.class.to_lowercase(), CLASS_KEPT))
    }
}

/// Roles whose element does something when clicked, by AT-SPI's role names. at-spi2-core renamed `push button` to
/// `button`; an app on an older one still says the first.
const CLICKABLE: [&str; 17] = [
    "button",
    "push button",
    "toggle button",
    "check box",
    "radio button",
    "combo box",
    "menu",
    "menu item",
    "check menu item",
    "radio menu item",
    "link",
    "page tab",
    "list item",
    "tree item",
    "table cell",
    "icon",
    "push button menu",
];

/// Roles whose checked state is a value of their own, so an unchecked one says so rather than saying nothing.
const CHECKABLE: [&str; 5] = ["check box", "radio button", "toggle button", "check menu item", "radio menu item"];

fn operations(element: &Element) -> Vec<Operation> {
    // Editable is a state the app sets, on entries, spin buttons, password fields and editable combo boxes alike.
    if element.has("editable") {
        return vec![Operation::Click, Operation::Fill, Operation::Enter];
    }
    // A combo box's list opens as a popup whose items cannot be clicked in the background, so it is chosen in as a
    // web page's dropdown is, by naming an option, and the page picks it with the keyboard.
    if !options(element).is_empty() {
        return vec![Operation::Select];
    }
    if CLICKABLE.contains(&element.role.as_str()) {
        return vec![Operation::Click, Operation::Hover];
    }
    Vec::new()
}

/// The ARIA role an AT-SPI role is, by the W3C Core Accessibility API Mappings, which pair each ARIA role with the
/// AT-SPI role a browser exposes it as. Jev reads controls in ARIA's words on every page; a desktop control is shown
/// in the same ones. A role the mapping does not name keeps AT-SPI's.
fn aria_role(role: &str) -> &str {
    match role {
        "push button" | "toggle button" | "push button menu" => "button",
        "text" | "entry" | "password text" => "textbox",
        "check box" => "checkbox",
        "radio button" => "radio",
        "combo box" => "combobox",
        "menu item" => "menuitem",
        "check menu item" => "menuitemcheckbox",
        "radio menu item" => "menuitemradio",
        "page tab" => "tab",
        "list item" => "listitem",
        "tree item" => "treeitem",
        "table cell" => "cell",
        "spin button" => "spinbutton",
        other => other,
    }
}

/// The items of a combo box's list: the elements below it that the app marks selectable, each named.
fn items(element: &Element) -> Vec<&Element> {
    fn walk<'a>(element: &'a Element, out: &mut Vec<&'a Element>) {
        for child in &element.children {
            if child.has("selectable") && !child.name.is_empty() {
                out.push(child);
            } else {
                walk(child, out);
            }
        }
    }
    let mut out = Vec::new();
    if element.role == "combo box" {
        walk(element, &mut out);
    }
    out
}

/// The items a combo box offers to choose: those of its list the app has enabled, since the keys pass over the rest.
pub fn options(element: &Element) -> Vec<&Element> {
    items(element).into_iter().filter(|item| item.has("enabled")).collect()
}

/// The name of the item a combo box has chosen: the one of its options deskpilot marks selected.
pub fn chosen(element: &Element) -> Option<&str> {
    items(element).into_iter().find(|item| item.has("selected")).map(|item| item.name.as_str())
}

/// Whether a combo box's list is open on screen.
pub fn list_open(element: &Element) -> bool {
    items(element).iter().any(|item| item.showing())
}

/// The element with `id`, anywhere in the tree.
pub fn find<'a>(element: &'a Element, id: &str) -> Option<&'a Element> {
    if element.id == id {
        return Some(element);
    }
    element.children.iter().find_map(|child| find(child, id))
}

fn control(element: &Element, context: Option<&str>) -> Option<Control> {
    if !element.showing() || !element.has("enabled") {
        return None;
    }
    let operations = operations(element);
    if operations.is_empty() {
        return None;
    }
    let label = if element.name.is_empty() { element.text.clone().unwrap_or_default() } else { element.name.clone() };
    let mut control = Control::new(&element.id, aria_role(&element.role), &label, operations);
    let sensitive = element.role == "password text";
    control.sensitive = sensitive;
    if sensitive {
        control.input_type = Some("password".into());
    }
    if element.has("editable") && !sensitive {
        control.value = element.text.clone();
    } else if let Some(item) = chosen(element) {
        // A choice such as a combo box holds its value as the item its selection reports, which deskpilot marks
        // selected; the box's own name may be its label.
        control.value = Some(item.to_string());
    }
    control.options = options(element).iter().map(|item| item.name.clone()).collect();
    control.context = context.map(str::to_string);
    control.focused = Some(element.has("focused"));
    if CHECKABLE.contains(&element.role.as_str()) {
        control.checked = Some(element.has("checked") || element.has("pressed"));
    }
    if element.has("selectable") {
        control.selected = Some(element.has("selected"));
    }
    if element.has("expandable") {
        control.expanded = Some(element.has("expanded"));
    }
    if element.has("required") {
        control.required = Some(true);
    }
    if element.has("invalid entry") {
        control.invalid = Some(true);
    }
    Some(control)
}

/// Every control the window shows, in reading order. Its id is deskpilot's element number, which stays the same while
/// the tree before it keeps its shape, as a DOM node's id does while the node lives; `act` binds it to the snapshot of
/// the observation it was chosen from. A control's context is the name of the nearest named
/// element holding it below the window itself, as a group box or a page tab names what it holds.
pub fn controls(root: &Element) -> Vec<Control> {
    fn walk(element: &Element, context: Option<&str>, out: &mut Vec<Control>) {
        let made = control(element, context);
        // A control's own name is its label, not the context of what it holds.
        let inner = if element.name.is_empty() || made.is_some() { context } else { Some(element.name.as_str()) };
        out.extend(made);
        for child in &element.children {
            walk(child, inner, out);
        }
    }
    // The window's own name is its title, which every control shares, so it tells none apart.
    let mut out = Vec::new();
    out.extend(control(root, None));
    for child in &root.children {
        walk(child, None, &mut out);
    }
    out
}

/// The text an element shows of its own: an editable field's value is the control's, and a control's name is its
/// label, so only elements that are neither add text.
fn own_text(element: &Element) -> Option<&str> {
    if !operations(element).is_empty() {
        return None;
    }
    element.text.as_deref().filter(|text| !text.trim().is_empty())
}

/// The text the window shows on screen, and the text it holds out of view, each in the tree's order.
pub fn texts(root: &Element) -> (String, String) {
    fn walk<'a>(element: &'a Element, shown: &mut Vec<&'a str>, hidden: &mut Vec<&'a str>) {
        if let Some(text) = own_text(element) {
            if element.showing() { shown.push(text) } else { hidden.push(text) }
        }
        for child in &element.children {
            walk(child, shown, hidden);
        }
    }
    let (mut shown, mut hidden) = (Vec::new(), Vec::new());
    walk(root, &mut shown, &mut hidden);
    (shown.join("\n"), hidden.join("\n"))
}

/// The window as an observation. `page_key` is the snapshot, which deskpilot refuses ids from once a newer one
/// replaces it; a peek's observation has no snapshot and so can never be acted on.
pub fn observation(window: &Window, root: &Element) -> Observation {
    let snapshot = root.snapshot.clone().unwrap_or_default();
    let url = window.url();
    let (viewport_text, offscreen_text) = texts(root);
    // Every control is in the app's one origin, the scope a secret is declared for and checked against when typed.
    let origin = origin_of(&url);
    let mut controls = controls(root);
    for control in &mut controls {
        control.frame_origin = Some(origin.clone());
    }
    Observation {
        url: url.clone(),
        title: window.title.clone(),
        page_key: snapshot.clone(),
        captured_at: DateTime::now_utc(),
        response_status: None,
        // One window is one document for as long as it is open: typing into it or its title changing is not leaving.
        document_key: window.address.clone(),
        controls,
        viewport_text,
        offscreen_text,
        visual_content: false,
        modal: None,
        media: Vec::new(),
        tabs: vec![Tab { id: window.address.clone(), url, title: window.title.clone(), active: true, opener_id: None }],
        dialog: None,
        inaccessible_frames: 0,
        can_go_back: false,
        history_entry: None,
    }
}

fn kind(role: &str) -> BlockKind {
    match role {
        "heading" => BlockKind::Heading,
        "list item" => BlockKind::ListItem,
        "table" | "table cell" => BlockKind::Table,
        "link" => BlockKind::Link,
        _ => BlockKind::Paragraph,
    }
}

/// Every text the window holds, shown or not, as a capture whose blocks a quote is checked against.
pub fn capture(window: &Window, root: &Element) -> Capture {
    fn walk<'a>(element: &'a Element, out: &mut Vec<(&'a str, &'a str)>) {
        // A combo box says the item it has chosen, as it shows on screen; the items of its list are its options, not
        // what the window says.
        if element.role == "combo box" {
            if let Some(item) = chosen(element) {
                out.push((element.role.as_str(), item));
            }
            return;
        }
        let text = element.text.as_deref().filter(|t| !t.trim().is_empty());
        // A control's text is read too: a field's value or a button's words are part of what the window says.
        let named = (!operations(element).is_empty() && !element.name.is_empty()).then_some(element.name.as_str());
        if let Some(text) = text.or(named)
            && element.role != "password text"
        {
            out.push((element.role.as_str(), text));
        }
        for child in &element.children {
            walk(child, out);
        }
    }
    let mut found = Vec::new();
    walk(root, &mut found);
    let mut whole = String::new();
    let mut blocks = Vec::with_capacity(found.len());
    let mut offset = 0;
    for (role, block) in found {
        let start = offset;
        offset += text::len(block);
        whole.push_str(block);
        whole.push_str("\n\n");
        blocks.push(Block {
            source_id: format!("desktop/:{}", blocks.len()),
            source_url: None,
            complete_source: false,
            kind: kind(role),
            frame_id: None,
            start: start as i64,
            end: offset as i64,
            heading_path: Vec::new(),
            control_context: None,
            href: None,
            links: Vec::new(),
            whole_record: false,
        });
        offset += 2;
    }
    Capture {
        url: window.url(),
        title: window.title.clone(),
        captured_at: DateTime::now_utc(),
        sha256: sha256_hex(whole.as_bytes()),
        text: whole,
        blocks,
        inaccessible_frames: 0,
        masked: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn node(id: &str, role: &str, name: &str, states: &[&str], text: Option<&str>, children: Vec<Element>) -> Element {
        Element {
            id: id.into(),
            role: role.into(),
            name: name.into(),
            states: states.iter().map(|s| s.to_string()).collect(),
            text: text.map(str::to_string),
            x: 0,
            y: 0,
            children,
            snapshot: None,
        }
    }

    const SHOWN: [&str; 4] = ["enabled", "sensitive", "showing", "visible"];

    fn shown(extra: &[&'static str]) -> Vec<&'static str> {
        [&SHOWN[..], extra].concat()
    }

    /// The order form deskpilot read from GTK: a frame holding a named group, its fields, a closed combo box menu and
    /// a password field.
    fn form() -> (Window, Element) {
        let menu = node(
            "e5",
            "menu",
            "",
            &["enabled", "sensitive"],
            None,
            vec![
                node("e6", "menu item", "Ireland", &["enabled", "selectable", "visible", "selected"], None, vec![]),
                node("e20", "menu item", "France", &["enabled", "selectable", "visible"], None, vec![]),
                node("e21", "menu item", "Germany", &["selectable", "visible"], None, vec![]),
            ],
        );
        let group = node(
            "e2",
            "panel",
            "Delivery",
            &SHOWN,
            None,
            vec![
                node("e3", "label", "Not submitted", &SHOWN, Some("Not submitted"), vec![]),
                node("e4", "combo box", "Country", &SHOWN, None, vec![menu]),
                node("e7", "text", "Name", &shown(&["editable", "focusable", "focused"]), Some("Ada"), vec![]),
                node("e8", "check box", "Gift wrap", &shown(&["focusable"]), None, vec![]),
                node("e9", "radio button", "Small", &shown(&["checked", "focusable"]), None, vec![]),
                node("e10", "button", "Place order", &shown(&["focusable"]), None, vec![]),
                node("e11", "push button", "Disabled", &["showing", "visible"], None, vec![]),
                node("e12", "password text", "Password", &shown(&["editable"]), Some("hunter2"), vec![]),
                node("e13", "label", "Scrolled away", &["enabled", "visible"], Some("Scrolled away"), vec![]),
            ],
        );
        let mut root = node("e1", "frame", "Order form", &SHOWN, None, vec![group]);
        root.snapshot = Some("abc-1".into());
        let window = Window { address: "0xabc".into(), class: "dptest-Order".into(), title: "Order form".into() };
        (window, root)
    }

    #[test]
    fn a_window_reads_as_its_shown_enabled_controls_with_the_apps_own_states() {
        let (window, root) = form();
        let observation = observation(&window, &root);
        assert_eq!(observation.url, "desktop://dptest-order/");
        assert_eq!(observation.page_key, "abc-1");
        // The combo box's value is the item its selection reports, not its name, which is its label.
        let country = observation.controls.iter().find(|c| c.role == "combobox").unwrap();
        assert_eq!((country.label.as_str(), country.value.as_deref()), ("Country", Some("Ireland")));
        // A combo box is chosen in by naming one of its items, as a web page's dropdown is; a disabled one is not offered.
        assert_eq!(country.options, ["Ireland", "France"]);
        let origins: Vec<_> = observation.controls.iter().map(|c| c.frame_origin.as_deref()).collect();
        assert!(origins.iter().all(|&o| o == Some("desktop://dptest-order")), "{origins:?}");
        let summary: Vec<(&str, &str, Vec<&str>)> = observation
            .controls
            .iter()
            .map(|c| (c.id.as_str(), c.label.as_str(), c.operations.iter().map(|o| o.as_str()).collect()))
            .collect();
        // The closed menu's items are visible and not showing, and the disabled button is not enabled: none is one.
        assert_eq!(
            summary,
            [
                ("e4", "Country", vec!["select"]),
                ("e7", "Name", vec!["click", "enter", "fill"]),
                ("e8", "Gift wrap", vec!["click", "hover"]),
                ("e9", "Small", vec!["click", "hover"]),
                ("e10", "Place order", vec!["click", "hover"]),
                ("e12", "Password", vec!["click", "enter", "fill"]),
            ]
        );
        let roles: Vec<&str> = observation.controls.iter().map(|c| c.role.as_str()).collect();
        assert_eq!(roles, ["combobox", "textbox", "checkbox", "radio", "button", "textbox"]);
        let control = |id: &str| observation.controls.iter().find(|c| c.id == id).unwrap();
        assert_eq!(control("e7").value.as_deref(), Some("Ada"));
        assert_eq!(control("e7").focused, Some(true));
        assert_eq!(control("e7").context.as_deref(), Some("Delivery"));
        assert_eq!(control("e8").checked, Some(false));
        assert_eq!(control("e9").checked, Some(true));
        assert_eq!(control("e10").checked, None);
        let password = control("e12");
        assert!(password.sensitive && password.value.is_none());
        assert_eq!(password.input_type.as_deref(), Some("password"));
        assert_eq!(observation.viewport_text, "Not submitted");
        assert_eq!(observation.offscreen_text, "Scrolled away");
    }

    #[test]
    fn a_peek_binds_no_snapshot_and_a_capture_never_holds_a_password() {
        let (window, mut root) = form();
        root.snapshot = None;
        let peeked = observation(&window, &root);
        assert_eq!(peeked.page_key, "");
        let capture = capture(&window, &root);
        assert!(capture.text.contains("Scrolled away") && capture.text.contains("Place order"));
        assert!(!capture.text.contains("hunter2"));
        // A combo box reads as its chosen item, never as the options of its closed list.
        assert!(capture.text.contains("Ireland") && !capture.text.contains("France"));
        for block in &capture.blocks {
            assert!(!capture.block_text(block).is_empty());
        }
    }

    #[test]
    fn children_read_top_to_bottom_then_left_to_right() {
        let at = |id: &str, x, y| Element { x, y, ..node(id, "label", id, &SHOWN, Some(id), vec![]) };
        // GTK's grid lists a form's rows last attached first.
        let grid = node(
            "e1",
            "panel",
            "",
            &SHOWN,
            None,
            vec![at("submit", 80, 300), at("email", 80, 40), at("name", 80, 0), at("label", 0, 0)],
        );
        let read = in_reading_order(grid);
        let order: Vec<&str> = read.children.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, ["label", "name", "email", "submit"]);
    }

    #[test]
    fn deskpilots_tree_parses_as_it_sends_it() {
        let sent = json!({
            "id": "e1", "role": "frame", "name": "Order form", "states": ["showing"], "text": null,
            "x": 0, "y": 0, "width": 10, "height": 10, "children": [], "snapshot": "s-1", "truncated": true,
        });
        let root: Element = serde_json::from_value(sent).unwrap();
        assert_eq!(root.snapshot.as_deref(), Some("s-1"));
    }
}
