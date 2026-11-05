//! The call every entry point ends in: [`fastbrowse_run::run_task`], as a trait.
//!
//! The entry points code against [`Runner`], so the real one goes in with [`default_runner`] and the tests hand in
//! stand-ins.

use std::sync::Arc;

use fastbrowse_core::BoxFuture;
use fastbrowse_core::models::RunResult;
pub use fastbrowse_run::{Cancel, Host, RunError, RunTask};

/// `run_task`, or what stands in for it.
pub trait Runner: Send + Sync {
    fn run(&self, task: RunTask, host: Host) -> BoxFuture<'static, Result<RunResult, RunError>>;
}

impl<F> Runner for F
where
    F: Fn(RunTask, Host) -> BoxFuture<'static, Result<RunResult, RunError>> + Send + Sync,
{
    fn run(&self, task: RunTask, host: Host) -> BoxFuture<'static, Result<RunResult, RunError>> {
        self(task, host)
    }
}

/// The runner the binary uses when nothing stands in for it.
pub fn default_runner() -> Arc<dyn Runner> {
    Arc::new(|task: RunTask, host: Host| -> BoxFuture<'static, Result<RunResult, RunError>> {
        Box::pin(fastbrowse_run::run_task(task, host))
    })
}
