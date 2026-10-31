//! The seam between the browser layer and the decision loop.
//!
//! The browser layer produces `Observation` (bounded, for Jev's action choice) and `Capture` (complete, for reading),
//! and executes `Action`s. Nothing above this seam touches CDP.

use std::collections::BTreeSet;

use serde::{Deserialize, Deserializer, Serialize};

use crate::BoxFuture;
use crate::models::{Artifact, Attachment, MaybeUnavailable, Operation, SourceControl, StepOutcome};
use crate::py::datetime::DateTime;
use crate::py::json::dumped_len;
use crate::py::{lax, text};

pub fn cut_marker(omitted_chars: usize) -> String {
    format!("\n[Viewport text cut: {omitted_chars} characters omitted; read the page for the rest]")
}

/// `text` within `max_chars`, ending with how much was cut so a model knows the page goes on. Lengths are Python's,
/// in code points; with `json_encoded` they are of the text as `json.dumps` writes it, quotes excluded.
pub fn cut_text(text: &str, max_chars: usize, json_encoded: bool) -> String {
    cut_with(text, max_chars, json_encoded, cut_marker)
}

/// [`cut_text`] ending with `marker`, which is told how many characters were left out.
pub fn cut_with(text: &str, max_chars: usize, json_encoded: bool, marker: impl Fn(usize) -> String) -> String {
    // A JSON state escapes quotes and newlines; its budget must count those extra characters.
    let size = |value: &str| if json_encoded { dumped_len(value) } else { text::len(value) };
    if size(text) <= max_chars {
        return text.to_string();
    }
    let bounds: Vec<usize> = text.char_indices().map(|(at, _)| at).chain([text.len()]).collect();
    let total = bounds.len() - 1;
    let cut = |keep: usize| format!("{}{}", &text[..bounds[keep]], marker(total - keep));
    let fits = |keep: usize| size(&cut(keep)) <= max_chars;
    // The longest prefix that fits with its marker. Escaping makes one character cost up to six, so an excess
    // measured in encoded characters cannot be subtracted from a count of source characters.
    let (mut low, mut high) = (0, total.min(max_chars));
    while low < high {
        let middle = (low + high).div_ceil(2);
        if fits(middle) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    if low > 0 && fits(low) { cut(low) } else { String::new() }
}

/// A set of operations, written sorted by value: Python writes a `frozenset` in hash order, which changes from one
/// process to the next, and `sorted(control.operations)` is the order the policy shows Jev.
pub type Operations = BTreeSet<Operation>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Control {
    /// Kept across observations for as long as the same DOM node survives.
    pub id: String,
    #[serde(deserialize_with = "lax::nullable")]
    pub frame_id: Option<String>,
    #[serde(default)]
    pub frame_origin: Option<String>,
    /// Owner form or local field group, scoped to the frame and document that observed it.
    #[serde(default)]
    pub form_id: Option<String>,
    /// Browser guard without node ids, tying a replacement to the same semantics and receiving document. Read, never
    /// written: pydantic's `exclude=True`.
    #[serde(default, skip_serializing)]
    pub retarget_key: Option<String>,
    pub role: String,
    pub label: String,
    /// What tells this control apart from others reading exactly the same: the heading or first line of the nearest
    /// card, row or section holding it and none of its twins.
    #[serde(default)]
    pub context: Option<String>,
    pub operations: Operations,
    /// Current value; masked for sensitive fields.
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub href: Option<String>,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub input_type: Option<String>,
    #[serde(default)]
    pub input_name: Option<String>,
    /// The most characters this field accepts, when it sets a limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<usize>,
    #[serde(default)]
    pub autocomplete: Option<String>,
    /// The form submission triggered by Enter in a field or clicking a submit button.
    #[serde(default)]
    pub submit_semantics: Option<String>,
    #[serde(default, deserialize_with = "lax::opt_boolean")]
    pub checked: Option<bool>,
    #[serde(default, deserialize_with = "lax::opt_boolean")]
    pub selected: Option<bool>,
    #[serde(default, deserialize_with = "lax::opt_boolean")]
    pub expanded: Option<bool>,
    #[serde(default, deserialize_with = "lax::boolean")]
    pub sensitive: bool,
    #[serde(default, deserialize_with = "lax::boolean")]
    pub offscreen: bool,
    /// A field the page marks required (`required` or `aria-required`).
    #[serde(default, deserialize_with = "lax::opt_boolean")]
    pub required: Option<bool>,
    /// A field the page marks invalid (`aria-invalid`).
    #[serde(default, deserialize_with = "lax::opt_boolean")]
    pub invalid: Option<bool>,
    /// A link the page marks `rel="next"`: the next page of the list it belongs to.
    #[serde(default, deserialize_with = "lax::opt_boolean")]
    pub next_page: Option<bool>,
    /// A link the page marks with a `download` attribute: activating it saves a file instead of opening a page.
    #[serde(default, deserialize_with = "lax::opt_boolean")]
    pub download: Option<bool>,
}

impl Control {
    /// A control with every optional field at its default.
    pub fn new(id: &str, role: &str, label: &str, operations: impl IntoIterator<Item = Operation>) -> Control {
        Control {
            id: id.into(),
            frame_id: None,
            frame_origin: None,
            form_id: None,
            retarget_key: None,
            role: role.into(),
            label: label.into(),
            context: None,
            operations: operations.into_iter().collect(),
            value: None,
            href: None,
            options: Vec::new(),
            input_type: None,
            input_name: None,
            max_length: None,
            autocomplete: None,
            submit_semantics: None,
            checked: None,
            selected: None,
            expanded: None,
            sensitive: false,
            offscreen: false,
            required: None,
            invalid: None,
            next_page: None,
            download: None,
        }
    }
}

/// Whether the page marks this control as the link to the next page of a list (`rel="next"`), in any language and
/// behind any icon. Its label is never read for it: Jev picks a pager the page does not mark.
pub fn pager_link(control: &Control) -> bool {
    control.role == "link"
        && control.next_page == Some(true)
        && control.operations.contains(&Operation::Click)
        && control.href.as_deref().is_some_and(|href| !href.is_empty())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tab {
    pub id: String,
    pub url: String,
    pub title: String,
    #[serde(deserialize_with = "lax::boolean")]
    pub active: bool,
    #[serde(deserialize_with = "lax::nullable")]
    pub opener_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dialog {
    pub kind: String,
    pub message: String,
    #[serde(default)]
    pub default_prompt: Option<String>,
}

fn http_status<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    match lax::opt_int(d)? {
        Some(status) if !(100..=999).contains(&status) => {
            Err(serde::de::Error::custom("Input should be between 100 and 999"))
        }
        status => Ok(status),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub url: String,
    pub title: String,
    /// Fingerprint of the observed page. `act` refuses a key other than the latest observation's, then re-checks the
    /// target element itself before dispatch.
    pub page_key: String,
    pub captured_at: DateTime,
    /// HTTP status of this document, when the browser exposes it; absent for non-HTTP pages.
    #[serde(default, deserialize_with = "http_status")]
    pub response_status: Option<i64>,
    /// Document identity, independent of field edits and scrolling, for access-wall checks.
    #[serde(default)]
    pub document_key: String,
    pub controls: Vec<Control>,
    pub viewport_text: String,
    /// Characters of the page's visible text outside the viewport: text a read takes in that `viewport_text` leaves
    /// out.
    #[serde(default, deserialize_with = "lax::int_ge0")]
    pub offscreen_text: i64,
    /// Whether the page's visible state is carried by pixels: a chart, picture, drawing or video with no control
    /// for it. snapshot.js reads it from the viewport, so page text and the control list cannot stand in for it.
    #[serde(default, deserialize_with = "lax::boolean")]
    pub visual_content: bool,
    pub tabs: Vec<Tab>,
    #[serde(default)]
    pub dialog: Option<Dialog>,
    /// Frames whose content could not be read; reported so answers never claim full coverage.
    #[serde(default, deserialize_with = "lax::int_ge0")]
    pub inaccessible_frames: i64,
    /// A same-origin http(s) entry sits immediately before the current one in browser history.
    #[serde(default, deserialize_with = "lax::boolean")]
    pub can_go_back: bool,
    /// The tab's browser history entry this page is. None when the browser would not say.
    #[serde(default)]
    pub history_entry: Option<String>,
}

impl Observation {
    /// The capture date with its weekday, then a calendar of this month and the twelve after it, a month to a line and
    /// each weekday's dates together. A relative day is looked up in it rather than counted. It runs to this month next
    /// year because a month or weekday named without a year means its next occurrence, which always falls in that span.
    pub fn today(&self) -> String {
        let today = self.captured_at;
        let mut lines = vec![format!("{} ({})", today.date_string(), today.weekday_name())];
        let mut month = today.first_of_month();
        for _ in 0..=12 {
            lines.push(month_calendar(month));
            month = month.add_days(32).first_of_month();
        }
        lines.join("\n")
    }
}

/// One month as a line, Monday first: `November 2026: Mon 2 9 16 23 30; Tue 3 10 17 24; ...`.
fn month_calendar(first: DateTime) -> String {
    let mut columns: Vec<(usize, String)> = (0..7)
        .map(|offset| {
            let start = first.add_days(offset);
            let days: Vec<String> = (0..)
                .map(|weeks| start.add_days(7 * weeks))
                .take_while(|day| day.month == first.month)
                .map(|day| day.day.to_string())
                .collect();
            (start.weekday(), format!("{} {}", &start.weekday_name()[..3], days.join(" ")))
        })
        .collect();
    columns.sort();
    let columns: Vec<String> = columns.into_iter().map(|(_, column)| column).collect();
    format!("{} {}: {}", first.month_name(), first.year, columns.join("; "))
}

str_enum! {
    pub enum BlockKind {
        Heading = "heading",
        Paragraph = "paragraph",
        ListItem = "list_item",
        Table = "table",
        Record = "record",
        Code = "code",
        Link = "link",
        Observation = "observation",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Block {
    pub source_id: String,
    #[serde(default)]
    pub source_url: Option<String>,
    /// This block contains the entire downloaded text, with no omitted bytes.
    #[serde(default, deserialize_with = "lax::boolean")]
    pub complete_source: bool,
    pub kind: BlockKind,
    #[serde(deserialize_with = "lax::nullable")]
    pub frame_id: Option<String>,
    /// Offsets into `Capture.text` in code points; `text[start:end]` is this block's rendering in Python.
    #[serde(deserialize_with = "lax::int_ge0")]
    pub start: i64,
    #[serde(deserialize_with = "lax::int_ge0")]
    pub end: i64,
    #[serde(default)]
    pub heading_path: Vec<String>,
    #[serde(default)]
    pub control_context: Option<SourceControl>,
    #[serde(default)]
    pub href: Option<String>,
    #[serde(default)]
    pub list_id: Option<String>,
    #[serde(default, deserialize_with = "lax::opt_int_ge0")]
    pub list_count: Option<i64>,
    /// The capture saw no frame inside this block's list element, so a frame it could not open elsewhere on the page
    /// cannot hold one of the list's members.
    #[serde(default, deserialize_with = "lax::boolean")]
    pub list_frameless: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    pub url: String,
    pub title: String,
    pub captured_at: DateTime,
    /// SHA-256 of `text`; evidence records it so a quote can be re-validated later.
    pub sha256: String,
    pub text: String,
    pub blocks: Vec<Block>,
    #[serde(default, deserialize_with = "lax::int_ge0")]
    pub inaccessible_frames: i64,
}

impl Capture {
    /// `text[block.start:block.end]`: the offsets are code points, as Python slices.
    pub fn block_text(&self, block: &Block) -> &str {
        let start = block.start.max(0) as usize;
        let rest = match self.text.char_indices().nth(start) {
            Some((at, _)) => &self.text[at..],
            None => "",
        };
        text::prefix(rest, (block.end - block.start).max(0) as usize)
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub operation: Operation,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub target_id: Option<String>,
    /// Drop target of a drag: the control the dragged element is released onto.
    #[serde(default)]
    pub destination_id: Option<String>,
    /// Value to type or option to select. May hold a resolved secret: never log or persist an Action.
    #[serde(default)]
    pub text: Option<String>,
    /// `text` is a resolved secret: the page masks the field so it never renders in a screenshot.
    #[serde(default, deserialize_with = "lax::boolean")]
    pub secret: bool,
    #[serde(default)]
    pub tab_id: Option<String>,
    /// Origin authorized by the resolver; the receiving document must still match at insertion.
    #[serde(default)]
    pub secret_origin: Option<String>,
    #[serde(default)]
    pub files: Vec<Attachment>,
    #[serde(default, deserialize_with = "lax::opt_boolean")]
    pub accept_dialog: Option<bool>,
}

impl Action {
    pub fn new(operation: Operation) -> Action {
        Action {
            operation,
            url: None,
            target_id: None,
            destination_id: None,
            text: None,
            secret: false,
            tab_id: None,
            secret_origin: None,
            files: Vec::new(),
            accept_dialog: None,
        }
    }
}

impl std::fmt::Debug for Action {
    // `text` may be a resolved secret, so it never reaches a debug dump.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Action")
            .field("operation", &self.operation)
            .field("url", &self.url)
            .field("target_id", &self.target_id)
            .field("destination_id", &self.destination_id)
            .field("text", &self.text.as_ref().map(|_| ".."))
            .field("secret", &self.secret)
            .field("tab_id", &self.tab_id)
            .finish_non_exhaustive()
    }
}

/// A browser failure a `Page` returns, with a message safe to put in a run result: never page text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrowserError {
    #[error("{0}")]
    Failed(String),
    /// CDP's -32000 server error, which Chrome answers when the document a command addressed was replaced while it
    /// ran. Only the code is kept, as the message can carry page text, and other -32000 failures share it, so a
    /// reader treats it as a navigation only by reading the page again.
    #[error("{0}")]
    ContextLost(String),
    /// The browser session no longer has a page available for this run.
    #[error("{0}")]
    Unavailable(String),
    /// A page cannot safely attribute pixels to its permitted documents.
    #[error("{0}")]
    ScreenshotsUnavailable(String),
    /// `Page.navigate` gave up waiting for a document. Before a run's first step the run ends `unavailable`; a
    /// navigation action reports a failed step so the agent can try another destination.
    #[error("{0}")]
    NavigationTimeout(String),
    /// `Page.navigate` got nothing from the site: a dropped, refused or empty connection. A name that does not
    /// resolve is not one of these; that can be a mistyped address.
    #[error("{0}")]
    SiteUnreachable(String),
    /// A page with no browser behind it was asked to act, render or read what it could not fetch. Nothing failed, so
    /// the run ends `blocked`, and no handler that recovers from a browser failure may catch it.
    #[error("{0}")]
    NeedsBrowser(String),
}

impl BrowserError {
    pub fn needs_browser(&self) -> bool {
        matches!(self, BrowserError::NeedsBrowser(_))
    }
}

impl MaybeUnavailable for BrowserError {
    fn is_unavailable(&self) -> bool {
        matches!(self, BrowserError::Unavailable(_))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActResult {
    pub outcome: StepOutcome,
    #[serde(deserialize_with = "lax::boolean")]
    pub page_changed: bool,
    #[serde(default)]
    pub detail: Option<String>,
    /// The action itself took the page elsewhere: its own input event began a navigation or changed the address, or
    /// it opened a tab. A page that moves on its own time is not.
    #[serde(default, deserialize_with = "lax::boolean")]
    pub moved: bool,
}

pub type PageResult<T> = Result<T, BrowserError>;

pub trait Page: Send + Sync {
    /// Whether this page was fetched over HTTP: its scripts never ran, and it can only follow links.
    fn over_http(&self) -> bool {
        false
    }

    /// The error that ends this run's page and sends it on to a browser.
    fn needs_browser(&self, message: String) -> BrowserError {
        BrowserError::NeedsBrowser(message)
    }

    fn artifacts(&self) -> Vec<Artifact>;

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>>;

    /// Whether the current URL or document differs, without indexing the page's controls again.
    fn document_changed<'a>(&'a self, observation: &'a Observation) -> BoxFuture<'a, PageResult<bool>>;

    /// `back_to` is the page BACK opens when this tab has no earlier entry on its site: a run that went straight to a
    /// deep address keeps the start page it skipped one BACK away without spending a load on it.
    fn navigate<'a>(&'a self, url: &'a str, back_to: Option<&'a str>) -> BoxFuture<'a, PageResult<()>>;

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>>;

    /// Dispatch at most once. Returns STALE or COVERED without dispatching when guards fail.
    fn act<'a>(&'a self, action: &'a Action, observation: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>>;

    /// Whether `target_id`, or with none any control `observation` offered, changed within `timeout_seconds`.
    ///
    /// `act` refuses a control that changed since it was observed, so work deciding on it is already spent. A change
    /// is reported once the page has settled after it, ready to be observed again.
    fn redrawn<'a>(
        &'a self,
        observation: &'a Observation,
        timeout_seconds: f64,
        target_id: Option<&'a str>,
    ) -> BoxFuture<'a, PageResult<bool>>;

    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>>;

    /// Click at `x` and `y`, fractions of the viewport's width and height as `screenshot` shows it, and report what
    /// changed as `act` does. A page that draws no screenshot has no point to click.
    fn click_viewport(&self, _x: f64, _y: f64) -> BoxFuture<'_, PageResult<ActResult>> {
        Box::pin(async { Err(BrowserError::ScreenshotsUnavailable("this page has no viewport to click".into())) })
    }

    /// Hold back live and recorded frames while the page may show a secret, which pixels cannot mask.
    fn withhold_frames(&self, withheld: bool);

    /// The address the current document was served from, after any redirect.
    fn address(&self) -> BoxFuture<'_, PageResult<String>>;

    /// The HTTP status the current document was served with, or None when the browser does not say.
    fn response_status(&self) -> BoxFuture<'_, PageResult<Option<i64>>>;

    /// The page's current lifecycle generation, for a caller waiting on the next transition.
    fn lifecycle_mark(&self) -> u64;

    /// Wait for a lifecycle event newer than `mark`, returning whether one arrived before `seconds`.
    fn wait_for_lifecycle(&self, mark: u64, seconds: f64) -> BoxFuture<'_, bool>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(label: &str, next_page: Option<bool>, i: usize) -> Control {
        let mut control = Control::new(&format!("l{i}"), "link", label, [Operation::Click]);
        control.href = Some(format!("/{i}"));
        control.next_page = next_page;
        control
    }

    #[test]
    fn test_only_a_link_the_page_marks_rel_next_is_a_pager() {
        assert!(pager_link(&link("Suivant", Some(true), 0)));
        assert!(!pager_link(&link("Next", None, 0)));
        let mut button = link("next", Some(true), 0);
        button.role = "button".into();
        assert!(!pager_link(&button));
        let mut nowhere = link("next", Some(true), 0);
        nowhere.href = None;
        assert!(!pager_link(&nowhere));
        let mut inert = link("next", Some(true), 0);
        inert.operations.clear();
        assert!(!pager_link(&inert));
    }

    #[test]
    fn test_escaped_text_keeps_every_character_that_fits() {
        // Each character of this costs six once JSON-escaped; subtracting the escaped excess from a count of source
        // characters cut it to nothing although nine hundred of them fit.
        let text = "\u{6771}\u{4eac}".repeat(1000);
        let cut = cut_text(&text, 6000, true);
        assert!(dumped_len(&cut) <= 6000);
        assert!(cut.starts_with(&"\u{6771}\u{4eac}".repeat(450)) && cut.contains("characters omitted"));
    }

    #[test]
    fn a_short_text_is_not_cut_and_a_tiny_budget_keeps_nothing() {
        assert_eq!(cut_text("abc", 3, false), "abc");
        assert_eq!(cut_text("abcdef", 3, false), "");
        let cut = cut_text(&"a".repeat(200), 100, false);
        assert_eq!(text::len(&cut), 100);
        assert!(cut.ends_with("[Viewport text cut: 172 characters omitted; read the page for the rest]"));
    }

    #[test]
    fn today_names_the_weekday_and_the_coming_months() {
        let observation = Observation {
            url: "https://example.test/".into(),
            title: "t".into(),
            page_key: "k".into(),
            captured_at: DateTime::new(2026, 9, 27, 10, 0, 0).utc(),
            response_status: None,
            document_key: String::new(),
            controls: vec![],
            viewport_text: String::new(),
            offscreen_text: 0,
            visual_content: false,
            tabs: vec![],
            dialog: None,
            inaccessible_frames: 0,
            can_go_back: false,
            history_entry: None,
        };
        assert_eq!(
            observation.today(),
            "2026-09-27 (Sunday)\n\
             September 2026: Mon 7 14 21 28; Tue 1 8 15 22 29; Wed 2 9 16 23 30; Thu 3 10 17 24; Fri 4 11 18 25; Sat 5 12 19 26; Sun 6 13 20 27\n\
             October 2026: Mon 5 12 19 26; Tue 6 13 20 27; Wed 7 14 21 28; Thu 1 8 15 22 29; Fri 2 9 16 23 30; Sat 3 10 17 24 31; Sun 4 11 18 25\n\
             November 2026: Mon 2 9 16 23 30; Tue 3 10 17 24; Wed 4 11 18 25; Thu 5 12 19 26; Fri 6 13 20 27; Sat 7 14 21 28; Sun 1 8 15 22 29\n\
             December 2026: Mon 7 14 21 28; Tue 1 8 15 22 29; Wed 2 9 16 23 30; Thu 3 10 17 24 31; Fri 4 11 18 25; Sat 5 12 19 26; Sun 6 13 20 27\n\
             January 2027: Mon 4 11 18 25; Tue 5 12 19 26; Wed 6 13 20 27; Thu 7 14 21 28; Fri 1 8 15 22 29; Sat 2 9 16 23 30; Sun 3 10 17 24 31\n\
             February 2027: Mon 1 8 15 22; Tue 2 9 16 23; Wed 3 10 17 24; Thu 4 11 18 25; Fri 5 12 19 26; Sat 6 13 20 27; Sun 7 14 21 28\n\
             March 2027: Mon 1 8 15 22 29; Tue 2 9 16 23 30; Wed 3 10 17 24 31; Thu 4 11 18 25; Fri 5 12 19 26; Sat 6 13 20 27; Sun 7 14 21 28\n\
             April 2027: Mon 5 12 19 26; Tue 6 13 20 27; Wed 7 14 21 28; Thu 1 8 15 22 29; Fri 2 9 16 23 30; Sat 3 10 17 24; Sun 4 11 18 25\n\
             May 2027: Mon 3 10 17 24 31; Tue 4 11 18 25; Wed 5 12 19 26; Thu 6 13 20 27; Fri 7 14 21 28; Sat 1 8 15 22 29; Sun 2 9 16 23 30\n\
             June 2027: Mon 7 14 21 28; Tue 1 8 15 22 29; Wed 2 9 16 23 30; Thu 3 10 17 24; Fri 4 11 18 25; Sat 5 12 19 26; Sun 6 13 20 27\n\
             July 2027: Mon 5 12 19 26; Tue 6 13 20 27; Wed 7 14 21 28; Thu 1 8 15 22 29; Fri 2 9 16 23 30; Sat 3 10 17 24 31; Sun 4 11 18 25\n\
             August 2027: Mon 2 9 16 23 30; Tue 3 10 17 24 31; Wed 4 11 18 25; Thu 5 12 19 26; Fri 6 13 20 27; Sat 7 14 21 28; Sun 1 8 15 22 29\n\
             September 2027: Mon 6 13 20 27; Tue 7 14 21 28; Wed 1 8 15 22 29; Thu 2 9 16 23 30; Fri 3 10 17 24; Sat 4 11 18 25; Sun 5 12 19 26"
        );
    }

    #[test]
    fn an_action_never_shows_its_text() {
        let mut action = Action::new(Operation::Fill);
        action.text = Some("hunter2".into());
        assert!(!format!("{action:?}").contains("hunter2"));
    }
}
