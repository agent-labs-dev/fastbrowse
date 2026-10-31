//! A browser agent that picks instead of generating. Every page is indexed into the controls it has, a choice model
//! picks one of them and decides when the run is done, and code owns authorization, secrets and cost limits.
//!
//! [`run_task`] runs one task end to end and returns a [`RunResult`]: a status, an answer with citations, the
//! structured data asked for, the steps and what they cost. Model keys are read from the environment, as the
//! `fastbrowse` command reads them. The run starts local Chrome, or a Browser Use Cloud browser when
//! [`Host::browser_api_key`] is given.
//!
//! ```no_run
//! use fastbrowse::{Host, Limits, RunTask, run_task};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), fastbrowse::RunError> {
//!     let task = RunTask {
//!         task: "What is the top story's title?".into(),
//!         start: Some("https://news.ycombinator.com/".into()),
//!         limits: Some(Limits { max_dollars: Some(0.25), ..Limits::default() }),
//!         ..RunTask::default()
//!     };
//!     let result = run_task(task, Host::default()).await?;
//!     println!("{:?}: {}", result.status, result.answer.unwrap_or_default());
//!     Ok(())
//! }
//! ```
//!
//! The crates beneath this one (`fastbrowse-core`, `-run` and the rest) are published so this one can be, and carry
//! no promise of their own: what is meant to be used is re-exported here.

pub use fastbrowse_core::config::Config;
pub use fastbrowse_core::models::{
    Artifact, Attachment, Authorization, BrowserEvent, Citation, CostBreakdown, CostLine, Event, Evidence, Limits,
    LocalChrome, RunResult, SecretRef, SecretResolver, Status, StepEvent, StepResult, Wall,
};
pub use fastbrowse_run::{ConnectCdp, Connected, Host, RunError, RunTask, connect_cdp, run_task};
