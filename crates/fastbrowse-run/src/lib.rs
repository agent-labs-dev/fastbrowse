//! Run one task end to end. Assemble a browser, the model clients and the agent,
//! then return the result.
//!
//! This is the embedding entry point: the terminal and the live eval both call it, so the assembly has one
//! definition. An embedder gets the parts that are easy to forget: the cloud browser's own cost folded into the
//! result, downloads kept when a directory is given, and owned tabs closed on every path out.

pub mod desktop;
pub mod fetch;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use fastbrowse_agent::agent::{Agent, RecipeSink, Resume, RunOptions};
use fastbrowse_agent::verification::RecordSchema;
use fastbrowse_browser::adapters::browser_use_cloud::{BrowserUseCloudBrowser, BrowserUseCloudOptions};
use fastbrowse_browser::adapters::local_chrome::LocalChromeProcess;
use fastbrowse_browser::cursor::{CursorFeedback, cursor_feedback};
use fastbrowse_browser::error::BrowserError;
use fastbrowse_browser::extension::SCHEME;
use fastbrowse_browser::extension::protocol::{HostRecord, host_dir, pick_host};
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
    ArtifactSink, Attachment, BrowserConnection, BrowserEvent, BrowserUsed, BrowserVia, BudgetResource, BudgetStop,
    Bytes, CostBasis, CostBreakdown, CostComponent, CostLine, Event, EventHandler, FrameHandler, Limits, LocalChrome,
    RunResult, SecretResolver, Status, StepResult, UntilCheck, WallKind,
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
    /// [`Host::cancel`] was tripped; the browser was closed before this returned. The receipt is what the run did
    /// until then, which is never a result: a cancelled run proved nothing.
    Cancelled(CancellationReceipt),
    /// An assembly failure after work was done, with its original error and accounting receipt.
    AfterSpending {
        error: Box<RunError>,
        receipt: CancellationReceipt,
    },
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Invalid(message)
            | RunError::UnsupportedSchema(message)
            | RunError::Configuration(message)
            | RunError::Recording(message) => f.write_str(message),
            RunError::Cancelled(receipt) => f.write_str(receipt.why()),
            RunError::AfterSpending { error, .. } => error.fmt(f),
        }
    }
}

impl RunError {
    /// What the run did and spent before an error stopped delivery of its result.
    pub fn receipt(&self) -> Option<&CancellationReceipt> {
        match self {
            Self::Cancelled(receipt) | Self::AfterSpending { receipt, .. } => Some(receipt),
            _ => None,
        }
    }

    /// The original refusal, without its accounting envelope.
    pub fn cause(&self) -> &Self {
        match self {
            Self::AfterSpending { error, .. } => error.cause(),
            error => error,
        }
    }

    fn after(self, told: &Told, lines: Vec<CostLine>, recordings: Vec<PathBuf>) -> Self {
        let steps = told.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        if steps.is_empty() && lines.is_empty() && recordings.is_empty() {
            return self;
        }
        Self::AfterSpending {
            error: Box::new(self),
            receipt: CancellationReceipt { steps, cost: CostBreakdown { lines }, recordings, ..Default::default() },
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
pub use fastbrowse_browser::helper;
pub use fastbrowse_core::protocol::{CancellationReceipt, RunTask};

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
    pub recipe_sink: Option<RecipeSink>,
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
    reason: std::sync::Mutex<Option<Box<str>>>,
}

impl Cancel {
    /// Trip it and say why, for the error the run ends with. The first reason given is kept.
    pub fn trip_because(&self, reason: &str) {
        self.0.reason.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_or_insert_with(|| reason.into());
        self.trip();
    }

    /// Why it was tripped, when whoever tripped it said.
    pub fn reason(&self) -> Option<Box<str>> {
        self.0.reason.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

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

    /// `work`, unless the run is stopped first. The receipt is empty: a caller that did or spent something before it
    /// was stopped builds its own.
    pub async fn guard<T>(&self, work: impl std::future::Future<Output = T>) -> Result<T, RunError> {
        tokio::select! {
            biased;
            () = self.tripped() => Err(RunError::Cancelled(CancellationReceipt { reason: self.reason(), ..Default::default() })),
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
    if task.task.trim().is_empty() {
        return refuse("task must not be empty".into());
    }
    let given = |pairs: &[(&'static str, bool)]| -> Vec<&'static str> {
        pairs.iter().filter(|(_, on)| *on).map(|(name, _)| *name).collect()
    };
    let chrome = task.chrome.clone().unwrap_or_default();
    if task.viewport.is_some_and(|(width, height)| width == 0 || height == 0) {
        return refuse("viewport width and height must be greater than zero".into());
    }
    let handed = task.cdp_url.is_some() || task.cdp_port.is_some();
    let extension = task.wants_extension();
    if task.fetch && task.browser {
        return refuse("fetch never starts a browser and browser always does, so pass one of them".into());
    }
    if task.fetch {
        // None of these names anything a browserless run has, and accepting one would report a browser that was never
        // started, a file that was never written or a cursor nobody saw.
        let browser = given(&[
            ("local", task.local),
            ("launch", task.launch),
            ("chrome", task.chrome.is_some()),
            ("cloud_profile", task.cloud_profile.is_some()),
            ("cdp_url", task.cdp_url.is_some()),
            ("cdp_port", task.cdp_port.is_some()),
            ("attach", task.attach),
            ("target_match", task.target_match.is_some()),
            ("foreground", task.foreground),
            ("proxy_country", task.proxy_country.is_some()),
            ("cloud_extensions", !task.cloud_extensions.is_empty()),
            ("viewport", task.viewport.is_some()),
            ("cloud_timeout_minutes", task.cloud_timeout_minutes.is_some()),
            ("record", task.record.is_some()),
            ("recipe", task.recipe.is_some()),
            ("save_recipe", task.save_recipe.is_some()),
            ("cursor", task.cursor == Some(true)),
            ("downloads", task.downloads.is_some()),
            ("extension", extension),
        ]);
        if !browser.is_empty() {
            return refuse(format!("fetch starts no browser, so it cannot be combined with {}", browser.join(", ")));
        }
    }
    if let Some(app) = &task.desktop {
        if shlex::split(app).is_none_or(|words| words.is_empty()) {
            return refuse("desktop must name an app to start, as a command line".into());
        }
        // A desktop run opens no browser and no address. Recipes key their steps by web address, so a desktop run
        // neither records one nor replays one yet.
        let browser = given(&[
            ("start", task.start.is_some()),
            ("fetch", task.fetch),
            ("browser", task.browser),
            ("local", task.local),
            ("launch", task.launch),
            ("chrome", task.chrome.is_some()),
            ("cloud_profile", task.cloud_profile.is_some()),
            ("cdp_url", task.cdp_url.is_some()),
            ("cdp_port", task.cdp_port.is_some()),
            ("attach", task.attach),
            ("target_match", task.target_match.is_some()),
            ("foreground", task.foreground),
            ("proxy_country", task.proxy_country.is_some()),
            ("cloud_extensions", !task.cloud_extensions.is_empty()),
            ("viewport", task.viewport.is_some()),
            ("cloud_timeout_minutes", task.cloud_timeout_minutes.is_some()),
            ("record", task.record.is_some()),
            ("recipe", task.recipe.is_some()),
            ("save_recipe", task.save_recipe.is_some()),
            ("cursor", task.cursor == Some(true)),
            ("downloads", task.downloads.is_some()),
            ("extension", extension),
            ("allowed_origins", task.allowed_origins.is_some()),
        ]);
        if !browser.is_empty() {
            return refuse(format!("desktop drives an app, not a browser, so drop {}", browser.join(", ")));
        }
    }
    if task.cdp_url.is_some() && task.cdp_port.is_some() {
        return refuse("cdp_url and cdp_port both name a browser to attach to; pass one".into());
    }
    // A cloud profile or a country asked of a browser on this machine would run signed out, or browse from this
    // machine's address, and report neither.
    let cloud = given(&[
        ("cloud_profile", task.cloud_profile.is_some()),
        ("proxy_country", task.proxy_country.is_some()),
        ("cloud_extensions", !task.cloud_extensions.is_empty()),
        ("cloud_timeout_minutes", task.cloud_timeout_minutes.is_some()),
    ]);
    if !handed && !cloud.is_empty() && (task.local || task.launch || chrome.headed || chrome.profile.is_some()) {
        return refuse(format!(
            "{} needs the cloud browser: drop it, or drop local, launch, chrome.headed and chrome.profile \
             (and FASTBROWSE_HEADED / FASTBROWSE_PROFILE)",
            cloud.join(", ")
        ));
    }
    if extension {
        // The user's own browser is already running, signed in as they are: nothing here can start or shape it.
        let other = given(&[
            ("launch", task.launch),
            ("chrome.headed", chrome.headed),
            ("chrome.profile", chrome.profile.is_some()),
            ("chrome.binary", task.chrome.as_ref().is_some_and(|chrome| chrome.binary.is_some())),
            ("cdp_url", task.cdp_url.is_some()),
            ("cdp_port", task.cdp_port.is_some()),
            ("cloud_profile", task.cloud_profile.is_some()),
            ("proxy_country", task.proxy_country.is_some()),
            ("cloud_extensions", !task.cloud_extensions.is_empty()),
            ("cloud_timeout_minutes", task.cloud_timeout_minutes.is_some()),
            ("viewport", task.viewport.is_some()),
        ]);
        if !other.is_empty() {
            if task.local_means_extension() {
                return refuse(format!(
                    "local drives your own Chrome through the fastbrowse extension; pass launch to start a separate \
                     Chrome, or drop {}",
                    other.join(", ")
                ));
            }
            return refuse(format!("extension drives the browser it is connected in; drop {}", other.join(", ")));
        }
    } else if handed {
        // Attaching replaces every option that shapes a started browser.
        let shaping = given(&[
            ("local", task.local),
            ("launch", task.launch),
            ("chrome.headed", chrome.headed),
            ("chrome.profile", chrome.profile.is_some()),
            ("cloud_profile", task.cloud_profile.is_some()),
            ("proxy_country", task.proxy_country.is_some()),
            ("cloud_extensions", !task.cloud_extensions.is_empty()),
            ("cloud_timeout_minutes", task.cloud_timeout_minutes.is_some()),
            ("viewport", task.viewport.is_some()),
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
            return refuse("attach and target_match need cdp_url, cdp_port or extension".into());
        }
    }
    if let Some(country) = &task.proxy_country {
        country_code(country).map_err(|error| RunError::Invalid(format!("proxy_country: {error}")))?;
    }
    let origins = match &task.allowed_origins {
        None => None,
        Some(origins) => Some(parse_origins(origins.clone()).map_err(|error| RunError::Invalid(error.to_string()))?),
    };
    if origins.is_some() && extension {
        // A popup's first request can leave before the extension attaches to it, so the grant could not refuse it
        // before it was sent, as a scoped run promises.
        return refuse(
            "allowed_origins cannot be combined with extension: a popup's first request can leave before the \
             extension reaches it, so it could not be refused first"
                .into(),
        );
    }
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

/// The steps a run has told, held outside the run so a cancelled one can still report them.
type Told = Arc<std::sync::Mutex<Vec<StepResult>>>;

/// Keep each step as it is told, before the caller's handler is awaited: a run stopped while that handler runs, or
/// by it, still reports the step it was telling. The agent redacted the step before telling it.
fn noting(on_event: Option<EventHandler>, told: &Told) -> EventHandler {
    let told = told.clone();
    Arc::new(move |event: Event| {
        if let Event::Step(step) = &event {
            told.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(step.step.clone());
        }
        let on_event = on_event.clone();
        Box::pin(async move {
            if let Some(on_event) = on_event {
                on_event(event).await;
            }
        })
    })
}

/// What a run stopped by [`Host::cancel`] did: the steps it told and every cost line its phases left. Read once the
/// run's work is dropped, so a request it had in flight has recorded what it is owed.
fn cancelled(
    told: &Told,
    lines: Vec<CostLine>,
    recordings: Vec<PathBuf>,
    browser: Option<BrowserUsed>,
    reason: Option<Box<str>>,
) -> RunError {
    let steps = told.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    RunError::Cancelled(CancellationReceipt {
        steps,
        cost: CostBreakdown { lines },
        recordings,
        browser: browser.map(Box::new),
        reason,
    })
}

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
    /// The user's own browser, reached through the fastbrowse extension: its tabs close when the run disconnects.
    Extension(Box<HostRecord>),
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
fn reads_first(task: &RunTask, chrome: &LocalChrome) -> bool {
    task.fetch
        || !(task.browser
            || task.wants_extension()
            || task.record.is_some()
            || task.recipe.is_some()
            || task.save_recipe.is_some()
            || task.attach
            || task.target_match.is_some()
            || task.cdp_url.is_some()
            || task.cdp_port.is_some()
            || task.cloud_profile.is_some()
            || task.proxy_country.is_some()
            || !task.cloud_extensions.is_empty()
            || task.foreground
            || chrome.headed
            || chrome.profile.is_some())
}

/// Whether a run starts a Browser Use Cloud browser: it is handed none, fetches without one, and was not asked for a
/// browser on this machine by `local`, `launch`, a headed window or a kept profile, which only local Chrome has.
pub fn on_cloud(task: &RunTask, chrome: &LocalChrome) -> bool {
    let handed = task.cdp_url.is_some() || task.cdp_port.is_some();
    !(task.fetch
        || handed
        || task.wants_extension()
        || task.local
        || task.launch
        || chrome.headed
        || chrome.profile.is_some())
}

/// The browser a run drives: one it is handed, a cloud browser, or local Chrome.
async fn browser(
    task: &RunTask,
    host: &Host,
    chrome: &LocalChrome,
    http: &Http,
    starting: &mut Option<BrowserUseCloudBrowser>,
) -> Result<(BrowserConnection, Owned), Refused> {
    let key = host.browser_api_key.as_deref();
    let lazily = host.open_browser.is_some();
    let foreground = lazily || task.foreground || task.cursor == Some(true);
    if task.wants_extension() {
        let (record, connected) = pick_host(&host_dir(), task.extension_browser.as_deref())?;
        // The task after `#` names the run's tab group.
        let connection = BrowserConnection {
            attach: task.attach || task.target_match.is_some(),
            target_match: task.target_match.clone(),
            foreground,
            ..BrowserConnection::new(format!("{SCHEME}{}#{}", record.display(), task.task), true)
        };
        return Ok((connection, Owned::Extension(Box::new(connected))));
    }
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
            ("cloud_timeout_minutes", task.cloud_timeout_minutes.is_some()),
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
        timeout_minutes: task.cloud_timeout_minutes.unwrap_or(BrowserUseCloudOptions::default().timeout_minutes),
        profile: task.cloud_profile.clone(),
        viewport: task.viewport,
        extensions: task.cloud_extensions.clone(),
        ..BrowserUseCloudOptions::default()
    };
    let cloud = |error: fastbrowse_browser::adapters::browser_use_cloud::BrowserUseCloudError| Refused {
        unavailable: error.is_unavailable(),
        message: error.to_string(),
    };
    // Held by the caller while it starts, so a start that fails or is cancelled still reports the browser it billed.
    let remote = starting.insert(BrowserUseCloudBrowser::new(key, options).map_err(cloud)?);
    let connection = remote.start().await.map_err(cloud)?;
    let remote = starting.take().expect("the cloud browser was put there above");
    Ok((connection, Owned::Cloud(Box::new(remote))))
}

/// What a cloud browser whose start never finished is known to have cost. A start that failed stopped its browser
/// and read the final figure. A cancelled one leaves the browser to be stopped behind the run, so its reading is the
/// service's figure so far, and with no reading at all a browser may still have been created and billed: unknown.
fn unfinished_start(remote: BrowserUseCloudBrowser, cancelled: bool) -> Vec<CostLine> {
    let lines = remote.cost();
    // Dropped here, as the start's own future would have dropped it: that is what stops a browser left running.
    drop(remote);
    if !cancelled {
        return lines;
    }
    if lines.is_empty() {
        return vec![CostLine::new(CostComponent::Browser, CostBasis::Unknown, None)];
    }
    lines.into_iter().map(read_too_early).collect()
}

/// `line` as a reading taken before the service finished metering: a priced line can only grow, and an unpriced
/// one stays unknown.
fn read_too_early(line: CostLine) -> CostLine {
    let basis = if line.dollars.is_some() { CostBasis::Unsettled } else { CostBasis::Unknown };
    CostLine { basis, ..line }
}

/// The browser a run drove, for its result.
fn browser_used(owned: &Owned, task: &RunTask, chrome: &LocalChrome) -> BrowserUsed {
    let (via, profile, browser) = match owned {
        Owned::Handed => (BrowserVia::Attached, None, None),
        Owned::Local(_) => (BrowserVia::Launched, chrome.profile.as_ref().map(|p| p.display().to_string()), None),
        Owned::Cloud(_) => (BrowserVia::Cloud, task.cloud_profile.clone(), None),
        Owned::Extension(host) => (
            BrowserVia::Extension,
            Some(host.label.clone()),
            Some(format!("{} {}", host.browser, host.version).trim().to_owned()),
        ),
    };
    BrowserUsed { via, profile, browser }
}

/// Stop what the run started, keeping the cloud browser's last reported cost.
async fn stop(owned: Owned, cost: &mut Vec<CostLine>) {
    match owned {
        Owned::Handed | Owned::Extension(_) => {}
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
pub async fn run_task(task: RunTask, host: Host) -> Result<RunResult, RunError> {
    // Boxed so a caller holds a pointer rather than the whole run, which an unoptimized build copies into each
    // caller's frame: inline, a run on a 2 MB thread stack overflowed it before the browser started.
    Box::pin(run_unboxed(task, host)).await
}

async fn run_unboxed(mut task: RunTask, mut host: Host) -> Result<RunResult, RunError> {
    let Checked { schema, origins } = checked(&task)?;
    if host.open_browser.is_some() && task.viewport.is_some() {
        return Err(RunError::Invalid("viewport cannot be applied to a browser handed through open_browser".into()));
    }
    if let Some(path) = &task.save_recipe {
        fastbrowse_core::recipe::Recipe::check_destination(path)
            .map_err(|error| RunError::Invalid(format!("cannot save a recipe to {}: {error}", path.display())))?;
    }
    let recipe =
        task.recipe.as_deref().map(fastbrowse_core::recipe::Recipe::load).transpose().map_err(RunError::Invalid)?;
    if recipe.as_ref().is_some_and(|recipe| recipe.task != task.task) {
        return Err(RunError::Invalid(
            "the recipe task differs from this task; record a new recipe for changed instructions".into(),
        ));
    }
    let recipe_sink = host.recipe_sink.clone().or_else(|| task.save_recipe.is_some().then(RecipeSink::default));
    if task.fetch && host.browser_api_key.is_some() {
        return Err(RunError::Invalid("fetch starts no browser, so it cannot be combined with browser_api_key".into()));
    }
    let settings = match host.settings.take() {
        Some(settings) => settings,
        None => Settings::load()?,
    };
    let http = host.http.take().unwrap_or_else(Http::hyper);
    // The agent learns each secret, and its clients write the requests that may carry one, so they share a redactor.
    let trace = http.trace.clone().redacting(Arc::default());
    let http = http.with_trace(trace);
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
    let search = settings
        .exa_api_key
        .as_ref()
        .filter(|key| key.is_set())
        .map(|key| fastbrowse_clients::search::Exa::new(http.clone(), key.clone()));
    let mut fetch_spent: Vec<CostLine> = Vec::new();
    let chrome = task.chrome.clone().unwrap_or_else(|| settings.local_chrome());
    let mut limits = task.limits.clone();
    let mut resume = None;
    let told: Told = Arc::default();
    if let Some(app) = task.desktop.clone() {
        let on_event = noting(host.on_event.clone(), &told);
        return desktop(&task, &app, &host, schema, &config, &http, (jev, llm), on_event, &told).await;
    }
    if task.recipe.is_none() && task.save_recipe.is_none() && reads_first(&task, &chrome) {
        let on_event = noting(host.on_event.clone(), &told);
        let began = std::time::Instant::now();
        let clients = (jev.clone(), llm.clone());
        let read = fetched(
            &task,
            &host,
            schema.clone(),
            &config,
            http.clone(),
            clients,
            search.clone(),
            origins.clone(),
            on_event,
            &told,
        )
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
    // A `chrome` argument names local Chrome; with no browser named at all the run takes the cloud one.
    if host.browser_api_key.is_none()
        && host.open_browser.is_none()
        && task.chrome.is_none()
        && on_cloud(&task, &chrome)
    {
        host.browser_api_key = Some(
            settings
                .browser_key()
                .map_err(|error| RunError::from(error).after(&told, fetch_spent.clone(), Vec::new()))?
                .to_string(),
        );
    }
    let mut browser_cost: Vec<CostLine> = Vec::new();
    // Held here rather than by the agent, so what the browser run spent is still read when it is cancelled.
    // The browser run enforces what a fetched attempt left of the caller's limits but names them as set.
    let ledger =
        Arc::new(Ledger::continuing(task.limits.clone().unwrap_or_default(), limits.clone().unwrap_or_default()));
    let scratch = Scratch::new()
        .map_err(|error| RunError::Invalid(error.to_string()).after(&told, fetch_spent.clone(), Vec::new()))?;
    let mut session: Option<BrowserSession> = None;
    let mut owned: Option<Owned> = None;
    let mut starting: Option<BrowserUseCloudBrowser> = None;
    // Kept outside the run so each is closed however the run ends, before the browser is.
    let recording: Filming = Arc::default();
    let mut recording_failed: Option<String> = None;
    let mut feedback: Option<Arc<CursorFeedback>> = None;
    let work = async {
        check_browser_access(host.check_access.as_ref()).await?;
        // Asked while the browser starts: a task that answers a consent banner itself needs it left on the page.
        let consent = async {
            config.refuse_cookie_banners
                && fastbrowse_agent::start::answers_consent_itself(&*jev, &task.task, &ledger).await.unwrap_or(false)
        };
        let (started, answers_consent) = if task.wants_extension() {
            // The connected browser is found first, so with none the run ends before the consent check spends.
            let started = browser(&task, &host, &chrome, &http, &mut starting).await?;
            (Ok::<_, Refused>(started), consent.await)
        } else {
            tokio::join!(browser(&task, &host, &chrome, &http, &mut starting), consent)
        };
        let (connection, started) = started?;
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
        task.start = recipe_start(task.start.as_deref(), recipe.as_ref(), attach);
        let opened = BrowserSession::new(
            connection,
            sink(task.downloads.as_deref(), &scratch)?,
            SessionOptions {
                max_download_bytes: config.max_download_bytes as usize,
                refuse_cookie_banners: config.refuse_cookie_banners && !answers_consent,
                has_attachments: !task.attachments.is_empty(),
                on_frame: host.on_frame.clone(),
                check_access: host.check_access.clone(),
                viewport: matches!(owned.as_ref(), Some(Owned::Local(_))).then_some(task.viewport).flatten(),
                // The user stopping the run in their browser cancels it, as the caller cancelling it would.
                on_stop: {
                    let cancel = host.cancel.clone();
                    Some(Arc::new(move |reason: &str| cancel.trip_because(reason)))
                },
                trace: http.trace.clone(),
            },
        );
        session = Some(opened.clone());
        opened.open().await?;
        // A cloud or headless browser has no window on this machine to draw over.
        let extension_pid = match owned.as_ref() {
            Some(Owned::Extension(host)) => host.browser_pid,
            _ => None,
        };
        let shown = task.cursor != Some(false)
            && host.browser_api_key.is_none()
            && (extension_pid.is_some()
                || (is_loopback(&opened.connection().cdp_url)
                    && (task.cdp_url.is_some() || task.cdp_port.is_some() || chrome.headed)));
        feedback = cursor_feedback(shown).await.map(Arc::new);
        let pointer_marks = Arc::new(std::sync::Mutex::new(Vec::<fastbrowse_browser::pointer::PointerMark>::new()));
        let mut cdp = CdpPage::new(opened.clone(), config.clone())
            .with_cursor(feedback.clone())
            .with_cursor_browser_pid(extension_pid);
        if task.record.is_some() {
            cdp = cdp.with_pointer_marks(pointer_marks.clone());
        }
        let page: Arc<dyn Page> = Arc::new(cdp);
        let page: Arc<dyn Page> = match &search {
            Some(provider) => Arc::new(fetch::search::SearchPage::new(
                page,
                fetch::FetchedPage::new(http.clone(), origins.clone(), host.check_access.clone()),
                provider.clone(),
            )),
            None => page,
        };
        if let Some(path) = &task.record {
            match Recording::start_with_marks(opened.clone(), path, pointer_marks).await {
                Ok(started) => *recording.lock().await = Some(started),
                Err(error) => {
                    recording_failed = Some(error.0.clone());
                    return Err(refused(&error.0));
                }
            }
        }
        let agent = Agent::new(page, jev.clone(), llm.clone())
            .with_redactor(http.trace.redactor().unwrap_or_default())
            .with_config(config.clone())
            .with_secrets(host.secrets.clone())
            .with_trace(http.trace.clone())
            .with_events(Some(noting(captioned(host.on_event.clone(), &recording), &told)))
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
            limits: None,
            ledger: Some(ledger.clone()),
            authorization: task.authorization.clone(),
            until: host.until.clone(),
            resume: resume.take(),
            recipe: recipe.clone(),
            recipe_sink: recipe_sink.clone(),
        };
        let result = agent.run(&task.task, run).await.map_err(|error| refused(&error.to_string()))?;
        // The card is a data: document navigated into the tab, which would replace the caller's own app page; an
        // attached window is theirs and stays where it is, as does a tab the run left for a person to finish.
        // Shown while the recording stays in its slot, so a run cancelled during the card still finishes the videos.
        if !attach
            && !result.status.waits_on_person()
            && let Some(filming) = recording.lock().await.as_mut()
        {
            let _ = filming.show_result(&task.task, &card(&result)).await;
        }
        Ok(result)
    };
    let outcome: Option<Result<RunResult, Refused>> = host.cancel.guard(Box::pin(work)).await.ok();
    if let Some(remote) = starting.take() {
        browser_cost = unfinished_start(remote, outcome.is_none());
    }
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
    let browser_used = owned.as_ref().map(|owned| browser_used(owned, &task, &chrome));
    if let Some(owned) = owned.take() {
        stop(owned, &mut browser_cost).await;
    }
    let result = match outcome {
        None => {
            // In the order a finished run's lines are: the fetched read, the browser run, then the browser itself.
            let mut lines = fetch_spent;
            lines.extend(ledger.breakdown().lines);
            lines.extend(browser_cost);
            return Err(cancelled(&told, lines, recordings, browser_used, host.cancel.reason()));
        }
        Some(Ok(result)) => result,
        Some(Err(refusal)) => {
            // A cloud browser that cannot be started is an outage, not a failed run.
            RunResult {
                browser: None,
                status: if refusal.unavailable { Status::Unavailable } else { Status::Error },
                budget: None,
                answer: None,
                data: Value::Null,
                evidence: Vec::new(),
                steps: Vec::new(),
                // What the browser run spent before it failed, which is nothing when it never started.
                cost: ledger.breakdown(),
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
    let mut result = finish(result, recording_failed, &told, fetch_spent, browser_cost, recordings)?;
    result.browser = browser_used;
    if result.status == Status::Complete
        && let (Some(path), Some(sink)) = (&task.save_recipe, &recipe_sink)
    {
        let outcome = sink.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        if let Some(outcome) = outcome
            && let Err(error) = outcome.recipe.save(path)
        {
            fastbrowse_browser::warn(&format!(
                "the run completed but its recipe could not be written to {}: {error}",
                path.display()
            ));
        }
    }
    Ok(result)
}

fn finish(
    mut result: RunResult,
    recording_failed: Option<String>,
    told: &Told,
    mut fetch_spent: Vec<CostLine>,
    browser_cost: Vec<CostLine>,
    recordings: Vec<PathBuf>,
) -> Result<RunResult, RunError> {
    result.cost.lines.extend(browser_cost);
    fetch_spent.append(&mut result.cost.lines);
    result.cost.lines = fetch_spent;
    result.recordings = recordings;
    if let Some(message) = recording_failed {
        return Err(RunError::Recording(message).after(told, result.cost.lines, result.recordings));
    }
    Ok(result)
}

fn recipe_start(
    explicit: Option<&str>,
    recipe: Option<&fastbrowse_core::recipe::Recipe>,
    attach: bool,
) -> Option<String> {
    explicit.map(str::to_owned).or_else(|| (!attach).then(|| recipe.and_then(|recipe| recipe.start.clone())).flatten())
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
    (jev, llm): (Arc<dyn DecisionClient>, Arc<dyn LLMClient>),
    search: Option<fastbrowse_clients::search::Exa>,
    origins: Option<Vec<String>>,
    on_event: EventHandler,
    told: &Told,
) -> Result<Read, RunError> {
    if check_browser_access(host.check_access.as_ref()).await.is_err() {
        // The caller's own gate refused the run, which is an error result, not an error to the embedder. Python reports
        // the exception's class name.
        let result = RunResult {
            browser: None,
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
    let redactor = http.trace.redactor().unwrap_or_default();
    let trace = http.trace.clone();
    let fetched = Arc::new(fetch::FetchedPage::new(http.clone(), origins.clone(), host.check_access.clone()));
    let page: Arc<dyn Page> = fetched.clone();
    let search_page = search.map(|provider| {
        Arc::new(fetch::search::SearchPage::new(
            page.clone(),
            fetch::FetchedPage::new(http.clone(), origins.clone(), host.check_access.clone()),
            provider,
        ))
    });
    let page: Arc<dyn Page> = search_page.as_ref().map_or(page, |page| page.clone());
    let agent = Agent::new(page, jev, llm)
        .with_redactor(redactor)
        .with_config(config.clone())
        .with_secrets(host.secrets.clone())
        .with_trace(trace)
        .with_events(Some(on_event));
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
        recipe: None,
        recipe_sink: None,
    };
    let Ok(outcome) = host.cancel.guard(agent.run(&task.task, run)).await else {
        return Err(cancelled(told, ledger.breakdown().lines, Vec::new(), None, host.cancel.reason()));
    };
    let result = outcome.map_err(|error| RunError::Invalid(error.to_string()))?;
    // Read from the page rather than the message: a refusal the page itself raised means the task needs a browser. It
    // ends the run blocked, or unverified when the refusal was the screenshot an ending frame asked for. A bot check Jev
    // saw in the document is the other: its script clears it only in a browser.
    let bot_check = result.wall.as_ref().is_some_and(|wall| wall.kind == WallKind::BotCheck);
    let needs_browser = matches!(result.status, Status::Blocked | Status::Unverified)
        && (fetched.needed_browser() || search_page.as_ref().is_some_and(|page| page.needed_browser()) || bot_check);
    let spent = ledger.state().clone();
    Ok(Read { result, needs_browser, spent, resume: agent.resume() })
}

/// A run on a desktop app the run starts: the same agent, with deskpilot as its page. There is no address to start
/// from, so the run begins on the app's window as it opens.
#[allow(clippy::too_many_arguments)]
async fn desktop(
    task: &RunTask,
    app: &str,
    host: &Host,
    schema: Option<Arc<RecordSchema>>,
    config: &Config,
    http: &Http,
    (jev, llm): (Arc<dyn DecisionClient>, Arc<dyn LLMClient>),
    on_event: EventHandler,
    told: &Told,
) -> Result<RunResult, RunError> {
    let program = std::env::var("FASTBROWSE_DESKPILOT").unwrap_or_else(|_| "deskpilot".into());
    let ledger = Arc::new(Ledger::new(task.limits.clone().unwrap_or_default()));
    let run = async {
        // `check` refused a command line that does not split into words.
        let command = shlex::split(app).unwrap_or_default();
        let page = match desktop::DeskpilotPage::open(&program, &command).await {
            Ok(page) => page,
            Err(desktop::OpenError::Invalid(message)) => return Err(RunError::Invalid(message)),
            // A deskpilot that cannot start or answer is an outage, as a cloud browser that cannot start is.
            Err(desktop::OpenError::Unavailable(message)) => {
                return Ok(RunResult {
                    browser: None,
                    status: Status::Unavailable,
                    budget: None,
                    answer: None,
                    data: Value::Null,
                    evidence: Vec::new(),
                    steps: Vec::new(),
                    cost: ledger.breakdown(),
                    artifacts: Vec::new(),
                    citations: Vec::new(),
                    error: Some(message),
                    final_url: None,
                    final_frame: None,
                    recordings: Vec::new(),
                    wall: None,
                });
            }
        };
        let agent = Agent::new(Arc::new(page), jev, llm)
            .with_redactor(http.trace.redactor().unwrap_or_default())
            .with_config(config.clone())
            .with_secrets(host.secrets.clone())
            .with_trace(http.trace.clone())
            .with_events(Some(on_event));
        let run = RunOptions {
            start: None,
            choose_start: false,
            context: task.context.clone(),
            inputs: task.inputs.clone().unwrap_or_default(),
            attachments: attachments(task),
            output_schema: schema,
            limits: None,
            ledger: Some(ledger.clone()),
            authorization: task.authorization.clone(),
            until: host.until.clone(),
            resume: None,
            recipe: None,
            recipe_sink: None,
        };
        agent.run(&task.task, run).await.map_err(|error| RunError::Invalid(error.to_string()))
    };
    // Starting deskpilot is inside the guard too, so a cancel drops a startup that hangs and its child with it.
    let Ok(outcome) = host.cancel.guard(run).await else {
        return Err(cancelled(told, ledger.breakdown().lines, Vec::new(), None, host.cancel.reason()));
    };
    outcome
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn a_stop_that_says_why_is_the_errors_text() {
        let cancel = Cancel::default();
        cancel.trip_because("the run was stopped from the fastbrowse extension's popup");
        cancel.trip_because("a later reason");
        let error = cancel.guard(std::future::pending::<()>()).await.unwrap_err();
        assert_eq!(error.to_string(), "the run was stopped from the fastbrowse extension's popup");
        let error = Cancel::default();
        error.trip();
        assert_eq!(error.guard(std::future::pending::<()>()).await.unwrap_err().to_string(), "the run was cancelled");
    }

    #[test]
    fn a_recording_start_failure_keeps_fetch_model_and_browser_charges() {
        let fetch = CostLine::new(CostComponent::Jev, CostBasis::Metered, Some(0.01));
        let model = CostLine::new(CostComponent::Llm, CostBasis::Metered, Some(0.02));
        let browser = CostLine::new(CostComponent::Browser, CostBasis::Metered, Some(0.03));
        let result = serde_json::from_value(serde_json::json!({
            "status": "error", "answer": null, "data": null, "evidence": [], "steps": [],
            "cost": {"lines": [model.clone()]}, "artifacts": [],
        }))
        .unwrap();
        let error = finish(
            result,
            Some("ffmpeg failed to start".into()),
            &Arc::default(),
            vec![fetch.clone()],
            vec![browser.clone()],
            Vec::new(),
        )
        .unwrap_err();
        assert!(matches!(error.cause(), RunError::Recording(_)));
        let receipt = error.receipt().expect("recording failure must retain the closed browser's charges");
        assert_eq!(receipt.cost.lines, [fetch, model, browser]);
    }

    #[test]
    fn a_recipe_start_is_used_only_when_the_caller_did_not_supply_one() {
        let recipe = fastbrowse_core::recipe::Recipe {
            version: 1,
            task: "t".into(),
            start: Some("https://saved.example/".into()),
            steps: vec![],
        };
        assert_eq!(recipe_start(None, Some(&recipe), false).as_deref(), Some("https://saved.example/"));
        assert_eq!(
            recipe_start(Some("https://explicit.example/"), Some(&recipe), false).as_deref(),
            Some("https://explicit.example/")
        );
        assert_eq!(recipe_start(None, None, false), None);
        assert_eq!(recipe_start(None, Some(&recipe), true), None);
        assert_eq!(
            recipe_start(Some("https://explicit.example/"), Some(&recipe), true).as_deref(),
            Some("https://explicit.example/")
        );
    }

    /// A stand-in Browser Use Cloud that creates a browser at `$0.01` so far, with a DevTools address that never
    /// answers, so a start against it stays unfinished until it is cancelled.
    async fn a_cloud_that_never_finishes_starting() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let devtools = origin.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let devtools = devtools.clone();
                tokio::spawn(async move {
                    let mut asked = vec![0; 8192];
                    let read = socket.read(&mut asked).await.unwrap_or(0);
                    if !String::from_utf8_lossy(&asked[..read]).starts_with("POST /api/v4/browsers") {
                        // The DevTools address and the stop behind a cancelled start: held open, never answered.
                        return tokio::time::sleep(Duration::from_secs(60)).await;
                    }
                    let body = format!(r#"{{"id":"b1","cdpUrl":"{devtools}","browserCost":"0.01"}}"#);
                    let reply = format!(
                        "HTTP/1.1 201 Created\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                });
            }
        });
        origin
    }

    fn cloud(origin: &str) -> BrowserUseCloudBrowser {
        let clock: fastbrowse_browser::adapters::browser_use_cloud::Clock = Arc::new(|| 0.0);
        let sleep: fastbrowse_browser::adapters::browser_use_cloud::Sleep = Arc::new(|_| Box::pin(async {}));
        let options = BrowserUseCloudOptions { timeout_minutes: 15, ..BrowserUseCloudOptions::default() };
        BrowserUseCloudBrowser::with_seams("key", options, origin, clock, sleep).unwrap()
    }

    #[tokio::test]
    async fn a_start_cancelled_after_the_browser_was_created_keeps_the_figure_so_far_as_unsettled() {
        let origin = a_cloud_that_never_finishes_starting().await;
        let mut starting = Some(cloud(&origin));
        let start = starting.as_mut().unwrap().start();
        // Cancelled while it waits on the browser's DevTools address, after the service answered the create.
        assert!(tokio::time::timeout(Duration::from_millis(500), start).await.is_err());
        let lines = unfinished_start(starting.take().unwrap(), true);
        assert_eq!(
            lines.iter().map(|line| (line.component, line.basis, line.dollars)).collect::<Vec<_>>(),
            [
                (CostComponent::Browser, CostBasis::Unsettled, Some(0.01)),
                (CostComponent::Proxy, CostBasis::Unknown, None),
            ]
        );
    }

    #[tokio::test]
    async fn a_start_cancelled_before_the_service_answered_is_an_unknown_charge_not_a_free_one() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let mut starting = Some(cloud(&origin));
        let start = starting.as_mut().unwrap().start();
        assert!(tokio::time::timeout(Duration::from_millis(200), start).await.is_err());
        let lines = unfinished_start(starting.take().unwrap(), true);
        assert_eq!(lines, [CostLine::new(CostComponent::Browser, CostBasis::Unknown, None)]);
        drop(listener);
    }

    #[test]
    fn a_figure_read_too_early_can_only_grow_and_a_start_never_made_has_no_line() {
        let metered = CostLine::new(CostComponent::Browser, CostBasis::Metered, Some(0.02));
        assert_eq!(read_too_early(metered.clone()).basis, CostBasis::Unsettled);
        assert_eq!(read_too_early(CostLine::new(CostComponent::Proxy, CostBasis::Unknown, None)).dollars, None);
        // A start that never reached the service billed nothing that can be known, and is not a line at all.
        let never_sent = cloud("http://127.0.0.1:9");
        assert!(unfinished_start(never_sent, false).is_empty());
    }

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
