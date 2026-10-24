//! The call every entry point ends in: `run_task(...)` from `src/fastbrowse/run.py`, as a trait.
//!
//! The agent loop and `run_task` are ported in `fastbrowse-agent`. The entry points code against [`Runner`], so the
//! real one goes in with [`default_runner`] and the tests hand in stand-ins, as Python's tests pass `runner=`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fastbrowse_core::BoxFuture;
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

/// The `run_task` the binary runs when nothing stands in for it.
///
/// The agent loop is being ported in `fastbrowse-agent`; until its `run_task` lands, a run is refused as a
/// configuration error before any browser work, so the protocol and the CLI behave as for a misconfigured agent.
pub fn default_runner() -> Arc<dyn Runner> {
    Arc::new(|_: RunArgs| -> BoxFuture<'static, Result<RunResult, RunError>> {
        Box::pin(async { Err(RunError::Configuration("this fastbrowse build has no agent to run the task".into())) })
    })
}
