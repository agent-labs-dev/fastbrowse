//! The helpers the capture, navigation and evidence suites share: slicing a capture's text by a block's code-point
//! offsets, and setting up a page.

#![allow(dead_code)]

use fastbrowse_browser::page::CdpPage;
use fastbrowse_browser::session::BrowserSession;
use fastbrowse_core::page::{Block, BlockKind, Capture};
use serde_json::json;

/// Have the active tab report itself visible. pytest runs one test at a time, so its tab is the window's front one;
/// here the binary's tests share one Chrome at once, every tab but the newest is hidden, and a hidden page skips the
/// settle's wait for a drawn frame.
pub async fn visible(session: &BrowserSession) {
    let session_id = session.active_session_id().expect("active tab");
    session
        .send("Emulation.setFocusEmulationEnabled", json!({"enabled": true}), Some(&session_id))
        .await
        .expect("focus");
}

/// `document.body.innerHTML = {json.dumps(markup)}` on the active tab.
pub async fn set_body(session: &BrowserSession, markup: &str) {
    crate::common::eval_active(session, &format!("document.body.innerHTML = {}", crate::common::js(markup))).await;
}

/// `capture.text[block.start : block.end]`: the offsets count code points, as Python indexes a `str`.
pub fn text_of(capture: &Capture, block: &Block) -> String {
    capture.block_text(block).to_owned()
}

/// Every block's text, in order.
pub fn texts(capture: &Capture) -> Vec<String> {
    capture.blocks.iter().map(|b| text_of(capture, b)).collect()
}

/// These checks cover rendered content; DOM observations have their own browser regressions.
pub async fn content_capture(page: &CdpPage) -> Capture {
    let mut capture = page.capture().await.expect("capture");
    capture.blocks.retain(|b| b.kind != BlockKind::Observation);
    capture
}

/// `heading_path` as the tuple of `&str` a test compares it with.
pub fn path(block: &Block) -> Vec<&str> {
    block.heading_path.iter().map(String::as_str).collect()
}
