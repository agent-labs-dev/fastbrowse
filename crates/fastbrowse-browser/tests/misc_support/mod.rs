//! The helpers tests/browser/test_browser.py lends the capture, navigation and evidence suites: `eval_value`,
//! `wait_until`, `find` and `observe_until`, and slicing a capture's text by a block's code-point offsets.

#![allow(dead_code)]

use std::future::Future;
use std::time::{Duration, Instant};

use fastbrowse_browser::error::BrowserError;
use fastbrowse_browser::page::CdpPage;
use fastbrowse_browser::session::BrowserSession;
use fastbrowse_core::page::{Block, BlockKind, Capture, Control, Observation};
use serde_json::{Value, json};

/// `eval_value`, keeping the failure: a read that straddles a navigation can fail.
pub async fn try_eval(session: &BrowserSession, session_id: &str, expression: &str) -> Result<Value, BrowserError> {
    let mut out = session
        .send("Runtime.evaluate", json!({"expression": expression, "returnByValue": true}), Some(session_id))
        .await?;
    Ok(out["result"]["value"].take())
}

/// `eval_value`: the expression's value in the page, `Null` for Python's None.
pub async fn eval_value(session: &BrowserSession, session_id: &str, expression: &str) -> Value {
    try_eval(session, session_id, expression).await.expect("Runtime.evaluate")
}

/// `eval_value` on the session's active tab.
pub async fn eval_active(session: &BrowserSession, expression: &str) -> Value {
    eval_value(session, &session.active_session_id().expect("active tab"), expression).await
}

/// Have the active tab report itself visible. pytest runs one test at a time, so its tab is the window's front one;
/// here the binary's tests share one Chrome at once, every tab but the newest is hidden, and a hidden page skips the
/// waits on its loading indicator.
pub async fn visible(session: &BrowserSession) {
    let session_id = session.active_session_id().expect("active tab");
    session
        .send("Emulation.setFocusEmulationEnabled", json!({"enabled": true}), Some(&session_id))
        .await
        .expect("focus");
}

/// `json.dumps(text)` for a string, as the tests splice markup into a script.
pub fn js(text: &str) -> String {
    serde_json::to_string(text).unwrap()
}

/// `document.body.innerHTML = {json.dumps(markup)}` on the active tab.
pub async fn set_body(session: &BrowserSession, markup: &str) {
    eval_active(session, &format!("document.body.innerHTML = {}", js(markup))).await;
}

pub async fn wait_until<F, Fut>(mut predicate: F, timeout_seconds: f64)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_seconds);
    while Instant::now() < deadline {
        if predicate().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("condition was not met in time");
}

/// `find`: the first control whose label contains `label`.
pub fn find<'a>(observation: &'a Observation, label: &str) -> &'a Control {
    observation.controls.iter().find(|c| c.label.contains(label)).unwrap_or_else(|| {
        panic!(
            "no control labelled like {label:?} in {:?}",
            observation.controls.iter().map(|c| &c.label).collect::<Vec<_>>()
        )
    })
}

/// Observe until a control is actually there: an attached frame session predates its document.
pub async fn observe_until(page: &CdpPage, label: &str, timeout_seconds: f64) -> Observation {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_seconds);
    while Instant::now() < deadline {
        let observation = page.observe().await.expect("observe");
        if observation.controls.iter().any(|c| c.label.contains(label)) {
            return observation;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("condition was not met in time");
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
