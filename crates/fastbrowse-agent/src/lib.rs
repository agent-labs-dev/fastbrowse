//! The agent above fastbrowse-core: the run loop, the driver that picks actions, reading, notes and verification.
//!
//! Every prompt, Jev request and structured-output schema built here is a pure function of its inputs, since a run is
//! replayed from recorded model traffic keyed on a hash of the request body.

pub mod agent;

pub mod batches;
pub mod dates;
pub mod error;
pub mod memory;
pub(crate) mod policy;
mod pyre;
pub mod retrieval;
pub mod scripted;
pub mod start;
mod support;
pub mod verification;

pub use error::AgentError;
