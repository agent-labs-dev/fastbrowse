//! Requests a decision sends after the page redrew under it.
//!
//! A decision the page redrew under is discarded, but a request it already sent is billed, and one dropped before its
//! response has no cost the run can know (#1218). So the decision runs on to finish what it sent, and from the redraw
//! on, the run's clients send nothing new for it, and the ledger counts no call for a request that is never sent.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

tokio::task_local! {
    static SUPERSEDED: Arc<AtomicBool>;
}

/// Run `work` so that [`supersede`]ing `flag` stops it sending new requests. The agent spawns no tasks, so every
/// request `work` makes is polled inside this scope.
pub async fn superseding<F: Future>(flag: Arc<AtomicBool>, work: F) -> F::Output {
    SUPERSEDED.scope(flag, work).await
}

pub fn supersede(flag: &AtomicBool) {
    flag.store(true, Ordering::SeqCst);
}

/// Whether the work polling this was superseded: a request it would send now is refused unsent.
pub fn superseded() -> bool {
    SUPERSEDED.try_with(|flag| flag.load(Ordering::SeqCst)).unwrap_or(false)
}
