//! Run one task end to end. Assemble a browser, the model clients and the agent,
//! then return the result.
//!
//! This is the embedding entry point: the terminal and the live eval both call it, so the assembly has one
//! definition. An embedder gets the parts that are easy to forget: the cloud browser's own cost folded into the
//! result, downloads kept when a directory is given, and owned tabs closed on every path out.

pub mod fetch;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use fastbrowse_agent::agent::{Agent, Resume, RunOptions};
use fastbrowse_agent::verification::RecordSchema;
use fastbrowse_browser::adapters::browser_use_cloud::{BrowserUseCloudBrowser, BrowserUseCloudOptions};
use fastbrowse_browser::adapters::local_chrome::LocalChromeProcess;
use fastbrowse_browser::cursor::{CursorFeedback, cursor_feedback};
use fastbrowse_browser::error::BrowserError;
use fastbrowse_browser::page::CdpPage;
use fastbrowse_browser::recording::{CardCitation, Recording, ResultCard};
use fastbrowse_browser::session::{BrowserSession, CheckAccess, SessionOptions, check_browser_access};
use fastbrowse_clients::environment::{ConfigurationError, Settings};
use fastbrowse_clients::http::Http;
use fastbrowse_core::artifacts::DirectorySink;
use fastbrowse_core::config::Config;
use fastbrowse_core::decision::DecisionClient;
use fastbrowse_core::llm::LLMClient;
use fastbrowse_core::models::{
    ArtifactSink, Attachment, BrowserConnection, BrowserEvent, BudgetResource, BudgetStop, Bytes, CostBreakdown,
    CostLine, Event, EventHandler, FrameHandler, Limits, LocalChrome, RunResult, SecretResolver, Status, UntilCheck,
    WallKind,
};
use fastbrowse_core::origins::parse_origins;
use fastbrowse_core::output_schema::output_model;
use fastbrowse_core::page::Page;
use fastbrowse_core::py::text::repr;
use fastbrowse_core::telemetry::{Ledger, LedgerState};
use serde_json::{Map, Value};
use tokio::sync::Notify;

/// Why a run could not even be assembled: the arguments contradict each other, or the clients cannot be built.
#[derive(Debug, Clone, PartialEq)]
pub enum RunError {
    /// The arguments contradict each other, or one cannot be read.
    Invalid(String),
    /// `output_schema` uses JSON Schema the run cannot fill.
    UnsupportedSchema(String),
    Configuration(String),
    /// `record` could not start, so the run stopped with its browser closed.
    Recording(String),
    /// [`Host::cancel`] was tripped; the browser was closed before this returned.
    Cancelled,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Invalid(message)
            | RunError::UnsupportedSchema(message)
            | RunError::Configuration(message)
            | RunError::Recording(message) => f.write_str(message),
            RunError::Cancelled => f.write_str("the run was cancelled"),
        }
    }
}

impl std::error::Error for RunError {}

impl From<ConfigurationError> for RunError {
    fn from(error: ConfigurationError) -> Self {
        RunError::Configuration(error.to_string())
    }
}

pub use fastbrowse_browser::adapters::local_chrome::which;
pub use fastbrowse_core::protocol::RunTask;

/// What a run is given besides its arguments: the caller's own objects, which no wire carries.
#[derive(Clone, Default)]
pub struct Host {
    /// A Browser Use Cloud key. Without it, a run that names no other browser takes the one the settings hold.
    pub browser_api_key: Option<String>,
    pub check_access: Option<CheckAccess>,
    pub jev: Option<Arc<dyn DecisionClient>>,
    pub llm: Option<Arc<dyn LLMClient>>,
    pub secrets: Option<Arc<dyn SecretResolver>>,
    pub on_event: Option<EventHandler>,
    pub on_frame: Option<FrameHandler>,
    pub until: Option<UntilCheck>,
    /// The caller's browser, opened only if the run reaches a step that needs one: it answers the address to attach
    /// to, and the caller closes it after the run.
    pub open_browser: Option<OpenBrowser>,
    /// Called once the browser run's first page is open, the moment setup ends and the agent's own work begins, so a
    /// caller timing the agent can keep browser setup off its clock.
    pub page_ready: Option<PageReady>,
    pub config: Option<Config>,
    pub settings: Option<Settings>,
    /// The HTTP client the model clients and DevTools discovery use; a caller's outlives the run.
    pub http: Option<Http>,
    /// Tripped when the caller stops the run: it stops where it is, closes what it opened, then returns
    /// [`RunError::Cancelled`].
    pub cancel: Cancel,
}

/// See [`Host::open_browser`].
pub type OpenBrowser = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> + Send + Sync>;

pub use fastbrowse_agent::agent::PageReady;

/// A run's stop signal, shared by whoever may stop it and the run itself.
#[derive(Clone, Default)]
pub struct Cancel(Arc<CancelState>);

#[derive(Default)]
struct CancelState {
    tripped: AtomicBool,
    notify: Notify,
}

impl Cancel {
    pub fn trip(&self) {
        self.0.tripped.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    pub fn is_tripped(&self) -> bool {
        self.0.tripped.load(Ordering::SeqCst)
    }

    /// Resolves once tripped, at once if it already was.
    pub async fn tripped(&self) {
        loop {
            let notified = self.0.notify.notified();
            if self.is_tripped() {
                return;
            }
            notified.await;
        }
    }

    /// `work`, unless the run is stopped first.
    pub async fn guard<T>(&self, work: impl std::future::Future<Output = T>) -> Result<T, RunError> {
        tokio::select! {
            biased;
            () = self.tripped() => Err(RunError::Cancelled),
            done = work => Ok(done),
        }
    }
}

/// A two-letter code, lowercased as Browser Use Cloud expects; a shop serves the visitor's country from it.
pub fn country_code(value: &str) -> Result<String, String> {
    let code = value.trim().to_lowercase();
    if code.chars().count() != 2 || !code.chars().all(char::is_alphabetic) {
        return Err(format!("{} is not a two-letter country code, e.g. uk", repr(value)));
    }
    if code == "gb" {
        // ISO says gb; Browser Use's code for the United Kingdom is uk, and it would reject gb only at browser start.
        return Err("Browser Use's code for the United Kingdom is uk, not gb".into());
    }
    Ok(code)
}

/// What `check` makes of the arguments that need reading.
struct Checked {
    schema: Option<Arc<RecordSchema>>,
    origins: Option<Vec<String>>,
}

/// Whether the arguments hold together, found before anything about the machine or the network is.
///
/// Every entry point calls this before it reads its environment, and `run_task` calls it again, so each rule has one
/// wording wherever the run was asked for.
pub fn check(task: &RunTask) -> Result<(), RunError> {
    checked(task).map(drop)
}

fn checked(task: &RunTask) -> Result<Checked, RunError> {
    let refuse = |message: String| Err(RunError::Invalid(message));
    let given = |pairs: &[(&'static str, bool)]| -> Vec<&'static str> {
        pairs.iter().filter(|(_, on)| *on).map(|(name, _)| *name).collect()
    };
    let chrome = task.chrome.clone().unwrap_or_default();
    let handed = task.cdp_url.is_some() || task.cdp_port.is_some();
    if task.fetch && task.browser {
        return refuse("fetch never starts a browser and browser always does, so pass one of them".into());
    }
    if task.fetch {
        // None of these names anything a browserless run has, and accepting one would report a browser that was never
        // started, a file that was never written or a cursor nobody saw.
        let browser = given(&[
            ("local", task.local),
            ("chrome", task.chrome.is_some()),
            ("cloud_profile", task.cloud_profile.is_some()),
            ("cdp_url", task.cdp_url.is_some()),
            ("cdp_port", task.cdp_port.is_some()),
            ("attach", task.attach),
            ("target_match", task.target_match.is_some()),
            ("foreground", task.foreground),
            ("proxy_country", task.proxy_country.is_some()),
            ("cloud_extensions", !task.cloud_extensions.is_empty()),
            ("record", task.record.is_some()),
            ("cursor", task.cursor == Some(true)),
            ("downloads", task.downloads.is_some()),
        ]);
        if !browser.is_empty() {
            return refuse(format!("fetch starts no browser, so it cannot be combined with {}", browser.join(", ")));
        }
    }
    if task.cdp_url.is_some() && task.cdp_port.is_some() {
        return refuse("cdp_url and cdp_port both name a browser to attach to; pass one".into());
    }
    if handed {
        // Attaching replaces every option that shapes a started browser.
        let shaping = given(&[
            ("local", task.local),
            ("chrome.headed", chrome.headed),
            ("chrome.profile", chrome.profile.is_some()),
            ("cloud_profile", task.cloud_profile.is_some()),
            ("proxy_country", task.proxy_country.is_some()),
            ("cloud_extensions", !task.cloud_extensions.is_empty()),
        ]);
        if !shaping.is_empty() {
            return refuse(format!(
                "cdp_url and cdp_port attach to a browser already running; drop {}",
                shaping.join(", ")
            ));
        }
        if let Some(url) = &task.cdp_url
            && !(url.starts_with("ws://") || url.starts_with("wss://"))
        {
            return refuse(format!("cdp_url expects a ws:// or wss:// URL, got {}", repr(url)));
        }
        if task.cdp_port == Some(0) {
            return refuse("cdp_port expects a port from 1 to 65535, got 0".into());
        }
    } else {
        if task.attach || task.target_match.is_some() {
            return refuse("attach and target_match need cdp_url or cdp_port".into());
        }
        // A cloud profile or a country asked of local Chrome would start signed out, or browse from this machine's
        // address, and report neither.
        if task.local || chrome.headed || chrome.profile.is_some() {
            let cloud = given(&[
                ("cloud_profile", task.cloud_profile.is_some()),
                ("proxy_country", task.proxy_country.is_some()),
                ("cloud_extensions", !task.cloud_extensions.is_empty()),
            ]);
            if !cloud.is_empty() {
                return refuse(format!(
                    "{} needs the cloud browser: drop it, or drop local, chrome.headed and chrome.profile \
                     (and FASTBROWSE_HEADED / FASTBROWSE_PROFILE)",
                    cloud.join(", ")
                ));
            }
        }
    }
    if let Some(country) = &task.proxy_country {
        country_code(country).map_err(|error| RunError::Invalid(format!("proxy_country: {error}")))?;
    }
    let origins = match &task.allowed_origins {
        None => None,
        Some(origins) => Some(parse_origins(origins.clone()).map_err(|error| RunError::Invalid(error.to_string()))?),
    };
    if origins.is_some() && task.record.is_some() {
        return refuse("record shows whatever the tab shows, so it cannot be combined with allowed_origins".into());
    }
    let schema = match &task.output_schema {
        None => None,
        Some(schema) => Some(Arc::new(RecordSchema::from_output(
            output_model(&Value::Object(schema.clone()))
                .map_err(|error| RunError::UnsupportedSchema(format!("output_schema: {error}")))?,
        ))),
    };
    Ok(Checked { schema, origins })
}

/// The run's attachments, as the agent holds them.
fn attachments(task: &RunTask) -> Vec<Attachment> {
    task.attachments
        .iter()
        .map(|attachment| Attachment {
            name: attachment.name.clone(),
            mime_type: attachment.mime_type.clone(),
            content: Bytes(attachment.content.0.clone()),
        })
        .collect()
}

/// A directory removed when the run that made it ends, as `TemporaryDirectory` is.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> std::io::Result<Scratch> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!("fastbrowse-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let path = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&path)?;
        Ok(Scratch(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sink(downloads: Option<&Path>, scratch: &Scratch) -> Result<Arc<dyn ArtifactSink>, BrowserError> {
    let root = downloads.unwrap_or(&scratch.0);
    DirectorySink::new(root)
        .map(|sink| Arc::new(sink) as Arc<dyn ArtifactSink>)
        .map_err(|error| BrowserError::Failed(format!("cannot keep downloads in {}: {error}", root.display())))
}

/// The websocket URL of the browser serving DevTools on `port`, read from its `/json/version` endpoint.
pub async fn resolve_cdp_port(port: u16, host: &str, http: &Http) -> Result<String, BrowserError> {
    let endpoint = format!("http://{host}:{port}/json/version");
    let response = http
        .get(&endpoint, &[], 5.0)
        .await
        .map_err(|_| BrowserError::Failed(format!("no DevTools endpoint at {endpoint} (ConnectError)")))?;
    if response.status == 404 {
        // Chrome's remote debugging switch at chrome://inspect serves the websocket but not this discovery route.
        return Err(BrowserError::Failed(format!(
            "{endpoint} answered 404, so it names no DevTools websocket. A Chrome whose remote debugging was turned on at chrome://inspect serves none here: its DevToolsActivePort file holds the port on the first line and the browser path on the second, so pass ws://127.0.0.1:<port><path> as cdp_url instead."
        )));
    }
    if !(200..300).contains(&response.status) {
        return Err(BrowserError::Failed(format!(
            "{endpoint} answered {}, so it names no DevTools websocket",
            response.status
        )));
    }
    let version: Value = serde_json::from_slice(&response.body)
        .map_err(|_| BrowserError::Failed(format!("no DevTools endpoint at {endpoint} (JSONDecodeError)")))?;
    match version.get("webSocketDebuggerUrl").and_then(Value::as_str) {
        Some(url) if !url.is_empty() => Ok(url.to_string()),
        _ => Err(BrowserError::Failed(format!("{endpoint} named no webSocketDebuggerUrl"))),
    }
}

/// `connect_cdp`'s keyword arguments.
pub struct ConnectCdp {
    pub port: Option<u16>,
    pub cdp_url: Option<String>,
    pub host: String,
    pub target_match: Option<String>,
    pub attach: bool,
    pub foreground: bool,
    pub allowed_origins: Option<Vec<String>>,
    pub check_access: Option<CheckAccess>,
    pub config: Option<Config>,
    pub artifact_sink: Option<Arc<dyn ArtifactSink>>,
    pub downloads: Option<PathBuf>,
}

impl Default for ConnectCdp {
    fn default() -> Self {
        ConnectCdp {
            port: None,
            cdp_url: None,
            host: "127.0.0.1".into(),
            target_match: None,
            attach: true,
            foreground: false,
            allowed_origins: None,
            check_access: None,
            config: None,
            artifact_sink: None,
            downloads: None,
        }
    }
}

/// A page on a browser or Electron app already running, for driving it without the agent.
pub struct Connected {
    pub session: BrowserSession,
    pub page: CdpPage,
    _scratch: Scratch,
}

impl Connected {
    /// Close the session; an attached window is left open.
    pub async fn close(self) -> Result<(), BrowserError> {
        self.session.close().await
    }
}

/// Python's `connect_cdp` context manager: the caller closes what it returns.
pub async fn connect_cdp(options: ConnectCdp) -> Result<Connected, BrowserError> {
    check_browser_access(options.check_access.as_ref()).await?;
    if options.port.is_some() && options.cdp_url.is_some() {
        return Err(BrowserError::Failed("port and cdp_url both name a browser to attach to; pass one".into()));
    }
    let cdp_url = match (options.cdp_url, options.port) {
        (Some(url), _) => url,
        (None, Some(port)) => resolve_cdp_port(port, &options.host, &Http::hyper()).await?,
        (None, None) => return Err(BrowserError::Failed("connect_cdp needs a port or a cdp_url".into())),
    };
    let allowed = match options.allowed_origins {
        None => None,
        Some(origins) => Some(parse_origins(origins).map_err(|error| BrowserError::Failed(error.to_string()))?),
    };
    let connection = BrowserConnection {
        attach: options.attach || options.target_match.is_some(),
        target_match: options.target_match,
        foreground: options.foreground,
        allowed_origins: allowed,
        ..BrowserConnection::new(cdp_url, true)
    };
    let config = options.config.unwrap_or_default();
    let scratch = Scratch::new().map_err(|error| BrowserError::Failed(error.to_string()))?;
    let artifacts = match options.artifact_sink {
        Some(sink) => sink,
        None => sink(options.downloads.as_deref(), &scratch)?,
    };
    let session = BrowserSession::new(
        connection,
        artifacts,
        SessionOptions {
            max_download_bytes: config.max_download_bytes as usize,
            refuse_cookie_banners: config.refuse_cookie_banners,
            check_access: options.check_access,
            ..SessionOptions::default()
        },
    );
    session.open().await?;
    let page = CdpPage::new(session.clone(), config);
    Ok(Connected { session, page, _scratch: scratch })
}

/// Whether DevTools is served from this machine, where a window could be drawn over.
fn is_loopback(cdp_url: &str) -> bool {
    url::Url::parse(cdp_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]"))
}

/// The run's recording, held outside the run so teardown finishes it however the run ends.
type Filming = Arc<tokio::sync::Mutex<Option<Recording>>>;

/// Pass each step to the recording as well, which captions it.
fn captioned(on_event: Option<EventHandler>, recording: &Filming) -> Option<EventHandler> {
    let recording = recording.clone();
    Some(Arc::new(move |event: Event| {
        if let Event::Step(step) = &event
            // Steps arrive while the agent runs; only the result card, after it, holds the recording longer.
            && let Ok(mut filming) = recording.try_lock()
            && let Some(filming) = filming.as_mut()
        {
            filming.caption(&step.step);
        }
        let on_event = on_event.clone();
        Box::pin(async move {
            if let Some(on_event) = on_event {
                on_event(event).await;
            }
        })
    }))
}

/// What the closing card shows of `result`.
fn card(result: &RunResult) -> ResultCard {
    ResultCard {
        status: result.status.as_str().to_string(),
        answer: result.answer.clone(),
        error: result.error.clone(),
        citations: result
            .citations
            .iter()
            .map(|cited| CardCitation {
                id: u32::try_from(cited.id).unwrap_or_default(),
                url: cited.url.clone(),
                quote: cited.quote.clone(),
            })
            .collect(),
        steps: result.steps.len(),
        known_dollars: result.cost.known_dollars(),
    }
}

/// A browser the run drives, and what stopping it needs.
enum Owned {
    /// One the caller handed over: nothing to start and nothing to stop.
    Handed,
    Local(Box<LocalChromeProcess>),
    Cloud(Box<BrowserUseCloudBrowser>),
}

/// Why the browser could not be had: an outage ends the run `unavailable`, anything else `error`.
struct Refused {
    message: String,
    unavailable: bool,
}

impl From<BrowserError> for Refused {
    fn from(error: BrowserError) -> Self {
        Refused { unavailable: error.is_unavailable(), message: error.to_string() }
    }
}

fn refused(message: &str) -> Refused {
    Refused { message: message.into(), unavailable: false }
}

/// Whether a run reads over HTTP before any browser starts. A browser it was told to use, or anything that only has a
/// meaning from the first step in a browser (a recording, an attached window, a signed-in profile, a window to watch),
/// goes straight to it.
fn reads_first(task: &RunTask) -> bool {
    let chrome = task.chrome.clone().unwrap_or_default();
    task.fetch
        || !(task.browser
            || task.record.is_some()
            || task.attach
            || task.target_match.is_some()
            || task.cdp_url.is_some()
            || task.cdp_port.is_some()
            || task.cloud_profile.is_some()
            || task.foreground
            || chrome.headed
            || chrome.profile.is_some())
}

/// Whether a run starts a Browser Use Cloud browser: it is handed none, fetches without one, and was not asked for
/// local Chrome by `local`, a headed window or a kept profile, which only local Chrome has.
pub fn on_cloud(task: &RunTask, chrome: &LocalChrome) -> bool {
    let handed = task.cdp_url.is_some() || task.cdp_port.is_some();
    !(task.fetch || handed || task.local || chrome.headed || chrome.profile.is_some())
}

/// The browser a run drives: one it is handed, a cloud browser, or local Chrome.
async fn browser(
    task: &RunTask,
    host: &Host,
    chrome: &LocalChrome,
    http: &Http,
) -> Result<(BrowserConnection, Owned), Refused> {
    let key = host.browser_api_key.as_deref();
    let lazily = host.open_browser.is_some();
    let foreground = lazily || task.foreground || task.cursor == Some(true);
    let handed = match (&host.open_browser, &task.cdp_url, task.cdp_port) {
        (Some(open), ..) => Some(open().await.map_err(|error| refused(&error))?),
        (None, Some(url), _) => Some(url.clone()),
        (None, None, Some(port)) => Some(resolve_cdp_port(port, "127.0.0.1", http).await?),
        (None, None, None) => None,
    };
    if let Some(cdp_url) = handed {
        if key.is_some() {
            return Err(refused("cdp_url/cdp_port is a browser to attach to; a cloud key would start a second one"));
        }
        // The caller's browser outlives the run. The session either opens its own tab and closes only that, or,
        // attaching, drives a window already open and leaves it open.
        let connection = BrowserConnection {
            attach: lazily || task.attach || task.target_match.is_some(),
            target_match: task.target_match.clone(),
            foreground,
            ..BrowserConnection::new(cdp_url, true)
        };
        return Ok((connection, Owned::Handed));
    }
    let Some(key) = key else {
        let cloud = [
            ("cloud_profile", task.cloud_profile.is_some()),
            ("proxy_country", task.proxy_country.is_some()),
            ("cloud_extensions", !task.cloud_extensions.is_empty()),
        ];
        if let Some((name, _)) = cloud.iter().find(|(_, on)| *on) {
            return Err(refused(&format!("{name} belongs to a Browser Use Cloud browser, which needs a key")));
        }
        let (process, connection) =
            LocalChromeProcess::launch(chrome).await.map_err(|error| refused(&error.to_string()))?;
        let connection = BrowserConnection { foreground: connection.foreground || foreground, ..connection };
        return Ok((connection, Owned::Local(Box::new(process))));
    };
    let options = BrowserUseCloudOptions {
        proxy_country: Some(
            task.proxy_country.as_deref().and_then(|code| country_code(code).ok()).unwrap_or_else(|| "us".into()),
        ),
        timeout_minutes: task.cloud_timeout_minutes.unwrap_or(15),
        profile: task.cloud_profile.clone(),
        viewport: task.viewport,
        extensions: task.cloud_extensions.clone(),
        ..BrowserUseCloudOptions::default()
    };
    let cloud = |error: fastbrowse_browser::adapters::browser_use_cloud::BrowserUseCloudError| Refused {
        unavailable: error.is_unavailable(),
        message: error.to_string(),
    };
    let mut remote = BrowserUseCloudBrowser::new(key, options).map_err(cloud)?;
    let connection = remote.start().await.map_err(cloud)?;
    Ok((connection, Owned::Cloud(Box::new(remote))))
}

/// Stop what the run started, keeping the cloud browser's last reported cost.
async fn stop(owned: Owned, cost: &mut Vec<CostLine>) {
    match owned {
        Owned::Handed => {}
        Owned::Local(mut process) => process.close().await,
        Owned::Cloud(mut remote) => {
            if remote.stop().await.is_err() {
                // The error's text can carry the service's reply; the browser bills until its timeout either way.
                fastbrowse_browser::warn("the cloud browser did not stop and bills until its timeout");
            }
            cost.extend(remote.cost());
        }
    }
}

/// Open `start`, pursue `task`, and return what the run could prove.
pub async fn run_task(task: RunTask, mut host: Host) -> Result<RunResult, RunError> {
    let Checked { schema, origins } = checked(&task)?;
    if task.fetch && host.browser_api_key.is_some() {
        return Err(RunError::Invalid("fetch starts no browser, so it cannot be combined with browser_api_key".into()));
    }
    let settings = match host.settings.take() {
        Some(settings) => settings,
        None => Settings::load()?,
    };
    let http = host.http.take().unwrap_or_else(Http::hyper);
    // The providers this run calls, for an eval to keep beside its result. A caller's own clients are not described.
    if host.jev.is_none() && host.llm.is_none() {
        http.trace.event("route", || Map::from_iter([("providers".into(), Value::from(settings.providers()))]));
    }
    let jev: Arc<dyn DecisionClient> = match host.jev.take() {
        Some(jev) => jev,
        None => Arc::from(settings.decision(&http)?),
    };
    let profile = jev.profile();
    if !profile.validated() {
        // No thresholds were measured on this model, so the run starts from Jev's: an eval reads its result with that.
        http.trace.event("decision_unvalidated", Map::new);
    }
    let config = profile.fit(host.config.clone());
    let llm: Arc<dyn LLMClient> = match host.llm.take() {
        Some(llm) => llm,
        None => Arc::from(settings.llm(&http)?),
    };
    let mut fetch_spent: Vec<CostLine> = Vec::new();
    let mut limits = task.limits.clone();
    let mut resume = None;
    if host.open_browser.is_some() || reads_first(&task) {
        let began = std::time::Instant::now();
        let read =
            fetched(&task, &host, schema.clone(), &config, http.clone(), jev.clone(), llm.clone(), origins.clone())
                .await?;
        if task.fetch || !read.needs_browser {
            return Ok(read.result);
        }
        limits = match remaining(task.limits.as_ref(), &read.spent, began.elapsed().as_secs_f64()) {
            Ok(limits) => limits,
            // The attempt spent what a browser run would have needed, so the run ends on that limit, not the page.
            Err(stop) => return Ok(out_of_budget(read.result, stop)),
        };
        http.trace.event("escalate", || {
            Map::from_iter([("reason".into(), Value::from(read.result.error.clone().unwrap_or_default()))])
        });
        fetch_spent = read.result.cost.lines;
        resume = read.resume;
    }
    let chrome = task.chrome.clone().unwrap_or_else(|| settings.local_chrome());
    // A `chrome` argument names local Chrome; with no browser named at all the run takes the cloud one.
    if host.browser_api_key.is_none()
        && host.open_browser.is_none()
        && task.chrome.is_none()
        && on_cloud(&task, &chrome)
    {
        host.browser_api_key = Some(settings.browser_key()?.to_string());
    }
    let mut browser_cost: Vec<CostLine> = Vec::new();
    let scratch = Scratch::new().map_err(|error| RunError::Invalid(error.to_string()))?;
    let mut session: Option<BrowserSession> = None;
    let mut owned: Option<Owned> = None;
    // Kept outside the run so each is closed however the run ends, before the browser is.
    let recording: Filming = Arc::default();
    let mut recording_failed: Option<String> = None;
    let mut feedback: Option<Arc<CursorFeedback>> = None;
    let work = async {
        check_browser_access(host.check_access.as_ref()).await?;
        let (connection, started) = browser(&task, &host, &chrome, &http).await?;
        owned = Some(started);
        let connection = BrowserConnection { allowed_origins: origins.clone(), ..connection };
        if let Some(on_event) = &host.on_event {
            let event = BrowserEvent {
                kind: Default::default(),
                live_url: connection.live_url.clone(),
                browser_id: connection.browser_id.clone(),
            };
            on_event(Event::Browser(event)).await;
        }
        let attach = connection.attach;
        let opened = BrowserSession::new(
            connection,
            sink(task.downloads.as_deref(), &scratch)?,
            SessionOptions {
                max_download_bytes: config.max_download_bytes as usize,
                refuse_cookie_banners: config.refuse_cookie_banners,
                on_frame: host.on_frame.clone(),
                check_access: host.check_access.clone(),
            },
        );
        session = Some(opened.clone());
        opened.open().await?;
        // A cloud or headless browser has no window on this machine to draw over.
        let shown = task.cursor != Some(false)
            && host.browser_api_key.is_none()
            && is_loopback(&opened.connection().cdp_url)
            && (task.cdp_url.is_some() || task.cdp_port.is_some() || chrome.headed);
        feedback = cursor_feedback(shown).await.map(Arc::new);
        let page: Arc<dyn Page> = Arc::new(CdpPage::new(opened.clone(), config.clone()).with_cursor(feedback.clone()));
        if let Some(path) = &task.record {
            match Recording::start(opened.clone(), path).await {
                Ok(started) => *recording.lock().await = Some(started),
                Err(error) => {
                    recording_failed = Some(error.0.clone());
                    return Err(refused(&error.0));
                }
            }
        }
        let agent = Agent::new(page, jev.clone(), llm.clone())
            .with_config(config.clone())
            .with_secrets(host.secrets.clone())
            .with_trace(http.trace.clone())
            .with_events(captioned(host.on_event.clone(), &recording))
            .with_page_ready(host.page_ready.clone());
        let run = RunOptions {
            start: task.start.clone(),
            // With no page named, the first address is worked out from the task, unless attaching to an existing
            // window where the page is preserved.
            choose_start: task.start.is_none() && !attach,
            context: task.context.clone(),
            inputs: task.inputs.clone().unwrap_or_default(),
            attachments: attachments(&task),
            output_schema: schema.clone(),
            limits: limits.clone(),
            ledger: None,
            authorization: task.authorization.clone(),
            until: host.until.clone(),
            resume: resume.take(),
        };
        let result = agent.run(&task.task, run).await.map_err(|error| refused(&error.to_string()))?;
        // The card is a data: document navigated into the tab, which would replace the caller's own app page; an
        // attached window is theirs and stays where it is.
        // Shown while the recording stays in its slot, so a run cancelled during the card still finishes the videos.
        if !attach && let Some(filming) = recording.lock().await.as_mut() {
            let _ = filming.show_result(&task.task, &card(&result)).await;
        }
        Ok(result)
    };
    let outcome: Option<Result<RunResult, Refused>> = host.cancel.guard(work).await.ok();
    // Read after the run either way: a failed result card still leaves the finished videos.
    let filming = recording.lock().await.take();
    let recordings = match filming {
        Some(filming) => filming.finish().await,
        None => Vec::new(),
    };
    if let Some(feedback) = feedback.take() {
        feedback.close().await;
    }
    if let Some(session) = &session {
        let _ = session.close().await;
    }
    if let Some(owned) = owned.take() {
        stop(owned, &mut browser_cost).await;
    }
    let mut result = match outcome {
        None => return Err(RunError::Cancelled),
        Some(Ok(result)) => result,
        Some(Err(refusal)) => {
            // A cloud browser that cannot be started is an outage, not a failed run.
            RunResult {
                status: if refusal.unavailable { Status::Unavailable } else { Status::Error },
                budget: None,
                answer: None,
                data: Value::Null,
                evidence: Vec::new(),
                steps: Vec::new(),
                cost: CostBreakdown { lines: Vec::new() },
                artifacts: session.as_ref().map(BrowserSession::artifacts).unwrap_or_default(),
                citations: Vec::new(),
                error: Some(refusal.message),
                final_url: None,
                final_frame: None,
                recordings: Vec::new(),
                wall: None,
            }
        }
    };
    if let Some(message) = recording_failed {
        return Err(RunError::Recording(message));
    }
    result.cost.lines.extend(browser_cost);
    fetch_spent.append(&mut result.cost.lines);
    result.cost.lines = fetch_spent;
    result.recordings = recordings;
    Ok(result)
}

/// `limits` less what a fetched attempt spent, for the browser run that follows it, or the limit the attempt used up.
///
/// The counts are the attempt ledger's own, which count a call when it is reserved: a retry adds a cost line but not a
/// call, and a call cancelled when the attempt ended adds a call but no line.
fn remaining(limits: Option<&Limits>, spent: &LedgerState, seconds: f64) -> Result<Option<Limits>, BudgetStop> {
    let Some(limits) = limits else { return Ok(None) };
    let count = |bound: Option<i64>, used: i64, resource: BudgetResource| match bound {
        Some(bound) if bound <= used => Err(BudgetStop { resource, limit: bound as f64 }),
        bound => Ok(bound.map(|bound| bound - used)),
    };
    let amount = |bound: Option<f64>, used: f64, resource: BudgetResource| match bound {
        Some(bound) if bound <= used => Err(BudgetStop { resource, limit: bound }),
        bound => Ok(bound.map(|bound| bound - used)),
    };
    let cost = CostBreakdown { lines: spent.lines.clone() };
    if let Some(max) = limits.max_dollars
        && cost.has_unknown()
    {
        // An unpriced call could have spent anything, so no dollar allowance is left that a browser run could honour.
        return Err(BudgetStop { resource: BudgetResource::Dollars, limit: max });
    }
    Ok(Some(Limits {
        max_steps: count(limits.max_steps, spent.steps, BudgetResource::Steps)?,
        max_jev_calls: count(limits.max_jev_calls, spent.jev_calls, BudgetResource::JevCalls)?,
        max_llm_calls: count(limits.max_llm_calls, spent.llm_calls, BudgetResource::LlmCalls)?,
        max_dollars: amount(limits.max_dollars, cost.known_dollars(), BudgetResource::Dollars)?,
        max_seconds: amount(limits.max_seconds, seconds, BudgetResource::Seconds)?,
    }))
}

/// The fetched attempt's result, ended on the limit it used up rather than on the page's refusal.
fn out_of_budget(mut result: RunResult, stop: BudgetStop) -> RunResult {
    result.status = Status::BudgetExceeded;
    result.error = Some(format!(
        "the fetched read used the {} limit of {} before a browser could take over",
        stop.resource,
        fastbrowse_core::py::json::float_str(stop.limit)
    ));
    result.budget = Some(stop);
    result
}

/// What a fetched run ended with, and whether it ended because the task needs a browser.
struct Read {
    result: RunResult,
    needs_browser: bool,
    /// The attempt ledger's counters and lines when it ended.
    spent: LedgerState,
    /// The attempt itself, for a browser to carry on from.
    resume: Option<Resume>,
}

/// `task` read over HTTP, with no browser started at all.
///
/// A step that would act, download or look at pixels ends the run `blocked`: the document was never the problem, and a
/// browser would answer a different task.
#[allow(clippy::too_many_arguments)]
async fn fetched(
    task: &RunTask,
    host: &Host,
    schema: Option<Arc<RecordSchema>>,
    config: &Config,
    http: Http,
    jev: Arc<dyn DecisionClient>,
    llm: Arc<dyn LLMClient>,
    origins: Option<Vec<String>>,
) -> Result<Read, RunError> {
    if check_browser_access(host.check_access.as_ref()).await.is_err() {
        // The caller's own gate refused the run, which is an error result, not an error to the embedder. Python reports
        // the exception's class name.
        let result = RunResult {
            status: Status::Error,
            budget: None,
            answer: None,
            data: Value::Null,
            evidence: Vec::new(),
            steps: Vec::new(),
            cost: CostBreakdown { lines: Vec::new() },
            artifacts: Vec::new(),
            citations: Vec::new(),
            error: Some("BrowserError".into()),
            final_url: None,
            final_frame: None,
            recordings: Vec::new(),
            wall: None,
        };
        return Ok(Read { result, needs_browser: false, spent: LedgerState::default(), resume: None });
    }
    let ledger = Arc::new(Ledger::new(task.limits.clone().unwrap_or_default()));
    let trace = http.trace.clone();
    let fetched = Arc::new(fetch::FetchedPage::new(http, origins, host.check_access.clone()));
    let page: Arc<dyn Page> = fetched.clone();
    let agent = Agent::new(page, jev, llm)
        .with_config(config.clone())
        .with_secrets(host.secrets.clone())
        .with_trace(trace)
        .with_events(host.on_event.clone());
    let run = RunOptions {
        start: task.start.clone(),
        // With no page named, the first address is worked out from the task, the same way a browser run with no
        // `start` begins.
        choose_start: task.start.is_none(),
        context: task.context.clone(),
        inputs: task.inputs.clone().unwrap_or_default(),
        attachments: attachments(task),
        output_schema: schema,
        limits: None,
        ledger: Some(ledger.clone()),
        authorization: task.authorization.clone(),
        until: host.until.clone(),
        resume: None,
    };
    let outcome = host.cancel.guard(agent.run(&task.task, run)).await?;
    let result = outcome.map_err(|error| RunError::Invalid(error.to_string()))?;
    // Read from the page rather than the message: a refusal the page itself raised means the task needs a browser. It
    // ends the run blocked, or unverified when the refusal was the screenshot an ending frame asked for. A bot check Jev
    // saw in the document is the other: its script clears it only in a browser.
    let bot_check = result.wall.as_ref().is_some_and(|wall| wall.kind == WallKind::BotCheck);
    let needs_browser =
        matches!(result.status, Status::Blocked | Status::Unverified) && (fetched.needed_browser() || bot_check);
    let spent = ledger.state().clone();
    Ok(Read { result, needs_browser, spent, resume: agent.resume() })
}

#[cfg(test)]
mod tests {
    use fastbrowse_core::models::{CostBasis, CostComponent};

    use super::*;

    #[test]
    fn a_fetched_attempt_spends_from_the_browser_runs_limits() {
        let priced = LedgerState {
            lines: vec![
                CostLine::new(CostComponent::Llm, CostBasis::Metered, Some(0.01)),
                CostLine::new(CostComponent::Jev, CostBasis::Metered, Some(0.002)),
            ],
            steps: 2,
            // A call the attempt cancelled as it ended is counted with no line of its own.
            jev_calls: 2,
            llm_calls: 2,
        };
        let unpriced =
            LedgerState { lines: vec![CostLine::new(CostComponent::Jev, CostBasis::Unknown, None)], ..priced.clone() };
        let limits = |change: fn(&mut Limits)| {
            let mut limits = Limits::default();
            change(&mut limits);
            limits
        };
        let stop = |resource, limit| Err(BudgetStop { resource, limit });
        for (given, spent, seconds, left) in [
            (None, &priced, 3.0, Ok(None)),
            (Some(limits(|l| l.max_steps = Some(5))), &priced, 3.0, Ok(Some(limits(|l| l.max_steps = Some(3))))),
            (Some(limits(|l| l.max_steps = Some(2))), &priced, 3.0, stop(BudgetResource::Steps, 2.0)),
            (
                Some(limits(|l| l.max_llm_calls = Some(3))),
                &priced,
                3.0,
                Ok(Some(limits(|l| l.max_llm_calls = Some(1)))),
            ),
            (Some(limits(|l| l.max_jev_calls = Some(2))), &priced, 3.0, stop(BudgetResource::JevCalls, 2.0)),
            (
                Some(limits(|l| l.max_seconds = Some(10.0))),
                &priced,
                3.0,
                Ok(Some(limits(|l| l.max_seconds = Some(7.0)))),
            ),
            (Some(limits(|l| l.max_seconds = Some(3.0))), &priced, 3.0, stop(BudgetResource::Seconds, 3.0)),
            (Some(limits(|l| l.max_dollars = Some(0.012))), &priced, 0.0, stop(BudgetResource::Dollars, 0.012)),
            // An unpriced call leaves no allowance a dollar cap can be enforced against.
            (Some(limits(|l| l.max_dollars = Some(5.0))), &unpriced, 0.0, stop(BudgetResource::Dollars, 5.0)),
            (Some(limits(|l| l.max_steps = Some(5))), &unpriced, 0.0, Ok(Some(limits(|l| l.max_steps = Some(3))))),
        ] {
            assert_eq!(remaining(given.as_ref(), spent, seconds), left, "{given:?}");
        }
        let left = remaining(Some(&limits(|l| l.max_dollars = Some(0.05))), &priced, 0.0).unwrap().unwrap();
        assert!((left.max_dollars.unwrap() - 0.038).abs() < 1e-9);
    }
}
