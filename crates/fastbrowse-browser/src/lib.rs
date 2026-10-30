//! fastbrowse's browser layer in Rust: `crates/fastbrowse-browser/js/` and `src/fastbrowse/adapters/`, ported.
//!
//! CDP goes over a raw websocket ([ADR 0003](../../../docs/adr/0003-cdp-over-raw-websocket.md)): one reader task
//! routes replies by id and events by method and session. `session` is `browser/session.py`'s `BrowserSession`,
//! `page` is `browser/page.py`'s `CdpPage`, and the page scripts (`snapshot.js`, `capture.js`, autoconsent) are
//! the Python package's own files, included unchanged.

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

/// A warning for whoever runs this, on stderr. Never pass an error's message here, only its kind: transport errors
/// can carry credentials or page text.
pub fn warn(message: &str) {
    eprintln!("fastbrowse: {message}");
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_warning_reaches_stderr_with_nothing_configured() {
        if std::env::var_os("FASTBROWSE_TEST_WARN").is_some() {
            super::warn("left running");
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::a_warning_reaches_stderr_with_nothing_configured", "--nocapture"])
            .env("FASTBROWSE_TEST_WARN", "1")
            .env_remove("FASTBROWSE_LOG")
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&output.stderr).contains("fastbrowse: left running"));
    }
}
