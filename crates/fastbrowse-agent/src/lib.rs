//! The agent above fastbrowse-core: the run loop, Jev's per-step policy, reading, notes and verification.
//!
//! Every prompt, Jev request and structured-output schema built here is a pure function of its inputs, since a run is
//! replayed from recorded model traffic keyed on a hash of the request body.

#[macro_use]
mod macros;

pub mod agent;

pub mod batches;
pub mod error;
pub mod memory;
pub mod policy;
mod pyre;
pub mod retrieval;
pub mod scripted;
pub mod start;
mod support;
pub mod verification;

pub use error::AgentError;
