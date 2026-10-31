//! `CdpPage`: the `Page` seam on a `BrowserSession`. `browser/page.py`.
//!
//! Freshness (page_key plus a per-element guard), hit-test-before-input (covered detection), scroll-into-view for
//! offscreen targets and password masking are ported from jev-ultrafast (MIT):
//! jev_ultrafast/browser.py and jev_ultrafast/snapshot.js. `act` dispatches at most once per call.

pub(crate) mod act;
mod pointer;
pub mod scripts;

use std::collections::{HashMap, HashSet};
use std::future::Future;
#[cfg(feature = "cursor")]
use std::sync::Arc;
use std::sync::Mutex;

use data_encoding::{BASE64, HEXLOWER};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::addresses::search_address;
use fastbrowse_core::config::Config;
use fastbrowse_core::decision::JEV_IMAGE_SIDE;
use fastbrowse_core::models::{Artifact, Operation, SourceControl};
use fastbrowse_core::page::{
    ActResult, Action, Block, BlockKind, Capture, Control, Dialog, Observation, Page, PageResult,
};
use fastbrowse_core::py::datetime::DateTime;
use fastbrowse_core::py::text;
use ring::digest::{SHA256, digest};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::time::{Instant, sleep, timeout};

use crate::cdp::secs;
use crate::error::BrowserError;
use crate::session::{BrowserSession, CAPTURE_JS, Refusal, SNAPSHOT_UNINSTALLED, STATUS_WORLD, py_json};
use crate::urlsplit::urlsplit;
use scripts::py_float;

pub(crate) const SETTLE_SECONDS: f64 = 5.0;
pub(crate) const SETTLE_POLL_SECONDS: f64 = 0.1;
pub(crate) const SETTLE_QUIET_SECONDS: f64 = 0.2;
/// How long a settle waits for the page to draw a frame before it starts polling without one.
pub(crate) const SETTLE_FRAME_SECONDS: f64 = 0.5;
/// How many times one reading of the page starts again after the document it was reading was replaced. A
/// self-clearing bot check such as PyPI's "Client Challenge" runs its script and then replaces itself with the page,
/// and an observation in flight across that swap failed with CDP -32000 and ended the run as an error. This bounds
/// only how often one read follows a document that keeps replacing itself, each try waiting for the next one to
/// settle; it is not a limit on the agent, whose steps, time and spend are counted elsewhere and untouched here.
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
/// Two tries a second apart were not enough behind a cloud browser's proxy: in three 0.5.8 evals of about 300 runs,
/// a first page failed both, once on a certificate error, twice timing out connecting and once never becoming ready,
/// each while the site answered the harness and Browser Use's agent reloaded its way through.
pub(crate) const NAVIGATE_ATTEMPTS: u32 = 4;
/// Doubled after each failed try.
pub(crate) const NAVIGATE_RETRY_SECONDS: f64 = 1.0;
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
    href: Option<String>,
    #[serde(default)]
    options: Vec<String>,
    input_type: Option<String>,
    input_name: Option<String>,
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
    next_page: Option<bool>,
    download: Option<bool>,
}

pub(crate) trait FrameText: DeserializeOwned {
    const SHAPE: &'static str;
    fn url(&self) -> &str;
    fn title(&self) -> &str;
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentResponse {
    url: String,
    response_status: Option<i64>,
    document_key: String,
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
    offscreen_text: i64,
    document_key: String,
    /// 1 when the viewport shows a picture, chart, drawing or video that no control carries; snapshot.js sends 0/1.
    #[serde(default)]
    visual_content: i64,
    controls: Vec<SnapshotControl>,
    page_key: String,
    /// Digest of each control's freshness guard, keyed by its local id as a string (a JSON object key).
    guards: HashMap<String, Option<String>>,
    /// Digest of the guard without the node ids at its ends, which a redraw changes.
    retarget_keys: HashMap<String, Option<String>>,
}

impl FrameText for Snapshot {
    fn title(&self) -> &str {
        &self.title
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
    list_id: Option<String>,
    list_count: Option<i64>,
    #[serde(default)]
    list_frameless: bool,
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

/// One frame's page-script output, tagged with how to reach that frame again.
struct FrameObservation<T> {
    frame_id: Option<String>,
    session_id: String,
    raw: T,
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
}

pub struct CdpPage {
    session: BrowserSession,
    config: Config,
    pub(crate) page_js: String,
    capture_js: String,
    pub(crate) fingerprint_js: String,
    pub(crate) state: Mutex<PageState>,
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
        autocomplete: c.autocomplete.clone(),
        submit_semantics: c.submit_semantics.clone(),
        checked: c.checked,
        selected: c.selected,
        expanded: c.expanded,
        sensitive: c.sensitive,
        offscreen: c.offscreen,
        required: c.required,
        invalid: c.invalid,
        next_page: c.next_page,
        download: c.download,
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
            #[cfg(feature = "cursor")]
            cursor: None,
            #[cfg(feature = "cursor")]
            cursor_pid: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(feature = "cursor")]
    pub fn with_cursor(mut self, cursor: Option<Arc<crate::cursor::CursorFeedback>>) -> Self {
        self.cursor = cursor;
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
        self.reread(|| self.observe_once(0)).await
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
                let _ = self.settled_fingerprint(SETTLE_SECONDS).await;
            }
            Ok::<(), BrowserError>(())
        };
        // A dialog the new document opens blocks its renderer; the next read reports the dialog instead.
        match self.until_dialog(settle).await {
            Some(Err(BrowserError::ContextLost(_))) => {
                // The new document can be replaced in turn while this waits; the next read is what counts.
                self.wait_for_lifecycle(self.lifecycle_mark(), SETTLE_POLL_SECONDS).await;
                Ok(())
            }
            Some(Err(error)) => Err(error),
            _ => Ok(()),
        }
    }

    fn observe_once(&self, attempt: u32) -> BoxFuture<'_, Result<Observation, BrowserError>> {
        Box::pin(async move {
            self.session.assert_clear().await?;
            if let Some(dialog) = self.session.pending_dialog() {
                // A JavaScript dialog blocks the renderer, so any page evaluate would hang until it is handled.
                return Ok(self.dialog_observation(dialog));
            }
            // A timer can open a dialog while the snapshot evaluates, and the dialog blocks that
            // evaluate until the 120s command deadline.
            let read = async {
                let history = async { Ok::<_, BrowserError>(self.history().await) };
                tokio::try_join!(history, self.snapshot_all_frames(), self.document_response())
            };
            let Some(read) = self.until_dialog(read).await else {
                return match self.session.pending_dialog() {
                    Some(dialog) => Ok(self.dialog_observation(dialog)),
                    None => self.observe_once(attempt).await,
                };
            };
            let ((can_go_back, mut history_entry, entry_url), (frames, inaccessible), document) = read?;
            let main = frames.iter().find(|(key, _)| key == MAIN).map(|(_, f)| f);
            // History and the snapshot are read at once, so a pushState between them paired the new page with the
            // old entry, whose lineage the new page then overwrote. Both are read again until they name one address;
            // a pair that never agrees names no entry, which lineage judges as a new entry.
            if let (Some(main), Some(entry_url)) = (main, &entry_url)
                && search_address(entry_url) != search_address(&main.raw.url)
            {
                if attempt < HISTORY_REREADS {
                    return self.observe_once(attempt + 1).await;
                }
                history_entry = None;
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
            self.set_last(ObservedState { page_key: page_key.clone(), controls: control_state, labels });

            let title = main.map(|m| m.raw.title.clone()).unwrap_or_default();
            let url = match main {
                Some(main) => main.raw.url.clone(),
                None => self.origin_once().await?,
            };
            let mut viewport_text = main.map(|m| m.raw.viewport_text.clone()).unwrap_or_default();
            let mut notices = String::new();
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
                offscreen_text: main.map(|m| m.raw.offscreen_text).unwrap_or_default(),
                // Any frame's picture counts: a screenshot spans every frame the observation read.
                visual_content: frames.iter().any(|(_, f)| f.raw.visual_content != 0),
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

    fn dialog_observation(&self, dialog: Dialog) -> Observation {
        let tabs = self.session.tabs();
        let active = tabs.iter().find(|t| t.active);
        let page_key = format!("dialog:{}", dialog.kind);
        self.set_last(ObservedState { page_key: page_key.clone(), ..ObservedState::default() });
        Observation {
            url: active.map(|t| t.url.clone()).unwrap_or_default(),
            title: active.map(|t| t.title.clone()).unwrap_or_default(),
            page_key,
            captured_at: DateTime::now_utc(),
            response_status: None,
            document_key: String::new(),
            controls: vec![],
            viewport_text: String::new(),
            offscreen_text: 0,
            visual_content: false,
            tabs: self.session.tabs(),
            dialog: Some(dialog),
            inaccessible_frames: 0,
            can_go_back: false,
            history_entry: None,
        }
    }

    async fn snapshot_all_frames(&self) -> Result<(Vec<(String, FrameObservation<Snapshot>)>, usize), BrowserError> {
        let script = format!("{}('snapshot')", self.page_js);
        let result = self.read_frames::<Snapshot>(&script).await?;
        let coverage: Vec<(String, usize)> =
            result.iter().map(|(_, f)| (f.session_id.clone(), f.raw.inaccessible_frames)).collect();
        let inaccessible = self.inaccessible_frames(&coverage)?;
        Ok((result, inaccessible))
    }

    async fn read_frames<T: FrameText>(
        &self,
        expression: &str,
    ) -> Result<Vec<(String, FrameObservation<T>)>, BrowserError> {
        let main_session = self.session.active_session_id()?;
        let target_id = self.session.active_target_id();
        let mut sources: Vec<(String, String)> = vec![(MAIN.to_owned(), main_session)];
        sources.extend(self.session.frame_sessions());
        let mut result: Vec<(String, FrameObservation<T>)> = vec![];
        // Preserve source order regardless of completion order so capture offsets and hashes stay stable.
        let reads = sources.iter().map(|(key, sid)| self.read_frame::<T>(key, sid, expression));
        let frames = futures_util::future::try_join_all(reads).await?;
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
        let live: HashSet<String> = self.session.frame_sessions().into_iter().map(|(_, s)| s).collect();
        self.state.lock().unwrap().unanswered.retain(|session_id, _| live.contains(session_id));
        Ok(result)
    }

    async fn read_frame<T: FrameText>(
        &self,
        frame_key: &str,
        session_id: &str,
        expression: &str,
    ) -> Result<Option<FrameObservation<T>>, BrowserError> {
        let is_main = frame_key == MAIN;
        let read = if is_main {
            self.evaluate(session_id, expression).await
        } else {
            self.frame_evaluate(session_id, expression).await
        };
        let raw = match read {
            Ok(raw) => raw,
            Err(_) if !is_main => return Ok(None),
            Err(error) => return Err(error),
        };
        if raw.is_null() {
            return Ok(None);
        }
        let parsed: T = parse(raw, T::SHAPE)?;
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
        let mut text = String::new();
        let mut blocks: Vec<Block> = vec![];
        let mut offset = 0usize;
        let mut coverage: Vec<(String, usize)> = vec![];
        let frames = self.read_frames::<CaptureSnapshot>(&self.capture_js).await?;
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
            for block in &frame.raw.blocks {
                let start = offset;
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
                    list_id: block.list_id.as_ref().map(|id| format!("{frame_key}/{}:{id}", block.source_path)),
                    list_count: block.list_count,
                    list_frameless: block.list_frameless,
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
                    list_id: None,
                    list_count: None,
                    list_frameless: false,
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
             return ids.some(id => r.observed.has(id) && r.guardKey(r.nodes.get(id)) !== r.observed.get(id)); }})({})",
            py_json(&json!(ids))
        );
        // One renderer call waits for the guard to change, as `settled_fingerprint` waits for quiet: a poll from here
        // paid a cloud round trip per interval and could let the wait outlast the change it was watching for.
        let watch = format!(
            "new Promise(resolve => {{ const changed = () => {check}; \
             const deadline = performance.now() + {}; \
             const poll = () => {{ \
             let moved = false; try {{ moved = changed(); }} catch {{ resolve(true); return; }} \
             if (moved) {{ resolve(true); return; }} \
             if (performance.now() >= deadline) {{ resolve(false); return; }} \
             setTimeout(poll, {}); }}; poll(); }})",
            py_float(timeout_seconds * 1000.0),
            py_float(SETTLE_POLL_SECONDS * 1000.0)
        );
        let Ok(changed) = self.evaluate(&session_id, &watch).await else {
            // A navigation destroys the context mid-check; `act` refuses the old controls on its own.
            return Ok(false);
        };
        if truthy(&changed) {
            let _ = self.settled_fingerprint(SETTLE_SECONDS).await;
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
        self.navigate_within(url, 15.0, back_to).await
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
        let session_id = self.session.active_session_id()?;
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
            let result = self.session.send("Page.navigate", json!({"url": url}), Some(&session_id)).await?;
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
            if self.ready(&session_id, load_timeout_seconds).await? {
                // Unsettled by the deadline is still a usable page, and a redirect destroys the promise mid-wait.
                let _ = self.settled_fingerprint(SETTLE_SECONDS).await;
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
        if failure.strip_prefix("net::ERR_").is_some_and(|name| UNREACHABLE.contains(&name)) {
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
            let state = self
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
                .await?;
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
    /// A click that navigates returns before the navigation starts, so an immediate fingerprint would describe the old
    /// page. Settled means an interactive document with a quiet DOM. A blocking JavaScript dialog freezes the renderer,
    /// and an evaluate that fails mid-navigation is itself evidence of change. A page that has not changed while a
    /// fetch or XHR request started after `requests` (a `requests_mark`) is open has not answered yet, so the settle
    /// waits for the response, within the same deadline: a quick-add cart redrew 0.5s after its quiet DOM was read,
    /// and the run took the add for a click that did nothing and added the item again.
    #[doc(hidden)]
    pub async fn changed_since(&self, before: &str, input_settling: bool, requests: u64) -> Result<bool, BrowserError> {
        let deadline = Instant::now() + secs(SETTLE_SECONDS);
        let dialog = self.session.wait_for_dialog();
        tokio::pin!(dialog);
        // Fills already check retention after insertion. Other inputs start the top renderer's quiet clock; events in
        // a separate frame cannot reach it, so those still need the delay.
        if !input_settling {
            // Let the renderer's own quiet clock start. The page's lifecycle events say when the document is still
            // moving, so this waits on the page rather than a flat interval that expires regardless.
            self.wait_for_lifecycle(self.lifecycle_mark(), SETTLE_QUIET_SECONDS).await;
        }
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
            let settled = self.settled_fingerprint(remaining.as_secs_f64());
            tokio::select! {
                () = &mut dialog => return Ok(true),
                () = sleep(remaining) => return Ok(!unchanged),
                out = settled => match out {
                    Ok((true, current)) if current.as_deref() != Some(before) => return Ok(true),
                    Ok((true, _)) if !self.session.requests_open_since(requests) && self.lifecycle_mark() == mark => {
                        return Ok(false);
                    }
                    Ok((quiet, _)) => unchanged = quiet,
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
    pub async fn settled_fingerprint(&self, timeout_seconds: f64) -> Result<(bool, Option<String>), BrowserError> {
        // Poll inside the renderer: a cloud round trip can exceed the entire old per-poll timeout. Hidden tabs throttle
        // timers, so return a single sample for the caller to poll in that case. The fingerprint walks the whole
        // document, so it is taken only once the clock says quiet, or as the wait ends; taking it tracks any shadow
        // root or frame added since, which restarts the quiet clock.
        let script = format!(
            "new Promise(resolve => {{ const sample = {page_js}; \
             const deadline = performance.now() + {deadline}; \
             const quietOf = state => state.ready && state.quietFor >= {quiet}; \
             const poll = () => {{ \
             let state = sample('quiet'); \
             if (quietOf(state) || state.hidden || performance.now() >= deadline) state = sample('fingerprint'); \
             const quiet = quietOf(state); \
             if (quiet || state.hidden || performance.now() >= deadline) {{ \
             resolve([quiet, state.ready ? state.fingerprint : null]); return; }} \
             setTimeout(poll, state.ready && !quiet ? Math.min({poll}, \
             Math.max(1, {quiet} - state.quietFor)) \
             : {poll}); }}; \
             let started = false; const start = () => {{ if (!started) {{ started = true; poll(); }} }}; \
             setTimeout(start, {frame}); \
             if (document.hidden) start(); else requestAnimationFrame(() => setTimeout(start, 0)); }})",
            page_js = self.page_js,
            deadline = py_float(timeout_seconds * 1000.0),
            quiet = py_float(SETTLE_QUIET_SECONDS * 1000.0),
            poll = py_float(SETTLE_POLL_SECONDS * 1000.0),
            frame = py_float(SETTLE_FRAME_SECONDS * 1000.0),
        );
        let result = self.evaluate(&self.session.active_session_id()?, &script).await?;
        match result.as_array().map(Vec::as_slice) {
            Some([Value::Bool(stable), fingerprint]) if fingerprint.is_string() || fingerprint.is_null() => {
                Ok((*stable, fingerprint.as_str().map(str::to_owned)))
            }
            _ => Err(BrowserError::Failed("page script returned an unexpected tuple[bool, str | None]".into())),
        }
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

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        Box::pin(async move { seam(CdpPage::observe(self).await) })
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

    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        Box::pin(async move { seam(CdpPage::address(self).await) })
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn net_errors_are_matched_whole() {
        assert!(net_error("net::ERR_EMPTY_RESPONSE"));
        assert!(!net_error("net::ERR_"));
        assert!(!net_error("Cannot navigate to invalid URL"));
    }
}
