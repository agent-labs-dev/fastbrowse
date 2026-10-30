//! fastbrowse's types: the models, the protocol, configuration, origins and citations.
//!
//! URLs follow the WHATWG URL Standard (the `url` crate, UTS #46 hosts), as Chrome parses them, rather than
//! CPython's `urlsplit`.
//!
//! Anything that crosses a seam serializes to the bytes Python writes: [`py`] holds what pydantic and the standard
//! library do implicitly, and [`schema`] writes JSON Schema as pydantic does.

#[macro_use]
mod macros;

pub mod addresses;
pub mod artifacts;
pub mod citations;
pub mod config;
pub mod decision;
pub mod effects;
pub mod llm;
pub mod models;
pub mod navigation;
pub mod origins;
pub mod output_schema;
pub mod page;
pub mod protocol;
pub mod py;
pub mod safety;
pub mod schema;
pub mod telemetry;
pub mod validation;

use std::future::Future;
use std::pin::Pin;

/// An owned future a trait method returns, so the client traits stay usable as `dyn` objects (a failover holds two
/// providers behind one trait). Native `async fn` in a trait cannot be called through `dyn`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[cfg(test)]
pub(crate) mod testing {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    /// Runs a future that never waits on I/O; the tests' futures resolve on their first polls.
    pub fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return output;
            }
        }
    }
}
