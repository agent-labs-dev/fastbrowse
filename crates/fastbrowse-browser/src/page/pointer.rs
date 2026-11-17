//! A click at a point of the viewport, for a target Jev found in a screenshot rather than in the DOM.

use fastbrowse_core::models::StepOutcome;
use fastbrowse_core::page::ActResult;
use serde_json::json;

use super::CdpPage;
use super::scripts::DEEP_HIT_JS;
use crate::error::BrowserError;

impl CdpPage {
    /// Press and release the left button at `x` and `y`, fractions of the visual viewport's width and height, as the
    /// viewport screenshot shows it, and report what changed as `act` does. With `within`, the click is made only when
    /// the point is on that control of the last observation, and is refused as covered otherwise.
    pub async fn click_viewport(&self, x: f64, y: f64, within: Option<&str>) -> Result<ActResult, BrowserError> {
        self.session.assert_clear().await?;
        let session_id = self.session.active_session_id()?;
        let metrics = self.session.send("Page.getLayoutMetrics", json!({}), Some(&session_id)).await?;
        let viewport = &metrics["cssVisualViewport"];
        let (Some(width), Some(height)) = (viewport["clientWidth"].as_f64(), viewport["clientHeight"].as_f64()) else {
            return Err(BrowserError::Failed("the page reported no viewport size".into()));
        };
        let (x, y) = (x * width, y * height);
        if let Some(control) = within
            && !self.holds_point(&session_id, control, x, y).await?
        {
            return Ok(ActResult {
                outcome: StepOutcome::Covered,
                failure: None,
                page_changed: false,
                detail: Some(format!("the point is not on control {control}")),
                moved: false,
            });
        }
        let before = self.fingerprint().await?;
        let (popups, window_opens, requests) =
            (self.session.popups(), self.session.window_opens(), self.session.requests_mark());
        let mouse = |kind| {
            let params = json!({"type": kind, "x": x, "y": y, "button": "left", "clickCount": 1});
            self.input(self.session.send("Input.dispatchMouseEvent", params, Some(&session_id)))
        };
        tokio::try_join!(mouse("mousePressed"), mouse("mouseReleased"))?;
        let changed = self.changed_since(&before, Some(requests)).await?;
        let opened = self.session.follow_popup(&popups, self.session.window_opens() != window_opens).await?.is_some();
        Ok(ActResult {
            outcome: StepOutcome::Executed,
            failure: None,
            page_changed: changed || opened,
            detail: Some(
                if opened { "opened a new tab, now active" } else { "clicked where the screenshot shows it" }.into(),
            ),
            moved: opened,
        })
    }

    /// Whether the topmost element at `x`, `y` (CSS pixels of the active document) is `control` or inside it. A control
    /// in another process's frame, or gone from the page, holds no point.
    async fn holds_point(&self, session_id: &str, control: &str, x: f64, y: f64) -> Result<bool, BrowserError> {
        let Some(target) = self.last().and_then(|last| last.controls.get(control).cloned()) else { return Ok(false) };
        if target.0 != session_id {
            return Ok(false);
        }
        let script = format!(
            "((id, x, y) => {{ const e = window.__fastbrowse?.nodes.get(id); if (!e?.isConnected || e.ownerDocument !== document) return false; \
             {DEEP_HIT_JS} const hit = deepHit(document, x, y); return !!hit && holds(e, hit); }})({}, {x}, {y})",
            target.2
        );
        Ok(self.evaluate(session_id, &script).await? == serde_json::Value::Bool(true))
    }
}
