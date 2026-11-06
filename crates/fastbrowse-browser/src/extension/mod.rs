//! The extension backend: a run drives the user's own Chrome through the fastbrowse extension, which relays CDP over
//! `chrome.debugger` ([ADR 0005](../../../../docs/adr/0005-extension-relays-cdp-through-chrome-debugger.md)).
//!
//! Three processes take part. The extension runs in the user's browser. `fastbrowse native-host` is started by
//! Chrome over stdio when the extension connects, and listens on a per-user local socket ([`host`]). A run dials
//! that socket, and [`shim`] presents the extension to [`crate::cdp::Cdp`] as an ordinary browser-level CDP
//! endpoint, so the session code has one path for both backends.

pub mod host;
pub mod protocol;
pub mod shim;

/// The `cdp_url` scheme that names a connected extension's socket instead of a DevTools websocket.
pub const SCHEME: &str = "fastbrowse-extension:";

/// The event a relay raises when the user stops the run in the browser: the popup's Stop, the infobar's Cancel, or
/// closing every tab the run opened. Commands fail afterwards; this says why.
pub const STOPPED_EVENT: &str = "Fastbrowse.stoppedByUser";

/// Why the run stopped, from the `by` the extension sends with its stop event.
pub fn stop_reason(by: &serde_json::Value) -> &'static str {
    match by.as_str() {
        Some("popup") => "the run was stopped from the fastbrowse extension's popup",
        Some("debugging_bar") => {
            "the run was stopped by Cancel on Chrome's debugging bar, which stops every run in that browser"
        }
        Some("last_tab_closed") => "the run was stopped because its last tab was closed",
        _ => "the run was stopped in the browser",
    }
}

/// The unpacked extension, as `(file name, contents)`: what `fastbrowse extension path` writes for Load unpacked.
pub const FILES: [(&str, &[u8]); 9] = [
    ("manifest.json", include_bytes!("../../extension/manifest.json")),
    ("background.js", include_bytes!("../../extension/background.js")),
    ("popup.html", include_bytes!("../../extension/popup.html")),
    ("popup.js", include_bytes!("../../extension/popup.js")),
    ("allowed-methods.json", protocol::ALLOWED_METHODS.as_bytes()),
    ("icon-16.png", include_bytes!("../../extension/icon-16.png")),
    ("icon-32.png", include_bytes!("../../extension/icon-32.png")),
    ("icon-48.png", include_bytes!("../../extension/icon-48.png")),
    ("icon-128.png", include_bytes!("../../extension/icon-128.png")),
];
