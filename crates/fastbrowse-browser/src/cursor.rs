//! The agent cursor fastbrowse draws inside the page it drives (js/cursor.js), so whoever watches the tab, live or in a
//! recording, sees where each action lands while their own pointer stays where they left it.
//!
//! It is drawn by the page itself, in an isolated world, so it needs no desktop tool and shows the same on Wayland,
//! X11, macOS and in a cloud browser's screencast, and the extension and CDP runs share it. It is purely visual: the
//! input is still sent over CDP, so nothing here can change what an action did. A document that refuses or loses the
//! script (a navigation destroys its world) only misses that drawing; the next document starts from where the last
//! one left the cursor.
//!
//! It never reaches what the agent reads: snapshot.js and capture.js skip its host, and `hidden` takes it out of the
//! page around a screenshot for the model while the video and live view hold their last frame.

use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::timeout;

use crate::session::BrowserSession;

pub const CURSOR_JS: &str = include_str!("../js/cursor.js");
/// The isolated world the cursor runs in, so page scripts can neither see nor call it.
pub const CURSOR_WORLD: &str = "fastbrowse-cursor";

// The glide is paced to a travel speed rather than one duration, so a sweep across the page and a hop between
// neighbouring controls cross at the same pace a hand would: as long as the distance needs at `GLIDE_PX_PER_S`, no
// shorter than a floor that keeps a short hop visible, and capped so one step is not held up behind a long sweep.
const GLIDE_MS: f64 = 550.0;
const GLIDE_MAX_MS: f64 = 2200.0;
const GLIDE_PX_PER_S: f64 = 500.0;
/// How long a command to the page may take beyond its own animation before the drawing is given up on.
const CALL_SLACK: Duration = Duration::from_secs(1);

/// The glide to a point `distance` CSS pixels away.
fn glide_ms(distance: f64) -> f64 {
    (distance.max(0.0) / GLIDE_PX_PER_S * 1000.0).clamp(GLIDE_MS, GLIDE_MAX_MS)
}

/// The run's cursor in the tab it drives.
pub struct PageCursor {
    session: BrowserSession,
    /// Where the cursor was last sent, in viewport CSS pixels, so a new document starts it there.
    last: Mutex<Option<(f64, f64)>>,
}

impl PageCursor {
    pub fn new(session: BrowserSession) -> Self {
        Self { session, last: Mutex::new(None) }
    }

    /// Glide to a point in the active page's viewport and wait until it has landed.
    pub async fn glide(&self, x: f64, y: f64) {
        let from = self.last.lock().unwrap().replace((x, y));
        // An unchanged point is still sent, with no travel: a navigation since may have taken the cursor out of the page.
        let ms = from.filter(|&at| at != (x, y)).map_or(0.0, |(fx, fy)| glide_ms((x - fx).hypot(y - fy)));
        let from = from.map_or(Value::Null, |(fx, fy)| json!([fx, fy]));
        let wait = Duration::from_secs_f64(ms / 1000.0) + CALL_SLACK;
        self.call(&format!("glide({x}, {y}, {ms}, {from})"), true, wait).await;
    }

    /// Show a press at the point the input is about to be sent to.
    pub async fn press(&self, x: f64, y: f64) {
        *self.last.lock().unwrap() = Some((x, y));
        self.call(&format!("press({x}, {y})"), false, CALL_SLACK).await;
    }

    /// Carry the cursor along a drag's path, in viewport CSS pixels, over the time its input takes. Returns at once,
    /// so the drawing runs alongside the input.
    pub async fn trace(&self, points: &[(f64, f64)], duration: Duration) {
        let Some(&end) = points.last() else { return };
        *self.last.lock().unwrap() = Some(end);
        let points: Vec<[f64; 2]> = points.iter().map(|&(x, y)| [x, y]).collect();
        let ms = duration.as_secs_f64() * 1000.0;
        self.call(&format!("trace({}, {ms})", json!(points)), false, CALL_SLACK).await;
    }

    /// Run `work` with the cursor out of the page: a screenshot for the model must not show it. The recording and
    /// live view hold their last frame meanwhile, so they never show the page without it. None, without running
    /// `work`, when the page did not confirm the cursor is gone.
    pub async fn hidden<T>(&self, work: impl Future<Output = T>) -> Option<T> {
        if self.last.lock().unwrap().is_none() {
            return Some(work.await);
        }
        // Frames resume however this ends: a run cancelled mid-screenshot must not freeze the recording and live view.
        struct Withheld<'a>(&'a BrowserSession);
        impl Drop for Withheld<'_> {
            fn drop(&mut self) {
                self.0.set_cursor_hidden(false);
            }
        }
        self.session.set_cursor_hidden(true);
        let withheld = Withheld(&self.session);
        let gone = self.call("hide()", true, CALL_SLACK).await.is_some_and(|reply| reply["result"]["value"] == true);
        let result = if gone { Some(work.await) } else { None };
        self.call("show()", false, CALL_SLACK).await;
        drop(withheld);
        result
    }

    /// Install the cursor in the active page's top document if it has none, then call it, returning the reply. Every
    /// failure is quiet: a document that navigated away mid-call has no cursor to draw, and the next action draws it in
    /// the new one.
    async fn call(&self, method: &str, await_promise: bool, wait: Duration) -> Option<Value> {
        timeout(wait, async {
            let session_id = self.session.active_session_id().ok()?;
            let tree = self.session.send("Page.getFrameTree", json!({}), Some(&session_id)).await.ok()?;
            let world = self
                .session
                .send(
                    "Page.createIsolatedWorld",
                    json!({"frameId": tree["frameTree"]["frame"]["id"], "worldName": CURSOR_WORLD}),
                    Some(&session_id),
                )
                .await
                .ok()?;
            let expression = format!("({}).{method}", CURSOR_JS.trim_end().trim_end_matches(';'));
            let params = json!({
                "expression": expression,
                "contextId": world["executionContextId"],
                "awaitPromise": await_promise,
                "returnByValue": true,
            });
            self.session.send("Runtime.evaluate", params, Some(&session_id)).await.ok()
        })
        .await
        .ok()
        .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glides_at_a_steady_pace_between_floor_and_ceiling() {
        assert_eq!(glide_ms(0.0), GLIDE_MS);
        assert_eq!(glide_ms(500.0), 1000.0);
        assert_eq!(glide_ms(10_000.0), GLIDE_MAX_MS);
    }
}
