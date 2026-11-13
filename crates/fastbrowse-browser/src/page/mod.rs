//! `CdpPage`: the `Page` seam on a `BrowserSession`.
//!
//! Freshness (page_key plus a per-element guard), hit-test-before-input (covered detection), scroll-into-view for
//! offscreen targets and password masking are ported from jev-ultrafast (MIT):
//! jev_ultrafast/browser.py and jev_ultrafast/snapshot.js. `act` dispatches at most once per call.

pub(crate) mod act;
mod delta;
mod pointer;
pub mod scripts;

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;

use data_encoding::{BASE64, HEXLOWER};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::addresses::search_address;
use fastbrowse_core::config::Config;
use fastbrowse_core::decision::JEV_IMAGE_SIDE;
use fastbrowse_core::models::{Artifact, ArtifactKind, Link, Operation, SourceControl};
use fastbrowse_core::origins::origin_of;
use fastbrowse_core::page::json as json_document;
use fastbrowse_core::page::{
    ActResult, Action, Block, BlockKind, CaptchaSolve, Capture, Control, Dialog, Media, Observation, Page, PageResult,
};
use fastbrowse_core::py::datetime::DateTime;
use fastbrowse_core::py::text;
use ring::digest::{SHA256, digest};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::time::{Instant, sleep, timeout};

use crate::PdfUnread;
use crate::cdp::secs;
use crate::error::BrowserError;
use crate::pdf::PARSE_SECONDS;
use crate::session::{
    BrowserSession, CAPTURE_JS, LISTENERS_UNREAD, Refusal, SNAPSHOT_UNINSTALLED, STATUS_WORLD, py_json,
};
use crate::urlsplit::urlsplit;
use scripts::py_float;

pub(crate) const SETTLE_SECONDS: f64 = 5.0;
/// How long the document must have gone without a mutation or input before a settle reports it quiet. This is the
/// page's own event stream answering, not a poll: snapshot.js stamps `registry.lastMutation` from its mutation
/// observer and input listeners, so a settle waits on one promise instead of being sent again until two reads agree.
/// A click that mutates settles `SETTLE_QUIET_SECONDS` after its last write; a page that never mutates after the
/// action has no such wait at all.
pub(crate) const SETTLE_QUIET_SECONDS: f64 = 0.2;
/// How long a visible progress indicator outranks a quiet DOM, inside `SETTLE_SECONDS`. A spinner settles the
/// fingerprint while the page it waits for does not exist yet, but a page that spins forever must not pay the whole
/// settle budget on every action, so past this quiet decides alone. Only `aria-busy`, a progressbar role and a
/// `progress` element are read: #501 removed the guessed class and id names, which matched content.
pub(crate) const SETTLE_LOADING_SECONDS: f64 = 1.5;
/// A document can replace itself while its snapshot is being read. Restart that read after the replacement settles,
/// and report failure if replacement continues. Run steps, time and spend are counted separately.
pub(crate) const REREADS: u32 = 10;
/// How long one child frame's page script may take before the read goes on without it. A child frame that stops
/// answering (an advertisement's renderer stuck in a loop) left every observation waiting out the whole CDP command
/// deadline, two minutes, and one login run spent 26 minutes that way while the page it was signing into answered at
/// once. A frame past this is reported unread, as a cross-origin frame that cannot be read is.
pub(crate) const FRAME_READ_SECONDS: f64 = 5.0;
/// What a frame that missed `FRAME_READ_SECONDS` gets on later reads, until it answers again or
/// `FRAME_RECHECK_SECONDS` have passed.
pub(crate) const FRAME_RETRY_SECONDS: f64 = 0.5;
/// How long a frame that missed its deadline keeps the short budget. A frame that recovers but still answers slowly,
/// as over a distant cloud connection, would otherwise never be read again for the rest of the run.
pub(crate) const FRAME_RECHECK_SECONDS: f64 = 30.0;
pub(crate) const SCREENSHOT_WAIT_SECONDS: f64 = 1.0;
/// How long a background tab gets to paint a frame once its cast is restarted. Nothing else can make it paint without
/// bringing its window forward, so past this the run goes on without the image.
pub(crate) const SCREENSHOT_STALL_SECONDS: f64 = 5.0;
/// Times an observation is read again when its history entry and its snapshot name different addresses.
pub(crate) const HISTORY_REREADS: u32 = 2;
/// A proxy connection can fail transiently, and a slow page needs longer to become ready after a retry. Each try
/// doubles its readiness wait so restarting the load does not repeatedly interrupt the same slow scripts.
pub(crate) const NAVIGATE_ATTEMPTS: u32 = 4;
/// Doubled after each failed try.
pub(crate) const NAVIGATE_RETRY_SECONDS: f64 = 1.0;
/// The readiness wait of a navigation's first try.
pub(crate) const LOAD_TIMEOUT_SECONDS: f64 = 15.0;
/// How long a load is waited on before it is no longer on its way: every try `navigate` gives one, added up. A
/// server that never answers leaves Chrome loading with no further event, so nothing else ends it; a page that keeps
/// navigating, such as a challenge, starts a new load each time and with it a new wait.
pub(crate) const LOAD_AWAITED_SECONDS: f64 = LOAD_TIMEOUT_SECONDS * ((1u32 << NAVIGATE_ATTEMPTS) - 1) as f64;
/// Whether Chrome's error names the site or the connection to it giving nothing, rather than the address being wrong.
///
/// A certificate or TLS failure belongs here because the agent cannot repair the site's connection. A mistyped
/// address gives `ERR_NAME_NOT_RESOLVED`, which stays a plain failure because the address is the caller's own.
fn site_unreachable(name: &str) -> bool {
    UNREACHABLE.contains(&name) || name.starts_with("CERT_") || name.starts_with("SSL_")
}

/// The error a document navigation ends with when its response turned out to be a download, not a page.
const ABORTED: &str = "net::ERR_ABORTED";
/// The site or the connection to it gave nothing: the-internet.herokuapp.com answered ERR_EMPTY_RESPONSE for a
/// quarter hour of one eval. Not ERR_NAME_NOT_RESOLVED, which a mistyped address also gives.
const UNREACHABLE: [&str; 9] = [
    "EMPTY_RESPONSE",
    "CONNECTION_RESET",
    "CONNECTION_CLOSED",
    "CONNECTION_REFUSED",
    "CONNECTION_TIMED_OUT",
    "TIMED_OUT",
    "TUNNEL_CONNECTION_FAILED",
    "PROXY_CONNECTION_FAILED",
    "NETWORK_CHANGED",
];
/// Frame key used for the top frame; OOPIF frames key on their CDP target id, per the browser session.
pub(crate) const MAIN: &str = "main";

/// What `act` needs to re-find and freshness-check a control: (session id, frame key, local node id, guard digest).
pub(crate) type Target = (String, String, i64, Option<String>);

/// What `act` needs to re-find and freshness-check the exact control an `Observation` named.
#[derive(Debug, Clone, Default)]
pub(crate) struct ObservedState {
    pub page_key: String,
    pub controls: HashMap<String, Target>,
    /// frame_key -> the labels of the controls observed in that frame, for naming what a covering frame offers.
    pub labels: HashMap<String, Vec<String>>,
}

/// One control as snapshot.js reports it; `id` is the frame-local node id.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotControl {
    id: i64,
    frame_path: Option<String>,
    frame_origin: Option<String>,
    form_id: Option<String>,
    role: String,
    label: String,
    context: Option<String>,
    operations: Vec<Operation>,
    value: Option<String>,
    focused: Option<bool>,
    href: Option<String>,
    #[serde(default)]
    options: Vec<String>,
    input_type: Option<String>,
    input_name: Option<String>,
    #[serde(default)]
    max_length: Option<usize>,
    autocomplete: Option<String>,
    submit_semantics: Option<String>,
    checked: Option<bool>,
    selected: Option<bool>,
    expanded: Option<bool>,
    #[serde(default)]
    sensitive: bool,
    #[serde(default)]
    offscreen: bool,
    required: Option<bool>,
    invalid: Option<bool>,
    validation_message: Option<String>,
    next_page: Option<bool>,
    download: Option<bool>,
}

pub(crate) trait FrameText: DeserializeOwned {
    const SHAPE: &'static str;
    fn url(&self) -> &str;
    fn title(&self) -> &str;
    /// A shape that keeps reads serves the validated parse of its named baseline.
    fn from_kept(_kept: &KeptRead) -> Option<Arc<Self>> {
        None
    }
    /// None removes the slot: this shape or this read has no numbered baseline to serve.
    fn keep(_raw: Value, _parsed: &Arc<Self>) -> Option<KeptRead> {
        None
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentResponse {
    url: String,
    response_status: Option<i64>,
    document_key: String,
}

/// A document's response read, kept with the tab and the document mark it was read at.
///
/// The status of a document is fixed once it is served, and the read costs an isolated-world evaluate on every
/// observation. An observation whose document is the one already read keeps that read; a document the read does not
/// name is read again rather than assumed.
#[derive(Debug, Clone)]
pub(crate) struct KeptDocument {
    pub(crate) session_id: String,
    pub(crate) mark: u64,
    pub(crate) document: DocumentResponse,
}

/// snapshot.js output for one frame.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    url: String,
    title: String,
    #[serde(default)]
    inaccessible_frames: usize,
    viewport_text: String,
    /// Characters of visible text outside the viewport.
    #[serde(default)]
    offscreen_text: String,
    document_key: String,
    /// 1 when the viewport shows a picture, chart, drawing or video that no control carries; snapshot.js sends 0/1.
    #[serde(default)]
    visual_content: i64,
    /// The open modal's name, empty when it has none; null when no modal is open.
    #[serde(default)]
    modal: Option<String>,
    #[serde(default)]
    media: Vec<Media>,
    controls: Vec<SnapshotControl>,
    page_key: String,
    /// Digest of each control's freshness guard, keyed by its local id as a string (a JSON object key).
    guards: HashMap<String, Option<String>>,
    /// Digest of the guard without the node ids at its ends, which a redraw changes.
    retarget_keys: HashMap<String, Option<String>>,
    /// The page's number for this read when it kept it; null for a read it did not keep.
    #[serde(default)]
    read: Option<u64>,
}

impl FrameText for Snapshot {
    fn title(&self) -> &str {
        &self.title
    }
    fn from_kept(kept: &KeptRead) -> Option<Arc<Self>> {
        Some(Arc::clone(&kept.parsed))
    }
    fn keep(raw: Value, parsed: &Arc<Self>) -> Option<KeptRead> {
        parsed.read.map(|_| KeptRead { raw, parsed: Arc::clone(parsed) })
    }
    const SHAPE: &'static str = "_Snapshot";
    fn url(&self) -> &str {
        &self.url
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotBlock {
    kind: BlockKind,
    text: String,
    #[serde(default)]
    heading_path: Vec<String>,
    control_context: Option<SourceControl>,
    frame_path: Option<String>,
    #[serde(default)]
    source_path: String,
    href: Option<String>,
    /// Each link's text and address, in the order the block shows them.
    #[serde(default)]
    links: Vec<SnapshotLink>,
    /// The block is a record's whole text, its textless parts named.
    #[serde(default)]
    whole_record: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotLink {
    text: String,
    href: String,
}

/// capture.js output for one frame.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureSnapshot {
    url: String,
    title: String,
    #[serde(default)]
    inaccessible_frames: usize,
    blocks: Vec<SnapshotBlock>,
    /// The frame document's MIME type, as `document.contentType` gives it.
    #[serde(default)]
    content_type: String,
    /// The served text of a JSON document, as Chrome renders it into the body's first `pre`.
    #[serde(default)]
    json_text: Option<String>,
}

impl CaptureSnapshot {
    /// A JSON document's blocks by its structure, as a browserless fetch reads it, when the document's type is JSON
    /// and all of its served text parses. Any other document is read by its rendered blocks.
    fn json_blocks(&self) -> Option<Vec<json_document::Found>> {
        if !json_document::is_json(&self.content_type) {
            return None;
        }
        json_document::blocks(self.json_text.as_deref()?).filter(|found| !found.is_empty())
    }
}

impl FrameText for CaptureSnapshot {
    fn title(&self) -> &str {
        &self.title
    }
    const SHAPE: &'static str = "_CaptureSnapshot";
    fn url(&self) -> &str {
        &self.url
    }
}

/// Raw delta rows and their validated snapshot stay together so an unchanged read needs no second parse.
pub(crate) struct KeptRead {
    raw: Value,
    parsed: Arc<Snapshot>,
}

/// One frame's page-script output, tagged with how to reach that frame again.
struct FrameObservation<T> {
    frame_id: Option<String>,
    session_id: String,
    raw: Arc<T>,
}

/// The page's own text, then each cross-origin frame's in `frames` order under a `[Frame: origin]` line. Only the
/// origin is named: a frame's address often carries tokens. A frame with no text adds no line.
fn frame_text(frames: &[(String, FrameObservation<Snapshot>)], part: fn(&Snapshot) -> &str) -> String {
    let mut text = frames.iter().find(|(key, _)| key == MAIN).map(|(_, f)| part(&f.raw).to_owned()).unwrap_or_default();
    for (_, frame) in frames.iter().filter(|(key, _)| key != MAIN) {
        let own = part(&frame.raw);
        if own.trim().is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        let origin = origin_of(&frame.raw.url).unwrap_or_else(|| "embedded page".into());
        text.push_str(&format!("[Frame: {origin}]\n{own}"));
    }
    text
}

/// The tab's history read: whether BACK is offered, which entry of which tab the page is, and its address.
pub type HistoryRead = (bool, Option<String>, Option<String>);

#[derive(Debug, Clone, PartialEq)]
enum BackTarget {
    Entry(i64),
    Skipped(String),
}

#[derive(Default)]
pub(crate) struct PageState {
    pub(crate) last: Option<ObservedState>,
    /// The count of navigations the run's input had begun, and the page's address, when the last action began. A
    /// dialog blocks the page, so accepting one is judged against this mark, and only when the dialog came from that
    /// same address: a dialog a page opened as it loaded after a navigation is not the action's, and accepting it
    /// moved nothing.
    pub(crate) marked: Option<(u32, String)>,
    pub(crate) back_to: HashMap<String, String>,
    /// Child frame session -> when its last read missed the full deadline.
    pub(crate) unanswered: HashMap<String, Instant>,
    /// Tab session -> the unique id of the isolated world its HTTP status is read in.
    pub(crate) status_worlds: HashMap<String, String>,
    /// The last document response read, so an observation of the same document sends no status read.
    pub(crate) document: Option<KeptDocument>,
    /// Frame session, when it was read, and the viewport size that pre-action measurement already returned. The wheel
    /// and the pointer marks both need it, and asking again was a command of its own on every action.
    pub(crate) viewport: Option<(String, Instant, (f64, f64))>,
    /// `<shape>|<slot>|<frame>` -> the read last kept for that frame. A frame answers "unchanged" only about the
    /// read it kept, so this is what such an answer is served from.
    pub(crate) kept: HashMap<String, Arc<KeptRead>>,
}

pub struct CdpPage {
    session: BrowserSession,
    config: Config,
    pub(crate) page_js: String,
    capture_js: String,
    pub(crate) fingerprint_js: String,
    pub(crate) state: Mutex<PageState>,
    pub(crate) pointer_marks: Option<Arc<Mutex<Vec<crate::pointer::PointerMark>>>>,
    #[cfg(feature = "cursor")]
    cursor: Option<Arc<crate::cursor::CursorFeedback>>,
    #[cfg(feature = "cursor")]
    cursor_pid: Arc<Mutex<Option<i64>>>,
}

/// Python's truthiness of a JSON value.
pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Whether `previous_url` is http(s) and shares scheme, host and effective port with `current_url`.
///
/// A wizard that never leaves its one URL has no earlier entry but `about:blank`, and a link out to another site sits
/// before an entry on this one: neither is a page BACK can return the task to.
fn same_http_origin(previous_url: &str, current_url: &str) -> bool {
    fn origin(url: &str) -> Option<(String, Option<String>, Option<u32>)> {
        let (scheme, netloc, ..) = urlsplit(url);
        // `hostname` and `port` as urlsplit reads them from the netloc: after userinfo, brackets kept off, lowered.
        let host_port = netloc.rsplit_once('@').map_or(netloc.as_str(), |(_, h)| h);
        let (host, port) = if let Some(rest) = host_port.strip_prefix('[') {
            let (host, after) = rest.split_once(']').unwrap_or((rest, ""));
            (host.to_owned(), after.strip_prefix(':').map(str::to_owned))
        } else {
            match host_port.rsplit_once(':') {
                Some((h, p)) => (h.to_owned(), Some(p.to_owned())),
                None => (host_port.to_owned(), None),
            }
        };
        let port = match port.as_deref() {
            None | Some("") => None,
            Some(p) => Some(p.parse::<u32>().ok().filter(|p| *p <= 65_535)?),
        };
        let default = match scheme.as_str() {
            "http" => Some(80),
            "https" => Some(443),
            _ => None,
        };
        let host = (!host.is_empty()).then(|| host.to_lowercase());
        Some((scheme, host, port.or(default)))
    }
    let previous_scheme = urlsplit(previous_url).0;
    if previous_scheme != "http" && previous_scheme != "https" {
        return false;
    }
    match (origin(previous_url), origin(current_url)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// The control a snapshot row names in `frame`, recorded in `control_state` for `act` to find again.
fn add_control(
    control_state: &mut HashMap<String, Target>,
    frame_key: &str,
    frame: &FrameObservation<Snapshot>,
    c: &SnapshotControl,
    guard: Option<String>,
    retarget: Option<String>,
) -> Control {
    let control_id = format!("{frame_key}:{}", c.id);
    let fid = match c.frame_path.as_deref().filter(|p| !p.is_empty()) {
        Some(path) => Some(format!("{frame_key}/{path}")),
        None => frame.frame_id.clone(),
    };
    control_state.insert(control_id.clone(), (frame.session_id.clone(), frame_key.to_owned(), c.id, guard));
    control_from_raw(control_id, fid, c, retarget)
}

fn control_from_raw(
    control_id: String,
    frame_id: Option<String>,
    c: &SnapshotControl,
    retarget: Option<String>,
) -> Control {
    Control {
        id: control_id,
        frame_id,
        frame_origin: c.frame_origin.clone(),
        form_id: c.form_id.clone(),
        retarget_key: retarget,
        role: c.role.clone(),
        label: c.label.clone(),
        context: c.context.clone(),
        operations: c.operations.iter().copied().collect(),
        value: c.value.clone(),
        href: c.href.clone(),
        options: c.options.clone(),
        input_type: c.input_type.clone(),
        input_name: c.input_name.clone(),
        max_length: c.max_length,
        autocomplete: c.autocomplete.clone(),
        submit_semantics: c.submit_semantics.clone(),
        checked: c.checked,
        selected: c.selected,
        expanded: c.expanded,
        sensitive: c.sensitive,
        offscreen: c.offscreen,
        required: c.required,
        invalid: c.invalid,
        validation_message: c.validation_message.clone(),
        focused: c.focused,
        next_page: c.next_page,
        download: c.download,
        already_read: None,
        shown_by: None,
    }
}

/// A byte count in the unit a person reads it in.
fn bytes_of(count: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB"];
    let mut size = count as f64;
    for (index, unit) in units.iter().enumerate() {
        if size < 1024.0 || index == units.len() - 1 {
            return if index == 0 { format!("{size:.0} {unit}") } else { format!("{size:.1} {unit}") };
        }
        size /= 1024.0;
    }
    format!("{size:.1} GiB")
}

fn combine_page_keys(mut keys: Vec<String>) -> String {
    keys.sort();
    sha256_hex(&keys.join("|"))
}

pub(crate) fn sha256_hex(text: &str) -> String {
    sha256_bytes(text.as_bytes())
}

pub(crate) fn sha256_bytes(bytes: &[u8]) -> String {
    HEXLOWER.encode(digest(&SHA256, bytes).as_ref())
}

fn parse<T: DeserializeOwned>(raw: Value, shape: &str) -> Result<T, BrowserError> {
    // Our own page script produced this, so a mismatch is a bug in one side of the contract.
    serde_json::from_value(raw)
        .map_err(|e| BrowserError::Failed(format!("page script returned an unexpected {shape}: {e}")))
}

/// A length or offset as core's models hold it, the `int` Python writes.
fn int(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

impl CdpPage {
    pub fn new(session: BrowserSession, config: Config) -> Self {
        let origins = match session.grant_origins() {
            None => "null".to_owned(),
            Some(origins) => py_json(&json!(origins)),
        };
        let page_js = session.snapshot().function.clone();
        let capture_js = format!("({CAPTURE_JS})({origins})");
        let fingerprint_js = format!("{page_js}('fingerprint').fingerprint");
        Self {
            session,
            config,
            page_js,
            capture_js,
            fingerprint_js,
            state: Mutex::default(),
            pointer_marks: None,
            #[cfg(feature = "cursor")]
            cursor: None,
            #[cfg(feature = "cursor")]
            cursor_pid: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_pointer_marks(mut self, marks: Arc<Mutex<Vec<crate::pointer::PointerMark>>>) -> Self {
        self.pointer_marks = Some(marks);
        self
    }

    #[cfg(feature = "cursor")]
    pub fn with_cursor(mut self, cursor: Option<Arc<crate::cursor::CursorFeedback>>) -> Self {
        self.cursor = cursor;
        self
    }

    /// Native messaging cannot call browser-level SystemInfo, so the host supplies Chrome's process id.
    #[cfg(feature = "cursor")]
    pub fn with_cursor_browser_pid(self, pid: Option<u32>) -> Self {
        *self.cursor_pid.lock().unwrap() = pid.map(i64::from);
        self
    }

    pub fn session(&self) -> &BrowserSession {
        &self.session
    }

    pub fn artifacts(&self) -> Vec<Artifact> {
        self.session.artifacts()
    }

    /// Hold back live and recorded frames while the page may show a secret, which pixels cannot mask.
    pub fn withhold_frames(&self, withheld: bool) {
        self.session.set_frames_withheld(withheld);
    }

    fn last(&self) -> Option<ObservedState> {
        self.state.lock().unwrap().last.clone()
    }

    fn set_last(&self, last: ObservedState) {
        self.state.lock().unwrap().last = Some(last);
    }

    // -- observe / capture ----------------------------------------------------------------------------------

    pub async fn observe(&self) -> Result<Observation, BrowserError> {
        self.session.choosers_pinned().await;
        self.reread(|| self.observe_once(0, true)).await
    }

    /// The current page for a screenshot's secret check, without changing the observation an action refers to.
    pub async fn glance(&self) -> Result<Observation, BrowserError> {
        self.reread(|| self.observe_once(0, false)).await
    }

    /// Run `read`, and again on the next settled document each time a navigation destroys the one it read.
    async fn reread<T, F, Fut>(&self, read: F) -> Result<T, BrowserError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<T, BrowserError>>,
    {
        let mut rereads = 0;
        loop {
            match read().await {
                Err(BrowserError::ContextLost(lost)) => {
                    if rereads == REREADS {
                        return Err(BrowserError::ContextLost(format!(
                            "{lost} on {} reads in a row, each after waiting for the page to settle",
                            rereads + 1
                        )));
                    }
                }
                other => return other,
            }
            rereads += 1;
            self.await_document().await?;
        }
    }

    /// Wait for the document that replaced the one a read lost to be interactive and quiet, as `navigate` does.
    async fn await_document(&self) -> Result<(), BrowserError> {
        let session_id = self.session.active_session_id()?;
        let settle = async {
            if self.ready(&session_id, SETTLE_SECONDS).await? {
                // Unsettled by the deadline is still a readable page, as after `navigate`.
                self.changed_since("", None).await?;
            }
            Ok::<(), BrowserError>(())
        };
        // A dialog the new document opens blocks its renderer; the next read reports the dialog instead.
        match self.until_dialog(settle).await {
            Some(Err(BrowserError::ContextLost(_))) => {
                // The new document can be replaced in turn while this waits; the next read is what counts.
                Ok(())
            }
            Some(Err(error)) => Err(error),
            _ => Ok(()),
        }
    }

    fn observe_once(&self, attempt: u32, bind: bool) -> BoxFuture<'_, Result<Observation, BrowserError>> {
        Box::pin(async move {
            self.session.assert_clear().await?;
            if let Some(dialog) = self.session.pending_dialog() {
                // A JavaScript dialog blocks the renderer, so any page evaluate would hang until it is handled.
                return Ok(self.dialog_observation(dialog, bind));
            }
            // The status read is a command of its own, and it answers the same for as long as the document is: an
            // observation of a document already read keeps that read instead of asking the browser again. The
            // document mark is taken before the read, so a navigation during it leaves the kept copy naming an older
            // mark and the next observation reads the new document.
            let session_id = self.session.active_session_id()?;
            let mark = self.session.document_mark(&session_id);
            let kept = self
                .state
                .lock()
                .unwrap()
                .document
                .clone()
                .filter(|kept| kept.session_id == session_id && kept.mark == mark);
            // A timer can open a dialog while the snapshot evaluates, and the dialog blocks that
            // evaluate until the 120s command deadline.
            let read = async {
                let history = async { Ok::<_, BrowserError>(self.history().await) };
                if kept.is_some() {
                    let (history, snapshot) = tokio::try_join!(history, self.snapshot_all_frames(bind))?;
                    Ok((history, snapshot, None))
                } else {
                    let (history, snapshot, document) =
                        tokio::try_join!(history, self.snapshot_all_frames(bind), self.document_response())?;
                    Ok((history, snapshot, Some(document)))
                }
            };
            let Some(read) = self.until_dialog(read).await else {
                return match self.session.pending_dialog() {
                    Some(dialog) => Ok(self.dialog_observation(dialog, bind)),
                    None => self.observe_once(attempt, bind).await,
                };
            };
            let ((can_go_back, mut history_entry, entry_url), (frames, inaccessible), read_document) = read?;
            let main = frames.iter().find(|(key, _)| key == MAIN).map(|(_, f)| f);
            // History and the snapshot are read at once, so a pushState between them paired the new page with the
            // old entry, whose lineage the new page then overwrote. Both are read again until they name one address;
            // a pair that never agrees names no entry, which lineage judges as a new entry.
            if let (Some(main), Some(entry_url)) = (main, &entry_url)
                && search_address(entry_url) != search_address(&main.raw.url)
            {
                if attempt < HISTORY_REREADS {
                    return self.observe_once(attempt + 1, bind).await;
                }
                history_entry = None;
            }
            let document = match read_document {
                Some(document) => document,
                // A kept read names the document it was made in. One that names another document was replaced
                // without the mark moving, so its status is read now rather than carried over.
                None => match kept
                    .filter(|kept| main.is_some_and(|main| main.raw.document_key == kept.document.document_key))
                {
                    Some(kept) => kept.document,
                    None => self.document_response().await?,
                },
            };
            if main.is_some_and(|main| main.raw.document_key == document.document_key) {
                self.state.lock().unwrap().document =
                    Some(KeptDocument { session_id, mark, document: document.clone() });
            }
            let mut kept: Vec<Control> = vec![];
            let mut control_state: HashMap<String, Target> = HashMap::new();
            for (frame_key, frame) in &frames {
                for c in &frame.raw.controls {
                    let key = c.id.to_string();
                    let guard = frame.raw.guards.get(&key).cloned().flatten();
                    let retarget = frame.raw.retarget_keys.get(&key).cloned().flatten();
                    kept.push(add_control(&mut control_state, frame_key, frame, c, guard, retarget));
                }
            }
            let mut labels: HashMap<String, Vec<String>> = HashMap::new();
            for c in &kept {
                labels.entry(control_state[&c.id].1.clone()).or_default().push(c.label.clone());
            }

            let page_key = combine_page_keys(frames.iter().map(|(_, f)| f.raw.page_key.clone()).collect());
            if bind {
                self.set_last(ObservedState { page_key: page_key.clone(), controls: control_state, labels });
            }

            let title = main.map(|m| m.raw.title.clone()).unwrap_or_default();
            let url = match main {
                Some(main) => main.raw.url.clone(),
                None => self.origin_once().await?,
            };
            // Each frame splits its text in its own viewport's terms, like its controls' `offscreen` flag;
            // same-process frames are already inlined by snapshot.js.
            let mut viewport_text = frame_text(&frames, |s| &s.viewport_text);
            let mut notices = String::new();
            if bind {
                for download in self.session.pending_download_texts() {
                    notices.push_str(&format!(
                        "\n\n[Downloaded file: {} from {}]\n{}",
                        download.name, download.url, download.text
                    ));
                }
                for refused in self.session.pending_download_refusals() {
                    let why = match refused.reason {
                        Refusal::TooLarge { size_bytes, limit_bytes } => format!(
                            "The file is {} and this run keeps downloads up to {}",
                            bytes_of(size_bytes),
                            bytes_of(limit_bytes)
                        ),
                        Refusal::Failed => "The browser could not hand over or store the file".to_owned(),
                    };
                    notices.push_str(&format!(
                        "\n\n[Download not kept: {} from {}] {why}; nothing was saved.",
                        refused.name, refused.url,
                    ));
                }
                for unread in self.session.pending_download_unread() {
                    let why = match unread.reason {
                        PdfUnread::NoReader => "this program has no document reader set up".to_owned(),
                        PdfUnread::TooLarge => "its decompressed contents or complete text exceed max_download_bytes".to_owned(),
                        PdfUnread::TimedOut => format!("reading it took longer than {PARSE_SECONDS} seconds"),
                        PdfUnread::Unparsable => "it could not be parsed as its declared document format".to_owned(),
                        // Not the file's fault, so not said to be: the run should not judge the document by it.
                        PdfUnread::ReaderFailed => {
                            "the document reader did not start or ended without an answer, which says nothing about the file"
                                .to_owned()
                        }
                    };
                    notices.push_str(&format!(
                        "\n\n[Downloaded file: {} from {}] The file was kept, but its text was not read: {why}.",
                        unread.name, unread.url,
                    ));
                }
            }
            // The count alone reached only the reader; said here, the step, the done check and every verdict on the
            // page know some of it went unread.
            match inaccessible {
                0 => {}
                1 => notices
                    .push_str("\n\n[1 frame on this page could not be read: its text and controls are not shown]"),
                n => notices.push_str(&format!(
                    "\n\n[{n} frames on this page could not be read: their text and controls are not shown]"
                )),
            }
            viewport_text.push_str(&notices);
            let response_status = match main {
                Some(main) if main.raw.url == document.url && main.raw.document_key == document.document_key => {
                    // pydantic's bound on the field: a status outside it would make an observation core cannot read.
                    document.response_status.filter(|s| (100..=999).contains(s))
                }
                _ => None,
            };
            Ok(Observation {
                url,
                title,
                page_key,
                captured_at: DateTime::now_utc(),
                response_status,
                document_key: main.map(|m| m.raw.document_key.clone()).unwrap_or_default(),
                controls: kept,
                viewport_text,
                offscreen_text: frame_text(&frames, |s| &s.offscreen_text),
                // Any frame's picture counts: a screenshot spans every frame the observation read.
                visual_content: frames.iter().any(|(_, f)| f.raw.visual_content != 0),
                modal: main.and_then(|m| m.raw.modal.clone()),
                media: frames.iter().flat_map(|(_, f)| f.raw.media.iter().cloned()).collect(),
                tabs: self.session.tabs(),
                dialog: self.session.pending_dialog(),
                inaccessible_frames: int(inaccessible),
                can_go_back,
                history_entry,
            })
        })
    }

    /// Whether BACK is offered, which history entry of which tab the page is, and that entry's address.
    ///
    /// Only these hang on it, and BACK reads history again before it goes. A history read that failed (CDP -32000)
    /// as a run's first page opened ended the run at step 0.
    #[doc(hidden)]
    pub async fn history(&self) -> HistoryRead {
        let Ok(session_id) = self.session.active_session_id() else { return (false, None, None) };
        let Ok(history) = self.session.send("Page.getNavigationHistory", json!({}), Some(&session_id)).await else {
            return (false, None, None);
        };
        let entries = history["entries"].as_array().cloned().unwrap_or_default();
        let index = history["currentIndex"].as_i64().unwrap_or(-1);
        let back = self.back_target(&history).is_some();
        let Some(entry) = usize::try_from(index).ok().and_then(|i| entries.get(i)) else {
            return (back, None, None);
        };
        (back, Some(format!("{session_id}:{}", entry["id"])), entry["url"].as_str().map(str::to_owned))
    }

    /// The earlier same-origin history entry's id, else the start page this tab skipped, else None.
    fn back_target(&self, history: &Value) -> Option<BackTarget> {
        let index = history["currentIndex"].as_i64().unwrap_or(0);
        let entries = history["entries"].as_array().cloned().unwrap_or_default();
        let url_at = |i: i64| usize::try_from(i).ok().and_then(|i| entries.get(i)).and_then(|e| e["url"].as_str());
        if index > 0
            && let (Some(previous), Some(current)) = (url_at(index - 1), url_at(index))
            && same_http_origin(previous, current)
        {
            return Some(BackTarget::Entry(entries[usize::try_from(index - 1).ok()?]["id"].as_i64()?));
        }
        let session_id = self.session.active_session_id().ok()?;
        let skipped = self.state.lock().unwrap().back_to.get(&session_id).cloned()?;
        let current = if entries.is_empty() { "" } else { url_at(index).unwrap_or_default() };
        (skipped != current && same_http_origin(&skipped, current)).then_some(BackTarget::Skipped(skipped))
    }

    /// The active tab's address and title, as the browser reports them while a dialog blocks the page.
    fn active_tab_page(&self) -> (String, String) {
        let tabs = self.session.tabs();
        tabs.into_iter().find(|tab| tab.active).map(|tab| (tab.url, tab.title)).unwrap_or_default()
    }

    fn dialog_observation(&self, dialog: Dialog, bind: bool) -> Observation {
        let (url, title) = self.active_tab_page();
        let page_key = format!("dialog:{}", dialog.kind);
        if bind {
            self.set_last(ObservedState { page_key: page_key.clone(), ..ObservedState::default() });
        }
        Observation {
            url,
            title,
            page_key,
            captured_at: DateTime::now_utc(),
            response_status: None,
            document_key: String::new(),
            controls: vec![],
            viewport_text: String::new(),
            offscreen_text: String::new(),
            visual_content: false,
            modal: None,
            media: Vec::new(),
            tabs: self.session.tabs(),
            dialog: Some(dialog),
            inaccessible_frames: 0,
            can_go_back: false,
            history_entry: None,
        }
    }

    async fn snapshot_all_frames(
        &self,
        bind: bool,
    ) -> Result<(Vec<(String, FrameObservation<Snapshot>)>, usize), BrowserError> {
        let mode = if bind { "snapshot" } else { "glance" };
        // Each frame supplies its own baseline. A producer with a different baseline sends a full result directly.
        let script = format!("(base => {}('{mode}-delta', base))", self.page_js);
        let full = format!("{}('{mode}')", self.page_js);
        let result = self.read_frames::<Snapshot>(&script, Some((mode, full.as_str()))).await?;
        let coverage: Vec<(String, usize)> =
            result.iter().map(|(_, f)| (f.session_id.clone(), f.raw.inaccessible_frames)).collect();
        let inaccessible = self.inaccessible_frames(&coverage)?;
        Ok((result, inaccessible))
    }

    /// Read every frame of the active tab with `expression`.
    ///
    /// `retry`, when a read is one this side keeps, names the slot it is kept under and the expression that reads the
    /// frame again in full, for the frame that answers "unchanged" when no read is kept for it.
    async fn read_frames<T: FrameText>(
        &self,
        expression: &str,
        retry: Option<(&str, &str)>,
    ) -> Result<Vec<(String, FrameObservation<T>)>, BrowserError> {
        let main_session = self.session.active_session_id()?;
        let target_id = self.session.active_target_id();
        let mut sources: Vec<(String, String)> = vec![(MAIN.to_owned(), main_session)];
        // An ad is not the page's content: read as it, an ad's text was given as a page's answer.
        sources.extend(self.session.frame_sessions().into_iter().filter(|(frame, _)| !self.session.ad_frame(frame)));
        let mut result: Vec<(String, FrameObservation<T>)> = vec![];
        // Preserve source order regardless of completion order so capture offsets and hashes stay stable.
        let reads = sources.iter().map(|(key, sid)| self.read_frame::<T>(key, sid, expression, retry));
        let frames = futures_util::future::try_join_all(reads).await;
        let (live_frames, live_sessions) = self.session.frame_read_liveness();
        {
            let mut state = self.state.lock().unwrap();
            state.unanswered.retain(|session_id, _| live_sessions.contains(session_id));
            // A detached frame is never read again, so its last full snapshot cannot expire through read_frame.
            state.kept.retain(|key, _| {
                key.rsplit_once('|').is_some_and(|(_, frame)| frame == MAIN || live_frames.contains(frame))
            });
        }
        let frames = frames?;
        for ((key, _), frame) in sources.into_iter().zip(frames) {
            if let Some(frame) = frame {
                // A dict in Python: a key read twice keeps its first place.
                match result.iter_mut().find(|(k, _)| *k == key) {
                    Some(entry) => entry.1 = frame,
                    None => result.push((key, frame)),
                }
            }
        }
        if let Some((_, main)) = result.iter().find(|(k, _)| k == MAIN) {
            self.session.set_tab_info(&target_id, main.raw.url(), main.raw.title());
        }
        Ok(result)
    }

    async fn read_frame<T: FrameText>(
        &self,
        frame_key: &str,
        session_id: &str,
        expression: &str,
        retry: Option<(&str, &str)>,
    ) -> Result<Option<FrameObservation<T>>, BrowserError> {
        let is_main = frame_key == MAIN;
        let expression = if let Some((slot, _)) = retry {
            let key = format!("{}|{slot}|{frame_key}", T::SHAPE);
            let state = self.state.lock().unwrap();
            let base = state.kept.get(&key).map_or(
                Value::Null,
                |kept| json!({"document_key": kept.raw.get("document_key"), "read": kept.raw.get("read")}),
            );
            format!("{expression}({base})")
        } else {
            expression.to_string()
        };
        let mut raw = match self.read_frame_value(is_main, session_id, &expression).await {
            Ok(raw) => raw,
            Err(_) if !is_main => return Ok(None),
            Err(error) => return Err(error),
        };
        if raw.is_null() {
            return Ok(None);
        }
        // The page answers "unchanged" only about the read it kept, so that answer is the kept read, sent back as the
        // reference it is rather than derived and transferred again. A frame whose kept read this side does not have
        // is read again in full: the page's copy and this side's are kept together, but a reused page must never be
        // assumed to agree with a read it does not name.
        let mut served = None;
        if let Some((slot, full)) = retry
            && truthy(&raw)
            && raw.get("unchanged").is_some_and(truthy)
        {
            let key = format!("{}|{slot}|{frame_key}", T::SHAPE);
            // Another page on the same session can have replaced the page's kept read since this side took its own,
            // so the answer is served only when it names the very read held here, by its document and its number.
            fn names(value: &Value) -> (Option<&str>, Option<u64>) {
                (value.get("document_key").and_then(Value::as_str), value.get("read").and_then(Value::as_u64))
            }
            let kept = self.state.lock().unwrap().kept.get(&key).cloned();
            let parsed = kept
                .filter(|kept| matches!(names(&raw), (Some(_), Some(_))) && names(&kept.raw) == names(&raw))
                .and_then(|kept| T::from_kept(&kept));
            match parsed {
                Some(parsed) => served = Some(parsed),
                // The page has a read this side did not keep: read the frame again rather than answer from nothing.
                _ => {
                    raw = match self.read_frame_value(is_main, session_id, full).await {
                        Ok(raw) => raw,
                        Err(_) if !is_main => return Ok(None),
                        Err(error) => return Err(error),
                    };
                    if raw.is_null() {
                        return Ok(None);
                    }
                }
            }
        }
        if let Some((slot, full)) = retry
            && served.is_none()
            && raw.get("control_delta").is_some()
        {
            let key = format!("{}|{slot}|{frame_key}", T::SHAPE);
            let kept = self.state.lock().unwrap().kept.get(&key).cloned();
            raw = match kept.as_ref().and_then(|kept| delta::apply(&kept.raw, &raw)) {
                Some(reconstructed) => reconstructed,
                None => match self.read_frame_value(is_main, session_id, full).await {
                    Ok(raw) => raw,
                    Err(_) if !is_main => return Ok(None),
                    Err(error) => return Err(error),
                },
            };
            if raw.is_null() {
                return Ok(None);
            }
        }
        let was_served = served.is_some();
        let parsed: Arc<T> = match served {
            Some(parsed) => parsed,
            None => Arc::new(parse(raw.clone(), T::SHAPE)?),
        };
        // Numbered reads are transport baselines. A read whose page has not moved answers unchanged, and a changed
        // one sends a delta against it; both name this exact baseline.
        if let Some((slot, _)) = retry
            && !was_served
        {
            let key = format!("{}|{slot}|{frame_key}", T::SHAPE);
            let mut state = self.state.lock().unwrap();
            match T::keep(raw, &parsed) {
                Some(kept) => {
                    state.kept.insert(key, Arc::new(kept));
                }
                None => {
                    state.kept.remove(&key);
                }
            }
        }
        // The text and the address it came from were read in one evaluation, so this cannot be raced.
        if let Some(grant) = self.session.grant()
            && !grant.allows(parsed.url())
        {
            if is_main {
                return Err(BrowserError::OriginNotAllowed);
            }
            return Ok(None);
        }
        Ok(Some(FrameObservation {
            frame_id: (!is_main).then(|| frame_key.to_owned()),
            session_id: session_id.to_owned(),
            raw: parsed,
        }))
    }

    /// Read one frame, in the main frame or a child's own session.
    async fn read_frame_value(&self, is_main: bool, session_id: &str, expression: &str) -> Result<Value, BrowserError> {
        if is_main {
            self.evaluate(session_id, expression).await
        } else {
            self.frame_evaluate(session_id, expression).await
        }
    }

    /// Evaluate in a child frame within its deadline; past it, the frame reads as unreadable (`Null`).
    async fn frame_evaluate(&self, session_id: &str, script: &str) -> Result<Value, BrowserError> {
        let missed = self.state.lock().unwrap().unanswered.get(session_id).copied();
        let short = missed.is_some_and(|at| at.elapsed() < secs(FRAME_RECHECK_SECONDS));
        let budget = if short { FRAME_RETRY_SECONDS } else { FRAME_READ_SECONDS };
        match timeout(secs(budget), self.evaluate(session_id, script)).await {
            Err(_) => {
                if !short {
                    self.state.lock().unwrap().unanswered.insert(session_id.to_owned(), Instant::now());
                }
                Ok(Value::Null)
            }
            Ok(raw) => {
                let raw = raw?;
                self.state.lock().unwrap().unanswered.remove(session_id);
                Ok(raw)
            }
        }
    }

    fn inaccessible_frames(&self, coverage: &[(String, usize)]) -> Result<usize, BrowserError> {
        // Each inaccessible child is counted by its parent. Only a successful capture of that parent's OOPIF resolves
        // it; an unrelated frame cannot hide a failed document.
        let active = self.session.active_session_id()?;
        let mut missing = usize::from(!coverage.iter().any(|(s, _)| *s == active));
        for (session_id, count) in coverage {
            let captured = coverage
                .iter()
                .filter(|(child, _)| self.session.frame_parent_session(child).as_deref() == Some(session_id))
                .count();
            missing += count.saturating_sub(captured);
        }
        Ok(missing)
    }

    pub async fn capture(&self) -> Result<Capture, BrowserError> {
        self.reread(|| self.capture_once()).await
    }

    async fn capture_once(&self) -> Result<Capture, BrowserError> {
        self.session.assert_clear().await?;
        // A JavaScript dialog blocks the renderer, so the page reads as nothing until it is answered, as it observes.
        let Some(frames) = self.until_dialog(self.read_frames::<CaptureSnapshot>(&self.capture_js, None)).await else {
            let (url, title) = self.active_tab_page();
            return Ok(Capture {
                url,
                title,
                captured_at: DateTime::now_utc(),
                sha256: sha256_hex(""),
                text: String::new(),
                blocks: vec![],
                inaccessible_frames: 0,
            });
        };
        let frames = frames?;
        let mut text = String::new();
        let mut blocks: Vec<Block> = vec![];
        let mut offset = 0usize;
        let mut coverage: Vec<(String, usize)> = vec![];
        let main = frames.iter().find(|(k, _)| k == MAIN).map(|(_, f)| f);
        let title = main.map(|m| m.raw.title.clone()).unwrap_or_default();
        let url = match main {
            Some(main) => main.raw.url.clone(),
            None => self.origin_once().await?,
        };
        for (_, frame) in &frames {
            let frame_key = frame.frame_id.clone().unwrap_or_else(|| MAIN.to_owned());
            match coverage.iter_mut().find(|(s, _)| *s == frame.session_id) {
                Some(entry) => entry.1 = frame.raw.inaccessible_frames,
                None => coverage.push((frame.session_id.clone(), frame.raw.inaccessible_frames)),
            }
            if let Some(found) = frame.raw.json_blocks() {
                for block in found {
                    let start = offset;
                    text.push_str(&block.text);
                    offset += text::len(&block.text);
                    let end = offset;
                    text.push_str("\n\n");
                    offset += 2;
                    blocks.push(Block {
                        source_id: format!("{frame_key}/json:{}", blocks.len()),
                        source_url: None,
                        complete_source: false,
                        kind: block.kind,
                        frame_id: frame.frame_id.clone(),
                        start: int(start),
                        end: int(end),
                        heading_path: block.heading_path,
                        control_context: None,
                        href: None,
                        links: Vec::new(),
                        whole_record: block.whole_record,
                    });
                }
                continue;
            }
            for block in &frame.raw.blocks {
                let start = offset;
                let found: Vec<(String, String)> =
                    block.links.iter().map(|link| (link.text.clone(), link.href.clone())).collect();
                let links = Link::locate(&block.text, int(start), &found);
                text.push_str(&block.text);
                offset += text::len(&block.text);
                let end = offset;
                text.push_str("\n\n");
                offset += 2;
                blocks.push(Block {
                    source_id: format!("{frame_key}/{}:{}", block.source_path, blocks.len()),
                    source_url: None,
                    complete_source: false,
                    kind: block.kind,
                    frame_id: match block.frame_path.as_deref().filter(|p| !p.is_empty()) {
                        Some(path) => Some(format!("{frame_key}/{path}")),
                        None => frame.frame_id.clone(),
                    },
                    start: int(start),
                    end: int(end),
                    heading_path: block.heading_path.clone(),
                    control_context: block.control_context.clone(),
                    href: block.href.clone(),
                    links,
                    whole_record: block.whole_record,
                });
            }
        }
        for (index, download) in self.session.download_texts().into_iter().enumerate() {
            // A CSV file is shown as a page table is: one block per row under its header row, so a reader cites,
            // counts and totals its records one by one, as it does a table's.
            let table = download.mime == "text/csv"
                || download.mime == "application/csv"
                || download.name.to_lowercase().ends_with(".csv");
            let rows = if table { csv_rows(&download.text) } else { Vec::new() };
            let pieces: Vec<(String, String, bool)> = match rows.split_first() {
                Some((header, data)) if !data.is_empty() => data
                    .iter()
                    .enumerate()
                    .map(|(row, line)| (format!("download/{index}:{row}"), format!("{header}\n{line}"), false))
                    .collect(),
                _ => vec![(format!("download/{index}"), download.text.clone(), download.complete)],
            };
            let heading = if download.complete {
                format!("Downloaded file: {}", download.name)
            } else {
                format!("Downloaded file: {}, of which the server sent only part", download.name)
            };
            for (source_id, piece, complete_source) in pieces {
                let start = offset;
                text.push_str(&piece);
                offset += text::len(&piece);
                blocks.push(Block {
                    source_id,
                    source_url: Some(download.url.clone()),
                    complete_source,
                    kind: if table { BlockKind::Table } else { BlockKind::Paragraph },
                    frame_id: None,
                    start: int(start),
                    end: int(offset),
                    heading_path: vec![heading.clone()],
                    control_context: None,
                    href: None,
                    links: Vec::new(),
                    whole_record: false,
                });
                text.push_str("\n\n");
                offset += 2;
            }
        }
        Ok(Capture {
            url,
            title,
            captured_at: DateTime::now_utc(),
            sha256: sha256_hex(&text),
            text,
            blocks,
            inaccessible_frames: int(self.inaccessible_frames(&coverage)?),
        })
    }

    // -- reads of the live page -------------------------------------------------------------------------------

    pub async fn redrawn(
        &self,
        observation: &Observation,
        timeout_seconds: f64,
        target_id: Option<&str>,
    ) -> Result<bool, BrowserError> {
        self.session.check_access().await?;
        let Some(last) = self.last() else { return Ok(false) };
        if last.page_key != observation.page_key || self.session.pending_dialog().is_some() {
            return Ok(false);
        }
        let (session_id, ids): (String, Vec<i64>) = match target_id {
            Some(target_id) => {
                let Some(target) = last.controls.get(target_id) else { return Ok(false) };
                (target.0.clone(), vec![target.2])
            }
            None => {
                let session_id = self.session.active_session_id()?;
                let mut ids: Vec<i64> =
                    last.controls.values().filter(|(sid, ..)| *sid == session_id).map(|(_, _, id, _)| *id).collect();
                ids.sort_unstable();
                (session_id, ids)
            }
        };
        if ids.is_empty() {
            return Ok(false);
        }
        // The comparison `act` makes, `registry.guard` against the guard observed, in the frame that observed it. A
        // changed or vanished control, or a new document, is a redraw.
        let check = format!(
            "(ids => {{ const r = window.__fastbrowse; if (!r?.observed) return true; \
             return ids.some(id => r.observed.has(id) && r.guardKey(r.nodes.get(id), id) !== r.observed.get(id)); }})({})",
            py_json(&json!(ids))
        );
        // One renderer call hears every change: the page's own mutations say when to look again, and the time given
        // only ends the watch.
        let watch = format!(
            "new Promise(resolve => {{ const r = window.__fastbrowse; if (!r?.observed) {{ resolve(true); return; }} \
             const changed = () => {check}; \
             const stop = moved => {{ observer.disconnect(); r.nativeClearTimeout(timer); resolve(moved); }}; \
             const look = () => {{ let moved = true; try {{ moved = changed(); }} catch {{}} \
             if (moved) stop(true); }}; \
             const observer = new MutationObserver(look); \
             observer.observe(document, {{childList: true, subtree: true, attributes: true, characterData: true}}); \
             const timer = r.nativeSetTimeout(() => stop(false), {}); look(); }})",
            py_float(timeout_seconds * 1000.0),
        );
        let Ok(changed) = self.evaluate(&session_id, &watch).await else {
            // A navigation destroys the context mid-check; `act` refuses the old controls on its own.
            return Ok(false);
        };
        if truthy(&changed) {
            let _ = self.changed_since("", None).await;
            return Ok(true);
        }
        Ok(false)
    }

    /// Capture the active tab, bringing it to the front only if a tab behind another produces no frame.
    ///
    /// An idle tab behind another composites nothing new, so a capture can wait many seconds, while activating always
    /// yields a frame at once. Screenshots are rare (recovery and uncertain completion), so a foreground run moves
    /// focus only when it has to. A background run never does: its tab is kept painting by a cast, which is restarted
    /// if a capture stalls, and a tab that still paints nothing has no screenshot.
    pub async fn screenshot(&self) -> Result<Vec<u8>, BrowserError> {
        self.session.assert_clear().await?;
        if self.session.grant().is_some() {
            return Err(BrowserError::ScreenshotsUnavailable(
                "screenshots are unavailable for scoped documents".into(),
            ));
        }
        let client = self.session.client()?;
        let session_id = self.session.active_session_id()?;
        let metrics = client.send("Page.getLayoutMetrics", json!({}), Some(&session_id)).await?;
        let capture = client.send("Page.captureScreenshot", screenshot_params(&metrics), Some(&session_id));
        tokio::pin!(capture);
        let shot = match timeout(secs(SCREENSHOT_WAIT_SECONDS), &mut capture).await {
            Ok(shot) => shot?,
            Err(_) => {
                if self.session.wake().await? {
                    capture.await?
                } else {
                    match timeout(secs(SCREENSHOT_STALL_SECONDS), &mut capture).await {
                        Ok(shot) => shot?,
                        Err(_) => {
                            return Err(BrowserError::ScreenshotsUnavailable(
                                "the tab painted no frame to capture".into(),
                            ));
                        }
                    }
                }
            }
        };
        BASE64
            .decode(shot["data"].as_str().unwrap_or_default().as_bytes())
            .map_err(|_| BrowserError::Failed("Page.captureScreenshot failed (Error)".into()))
    }

    pub async fn origin(&self) -> Result<String, BrowserError> {
        self.reread(|| self.origin_once()).await
    }

    async fn origin_once(&self) -> Result<String, BrowserError> {
        // An observation already reading again calls this, so it reads once rather than nesting a second allowance.
        self.address_once().await?; // refuses a document outside the grant before its origin is read
        let raw = self.evaluate(&self.session.active_session_id()?, "location.origin").await?;
        Ok(value_str(&raw))
    }

    pub async fn address(&self) -> Result<String, BrowserError> {
        self.reread(|| self.address_once()).await
    }

    async fn address_once(&self) -> Result<String, BrowserError> {
        self.session.check_access().await?;
        let raw = self.evaluate(&self.session.active_session_id()?, "location.href").await?;
        let address = value_str(&raw);
        if let Some(grant) = self.session.grant()
            && !address.is_empty()
            && !grant.allows(&address)
        {
            return Err(BrowserError::OriginNotAllowed);
        }
        Ok(address)
    }

    /// Whether the current URL or document differs, without indexing the page's controls again.
    pub async fn document_changed(&self, observation: &Observation) -> Result<bool, BrowserError> {
        self.session.check_access().await?;
        let dialog = self.session.pending_dialog();
        if dialog.is_some() || observation.dialog.is_some() {
            return Ok(dialog != observation.dialog);
        }
        let check = format!(
            "location.href !== {} || String(performance.timeOrigin) !== {}",
            py_json(&json!(observation.url)),
            py_json(&json!(observation.document_key))
        );
        match self.evaluate(&self.session.active_session_id()?, &check).await {
            // The document this asks about was replaced while it asked, which is the answer.
            Err(BrowserError::ContextLost(_)) => Ok(true),
            Err(error) => Err(error),
            Ok(raw) => Ok(raw != Value::Bool(false)),
        }
    }

    async fn document_response(&self) -> Result<DocumentResponse, BrowserError> {
        // Page scripts can replace timing APIs, so status is read in a world they cannot modify.
        let session_id = self.session.active_session_id()?;
        let read = "(() => { const navigation = performance.getEntriesByType('navigation')[0]; \
                    return {url: location.href, document_key: String(performance.timeOrigin), \
                    response_status: navigation?.name.split('#')[0] === location.href.split('#')[0] \
                    ? navigation.responseStatus || null : null}; })()";
        // Making the world took two round trips more than reading it, on every observation, so one is kept until its
        // document goes. It is kept by the unique id Chrome gives each context, never by its context id: Chrome reuses
        // context ids across renderer processes, so a kept context id could name another document's world, even the
        // page's own, while a unique id names this world or nothing.
        let kept = self.state.lock().unwrap().status_worlds.get(&session_id).cloned();
        if let Some(kept) = kept {
            if let Ok(raw) = self.evaluate_in(&session_id, read, None, Some(&kept)).await
                && let Ok(response) = parse::<DocumentResponse>(raw, "_DocumentResponse")
            {
                return Ok(response);
            }
            self.state.lock().unwrap().status_worlds.remove(&session_id);
        }
        let tree = self.session.send("Page.getFrameTree", json!({}), Some(&session_id)).await?;
        let world = self
            .session
            .send(
                "Page.createIsolatedWorld",
                json!({"frameId": tree["frameTree"]["frame"]["id"], "worldName": STATUS_WORLD}),
                Some(&session_id),
            )
            .await?;
        let context = world["executionContextId"].as_i64().unwrap_or_default();
        let raw = self.evaluate_in(&session_id, read, Some(context), None).await?;
        if let Some(unique) = self.session.status_context(&session_id, context) {
            self.state.lock().unwrap().status_worlds.insert(session_id, unique);
        }
        parse(raw, "_DocumentResponse")
    }

    /// The HTTP status the current document was served with, or None when the browser does not say.
    pub async fn response_status(&self) -> Result<Option<i64>, BrowserError> {
        self.session.assert_clear().await?;
        Ok(self.reread(|| self.document_response()).await?.response_status)
    }

    /// Navigate the active tab and wait until its document is usable.
    ///
    /// Waiting for `complete` also waits on every image and tracker, which behind a proxy can outlast the page
    /// becoming interactive, so the wait is for an interactive document whose DOM has gone quiet instead. An
    /// interactive document can still be hydrating: Google Flights rewrote the text around its first control after
    /// the first observation, so the run's first click was refused as stale, and a read of a results page still
    /// "Loading results" had to be taken twice.
    ///
    /// `back_to` is the page BACK opens when this tab has no earlier entry on its site: a run that went straight to a
    /// deep address keeps the start page it skipped one BACK away without spending a load on it.
    pub async fn navigate(&self, url: &str, back_to: Option<&str>) -> Result<(), BrowserError> {
        self.navigate_within(url, LOAD_TIMEOUT_SECONDS, back_to).await
    }

    pub async fn navigate_within(
        &self,
        url: &str,
        load_timeout_seconds: f64,
        back_to: Option<&str>,
    ) -> Result<(), BrowserError> {
        self.session.check_access().await?;
        if let Some(grant) = self.session.grant()
            && !grant.allows(url)
        {
            return Err(BrowserError::OriginNotAllowed);
        }
        let mut session_id = self.session.active_session_id()?;
        if self.session.client()?.wedged(&session_id).await {
            // Page.navigate to a hung renderer waits behind its script, so the page is opened in a fresh tab whose
            // session starts with no deadlines. The hung tab is closed only when the run opened it: an attached
            // user's tab is left alone.
            let hung = self.session.active_target_id();
            let owned = self.session.owns(&hung);
            self.session.open_owned_tab("about:blank").await?;
            if owned {
                self.session.send("Target.closeTarget", json!({"targetId": hung}), None).await?;
            }
            session_id = self.session.active_session_id()?;
        }
        {
            let mut state = self.state.lock().unwrap();
            // A dialog the new document opens is not the last action's.
            state.marked = None;
            match back_to {
                None => state.back_to.remove(&session_id),
                Some(back_to) => state.back_to.insert(session_id.clone(), back_to.to_owned()),
            };
        }
        // A cloud browser's proxy drops a first connection now and then, or leaves a document loading, and a run that
        // never started was scored as a failed task. Chrome's error names (net::ERR_...) carry no page content, so
        // they are shown.
        let mut failure = "NavigationError".to_owned();
        let mut timed_out = false;
        for attempt in 0..NAVIGATE_ATTEMPTS {
            if attempt > 0 {
                sleep(secs(NAVIGATE_RETRY_SECONDS * f64::from(1u32 << (attempt - 1)))).await;
            }
            let downloads_before = self.session.downloads_settled();
            let denials_before = self.session.denials();
            let result = self
                .session
                .send("Page.navigate", json!({"url": url}), Some(&session_id))
                .await
                .map_err(|error| held_in_background(error, self.session.connection().foreground))?;
            if self.session.denials() != denials_before {
                // A redirect left the grant; retrying would only repeat it.
                return Err(BrowserError::OriginNotAllowed);
            }
            if let Some(error) = result["errorText"].as_str().filter(|e| !e.is_empty()) {
                // A link whose target is a file downloads it instead of navigating: Chrome aborts the document
                // navigation with ERR_ABORTED, but the intercept has already taken the bytes into the artifact sink
                // and the tab keeps its page. That is the navigation's whole purpose, not a failure.
                if error == ABORTED && self.session.download_settled(downloads_before, 3.0).await {
                    return Ok(());
                }
                failure = if net_error(error) { error.to_owned() } else { "NavigationError".to_owned() };
                timed_out = false;
                continue;
            }
            if self.ready(&session_id, load_timeout_seconds * f64::from(1u32 << attempt)).await? {
                // Unsettled by the deadline is still a usable page, and a redirect destroys the promise mid-wait.
                let _ = self.changed_since("", None).await;
                return Ok(());
            }
            failure = "TimeoutError".to_owned();
            timed_out = true;
        }
        // A timeout says nothing about the page's content; a caller with no step of its own yet needs the distinct
        // type, not this message, to tell a slow site or session from its own broken navigation. The address is the
        // caller's own, and a run that failed before its first step has no other record of where it was going.
        let message = format!("Page.navigate failed ({failure}) for {url}");
        if timed_out {
            return Err(BrowserError::NavigationTimeout(message));
        }
        if failure.strip_prefix("net::ERR_").is_some_and(site_unreachable) {
            return Err(BrowserError::SiteUnreachable(message));
        }
        Err(BrowserError::Failed(message))
    }

    #[doc(hidden)]
    pub async fn ready(&self, session_id: &str, timeout_seconds: f64) -> Result<bool, BrowserError> {
        let deadline = Instant::now() + secs(timeout_seconds);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            // Page.navigate can answer before the new document has its own context, so this can run in the document
            // being replaced, and Chrome drops the promise with it ("Promise was collected"). Ask the new one.
            let state = match self
                .evaluate(
                    session_id,
                    &format!(
                        "new Promise(resolve => {{ \
                         if (document.readyState !== 'loading') {{ resolve(document.readyState); return; }} \
                         const done = () => {{ if (document.readyState !== 'loading') finish(); }}; \
                         const finish = () => {{ clearTimeout(timer); \
                         document.removeEventListener('readystatechange', done); resolve(document.readyState); }}; \
                         const timer = setTimeout(finish, {}); \
                         document.addEventListener('readystatechange', done); }})",
                        py_float(remaining.as_secs_f64() * 1000.0)
                    ),
                )
                .await
            {
                Err(BrowserError::ContextLost(_)) => {
                    sleep(secs(0.05)).await;
                    continue;
                }
                other => other?,
            };
            if state == "interactive" || state == "complete" {
                return Ok(true);
            }
        }
    }

    // -- settling ---------------------------------------------------------------------------------------------

    #[doc(hidden)]
    pub async fn fingerprint(&self) -> Result<String, BrowserError> {
        let result = self.evaluate(&self.session.active_session_id()?, &self.fingerprint_js).await?;
        Ok(value_str(&result))
    }

    /// Wait for the page to settle after an action, then report whether it changed.
    ///
    /// Settled is what the browser says: no navigation of the page is on its way, no frame in it is still loading its
    /// document, no fetch, XHR or script request started after `requests` (a `requests_mark`) is open, and the tasks
    /// the action queued have run. A click that navigates has requested it by the time the input is answered, and a
    /// request a handler starts is reported before the reply to the read that follows. A quick-add cart redrew 0.5s
    /// after its DOM went quiet, and the run took the add for a click that did nothing and added the item again: its
    /// open request is what holds the settle now. A blocking JavaScript dialog freezes the renderer, and a read that fails
    /// mid-navigation is itself evidence of change. `SETTLE_SECONDS` bounds a page that never does any of this.
    ///
    /// `requests` is `None` after a load, whose own end is `ready`, so a request that never answers holds nothing. An
    /// action passes its mark even when it read no `before`: accepting a confirm is one, and the fetch it starts
    /// holds the settle, or the next observation reads the page before the handler that answers it has run.
    #[doc(hidden)]
    pub async fn changed_since(&self, before: &str, requests: Option<u64>) -> Result<bool, BrowserError> {
        let deadline = Instant::now() + secs(SETTLE_SECONDS);
        let dialog = self.session.wait_for_dialog();
        tokio::pin!(dialog);
        // A deadline reached while the page sat quiet and unchanged, waiting only on a request that never answered,
        // is a page the action left as it was; one reached while the page kept moving is a change.
        let mut unchanged = false;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(!unchanged);
            }
            if self.session.pending_dialog().is_some() {
                return Ok(true);
            }
            // Taken before the read: a request that finishes while the page is read bumps it, and its handler's redraw
            // emits no lifecycle event of its own, so the page is read again rather than judged unchanged.
            let mark = self.lifecycle_mark();
            if self.session.navigating() {
                unchanged = false;
                tokio::select! {
                    () = &mut dialog => return Ok(true),
                    _ = self.wait_for_lifecycle(mark, remaining.as_secs_f64()) => continue,
                }
            }
            // Decorative mutations may never become quiet. Bound this wait like a progress indicator,
            // then compare the fingerprint while the outer deadline still bounds a stuck renderer.
            let settled = self.settled_within(remaining.as_secs_f64().min(SETTLE_LOADING_SECONDS));
            tokio::select! {
                () = &mut dialog => return Ok(true),
                () = sleep(remaining) => return Ok(!unchanged),
                out = settled => match out {
                    Ok((true, current)) => {
                        let differs = current.as_deref() != Some(before);
                        // A load has its own end in `ready`; a request only holds an action. One that never
                        // answers, as on a page's first load, would otherwise hold every run to the deadline.
                        if !requests.is_some_and(|mark| self.session.requests_open_since(mark))
                            && !self.session.navigating()
                            && !self.session.frames_loading()
                            && self.lifecycle_mark() == mark
                        {
                            return Ok(differs);
                        }
                        unchanged = !differs;
                    }
                    Ok((false, _)) => unchanged = false,
                    // Navigation destroys the promise with its execution context, which is itself a change, so a
                    // deadline reached here is not read as a page left as it was. Only this read is retried.
                    Err(_) => unchanged = false,
                },
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            self.wait_for_lifecycle(mark, remaining.as_secs_f64()).await;
        }
    }

    /// The page's current lifecycle generation, to pass to `wait_for_lifecycle`.
    pub fn lifecycle_mark(&self) -> u64 {
        self.session.lifecycle_mark()
    }

    /// Wait for a lifecycle event newer than `mark`, returning whether one arrived before `seconds`.
    pub async fn wait_for_lifecycle(&self, mark: u64, seconds: f64) -> bool {
        self.session.wait_for_lifecycle(mark, seconds).await
    }

    #[doc(hidden)]
    pub async fn settled_fingerprint(&self) -> Result<(bool, Option<String>), BrowserError> {
        self.settled_within(SETTLE_SECONDS).await
    }

    /// One settle: a promise the page resolves once its own event stream says it has gone quiet, sampled for the
    /// fingerprint in the same evaluation.
    ///
    /// Waiting in the page is the whole change (#946). The document's mutation observer and input listeners stamp
    /// `registry.lastMutation`, so a page still writing resolves the promise later, on its own schedule, instead of
    /// this sending another read to find out whether the previous one is stale. A page that settles 30 ms after a
    /// click therefore costs one evaluation rather than one per sample, and the fingerprint walk that follows runs on
    /// a page that has stopped moving, so it is not repeated.
    ///
    /// `seconds` is the caller's remaining settle budget, and the promise answers at it with whatever state the page
    /// is in: an unsettled page is still a readable page. A hidden tab draws no frames, so it answers from one sample,
    /// as the poll it replaces did.
    pub async fn settled_within(&self, seconds: f64) -> Result<(bool, Option<String>), BrowserError> {
        let script = self.settle_script(seconds);
        let result = self.evaluate(&self.session.active_session_id()?, &script).await?;
        match result.as_array().map(Vec::as_slice) {
            Some([Value::Bool(stable), fingerprint]) if fingerprint.is_string() || fingerprint.is_null() => {
                Ok((*stable, fingerprint.as_str().map(str::to_owned)))
            }
            _ => Err(BrowserError::Failed("page script returned an unexpected tuple[bool, str | None]".into())),
        }
    }

    /// The page script for one settle: a promise the page resolves on its own frame and mutation events once the
    /// document is interactive, has not mutated for `SETTLE_QUIET_SECONDS`, shows no progress indicator (or has
    /// shown one for `SETTLE_LOADING_SECONDS`) and has run the timers and frame callbacks the action's input scheduled
    /// that are due before the deadline, then samples the fingerprint in the same call. `seconds` bounds it.
    ///
    /// The `fastbrowse:settle` marker is what the integration test matches to count settle reads.
    fn settle_script(&self, seconds: f64) -> String {
        format!(
            "/* fastbrowse:settle */ new Promise(resolve => {{ const r = window.__fastbrowse; \
             const sample = {page_js}; \
             const finish = () => {{ const s = sample('fingerprint'); \
             resolve([Boolean(s && s.ready), s && s.ready ? s.fingerprint : null]); }}; \
             if (!r || document.hidden) {{ finish(); return; }} \
             const started = performance.now(); const deadline = started + {deadline}; \
             const quiet = {quiet}; const loading = {loading}; \
             let settled = false, frameAt = 0; \
             const done = () => {{ if (settled) return; settled = true; r.nativeClearTimeout(timer); finish(); }}; \
             const busy = () => {{ \
               for (const e of document.querySelectorAll('[aria-busy=\"true\"],[role=\"progressbar\"],progress')) {{ \
                 try {{ if (e.checkVisibility({{checkOpacity: true, checkVisibilityCSS: true}})) return true; }} catch {{}} \
               }} return false; }}; \
             const check = () => {{ \
               if (settled) return; const now = performance.now(); \
               const ready = document.readyState !== 'loading'; const held = busy(); \
               const last = r.lastMutation || 0; \
               const moved = last >= (r.lastInput || 0); \
               const drawing = now - Math.max(last, r.lastInput || 0) < quiet; \
               const pending = Boolean(r.pendingBefore && r.pendingBefore(deadline, drawing)); \
               if (ready && !held && !pending && (!moved || now - last >= quiet) && frameAt >= last) return done(); \
               if (now >= deadline || (held && now - started >= loading)) return done(); \
               r.nativeRequestAnimationFrame(t => {{ frameAt = t; check(); }}); }}; \
             const timer = r.nativeSetTimeout(done, {deadline}); \
             r.nativeRequestAnimationFrame(t => {{ frameAt = t; check(); }}); }})",
            page_js = self.page_js,
            deadline = py_float(seconds.max(0.0) * 1000.0),
            quiet = py_float(SETTLE_QUIET_SECONDS * 1000.0),
            loading = py_float(SETTLE_LOADING_SECONDS * 1000.0),
        )
    }

    // -- evaluation -------------------------------------------------------------------------------------------

    #[doc(hidden)]
    pub async fn evaluate(&self, session_id: &str, expression: &str) -> Result<Value, BrowserError> {
        self.evaluate_in(session_id, expression, None, None).await
    }

    async fn evaluate_in(
        &self,
        session_id: &str,
        expression: &str,
        context_id: Option<i64>,
        unique_context_id: Option<&str>,
    ) -> Result<Value, BrowserError> {
        let mut params = json!({"expression": expression, "returnByValue": true, "awaitPromise": true});
        if let Some(context) = context_id {
            params["contextId"] = context.into();
        }
        if let Some(unique) = unique_context_id {
            params["uniqueContextId"] = unique.into();
        }
        let mut out = self.session.send("Runtime.evaluate", params.clone(), Some(session_id)).await?;
        if out.get("exceptionDetails").is_some_and(|details| details.to_string().contains(SNAPSHOT_UNINSTALLED)) {
            // A document older than the session never ran the script that installs snapshot.js. The call threw as it
            // looked the function up, before doing anything, so it is made again once installed.
            let mut install = params.clone();
            install["expression"] = format!("void {}", self.session.snapshot().install).into();
            self.session.send("Runtime.evaluate", install, Some(session_id)).await?;
            out = self.session.send("Runtime.evaluate", params.clone(), Some(session_id)).await?;
        }
        if out.get("exceptionDetails").is_some_and(|details| details.to_string().contains(LISTENERS_UNREAD)) {
            // A look weighs pointer listeners through `getEventListeners`, which only an evaluate with the command
            // line API has. That API also lends the page `$` and the rest while it runs, so it gets an evaluate of its
            // own, once per document, which keeps the function for every later look (snapshot.js `listeners`).
            let mark = json!({
                "expression": format!("{}('listeners')", self.page_js),
                "includeCommandLineAPI": true,
                "returnByValue": true,
            });
            self.session.send("Runtime.evaluate", mark, Some(session_id)).await?;
            out = self.session.send("Runtime.evaluate", params, Some(session_id)).await?;
        }
        if out.get("exceptionDetails").is_some() {
            return Err(BrowserError::Script);
        }
        Ok(out["result"]["value"].take())
    }

    /// `_until_dialog`: `work`'s result, or `None` once a dialog blocks the page. Input and focus handlers can open a
    /// dialog before CDP answers, blocking every following renderer check. `work` is dropped then, which cancels its
    /// pending calls.
    #[doc(hidden)]
    pub async fn until_dialog<T>(&self, work: impl Future<Output = T>) -> Option<T> {
        tokio::pin!(work);
        tokio::select! {
            out = &mut work => {
                if self.session.pending_dialog().is_some() {
                    return None;
                }
                Some(out)
            }
            () = self.session.wait_for_dialog() => None,
        }
    }
}

/// Lift a browser failure to the variant the decision loop reads; the loop holds the page as `Arc<dyn Page>`.
/// The capture of the viewport `metrics` describe, shrunk only when its device pixels pass Jev's side, which a capture
/// does on a high density screen: Browser Use has started browsers at densities of 1.25 and 2, so 1120 wide can be 2240.
fn screenshot_params(metrics: &Value) -> Value {
    let size = |key: &str| {
        let viewport = &metrics[key];
        Some((viewport["clientWidth"].as_f64()?, viewport["clientHeight"].as_f64()?))
    };
    let side = f64::from(JEV_IMAGE_SIDE);
    match (size("visualViewport"), size("cssVisualViewport")) {
        (Some((pixels_wide, pixels_high)), Some((width, height))) if pixels_wide.max(pixels_high) > side => {
            // One short of the side, because the capture rounds its size up.
            let scale = (side - 1.0) / pixels_wide.max(pixels_high);
            let (x, y) = (&metrics["cssVisualViewport"]["pageX"], &metrics["cssVisualViewport"]["pageY"]);
            json!({"format": "png", "clip": {"x": x, "y": y, "width": width, "height": height, "scale": scale}})
        }
        _ => json!({"format": "png"}),
    }
}

fn seam<T>(result: Result<T, BrowserError>) -> PageResult<T> {
    result.map_err(Into::into)
}

impl Page for CdpPage {
    fn artifacts(&self) -> Vec<Artifact> {
        CdpPage::artifacts(self)
    }

    fn keep_capture<'a>(&'a self, name: &'a str, text: &'a str) -> BoxFuture<'a, PageResult<Option<Artifact>>> {
        Box::pin(async move {
            let sink = &self.session.0.artifact_sink;
            match sink.put(ArtifactKind::Capture, name, "text/plain; charset=utf-8", text.as_bytes()).await {
                Ok(artifact) => Ok(Some(artifact)),
                Err(error) => Err(fastbrowse_core::page::BrowserError::Failed(format!("keeping {name}: {error}"))),
            }
        })
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        Box::pin(async move { seam(CdpPage::observe(self).await) })
    }

    fn glance(&self) -> BoxFuture<'_, PageResult<Observation>> {
        Box::pin(async move { seam(CdpPage::glance(self).await) })
    }

    fn document_changed<'a>(&'a self, observation: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async move { seam(CdpPage::document_changed(self, observation).await) })
    }

    fn navigate<'a>(&'a self, url: &'a str, back_to: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        Box::pin(async move { seam(CdpPage::navigate(self, url, back_to).await) })
    }

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        Box::pin(async move { seam(CdpPage::capture(self).await) })
    }

    fn act<'a>(&'a self, action: &'a Action, observation: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        Box::pin(async move { seam(CdpPage::act(self, action, observation).await) })
    }

    fn wait<'a>(
        &'a self,
        action: &'a Action,
        observation: &'a Observation,
        seconds: f64,
    ) -> BoxFuture<'a, PageResult<ActResult>> {
        Box::pin(async move { seam(CdpPage::wait(self, action, observation, seconds).await) })
    }

    fn redrawn<'a>(
        &'a self,
        observation: &'a Observation,
        timeout_seconds: f64,
        target_id: Option<&'a str>,
    ) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async move { seam(CdpPage::redrawn(self, observation, timeout_seconds, target_id).await) })
    }

    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        Box::pin(async move { seam(CdpPage::screenshot(self).await) })
    }

    fn click_viewport(&self, x: f64, y: f64) -> BoxFuture<'_, PageResult<ActResult>> {
        Box::pin(async move { seam(CdpPage::click_viewport(self, x, y).await) })
    }

    fn withhold_frames(&self, withheld: bool) {
        CdpPage::withhold_frames(self, withheld);
    }

    fn hand_over_tab(&self) {
        self.session.hand_over_active_tab();
    }

    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        Box::pin(async move { seam(CdpPage::address(self).await) })
    }

    fn active_url(&self) -> BoxFuture<'_, PageResult<Option<String>>> {
        Box::pin(
            async move { seam(self.session.active_url().await.map(|url| Some(url).filter(|url| !url.is_empty()))) },
        )
    }

    fn response_status(&self) -> BoxFuture<'_, PageResult<Option<i64>>> {
        Box::pin(async move { seam(CdpPage::response_status(self).await) })
    }

    fn lifecycle_mark(&self) -> u64 {
        CdpPage::lifecycle_mark(self)
    }

    fn wait_for_lifecycle(&self, mark: u64, seconds: f64) -> BoxFuture<'_, bool> {
        Box::pin(CdpPage::wait_for_lifecycle(self, mark, seconds))
    }

    fn captcha_solve(&self) -> Option<CaptchaSolve> {
        self.session.captcha_solve()
    }

    fn loading(&self) -> Option<f64> {
        self.session.loading_left()
    }
}

/// `str(raw) if raw is not None else ""`, for the strings the page reports.
fn value_str(raw: &Value) -> String {
    match raw {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => other.to_string(),
    }
}

/// `_NET_ERROR.fullmatch`: `net::ERR_[A-Z_]+`.
fn net_error(error: &str) -> bool {
    error
        .strip_prefix("net::ERR_")
        .is_some_and(|name| !name.is_empty() && name.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
}

/// A CSV text's records in order, blank lines left out. A newline inside a quoted field belongs to its record.
fn csv_rows(text: &str) -> Vec<&str> {
    let (mut rows, mut start, mut quoted) = (Vec::new(), 0, false);
    for (at, character) in text.char_indices() {
        match character {
            '"' => quoted = !quoted,
            '\n' if !quoted => {
                rows.push(&text[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    rows.push(&text[start..]);
    rows.into_iter().map(|row| row.strip_suffix('\r').unwrap_or(row)).filter(|row| !row.trim().is_empty()).collect()
}

/// A run that is not `--foreground` opens its tab behind the window's current one, and a browser profile can hold such
/// a tab's navigation until the command deadline: a signed-in Chrome held aliexpress.com for 63 s in the background and
/// 0.6 s in front (#771). The failure says so, since nothing else points at the option.
fn held_in_background(error: BrowserError, foreground: bool) -> BrowserError {
    match error {
        BrowserError::PageUnresponsive(message) if !foreground => BrowserError::PageUnresponsive(format!(
            "{message}. The tab was opened behind the window's current tab, where a browser profile can hold a \
             navigation; --foreground opens it in front"
        )),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A signed-in profile held a background tab's Page.navigate to aliexpress.com for 63 s while the same tab in front
    // loaded in 0.6 s (#771), and the run ended unavailable with nothing pointing at --foreground.
    #[test]
    fn a_navigation_held_in_a_background_tab_names_the_foreground_option() {
        let held = BrowserError::PageUnresponsive("Page.navigate exceeded its command deadline".into());
        let told = held_in_background(held.clone(), false).to_string();
        assert!(matches!(held_in_background(held.clone(), false), BrowserError::PageUnresponsive(_)));
        assert!(told.contains("--foreground"), "{told}");
        assert_eq!(held_in_background(held.clone(), true).to_string(), held.to_string());
        let other = BrowserError::ContextLost("c".into());
        assert_eq!(held_in_background(other.clone(), false).to_string(), other.to_string());
    }

    #[test]
    fn same_origin_needs_http_and_matching_ports() {
        assert!(same_http_origin("https://a.test/x", "https://a.test:443/y"));
        assert!(!same_http_origin("about:blank", "https://a.test/"));
        assert!(!same_http_origin("http://a.test/", "https://a.test/"));
        assert!(!same_http_origin("http://a.test:8080/", "http://a.test/"));
        assert!(same_http_origin("http://A.test/", "http://a.TEST/z"));
    }

    #[test]
    fn a_capture_past_jevs_side_is_shrunk_to_fit_and_any_other_is_left_alone() {
        let metrics = |pixels: (u32, u32)| {
            json!({
                "visualViewport": {"clientWidth": pixels.0, "clientHeight": pixels.1},
                "cssVisualViewport": {"pageX": 0, "pageY": 40, "clientWidth": 1120, "clientHeight": 534},
            })
        };
        assert_eq!(screenshot_params(&metrics((1400, 668))), json!({"format": "png"}));
        assert_eq!(screenshot_params(&json!({})), json!({"format": "png"}));
        let shrunk = screenshot_params(&metrics((2240, 1068)));
        let scale = shrunk["clip"]["scale"].as_f64().unwrap();
        assert!(2240.0 * scale <= f64::from(JEV_IMAGE_SIDE));
        assert_eq!(shrunk["clip"]["width"], 1120.0);
        assert_eq!(shrunk["clip"]["y"], 40);
    }

    #[test]
    fn a_certificate_or_tls_failure_is_the_site_giving_nothing() {
        assert!(site_unreachable("CERT_AUTHORITY_INVALID"));
        assert!(site_unreachable("CERT_DATE_INVALID"));
        assert!(site_unreachable("SSL_PROTOCOL_ERROR"));
        assert!(site_unreachable("CONNECTION_REFUSED"));
        // A mistyped address, and every error that says nothing about the site, stay a plain failure.
        assert!(!site_unreachable("NAME_NOT_RESOLVED"));
        assert!(!site_unreachable("ABORTED"));
    }

    fn frame(key: &str, url: &str, viewport: &str, offscreen: &str) -> (String, FrameObservation<Snapshot>) {
        let raw: Snapshot = serde_json::from_value(json!({
            "url": url, "title": "", "viewport_text": viewport, "offscreen_text": offscreen,
            "document_key": "", "controls": [], "page_key": "", "guards": {}, "retarget_keys": {},
        }))
        .expect("snapshot");
        (key.to_owned(), FrameObservation { frame_id: None, session_id: String::new(), raw: Arc::new(raw) })
    }

    #[test]
    fn frame_text_puts_the_page_first_and_labels_each_frame_by_origin() {
        let frames = vec![
            frame("f1", "https://pay.test:8443/card?token=SECRET#x", "Card declined", "Help"),
            frame(MAIN, "https://shop.test/", "Checkout", ""),
            frame("f2", "https://ads.test/", "  \n ", ""),
            frame("f3", "about:srcdoc", "Embedded words", ""),
        ];
        let viewport = frame_text(&frames, |s| &s.viewport_text);
        assert_eq!(
            viewport,
            "Checkout\n\n[Frame: https://pay.test:8443]\nCard declined\n\n[Frame: embedded page]\nEmbedded words"
        );
        assert!(!viewport.contains("token") && !viewport.contains("/card"));
        // A page with no text of its own starts with the first frame's label.
        assert_eq!(frame_text(&frames, |s| &s.offscreen_text), "[Frame: https://pay.test:8443]\nHelp");
    }

    #[test]
    fn net_errors_are_matched_whole() {
        assert!(net_error("net::ERR_EMPTY_RESPONSE"));
        assert!(!net_error("net::ERR_"));
        assert!(!net_error("Cannot navigate to invalid URL"));
    }
}
