//! The decision logic of `src/fastbrowse`, ported module by module above fastbrowse-core: one module per Python file.
//!
//! Every prompt, Jev request and structured-output schema built here is byte for byte the one Python builds for the
//! same inputs, since a run is proven by replaying recorded model traffic keyed on a hash of the request body.

#[macro_use]
mod macros;

pub mod batches;
pub mod comparison;
mod difflib;
pub mod error;
pub mod memory;
pub mod planner;
pub mod policy;
mod pyre;
pub mod retrieval;
pub mod scripted;
pub mod shortcut;
mod support;
pub mod tripwires;

pub use error::AgentError;
