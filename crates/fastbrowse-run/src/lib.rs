//! Port of `src/fastbrowse/run.py`: run one task end to end. Assemble a browser, the model clients and the agent,
//! then return the result.
//!
//! This is the embedding entry point: the terminal and the live eval both call it, so the assembly has one
//! definition. An embedder gets the parts that are easy to forget: the cloud browser's own cost folded into the
//! result, downloads kept when a directory is given, and owned tabs closed on every path out.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use fastbrowse_agent::agent::{Agent, HeadStart, RunOptions};
use fastbrowse_agent::verification::RecordSchema;
use fastbrowse_browser::adapters::browser_use_cloud::{BrowserUseCloudBrowser, BrowserUseCloudOptions};
use fastbrowse_browser::adapters::local_chrome::LocalChromeProcess;
use fastbrowse_browser::error::BrowserError;
use fastbrowse_browser::page::CdpPage;
use fastbrowse_browser::session::{BrowserSession, CheckAccess, SessionOptions, check_browser_access};
use fastbrowse_clients::environment::{ConfigurationError, Settings};
use fastbrowse_clients::http::Http;
use fastbrowse_core::artifacts::DirectorySink;
use fastbrowse_core::config::Config;
use fastbrowse_core::jev::JevClient;
use fastbrowse_core::llm::LLMClient;
use fastbrowse_core::models::{
    ArtifactSink, Attachment, Authorization, BrowserConnection, BrowserEvent, CostBreakdown, CostLine, Event,
    EventHandler, FrameHandler, Limits, LocalChrome, RunResult, SecretResolver, Status, UntilCheck,
};
use fastbrowse_core::origins::parse_origins;
use fastbrowse_core::page::Page;
use indexmap::IndexMap;
use serde_json::Value;

/// Why a run could not even be assembled: the arguments contradict each other, or the clients cannot be built.
#[derive(Debug, Clone, PartialEq)]
pub enum RunError {
    /// Python's `ValueError` from `run_task`'s own checks.
    Invalid(String),
    Configuration(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Invalid(message) | RunError::Configuration(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RunError {}

impl From<ConfigurationError> for RunError {
    fn from(error: ConfigurationError) -> Self {
        RunError::Configuration(error.to_string())
    }
}

/// `run_task`'s keyword arguments; see `run.py` for what each one means.
pub struct RunTask {
    pub start: Option<String>,
    pub browser_api_key: Option<String>,
    pub chrome: Option<LocalChrome>,
    pub cloud_profile: Option<String>,
    pub cdp_url: Option<String>,
    pub cdp_port: Option<u16>,
    pub attach: bool,
    pub target_match: Option<String>,
    pub foreground: bool,
    pub allowed_origins: Option<Vec<String>>,
    pub check_access: Option<CheckAccess>,
    pub proxy_country: Option<String>,
    pub viewport: Option<(u32, u32)>,
    pub cloud_allow_resizing: bool,
    pub cloud_extensions: Vec<String>,
    pub cloud_timeout_minutes: u32,
    pub jev: Option<Arc<dyn JevClient>>,
    pub llm: Option<Arc<dyn LLMClient>>,
    pub output_schema: Option<Arc<RecordSchema>>,
    pub inputs: IndexMap<String, String>,
    pub attachments: Vec<Attachment>,
    pub limits: Option<Limits>,
    pub authorization: Option<Authorization>,
    pub secrets: Option<Arc<dyn SecretResolver>>,
    pub downloads: Option<PathBuf>,
    pub on_event: Option<EventHandler>,
    pub on_frame: Option<FrameHandler>,
    pub until: Option<UntilCheck>,
    pub config: Option<Config>,
    pub settings: Option<Settings>,
    /// The HTTP client the model clients and DevTools discovery use; a caller's outlives the run.
    pub http: Option<Http>,
    pub record: Option<PathBuf>,
    pub cursor: bool,
}

impl Default for RunTask {
    fn default() -> Self {
        RunTask {
            start: None,
            browser_api_key: None,
            chrome: None,
            cloud_profile: None,
            cdp_url: None,
            cdp_port: None,
            attach: false,
            target_match: None,
            foreground: false,
            allowed_origins: None,
            check_access: None,
            proxy_country: Some("us".into()),
            viewport: None,
            cloud_allow_resizing: false,
            cloud_extensions: Vec::new(),
            cloud_timeout_minutes: 15,
            jev: None,
            llm: None,
            output_schema: None,
            inputs: IndexMap::new(),
            attachments: Vec::new(),
            limits: None,
            authorization: None,
            secrets: None,
            downloads: None,
            on_event: None,
            on_frame: None,
            until: None,
            config: None,
            settings: None,
            http: None,
            record: None,
            cursor: false,
        }
    }
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
            refuse_cookie_banners: config.refuse_cookie_banners,
            check_access: options.check_access,
            ..SessionOptions::default()
        },
    );
    session.open().await?;
    let page = CdpPage::new(session.clone(), config);
    Ok(Connected { session, page, _scratch: scratch })
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

/// The browser a run drives: one it is handed, a cloud browser, or local Chrome.
async fn browser(task: &RunTask, chrome: &LocalChrome, http: &Http) -> Result<(BrowserConnection, Owned), Refused> {
    let key = task.browser_api_key.as_deref();
    if !task.cloud_extensions.is_empty() && (key.is_none() || task.cdp_url.is_some() || task.cdp_port.is_some()) {
        return Err(refused(
            "cloud_extensions load into a Browser Use Cloud browser this run starts, which needs a key",
        ));
    }
    if task.cdp_url.is_some() && task.cdp_port.is_some() {
        return Err(refused("cdp_url and cdp_port both name a browser to attach to; pass one"));
    }
    let foreground = task.foreground || task.cursor;
    if task.cdp_url.is_some() || task.cdp_port.is_some() {
        if key.is_some() {
            return Err(refused("cdp_url/cdp_port is a browser to attach to; a cloud key would start a second one"));
        }
        if task.cloud_profile.is_some() {
            return Err(refused("cloud_profile belongs to a browser fastbrowse starts, not to one it is handed"));
        }
        let cdp_url = match (&task.cdp_url, task.cdp_port) {
            (Some(url), _) => url.clone(),
            (None, Some(port)) => resolve_cdp_port(port, "127.0.0.1", http).await?,
            (None, None) => unreachable!("one of them is set"),
        };
        // The caller's browser outlives the run. The session either opens its own tab and closes only that, or,
        // attaching, drives a window already open and leaves it open.
        let connection = BrowserConnection {
            attach: task.attach || task.target_match.is_some(),
            target_match: task.target_match.clone(),
            foreground,
            ..BrowserConnection::new(cdp_url, true)
        };
        return Ok((connection, Owned::Handed));
    }
    if task.attach || task.target_match.is_some() {
        return Err(refused("attach and target_match require cdp_url or cdp_port"));
    }
    let Some(key) = key else {
        if task.cloud_profile.is_some() {
            return Err(refused("cloud_profile names a Browser Use Cloud profile, which needs a cloud browser"));
        }
        let (process, connection) =
            LocalChromeProcess::launch(chrome).await.map_err(|error| refused(&error.to_string()))?;
        let connection = BrowserConnection { foreground: connection.foreground || foreground, ..connection };
        return Ok((connection, Owned::Local(Box::new(process))));
    };
    let options = BrowserUseCloudOptions {
        proxy_country: task.proxy_country.clone(),
        timeout_minutes: task.cloud_timeout_minutes,
        profile: task.cloud_profile.clone(),
        viewport: task.viewport,
        allow_resizing: task.cloud_allow_resizing,
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
            let _ = remote.stop().await;
            cost.extend(remote.cost());
        }
    }
}

/// Open `start`, pursue `task`, and return what the run could prove.
pub async fn run_task(task: &str, mut options: RunTask) -> Result<RunResult, RunError> {
    let config = options.config.clone().unwrap_or_default();
    let settings = match options.settings.take() {
        Some(settings) => settings,
        None => Settings::load()?,
    };
    let origins = match &options.allowed_origins {
        None => None,
        Some(origins) => Some(parse_origins(origins.clone()).map_err(|error| RunError::Invalid(error.to_string()))?),
    };
    if origins.is_some() && options.record.is_some() {
        return Err(RunError::Invalid(
            "record shows whatever the tab shows, so it cannot be combined with allowed_origins".into(),
        ));
    }
    if options.record.is_some() || options.cursor {
        // Recording and the cursor overlay are behind the browser crate's `recording` and `cursor` features, which
        // this crate does not build yet.
        return Err(RunError::Invalid("record and cursor are not built into this binary".into()));
    }
    let http = options.http.take().unwrap_or_else(Http::hyper);
    let jev: Arc<dyn JevClient> = match options.jev.take() {
        Some(jev) => jev,
        None => Arc::from(settings.jev(&http)?),
    };
    let llm: Arc<dyn LLMClient> = match options.llm.take() {
        Some(llm) => llm,
        None => Arc::from(settings.llm(&http)?),
    };
    let chrome = options.chrome.clone().unwrap_or_else(|| settings.local_chrome());
    let mut browser_cost: Vec<CostLine> = Vec::new();
    // Begun before the browser, which takes 2 to 3 seconds to start and open a tab, so the shortcut is ready to open
    // when the tab is.
    let head = HeadStart::begin(llm.clone(), task, options.start.as_deref(), options.limits.clone());
    let scratch = Scratch::new().map_err(|error| RunError::Invalid(error.to_string()))?;
    let mut session: Option<BrowserSession> = None;
    let mut owned: Option<Owned> = None;
    let mut head = Some(head);
    let outcome: Result<RunResult, Refused> = async {
        check_browser_access(options.check_access.as_ref()).await?;
        let (connection, started) = browser(&options, &chrome, &http).await?;
        owned = Some(started);
        let connection = BrowserConnection { allowed_origins: origins.clone(), ..connection };
        if let Some(on_event) = &options.on_event {
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
            sink(options.downloads.as_deref(), &scratch)?,
            SessionOptions {
                refuse_cookie_banners: config.refuse_cookie_banners,
                on_frame: options.on_frame.clone(),
                check_access: options.check_access.clone(),
                ..SessionOptions::default()
            },
        );
        session = Some(opened.clone());
        opened.open().await?;
        let page: Arc<dyn Page> = Arc::new(CdpPage::new(opened.clone(), config.clone()));
        let agent = Agent::new(page, jev.clone(), llm.clone())
            .with_config(config.clone())
            .with_secrets(options.secrets.clone())
            .with_events(options.on_event.clone());
        let run = RunOptions {
            start: options.start.clone(),
            // With no page named, the first address is worked out from the task, unless attaching to an existing
            // window where the page is preserved.
            choose_start: options.start.is_none() && !attach,
            inputs: options.inputs.clone(),
            attachments: options.attachments.clone(),
            output_schema: options.output_schema.clone(),
            limits: None,
            authorization: options.authorization.clone(),
            until: options.until.clone(),
            head_start: head.take(),
        };
        agent.run(task, run).await.map_err(|error| refused(&error.to_string()))
    }
    .await;
    if let Some(session) = &session {
        let _ = session.close().await;
    }
    if let Some(owned) = owned.take() {
        stop(owned, &mut browser_cost).await;
    }
    let mut result = match outcome {
        Ok(result) => result,
        Err(refusal) => {
            // A cloud browser that cannot be started is an outage, not a failed run. The plan and shortcut were asked
            // for before the browser failed, and what they spent is real.
            let lines = match &head {
                Some(head) => head.abandon().await,
                None => Vec::new(),
            };
            RunResult {
                status: if refusal.unavailable { Status::Unavailable } else { Status::Error },
                budget: None,
                answer: None,
                data: Value::Null,
                evidence: Vec::new(),
                steps: Vec::new(),
                cost: CostBreakdown { lines },
                artifacts: session.as_ref().map(BrowserSession::artifacts).unwrap_or_default(),
                citations: Vec::new(),
                error: Some(refusal.message),
                final_url: None,
                final_frame: None,
                would_fire: Vec::new(),
                recordings: Vec::new(),
            }
        }
    };
    // A run that never began leaves its head start running; one that did has discarded it already.
    if let Some(head) = &head {
        head.discard().await;
    }
    result.cost.lines.extend(browser_cost);
    Ok(result)
}
