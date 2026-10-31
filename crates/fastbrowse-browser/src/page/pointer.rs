//! A click at a point of the viewport, for a target Jev found in a screenshot rather than in the DOM.

use fastbrowse_core::models::StepOutcome;
use fastbrowse_core::page::ActResult;
use serde_json::json;

use super::CdpPage;
use crate::error::BrowserError;

impl CdpPage {
    /// Press and release the left button at `x` and `y`, fractions of the visual viewport's width and height, as the
    /// viewport screenshot shows it, and report what changed as `act` does.
    pub async fn click_viewport(&self, x: f64, y: f64) -> Result<ActResult, BrowserError> {
        self.session.assert_clear().await?;
        let session_id = self.session.active_session_id()?;
        let metrics = self.session.send("Page.getLayoutMetrics", json!({}), Some(&session_id)).await?;
        let viewport = &metrics["cssVisualViewport"];
        let (Some(width), Some(height)) = (viewport["clientWidth"].as_f64(), viewport["clientHeight"].as_f64()) else {
            return Err(BrowserError::Failed("the page reported no viewport size".into()));
        };
        let before = self.fingerprint().await?;
        let (popups, window_opens, requests) =
            (self.session.popups(), self.session.window_opens(), self.session.requests_mark());
        let mouse = |kind| {
            let params = json!({"type": kind, "x": x * width, "y": y * height, "button": "left", "clickCount": 1});
            self.input(self.session.send("Input.dispatchMouseEvent", params, Some(&session_id)))
        };
        tokio::try_join!(mouse("mousePressed"), mouse("mouseReleased"))?;
        let changed = self.changed_since(&before, requests).await?;
        let opened = self.session.follow_popup(&popups, self.session.window_opens() != window_opens).await?.is_some();
        Ok(ActResult {
            outcome: StepOutcome::Executed,
            page_changed: changed || opened,
            detail: Some(
                if opened { "opened a new tab, now active" } else { "clicked where the screenshot shows it" }.into(),
            ),
            moved: opened,
        })
    }
}
