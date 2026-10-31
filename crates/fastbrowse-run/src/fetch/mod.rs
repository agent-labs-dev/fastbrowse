//! A page read straight off the network, with no browser behind it.
//!
//! A run that only needs to read a page pays for a browser it never uses: a Chrome that takes seconds to start, a
//! navigation that waits for scripts, and an index of controls nobody clicks. So a browserless run fetches the
//! document, turns its markup into the same `Capture` blocks a browser capture produces, and lets the rest of the run
//! read and cite it unchanged.
//!
//! A link is followed the same way, so a run that only walks from one document to another stays off a browser. What
//! it cannot do is act: a fetched document's form controls are offered as what the page has, and pixels are not
//! rendered, so a step that would fill or press one ends the run as blocked with the reason rather than pretending.
//! Anything a page builds with scripts is likewise invisible here, and a document with no readable text is read as
//! exactly that: [`NotFetched`], with the browser left as the only reader.

pub mod html;

use std::borrow::Cow;
use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use encoding_rs::{Encoding, UTF_8};
use flate2::read::{DeflateDecoder, MultiGzDecoder, ZlibDecoder};

use fastbrowse_browser::session::{CheckAccess, check_browser_access};
use fastbrowse_browser::urlsplit::urlsplit;
use fastbrowse_clients::http::Http;
use fastbrowse_core::BoxFuture;
use fastbrowse_core::models::{Artifact, Operation, StepOutcome};
use fastbrowse_core::origins::origin_of;
use fastbrowse_core::page::{
    ActResult, Action, Block, BlockKind, BrowserError, Capture, Control, Observation, Page, PageResult, Tab,
};
use fastbrowse_core::py::datetime::DateTime;
use fastbrowse_core::py::{sha256_hex, text};

use html::{Found, clean, join, read};

/// A page that never finishes arriving would hold a run for its whole deadline, so the read is bounded.
const TIMEOUT_SECONDS: f64 = 15.0;
/// A body past this is left to the browser rather than held in memory, before or after its content coding is removed.
/// It is the one point where a browserless read hands over by size.
const MAX_BYTES: usize = 5 * 1024 * 1024;
/// A redirect chain longer than this is a loop or a tracker maze, and the browser is the reader for both.
const MAX_REDIRECTS: usize = 10;
const HTML_TYPES: [&str; 2] = ["text/html", "application/xhtml+xml"];
const REDIRECTS: [u16; 5] = [301, 302, 303, 307, 308];

/// The document cannot be read without a browser. The message says why, and never quotes page text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotFetched(pub String);

impl std::fmt::Display for NotFetched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotFetched {}

fn refused(message: impl Into<String>) -> NotFetched {
    NotFetched(message.into())
}

/// The fetched blocks as a `Capture`, with the spans and hash a quote is validated against.
fn capture(url: &str, title: String, found: Vec<Found>) -> Capture {
    let mut text_out = String::new();
    let mut captured = Vec::with_capacity(found.len());
    let mut offset = 0;
    for block in found {
        let start = offset;
        offset += text::len(&block.text);
        text_out.push_str(&block.text);
        text_out.push_str("\n\n");
        captured.push(Block {
            source_id: format!("fetch/:{}", captured.len()),
            source_url: None,
            complete_source: false,
            kind: block.kind,
            frame_id: None,
            start: start as i64,
            end: offset as i64,
            heading_path: block.heading_path,
            control_context: None,
            href: block.href,
            list_id: None,
            list_count: None,
            list_frameless: false,
        });
        offset += 2;
    }
    Capture {
        url: url.to_string(),
        title,
        captured_at: DateTime::now_utc(),
        sha256: sha256_hex(text_out.as_bytes()),
        text: text_out,
        blocks: captured,
        inaccessible_frames: 0,
    }
}

/// Refuse an address that is not a document this may read, naming the reason.
///
/// Credentials in the address are refused rather than dropped: `https://user:pass@host` would otherwise be fetched as
/// an anonymous request and reported as the document that address serves.
pub fn eligible(url: &str, allowed_origins: Option<&[String]>) -> Result<(), NotFetched> {
    let (scheme, netloc, ..) = urlsplit(url);
    if scheme != "http" && scheme != "https" {
        let named = if scheme.is_empty() { "that" } else { &scheme };
        return Err(refused(format!("{named} is not an address a browserless run can fetch")));
    }
    if netloc.contains('@') {
        return Err(refused("an address carrying credentials is not fetched; the run would read it signed out"));
    }
    outside(url, allowed_origins, "")
}

fn outside(url: &str, allowed_origins: Option<&[String]>, lead: &str) -> Result<(), NotFetched> {
    let origin = origin_of(url).unwrap_or_default();
    match allowed_origins {
        Some(allowed) if !allowed.contains(&origin) => {
            Err(refused(format!("{lead}{origin} is outside the origins this run may read")))
        }
        _ => Ok(()),
    }
}

/// The body as text, by the WHATWG Encoding Standard: a byte order mark first, then the charset the response names, then
/// for HTML the one a `<meta>` names, and UTF-8 when nothing names one. A label the standard does not know is ignored,
/// as a browser ignores it, and bytes the encoding cannot map read as U+FFFD.
fn decode(content_type: &str, body: &[u8], html_document: bool) -> String {
    let named = html::charset_parameter(content_type).and_then(|label| Encoding::for_label(label.as_bytes()));
    let encoding = named.or_else(|| html_document.then(|| html::meta_charset(body)).flatten()).unwrap_or(UTF_8);
    encoding.decode(body).0.into_owned()
}

/// The body with its content coding removed, bounded by what it decodes to: a small body can inflate to far more than
/// a text read takes, and that is refused before it fills memory. A coding outside the two asked for is not text.
fn decoded<'a>(coding: Option<&str>, body: &'a [u8]) -> Result<Cow<'a, [u8]>, NotFetched> {
    let coding = coding.map(|coding| coding.trim().to_ascii_lowercase()).unwrap_or_default();
    let inflate = |reader: &mut dyn Read| {
        let mut out = Vec::new();
        reader.take(MAX_BYTES as u64 + 1).read_to_end(&mut out).map(|_| out)
    };
    let out = match coding.as_str() {
        "" | "identity" => return Ok(Cow::Borrowed(body)),
        "gzip" | "x-gzip" => inflate(&mut MultiGzDecoder::new(body)),
        // RFC 9110's deflate is zlib-wrapped, but servers send raw deflate too.
        "deflate" => inflate(&mut ZlibDecoder::new(body)).or_else(|_| inflate(&mut DeflateDecoder::new(body))),
        _ => return Err(refused("the response uses a compression a browserless read does not decode")),
    }
    .map_err(|_| refused("the compressed response is corrupt"))?;
    if out.len() > MAX_BYTES {
        return Err(refused(format!(
            "the document is past {MAX_BYTES} bytes, which a browserless read leaves to the browser"
        )));
    }
    Ok(Cow::Owned(out))
}

/// One document read over HTTP: what it says and the controls its markup offers.
pub struct Fetched {
    pub capture: Capture,
    pub controls: Vec<Control>,
}

/// `url` read over HTTP, or [`NotFetched`] with the reason a browser is needed.
pub async fn fetch_document(
    http: &Http,
    url: &str,
    allowed_origins: Option<&[String]>,
    seconds: f64,
) -> Result<Fetched, NotFetched> {
    eligible(url, allowed_origins)?;
    // The codings this decodes. A server may compress whatever it is asked, as python.org's CDN gzips a
    // request for identity, so asking for these costs nothing a refusal would have saved.
    // A server refuses a client that does not name itself: Wikipedia answers 403 to a request with no User-Agent and
    // 200 to one that names it. The version is the crate's own.
    let headers = [
        ("accept".to_string(), "text/html,application/xhtml+xml;q=0.9,text/plain;q=0.8".to_string()),
        ("accept-encoding".to_string(), "gzip, deflate".to_string()),
        ("user-agent".to_string(), format!("fastbrowse/{}", env!("CARGO_PKG_VERSION"))),
    ];
    // Sent as WHATWG serializes it, the address the capture reports: hyper takes no space or non-ASCII path.
    let mut target = url::Url::parse(url).map_or_else(|_| url.to_string(), String::from);
    let mut hops = 0;
    // Followed here rather than by the client: a redirect to an origin outside the grant has to be refused before it
    // is sent, not after the request arrived. The body is read only up to one byte past the bound, so a long document
    // cannot fill memory before the refusal is reached.
    let response = loop {
        if hops == MAX_REDIRECTS {
            return Err(refused("the page redirects further than this follows"));
        }
        hops += 1;
        let response = http
            .get_bounded(&target, &headers, seconds, Some(MAX_BYTES))
            .await
            .map_err(|error| refused(format!("the fetch failed: {}", error.name())))?;
        let location = response.header("location").map(str::to_string);
        match location {
            Some(location) if REDIRECTS.contains(&response.status) => {
                target = join(&target, &location);
                eligible(&target, allowed_origins)?;
            }
            _ => break response,
        }
    };
    if response.status >= 400 {
        return Err(refused(format!("the fetch got HTTP {}", response.status)));
    }
    if response.body.len() > MAX_BYTES {
        return Err(refused(format!(
            "the document is past {MAX_BYTES} bytes, which a browserless read leaves to the browser"
        )));
    }
    let body = decoded(response.header("content-encoding"), &response.body)?;
    let content_type = response.header("content-type").unwrap_or_default();
    let mime = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    let final_url = url::Url::parse(&target).map_or(target.clone(), String::from);
    outside(&final_url, allowed_origins, "a redirect left for ")?;
    let body = decode(content_type, &body, HTML_TYPES.contains(&mime.as_str()));
    if mime == "text/plain" {
        if text::strip(&body).is_empty() {
            return Err(refused("the document is empty"));
        }
        let found = Found { kind: BlockKind::Paragraph, text: clean(&body), heading_path: Vec::new(), href: None };
        return Ok(Fetched { capture: capture(&final_url, String::new(), vec![found]), controls: Vec::new() });
    }
    if !HTML_TYPES.contains(&mime.as_str()) {
        let named = if mime.is_empty() { "the response" } else { &mime };
        return Err(refused(format!("{named} is not a document read as text")));
    }
    let (title, found, controls) = read(&body, &final_url);
    if found.iter().all(|block| block.text.is_empty()) {
        // A page whose markup is scripts and styles has nothing to quote: the browser is the only reader of what they
        // build, and reading the shell as the page would answer from nothing.
        return Err(refused("the page renders its content with scripts, so the markup holds no readable text"));
    }
    Ok(Fetched { capture: capture(&final_url, title, found), controls })
}

#[derive(Default)]
struct Loaded {
    capture: Option<Capture>,
    controls: Vec<Control>,
    generation: u64,
}

/// A `Page` served by HTTP: it reads documents and cannot touch them.
///
/// Everything above the page seam reads `Capture` blocks and cites their spans, so a fetched document is read and
/// cited exactly as a captured one. The methods that would move a page or inspect pixels return
/// [`BrowserError::NeedsBrowser`], which ends the run as blocked instead of failing a step or inventing an answer. A
/// click on a link is the one action it takes, by opening the link's address.
pub struct FetchedPage {
    http: Http,
    origins: Option<Vec<String>>,
    check_access: Option<CheckAccess>,
    loaded: Mutex<Loaded>,
    needed_browser: AtomicBool,
}

impl FetchedPage {
    pub fn new(http: Http, allowed_origins: Option<Vec<String>>, check_access: Option<CheckAccess>) -> FetchedPage {
        FetchedPage {
            http,
            origins: allowed_origins,
            check_access,
            loaded: Mutex::new(Loaded::default()),
            needed_browser: AtomicBool::new(false),
        }
    }

    /// The caller's gate, asked again: permission can be withdrawn while the run is reading.
    async fn gate(&self) -> PageResult<()> {
        check_browser_access(self.check_access.as_ref()).await.map_err(BrowserError::from)
    }

    /// Whether this page refused something only a browser could do, which is what a fetch-first run escalates on.
    pub fn needed_browser(&self) -> bool {
        self.needed_browser.load(Ordering::Relaxed)
    }

    fn current(&self) -> PageResult<Capture> {
        let capture = self.loaded.lock().unwrap().capture.clone();
        capture.ok_or_else(|| self.needs_browser("no page has been fetched yet".into()))
    }

    /// The address a click on `id` opens: a link to another document. A link to a place in this one, or one a script
    /// answers, is the browser's to follow.
    fn link_target(&self, id: &str) -> Option<String> {
        let loaded = self.loaded.lock().unwrap();
        let link = loaded.controls.iter().find(|control| control.id == id && control.role == "link")?;
        let target = url::Url::parse(link.href.as_deref()?).ok()?;
        let mut here = url::Url::parse(&loaded.capture.as_ref()?.url).ok()?;
        here.set_fragment(target.fragment());
        (matches!(target.scheme(), "http" | "https") && target != here && link.download.is_none())
            .then(|| target.to_string())
    }

    async fn open(&self, url: &str) -> Result<(), NotFetched> {
        let fetched = fetch_document(&self.http, url, self.origins.as_deref(), TIMEOUT_SECONDS).await?;
        let mut loaded = self.loaded.lock().unwrap();
        loaded.capture = Some(fetched.capture);
        loaded.controls = fetched.controls;
        loaded.generation += 1;
        Ok(())
    }
}

impl Page for FetchedPage {
    fn over_http(&self) -> bool {
        true
    }

    fn needs_browser(&self, message: String) -> BrowserError {
        self.needed_browser.store(true, Ordering::Relaxed);
        BrowserError::NeedsBrowser(message)
    }

    /// A fetched run downloads nothing, so it has nothing to report.
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        Box::pin(async move {
            self.gate().await?;
            let capture = self.current()?;
            Ok(Observation {
                url: capture.url.clone(),
                title: capture.title.clone(),
                page_key: capture.sha256.clone(),
                captured_at: capture.captured_at,
                response_status: None,
                document_key: capture.sha256.clone(),
                controls: self.loaded.lock().unwrap().controls.clone(),
                viewport_text: capture.text.clone(),
                offscreen_text: 0,
                visual_content: false,
                // Nothing here can be scrolled, clicked or looked at, so no step may be chosen to do it.
                tabs: vec![Tab {
                    id: "fetched".into(),
                    url: capture.url.clone(),
                    title: capture.title.clone(),
                    active: true,
                    opener_id: None,
                }],
                dialog: None,
                inaccessible_frames: 0,
                can_go_back: false,
                history_entry: Some(capture.url),
            })
        })
    }

    fn document_changed<'a>(&'a self, observation: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async move {
            let loaded = self.loaded.lock().unwrap();
            Ok(loaded.capture.as_ref().is_none_or(|capture| capture.sha256 != observation.page_key))
        })
    }

    fn navigate<'a>(&'a self, url: &'a str, back_to: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        Box::pin(async move {
            self.gate().await?;
            match self.open(url).await {
                Ok(()) => Ok(()),
                // A guessed address, not the caller's page: the loop falls back to `start` on a browser failure, and
                // concluding that the task needs a browser here would end the run.
                Err(NotFetched(message)) if back_to.is_some() => Err(BrowserError::Failed(message)),
                Err(NotFetched(message)) => Err(self.needs_browser(message.into())),
            }
        })
    }

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        Box::pin(async move {
            self.gate().await?;
            self.current()
        })
    }

    fn act<'a>(&'a self, action: &'a Action, _observation: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        Box::pin(async move {
            self.gate().await?;
            let link = (action.operation == Operation::Click)
                .then(|| action.target_id.as_deref().and_then(|id| self.link_target(id)))
                .flatten();
            let Some(url) = link else {
                return Err(self.needs_browser(
                    "this run reads pages over HTTP and can only follow links; run it again without fetch mode to \
                     use a browser"
                        .into(),
                ));
            };
            self.open(&url).await.map_err(|NotFetched(message)| self.needs_browser(message.into()))?;
            Ok(ActResult { outcome: StepOutcome::Executed, page_changed: true, detail: None, moved: true })
        })
    }

    /// Nothing redraws a fetched document, so nothing it offered can have changed.
    fn redrawn<'a>(
        &'a self,
        _observation: &'a Observation,
        _timeout_seconds: f64,
        _target_id: Option<&'a str>,
    ) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        Box::pin(async move {
            self.gate().await?;
            Err(self.needs_browser("a browserless run renders no pixels, so it has no screenshot to take".into()))
        })
    }

    /// There are no frames to hold back.
    fn withhold_frames(&self, _withheld: bool) {}

    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        Box::pin(async move {
            self.gate().await?;
            Ok(self.current()?.url)
        })
    }

    fn response_status(&self) -> BoxFuture<'_, PageResult<Option<i64>>> {
        Box::pin(async { Ok(None) })
    }

    fn lifecycle_mark(&self) -> u64 {
        self.loaded.lock().unwrap().generation
    }

    /// A fetched document begins and ends with its response, so no later transition can arrive.
    fn wait_for_lifecycle(&self, _mark: u64, _seconds: f64) -> BoxFuture<'_, bool> {
        Box::pin(async {
            tokio::task::yield_now().await;
            false
        })
    }
}
