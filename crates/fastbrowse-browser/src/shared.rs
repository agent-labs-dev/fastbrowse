//! The types the browser layer shares with the rest of fastbrowse, ported from `models.py`, `page.py`,
//! `config.py` and `addresses.py`.
//!
//! This module is a stand-in: fastbrowse-core will own these types, and this file is replaced by its imports.
//! Names and fields mirror the Python so the swap is mechanical. A type cut to the fields the browser reads says
//! so, and nothing here carries behavior those modules own beyond what the browser itself calls. Serialization
//! follows pydantic's `model_dump(mode="json")`, field order included, so an observation dumps byte for byte as
//! Python's does.

use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize, Serializer};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// -- models.py ------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Click,
    Hover,
    Drag,
    Fill,
    Select,
    Enter,
    Escape,
    Scroll,
    ScrollUp,
    Back,
    SwitchTab,
    Upload,
    Dialog,
    Read,
    Done,
    Escalate,
    Navigate,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::Hover => "hover",
            Self::Drag => "drag",
            Self::Fill => "fill",
            Self::Select => "select",
            Self::Enter => "enter",
            Self::Escape => "escape",
            Self::Scroll => "scroll",
            Self::ScrollUp => "scroll_up",
            Self::Back => "back",
            Self::SwitchTab => "switch_tab",
            Self::Upload => "upload",
            Self::Dialog => "dialog",
            Self::Read => "read",
            Self::Done => "done",
            Self::Escalate => "escalate",
            Self::Navigate => "navigate",
        }
    }

    /// `TARGETED`: operations aimed at one observed control, which is hit-tested before input reaches it.
    pub fn targeted(self) -> bool {
        matches!(self, Self::Click | Self::Hover | Self::Drag | Self::Fill | Self::Select | Self::Enter | Self::Upload)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    Executed,
    /// The target was not the topmost element at its point; nothing was dispatched.
    Covered,
    /// The page changed between observation and dispatch; nothing was dispatched.
    Stale,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceControl {
    pub role: String,
    pub label: String,
    #[serde(default)]
    pub context: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Download,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub sha256: String,
    /// Reference returned by the caller's `ArtifactSink`; fastbrowse never keeps bytes past the run.
    pub uri: String,
}

/// `ArtifactSink`, the caller's store for downloaded bytes.
pub trait ArtifactSink: Send + Sync {
    fn put<'a>(
        &'a self,
        kind: ArtifactKind,
        name: &'a str,
        mime_type: &'a str,
        content: Vec<u8>,
    ) -> BoxFuture<'a, Result<Artifact, BoxError>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub name: String,
    pub mime_type: String,
    pub content: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BrowserConnection {
    /// Browser-level DevTools WebSocket URL (`ws://` or `wss://`).
    pub cdp_url: String,
    pub live_url: Option<String>,
    pub browser_id: Option<String>,
    /// True when the browser runs on another host: files move as bytes.
    pub remote: bool,
    /// The run brings its tab, and with it the browser window, to the front.
    pub foreground: bool,
    /// Attach to an existing window without creating a tab.
    pub attach: bool,
    pub target_match: Option<String>,
    /// Exact origins whose documents the run may inspect and control, normalized by `parse_origins`.
    pub allowed_origins: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocalChrome {
    pub binary: Option<String>,
    pub headed: bool,
    pub profile: Option<PathBuf>,
}

/// `FrameHandler`: receives JPEG bytes of the active tab.
pub type FrameHandler = Arc<dyn Fn(Vec<u8>) -> BoxFuture<'static, Result<(), BoxError>> + Send + Sync>;

/// `SecretValue`: a secret's value, or what computes it when it is typed.
#[derive(Clone)]
pub enum SecretValue {
    Value(String),
    Provider(Arc<dyn Fn() -> BoxFuture<'static, String> + Send + Sync>),
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the value: a debug print of a login must not leak it.
        f.write_str(match self {
            Self::Value(_) => "SecretValue::Value(..)",
            Self::Provider(_) => "SecretValue::Provider(..)",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostComponent {
    Jev,
    Llm,
    Browser,
    Proxy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostBasis {
    Metered,
    Estimated,
    Bounded,
    Unknown,
}

/// `CostLine`, cut to the fields a cloud browser's cost fills (no purpose, tokens or seconds).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostLine {
    pub component: CostComponent,
    pub basis: CostBasis,
    pub dollars: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decider {
    Jev,
    Llm,
}

/// `StepResult`, cut to the fields a recording captions a step with.
#[derive(Debug, Clone, PartialEq)]
pub struct StepResult {
    pub index: u32,
    pub operation: Operation,
    pub decided_by: Decider,
    pub target: Option<String>,
    pub duration_ms: u64,
}

// -- config.py --------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationLimits {
    pub max_controls: usize,
    pub max_offscreen_controls: usize,
    pub max_offered_controls: usize,
    pub viewport_text_chars: usize,
    pub working_notes_chars: usize,
    pub history_entries: usize,
    pub earlier_history_entries: usize,
    pub group_size: usize,
    pub max_choice_options: usize,
}

impl Default for ObservationLimits {
    fn default() -> Self {
        Self {
            max_controls: 320,
            max_offscreen_controls: 120,
            max_offered_controls: 160,
            viewport_text_chars: 6000,
            working_notes_chars: 6000,
            history_entries: 6,
            earlier_history_entries: 14,
            group_size: 30,
            max_choice_options: 240,
        }
    }
}

/// `Config`, cut to the fields the browser layer reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub observation: ObservationLimits,
    pub refuse_cookie_banners: bool,
    pub max_upload_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            observation: ObservationLimits::default(),
            refuse_cookie_banners: true,
            max_upload_bytes: 25 * 1024 * 1024,
        }
    }
}

// -- page.py ----------------------------------------------------------------------------------------------------

pub fn cut_marker(omitted_chars: usize) -> String {
    format!("\n[Viewport text cut: {omitted_chars} characters omitted; read the page for the rest]")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Control {
    /// Kept across observations for as long as the same DOM node survives.
    pub id: String,
    pub frame_id: Option<String>,
    pub frame_origin: Option<String>,
    pub form_id: Option<String>,
    /// Browser guard without node ids; pydantic excludes it from dumps.
    #[serde(skip_serializing, default)]
    pub retarget_key: Option<String>,
    pub role: String,
    pub label: String,
    pub context: Option<String>,
    pub operations: Vec<Operation>,
    pub value: Option<String>,
    pub href: Option<String>,
    pub options: Vec<String>,
    pub input_type: Option<String>,
    pub input_name: Option<String>,
    pub autocomplete: Option<String>,
    pub submit_semantics: Option<String>,
    pub checked: Option<bool>,
    pub selected: Option<bool>,
    pub expanded: Option<bool>,
    pub sensitive: bool,
    pub offscreen: bool,
    pub blocking: Option<bool>,
    pub next_page: Option<bool>,
    pub download: Option<bool>,
}

const ARROWS: [char; 4] = ['\u{203a}', '\u{bb}', '\u{2192}', '>'];

/// `" ".join(label.split())`: Python's `str.split()` breaks on every Unicode whitespace run.
fn squeeze(label: &str) -> String {
    label.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `_NEXT_PAGE.fullmatch`: `(?:next(?: page)?|more results|older(?: posts)?)\s*[arrows]*|[arrows]{1,2}`,
/// ignoring case.
fn next_page_label(label: &str) -> bool {
    let lower = label.to_lowercase();
    let arrows = lower.chars().take_while(|c| ARROWS.contains(c)).count();
    if arrows == lower.chars().count() && (1..=2).contains(&arrows) {
        return true;
    }
    ["next page", "next", "more results", "older posts", "older"]
        .iter()
        .any(|word| lower.strip_prefix(word).is_some_and(|rest| rest.trim_start().chars().all(|c| ARROWS.contains(&c))))
}

/// Whether a link is a pager's way to the next page of a list: marked `rel="next"`, or labelled like one.
pub fn pages_forward(control: &Control) -> bool {
    control.next_page == Some(true) || next_page_label(&squeeze(&control.label))
}

/// `_LOAD_MORE.fullmatch`: `(?:show|view|load|see)(?: \d+)? more(?: [\w-]+){0,3}`, ignoring case.
fn load_more_label(label: &str) -> bool {
    let lower = label.to_lowercase();
    let mut words = lower.split(' ');
    if !matches!(words.next(), Some("show" | "view" | "load" | "see")) {
        return false;
    }
    let mut next = words.next();
    if next.is_some_and(|w| !w.is_empty() && w.chars().all(char::is_numeric)) {
        next = words.next();
    }
    if next != Some("more") {
        return false;
    }
    let rest: Vec<&str> = words.collect();
    rest.len() <= 3
        && rest.iter().all(|w| !w.is_empty() && w.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-'))
}

/// Whether a control is labelled as loading more of the list it ends: "View more flights", "Show 20 more".
pub fn loads_more(control: &Control) -> bool {
    control.operations.contains(&Operation::Click) && load_more_label(&squeeze(&control.label))
}

/// Whether this control is one the loop could walk a list with: a link, to an address.
pub fn pager_link(control: &Control) -> bool {
    control.role == "link"
        && control.operations.contains(&Operation::Click)
        && control.href.as_deref().is_some_and(|h| !h.is_empty())
        && pages_forward(control)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    pub id: String,
    pub url: String,
    pub title: String,
    pub active: bool,
    pub opener_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dialog {
    pub kind: String,
    pub message: String,
    pub default_prompt: Option<String>,
}

/// A UTC instant, dumped as pydantic dumps an aware `datetime`: `2026-10-23T12:34:56.123456Z`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp {
    pub micros: i64,
}

impl Timestamp {
    pub fn now() -> Self {
        let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        Self { micros: i64::try_from(since.as_micros()).unwrap_or(i64::MAX) }
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seconds = self.micros.div_euclid(1_000_000);
        let micros = self.micros.rem_euclid(1_000_000);
        let (days, second) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
        // Howard Hinnant's civil_from_days.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        let (h, m, s) = (second / 3600, second / 60 % 60, second % 60);
        write!(f, "{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}")?;
        // pydantic drops a zero fraction, as `datetime.isoformat` does.
        if micros != 0 {
            write!(f, ".{micros:06}")?;
        }
        f.write_str("Z")
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Observation {
    pub url: String,
    pub title: String,
    /// Fingerprint of the observed page. `act` refuses a key other than the latest observation's.
    pub page_key: String,
    pub captured_at: Timestamp,
    pub response_status: Option<u16>,
    pub document_key: String,
    pub controls: Vec<Control>,
    pub omitted_controls: usize,
    pub viewport_text: String,
    pub tabs: Vec<Tab>,
    pub dialog: Option<Dialog>,
    pub inaccessible_frames: usize,
    pub can_go_back: bool,
    pub history_entry: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Heading,
    Paragraph,
    ListItem,
    Table,
    Record,
    Code,
    Link,
    Observation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Block {
    pub source_id: String,
    pub source_url: Option<String>,
    pub complete_source: bool,
    pub kind: BlockKind,
    pub frame_id: Option<String>,
    /// Offsets into `Capture.text` in Unicode code points, as Python indexes a `str`.
    pub start: usize,
    pub end: usize,
    pub heading_path: Vec<String>,
    pub control_context: Option<SourceControl>,
    pub href: Option<String>,
    pub list_id: Option<String>,
    pub list_count: Option<u64>,
    pub list_frameless: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Capture {
    pub url: String,
    pub title: String,
    pub captured_at: Timestamp,
    /// SHA-256 of `text`; evidence records it so a quote can be re-validated later.
    pub sha256: String,
    pub text: String,
    pub blocks: Vec<Block>,
    pub inaccessible_frames: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub operation: Operation,
    pub url: Option<String>,
    pub target_id: Option<String>,
    /// Drop target of a drag.
    pub destination_id: Option<String>,
    /// Value to type or option to select. May hold a resolved secret: never log or persist an Action.
    pub text: Option<String>,
    pub secret: bool,
    pub tab_id: Option<String>,
    pub secret_origin: Option<String>,
    pub files: Vec<Attachment>,
    pub accept_dialog: Option<bool>,
    pub form_fill: bool,
}

impl Action {
    pub fn new(operation: Operation) -> Self {
        Self {
            operation,
            url: None,
            target_id: None,
            destination_id: None,
            text: None,
            secret: false,
            tab_id: None,
            secret_origin: None,
            files: vec![],
            accept_dialog: None,
            form_fill: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActResult {
    pub outcome: StepOutcome,
    pub page_changed: bool,
    pub detail: Option<String>,
    pub form_unchanged: bool,
    pub moved: bool,
}

/// Every browser failure a `Page` raises, with a message safe to put in a run result: never page text.
///
/// The first six are `page.py`'s `BrowserError` and its subclasses, which fastbrowse-core will own. The rest are
/// the browser layer's own subclasses (`session.py`'s `BrowserUnresponsive`, `PageUnresponsive` and
/// `OriginNotAllowed`, `page.py`'s `_ScriptError`), and stay here when the swap is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserError {
    Browser(String),
    /// CDP's -32000: the document a command addressed was replaced while it ran.
    ContextLost(String),
    /// `BrowserUnavailable`: the session has no page left for this run. Also `Unavailable`.
    BrowserUnavailable(String),
    ScreenshotsUnavailable(String),
    NavigationTimeout(String),
    SiteUnreachable(String),
    /// The browser stopped answering: an outage. Also `Unavailable`.
    BrowserUnresponsive(String),
    /// A tab that stopped answering while the browser behind it still answers.
    PageUnresponsive(String),
    /// A document outside `allowed_origins`. The message never names it: an address can carry a credential.
    OriginNotAllowed,
    /// A script this layer evaluated threw in the page.
    Script,
}

impl BrowserError {
    /// Whether Python's `isinstance(error, Unavailable)` holds: an outage, not a failed task.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::BrowserUnavailable(_) | Self::BrowserUnresponsive(_))
    }

    pub fn is_context_lost(&self) -> bool {
        matches!(self, Self::ContextLost(_))
    }

    /// The Python class name, for tests and the differential check.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Browser(_) => "BrowserError",
            Self::ContextLost(_) => "ContextLost",
            Self::BrowserUnavailable(_) => "BrowserUnavailable",
            Self::ScreenshotsUnavailable(_) => "ScreenshotsUnavailable",
            Self::NavigationTimeout(_) => "NavigationTimeout",
            Self::SiteUnreachable(_) => "SiteUnreachable",
            Self::BrowserUnresponsive(_) => "BrowserUnresponsive",
            Self::PageUnresponsive(_) => "PageUnresponsive",
            Self::OriginNotAllowed => "OriginNotAllowed",
            Self::Script => "_ScriptError",
        }
    }
}

impl fmt::Display for BrowserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Browser(m)
            | Self::ContextLost(m)
            | Self::BrowserUnavailable(m)
            | Self::ScreenshotsUnavailable(m)
            | Self::NavigationTimeout(m)
            | Self::SiteUnreachable(m)
            | Self::BrowserUnresponsive(m)
            | Self::PageUnresponsive(m) => f.write_str(m),
            Self::OriginNotAllowed => f.write_str("the page is outside allowed_origins"),
            Self::Script => f.write_str("Runtime.evaluate failed (JavaScriptError)"),
        }
    }
}

impl std::error::Error for BrowserError {}

// -- addresses.py -----------------------------------------------------------------------------------------------

/// The schemes `urlunsplit` writes `//` for when the netloc is empty (CPython's `uses_netloc`).
const USES_NETLOC: &[&str] = &[
    "",
    "ftp",
    "http",
    "gopher",
    "nntp",
    "telnet",
    "imap",
    "wais",
    "file",
    "mms",
    "https",
    "shttp",
    "snews",
    "prospero",
    "rtsp",
    "rtsps",
    "rtspu",
    "rsync",
    "svn",
    "svn+ssh",
    "sftp",
    "nfs",
    "git",
    "git+ssh",
    "ws",
    "wss",
    "itms-services",
];

/// CPython's `urlsplit`: (scheme, netloc, path, query, fragment).
pub fn urlsplit(url: &str) -> (String, String, String, String, String) {
    // The WHATWG C0 controls and space are stripped from the front, and tabs and newlines anywhere.
    let url: String =
        url.trim_start_matches(|c: char| c <= ' ').chars().filter(|c| !matches!(c, '\t' | '\r' | '\n')).collect();
    let mut rest = url.as_str();
    let mut scheme = String::new();
    if let Some(colon) = rest.find(':') {
        let candidate = &rest[..colon];
        if candidate.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && candidate.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        {
            scheme = candidate.to_ascii_lowercase();
            rest = &rest[colon + 1..];
        }
    }
    let mut netloc = String::new();
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = after[..end].to_owned();
        rest = &after[end..];
    }
    let (rest, fragment) = rest.split_once('#').unwrap_or((rest, ""));
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    (scheme, netloc, path.to_owned(), query.to_owned(), fragment.to_owned())
}

/// `search_address`: the address with its query and fragment, normalised as `urlsplit(url).geturl()` does.
pub fn search_address(url: &str) -> String {
    let (scheme, netloc, path, query, fragment) = urlsplit(url);
    let mut out = path;
    if !netloc.is_empty() {
        if !out.is_empty() && !out.starts_with('/') {
            out.insert(0, '/');
        }
        out = format!("//{netloc}{out}");
    } else if out.starts_with("//")
        || (!scheme.is_empty() && USES_NETLOC.contains(&scheme.as_str()) && (out.is_empty() || out.starts_with('/')))
    {
        out = format!("//{out}");
    }
    if !scheme.is_empty() {
        out = format!("{scheme}:{out}");
    }
    if !query.is_empty() {
        out = format!("{out}?{query}");
    }
    if !fragment.is_empty() {
        out = format!("{out}#{fragment}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control(role: &str, label: &str, href: Option<&str>) -> Control {
        Control {
            id: "main:1".into(),
            frame_id: None,
            frame_origin: None,
            form_id: None,
            retarget_key: None,
            role: role.into(),
            label: label.into(),
            context: None,
            operations: vec![Operation::Click],
            value: None,
            href: href.map(Into::into),
            options: vec![],
            input_type: None,
            input_name: None,
            autocomplete: None,
            submit_semantics: None,
            checked: None,
            selected: None,
            expanded: None,
            sensitive: false,
            offscreen: false,
            blocking: None,
            next_page: None,
            download: None,
        }
    }

    #[test]
    fn pager_labels_match_as_python_regexes_do() {
        for label in
            ["Next", "next page", "Next \u{203a}", "\u{bb}", ">>", "More results", "Older posts \u{2192}", "next  >"]
        {
            assert!(pager_link(&control("link", label, Some("/p2"))), "{label}");
        }
        for label in ["Next steps", ">>>", "", "Page 2"] {
            assert!(!pager_link(&control("link", label, Some("/p2"))), "{label}");
        }
        assert!(!pager_link(&control("button", "Next", Some("/p2"))));
        assert!(!pager_link(&control("link", "Next", None)));
    }

    #[test]
    fn load_more_labels_match_as_python_regexes_do() {
        for label in ["View more flights", "Show 20 more", "see more", "Load more of these-things now"] {
            assert!(loads_more(&control("button", label, None)), "{label}");
        }
        for label in ["Learn more", "Show more of these four extra words", "more"] {
            assert!(!loads_more(&control("button", label, None)), "{label}");
        }
    }

    #[test]
    fn search_address_normalises_as_geturl() {
        assert_eq!(search_address("HTTP://Example.com/a?#"), "http://Example.com/a");
        assert_eq!(search_address("https://x.test/p?q=1#f"), "https://x.test/p?q=1#f");
        assert_eq!(search_address("about:blank"), "about:blank");
        assert_eq!(search_address("file:///tmp/x"), "file:///tmp/x");
    }

    #[test]
    fn timestamps_dump_as_pydantic_does() {
        assert_eq!(Timestamp { micros: 0 }.to_string(), "1970-01-01T00:00:00Z");
        assert_eq!(Timestamp { micros: 1_792_756_496_123_456 }.to_string(), "2026-10-23T11:54:56.123456Z");
    }
}
