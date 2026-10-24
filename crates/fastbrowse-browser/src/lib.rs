//! fastbrowse's browser layer in Rust: `src/fastbrowse/browser/` and `src/fastbrowse/adapters/`, ported.
//!
//! CDP goes over a raw websocket ([ADR 0003](../../../docs/adr/0003-cdp-over-raw-websocket.md)): one reader task
//! routes replies by id and events by method and session. `session` is `browser/session.py`'s `BrowserSession`,
//! `page` is `browser/page.py`'s `CdpPage`, and the page scripts (`snapshot.js`, `capture.js`, autoconsent) are
//! the Python package's own files, included unchanged.

pub mod adapters;
pub mod cdp;
#[cfg(feature = "cursor")]
pub mod cursor;
pub mod page;
#[cfg(feature = "recording")]
pub mod recording;
pub mod session;
pub mod shared;

/// A warning Python logs through `logging`: written to stderr when `FASTBROWSE_LOG` is set, else dropped. Never
/// pass an error's message here, only its kind: transport errors can carry credentials or page text.
pub(crate) fn warn(message: &str) {
    if std::env::var_os("FASTBROWSE_LOG").is_some() {
        eprintln!("fastbrowse: {message}");
    }
}
