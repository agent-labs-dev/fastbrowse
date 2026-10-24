//! The call every entry point ends in: `run_task(...)` from `src/fastbrowse/run.py`, as a trait.
//!
//! The agent loop and `run_task` are ported in `fastbrowse-agent`. The entry points code against [`Runner`], so the
//! real one goes in with [`default_runner`] and the tests hand in stand-ins, as Python's tests pass `runner=`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fastbrowse_core::BoxFuture;
use fastbrowse_core::jev::JevClient;
use fastbrowse_core::llm::LLMClient;
use fastbrowse_core::models::{
    Attachment, Authorization, EventHandler, FrameHandler, Limits, LocalChrome, RunResult, SecretResolver, UntilCheck,
};
use fastbrowse_core::output_schema::OutputModel;
use indexmap::IndexMap;
use tokio::sync::Notify;

/// `run_task`'s keyword arguments, the ones an entry point passes. Each defaults as Python's does.
#[derive(Clone)]
pub struct RunArgs {
    pub task: String,
    pub start: Option<String>,
    pub browser_api_key: Option<String>,
    pub chrome: Option<LocalChrome>,
    pub cloud_profile: Option<String>,
    pub cdp_url: Option<String>,
    pub cdp_port: Option<i64>,
    pub attach: bool,
    pub target_match: Option<String>,
    pub foreground: bool,
    pub proxy_country: Option<String>,
    pub viewport: Option<(i64, i64)>,
    pub cloud_allow_resizing: bool,
    pub output_schema: Option<OutputModel>,
    pub inputs: Option<IndexMap<String, String>>,
    pub attachments: Vec<Attachment>,
    pub limits: Option<Limits>,
    pub authorization: Option<Authorization>,
    pub secrets: Option<Arc<dyn SecretResolver>>,
    pub downloads: Option<PathBuf>,
    pub on_event: Option<EventHandler>,
    pub on_frame: Option<FrameHandler>,
    pub until: Option<UntilCheck>,
    pub record: Option<PathBuf>,
    pub cursor: bool,
    /// Tripped when the caller stops the run. Python cancels the task; a Rust run watches this instead, closes its
    /// browser and returns [`RunError::Cancelled`].
    pub cancel: Cancel,
}

impl RunArgs {
    pub fn new(task: impl Into<String>) -> RunArgs {
        RunArgs {
            task: task.into(),
            start: None,
            browser_api_key: None,
            chrome: None,
            cloud_profile: None,
            cdp_url: None,
            cdp_port: None,
            attach: false,
            target_match: None,
            foreground: false,
            proxy_country: Some("us".into()),
            viewport: None,
            cloud_allow_resizing: false,
            output_schema: None,
            inputs: None,
            attachments: Vec::new(),
            limits: None,
            authorization: None,
            secrets: None,
            downloads: None,
            on_event: None,
            on_frame: None,
            until: None,
            record: None,
            cursor: false,
            cancel: Cancel::default(),
        }
    }
}

/// How a run ended without a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// `ConfigurationError`: the setup is wrong, and no browser opened.
    Configuration(String),
    /// The run saw [`Cancel`] tripped and has closed its browser.
    Cancelled,
    /// Any other exception, as `"{type(exc).__name__}: {exc}"`.
    Failed(String),
}

/// `run_task`, or what stands in for it.
pub trait Runner: Send + Sync {
    fn run(&self, args: RunArgs) -> BoxFuture<'static, Result<RunResult, RunError>>;
}

impl<F> Runner for F
where
    F: Fn(RunArgs) -> BoxFuture<'static, Result<RunResult, RunError>> + Send + Sync,
{
    fn run(&self, args: RunArgs) -> BoxFuture<'static, Result<RunResult, RunError>> {
        self(args)
    }
}

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

    /// `work`, unless the run is stopped first. A run awaits its steps through this, then closes what it opened.
    pub async fn guard<T>(&self, work: impl std::future::Future<Output = T>) -> Result<T, RunError> {
        tokio::select! {
            biased;
            () = self.tripped() => Err(RunError::Cancelled),
            done = work => Ok(done),
        }
    }
}

/// The `run_task` the binary runs when nothing stands in for it: `fastbrowse_run::run_task`.
pub fn default_runner() -> Arc<dyn Runner> {
    Arc::new(|args: RunArgs| -> BoxFuture<'static, Result<RunResult, RunError>> { Box::pin(run_task(args)) })
}

async fn run_task(args: RunArgs) -> Result<RunResult, RunError> {
    run_task_with(args, None, None).await
}

/// `run_task` with model clients of the caller's own in place of those the settings build, as Python's `run_task`
/// takes `jev=` and `llm=`.
pub async fn run_task_with(
    args: RunArgs,
    jev: Option<Arc<dyn JevClient>>,
    llm: Option<Arc<dyn LLMClient>>,
) -> Result<RunResult, RunError> {
    let options = run_options(&args)?;
    let cancel = args.cancel.clone();
    let cancelled = move || -> BoxFuture<'static, ()> {
        let cancel = cancel.clone();
        Box::pin(async move { cancel.tripped().await })
    };
    let options = fastbrowse_run::RunTask { cancel: Some(Arc::new(cancelled)), jev, llm, ..options };
    fastbrowse_run::run_task(&args.task, options).await.map_err(|error| match error {
        fastbrowse_run::RunError::Invalid(message) => RunError::Failed(format!("ValueError: {message}")),
        fastbrowse_run::RunError::Configuration(message) => RunError::Configuration(message),
        fastbrowse_run::RunError::Cancelled => RunError::Cancelled,
    })
}

/// `RunArgs` as `fastbrowse_run::RunTask`. Numbers that fit no port or window size fail as Python's `ValueError`.
fn run_options(args: &RunArgs) -> Result<fastbrowse_run::RunTask, RunError> {
    let invalid = |what: &str| RunError::Failed(format!("ValueError: {what} is out of range"));
    let cdp_port = args.cdp_port.map(|port| u16::try_from(port).map_err(|_| invalid("cdp_port"))).transpose()?;
    let viewport = args
        .viewport
        .map(|(width, height)| match (u32::try_from(width), u32::try_from(height)) {
            (Ok(width), Ok(height)) => Ok((width, height)),
            _ => Err(invalid("viewport")),
        })
        .transpose()?;
    Ok(fastbrowse_run::RunTask {
        start: args.start.clone(),
        browser_api_key: args.browser_api_key.clone(),
        chrome: args.chrome.clone(),
        cloud_profile: args.cloud_profile.clone(),
        cdp_url: args.cdp_url.clone(),
        cdp_port,
        attach: args.attach,
        target_match: args.target_match.clone(),
        foreground: args.foreground,
        proxy_country: args.proxy_country.clone(),
        viewport,
        cloud_allow_resizing: args.cloud_allow_resizing,
        output_schema: args
            .output_schema
            .clone()
            .map(|model| Arc::new(fastbrowse_agent::verification::RecordSchema::from_output(model))),
        inputs: args.inputs.clone().unwrap_or_default(),
        attachments: args.attachments.clone(),
        limits: args.limits.clone(),
        authorization: args.authorization.clone(),
        secrets: args.secrets.clone(),
        downloads: args.downloads.clone(),
        on_event: args.on_event.clone(),
        on_frame: args.on_frame.clone(),
        until: args.until.clone(),
        record: args.record.clone(),
        cursor: args.cursor,
        ..fastbrowse_run::RunTask::default()
    })
}
