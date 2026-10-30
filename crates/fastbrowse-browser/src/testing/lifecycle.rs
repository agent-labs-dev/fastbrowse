//! Test seams added for the lifecycle suites; see the module docs of `testing`.

use fastbrowse_core::models::StepOutcome;
use serde_json::json;

use crate::error::BrowserError;
use crate::page::CdpPage;
use crate::session::BrowserSession;

/// Whether the session is closing.
pub fn closing(session: &BrowserSession) -> bool {
    super::shared(session).state.lock().unwrap().closing
}

/// Run the page's back action.
pub async fn back(page: &CdpPage) -> Result<(StepOutcome, Option<String>), BrowserError> {
    page.back().await
}

/// Send a bare Enter keyDown through the page's input path.
pub async fn input_enter(page: &CdpPage, session_id: &str) -> Result<(), BrowserError> {
    let session = page.session();
    page.input(session.send("Input.dispatchKeyEvent", json!({"type": "keyDown", "key": "Enter"}), Some(session_id)))
        .await
}

/// The page's cursor geometry for this session and document.
#[cfg(feature = "cursor")]
pub async fn cursor_geometry(
    page: &CdpPage,
    expected_session: &str,
    expected_document: &str,
) -> Option<crate::cursor::Geometry> {
    page.cursor_geometry(expected_session.to_owned(), expected_document.to_owned())().await
}

/// `CursorFeedback` on the `cua-driver` at `binary`, after its handshake; None when the handshake fails.
#[cfg(feature = "cursor")]
pub async fn open_cursor(binary: &std::path::Path) -> Option<crate::cursor::CursorFeedback> {
    crate::cursor::open(binary).await
}
