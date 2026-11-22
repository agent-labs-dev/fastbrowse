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
    pub width: i64,
    #[serde(default)]
    pub height: i64,
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
    ///
    /// Qt leaves the items of a closed combo box showing, moved to x=-10000, and they were offered as options to pick
    /// (#1408): a box that ends before the window's left or top edge lies outside the window, whatever the state says.
    /// A box with no size says nothing of where it is, as GTK 4's list cells have none until the window is first drawn.
    /// One that starts past the right or bottom edge of `window`, the window's size, is outside it too: a tiled window
    /// smaller than its content leaves the app reporting elements beyond it (#1395). A window of no size says nothing.
    fn showing(&self, window: Size) -> bool {
        let sized = self.width > 0 && self.height > 0;
        let before = self.x + self.width <= 0 || self.y + self.height <= 0;
        let after = window.known() && (self.x >= window.width || self.y >= window.height);
        self.has("showing") && !(sized && (before || after))
    }

    /// Whether the point an action on this element aims at, its centre, lies inside `window`: deskpilot refuses a
    /// point outside the window, so an element whose centre is beyond its edge cannot be clicked though part of it shows.
    fn reachable(&self, window: Size) -> bool {
        let inside = |at: i64, size: i64, edge: i64| (0..edge).contains(&(at + size / 2));
        !window.known()
            || self.width <= 0
            || self.height <= 0
            || (inside(self.x, self.width, window.width) && inside(self.y, self.height, window.height))
    }

    /// Whether the app accepts input on this element. GTK 3 and Qt set both `enabled` and `sensitive` on a usable
    /// control, while GTK 4's AT-SPI bridge sets only `sensitive`; a disabled control carries neither.
    fn operable(&self) -> bool {
        self.has("enabled") || self.has("sensitive")
    }
}

/// The size of the window a tree is read from, in the window pixels its elements are placed in, as its root says.
#[derive(Debug, Clone, Copy)]
pub struct Size {
    width: i64,
    height: i64,
}

impl Size {
    pub fn of(root: &Element) -> Size {
        Size { width: root.width, height: root.height }
    }

    fn known(self) -> bool {
        self.width > 0 && self.height > 0
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
    /// `desktop://<app class>/<window address>`: the app is the origin, so a secret, an allowed origin and the
    /// irreversible-action gate are scoped to one app as a website's are to one site. The window is the path, so what
    /// was read in one window is not retired by a read of another: with one address for every window of an app, a
    /// read of a dialog marked the main window's facts superseded (#1343). The title is the page's title, not its
    /// address: it changes as the app's state does.
    pub fn url(&self) -> String {
        let class = utf8_percent_encode(&self.class.to_lowercase(), CLASS_KEPT).to_string();
        format!("desktop://{class}/{}", utf8_percent_encode(&self.address, CLASS_KEPT))
    }
}

/// Roles whose element does something when clicked, by the role names the app reports over AT-SPI. at-spi2-core
/// renamed `push button` to `button`; an app on an older one still says the first. A toolkit can name a role its own
/// way: GTK 4 says `tab`, `switch`, `grid cell` and `menu item checkbox` where at-spi2-core says `page tab` or
/// `check menu item`, and Qt 5 says `spin box`. A role missing here is never offered, so GNOME Text Editor's tabs,
/// its line numbers switch and GNOME Files' file icons could not be picked (#1397).
const CLICKABLE: &[&str] = &[
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
    "tab",
    "switch",
    "grid cell",
    "menu item checkbox",
    "menu item radio",
    "spin button",
    "spin box",
];

/// Roles whose checked state is a value of their own, so an unchecked one says so rather than saying nothing.
const CHECKABLE: &[&str] = &[
    "check box",
    "radio button",
    "toggle button",
    "check menu item",
    "radio menu item",
    "switch",
    "menu item checkbox",
    "menu item radio",
];

/// Whether an element's checked or pressed state is a value it holds. A toggle button that opens a menu or popover, as
/// GTK's menu buttons do, is pressed while its popup is open, which is whether it is expanded: read as a checked value,
/// a click on GNOME Text Editor's Main Menu was refused as a setting the task did not ask to change (#1393).
///
/// An element of another role holds one when the app marks it checkable, as Qt does a table cell that ticks its row
/// (#1408).
fn holds_checked(element: &Element) -> bool {
    (CHECKABLE.contains(&element.role.as_str()) || element.has("checkable"))
        && !element.has("has popup")
        && !element.has("expandable")
}

/// The role a control is offered as: its own in ARIA's words, or a checkbox for an element of another role that holds a
/// checked value, since that is what clicking it changes.
fn offered_role(element: &Element) -> &str {
    if holds_checked(element) && !CHECKABLE.contains(&element.role.as_str()) {
        return "checkbox";
    }
    aria_role(&element.role)
}

/// What names a control with no name of its own, from the element before it among `siblings`, at `at` in reading
/// order. A label read just before an unnamed field names it, as a sighted reader pairs them: Qt's Add Entry dialog
/// lays out "Name:" beside an unnamed text field, and each field was offered as `textbox`, so the run typed the value
/// into the name field (#1408). A table cell is named by the first named cell of its row, the cells beside it at its
/// height, since the tree holds no row element: a checkbox cell offered as `cell` did not say which option it sets.
fn sibling_label(siblings: &[Element], at: usize) -> Option<&str> {
    let element = &siblings[at];
    const CELLS: [&str; 2] = ["table cell", "grid cell"];
    if CELLS.contains(&element.role.as_str()) {
        return siblings[..at]
            .iter()
            .filter(|sibling| CELLS.contains(&sibling.role.as_str()) && sibling.y == element.y)
            .find_map(|sibling| Some(sibling.name.as_str()).filter(|name| !name.trim().is_empty()));
    }
    let before = siblings[..at].last().filter(|before| before.role == "label")?;
    Some(before.name.as_str())
        .filter(|name| !name.trim().is_empty())
        .or(before.text.as_deref().filter(|text| !text.trim().is_empty()))
}

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
        "text" | "text box" | "entry" | "password text" => "textbox",
        "check box" => "checkbox",
        "radio button" => "radio",
        "combo box" => "combobox",
        "menu item" => "menuitem",
        "check menu item" | "menu item checkbox" => "menuitemcheckbox",
        "radio menu item" | "menu item radio" => "menuitemradio",
        "page tab" => "tab",
        "grid cell" => "gridcell",
        "list item" => "listitem",
        "tree item" => "treeitem",
        "table cell" => "cell",
        "spin button" | "spin box" => "spinbutton",
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
    items(element).into_iter().filter(|item| item.operable()).collect()
}

/// The name of the item a combo box has chosen: the one of its options deskpilot marks selected.
pub fn chosen(element: &Element) -> Option<&str> {
    items(element).into_iter().find(|item| item.has("selected")).map(|item| item.name.as_str())
}

/// Whether a combo box's list is open on screen, in a window of size `window`.
pub fn list_open(element: &Element, window: Size) -> bool {
    items(element).iter().any(|item| item.showing(window))
}

/// The element with `id`, anywhere in the tree.
/// Where an action on element `id` lands, as the centre deskpilot aims at, with the size of the window `root` is:
/// window pixels, for a recording to draw its cursor there. None for an element with no size, which deskpilot cannot
/// place, or a root that does not say how big the window is.
pub fn aim(root: &Element, id: &str) -> Option<(f64, f64, f64, f64)> {
    let element = find(root, id)?;
    if element.width <= 0 || element.height <= 0 || root.width <= 0 || root.height <= 0 {
        return None;
    }
    let centre = |at: i64, size: i64| at as f64 + size as f64 / 2.0;
    Some((centre(element.x, element.width), centre(element.y, element.height), root.width as f64, root.height as f64))
}

pub fn find<'a>(element: &'a Element, id: &str) -> Option<&'a Element> {
    if element.id == id {
        return Some(element);
    }
    element.children.iter().find_map(|child| find(child, id))
}

fn control(element: &Element, window: Size, context: Option<&str>, beside: Option<&str>) -> Option<Control> {
    if !element.showing(window) || !element.reachable(window) || !element.operable() {
        return None;
    }
    let operations = operations(element);
    if operations.is_empty() {
        return None;
    }
    let role = offered_role(element);
    // A control with no name is offered under its words, the label beside it, or its role, as a page offers one. An
    // editable's text is its value, not its label: GNOME Text Editor's unnamed text view, labelled by what it held, was
    // offered as a field named "charger" holding "charger", and labelled '' when empty, Jev copied the task's answer
    // into it.
    let label = match element.text.as_deref() {
        _ if !element.name.is_empty() => element.name.clone(),
        Some(text) if !text.trim().is_empty() && !element.has("editable") => text.to_string(),
        _ => beside.unwrap_or(role).to_string(),
    };
    let mut control = Control::new(&element.id, role, &label, operations);
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
    if holds_checked(element) {
        control.checked = Some(element.has("checked") || element.has("pressed"));
    }
    if element.has("selectable") {
        control.selected = Some(element.has("selected"));
    }
    if element.has("expandable") {
        control.expanded = Some(element.has("expanded"));
    } else if element.has("has popup") {
        control.expanded = Some(element.has("pressed"));
    }
    if element.has("required") {
        control.required = Some(true);
    }
    if element.has("invalid entry") {
        control.invalid = Some(true);
    }
    Some(control)
}

/// Roles of an element that asks the person something, by the names AT-SPI, Qt and GTK 4 report: a Qt message box is an
/// `alert message`, GTK 4's an `alert dialog`.
const DIALOGS: [&str; 4] = ["alert", "alert message", "alert dialog", "dialog"];

/// What a dialog asks, as the context of the buttons that answer it: its title and the text it shows. A Yes button
/// judged by its name alone could not be told to answer "Are you sure you want to delete the cache?", so a run asked
/// to delete the cache ended `needs_confirmation` with the task judged not to ask for that click (#1404). None for an
/// element that is no dialog, or one that says nothing.
fn question(element: &Element, window: Size) -> Option<String> {
    if !DIALOGS.contains(&element.role.as_str()) {
        return None;
    }
    let (shown, _) = texts_in(element, window);
    let shown = shown.replace('\n', " ");
    match (element.name.trim(), shown.as_str()) {
        ("", "") => None,
        (title, "") => Some(title.to_string()),
        ("", said) => Some(said.to_string()),
        (title, said) => Some(format!("{title}: {said}")),
    }
}

/// Every control the window shows, in reading order. Its id is deskpilot's element number, which stays the same while
/// the tree before it keeps its shape, as a DOM node's id does while the node lives; `act` binds it to the snapshot of
/// the observation it was chosen from. A control's context is the name of the nearest named
/// element holding it below the window itself, as a group box or a page tab names what it holds, or what the dialog
/// holding it asks.
pub fn controls(root: &Element) -> Vec<Control> {
    fn walk(element: &Element, window: Size, context: Option<&str>, beside: Option<&str>, out: &mut Vec<Control>) {
        let made = control(element, window, context, beside);
        // A control's own name is its label, not the context of what it holds.
        let asked = question(element, window);
        let inner = match asked.as_deref() {
            Some(asked) => Some(asked),
            None if element.name.is_empty() || made.is_some() => context,
            None => Some(element.name.as_str()),
        };
        out.extend(made);
        for (at, child) in element.children.iter().enumerate() {
            walk(child, window, inner, sibling_label(&element.children, at), out);
        }
    }
    // The window's own name is its title, which every control shares, so it tells none apart; a dialog's is part of
    // what it asks.
    let window = Size::of(root);
    let mut out = Vec::new();
    out.extend(control(root, window, None, None));
    let asked = question(root, window);
    for (at, child) in root.children.iter().enumerate() {
        walk(child, window, asked.as_deref(), sibling_label(&root.children, at), &mut out);
    }
    out
}

/// The text an element shows of its own: an editable field's value is the control's, and a control's name is its
/// label, so only elements that are neither add text. A multi-line text view is the exception: what it holds is the
/// document the window shows, as GNOME Text Editor's body is, and left to its control's value alone the page read as
/// its title bar and status line while the run was asked about the document (#1398). A password field never adds its.
fn own_text(element: &Element) -> Option<&str> {
    let document = element.has("editable") && element.has("multi line") && element.role != "password text";
    if !operations(element).is_empty() && !document {
        return None;
    }
    element.text.as_deref().filter(|text| !text.trim().is_empty())
}

/// The text the window shows on screen, and the text it holds out of view, each in the tree's order.
pub fn texts(root: &Element) -> (String, String) {
    texts_in(root, Size::of(root))
}

/// The text `element` shows and holds out of view, in a window of size `window`.
fn texts_in(element: &Element, window: Size) -> (String, String) {
    fn walk<'a>(element: &'a Element, window: Size, shown: &mut Vec<&'a str>, hidden: &mut Vec<&'a str>) {
        if let Some(text) = own_text(element) {
            if element.showing(window) { shown.push(text) } else { hidden.push(text) }
        }
        for child in &element.children {
            walk(child, window, shown, hidden);
        }
    }
    let (mut shown, mut hidden) = (Vec::new(), Vec::new());
    walk(element, window, &mut shown, &mut hidden);
    (shown.join("\n"), hidden.join("\n"))
}

/// The window as an observation. `page_key` is the snapshot, which deskpilot refuses ids from once a newer one
/// replaces it; a peek's observation has no snapshot and so can never be acted on. Every window of the run is a tab,
/// as a browser's tabs are, with this one active, so the model moves between apps and dialogs as it moves between
/// tabs.
pub fn observation(window: &Window, root: &Element, windows: &[Window]) -> Observation {
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
        tabs: tabs(window, windows),
        dialog: None,
        inaccessible_frames: 0,
        can_go_back: false,
        history_entry: None,
    }
}

/// `windows` as tabs, with `active` among them however the list was read.
pub fn tabs(active: &Window, windows: &[Window]) -> Vec<Tab> {
    let listed = windows.iter().any(|window| window.address == active.address);
    let windows = if listed { windows.to_vec() } else { [windows, std::slice::from_ref(active)].concat() };
    windows
        .iter()
        .map(|window| {
            let current = window.address == active.address;
            // The active window as just read, so its tab's title is the observation's.
            let window = if current { active } else { window };
            Tab {
                id: window.address.clone(),
                url: window.url(),
                title: window.title.clone(),
                active: current,
                opener_id: None,
            }
        })
        .collect()
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
            fields: Vec::new(),
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
            width: 0,
            height: 0,
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
    fn every_window_of_the_run_is_a_tab_with_the_one_read_active() {
        let (window, root) = form();
        let dialog = Window { address: "0xdef".into(), class: "dptest-Order".into(), title: "Confirm".into() };
        let stale = Window { title: "Order form (old title)".into(), ..window.clone() };
        let observation = observation(&window, &root, &[stale, dialog]);
        let tabs: Vec<_> =
            observation.tabs.iter().map(|tab| (tab.id.as_str(), tab.title.as_str(), tab.active)).collect();
        assert_eq!(tabs, [("0xabc", "Order form", true), ("0xdef", "Confirm", false)]);
        assert_eq!(observation.tabs[1].url, "desktop://dptest-order/0xdef");
        // A window list read before the window opened still has it.
        let alone = observation_tabs(&window, &[]);
        assert_eq!(alone, [("0xabc".to_string(), true)]);
    }

    fn observation_tabs(window: &Window, windows: &[Window]) -> Vec<(String, bool)> {
        tabs(window, windows).into_iter().map(|tab| (tab.id, tab.active)).collect()
    }

    #[test]
    fn a_window_reads_as_its_shown_enabled_controls_with_the_apps_own_states() {
        let (window, root) = form();
        let observation = observation(&window, &root, &[]);
        assert_eq!(observation.url, "desktop://dptest-order/0xabc");
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
    fn tabs_switches_grid_cells_and_spin_boxes_are_offered_in_aria_words() {
        // As deskpilot read GNOME Text Editor's tab bar and Preferences, GNOME Files' icon view and a Qt 5 spin box
        // (#1396, #1397): the tab was never offered, only its Close Tab button, so the run closed the tab it meant.
        let gtk4 = |extra: &[&'static str]| -> Vec<&'static str> {
            shown(&["focusable"]).into_iter().chain(extra.iter().copied()).collect()
        };
        let root = node(
            "e1",
            "window",
            "contacts.txt - Text Editor",
            &SHOWN,
            None,
            vec![
                node("e2", "tab", "contacts.txt", &gtk4(&["selectable", "selected"]), None, vec![]),
                node("e3", "button", "Close Tab", &gtk4(&[]), None, vec![]),
                node("e4", "tab", "form.txt", &gtk4(&["selectable"]), None, vec![]),
                node("e5", "button", "Close Tab", &gtk4(&[]), None, vec![]),
                node("e6", "switch", "Display Line Numbers", &gtk4(&["checkable"]), None, vec![]),
                node("e7", "grid cell", "IMG_4471.txt. File", &gtk4(&["selectable"]), None, vec![]),
                node("e8", "spin box", "Double click interval", &gtk4(&[]), None, vec![]),
                node("e9", "menu item radio", "Light", &gtk4(&["checkable", "checked"]), None, vec![]),
            ],
        );
        let window = Window { address: "0xabc".into(), class: "org.gnome.TextEditor".into(), title: "x".into() };
        let controls = observation(&window, &root, &[]).controls;
        let summary: Vec<(&str, &str, Option<bool>)> =
            controls.iter().map(|c| (c.role.as_str(), c.label.as_str(), c.checked)).collect();
        assert_eq!(
            summary,
            [
                ("tab", "contacts.txt", None),
                ("button", "Close Tab", None),
                ("tab", "form.txt", None),
                ("button", "Close Tab", None),
                ("switch", "Display Line Numbers", Some(false)),
                ("gridcell", "IMG_4471.txt. File", None),
                ("spinbutton", "Double click interval", None),
                ("menuitemradio", "Light", Some(true)),
            ]
        );
        assert_eq!(controls[0].selected, Some(true));
        assert!(controls.iter().all(|c| c.operations.contains(&Operation::Click)));
        // What the window says names them too, so a quote of a file name is found in it.
        assert!(capture(&window, &root).text.contains("IMG_4471.txt. File"));
    }

    #[test]
    fn a_toggle_button_that_opens_a_menu_holds_no_checked_value() {
        // GNOME Text Editor's Main Menu is a toggle button with a popup; read as checked, a click on it was refused as a
        // setting change under a task that asks for some edits and rules out the rest (#1393).
        let root = node(
            "e1",
            "window",
            "Text Editor",
            &SHOWN,
            None,
            vec![
                node(
                    "e2",
                    "toggle button",
                    "Main Menu",
                    &shown(&["focusable", "has popup", "checkable"]),
                    None,
                    vec![],
                ),
                node("e3", "toggle button", "Bold", &shown(&["focusable", "checkable", "pressed"]), None, vec![]),
            ],
        );
        let window = Window { address: "0xabc".into(), class: "org.gnome.TextEditor".into(), title: "x".into() };
        let controls = observation(&window, &root, &[]).controls;
        let state: Vec<_> = controls.iter().map(|c| (c.label.as_str(), c.checked, c.expanded)).collect();
        assert_eq!(state, [("Main Menu", None, Some(false)), ("Bold", Some(true), None)]);
        let edits: Vec<bool> =
            controls.iter().map(|c| fastbrowse_core::safety::changes_a_value(Operation::Click, c)).collect();
        assert_eq!(edits, [false, true], "opening the menu changes no value; pressing Bold does");
    }

    #[test]
    fn a_qt_form_names_fields_by_their_labels_and_ticks_cells_and_drops_closed_combo_items() {
        // As deskpilot read CMake GUI's Add Entry dialog and cache table (#1408).
        let at = |element: Element, x, y| Element { x, y, width: 100, height: 20, ..element };
        let editable = shown(&["editable", "focusable"]);
        let closed = node(
            "e20",
            "list",
            "",
            &SHOWN,
            None,
            vec![
                at(node("e21", "list item", "BOOL", &shown(&["selectable", "selected"]), None, vec![]), -10000, -10000),
                at(node("e22", "list item", "STRING", &shown(&["selectable"]), None, vec![]), -10000, -9980),
            ],
        );
        let dialog = node(
            "e2",
            "panel",
            "",
            &SHOWN,
            None,
            vec![
                at(node("e3", "label", "Name:", &SHOWN, Some("Name:"), vec![]), 0, 0),
                at(node("e4", "text", "", &editable, Some(""), vec![]), 100, 0),
                at(node("e5", "label", "Type:", &SHOWN, Some("Type:"), vec![]), 0, 30),
                at(node("e6", "combo box", "", &shown(&["focusable"]), None, vec![closed]), 100, 30),
                at(node("e7", "label", "Value:", &SHOWN, Some("Value:"), vec![]), 0, 60),
                at(node("e8", "text", "", &editable, Some(""), vec![]), 100, 60),
            ],
        );
        let table = node(
            "e9",
            "table",
            "",
            &SHOWN,
            None,
            vec![
                at(node("e10", "table cell", "ENABLE_LOGS", &shown(&["selectable"]), None, vec![]), 0, 100),
                at(node("e11", "table cell", "", &shown(&["selectable", "checkable"]), None, vec![]), 100, 100),
                at(node("e12", "table cell", "GREETING", &shown(&["selectable"]), None, vec![]), 0, 120),
                at(
                    node("e13", "table cell", "", &shown(&["selectable", "checkable", "checked"]), None, vec![]),
                    100,
                    120,
                ),
            ],
        );
        let root = Element {
            width: 800,
            height: 600,
            ..node("e1", "dialog", "Add Cache Entry", &SHOWN, None, vec![dialog, table])
        };
        let window = Window { address: "0xabc".into(), class: "cmake-gui".into(), title: "CMake".into() };
        let controls = observation(&window, &root, &[]).controls;
        let summary: Vec<_> =
            controls.iter().map(|c| (c.id.as_str(), c.role.as_str(), c.label.as_str(), c.checked)).collect();
        assert_eq!(
            summary,
            [
                ("e4", "textbox", "Name:", None),
                ("e6", "combobox", "Type:", None),
                ("e8", "textbox", "Value:", None),
                ("e10", "cell", "ENABLE_LOGS", None),
                ("e11", "checkbox", "ENABLE_LOGS", Some(false)),
                ("e12", "cell", "GREETING", None),
                ("e13", "checkbox", "GREETING", Some(true)),
            ]
        );
        // The closed combo box's list is off the window, so it is not open, though Qt says its items are showing.
        let combo = find(&root, "e6").unwrap();
        assert!(!list_open(combo, Size::of(&root)));
        assert_eq!(chosen(combo), Some("BOOL"));
    }

    #[test]
    fn a_dialogs_buttons_carry_its_question() {
        // As deskpilot read CMake GUI's Delete Cache message box (#1404): the gate judged 'Yes' by its name alone.
        let root = node(
            "e1",
            "alert message",
            "Delete Cache",
            &SHOWN,
            None,
            vec![
                node(
                    "e2",
                    "label",
                    "Are you sure you want to delete the cache?",
                    &SHOWN,
                    Some("Are you sure you want to delete the cache?"),
                    vec![],
                ),
                node("e3", "push button", "Yes", &SHOWN, None, vec![]),
                node("e4", "push button", "No", &SHOWN, None, vec![]),
            ],
        );
        let window = Window { address: "0xabc".into(), class: "cmake-gui".into(), title: "Delete Cache".into() };
        let controls = observation(&window, &root, &[]).controls;
        let asked = "Delete Cache: Are you sure you want to delete the cache?";
        let contexts: Vec<_> = controls.iter().map(|c| (c.label.as_str(), c.context.as_deref())).collect();
        assert_eq!(contexts, [("Yes", Some(asked)), ("No", Some(asked))]);
        // The authorization gate's questions say what the button answers.
        let action = fastbrowse_core::safety::control_action(Operation::Click, &controls[0], None, &controls);
        assert!(action.contains(asked), "{action}");
        // A main window's text is no control's context: only a dialog asks something its buttons answer.
        let (_, form) = form();
        assert!(super::controls(&form).iter().all(|c| c.context.as_deref().is_none_or(|c| c == "Delivery")));
    }

    #[test]
    fn a_gtk4_control_is_operable_by_its_sensitive_state_alone() {
        // GTK 4's AT-SPI bridge reports a usable control as `sensitive` and never sets `enabled`, as GNOME Text
        // Editor's buttons and text view do; a disabled one carries neither.
        fn gtk4(extra: &[&'static str]) -> Vec<&'static str> {
            ["sensitive", "showing", "visible"].iter().chain(extra).copied().collect()
        }
        let root = node(
            "e1",
            "window",
            "New Document - Text Editor",
            &gtk4(&[]),
            None,
            vec![
                node("e2", "button", "New Tab", &gtk4(&["focusable"]), None, vec![]),
                node("e3", "text box", "", &gtk4(&["editable", "focusable", "multi line"]), Some(""), vec![]),
                node("e4", "button", "Save", &["showing", "visible"], None, vec![]),
            ],
        );
        let window =
            Window { address: "0xabc".into(), class: "org.gnome.TextEditor".into(), title: "Text Editor".into() };
        let ids: Vec<String> = observation(&window, &root, &[]).controls.into_iter().map(|c| c.id).collect();
        assert_eq!(ids, ["e2", "e3"]);
    }

    #[test]
    fn an_unnamed_field_is_labelled_by_its_role_never_by_what_it_holds() {
        // GNOME Text Editor's text view has no name: labelled by its text it read as a field named after what was
        // typed, and as '' when empty.
        let editable = shown(&["editable", "focusable", "multi line"]);
        let root = node(
            "e1",
            "window",
            "Text Editor",
            &SHOWN,
            None,
            vec![
                node("e2", "text box", "", &editable, Some(""), vec![]),
                node("e3", "text box", "", &editable, Some("charger"), vec![]),
                node("e4", "text", "Notes", &editable, Some("charger"), vec![]),
                node("e5", "push button", "", &SHOWN, Some("Save"), vec![]),
            ],
        );
        let window = Window { address: "0xabc".into(), class: "org.gnome.TextEditor".into(), title: "x".into() };
        let controls: Vec<(String, String, Option<String>)> =
            observation(&window, &root, &[]).controls.into_iter().map(|c| (c.role, c.label, c.value)).collect();
        let row = |role: &str, label: &str, value: Option<&str>| (role.into(), label.into(), value.map(Into::into));
        assert_eq!(
            controls,
            [
                row("textbox", "textbox", Some("")),
                row("textbox", "textbox", Some("charger")),
                row("textbox", "Notes", Some("charger")),
                row("button", "Save", None),
            ]
        );
    }

    #[test]
    fn a_text_views_whole_document_is_the_text_the_window_shows() {
        // As deskpilot read GNOME Text Editor holding a long document (#1398): the text view is the window's body, and
        // the page text was its title alone.
        let body: String = (1..=240).map(|line| format!("line {line}: item, qty, price\n")).collect();
        let view = Element {
            y: 47,
            width: 1238,
            height: 711,
            ..node("e2", "text box", "", &shown(&["editable", "focusable", "multi line"]), Some(&body), vec![])
        };
        let entry = node("e3", "text", "Search", &shown(&["editable", "focusable"]), Some("qty"), vec![]);
        let status = node("e4", "label", "", &SHOWN, Some("Ln 1, Col 1"), vec![]);
        let root = Element {
            width: 1238,
            height: 758,
            ..node("e1", "window", "invoice.txt - Text Editor", &SHOWN, None, vec![view, entry, status])
        };
        let window =
            Window { address: "0xabc".into(), class: "org.gnome.TextEditor".into(), title: "invoice.txt".into() };
        let observation = observation(&window, &root, &[]);
        assert_eq!(observation.viewport_text, format!("{body}\nLn 1, Col 1"));
        // The view is still a field to write in, holding the same text.
        assert_eq!(observation.controls[0].value.as_deref(), Some(body.as_str()));
    }

    #[test]
    fn a_peek_binds_no_snapshot_and_a_capture_never_holds_a_password() {
        let (window, mut root) = form();
        root.snapshot = None;
        let peeked = observation(&window, &root, &[]);
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

    #[test]
    fn an_element_past_the_windows_right_or_bottom_edge_is_no_control() {
        // As deskpilot read GNOME Text Editor tiled to 613x373 (#1395): its content wants more room, so the app places
        // elements beyond the window, and deskpilot refused each point as outside it.
        let at = |id: &str, name: &str, x, y, width| Element {
            x,
            y,
            width,
            height: 30,
            ..node(id, "button", name, &shown(&["focusable"]), None, vec![])
        };
        let label =
            Element { x: 650, y: 10, width: 80, height: 20, ..node("e7", "label", "", &SHOWN, Some("Beyond"), vec![]) };
        let root = Element {
            width: 613,
            height: 373,
            ..node(
                "e1",
                "window",
                "Text Editor",
                &SHOWN,
                None,
                vec![
                    at("e2", "Inside", 10, 10, 80),
                    at("e3", "Right", 620, 10, 80),
                    at("e4", "Below", 10, 380, 80),
                    // Half of it shows, but deskpilot aims at its centre, which is past the right edge.
                    at("e5", "Straddling", 560, 50, 200),
                    at("e6", "Edge", 540, 90, 60),
                    label,
                ],
            )
        };
        let window = Window { address: "0xabc".into(), class: "org.gnome.TextEditor".into(), title: "x".into() };
        let observation = observation(&window, &root, &[]);
        let labels: Vec<_> = observation.controls.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["Inside", "Edge"]);
        assert_eq!(observation.offscreen_text, "Beyond");
    }

    #[test]
    fn an_action_aims_at_its_element_centre_in_the_window() {
        let button =
            Element { x: 100, y: 40, width: 80, height: 20, ..node("e2", "push button", "Save", &[], None, vec![]) };
        let empty = node("e3", "label", "", &[], None, vec![]);
        let root = Element { width: 800, height: 600, ..node("e1", "frame", "", &[], None, vec![button, empty]) };
        assert_eq!(aim(&root, "e2"), Some((140.0, 50.0, 800.0, 600.0)));
        assert_eq!(aim(&root, "e3"), None);
        assert_eq!(aim(&root, "e9"), None);
    }
}
