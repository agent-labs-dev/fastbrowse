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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RecipeSink, Resume, RunOptions};
use fastbrowse_agent::verification::RecordSchema;
use fastbrowse_browser::adapters::browser_use_cloud::{BrowserUseCloudBrowser, BrowserUseCloudOptions};
use fastbrowse_browser::adapters::local_chrome::LocalChromeProcess;
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
    ArtifactSink, Attachment, BrowserConnection, BrowserEvent, BrowserUsed, BrowserVia, BrowserWarningEvent,
    BudgetResource, BudgetStop, Bytes, CostBasis, CostBreakdown, CostComponent, CostLine, Event, EventHandler,
    FailureKind, FrameHandler, Limits, LocalChrome, RunResult, SecretRef, SecretResolver, Status, StepResult,
    UntilCheck, WallKind,
};
use fastbrowse_core::origins::parse_origins;
use fastbrowse_core::output_schema::output_model;
use fastbrowse_core::page::Page;
use fastbrowse_core::protocol::run_task_fields;
use fastbrowse_core::py::text::repr;
use fastbrowse_core::safety::{Redactor, mask_userinfo};
use fastbrowse_core::schema::Schema;
use fastbrowse_core::telemetry::{Ledger, LedgerState, Trace};
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

impl FailureKind for RunError {
    fn failure_kind(&self) -> String {
        match self {
            RunError::Invalid(_) => "Invalid".into(),
            RunError::UnsupportedSchema(_) => "UnsupportedSchema".into(),
            RunError::Configuration(_) => "Configuration".into(),
            RunError::Recording(_) => "Recording".into(),
            RunError::Cancelled(_) => "Cancelled".into(),
            RunError::AfterSpending { error, .. } => error.failure_kind(),
        }
    }
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
/// wording wherever the run was asked for. `secrets` says whether the run was given any, which an entry point knows
/// before it resolves one: refusing them after resolving named a missing variable or vault item instead (#1337).
pub fn check(task: &RunTask, secrets: bool) -> Result<(), RunError> {
    // Called before `run_task`, so outside the run's redactor, and a refusal quotes the value it refused (a proxy
    // country, a DevTools endpoint) to an embedder that may log it: masked by what the task holds, each string learnt
    // as one whole address, as every door learns a message before it says anything of it (#1381).
    checked(task, secrets).map(drop).map_err(|error| {
        let learnt = serde_json::to_value(task).map(|given| fastbrowse_core::safety::learnt(&given));
        redacted_error(error, &learnt.unwrap_or_default())
    })
}

fn checked(task: &RunTask, secrets: bool) -> Result<Checked, RunError> {
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
            // A fetched page can only follow links (`FetchedPage::act`), so nothing is typed, attached or submitted:
            // accepting these reported a run that could use them when none could (#1337).
            ("inputs", task.inputs.as_ref().is_some_and(|inputs| !inputs.is_empty())),
            ("attachments", !task.attachments.is_empty()),
            ("authorization", task.authorization.as_ref().is_some_and(|given| given.irreversible_actions)),
            // Secrets are typed into fields, and a fetched page has none to type into.
            ("secrets", secrets),
        ]);
        if !browser.is_empty() {
            return refuse(format!("fetch starts no browser, so it cannot be combined with {}", browser.join(", ")));
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
    DirectorySink::new(root).map(|sink| Arc::new(sink) as Arc<dyn ArtifactSink>).map_err(|error| {
        BrowserError::failed("downloads/unwritable", format!("cannot keep downloads in {}: {error}", root.display()))
    })
}

/// The websocket URL of the browser serving DevTools on `port`, read from its `/json/version` endpoint.
pub async fn resolve_cdp_port(port: u16, host: &str, http: &Http) -> Result<String, BrowserError> {
    // Named in every error below, which an embedder may log; a host written with credentials keeps them out.
    let endpoint = mask_userinfo(&format!("http://{host}:{port}/json/version"));
    let address = format!("http://{host}:{port}/json/version");
    let response = http.get(&address, &[], 5.0).await.map_err(|_| {
        BrowserError::failed("devtools/ConnectError", format!("no DevTools endpoint at {endpoint} (ConnectError)"))
    })?;
    if response.status == 404 {
        // Chrome's remote debugging switch at chrome://inspect serves the websocket but not this discovery route.
        return Err(BrowserError::failed(
            "devtools/http-404",
            format!(
                "{endpoint} answered 404, so it names no DevTools websocket. A Chrome whose remote debugging was turned on at chrome://inspect serves none here: its DevToolsActivePort file holds the port on the first line and the browser path on the second, so pass ws://127.0.0.1:<port><path> as cdp_url instead."
            ),
        ));
    }
    if !(200..300).contains(&response.status) {
        return Err(BrowserError::failed(
            format!("devtools/http-{}", response.status),
            format!("{endpoint} answered {}, so it names no DevTools websocket", response.status),
        ));
    }
    let version: Value = serde_json::from_slice(&response.body).map_err(|_| {
        BrowserError::failed(
            "devtools/JSONDecodeError",
            format!("no DevTools endpoint at {endpoint} (JSONDecodeError)"),
        )
    })?;
    match version.get("webSocketDebuggerUrl").and_then(Value::as_str) {
        Some(url) if !url.is_empty() => Ok(url.to_string()),
        _ => Err(BrowserError::failed("devtools/no-websocket", format!("{endpoint} named no webSocketDebuggerUrl"))),
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
        return Err(BrowserError::failed(
            "connect_cdp/both-named",
            "port and cdp_url both name a browser to attach to; pass one",
        ));
    }
    let cdp_url = match (options.cdp_url, options.port) {
        (Some(url), _) => url,
        (None, Some(port)) => resolve_cdp_port(port, &options.host, &Http::hyper()).await?,
        (None, None) => {
            return Err(BrowserError::failed("connect_cdp/none-named", "connect_cdp needs a port or a cdp_url"));
        }
    };
    let allowed = match options.allowed_origins {
        None => None,
        Some(origins) => Some(
            parse_origins(origins)
                .map_err(|error| BrowserError::failed("allowed_origins/invalid", error.to_string()))?,
        ),
    };
    let connection = BrowserConnection {
        attach: options.attach || options.target_match.is_some(),
        target_match: options.target_match,
        foreground: options.foreground,
        allowed_origins: allowed,
        ..BrowserConnection::new(cdp_url, true)
    };
    let config = options.config.unwrap_or_default();
    let scratch = Scratch::new().map_err(|error| BrowserError::failed("scratch/unwritable", error.to_string()))?;
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

struct WarningTask(Option<tokio::task::JoinHandle<()>>);

impl Drop for WarningTask {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

impl WarningTask {
    fn start(
        on_event: EventHandler,
    ) -> (Self, tokio::sync::mpsc::UnboundedSender<BrowserWarningEvent>, tokio::sync::oneshot::Sender<()>) {
        let (warning_tx, mut warning_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(warning) = warning_rx.recv() => on_event(Event::BrowserWarning(warning)).await,
                    _ = &mut stop_rx => {
                        while let Ok(warning) = warning_rx.try_recv() {
                            on_event(Event::BrowserWarning(warning)).await;
                        }
                        break;
                    }
                }
            }
        });
        (Self(Some(task)), warning_tx, stop_tx)
    }

    async fn finish(mut self) {
        if let Some(task) = self.0.as_mut() {
            let _ = (&mut *task).await;
        }
        let _ = self.0.take();
    }
}

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
    /// `RunResult::failure_kind`: what failed, named from a typed error or the rule that refused the run.
    kind: Option<String>,
}

impl From<BrowserError> for Refused {
    fn from(error: BrowserError) -> Self {
        let kind = Some(error.failure_kind());
        Refused { unavailable: error.is_unavailable(), message: error.to_string(), kind }
    }
}

fn refused(kind: &str, message: &str) -> Refused {
    Refused { message: message.into(), unavailable: false, kind: Some(kind.into()) }
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
    let foreground = lazily || task.foreground;
    if task.wants_extension() {
        let (record, connected) = pick_host(&host_dir(), task.extension_browser.as_deref())?;
        let connection = BrowserConnection {
            attach: task.attach || task.target_match.is_some(),
            target_match: task.target_match.clone(),
            foreground,
            ..BrowserConnection::new(extension_address(&record, &task.task, &http.trace), true)
        };
        return Ok((connection, Owned::Extension(Box::new(connected))));
    }
    let handed = match (&host.open_browser, &task.cdp_url, task.cdp_port) {
        (Some(open), ..) => Some(open().await.map_err(|error| refused("OpenBrowser", &error))?),
        (None, Some(url), _) => Some(url.clone()),
        (None, None, Some(port)) => Some(resolve_cdp_port(port, "127.0.0.1", http).await?),
        (None, None, None) => None,
    };
    if let Some(cdp_url) = handed {
        if key.is_some() {
            return Err(refused(
                "Options/cdp-and-cloud-key",
                "cdp_url/cdp_port is a browser to attach to; a cloud key would start a second one",
            ));
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
            return Err(refused(
                &format!("Options/{name}-needs-cloud-key"),
                &format!("{name} belongs to a Browser Use Cloud browser, which needs a key"),
            ));
        }
        let (process, connection) = LocalChromeProcess::launch(chrome).await.map_err(|error| Refused {
            kind: Some(format!("LocalChrome.{}", error.failure_kind())),
            message: error.to_string(),
            unavailable: false,
        })?;
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
        kind: Some(format!("BrowserUseCloud.{}", error.failure_kind())),
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

/// The fields of a run's arguments that may carry an address's credentials: what a model reads as the task, where it
/// starts, its background and its inputs, which are masked wherever they are shown, and the DevTools address, which is
/// only connected to. Every other field is used or handed back as written (a path the caller looks up, an id a reply
/// is matched by, a selector, a schema key the data comes back under), so masking one would break the caller's own
/// use of it, and left raw it reached results, events, errors and model requests (#1381).
const CREDENTIAL_FIELDS: [&str; 5] = ["task", "start", "context", "inputs", "cdp_url"];

/// Learn every credential in everything the caller handed the run, then refuse the run when any field outside
/// `CREDENTIAL_FIELDS` holds one: an address's userinfo, or text a credential written elsewhere in the request
/// repeats. Each field is read by its type (`run_task_fields`), so a field added later is learnt and checked with the
/// rest, and a closed set's value is never mistaken for text. The refusal names the field, never its value.
fn admit(redactor: &Mutex<Redactor>, task: &RunTask, secrets: &[SecretRef]) -> Result<(), RunError> {
    let mut redactor = redactor.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let given =
        serde_json::to_value(task).map_err(|_| RunError::Invalid("the run's arguments cannot be read".into()))?;
    let secrets: Vec<Value> = secrets.iter().filter_map(|secret| serde_json::to_value(secret).ok()).collect();
    redactor.register_tree(&given);
    secrets.iter().for_each(|secret| redactor.register_tree(secret));
    let held = run_task_fields().into_iter().find(|field| {
        !CREDENTIAL_FIELDS.contains(&field.key())
            && given.get(field.key()).is_some_and(|value| redactor.holds_typed(&field.ty, value))
    });
    if let Some(field) = held {
        return Err(RunError::Invalid(format!(
            "{} holds a credential; only task, start, context, inputs and cdp_url may carry an address's credentials",
            field.key()
        )));
    }
    // A secret's name and origins are sent back to the caller to resolve it, so neither can be masked on the way.
    if secrets.iter().any(|secret| redactor.holds_typed(&SecretRef::ty(), secret)) {
        return Err(RunError::Invalid(
            "a secret's name or origin holds a credential; only task, start, context, inputs and cdp_url may carry an address's credentials".into(),
        ));
    }
    Ok(())
}

/// What else the run reads that the caller wrote, learnt before anything is shown: a recipe, whose start is refused at
/// load when it carries credentials but whose steps' links, labels and text can write an address a replay shows a
/// model, and the settings' text, whose base addresses may carry the provider's credentials (#1381).
fn learn_credentials(
    redactor: &Mutex<Redactor>,
    recipe: Option<&fastbrowse_core::recipe::Recipe>,
    settings: &Settings,
) -> bool {
    let mut redactor = redactor.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(recipe) = recipe.and_then(|recipe| serde_json::to_value(recipe).ok()) {
        redactor.register_tree(&recipe);
    }
    settings.text().iter().for_each(|(_, value)| redactor.register_tree(value));
    redactor.holds_credentials()
}

/// `on_event` with every event redacted on its way out (`Redactor::redacted`), the browser's warnings and the steps
/// alike: an event is built from what the run holds, and a field read straight from the caller or a page reached the
/// caller's handler, a serve client's `run/event` and the MCP log raw.
fn redacting(on_event: EventHandler, redactor: Arc<Mutex<Redactor>>) -> EventHandler {
    Arc::new(move |event| {
        let redacted = redactor.lock().unwrap_or_else(std::sync::PoisonError::into_inner).redacted(&event);
        match redacted {
            Some(event) => on_event(event),
            None => {
                fastbrowse_browser::warn("an event that could not be redacted was not sent");
                Box::pin(async {})
            }
        }
    })
}

/// The run's one way out: its result or error, every string the caller, a page or a model wrote redacted, map keys
/// included, so a path, a profile, a data key, an artifact's name or an error built from any of them is redacted
/// without a sink of its own (#1381).
fn exited(outcome: Result<RunResult, RunError>, redactor: &Mutex<Redactor>) -> Result<RunResult, RunError> {
    let redactor = redactor.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match outcome {
        Ok(mut result) => {
            let frame = result.final_frame.take();
            let mut redacted = redactor.redacted(&result).ok_or_else(|| {
                RunError::Invalid("the run's result could not be redacted, so it was withheld".into())
            })?;
            redacted.final_frame = frame;
            Ok(redacted)
        }
        Err(error) => Err(redacted_error(error, &redactor)),
    }
}

fn redacted_error(error: RunError, redactor: &Redactor) -> RunError {
    match error {
        RunError::Invalid(message) => RunError::Invalid(redactor.redact(&message)),
        RunError::UnsupportedSchema(message) => RunError::UnsupportedSchema(redactor.redact(&message)),
        RunError::Configuration(message) => RunError::Configuration(redactor.redact(&message)),
        RunError::Recording(message) => RunError::Recording(redactor.redact(&message)),
        RunError::Cancelled(receipt) => RunError::Cancelled(redacted_receipt(receipt, redactor)),
        RunError::AfterSpending { error, receipt } => RunError::AfterSpending {
            error: Box::new(redacted_error(*error, redactor)),
            receipt: redacted_receipt(receipt, redactor),
        },
    }
}

fn redacted_receipt(receipt: CancellationReceipt, redactor: &Redactor) -> CancellationReceipt {
    let reason = receipt.reason.as_deref().map(|reason| redactor.redact(reason).into_boxed_str());
    // Unreadable once redacted, the receipt keeps only its charges, which name models and purposes, never text.
    let redacted = redactor
        .redacted(&receipt)
        .unwrap_or_else(|| CancellationReceipt { cost: receipt.cost.clone(), ..Default::default() });
    CancellationReceipt { reason, ..redacted }
}

/// The extension's address for the host at `record`, the task after `#` naming the run's tab group. The name is shown
/// in the browser and traced with the connection, so it carries the task as the trace writes it.
fn extension_address(record: &Path, task: &str, trace: &Trace) -> String {
    format!("{SCHEME}{}#{}", record.display(), redacted(trace, task))
}

/// `text` as the run's trace writes it out, its secrets and every address's credentials named.
fn redacted(trace: &Trace, text: &str) -> String {
    trace
        .redactor()
        .map_or_else(|| fastbrowse_core::safety::Redactor::default().redact(text), |r| r.lock().unwrap().redact(text))
}

/// Open `start`, pursue `task`, and return what the run could prove.
///
/// The run learns every credential the caller handed it before anything else (`admit`), and everything it hands
/// back, its result, its error and each event, passes one redaction on the way (`exited`, `redacting`).
pub async fn run_task(task: RunTask, mut host: Host) -> Result<RunResult, RunError> {
    let redactor: Arc<Mutex<Redactor>> = Arc::default();
    fastbrowse_core::safety::live(&redactor);
    let secrets: Vec<SecretRef> = host.secrets.iter().flat_map(|secrets| secrets.available()).collect();
    let admitted = admit(&redactor, &task, &secrets);
    if let Err(refusal) = admitted {
        return exited(Err(refusal), &redactor);
    }
    // A declared value is masked from here, before anything types it: a token the caller put in the start address's
    // query is no credential by its structure, and reached every model and exit as given (#1381).
    if let Some(secrets) = &host.secrets {
        redactor.lock().unwrap_or_else(std::sync::PoisonError::into_inner).declare_held(secrets.as_ref());
    }
    host.on_event = host.on_event.take().map(|on_event| redacting(on_event, redactor.clone()));
    // Boxed so a caller holds a pointer rather than the whole run, which an unoptimized build copies into each
    // caller's frame: inline, a run on a 2 MB thread stack overflowed it before the browser started.
    let outcome = Box::pin(run_unboxed(task, host, redactor.clone())).await;
    exited(outcome, &redactor)
}

async fn run_unboxed(mut task: RunTask, mut host: Host, redactor: Arc<Mutex<Redactor>>) -> Result<RunResult, RunError> {
    // A resolver that declares no secret gives the run none to type, as an empty list to `serve` does; counting the
    // resolver itself refused a fetch run for an empty `ScopedSecrets` that serve and the CLI accept.
    let secrets = host.secrets.as_ref().is_some_and(|secrets| !secrets.available().is_empty());
    let Checked { schema, origins } = checked(&task, secrets)?;
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
    let trace = http.trace.clone().redacting(redactor.clone());
    let http = http.with_trace(trace);
    // Learned before anything reads the task: the consent check, the extension's tab group and the first frame all
    // come before the agent, and each took an address's credentials as the caller wrote them (#1381).
    let credentialed = learn_credentials(&redactor, recipe.as_ref(), &settings);
    let (warning_task, warning_tx, warning_stop_tx) = host
        .on_event
        .clone()
        .map(|on_event| {
            let (task, warning_tx, stop_tx) = WarningTask::start(on_event);
            (task, warning_tx, stop_tx)
        })
        .map_or((None, None, None), |(task, tx, stop)| (Some(task), Some(tx), Some(stop)));
    // The providers this run calls, for an eval to keep beside its result. A caller's own clients are not described.
    if host.jev.is_none() && host.llm.is_none() {
        http.trace.event("route", || Map::from_iter([("providers".into(), Value::from(settings.providers()))]));
    }
    let jev: Arc<dyn DecisionClient> = match host.jev.take() {
        Some(jev) => jev,
        None => Arc::from(settings.resolve_decision(&http).await?),
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
    let work = async {
        check_browser_access(host.check_access.as_ref()).await?;
        // Asked while the browser starts: a task that answers a consent banner itself needs it left on the page.
        let consent = async {
            config.refuse_cookie_banners
                && fastbrowse_agent::start::answers_consent_itself(&*jev, &redacted(&http.trace, &task.task), &ledger)
                    .await
                    .unwrap_or(false)
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
        let attach = connection.attach;
        task.start = recipe_start(task.start.as_deref(), recipe.as_ref(), attach);
        // One predicate for every image of the start page: a frame, a recording and a live view all show its pixels,
        // address bar included, before any reading can say what they show. A start holding a declared secret got a
        // live view while its frames were held (#1381).
        let images_held =
            redactor.lock().unwrap_or_else(std::sync::PoisonError::into_inner).holds_images_from(task.start.as_deref());
        if let Some(on_event) = &host.on_event {
            // A live view cannot be recalled once given, so a run whose images are held from its start gives none. A
            // credential learnt later is held from then on in frames, and the view already given stays.
            let event = BrowserEvent {
                kind: Default::default(),
                live_url: connection.live_url.clone().filter(|_| !images_held),
                browser_id: connection.browser_id.clone(),
            };
            on_event(Event::Browser(event)).await;
        }
        let opened = BrowserSession::new(
            connection,
            sink(task.downloads.as_deref(), &scratch)?,
            SessionOptions {
                max_download_bytes: config.max_download_bytes as usize,
                refuse_cookie_banners: config.refuse_cookie_banners && !answers_consent,
                has_attachments: !task.attachments.is_empty(),
                on_frame: host.on_frame.clone(),
                on_warning: host.on_event.as_ref().map(|_| {
                    let warning_tx = warning_tx.as_ref().expect("warning task has sender").clone();
                    Arc::new(move |warning| {
                        let _ = warning_tx.send(warning);
                    }) as Arc<dyn Fn(BrowserWarningEvent) + Send + Sync>
                }),
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
        // Held from the first frame to the end of the run: a frame cannot be checked for a value as it arrives, and
        // the start page's address bar and content can show the credentials it was opened with.
        // A declared secret in the start is held only until a reading of the page shows it gone: from the first
        // reading on, the agent holds frames while the page shows it.
        if credentialed {
            opened.hold_images();
        } else if images_held {
            opened.set_frames_withheld(true);
        }
        opened.open().await?;
        // The cursor is drawn wherever someone can see the page: the user's own Chrome, a headed or attached local
        // browser, a cloud browser's live view, the frames a host streams, or a recording. A headless run nobody
        // watches or films has no one to draw it for.
        let watched = matches!(owned.as_ref(), Some(Owned::Extension(_)))
            || opened.connection().live_url.is_some()
            || host.on_frame.is_some()
            || task.record.is_some()
            || (is_loopback(&opened.connection().cdp_url)
                && (task.cdp_url.is_some() || task.cdp_port.is_some() || chrome.headed));
        let mut cdp = CdpPage::new(opened.clone(), config.clone());
        if task.cursor.unwrap_or(watched) {
            cdp = cdp.with_cursor();
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
            match Recording::start(opened.clone(), path).await {
                Ok(started) => *recording.lock().await = Some(started),
                Err(error) => {
                    recording_failed = Some(error.0.clone());
                    return Err(refused("Recording/start", &error.0));
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
        let result = agent.run(&task.task, run).await.map_err(|error| Refused {
            kind: Some(error.failure_kind()),
            message: error.to_string(),
            unavailable: false,
        })?;
        // The card is a data: document navigated into the tab, which would replace the caller's own app page; an
        // attached window is theirs and stays where it is, as does a tab the run left for a person to finish.
        // Shown while the recording stays in its slot, so a run cancelled during the card still finishes the videos.
        if !attach
            && !result.status.waits_on_person()
            && let Some(filming) = recording.lock().await.as_mut()
        {
            let _ = filming.show_result(&redacted(&http.trace, &task.task), &card(&result)).await;
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
    if let Some(session) = &session {
        let _ = session.close().await;
    }
    if let Some(stop) = warning_stop_tx {
        let _ = stop.send(());
    }
    if let Some(task) = warning_task {
        task.finish().await;
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
                failure_kind: refusal.kind,
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
    let error = format!(
        "the fetched read used the {} limit of {} before a browser could take over",
        stop.resource,
        fastbrowse_core::py::json::float_str(stop.limit)
    );
    result.end_as(Status::BudgetExceeded, stop.failure_kind(), error);
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
    if let Err(refusal) = check_browser_access(host.check_access.as_ref()).await {
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
            failure_kind: Some(refusal.failure_kind()),
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
    // A run limited to fetching ends where a step needs a browser, so its pages tell Jev none opens.
    let fetched_page = || {
        let page = fetch::FetchedPage::new(http.clone(), origins.clone(), host.check_access.clone());
        if task.fetch { page.without_browser() } else { page }
    };
    let fetched = Arc::new(fetched_page());
    let page: Arc<dyn Page> = fetched.clone();
    let search_page =
        search.map(|provider| Arc::new(fetch::search::SearchPage::new(page.clone(), fetched_page(), provider)));
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// The consent check, the extension's tab group and the result card each took the task as the caller wrote it,
    /// an address's credentials included, before the agent had masked anything (#1381).
    #[test]
    fn test_the_tab_group_and_the_result_card_show_the_task_with_its_credentials_masked() {
        let redactor: Arc<Mutex<Redactor>> = Arc::default();
        let trace = Trace::collecting().redacting(redactor.clone());
        let task = RunTask {
            task: "Read https://shopper:t4sk-pw@shop.test/orders".into(),
            context: Some("The CRM is https://ana:c0ntext-pw@crm.test/".into()),
            ..RunTask::default()
        };
        admit(&redactor, &task, &[]).unwrap();
        assert!(redactor.lock().unwrap().holds_credentials());
        let group = extension_address(Path::new("/hosts/a"), &task.task, &trace);
        assert_eq!(
            group.split_once('#').map(|(_, name)| name),
            Some("Read https://[secret:address_credentials]@shop.test/orders")
        );
        // Learned, a credential is masked where the text names it outside an address too.
        let context = task.context.as_deref().unwrap_or_default();
        for shown in
            [group, redacted(&trace, &task.task), redacted(&trace, context), redacted(&trace, "typed c0ntext-pw")]
        {
            assert!(
                !shown.contains("t4sk-pw") && !shown.contains("shopper") && !shown.contains("c0ntext-pw"),
                "{shown}"
            );
            assert!(!shown.contains("ana:"), "{shown}");
        }
        let clean = RunTask { task: "Read https://shop.test/orders".into(), ..RunTask::default() };
        let fresh: Arc<Mutex<Redactor>> = Arc::default();
        admit(&fresh, &clean, &[]).unwrap();
        assert!(!learn_credentials(&fresh, None, &Settings::default()));
        // A caller's JSON is decoded before anything reads it, so an `@` it escaped is an `@` here, and a context
        // that is itself JSON keeps its addresses masked within it.
        let decoded: RunTask = serde_json::from_str(
            r#"{"task": "Read it", "context": "{\"crm\": \"https://ana:J5on9w\u0040crm.test/\"}"}"#,
        )
        .unwrap();
        let json: Arc<Mutex<Redactor>> = Arc::default();
        admit(&json, &decoded, &[]).unwrap();
        let context = json.lock().unwrap().redact(decoded.context.as_deref().unwrap_or_default());
        assert_eq!(context, r#"{"crm": "https://[secret:address_credentials]@crm.test/"}"#);
        // A replayed recipe's start opens the run as a caller's start does.
        let replayed: Arc<Mutex<Redactor>> = Arc::default();
        let recipe = fastbrowse_core::recipe::Recipe {
            version: fastbrowse_core::recipe::RECIPE_VERSION,
            task: clean.task.clone(),
            start: Some("https://ops:r3cipe-pw@shop.test/".into()),
            steps: Vec::new(),
        };
        assert!(learn_credentials(&replayed, Some(&recipe), &Settings::default()));
        assert!(!replayed.lock().unwrap().redact("it said r3cipe-pw").contains("r3cipe-pw"));
        // So do the settings' base addresses, which a provider's error can repeat.
        let settings = Settings { llm_base_url: Some("https://ops:ba5e-pw@llm.test/v1".into()), ..Settings::default() };
        let based: Arc<Mutex<Redactor>> = Arc::default();
        assert!(learn_credentials(&based, None, &settings));
        assert!(!based.lock().unwrap().redact("401 for ba5e-pw").contains("ba5e-pw"));
    }

    /// A sample of `ty` with each string and each chosen key a distinct marker, `leaf-0`, `leaf-1` and so on.
    fn populated(ty: &fastbrowse_core::schema::Ty, next: &mut usize) -> Value {
        use fastbrowse_core::schema::Ty;
        let marker = |next: &mut usize| {
            *next += 1;
            Value::from(format!("leaf-{next}"))
        };
        match ty {
            Ty::Str | Ty::Path => marker(next),
            Ty::Any | Ty::JsonValue | Ty::Map(_) => {
                let key = marker(next).as_str().unwrap_or_default().to_string();
                let value = match ty {
                    Ty::Map(item) => populated(item, next),
                    _ => marker(next),
                };
                Value::Object(Map::from_iter([(key, value)]))
            }
            Ty::Int => Value::from(1),
            Ty::Float => Value::from(1.0),
            Ty::Bool => Value::from(true),
            Ty::Null => Value::Null,
            Ty::Base64 | Ty::Bytes | Ty::Image => Value::from(""),
            Ty::DateTime => Value::from("2026-01-01T00:00:00Z"),
            Ty::List(item) | Ty::Set(item) => Value::Array(vec![populated(item, next)]),
            Ty::Tuple(items) => Value::Array(items.iter().map(|item| populated(item, next)).collect()),
            Ty::Optional(inner) | Ty::Alias(_, inner) | Ty::Constrained(inner, _) => populated(inner, next),
            Ty::Literal(values) => values[0].clone(),
            Ty::Enum(closed) => closed.values[0].clone(),
            Ty::Union(variants) => populated(&variants[0], next),
            Ty::Tagged { variants, .. } => populated(&variants[0].1, next),
            Ty::Model(model) => Value::Object(
                model
                    .fields
                    .iter()
                    .filter(|field| !field.exclude)
                    .map(|field| (field.key().to_string(), populated(&field.ty, next)))
                    .collect(),
            ),
        }
    }

    /// Every string a caller writes into a run's arguments, a key or a value, is learnt or refused. The arguments
    /// are built from `RunTask`'s own schema, so a field added later is covered without being named: one field at a
    /// time was found unlearnt by review (a run id, a profile, a schema description, a path), each reaching a
    /// result, an error, an event or a model request raw (#1381).
    #[test]
    fn test_every_string_a_caller_writes_is_learnt_or_refused() {
        let mut next = 0;
        let fields = run_task_fields();
        let sample = Value::Object(
            fields.iter().map(|field| (field.key().to_string(), populated(&field.ty, &mut next))).collect(),
        );
        let written = sample.to_string();
        let mut refused = std::collections::BTreeSet::new();
        for leaf in 1..=next {
            let marker = format!("\"leaf-{leaf}\"");
            let field = fields
                .iter()
                .map(|field| field.key())
                .find(|key| sample[*key].to_string().contains(&marker))
                .expect("every marker is under a field");
            let password = format!("pw-{leaf}x");
            let task: RunTask =
                serde_json::from_str(&written.replace(&marker, &format!("\"https://ops:{password}@h.test/\"")))
                    .unwrap_or_else(|error| panic!("{field}: {error}"));
            // The preflight an entry point calls before `run_task` answers outside any redactor; given the field
            // alone, so it is not refused for another field's value first.
            let alone = serde_json::json!({"task": "Read it", field: serde_json::to_value(&task).unwrap()[field]});
            let alone: RunTask = serde_json::from_value(alone).unwrap_or_else(|error| panic!("{field}: {error}"));
            if let Err(error) = check(&alone, false) {
                assert!(!error.to_string().contains(&password), "{field}: {error}");
            }
            let redactor: Arc<Mutex<Redactor>> = Arc::default();
            match admit(&redactor, &task, &[]) {
                Ok(()) => {
                    assert!(CREDENTIAL_FIELDS.contains(&field), "{field} carried a credential and was admitted");
                    let redactor = redactor.lock().unwrap();
                    assert!(!redactor.redact(&format!("said {password}")).contains(&password), "{field} unlearnt");
                }
                Err(error) => {
                    assert!(!CREDENTIAL_FIELDS.contains(&field), "{field} is the caller's to write: {error}");
                    let error = error.to_string();
                    assert!(error.starts_with(&format!("{field} holds a credential")), "{error}");
                    assert!(!error.contains(&password), "{error}");
                    refused.insert(field);
                }
            }
        }
        // Nothing is refused for holding no credential, and every field with text was tried.
        let plain: RunTask = serde_json::from_str(&written).unwrap();
        admit(&Arc::default(), &plain, &[]).unwrap();
        for field in
            ["record", "output_schema", "chrome", "cloud_profile", "target_match", "desktop_apps", "attachments"]
        {
            assert!(refused.contains(field), "{field} was never tried");
        }
        // A credential written once in the task is refused where it repeats outside an address, as in a path.
        let repeated = RunTask {
            task: "Read https://ops:r3peat-pw@h.test/".into(),
            record: Some("/tmp/r3peat-pw.mp4".into()),
            ..RunTask::default()
        };
        let error = admit(&Arc::default(), &repeated, &[]).unwrap_err().to_string();
        assert!(error.starts_with("record holds a credential"), "{error}");
        // A setting read from the environment alone, which no caller argument names, is refused by a message that
        // does not repeat it, or held as a key nothing prints, or learnt with the rest.
        for (name, variable) in fastbrowse_clients::environment::FIELDS {
            match Settings::from_sources(&[(variable, "https://ops:env-pw@h.test/")], None) {
                Err(error) => assert!(!error.to_string().contains("env-pw"), "{name}: {error}"),
                Ok(settings) if settings.text().is_empty() => {}
                Ok(settings) => {
                    let redactor = Mutex::default();
                    assert!(learn_credentials(&redactor, None, &settings), "{name} unlearnt");
                    let said = redactor.lock().unwrap().redact("said env-pw");
                    assert!(!said.contains("env-pw"), "{name} unlearnt");
                }
            }
        }
        // A secret's name and origins are sent back to resolve it, so a credential in either is refused too.
        let clean = RunTask { task: "Sign in".into(), ..RunTask::default() };
        for secret in [
            SecretRef { name: "https://ops:s3cret-pw@h.test/".into(), origins: vec!["https://h.test".into()] },
            SecretRef { name: "password".into(), origins: vec!["https://ops:s3cret-pw@h.test".into()] },
        ] {
            let error = admit(&Arc::default(), &clean, &[secret]).unwrap_err().to_string();
            assert!(error.starts_with("a secret's name or origin holds a credential") && !error.contains("s3cret"));
        }
    }

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

    /// A fetched read that spent the browser run's allowance ended `budget_exceeded` still carrying the kind of the
    /// read's own stop, which named a browser as needed rather than the limit that ended the run.
    #[test]
    fn a_fetched_read_out_of_budget_names_the_limit_as_its_kind() {
        let read = serde_json::from_value(serde_json::json!({
            "status": "blocked", "answer": null, "data": null, "evidence": [], "steps": [], "cost": {"lines": []},
            "artifacts": [], "error": "a step needs a browser", "failure_kind": "NeedsBrowser",
        }))
        .unwrap();
        let result = out_of_budget(read, BudgetStop { resource: BudgetResource::Steps, limit: 3.0 });
        assert_eq!(result.status, Status::BudgetExceeded);
        assert_eq!(result.failure_kind.as_deref(), Some("Budget/steps"));
        assert_eq!(
            result.error.as_deref(),
            Some("the fetched read used the steps limit of 3.0 before a browser could take over")
        );
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

    fn pending_warning_task() -> (WarningTask, tokio::sync::oneshot::Receiver<()>, tokio::sync::oneshot::Receiver<()>) {
        struct NotifyDrop(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for NotifyDrop {
            fn drop(&mut self) {
                let _ = self.0.take().unwrap().send(());
            }
        }
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let _guard = NotifyDrop(Some(dropped_tx));
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        (WarningTask(Some(handle)), started_rx, dropped_rx)
    }

    #[tokio::test]
    async fn warning_task_drop_aborts_callback_worker() {
        let (task, started, dropped) = pending_warning_task();
        started.await.unwrap();
        drop(task);
        tokio::time::timeout(std::time::Duration::from_secs(1), dropped).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn warning_task_finish_drop_aborts_waiting_callback() {
        let (task, started, dropped) = pending_warning_task();
        started.await.unwrap();
        let mut finish = Box::pin(task.finish());
        assert!(std::future::poll_fn(|cx| std::task::Poll::Ready(finish.as_mut().poll(cx))).await.is_pending());
        drop(finish);
        tokio::time::timeout(std::time::Duration::from_secs(1), dropped).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn warning_task_shutdown_drains_queued_events_with_sender_still_alive() {
        let delivered = Arc::new(std::sync::Mutex::new(Vec::new()));
        let received = delivered.clone();
        let handler: EventHandler = Arc::new(move |event| {
            let received = received.clone();
            Box::pin(async move {
                received.lock().unwrap().push(event);
            })
        });
        let (task, sender, stop) = WarningTask::start(handler);
        let warning = BrowserWarningEvent {
            kind: Default::default(),
            operation: fastbrowse_core::models::BrowserWarningOperation::ScreencastStart,
            failure: fastbrowse_core::models::BrowserWarningFailure::ContextLost,
        };
        sender.send(warning.clone()).unwrap();
        sender.send(warning).unwrap();
        stop.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), task.finish()).await.unwrap();
        assert_eq!(delivered.lock().unwrap().len(), 2);
        assert!(sender.is_closed());
    }
}
