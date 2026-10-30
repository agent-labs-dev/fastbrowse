//! fastbrowse's browser layer: the CDP connection, the session and page on it, and the adapters that supply a browser.
//!
//! CDP goes over a raw websocket ([ADR 0003](../../../docs/adr/0003-cdp-over-raw-websocket.md)): one reader task
//! routes replies by id and events by method and session. `session` holds `BrowserSession`, `page` holds
//! `CdpPage`, and the page scripts (`snapshot.js`, `capture.js`, autoconsent) live in `js/`.

pub mod adapters;
pub mod cdp;
#[cfg(feature = "cursor")]
pub mod cursor;
pub mod error;
pub mod page;
#[cfg(feature = "recording")]
pub mod recording;
pub mod session;
#[doc(hidden)]
pub mod testing;
pub mod urlsplit;

/// A warning: written to stderr when `FASTBROWSE_LOG` is set, else dropped. Never
/// pass an error's message here, only its kind: transport errors can carry credentials or page text.
pub(crate) fn warn(message: &str) {
    if std::env::var_os("FASTBROWSE_LOG").is_some() {
        eprintln!("fastbrowse: {message}");
    }
}
