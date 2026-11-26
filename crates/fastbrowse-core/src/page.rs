//! The seam between the browser layer and the decision loop.
//!
//! The browser layer produces `Observation` (bounded, for the driver's action choice) and `Capture` (complete, for reading),
//! and executes `Action`s. Nothing above this seam touches CDP.

use std::collections::BTreeSet;

use serde::{Deserialize, Deserializer, Serialize};

use crate::BoxFuture;
use crate::decision::Edges;
use crate::models::{Artifact, Attachment, Link, MaybeUnavailable, Operation, SourceControl, StepFailure, StepOutcome};
use crate::py::datetime::DateTime;
use crate::py::json::dumped_len;
use crate::py::{lax, text};
use crate::schema::{Field, ModelTy, Schema, Ty};
use indexmap::IndexMap;

pub mod json;

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
/// process to the next, and `sorted(control.operations)` is the order the policy shows the driver.
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
    /// The receiving iframe's document identity. The observation carries the top-level document identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_document_key: Option<String>,
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
    /// Whether this control owns keyboard focus in its document, when the browser reports it.
    #[serde(default, deserialize_with = "lax::opt_boolean", skip_serializing_if = "Option::is_none")]
    pub focused: Option<bool>,
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
    /// Which viewport-height band of the page the control's centre is in, counted from the viewport: 0 inside it, 1 the
    /// screen below, -1 the screen above. The driver is shown the controls one scroll reaches, as it is that text.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub screen: i32,
    /// A control the page has disabled (`disabled`, or inside `aria-disabled="true"`). It offers no operation until
    /// the page enables it, and is shown so a Save that stays disabled after an edit is not read as no Save (#1415).
    #[serde(default, deserialize_with = "lax::opt_boolean", skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    /// A field the page marks required (`required` or `aria-required`).
    #[serde(default, deserialize_with = "lax::opt_boolean")]
    pub required: Option<bool>,
    /// A field the page marks readonly (`readonly` or `aria-readonly`): it takes a click but not typing.
    #[serde(default, deserialize_with = "lax::boolean", skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// The layer the control is drawn in, numbered from 1 per observation: an element out of flow, positioned against
    /// the page and drawn over it, as a popup a page appends to `<body>` is. None for a control in the page itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<u32>,
    /// A field marked invalid by ARIA or a failed native constraint, without implying submission.
    #[serde(default, deserialize_with = "lax::opt_boolean")]
    pub invalid: Option<bool>,
    /// The browser's own words for why a native constraint refuses this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_message: Option<String>,
    /// What the page calls this control's role (`aria-roledescription`), such as "draggable".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_description: Option<String>,
    /// The text the page attaches to this control to describe it (`aria-describedby`), as the page states it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Which item of its list this control is, counted from 1: `aria-posinset`, or its place among the items of its
    /// nearest list. A position such as "first" is a place in that list, which a label does not state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_position: Option<u32>,
    /// How many items that list holds: `aria-setsize`, or the items counted in the list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_count: Option<u32>,
    /// A link the page marks `rel="next"`: the next page of the list it belongs to.
    #[serde(default, deserialize_with = "lax::opt_boolean")]
    pub next_page: Option<bool>,
    /// A link the page marks with a `download` attribute: activating it saves a file instead of opening a page.
    #[serde(default, deserialize_with = "lax::opt_boolean")]
    pub download: Option<bool>,
    /// An image, canvas or video the page takes input on: what it shows is pixels, with no page elements inside it
    /// to read or act on, such as a streamed remote browser or a canvas app.
    #[serde(default, deserialize_with = "lax::opt_boolean", skip_serializing_if = "Option::is_none")]
    pub drawn: Option<bool>,
    /// A control whose card shows a picture while its text leaves it alike to another control or unnamed: the task
    /// may name it by what the picture shows.
    #[serde(default, deserialize_with = "lax::opt_boolean", skip_serializing_if = "Option::is_none")]
    pub pictured: Option<bool>,
    /// A link to a page the run has already read, as a browser styles a visited link: the agent marks it, the page
    /// never does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub already_read: Option<bool>,
    /// A link to a different site from the one the browser opened for the task: the agent marks it from the link's
    /// address, the page never does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_site: Option<bool>,
}

impl Control {
    /// Whether the control offers any operation. A disabled control offers none: it is shown to Jev but never picked,
    /// so whatever asks "is there anything to act on" or counts what can be offered asks this, not whether it is
    /// listed (#1415).
    pub fn actionable(&self) -> bool {
        !self.operations.is_empty()
    }

    /// A control with every optional field at its default.
    pub fn new(id: &str, role: &str, label: &str, operations: impl IntoIterator<Item = Operation>) -> Control {
        Control {
            id: id.into(),
            frame_id: None,
            frame_origin: None,
            frame_document_key: None,
            form_id: None,
            retarget_key: None,
            role: role.into(),
            label: label.into(),
            context: None,
            operations: operations.into_iter().collect(),
            value: None,
            focused: None,
            href: None,
            options: Vec::new(),
            input_type: None,
            input_name: None,
            max_length: None,
            read_only: false,
            layer: None,
            autocomplete: None,
            submit_semantics: None,
            checked: None,
            selected: None,
            expanded: None,
            sensitive: false,
            offscreen: false,
            screen: 0,
            disabled: None,
            required: None,
            invalid: None,
            validation_message: None,
            role_description: None,
            description: None,
            item_position: None,
            item_count: None,
            next_page: None,
            download: None,
            drawn: None,
            pictured: None,
            already_read: None,
            other_site: None,
        }
    }
}

/// The native date and time input types, whose value is the one form HTML defines for each type whatever the
/// browser displays. The browser sets their value directly, and a value in any other form leaves the field empty.
pub const NATIVE_DATE_INPUT_TYPES: [&str; 5] = ["date", "datetime-local", "month", "week", "time"];

/// The value form HTML defines for a native date or time input type, with an example, or `None` for any other type.
/// A field writer that saw only the label wrote a date the way the page displays it (`11/16/2026`), which a `date`
/// input refuses, so the writer is told this form.
pub fn native_date_form(input_type: &str) -> Option<&'static str> {
    Some(match input_type {
        "date" => "YYYY-MM-DD, such as 2026-11-16",
        "datetime-local" => "YYYY-MM-DDThh:mm in 24-hour time, such as 2026-11-16T09:30",
        "month" => "YYYY-MM, such as 2026-11",
        "week" => "YYYY-Www with the ISO week number, such as 2026-W47",
        "time" => "hh:mm in 24-hour time, such as 09:30",
        _ => return None,
    })
}

/// Whether the page marks this control as the link to the next page of a list (`rel="next"`), in any language and
/// behind any icon. Its label is never read for it: a pager the page does not mark is not recorded as a next page.
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

impl Dialog {
    /// What answering this dialog did, for the step's record: the question it asked and whether the run accepted it.
    /// Without it a confirmed delete reads as a bare "dialog" step, and the check on whether the task's steps were
    /// carried out scored a real delete 0.39 to 0.67. The message is quoted as it stands, never escaped: the caller
    /// redacts the text, and an escaped password matches none of the forms the redactor holds.
    pub fn answered(&self, accepted: bool) -> String {
        let answer = if accepted { "accepted" } else { "dismissed" };
        format!("{answer} the {} dialog \"{}\"", self.kind, self.message)
    }
}

fn http_status<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    match lax::opt_int(d)? {
        Some(status) if !(100..=999).contains(&status) => {
            Err(serde::de::Error::custom("Input should be between 100 and 999"))
        }
        status => Ok(status),
    }
}

/// A CAPTCHA the browser's own solver took on in the active tab's document, as Browser Use Cloud reports it with its
/// `BrowserUse.captchaSolverStarted` and `BrowserUse.captchaSolverFinished` events. A solve that succeeded is no
/// longer held: the page it cleared moves on by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptchaSolve {
    /// The address of the page the CAPTCHA is on.
    pub url: String,
    /// The CAPTCHA's provider as the solver names it, such as `cloudflare` or `recaptcha`.
    pub vendor: String,
    /// True while the solver works on it, false once it finished without solving it.
    pub working: bool,
}

/// One audio or video player's state, read from the element when the page was observed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Media {
    /// `audio` or `video`.
    pub kind: String,
    /// The player's own accessible name, when it has one.
    #[serde(default)]
    pub label: Option<String>,
    /// The nearest heading before the player, which tells unnamed players apart.
    #[serde(default)]
    pub heading: Option<String>,
    pub paused: bool,
    pub ended: bool,
    pub muted: bool,
    pub elapsed_seconds: f64,
    /// None while the length is unknown or the stream has no end.
    #[serde(default)]
    pub duration_seconds: Option<f64>,
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
    /// The visible text in the viewport of every frame the observation read; a cross-origin frame's text follows
    /// the page's own under a `[Frame: origin]` line.
    pub viewport_text: String,
    /// Identity of the place the viewport shows, from the page's and its scroll containers' scroll positions: text that
    /// updates by itself made a place a run had seen read as new, and a textless place could not be told apart.
    #[serde(default)]
    pub viewport_key: String,
    /// The visible text outside the viewport, from every frame the observation read in the same order and form.
    #[serde(default)]
    pub offscreen_text: String,
    /// Whether the page's visible state is carried by pixels: a chart, picture, drawing or video with no control
    /// for it. snapshot.js reads it from the viewport, so page text and the control list cannot stand in for it.
    #[serde(default, deserialize_with = "lax::boolean")]
    pub visual_content: bool,
    /// The name of the open modal the rest of the page sits behind, empty when it has none. The page hides what is
    /// under a modal from assistive technology, so those controls leave the observation without leaving the document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modal: Option<String>,
    /// Every audio and video player on the page, shown or not: their state lives in the element, not the page text.
    #[serde(default)]
    pub media: Vec<Media>,
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

str_enum! {
    pub enum BlockKind {
        Heading = "heading",
        DocumentTitle = "document_title",
        Paragraph = "paragraph",
        ListItem = "list_item",
        Table = "table",
        Record = "record",
        Code = "code",
        Link = "link",
        Observation = "observation",
    }
}

/// Where an item sits in the list the page draws it in: the page's own `aria-posinset` and `aria-setsize` where it
/// declares them, otherwise its place among the rendered items of its list. Counted from 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemPlace {
    #[serde(deserialize_with = "lax::int_ge0")]
    pub position: i64,
    #[serde(deserialize_with = "lax::int_ge0")]
    pub count: i64,
}

/// One structured value's exact place in the rendered capture, in Unicode code points.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceField {
    pub path: Vec<String>,
    #[serde(deserialize_with = "lax::int_ge0")]
    pub start: i64,
    #[serde(deserialize_with = "lax::int_ge0")]
    pub end: i64,
}

impl SourceField {
    pub fn shifted(mut self, offset: i64) -> Self {
        self.start += offset;
        self.end += offset;
        self
    }
}

/// How the `source_id` of a block holding a downloaded file's text begins. Every capture taken after a download carries
/// the file, so a quote from it is the file's, wherever the page around it puts it.
pub const DOWNLOAD_SOURCE: &str = "download/";

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
    /// The links inside this block, with their text's place in `Capture.text`: a link's text alone does not say
    /// where it leads, nor where a value the page set apart as its own element ends.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    /// This block is all the text the page renders for one item it repeats, such as a comment, card or list row, with
    /// each part of it that shows no text named in square brackets: what the block does not show, that item does not
    /// show. A record can be a `list_item` as well as a `record`, so its kind does not say this.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub whole_record: bool,
    /// The place of the list item this block is, or sits inside: a label does not say it is the third of the list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_place: Option<ItemPlace>,
    /// Some of this block is drawn inside the viewport when the page is captured, so the viewport's place in
    /// `Capture.text` is the span of the blocks that say so.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub in_viewport: bool,
    /// Structured values retain their boundaries, so copying one never requires retyping its whitespace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<SourceField>,
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
    /// Where code blanked a secret value in `text`, as code point ranges: a `•` there is a mask, not page text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub masked: Vec<(i64, i64)>,
    /// Where code wrote its own advice into `text`, such as a link's title beside its label or a date's
    /// `datetime` beside the words that show it, as code point ranges with the space before each: words code added,
    /// not page text, so a quote that leaves them out still quotes the page.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advice: Vec<(i64, i64)>,
}

/// Marks a reader writes around its own advice while it reads a block, such as a link's title beside its label, so
/// where the advice sits is known after the block's cleanup moves it. Unicode reserves these noncharacters for a
/// program's internal use; the words between them were matched in the cleaned text before, which marked the page's
/// own copy of those words as advice too, so a quote that left them out passed as quoting the page.
pub const ADVICE_OPEN: char = '\u{FDD0}';
pub const ADVICE_CLOSE: char = '\u{FDD1}';

/// `text` without its advice marks, and where each piece of advice sits in it as code point ranges with the
/// whitespace before it. Advice is written after a space, so where page text already ends in whitespace that space is
/// dropped, as cleanup collapsed it before marks kept it apart. Only marks around words `advised` says the reader
/// wrote are advice: a page that holds the marks itself still has its text read as page text.
pub fn unmark_advice(text: &str, advised: impl Fn(&str) -> bool) -> (String, Vec<(i64, i64)>) {
    let mut plain = String::with_capacity(text.len());
    let mut length = 0i64;
    let mut spans = Vec::new();
    let mut open: Option<(usize, i64)> = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ADVICE_OPEN => {
                let kept = plain.trim_end_matches(crate::py::text::is_space);
                if kept.len() < plain.len() || plain.is_empty() {
                    chars.next_if_eq(&' ');
                }
                open = Some((plain.len(), crate::py::text::len(kept) as i64));
            }
            ADVICE_CLOSE => {
                if let Some((at, start)) = open.take()
                    && advised(plain[at..].trim_start_matches(crate::py::text::is_space))
                {
                    spans.push((start, length));
                }
            }
            c => {
                plain.push(c);
                length += 1;
            }
        }
    }
    (plain, spans)
}

impl Capture {
    /// A document title is otherwise context only: a reader cannot cite it when it is absent from the body.
    pub fn with_document_title(mut self) -> Self {
        if self.title.trim().is_empty() {
            return self;
        }
        if !self.text.is_empty() && !self.text.ends_with("\n\n") {
            self.text.push_str("\n\n");
        }
        let start = text::len(&self.text) as i64;
        self.text.push_str(&self.title);
        self.blocks.push(Block {
            source_id: "document/title".into(),
            source_url: None,
            complete_source: false,
            kind: BlockKind::DocumentTitle,
            frame_id: None,
            start,
            end: text::len(&self.text) as i64,
            heading_path: Vec::new(),
            control_context: None,
            href: None,
            links: Vec::new(),
            whole_record: false,
            item_place: None,
            in_viewport: false,
            fields: Vec::new(),
        });
        self.sha256 = crate::py::sha256_hex(self.text.as_bytes());
        self
    }

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
    /// A path drawn on the target itself, with coordinates normalized to its live box.
    #[serde(default)]
    pub drag_path: Option<DragPath>,
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
            drag_path: None,
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
            .field("drag_path", &self.drag_path)
            .field("text", &self.text.as_ref().map(|_| ".."))
            .field("secret", &self.secret)
            .field("tab_id", &self.tab_id)
            .finish_non_exhaustive()
    }
}

/// One point in a same-control drag, normalized to the target's live box.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DragPoint {
    #[serde(deserialize_with = "lax::float")]
    pub x: f64,
    #[serde(deserialize_with = "lax::float")]
    pub y: f64,
}

impl Schema for DragPoint {
    fn ty() -> Ty {
        ModelTy::frozen(
            "DragPoint",
            vec![
                Field::required("x", Ty::Float).doc("Horizontal position within the target box, from 0 to 1."),
                Field::required("y", Ty::Float).doc("Vertical position within the target box, from 0 to 1."),
            ],
        )
        .ty()
    }
}

/// The complete pointer path for a drag drawn on one control.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DragPath {
    pub press: DragPoint,
    pub points: Vec<DragPoint>,
    pub release: DragPoint,
    #[serde(default, deserialize_with = "lax::boolean")]
    pub shift: bool,
}

impl Schema for DragPath {
    fn ty() -> Ty {
        ModelTy::frozen(
            "DragPath",
            vec![
                Field::required("press", DragPoint::ty()),
                Field::required("points", Ty::list(DragPoint::ty()).constrained("minItems", 1)),
                Field::required("release", DragPoint::ty()),
                Field::default("shift", Ty::Bool, false),
            ],
        )
        .doc("A complete pointer path relative to the target's live box.")
        .ty()
    }
}

/// A browser failure a `Page` returns, with a message safe to put in a run result: never page text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrowserError {
    #[error("{}", .0.message)]
    Failed(Failure),
    /// CDP's -32000 server error, which Chrome answers when the document a command addressed was replaced while it
    /// ran. Only the code is kept, as the message can carry page text, and other -32000 failures share it, so a
    /// reader treats it as a navigation only by reading the page again.
    #[error("{0}")]
    ContextLost(String),
    /// The browser session no longer has a page available for this run.
    #[error("{0}")]
    Unavailable(String),
    /// The browser answered, but the renderer for this page stopped answering commands.
    #[error("{}", .0.message)]
    PageUnresponsive(Failure),
    /// A page cannot safely attribute pixels to its permitted documents.
    #[error("{0}")]
    ScreenshotsUnavailable(String),
    /// `Page.navigate` gave up waiting for a document. Before a run's first step the run ends `unavailable`; a
    /// navigation action reports a failed step so the agent can try another destination.
    #[error("{}", .0.message)]
    NavigationTimeout(Failure),
    /// `Page.navigate` got nothing from the site: a dropped, refused or empty connection. A name that does not
    /// resolve is not one of these; that can be a mistyped address.
    #[error("{}", .0.message)]
    SiteUnreachable(Failure),
    /// A page with no browser behind it was asked to act, render or read what it could not fetch. Nothing failed, so
    /// the run ends `blocked`, and no handler that recovers from a browser failure may catch it.
    #[error("{0}")]
    NeedsBrowser(String),
}

/// A browser failure with no variant of its own: its message, and a reason code that holds no content.
///
/// The hosted worker logs only `RunResult::failure_kind`, never the message, which can name an address; a failure
/// named only `Failed` said nothing of what failed (#1372). The reason is set where the error is made, from what it
/// knows as values: a CDP method and Chrome's code for its failure, or a fixed name for a check of fastbrowse's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub reason: String,
    pub message: String,
}

impl Failure {
    pub fn new(reason: impl Into<String>, message: impl Into<String>) -> Failure {
        Failure { reason: reason.into(), message: message.into() }
    }
}

impl crate::models::FailureKind for BrowserError {
    fn failure_kind(&self) -> String {
        match self {
            BrowserError::Failed(failure) => format!("Failed/{}", failure.reason),
            BrowserError::PageUnresponsive(failure) => format!("PageUnresponsive/{}", failure.reason),
            BrowserError::NavigationTimeout(failure) => format!("NavigationTimeout/{}", failure.reason),
            BrowserError::SiteUnreachable(failure) => format!("SiteUnreachable/{}", failure.reason),
            BrowserError::ContextLost(_) => "ContextLost".into(),
            BrowserError::Unavailable(_) => "Unavailable".into(),
            BrowserError::ScreenshotsUnavailable(_) => "ScreenshotsUnavailable".into(),
            BrowserError::NeedsBrowser(_) => "NeedsBrowser".into(),
        }
    }
}

impl BrowserError {
    pub fn failed(reason: impl Into<String>, message: impl Into<String>) -> BrowserError {
        BrowserError::Failed(Failure::new(reason, message))
    }

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
    #[serde(default)]
    pub failure: Option<StepFailure>,
    #[serde(deserialize_with = "lax::boolean")]
    pub page_changed: bool,
    #[serde(default)]
    pub detail: Option<String>,
    /// The action itself took the page elsewhere: its own input event began a navigation or changed the address, or
    /// it opened a tab. A page that moves on its own time is not.
    #[serde(default, deserialize_with = "lax::boolean")]
    pub moved: bool,
    /// For a fill: the control that holds the text it typed, as the observation names a control, as the browser
    /// reports the editable focused when the typing ended. It is not always the one asked for: a page can hand focus
    /// to another editor. None when the browser reported no such control, so nothing is claimed about where it went.
    /// The run's own bookkeeping, held in process: never part of the protocol, so never written or read.
    #[serde(skip)]
    pub landed: Option<FillReceiver>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillReceiver {
    pub element: String,
    pub document: String,
}

pub type PageResult<T> = Result<T, BrowserError>;

/// What a page read without a browser left out of the current document, which a browser shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unread {
    /// The markup carries scripts a browser would run, and none of them ran.
    pub scripts: bool,
    /// Frames whose documents were not read.
    pub frames: i64,
    /// Whether a step this page cannot take opens it in a browser in this run; a run limited to fetching cannot.
    pub browser: bool,
}

pub trait Page: Send + Sync {
    /// The documented maximum price of one configured web-search request. None leaves search unavailable.
    fn search_price(&self) -> Option<f64> {
        None
    }

    /// Replace the view with the provider's real result links, retaining any browser session behind it.
    fn search<'a>(&'a self, _query: &'a str, _timeout_seconds: f64) -> BoxFuture<'a, SearchOutcome> {
        Box::pin(async {
            SearchOutcome {
                cost: crate::models::CostLine::new(
                    crate::models::CostComponent::Search,
                    crate::models::CostBasis::Metered,
                    Some(0.0),
                ),
                error: Some("web search is not configured".into()),
                page_changed: false,
            }
        })
    }
    /// Why this page cannot press `key`, by its own key grammar, or None when it can. A browser presses the chords
    /// [`crate::keys::chord`] parses; a page with another key dialect says so here, so a key copied from the task is
    /// checked by the grammar of the page that will press it.
    fn key_error(&self, key: &str) -> Option<String> {
        crate::keys::chord(key).err().map(|error| error.to_string())
    }

    /// Whether this page was fetched over HTTP: its scripts never ran, and it can only follow links.
    fn over_http(&self) -> bool {
        false
    }

    /// What a browser would show of the current document that this page did not take in, as a page fetched over HTTP
    /// leaves out what scripts build and what unread frames hold, links included. None when it took in all of it.
    fn unread(&self) -> Option<Unread> {
        None
    }

    /// The error that ends this run's page and sends it on to a browser.
    fn needs_browser(&self, message: String) -> BrowserError {
        BrowserError::NeedsBrowser(message)
    }

    /// The address a browser opens to carry on from this page, when it is not the page's own. A page fetched over HTTP
    /// was redirected by a server answering a client that is not a browser, so a browser asks for the address the
    /// read asked for and meets the server's answer to a browser: Google Flights sends the reader's request to its
    /// unsupported-browser page, and a browser opened there started past a warning it was never shown.
    fn browser_address(&self) -> Option<String> {
        None
    }

    fn artifacts(&self) -> Vec<Artifact>;

    /// Keeps a capture's masked text as a `capture` artifact where this page keeps its downloads. A page with nowhere
    /// to keep one, such as a page fetched over HTTP, keeps none.
    fn keep_capture<'a>(&'a self, _name: &'a str, _text: &'a str) -> BoxFuture<'a, PageResult<Option<Artifact>>> {
        Box::pin(async { Ok(None) })
    }

    /// Keeps a secret-safe screenshot artifact where this page keeps its downloads.
    fn keep_screenshot<'a>(&'a self, _name: &'a str, _png: &'a [u8]) -> BoxFuture<'a, PageResult<Option<Artifact>>> {
        Box::pin(async { Ok(None) })
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>>;

    /// A fresh reading for screenshot safety, without replacing action guards or consuming pending notices.
    /// Implementations whose observations bind actions or drain notices must override this default.
    fn glance(&self) -> BoxFuture<'_, PageResult<Observation>> {
        self.observe()
    }

    /// Whether the current URL or document differs, without indexing the page's controls again.
    fn document_changed<'a>(&'a self, observation: &'a Observation) -> BoxFuture<'a, PageResult<bool>>;

    /// `back_to` is the page BACK opens when this tab has no earlier entry on its site: a run that went straight to a
    /// deep address keeps the start page it skipped one BACK away without spending a load on it.
    fn navigate<'a>(&'a self, url: &'a str, back_to: Option<&'a str>) -> BoxFuture<'a, PageResult<()>>;

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>>;

    /// Dispatch at most once. Returns STALE or COVERED without dispatching when guards fail.
    /// A Wait passed directly here has no deadline; use `wait` for a bounded wait or cancel the action.
    fn act<'a>(&'a self, action: &'a Action, observation: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>>;

    /// Dispatch a WAIT that lasts until the page changes by itself, for at most `seconds` (`f64::INFINITY` for a
    /// run with no time limit), and report what changed as `act` does. A page that cannot watch itself change acts on
    /// the WAIT as on any other action.
    fn wait<'a>(
        &'a self,
        action: &'a Action,
        observation: &'a Observation,
        _seconds: f64,
    ) -> BoxFuture<'a, PageResult<ActResult>> {
        self.act(action, observation)
    }

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
    /// changed as `act` does. With `within`, the point must be on that control of the last observation, or the click
    /// is refused as covered: a page that cannot tell refuses it. A page that draws no screenshot has no point to click.
    fn click_viewport<'a>(
        &'a self,
        _x: f64,
        _y: f64,
        _within: Option<&'a str>,
    ) -> BoxFuture<'a, PageResult<ActResult>> {
        Box::pin(async { Err(BrowserError::ScreenshotsUnavailable("this page has no viewport to click".into())) })
    }

    /// The card of each `pictured` control of the last observation among `ids`, as fractions of the viewport
    /// `screenshot` shows: what Jev is shown of it. A control out of view, gone, or in another frame has none.
    fn pictures<'a>(&'a self, _ids: &'a [String]) -> BoxFuture<'a, PageResult<IndexMap<String, Edges>>> {
        Box::pin(async { Ok(IndexMap::new()) })
    }

    /// Where control `id` of the last observation sits, as `pictures` measures a card. None out of view, gone, or in
    /// another frame.
    fn edges<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, PageResult<Option<Edges>>> {
        Box::pin(async { Ok(None) })
    }

    /// Scroll control `id` of the last observation into view if it is not already in full view, as an action on it
    /// would, so a screenshot taken next shows the box the action will use. A page with no viewport does nothing.
    fn reveal<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, PageResult<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Hold back live and recorded frames while the page may show a secret, which pixels cannot mask.
    fn withhold_frames(&self, withheld: bool);

    /// Take no image of the page from now to the end of the run, `screenshot` included, and hold every live and
    /// recorded frame: the run holds an address credential, which a page can paint where no reading finds it. A page
    /// that takes no images of its own has nothing to hold; the agent takes none for it either way.
    fn hold_images(&self) {
        self.withhold_frames(true);
    }

    /// Leave the current tab open when the run ends, because an irreversible action was sent from it: what it
    /// committed may still be in flight, and the result page is the user's to see. A page with no tab to keep does
    /// nothing.
    fn hand_over_tab(&self) {}

    /// The address the current document was served from, after any redirect.
    fn address(&self) -> BoxFuture<'_, PageResult<String>>;

    /// The active tab's URL as the browser reports it, which answers while the renderer is hung. None when the page
    /// cannot tell apart from its document.
    fn active_url(&self) -> BoxFuture<'_, PageResult<Option<String>>> {
        Box::pin(async { Ok(None) })
    }

    /// The HTTP status the current document was served with, or None when the browser does not say.
    fn response_status(&self) -> BoxFuture<'_, PageResult<Option<i64>>>;

    /// The page's current lifecycle generation, for a caller waiting on the next transition.
    fn lifecycle_mark(&self) -> u64;

    /// Wait for a lifecycle event newer than `mark`, returning whether one arrived before `seconds`; with
    /// `f64::INFINITY` the wait has no bound. A change in [`Page::captcha_solve`] is one.
    fn wait_for_lifecycle(&self, mark: u64, seconds: f64) -> BoxFuture<'_, bool>;

    /// The CAPTCHA the browser's own solver has taken on in the active tab's document, for a browser that reports one.
    fn captcha_solve(&self) -> Option<CaptchaSolve> {
        None
    }

    /// The seconds a top-level navigation or a frame's document load in flight in the active tab is still waited on:
    /// the page is on its way, and the end of that load is a lifecycle event. `None` when no load is, including one
    /// that has had as long as a navigation is given and so may never end.
    fn loading(&self) -> Option<f64> {
        None
    }
}

/// A search attempt keeps its receipt even when the provider refused it or returned an invalid response.
pub struct SearchOutcome {
    pub page_changed: bool,
    pub cost: crate::models::CostLine,
    pub error: Option<String>,
}

fn is_zero(value: &i32) -> bool {
    *value == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Advice is placed by its marks, not by its words: the page's own copy of the words stays page text.
    #[test]
    fn unmarked_advice_spans_only_what_the_reader_wrote() {
        let text = "東京\u{FDD0} (title: Tokyo)\u{FDD1}, 1887 and 東京 (title: Tokyo)";
        let (plain, spans) = unmark_advice(text, |words| words == "(title: Tokyo)");
        assert_eq!(plain, "東京 (title: Tokyo), 1887 and 東京 (title: Tokyo)");
        assert_eq!(spans, [(2, 17)]);
        let chars: Vec<char> = plain.chars().collect();
        assert_eq!(chars[2..17].iter().collect::<String>(), " (title: Tokyo)");
    }

    /// Whitespace before advice is the advice's, and the advice's own space is dropped where cleanup collapsed it.
    #[test]
    fn unmarked_advice_keeps_the_text_cleanup_wrote() {
        let (plain, spans) = unmark_advice("Boulenger\n\u{FDD0} (title: G)\u{FDD1}, 1887", |_| true);
        assert_eq!((plain.as_str(), spans.as_slice()), ("Boulenger\n(title: G), 1887", &[(9, 20)][..]));
        let (plain, spans) = unmark_advice("\u{FDD0} (2026)\u{FDD1}", |_| true);
        assert_eq!((plain.as_str(), spans.as_slice()), ("(2026)", &[(0, 6)][..]));
    }

    /// Marks a page wrote around words the reader never wrote are dropped and mark nothing.
    #[test]
    fn marks_around_words_the_reader_did_not_write_are_not_advice() {
        let (plain, spans) = unmark_advice("Price\u{FDD0} $10\u{FDD1}", |words| words == "(title: Tokyo)");
        assert_eq!((plain.as_str(), spans.len()), ("Price $10", 0));
    }

    #[test]
    fn document_title_secret_masking_keeps_the_title_span_valid() {
        let mut capture = Capture {
            url: "https://docs.test/".into(),
            title: "東京 private-token reference".into(),
            captured_at: DateTime::now_utc(),
            sha256: crate::py::sha256_hex(b""),
            text: String::new(),
            blocks: Vec::new(),
            inaccessible_frames: 0,
            masked: Vec::new(),
            advice: Vec::new(),
        }
        .with_document_title();
        let mut redactor = crate::safety::Redactor::new();
        redactor.register("token", "private-token", None);
        capture.masked = redactor.masked_spans(&capture.text);
        capture.text = redactor.mask(&capture.text);
        capture.title = redactor.mask(&capture.title);
        assert_eq!(capture.masked, [(3, 16)]);
        assert!(!capture.text.contains("private-token"));
        assert_eq!(capture.block_text(&capture.blocks[0]), capture.title);
        assert_eq!(text::len(&capture.text), text::len("東京 private-token reference"));
    }

    #[test]
    fn document_title_is_citable_metadata_without_shifting_body_spans() {
        let mut capture = Capture {
            url: "https://docs.test/".into(),
            title: "東京 🦀 reference".into(),
            captured_at: DateTime::now_utc(),
            sha256: crate::py::sha256_hex(b"Body"),
            text: "Body".into(),
            blocks: Vec::new(),
            inaccessible_frames: 0,
            masked: Vec::new(),
            advice: Vec::new(),
        };
        capture = capture.with_document_title();
        assert!(capture.text.starts_with("Body\n\n"));
        let title = capture.blocks.last().unwrap();
        assert_eq!(title.start, 6);
        assert_eq!(title.end - title.start, text::len(&capture.title) as i64);
        assert_eq!(capture.block_text(title), capture.title);
        assert!(title.heading_path.is_empty());
        assert_eq!(serde_json::to_value(title.kind).unwrap(), "document_title");
        assert_eq!(capture.sha256, crate::py::sha256_hex(capture.text.as_bytes()));
        capture.title.clear();
        let before = capture.clone();
        assert_eq!(capture.with_document_title(), before);
    }

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
    fn an_action_never_shows_its_text() {
        let mut action = Action::new(Operation::Fill);
        action.text = Some("hunter2".into());
        assert!(!format!("{action:?}").contains("hunter2"));
    }

    #[test]
    fn a_same_control_drag_path_defaults_shift_and_requires_a_move() {
        let value = serde_json::json!({
            "operation": "drag",
            "target_id": "pad",
            "drag_path": {
                "press": {"x": 0.1, "y": 0.2},
                "points": [{"x": 0.5, "y": 0.6}],
                "release": {"x": 0.9, "y": 0.8}
            }
        });
        let action: Action = serde_json::from_value(value).expect("drag action");
        assert_eq!(action.drag_path.as_ref().unwrap().points.len(), 1);
        assert!(!action.drag_path.as_ref().unwrap().shift);
        let schema = crate::schema::json_schema(&DragPath::ty());
        assert_eq!(schema["properties"]["points"]["minItems"], 1);
        assert!(schema["properties"]["points"]["maxItems"].is_null());
    }
}
