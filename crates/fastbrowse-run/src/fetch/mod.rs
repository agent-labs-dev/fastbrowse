//! Port of `src/fastbrowse/fetch.py`: a page read straight off the network, with no browser behind it.
//!
//! A run that only needs to read a page pays for a browser it never uses: a Chrome that takes seconds to start, a
//! navigation that waits for scripts, and an index of controls nobody clicks. So a browserless run fetches the
//! document, turns its markup into the same `Capture` blocks a browser capture produces, and lets the rest of the run
//! read and cite it unchanged.
//!
//! What it cannot do is act. There are no controls in a fetched document, and pixels are not rendered, so a step that
//! would click or fill ends the run as blocked with the reason rather than pretending. Anything a page builds with
//! scripts is likewise invisible here, and a document whose readable text is a sliver of its markup is read as exactly
//! that: [`NotFetched`], with the browser left as the only reader.

pub mod html;

use std::sync::Mutex;

use fastbrowse_browser::session::{CheckAccess, check_browser_access};
use fastbrowse_browser::urlsplit::urlsplit;
use fastbrowse_clients::http::Http;
use fastbrowse_core::BoxFuture;
use fastbrowse_core::models::Artifact;
use fastbrowse_core::origins::origin_of;
use fastbrowse_core::page::{
    ActResult, Action, Block, BlockKind, BrowserError, Capture, Observation, Page, PageResult, Tab, cut_text,
};
use fastbrowse_core::py::datetime::DateTime;
use fastbrowse_core::py::{sha256_hex, text};

use html::{Found, blocks, clean, join};

/// A page that never finishes arriving would hold a run for its whole deadline, so the read is bounded.
const TIMEOUT_SECONDS: f64 = 15.0;
/// Markup beyond this is not a document anyone reads as text; the browser can still be pointed at it.
const MAX_BYTES: usize = 5 * 1024 * 1024;
/// The walls a static fetch meets where a browser would pass: a challenge page, or a shell with no content until
/// scripts run. Either way the fetched text is not the page and the run has to use the browser.
const WALLS: [&str; 7] = [
    "just a moment",
    "checking your browser",
    "enable javascript",
    "verification required",
    "access denied",
    "are you a robot",
    "captcha",
];
/// A document whose text is under this share of its markup is a shell: a rendered page of any substance carries far
/// more words than an empty template, and the browser is the only reader of what scripts build.
const MIN_TEXT_SHARE: f64 = 0.01;
/// A page carrying this much text is not a wall telling the reader to turn scripts on.
const LITTLE_TEXT_CHARS: usize = 480;
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

/// The body decoded by the charset its response names, UTF-8 when it names none, as httpx decodes `response.text`.
/// UTF-8 and windows-1252 (which WHATWG reads `latin1` and `ascii` as) are decoded; another charset is refused, as
/// Python refuses one it has no codec for.
fn decode(content_type: &str, body: &[u8]) -> Result<String, NotFetched> {
    let charset = content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        name.trim().eq_ignore_ascii_case("charset").then(|| value.trim().trim_matches(['"', '\'']).to_ascii_lowercase())
    });
    match charset.as_deref() {
        None | Some("utf-8" | "utf8" | "unicode-1-1-utf-8") => Ok(String::from_utf8_lossy(body).into_owned()),
        Some(
            "windows-1252" | "cp1252" | "x-cp1252" | "latin1" | "latin-1" | "iso-8859-1" | "iso8859-1" | "l1" | "ascii"
            | "us-ascii",
        ) => Ok(body.iter().map(|&byte| windows_1252(byte)).collect()),
        Some(_) => Err(refused("the document's character set is not one this can decode")),
    }
}

fn windows_1252(byte: u8) -> char {
    const HIGH: [char; 32] = [
        '\u{20ac}', '\u{81}', '\u{201a}', '\u{192}', '\u{201e}', '\u{2026}', '\u{2020}', '\u{2021}', '\u{2c6}',
        '\u{2030}', '\u{160}', '\u{2039}', '\u{152}', '\u{8d}', '\u{17d}', '\u{8f}', '\u{90}', '\u{2018}', '\u{2019}',
        '\u{201c}', '\u{201d}', '\u{2022}', '\u{2013}', '\u{2014}', '\u{2dc}', '\u{2122}', '\u{161}', '\u{203a}',
        '\u{153}', '\u{9d}', '\u{17e}', '\u{178}',
    ];
    match byte {
        0x80..=0x9f => HIGH[usize::from(byte - 0x80)],
        _ => char::from(byte),
    }
}

/// `url` as a `Capture`, read over HTTP, or [`NotFetched`] with the reason a browser is needed.
pub async fn fetch_capture(
    http: &Http,
    url: &str,
    allowed_origins: Option<&[String]>,
    seconds: f64,
) -> Result<Capture, NotFetched> {
    eligible(url, allowed_origins)?;
    let headers = [("accept".to_string(), "text/html,application/xhtml+xml;q=0.9,text/plain;q=0.8".to_string())];
    let mut target = url.to_string();
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
        return Err(refused(format!("the document is past {MAX_BYTES} bytes, more than a text read takes")));
    }
    let content_type = response.header("content-type").unwrap_or_default();
    let mime = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    let final_url = url::Url::parse(&target).map_or(target.clone(), String::from);
    outside(&final_url, allowed_origins, "a redirect left for ")?;
    let body = decode(content_type, &response.body)?;
    if mime == "text/plain" {
        if text::strip(&body).is_empty() {
            return Err(refused("the document is empty"));
        }
        let found = Found { kind: BlockKind::Paragraph, text: clean(&body), heading_path: Vec::new(), href: None };
        return Ok(capture(&final_url, String::new(), vec![found]));
    }
    if !HTML_TYPES.contains(&mime.as_str()) {
        let named = if mime.is_empty() { "the response" } else { &mime };
        return Err(refused(format!("{named} is not a document read as text")));
    }
    let (title, found) = blocks(&body, &final_url);
    let readable: usize = found.iter().map(|block| text::len(&block.text)).sum();
    let head = text::prefix(&body, 20_000).to_ascii_lowercase();
    if WALLS.iter().any(|wall| head.contains(wall)) && readable < LITTLE_TEXT_CHARS {
        return Err(refused("the page is a bot check or an empty shell that scripts fill"));
    }
    if readable == 0 || readable < (text::len(&body) as f64 * MIN_TEXT_SHARE) as usize {
        // A page whose markup is scripts and styles has nothing to quote: the browser is the only reader of what they
        // build, and reading the shell as the page would answer from nothing. A genuinely short document passes,
        // because its text is most of its markup however few characters that is.
        return Err(refused("the page renders its content with scripts, so the markup holds no readable text"));
    }
    Ok(capture(&final_url, title, found))
}

fn needs_browser(message: &str) -> BrowserError {
    BrowserError::NeedsBrowser(message.to_string())
}

#[derive(Default)]
struct Loaded {
    capture: Option<Capture>,
    generation: u64,
}

/// A `Page` served by HTTP: it reads documents and cannot touch them.
///
/// Everything above the page seam reads `Capture` blocks and cites their spans, so a fetched document is read and
/// cited exactly as a captured one. The methods that would move a page or inspect pixels return
/// [`BrowserError::NeedsBrowser`], which ends the run as blocked instead of failing a step or inventing an answer.
pub struct FetchedPage {
    http: Http,
    origins: Option<Vec<String>>,
    viewport_chars: usize,
    seconds: f64,
    check_access: Option<CheckAccess>,
    loaded: Mutex<Loaded>,
}

impl FetchedPage {
    pub fn new(
        http: Http,
        allowed_origins: Option<Vec<String>>,
        viewport_text_chars: usize,
        check_access: Option<CheckAccess>,
    ) -> FetchedPage {
        FetchedPage {
            http,
            origins: allowed_origins,
            viewport_chars: viewport_text_chars,
            seconds: TIMEOUT_SECONDS,
            check_access,
            loaded: Mutex::new(Loaded::default()),
        }
    }

    /// The caller's gate, asked again: permission can be withdrawn while the run is reading.
    async fn gate(&self) -> PageResult<()> {
        check_browser_access(self.check_access.as_ref()).await.map_err(BrowserError::from)
    }

    fn current(&self) -> PageResult<Capture> {
        self.loaded.lock().unwrap().capture.clone().ok_or_else(|| needs_browser("no page has been fetched yet"))
    }
}

impl Page for FetchedPage {
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
                controls: Vec::new(),
                omitted_controls: 0,
                viewport_text: cut_text(&capture.text, self.viewport_chars, false),
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
            let capture = match fetch_capture(&self.http, url, self.origins.as_deref(), self.seconds).await {
                Ok(capture) => capture,
                // A guessed address, not the caller's page: the loop falls back to `start` on a browser failure, and
                // concluding that the task needs a browser here would end the run.
                Err(NotFetched(message)) if back_to.is_some() => return Err(BrowserError::Failed(message)),
                Err(NotFetched(message)) => return Err(BrowserError::NeedsBrowser(message)),
            };
            let mut loaded = self.loaded.lock().unwrap();
            loaded.capture = Some(capture);
            loaded.generation += 1;
            Ok(())
        })
    }

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        Box::pin(async move {
            self.gate().await?;
            self.current()
        })
    }

    fn act<'a>(&'a self, _action: &'a Action, _observation: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        Box::pin(async move {
            self.gate().await?;
            Err(needs_browser(
                "this run reads pages over HTTP and has no controls to act on; run it again without fetch mode to use \
                 a browser",
            ))
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
            Err(needs_browser("a browserless run renders no pixels, so it has no screenshot to take"))
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
