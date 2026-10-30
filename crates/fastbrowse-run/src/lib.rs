//! Port of `src/fastbrowse/run.py`: run one task end to end. Assemble a browser, the model clients and the agent,
//! then return the result.
//!
//! This is the embedding entry point: the terminal and the live eval both call it, so the assembly has one
//! definition. An embedder gets the parts that are easy to forget: the cloud browser's own cost folded into the
//! result, downloads kept when a directory is given, and owned tabs closed on every path out.

pub mod fetch;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use fastbrowse_agent::agent::{Agent, HeadStart, RunOptions};
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
use fastbrowse_core::BoxFuture;
use fastbrowse_core::artifacts::DirectorySink;
use fastbrowse_core::config::Config;
use fastbrowse_core::decision::DecisionClient;
use fastbrowse_core::llm::LLMClient;
use fastbrowse_core::models::{
    ArtifactSink, Attachment, Authorization, BrowserConnection, BrowserEvent, BudgetResource, BudgetStop,
    CostBreakdown, CostLine, Event, EventHandler, FrameHandler, Limits, LocalChrome, RunResult, SecretResolver, Status,
    UntilCheck, WallKind,
};
use fastbrowse_core::origins::parse_origins;
use fastbrowse_core::page::Page;
use fastbrowse_core::telemetry::LedgerState;
use indexmap::IndexMap;
use serde_json::{Map, Value};

/// Why a run could not even be assembled: the arguments contradict each other, or the clients cannot be built.
#[derive(Debug, Clone, PartialEq)]
pub enum RunError {
    /// Python's `ValueError` from `run_task`'s own checks.
    Invalid(String),
    Configuration(String),
    /// Python's `RecordingError`: `record` could not start, so the run stopped with its browser closed.
    Recording(String),
    /// [`RunTask::cancel`] resolved; the browser was closed before this returned.
    Cancelled,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Invalid(message) | RunError::Configuration(message) | RunError::Recording(message) => {
                f.write_str(message)
            }
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
    pub jev: Option<Arc<dyn DecisionClient>>,
    pub llm: Option<Arc<dyn LLMClient>>,
    pub output_schema: Option<Arc<RecordSchema>>,
    /// What the caller knows about the task: background for the planner, recovery, the composer and Jev, never a
    /// requirement.
    pub context: Option<String>,
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
    /// Read every page over HTTP and start no browser; see [`fetch`].
    pub fetch: bool,
    /// Read over HTTP first, and start the browser only if the fetched run ends needing one: a step that would act, or
    /// a page the static read cannot see. The browser run then begins again from the task, inside what is left of
    /// `limits`, and its result carries what the fetched attempt spent. Its steps are the browser run's: the attempt's
    /// own reached `on_event` as they happened, and the trace marks the hand-over with an `escalate` event.
    pub fetch_first: bool,
    /// Makes a future that resolves when the caller stops the run. Python cancels the task and its `finally` closes
    /// the browser; here the run stops where it is, closes what it opened, then returns [`RunError::Cancelled`].
    pub cancel: Option<Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>>,
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
            context: None,
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
            fetch: false,
            fetch_first: false,
            cancel: None,
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
    if options.fetch && options.fetch_first {
        return Err(RunError::Invalid(
            "fetch reads with no browser at all and fetch_first may start one; pass one".into(),
        ));
    }
    if options.fetch_first {
        // A recording or an attached window has to exist from the first step, and the fetched attempt has neither.
        for (flag, given) in [
            ("record", options.record.is_some()),
            ("attach", options.attach),
            ("target_match", options.target_match.is_some()),
        ] {
            if given {
                return Err(RunError::Invalid(format!(
                    "fetch_first may finish with no browser, so it cannot be combined with {flag}"
                )));
            }
        }
    }
    if options.fetch {
        // None of these names anything a browserless run has, and accepting one would report a browser that was never
        // started, a file that was never written or a cursor nobody saw.
        for (flag, given) in [
            ("browser_api_key", options.browser_api_key.is_some()),
            ("chrome", options.chrome.is_some()),
            ("cloud_profile", options.cloud_profile.is_some()),
            ("cdp_url", options.cdp_url.is_some()),
            ("cdp_port", options.cdp_port.is_some()),
            ("target_match", options.target_match.is_some()),
            ("downloads", options.downloads.is_some()),
            ("record", options.record.is_some()),
        ] {
            if given {
                return Err(RunError::Invalid(format!(
                    "fetch mode starts no browser, so it cannot be combined with {flag}"
                )));
            }
        }
        if options.attach || options.cursor {
            return Err(RunError::Invalid(
                "fetch mode starts no browser, so it cannot be combined with attach or cursor".into(),
            ));
        }
    }
    let http = options.http.take().unwrap_or_else(Http::hyper);
    // The providers this run calls, for an eval to keep beside its result. A caller's own clients are not described.
    if options.jev.is_none() && options.llm.is_none() {
        http.trace.event("route", || Map::from_iter([("providers".into(), Value::from(settings.providers()))]));
    }
    let jev: Arc<dyn DecisionClient> = match options.jev.take() {
        Some(jev) => jev,
        None => Arc::from(settings.decision(&http)?),
    };
    let profile = jev.profile();
    if !profile.validated() {
        // No thresholds were measured on this model, so the run starts from Jev's: an eval reads its result with that.
        http.trace.event("decision_unvalidated", Map::new);
    }
    let config = profile.fit(options.config.clone());
    let llm: Arc<dyn LLMClient> = match options.llm.take() {
        Some(llm) => llm,
        None => Arc::from(settings.llm(&http)?),
    };
    let mut fetch_spent: Vec<CostLine> = Vec::new();
    if options.fetch || options.fetch_first {
        let began = std::time::Instant::now();
        let read = fetched(task, &options, &config, http.clone(), jev.clone(), llm.clone(), origins.clone()).await?;
        if options.fetch || !read.needs_browser {
            return Ok(read.result);
        }
        let limits = match remaining(options.limits.as_ref(), &read.spent, began.elapsed().as_secs_f64()) {
            Ok(limits) => limits,
            // The attempt spent what a browser run would have needed, so the run ends on that limit, not the page.
            Err(stop) => return Ok(out_of_budget(read.result, stop)),
        };
        http.trace.event("escalate", || {
            Map::from_iter([("reason".into(), Value::from(read.result.error.clone().unwrap_or_default()))])
        });
        options.limits = limits;
        fetch_spent = read.result.cost.lines;
    }
    let chrome = options.chrome.clone().unwrap_or_else(|| settings.local_chrome());
    let mut browser_cost: Vec<CostLine> = Vec::new();
    // Begun before the browser, which takes 2 to 3 seconds to start and open a tab, so the shortcut is ready to open
    // when the tab is.
    let head = HeadStart::begin(
        llm.clone(),
        task,
        options.start.as_deref(),
        options.context.as_deref(),
        options.limits.clone(),
    );
    let head_stop = head.stopper();
    let scratch = Scratch::new().map_err(|error| RunError::Invalid(error.to_string()))?;
    let mut session: Option<BrowserSession> = None;
    let mut owned: Option<Owned> = None;
    let mut head = Some(head);
    let cancel = options.cancel.take();
    // Kept outside the run so each is closed however the run ends, before the browser is.
    let recording: Filming = Arc::default();
    let mut recording_failed: Option<String> = None;
    let mut feedback: Option<Arc<CursorFeedback>> = None;
    let work = async {
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
                max_download_bytes: config.max_download_bytes as usize,
                refuse_cookie_banners: config.refuse_cookie_banners,
                on_frame: options.on_frame.clone(),
                check_access: options.check_access.clone(),
            },
        );
        session = Some(opened.clone());
        opened.open().await?;
        // A cloud or headless browser has no window on this machine to draw over.
        let shown = options.cursor
            && options.browser_api_key.is_none()
            && is_loopback(&opened.connection().cdp_url)
            && (options.cdp_url.is_some() || options.cdp_port.is_some() || chrome.headed);
        feedback = cursor_feedback(shown).await.map(Arc::new);
        let page: Arc<dyn Page> = Arc::new(CdpPage::new(opened.clone(), config.clone()).with_cursor(feedback.clone()));
        if let Some(path) = &options.record {
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
            .with_secrets(options.secrets.clone())
            .with_events(captioned(options.on_event.clone(), &recording));
        let run = RunOptions {
            start: options.start.clone(),
            // With no page named, the first address is worked out from the task, unless attaching to an existing
            // window where the page is preserved.
            choose_start: options.start.is_none() && !attach,
            context: options.context.clone(),
            inputs: options.inputs.clone(),
            attachments: options.attachments.clone(),
            output_schema: options.output_schema.clone(),
            limits: None,
            authorization: options.authorization.clone(),
            until: options.until.clone(),
            head_start: head.take(),
        };
        let result = agent.run(task, run).await.map_err(|error| refused(&error.to_string()))?;
        // The card is a data: document navigated into the tab, which would replace the caller's own app page; an
        // attached window is theirs and stays where it is.
        // Shown while the recording stays in its slot, so a run cancelled during the card still finishes the videos.
        if !attach && let Some(filming) = recording.lock().await.as_mut() {
            let _ = filming.show_result(task, &card(&result)).await;
        }
        Ok(result)
    };
    let outcome: Option<Result<RunResult, Refused>> = match cancel {
        None => Some(work.await),
        Some(cancel) => tokio::select! {
            biased;
            () = cancel() => None,
            done = work => Some(done),
        },
    };
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
        None => {
            // The agent may hold the head start by now, and was dropped with it still running.
            head_stop.discard().await;
            return Err(RunError::Cancelled);
        }
        Some(Ok(result)) => result,
        Some(Err(refusal)) => {
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
                recordings: Vec::new(),
                wall: None,
            }
        }
    };
    // A run that never began leaves its head start running; one that did has settled it already.
    if let Some(head) = &head {
        head.discard().await;
    }
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
}

/// `task` read over HTTP, with no browser started at all.
///
/// A step that would act, download or look at pixels ends the run `blocked`: the document was never the problem, and a
/// browser would answer a different task.
async fn fetched(
    task: &str,
    options: &RunTask,
    config: &Config,
    http: Http,
    jev: Arc<dyn DecisionClient>,
    llm: Arc<dyn LLMClient>,
    origins: Option<Vec<String>>,
) -> Result<Read, RunError> {
    if check_browser_access(options.check_access.as_ref()).await.is_err() {
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
        return Ok(Read { result, needs_browser: false, spent: LedgerState::default() });
    }
    let head = HeadStart::begin(
        llm.clone(),
        task,
        options.start.as_deref(),
        options.context.as_deref(),
        options.limits.clone(),
    );
    let head_stop = head.stopper();
    let ledger = head.ledger.clone();
    let fetched = Arc::new(fetch::FetchedPage::new(http, origins, options.check_access.clone()));
    let page: Arc<dyn Page> = fetched.clone();
    let agent = Agent::new(page, jev, llm)
        .with_config(config.clone())
        .with_secrets(options.secrets.clone())
        .with_events(options.on_event.clone());
    let run = RunOptions {
        start: options.start.clone(),
        // With no page named, the first address is worked out from the task, the same way a browser run with no
        // `start` begins.
        choose_start: options.start.is_none(),
        context: options.context.clone(),
        inputs: options.inputs.clone(),
        attachments: options.attachments.clone(),
        output_schema: options.output_schema.clone(),
        limits: None,
        authorization: options.authorization.clone(),
        until: options.until.clone(),
        head_start: Some(head),
    };
    let work = agent.run(task, run);
    // Cloned, not taken: a run that escalates watches the same cancel while its browser works.
    let outcome = match options.cancel.clone() {
        None => work.await,
        Some(cancel) => tokio::select! {
            biased;
            () = cancel() => {
                head_stop.discard().await;
                return Err(RunError::Cancelled);
            }
            done = work => done,
        },
    };
    let result = outcome.map_err(|error| RunError::Invalid(error.to_string()))?;
    // Read from the page rather than the message: a refusal the page itself raised means the task needs a browser. It
    // ends the run blocked, or unverified when the refusal was the screenshot an ending frame asked for. A bot check Jev
    // saw in the document is the other: its script clears it only in a browser.
    let bot_check = result.wall.as_ref().is_some_and(|wall| wall.kind == WallKind::BotCheck);
    let needs_browser =
        matches!(result.status, Status::Blocked | Status::Unverified) && (fetched.needed_browser() || bot_check);
    let spent = ledger.state().clone();
    Ok(Read { result, needs_browser, spent })
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
