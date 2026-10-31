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
            .arg("tests::a_warning_reaches_stderr_with_nothing_configured")
            .env("RUST_TEST_NOCAPTURE", "1")
            .env("FASTBROWSE_TEST_WARN", "1")
            .env_remove("FASTBROWSE_LOG")
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&output.stderr).contains("fastbrowse: left running"));
    }
}
