//! `CdpPage.act` and every action it dispatches, with the hit tests and freshness guards in front of them.

use std::future::Future;

use data_encoding::BASE64;
use fastbrowse_core::models::{Attachment, Operation, StepOutcome};
use fastbrowse_core::page::{ActResult, Action, Observation};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep, timeout};

use super::scripts::{
    ACTIVATION_KEY_JS, COVER_JS, GUARD_HOLDS_JS, HANDED_FOCUS_JS, HIT_TEST_JS, OBSTRUCTIONS, PRESENTED_JS,
    SELECT_TEXT_JS, UNCOVER_JS, py_float,
};
use super::{BackTarget, CdpPage, Target, truthy};
use crate::cdp::secs;
use crate::error::BrowserError;
use crate::session::py_json;
use crate::urlsplit::urlsplit;

const TARGET_STABILITY_SECONDS: f64 = 1.0;
/// Points the pointer travels through between a drag's press and its release.
const DRAG_STEPS: u32 = 12;
/// Pause between a drag's moves, so the renderer reads travel and not one teleport to the target.
const DRAG_STEP_SECONDS: f64 = 0.01;
/// Bound on the measuring call that also waits for a drawn frame, out of the page's reach.
const MEASURE_DRAWN_SECONDS: f64 = 2.0;
/// A deadline, not a wait: focus normally lands in one or two ticks. A loaded CI runner took over 0.3s.
const FOCUS_SETTLE_SECONDS: f64 = 1.0;
/// Long enough for a suggestion request to come back over a slow connection, and paid only by a field that
/// advertises a popup at all.
const SUGGESTION_SECONDS: f64 = 1.2;
/// A lazily built menu or dialog arrived 0.15-0.7s after the click on the pages this was measured on. Paid only by a
/// control that advertised a popup and had not opened one yet.
const POPUP_SECONDS: f64 = 1.2;
/// A download's bytes are taken off the paused response in a task of its own, which normally finishes before the
/// click's settle does. Paid only by a click whose own input began a navigation and left the page unchanged.
const DOWNLOAD_SETTLE_SECONDS: f64 = 1.5;
/// Types whose committed value is the type's own ISO shape, set through the value setter, not typed keystrokes.
const DATE_INPUT_TYPES: [&str; 5] = ["date", "datetime-local", "month", "week", "time"];
const CHANGED: &str = "control changed before keyboard use";

/// Where a hit test found a control: a point of it, covered, still scrolling, or gone (`None` in Python).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Point {
    At(f64, f64),
    Covered,
    Unsettled,
    Gone,
}

#[derive(Debug, Clone)]
struct Refusal {
    outcome: StepOutcome,
    detail: Option<String>,
}

/// Where a covered control can be acted on: a point scrolled clear of the cover, the keyboard, or why nowhere.
enum Route {
    At(f64, f64),
    Keyboard,
    Refused(Refusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cover {
    Obstruction,
    Band,
    Other,
    None,
    Gone,
}

type Outcome = (StepOutcome, Option<String>);

fn bad(shape: &str) -> BrowserError {
    BrowserError::Failed(format!("page script returned an unexpected {shape}"))
}

fn parse_point(value: &Value, unsettled: bool) -> Result<Point, BrowserError> {
    match value {
        Value::Null => Ok(Point::Gone),
        Value::String(s) if s == "covered" => Ok(Point::Covered),
        Value::String(s) if s == "unsettled" && unsettled => Ok(Point::Unsettled),
        Value::Array(xy) if xy.len() == 2 => match (xy[0].as_f64(), xy[1].as_f64()) {
            (Some(x), Some(y)) => Ok(Point::At(x, y)),
            _ => Err(bad("point")),
        },
        _ => Err(bad("point")),
    }
}

fn parse_cover(value: &Value) -> Result<Cover, BrowserError> {
    Ok(match value.as_str() {
        Some("obstruction") => Cover::Obstruction,
        Some("band") => Cover::Band,
        Some("other") => Cover::Other,
        Some("none") => Cover::None,
        None if value.is_null() => Cover::Gone,
        _ => return Err(bad("cover")),
    })
}

/// `[fingerprint, live guard, hit-test point, popup announced, opens a window]` from the pre-action check.
type TargetState = (String, Option<String>, Point, bool, bool);

fn parse_target_state(value: &Value) -> Result<TargetState, BrowserError> {
    let shape = "tuple[str, str | None, _Point, bool, bool]";
    let Some([fingerprint, guard, point, announced, opens]) = value.as_array().map(Vec::as_slice) else {
        return Err(bad(shape));
    };
    let fingerprint = fingerprint.as_str().ok_or_else(|| bad(shape))?.to_owned();
    let guard = match guard {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        _ => return Err(bad(shape)),
    };
    let point = parse_point(point, false)?;
    Ok((
        fingerprint,
        guard,
        point,
        announced.as_bool().ok_or_else(|| bad(shape))?,
        opens.as_bool().ok_or_else(|| bad(shape))?,
    ))
}

fn opt_i64(value: &Value) -> Result<Option<i64>, BrowserError> {
    match value {
        Value::Null => Ok(None),
        v => v.as_i64().map(Some).ok_or_else(|| bad("int | None")),
    }
}

impl CdpPage {
    /// Dispatch at most once. Returns STALE or COVERED without dispatching when guards fail.
    pub async fn act(&self, action: &Action, observation: &Observation) -> Result<ActResult, BrowserError> {
        self.session.assert_clear().await?;
        let result = |outcome, page_changed, detail: &str| ActResult {
            outcome,
            page_changed,
            detail: Some(detail.to_owned()),
            form_unchanged: false,
            moved: false,
        };
        let Some(last) = self.last().filter(|last| last.page_key == observation.page_key) else {
            return Ok(result(StepOutcome::Stale, false, "observation is out of date"));
        };
        let mut target: Option<Target> = None;
        if let Some(target_id) = &action.target_id {
            match last.controls.get(target_id) {
                Some(found) => target = Some(found.clone()),
                None => return Ok(result(StepOutcome::Stale, false, "unknown control id")),
            }
        }
        if action.form_fill
            && let Some(target) = &target
        {
            let current = observation
                .controls
                .iter()
                .find(|c| Some(&c.id) == action.target_id.as_ref())
                .and_then(|c| c.value.clone())
                .unwrap_or_default();
            if !self.form_unchanged(target, &current).await {
                return Ok(result(StepOutcome::Stale, false, "form changed before fill"));
            }
        }
        // A pending dialog already blocks the renderer's main thread; evaluating now would hang.
        let mut before_fingerprint = String::new();
        let mut point = Point::Gone;
        let (mut announced, mut opens) = (false, false);
        let active = self.session.active_session_id()?;
        // The pointer must move only once the browser has drawn the layout the point was measured in. Waiting for that
        // frame inside the measuring call saves the pointer move a round trip of its own.
        let pointer_first = matches!(action.operation, Operation::Click | Operation::Fill | Operation::Hover);
        let mut drawn = pointer_first && target.as_ref().is_some_and(|t| t.0 == active);
        if self.session.pending_dialog().is_none() {
            let hit_test = action.operation.is_targeted();
            // The frame wait runs on the page's own timers, which a page can replace with ones that never fire; past
            // this bound the point is measured again and the pointer move waits for its own frame.
            let measured = if drawn {
                match timeout(
                    secs(MEASURE_DRAWN_SECONDS),
                    self.before_action(target.as_ref(), hit_test, false, false, true, true),
                )
                .await
                {
                    Ok(measured) => measured,
                    // The renderer may be synchronously blocked. Do not queue a second evaluation behind it.
                    Err(_) => {
                        return Err(BrowserError::PageUnresponsive(
                            "page did not draw the measured layout before the deadline".into(),
                        ));
                    }
                }
            } else {
                self.before_action(target.as_ref(), hit_test, false, false, true, false).await
            };
            let measured = match measured {
                Err(BrowserError::Script) => {
                    // A page can replace those timers with ones that throw while leaving the renderer responsive.
                    drawn = false;
                    self.before_action(target.as_ref(), hit_test, false, false, true, false).await?
                }
                other => other?,
            };
            let live_guard;
            (before_fingerprint, live_guard, point, announced, opens) = measured;
            if let Some(target) = &target
                && live_guard != target.3
            {
                return Ok(result(StepOutcome::Stale, false, "control changed since observation"));
            }
        }

        // Drawn beside the action, never in its path: a secret's field is left unmarked so nothing about it is drawn.
        #[cfg(feature = "cursor")]
        if let (Some(cursor), Point::At(x, y), Some(target)) = (&self.cursor, point, &target)
            && target.0 == active
            && !action.secret
        {
            cursor.follow(self.cursor_geometry(target.0.clone(), observation.document_key.clone()), (x, y));
        }
        let popups = self.session.popups();
        // A dialog blocks the page, and an observation taken under it reads no document, so the mark taken before the
        // action that opened it stands: accepting a confirm that navigates is a move the run made, and without a mark
        // it was never reported as one.
        let document = if action.operation == Operation::Dialog {
            let marked = self.state.lock().unwrap().marked.take();
            let opened_here =
                marked.as_ref().is_some_and(|m| self.session.pending_dialog_url().as_deref() == Some(&m.1));
            marked.filter(|_| opened_here).map(|m| m.0)
        } else {
            self.input_mark(&observation.url)
        };
        let window_opens = self.session.window_opens();
        let requests = self.session.requests_mark();
        let downloads_before = self.session.downloads_settled();
        let (outcome, mut detail) = match self.dispatch(action, target.as_ref(), point, drawn).await {
            Ok(dispatched) => dispatched,
            Err(error) if !action.form_fill => return Err(error),
            // An input handler can navigate before retention is checked. Its next field needs a fresh decision.
            Err(_) => return Ok(result(StepOutcome::Failed, true, "form changed during fill")),
        };
        if outcome != StepOutcome::Executed {
            return Ok(ActResult {
                outcome,
                page_changed: self.session.pending_dialog().is_some(),
                detail,
                form_unchanged: false,
                moved: false,
            });
        }
        if action.operation == Operation::Click
            && let Some(target) = &target
            && announced
        {
            self.until_dialog(self.await_popup(&target.0, target.2)).await;
        }
        let active_now = self.session.active_session_id().ok();
        let input_settling = action.operation == Operation::Fill
            || (!matches!(action.operation, Operation::Back | Operation::SwitchTab | Operation::Dialog)
                && target.as_ref().is_none_or(|t| Some(&t.0) == active_now.as_ref()));
        let mut changed = self.changed_since(&before_fingerprint, input_settling, requests).await?;
        if matches!(action.operation, Operation::Click | Operation::Enter)
            && self.session.follow_popup(&popups, opens || self.session.window_opens() != window_opens).await?.is_some()
        {
            return Ok(ActResult {
                outcome: StepOutcome::Executed,
                page_changed: true,
                detail: Some("opened a new tab, now active".into()),
                form_unchanged: false,
                moved: true,
            });
        }
        let moved = self.moved_by_input(document);
        if !changed && moved && self.download_landed(downloads_before).await {
            // A link whose target is a file downloads it: Chrome aborts the document navigation and the tab keeps
            // the page it was on, so nothing on the page changed. The download is the action's whole effect, and
            // the text it carries is only read by the next observation, so the run must not be told the click did
            // nothing and must not observe before the captured file is there to read.
            // Saving a file an earlier click already saved is not an effect: an exports page whose one link was
            // clicked four times, each time rewriting the same CSV, kept the run's click loop going.
            if self.session.repeated_download(downloads_before) {
                detail = detail.or_else(|| Some("saved the same file again, which changes nothing".into()));
            } else {
                changed = true;
                detail = detail.or_else(|| Some("downloaded the file the link points at".into()));
            }
        }
        let mut unchanged = false;
        if action.form_fill
            && !changed
            && let Some(target) = &target
            && !action.secret
        {
            unchanged = self.form_unchanged(target, action.text.as_deref().unwrap_or_default()).await;
        }
        Ok(ActResult {
            outcome: StepOutcome::Executed,
            page_changed: changed,
            detail,
            form_unchanged: unchanged,
            moved,
        })
    }

    /// Whether a download landed since `since`, waiting the capture's own task out briefly.
    ///
    /// Bounded and only asked after an input the run made began a navigation that left the page unchanged, so a
    /// plain click that only redrew its page pays nothing for it. A download already captured returns at once.
    async fn download_landed(&self, since: u32) -> bool {
        self.session.downloads_settled() > since || self.session.download_settled(since, DOWNLOAD_SETTLE_SECONDS).await
    }

    /// How many moves the run's input had made before an action, as the input watch reports them, marked with the
    /// address the action was checked against. That observation's address stands in for the page's own: asking the
    /// page cost every action a round trip, over 100ms to a cloud browser.
    fn input_mark(&self, address: &str) -> Option<u32> {
        let mark = self.session.pending_dialog().is_none().then(|| self.session.input_navigations());
        self.state.lock().unwrap().marked = mark.map(|begun| (begun, address.to_owned()));
        mark
    }

    /// Whether the action just taken moved the page: its own input began a navigation, to another document or on the
    /// same one. A new document alone proves nothing, since a page can move itself on a timer; unknown counts as not
    /// moved, which keeps a guessed page guessed, the safe side.
    fn moved_by_input(&self, begun: Option<u32>) -> bool {
        begun.is_some_and(|begun| self.session.input_navigations() > begun)
    }

    /// Where the active page's viewport sits in its window, as the page itself reports it.
    #[cfg(feature = "cursor")]
    pub(crate) fn cursor_geometry(
        &self,
        expected_session: String,
        expected_document: String,
    ) -> crate::cursor::GeometryFn {
        use crate::cursor::Geometry;
        let session = self.session.clone();
        let pid = self.cursor_pid.clone();
        std::sync::Arc::new(move || {
            let session = session.clone();
            let pid = pid.clone();
            let (expected_session, expected_document) = (expected_session.clone(), expected_document.clone());
            Box::pin(async move {
                let session_id = session.active_session_id().ok()?;
                if session_id != expected_session {
                    return None;
                }
                let known = *pid.lock().unwrap();
                let browser_pid = match known {
                    Some(known) => known,
                    None => {
                        let processes = session.send("SystemInfo.getProcessInfo", json!({}), None).await.ok()?;
                        let found = processes["processInfo"]
                            .as_array()?
                            .iter()
                            .find(|p| p["type"] == "browser")
                            .and_then(|p| p["id"].as_i64())?;
                        *pid.lock().unwrap() = Some(found);
                        found
                    }
                };
                let evaluate = session.send(
                    "Runtime.evaluate",
                    json!({"expression": super::scripts::GEOMETRY_JS, "returnByValue": true, "awaitPromise": true}),
                    Some(&session_id),
                );
                let metrics = session.send("Page.getLayoutMetrics", json!({}), Some(&session_id));
                // A navigating click destroys its old context; skip the drawing without disabling later feedback.
                let (reading, metrics) = tokio::try_join!(evaluate, metrics).ok()?;
                if reading.get("exceptionDetails").is_some() {
                    return None;
                }
                let reading = reading["result"]["value"].as_array()?.clone();
                if session.active_session_id().ok()? != session_id
                    || reading.len() != 9
                    || reading[8].as_str() != Some(expected_document.as_str())
                {
                    return None;
                }
                // CSS pixels in the page, device-independent pixels in the window: their ratio is the page zoom.
                let css_width = metrics["cssLayoutViewport"]["clientWidth"].as_f64().filter(|w| *w != 0.0)?;
                let number = |i: usize| reading[i].as_f64();
                Some(Geometry {
                    browser_pid: Some(browser_pid),
                    screen_x: number(0)?,
                    screen_y: number(1)?,
                    outer_width: number(2)?,
                    outer_height: number(3)?,
                    inner_width: number(4)?,
                    inner_height: number(5)?,
                    pinch_scale: number(6)?,
                    visible: reading[7].as_bool()?,
                    zoom: metrics["layoutViewport"]["clientWidth"].as_f64()? / css_width,
                })
            })
        })
    }

    pub(crate) async fn form_unchanged(&self, target: &Target, text: &str) -> bool {
        if self.session.pending_dialog().is_some() || Some(&target.0) != self.session.active_session_id().ok().as_ref()
        {
            return false;
        }
        // Other frame sessions can change independently of this renderer's guard.
        if !self.session.frame_sessions().is_empty() {
            return false;
        }
        let script = format!("({})({})", self.page_js, py_json(&json!({"id": target.2, "text": text})));
        matches!(self.until_dialog(self.evaluate(&target.0, &script)).await, Some(Ok(Value::Bool(true))))
    }

    async fn dispatch(
        &self,
        action: &Action,
        target: Option<&Target>,
        point: Point,
        drawn: bool,
    ) -> Result<Outcome, BrowserError> {
        let text = action.text.as_deref().unwrap_or_default();
        match action.operation {
            Operation::Navigate => {
                let url = action.url.as_deref().unwrap_or_default();
                if action.url.is_none() || !matches!(urlsplit(url).0.as_str(), "http" | "https") {
                    return Ok((StepOutcome::Failed, Some("navigate requires an HTTP(S) address".into())));
                }
                match self.navigate(url, None).await {
                    Err(error @ BrowserError::OriginNotAllowed) => Err(error),
                    Err(error) if error.is_unavailable() => Err(error),
                    Err(error) => Ok((StepOutcome::Failed, Some(error.to_string()))),
                    Ok(()) => Ok((StepOutcome::Executed, None)),
                }
            }
            Operation::Click => self.click(target, point, drawn).await,
            Operation::Hover => self.hover(target, point, drawn).await,
            Operation::Fill => {
                let secret_origin = action.secret_origin.as_deref();
                let filled =
                    self.until_dialog(self.fill(target, text, point, action.secret, secret_origin, drawn)).await;
                match filled {
                    Some(filled) => filled,
                    None => {
                        Ok((StepOutcome::Failed, Some("dialog interrupted fill before its value was verified".into())))
                    }
                }
            }
            Operation::Select => self.select(target, text, point).await,
            Operation::Enter => self.key(target, "Enter", point).await,
            Operation::Drag => self.drag(target, point, action.destination_id.as_deref()).await,
            Operation::Escape => self.key(None, "Escape", Point::Gone).await,
            Operation::Scroll | Operation::ScrollUp => {
                self.scroll(action.operation).await?;
                Ok((StepOutcome::Executed, None))
            }
            Operation::Back => self.back().await,
            Operation::SwitchTab => {
                let Some(tab_id) = &action.tab_id else {
                    return Ok((StepOutcome::Failed, Some("switch_tab requires tab_id".into())));
                };
                self.session.switch_tab(tab_id).await?;
                Ok((StepOutcome::Executed, None))
            }
            Operation::Upload => self.upload(target, &action.files, point).await,
            Operation::Dialog => {
                let Some(accept) = action.accept_dialog else {
                    return Ok((StepOutcome::Failed, Some("dialog requires accept_dialog".into())));
                };
                self.session.handle_dialog(accept, action.text.as_deref()).await?;
                Ok((StepOutcome::Executed, None))
            }
            Operation::Read | Operation::Done | Operation::Escalate => Err(BrowserError::Failed(format!(
                "{} does not dispatch through the browser layer",
                action.operation.as_str()
            ))),
        }
    }

    async fn covered_detail(&self, target: &Target) -> String {
        let (session_id, local_id) = (&target.0, target.2);
        // A cart or menu can cover a still-indexed button. Name the drawn obstruction so recovery can dismiss it.
        let script = format!(
            "(() => {{ const e = window.__fastbrowse?.nodes.get({local_id}); \
             if (!e) return null; const rect = e.getBoundingClientRect(); \
             const hit = e.ownerDocument.elementFromPoint(rect.x + rect.width / 2, rect.y + rect.height / 2); \
             let cover = hit?.closest({obstructions}); \
             if (!cover) for (let node = hit; node && node !== e.ownerDocument.body; node = node.parentElement) {{ \
             if (!node.contains(e) && e.ownerDocument.defaultView.getComputedStyle(node).position === 'fixed' && \
             node.querySelector('button,[role=button],a[href]')) {{ cover = node; break; }} }} \
             cover ||= hit; if (!cover) return null; const label = node => \
             (node.getAttribute('aria-label') || node.getAttribute('title') || node.textContent || '')\
             .trim().slice(0, 120); \
             const controls = Array.from(cover.querySelectorAll('button,[role=button],a[href]'))\
             .filter(node => node.checkVisibility({{checkOpacity:true,checkVisibilityCSS:true}}))\
             .slice(0, 8).map(label).filter(Boolean); \
             return JSON.stringify({{role:cover.getAttribute('role') || cover.tagName.toLowerCase(), \
             label:cover.getAttribute('aria-label') || cover.getAttribute('title') || null, controls}}); }})()",
            obstructions = py_json(&Value::from(OBSTRUCTIONS)),
        );
        let mut detail = self.evaluate(session_id, &script).await.ok().filter(truthy);
        // A frame drawn over the control keeps its controls in its own document, which a cross-origin frame does not
        // share: a full-window ad was reported with no controls, so recovery closed another ad and stayed covered.
        // Only a control that closes the frame is named; naming the rest offered recovery the ad's own links.
        if let Some(Value::String(text)) = &detail
            // Best effort: naming the frame's controls helps recovery, and failing to must not lose the report.
            && let Ok(labels) = self.frame_controls(session_id, local_id).await
        {
            let inside: Vec<String> = labels.into_iter().filter(|l| dismisses(l)).collect();
            if !inside.is_empty()
                && let Ok(mut described) = serde_json::from_str::<serde_json::Map<String, Value>>(text)
            {
                let mut controls: Vec<String> = described
                    .get("controls")
                    .and_then(Value::as_array)
                    .map(|c| c.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
                    .unwrap_or_default();
                for label in inside {
                    if !controls.contains(&label) {
                        controls.push(label);
                    }
                }
                controls.truncate(8);
                described.insert("controls".into(), json!(controls));
                detail = Some(Value::String(py_json(&Value::Object(described))));
            }
        }
        match detail {
            Some(detail) => format!(
                "Target is covered by {}. Dismiss an observed obstruction before retrying.",
                super::value_str(&detail)
            ),
            None => "Target is covered.".into(),
        }
    }

    /// The labels of the controls observed inside the frame drawn over the control's centre, if a frame is.
    async fn frame_controls(&self, session_id: &str, local_id: i64) -> Result<Vec<String>, BrowserError> {
        let expression = format!(
            "(() => {{ const e = window.__fastbrowse?.nodes.get({local_id}); \
             if (!e) return null; const r = e.getBoundingClientRect(); \
             const hit = e.ownerDocument.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2); \
             return hit?.tagName === 'IFRAME' || hit?.tagName === 'FRAME' ? hit : null; }})()"
        );
        let out = self
            .session
            .send("Runtime.evaluate", json!({"expression": expression, "returnByValue": false}), Some(session_id))
            .await?;
        let Some(handle) = out["result"]["objectId"].as_str() else { return Ok(vec![]) };
        let node = self.session.send("DOM.describeNode", json!({"objectId": handle}), Some(session_id)).await;
        let _ = self.session.send("Runtime.releaseObject", json!({"objectId": handle}), Some(session_id)).await;
        let node = node?;
        let Some(frame_id) = node["node"]["frameId"].as_str().filter(|f| !f.is_empty()) else { return Ok(vec![]) };
        let Some(last) = self.last() else { return Ok(vec![]) };
        let mut family: Vec<String> = self.session.frame_family(frame_id).into_iter().collect();
        // A set in Python: the order its labels come out in only names controls, and is kept stable here.
        family.sort();
        Ok(family.iter().flat_map(|key| last.labels.get(key).cloned().unwrap_or_default()).collect())
    }

    /// Where a covered control can still be acted on without touching what covers it.
    ///
    /// Only a band fixed to the window is routed around: it is first scrolled clear of, and a control it still covers
    /// is left to the keyboard. A dialog, a full-window backdrop or a panel over the control in the page's own flow is
    /// reported as covering it, as before.
    ///
    /// This is the one way onto the keyboard route, and it hands that route on only while the control's guard still
    /// holds: a date field's value is set without a key, and that branch returned before a check placed further on
    /// could run. Each key or insertion checks again after focusing.
    async fn route_around(&self, target: &Target, scroll: bool) -> Result<Route, BrowserError> {
        let (session_id, local_id, guard) = (&target.0, target.2, &target.3);
        let refused = |outcome, detail: Option<String>| Ok(Route::Refused(Refusal { outcome, detail }));
        if self.cover(session_id, local_id).await? != Cover::Band {
            return refused(StepOutcome::Covered, Some(self.covered_detail(target).await));
        }
        if scroll {
            // A page in a background tab draws no frames, so its scroll handlers would run only once the keyboard route
            // brought it forward to focus the control, after every check here.
            if self.evaluate(session_id, "document.hidden").await? == Value::Bool(true) {
                self.session.bring_to_front().await?;
            }
            let script = format!("({})({local_id}, {})", *UNCOVER_JS, py_json(&json!(guard)));
            let point = parse_point(&self.evaluate(session_id, &script).await?, true)?;
            // Scroll handlers run at the next frame, and one can repurpose the control; every check after this must see
            // what they did.
            let _ = timeout(secs(0.5), self.evaluate(session_id, PRESENTED_JS)).await;
            match point {
                Point::Gone => return refused(StepOutcome::Stale, Some("target disconnected".into())),
                Point::Unsettled => {
                    return refused(StepOutcome::Stale, Some("page did not finish scrolling the control clear".into()));
                }
                Point::At(x, y) => return Ok(Route::At(x, y)),
                Point::Covered => {}
            }
            // The last alignment can have put the control under something else.
            if self.cover(session_id, local_id).await? != Cover::Band {
                return refused(StepOutcome::Covered, Some(self.covered_detail(target).await));
            }
        }
        // Scroll handlers, run by now, can have repurposed the control: an authorized "Save" turned "Delete account"
        // would have had the new action pressed unclassified.
        if !self.guard_holds(target).await? {
            return refused(StepOutcome::Stale, Some(CHANGED.into()));
        }
        Ok(Route::Keyboard)
    }

    async fn cover(&self, session_id: &str, local_id: i64) -> Result<Cover, BrowserError> {
        parse_cover(&self.evaluate(session_id, &format!("({})({local_id})", *COVER_JS)).await?)
    }

    async fn click(&self, target: Option<&Target>, point: Point, drawn: bool) -> Result<Outcome, BrowserError> {
        let Some(target) = target else { return Ok((StepOutcome::Failed, Some("click requires a target".into()))) };
        let (x, y) = match point {
            Point::Gone | Point::Unsettled => return Ok((StepOutcome::Stale, Some("target disconnected".into()))),
            Point::At(x, y) => (x, y),
            Point::Covered => match self.route_around(target, true).await? {
                Route::Refused(refusal) => return Ok((refusal.outcome, refusal.detail)),
                Route::Keyboard => return self.press(target).await,
                Route::At(x, y) => (x, y),
            },
        };
        self.click_point(target, (x, y), false, drawn).await
    }

    /// Activate a control from the keyboard: focus it and press the key its semantics answer to.
    async fn press(&self, target: &Target) -> Result<Outcome, BrowserError> {
        let (session_id, local_id) = (&target.0, target.2);
        let key = self.evaluate(session_id, &format!("({ACTIVATION_KEY_JS})({local_id})")).await?;
        let key = match key.as_str() {
            Some(k @ ("Enter" | " ")) => Some(k.to_owned()),
            None if key.is_null() => None,
            _ => return Err(bad("Literal['Enter', ' '] | None")),
        };
        let Some(key) = key else { return Ok((StepOutcome::Covered, Some(self.covered_detail(target).await))) };
        if !self.focus(session_id, local_id, false, false, false).await? {
            return Ok((StepOutcome::Covered, Some(self.covered_detail(target).await)));
        }
        if let Some(refusal) = self.keys_reach(target, true, None).await? {
            return Ok((refusal.outcome, refusal.detail));
        }
        self.press_key(session_id, &key).await?;
        Ok((StepOutcome::Executed, Some("Pressed from the keyboard: a band fixed to the window covers it.".into())))
    }

    /// Why a key or text, sent now, would not go to the control as it was authorized, or None if it would.
    ///
    /// Asked after focusing and right before each key or insertion, on every path that sends one. Focus handlers can
    /// repurpose the control as scroll handlers can, and with `guarded` its guard is checked again. They can also open a
    /// dialog, a menu or a popover over it without changing the control at all: keys still reach the focused control,
    /// so an Enter checked only before focus pressed it through the menu the focus had opened. A band over it does not
    /// refuse; that is what the keyboard route is for. `local_id` names the field focus was handed to, when it was.
    async fn keys_reach(
        &self,
        target: &Target,
        guarded: bool,
        local_id: Option<i64>,
    ) -> Result<Option<Refusal>, BrowserError> {
        if guarded && !self.guard_holds(target).await? {
            return Ok(Some(Refusal { outcome: StepOutcome::Stale, detail: Some(CHANGED.into()) }));
        }
        let node = local_id.unwrap_or(target.2);
        let cover = self.cover(&target.0, node).await?;
        Ok(self.cover_refusal(target, node, cover).await)
    }

    /// The refusal [`Self::keys_reach`] gives a field `node` that `cover` was measured over after focus.
    async fn cover_refusal(&self, target: &Target, node: i64, cover: Cover) -> Option<Refusal> {
        if !matches!(cover, Cover::Obstruction | Cover::Other) {
            return None;
        }
        let handed = (target.0.clone(), target.1.clone(), node, target.3.clone());
        Some(Refusal { outcome: StepOutcome::Covered, detail: Some(self.covered_detail(&handed).await) })
    }

    async fn guard_holds(&self, target: &Target) -> Result<bool, BrowserError> {
        let script = format!("({GUARD_HOLDS_JS})({}, {})", target.2, py_json(&json!(target.3)));
        Ok(self.evaluate(&target.0, &script).await? == Value::Bool(true))
    }

    async fn hover(&self, target: Option<&Target>, point: Point, drawn: bool) -> Result<Outcome, BrowserError> {
        let Some(target) = target else { return Ok((StepOutcome::Failed, Some("hover requires a target".into()))) };
        match point {
            Point::Gone | Point::Unsettled => Ok((StepOutcome::Stale, Some("target disconnected".into()))),
            Point::Covered => Ok((StepOutcome::Covered, Some(self.covered_detail(target).await))),
            Point::At(x, y) => {
                // The pointer stays where it lands, so what the hover reveals is still shown when the page is next read.
                self.move_to(&target.0, (x, y), drawn).await?;
                Ok((StepOutcome::Executed, None))
            }
        }
    }

    /// Give a menu, picker or dialog the click opens time to arrive before the page is observed.
    ///
    /// A header that mutates the moment it is clicked settles the fingerprint and quiets the DOM before the dialog's
    /// own bundle has loaded, so the next observation still shows the page as it was and the agent acts on whatever
    /// else is on screen. The wait is bounded, resolves the instant the control reports itself expanded or the element
    /// it controls becomes visible, and is paid only by a control that said a popup was coming. Best effort by
    /// construction: a click that navigates discards the promise.
    async fn await_popup(&self, session_id: &str, local_id: i64) {
        let script = format!(
            "new Promise(resolve => {{ const e = window.__fastbrowse?.nodes.get({local_id}); \
             const owned = () => {{ const id = e.getAttribute('aria-controls') || e.getAttribute('aria-owns'); \
             return id ? e.ownerDocument.getElementById(id) : null; }}; \
             const open = () => e.getAttribute('aria-expanded') === 'true' \
             || !!owned()?.checkVisibility({{checkOpacity: true, checkVisibilityCSS: true}}); \
             if (!e || open()) {{ resolve(true); return; }} \
             const stop = ok => {{ observer.disconnect(); clearTimeout(timer); resolve(ok); }}; \
             const observer = new MutationObserver(() => {{ if (open()) stop(true); }}); \
             observer.observe(e.ownerDocument.documentElement, {{childList: true, subtree: true, \
             attributes: true}}); \
             const timer = setTimeout(() => stop(false), {}); }})",
            py_float(POPUP_SECONDS * 1000.0)
        );
        let _ = self.evaluate(session_id, &script).await;
    }

    async fn fill(
        &self,
        target: Option<&Target>,
        text: &str,
        point: Point,
        secret: bool,
        secret_origin: Option<&str>,
        drawn: bool,
    ) -> Result<Outcome, BrowserError> {
        let Some(target) = target else { return Ok((StepOutcome::Failed, Some("fill requires a target".into()))) };
        let session_id = target.0.as_str();
        let mut local_id = target.2;
        if matches!(point, Point::Gone | Point::Unsettled) {
            return Ok((StepOutcome::Stale, Some("target disconnected".into())));
        }
        if secret && secret_origin.is_none_or(str::is_empty) {
            return Ok((StepOutcome::Failed, Some("secret fill requires an authorized origin".into())));
        }
        let route = match point {
            Point::At(x, y) => Route::At(x, y),
            _ => self.route_around(target, true).await?,
        };
        // A field a band still covers is focused and typed into, as a keyboard user would; there is nothing of its own
        // to click, and a click there lands on the band.
        let keyboard = matches!(route, Route::Keyboard);
        // On the pointer route, the field focus landed on is checked for covers in the same evaluate, once focus took.
        let (handed, native_date, mut focused, mut covered): (Option<i64>, bool, Option<bool>, Option<Cover>) =
            match route {
                Route::Refused(refusal) => return Ok((refusal.outcome, refusal.detail)),
                Route::At(x, y) => {
                    // Ported from browser-use/jev-ultrafast (MIT), browser.py: fill clicks before typing. Focus alone
                    // bypasses pointer handlers that open autocomplete and calendar pickers.
                    let (outcome, detail) = self.click_point(target, (x, y), true, drawn).await?;
                    if outcome != StepOutcome::Executed {
                        return Ok((outcome, detail));
                    }
                    let script = format!(
                        "({handed})({local_id}, {secret}).then(async id => {{ \
                     const native = {types}.includes(window.__fastbrowse?.nodes.get(id)?.type); \
                     const focused = id === null || (native && !{secret}) ? false : \
                     await ({focus})(id, false); \
                     return [id, native, focused, focused === true ? ({cover})(id) : null]; }})",
                        handed = *HANDED_FOCUS_JS,
                        secret = py_json(&json!(secret)),
                        types = py_json(&json!(DATE_INPUT_TYPES)),
                        focus = focus_script(true, secret),
                        cover = *COVER_JS,
                    );
                    let raw = self.evaluate(session_id, &script).await?;
                    let shape = "tuple[int | None, bool, bool | None, _Cover | None]";
                    let Some([id, native, focused, cover]) = raw.as_array().map(Vec::as_slice) else {
                        return Err(bad(shape));
                    };
                    let focused = match focused {
                        Value::Null => None,
                        v => Some(v.as_bool().ok_or_else(|| bad(shape))?),
                    };
                    let covered = if focused == Some(true) { Some(parse_cover(cover)?) } else { None };
                    (opt_i64(id)?, native.as_bool().ok_or_else(|| bad("bool"))?, focused, covered)
                }
                Route::Keyboard => {
                    let script = format!(
                        "{}.includes(window.__fastbrowse?.nodes.get({local_id})?.type)",
                        py_json(&json!(DATE_INPUT_TYPES))
                    );
                    let native = self.evaluate(session_id, &script).await?;
                    (Some(local_id), native == Value::Bool(true), None, None)
                }
            };
        let Some(handed) = handed else {
            return Ok((
                StepOutcome::Failed,
                Some("clicked field has no replacement in the same document and position".into()),
            ));
        };
        local_id = handed;
        if !secret && native_date {
            return self.fill_native_date(session_id, local_id, text).await;
        }
        let mut suggests = false;
        for attempt in 0..2 {
            if attempt > 0 || focused.is_none() {
                let activate = focused.is_none();
                focused = Some(self.focus(session_id, local_id, true, secret, activate).await?);
                covered = None;
            }
            if focused != Some(true) {
                return Ok((StepOutcome::Failed, Some("target did not receive keyboard focus".into())));
            }
            // No pointer press rechecked the field on the keyboard route, so the text goes only to the field that was
            // authorized, and on either route only to a field nothing has since been drawn over.
            let refusal = match covered.take() {
                Some(cover) => self.cover_refusal(target, local_id, cover).await,
                None => self.keys_reach(target, keyboard, Some(local_id)).await?,
            };
            if let Some(refusal) = refusal {
                return Ok((refusal.outcome, refusal.detail));
            }
            if secret {
                // A secret must be checked and inserted in one renderer task: CDP insertText would leave a
                // navigation/focus race between checking the origin and dispatching the secret to the page.
                let script = format!(
                    "((id, text, origin) => {{ const e = window.__fastbrowse?.nodes.get(id); \
                     if (!e?.isConnected || (origin !== null && e.ownerDocument.location.origin !== origin)) return false; \
                     const doc = e.ownerDocument; if (e.getRootNode().activeElement !== e) return false; \
                     if (typeof e.select === 'function') e.select(); else {{ const range = doc.createRange(); \
                     range.selectNodeContents(e); const selection = doc.getSelection(); selection.removeAllRanges(); \
                     selection.addRange(range); }} \
                     return doc.execCommand('insertText', false, text); }})({local_id}, {}, {})",
                    py_json(&json!(text)),
                    py_json(&json!(secret_origin))
                );
                if !truthy(&self.evaluate(session_id, &script).await?) {
                    return Ok((StepOutcome::Failed, Some("secret origin or focus changed before insertion".into())));
                }
            } else {
                self.session.send("Input.insertText", json!({"text": text}), Some(session_id)).await?;
            }
            // Whether the field asks for suggestions comes back with the check, so an ordinary field's fill does not
            // pay a round trip to learn it has none to wait for.
            let check = format!(
                "((ok, e) => [!!ok, !!ok && !!e && (e.getAttribute('role') === 'combobox' || \
                 e.type === 'search' || !!e.getAttribute('aria-autocomplete') || \
                 !!((id => id && e.ownerDocument.getElementById(id))(\
                 e.getAttribute('aria-controls') || e.getAttribute('aria-owns'))))])({}, \
                 window.__fastbrowse?.nodes.get({local_id}))",
                landed_js(local_id, text)
            );
            let landed = self.evaluate(session_id, &check).await?;
            let (landed, wants) = match landed.as_array().map(Vec::as_slice) {
                Some([Value::Bool(landed), Value::Bool(wants)]) => (*landed, *wants),
                _ => return Err(bad("tuple[bool, bool]")),
            };
            suggests = wants;
            if landed {
                break;
            }
            // An editor that took focus after the hand-off looked, and so never got the text, is typed into once more,
            // as a person would on seeing the text had gone nowhere. A keyboard fill never clicked, so no editor was
            // handed focus and a hand-off left by an earlier fill must not be followed.
            let handed = if secret || keyboard {
                Some(local_id)
            } else {
                opt_i64(&self.evaluate(session_id, &format!("({})({local_id}, false)", *HANDED_FOCUS_JS)).await?)?
            };
            match handed {
                Some(handed) if attempt == 0 && handed != local_id => local_id = handed,
                _ => return Ok((StepOutcome::Failed, Some("field did not retain the supplied text".into()))),
            }
        }
        if suggests {
            self.await_suggestions(session_id, local_id).await;
        }
        Ok((StepOutcome::Executed, None))
    }

    /// Commit an ISO value to a native date/time input the way its own picker would.
    ///
    /// Typed keystrokes land in whichever locale-formatted segment the picker's shadow UI has focused, so the value the
    /// field writer produces (told the input's type) is set directly through the property setter React and friends also
    /// patch over, then the input/change events a form's own handlers expect to see.
    async fn fill_native_date(&self, session_id: &str, local_id: i64, value: &str) -> Result<Outcome, BrowserError> {
        let script = format!(
            "((id, value) => {{ const e = window.__fastbrowse?.nodes.get(id); \
             if (!e?.isConnected) return null; \
             const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set; \
             setter.call(e, value); \
             e.dispatchEvent(new Event('input', {{bubbles: true}})); \
             e.dispatchEvent(new Event('change', {{bubbles: true}})); \
             return e.value === value; }})({local_id}, {})",
            py_json(&json!(value))
        );
        let result = self.session.input_window(session_id, self.evaluate(session_id, &script)).await?;
        Ok(match result {
            Value::Null => (StepOutcome::Stale, Some("target disconnected".into())),
            Value::Bool(true) => (StepOutcome::Executed, None),
            _ => (StepOutcome::Failed, Some("field did not retain the supplied ISO value".into())),
        })
    }

    /// Give an autocomplete field's suggestions time to arrive before the page is read.
    ///
    /// Typing settles the fingerprint immediately, but the suggestions it asks for arrive over the network afterwards,
    /// so the snapshot caught an empty popup and Jev was asked to choose from a list that had not loaded. That is what
    /// stalled `wiki-godel` at an uncertain next step with the query typed and nothing chosen. The wait is one round
    /// trip, resolves as soon as options appear and returns at once for a field that has no popup to wait for.
    ///
    /// The wait is best effort by construction. Typing into a search box can submit it, and a navigation discards the
    /// renderer promise awaited here, so a fill that worked would fail on the wait that was only there to help it. A
    /// page that moved on has answered the question.
    async fn await_suggestions(&self, session_id: &str, local_id: i64) {
        // A popup that is open is not a popup that is populated, and an empty one is exactly what is waited out, so
        // expansion on its own does not end the wait.
        let script = format!(
            "new Promise(resolve => {{ const e = window.__fastbrowse?.nodes.get({local_id}); \
             const owned = () => {{ const id = e.getAttribute('aria-controls') || e.getAttribute('aria-owns'); \
             return id ? e.ownerDocument.getElementById(id) : null; }}; \
             const expects = !!e && (e.getAttribute('role') === 'combobox' || e.type === 'search' \
             || !!e.getAttribute('aria-autocomplete') || !!owned()); \
             if (!expects) {{ resolve(false); return; }} \
             const listed = () => !!owned()?.querySelector('[role=\"option\"], li, td') \
             || !!e.ownerDocument.querySelector('[role=\"listbox\"] [role=\"option\"]'); \
             if (listed()) {{ resolve(true); return; }} \
             const stop = ok => {{ observer.disconnect(); clearTimeout(timer); resolve(ok); }}; \
             const observer = new MutationObserver(() => {{ if (listed()) stop(true); }}); \
             observer.observe(e.ownerDocument.body, {{childList: true, subtree: true, attributes: true, \
             attributeFilter: ['aria-expanded']}}); \
             const timer = setTimeout(() => stop(false), {}); }})",
            py_float(SUGGESTION_SECONDS * 1000.0)
        );
        let _ = self.evaluate(session_id, &script).await;
    }

    async fn select(&self, target: Option<&Target>, option: &str, point: Point) -> Result<Outcome, BrowserError> {
        let Some(target) = target else { return Ok((StepOutcome::Failed, Some("select requires a target".into()))) };
        let (session_id, local_id) = (target.0.as_str(), target.2);
        match point {
            Point::Gone | Point::Unsettled => return Ok((StepOutcome::Stale, Some("target disconnected".into()))),
            // The choice is set without the pointer, so only a cover the page means to be dealt with first refuses it.
            Point::Covered => {
                if let Route::Refused(refusal) = self.route_around(target, false).await? {
                    return Ok((refusal.outcome, refusal.detail));
                }
            }
            Point::At(..) => {}
        }
        let script = format!(
            "((id, label) => {{ const e = window.__fastbrowse?.nodes.get(id); \
             if (!e?.isConnected || e.tagName !== 'SELECT') return null; \
             const opt = [...e.options].find(o => o.label === label && !o.disabled); \
             if (!opt) return false; e.value = opt.value; \
             e.dispatchEvent(new Event('input', {{bubbles: true}})); \
             e.dispatchEvent(new Event('change', {{bubbles: true}})); \
             return e.value === opt.value || (e.selectedOptions[0]?.label ?? ''); }})({local_id}, {})",
            py_json(&json!(option))
        );
        // The events are code's own, not trusted, so the window that counts a move as the run's is opened for them.
        let result = self.session.input_window(session_id, self.evaluate(session_id, &script)).await?;
        Ok(match result {
            Value::Null => (StepOutcome::Stale, Some("select target disconnected".into())),
            Value::Bool(false) => {
                (StepOutcome::Failed, Some(format!("no option labelled {}", crate::session::py_repr(option))))
            }
            Value::Bool(true) => (StepOutcome::Executed, None),
            kept => (
                StepOutcome::Failed,
                Some(format!(
                    "the page kept {} instead of {}",
                    match &kept {
                        Value::String(s) => crate::session::py_repr(s),
                        other => other.to_string(),
                    },
                    crate::session::py_repr(option)
                )),
            ),
        })
    }

    async fn key(&self, target: Option<&Target>, key: &str, point: Point) -> Result<Outcome, BrowserError> {
        let session_id = match target {
            Some(target) => target.0.clone(),
            None => self.session.active_session_id()?,
        };
        if let Some(target) = target {
            let local_id = target.2;
            match point {
                Point::Gone | Point::Unsettled => return Ok((StepOutcome::Stale, Some("target disconnected".into()))),
                // A key needs focus, not the pointer: Enter in a field under a band fixed to the window was refused.
                Point::Covered => {
                    if let Route::Refused(refusal) = self.route_around(target, false).await? {
                        return Ok((refusal.outcome, refusal.detail));
                    }
                }
                Point::At(..) => {}
            }
            if !self.focus(&session_id, local_id, false, false, false).await? {
                return Ok((StepOutcome::Failed, Some("target did not receive keyboard focus".into())));
            }
            if let Some(refusal) = self.keys_reach(target, point == Point::Covered, None).await? {
                return Ok((refusal.outcome, refusal.detail));
            }
        }
        self.press_key(&session_id, key).await?;
        Ok((StepOutcome::Executed, None))
    }

    async fn press_key(&self, session_id: &str, key: &str) -> Result<(), BrowserError> {
        // Each key pressed: its code, the text it types and its Windows virtual key code.
        let (code, text, virtual_key) = match key {
            "Enter" => ("Enter", "\r", 13),
            "Escape" => ("Escape", "", 27),
            " " => ("Space", " ", 32),
            _ => return Err(BrowserError::Failed(format!("unknown key {key}"))),
        };
        let down =
            json!({"type": "keyDown", "key": key, "code": code, "text": text, "windowsVirtualKeyCode": virtual_key});
        self.input(self.session.send("Input.dispatchKeyEvent", down, Some(session_id))).await?;
        let up = json!({"type": "keyUp", "key": key, "code": code, "windowsVirtualKeyCode": virtual_key});
        self.input(self.session.send("Input.dispatchKeyEvent", up, Some(session_id))).await
    }

    async fn scroll(&self, operation: Operation) -> Result<(), BrowserError> {
        let session_id = self.session.active_session_id()?;
        let delta = if operation == Operation::ScrollUp { -560 } else { 560 };
        let params = json!({"type": "mouseWheel", "x": 550, "y": 650, "deltaX": 0, "deltaY": delta});
        self.session.send("Input.dispatchMouseEvent", params, Some(&session_id)).await.map(drop)
    }

    pub(crate) async fn back(&self) -> Result<Outcome, BrowserError> {
        // History can change between the observation that offered BACK and this dispatch, so the predicate that gated
        // it is checked again against the live history rather than trusted from the stale observation.
        let session_id = self.session.active_session_id()?;
        let history = self.session.send("Page.getNavigationHistory", json!({}), Some(&session_id)).await?;
        match self.back_target(&history) {
            None => Ok((StepOutcome::Failed, Some("no same-origin earlier history entry".into()))),
            Some(BackTarget::Skipped(url)) => {
                // Loaded once, it is an ordinary entry, and BACK from it returns to the page the run began on.
                self.state.lock().unwrap().back_to.remove(&session_id);
                self.navigate(&url, None).await?;
                Ok((StepOutcome::Executed, None))
            }
            Some(BackTarget::Entry(entry)) => {
                self.session.send("Page.navigateToHistoryEntry", json!({"entryId": entry}), Some(&session_id)).await?;
                Ok((StepOutcome::Executed, None))
            }
        }
    }

    pub(crate) async fn upload(
        &self,
        target: Option<&Target>,
        files: &[Attachment],
        point: Point,
    ) -> Result<Outcome, BrowserError> {
        let Some(target) = target else { return Ok((StepOutcome::Failed, Some("upload requires a target".into()))) };
        if files.is_empty() {
            return Ok((StepOutcome::Failed, Some("no files supplied".into())));
        }
        let total: i64 = files.iter().map(|f| f.content.0.len() as i64).sum();
        let limit = self.config.max_upload_bytes;
        if total > limit {
            return Ok((StepOutcome::Failed, Some(format!("{total} bytes exceeds max_upload_bytes ({limit})"))));
        }
        match point {
            Point::Gone | Point::Unsettled => return Ok((StepOutcome::Stale, Some("target disconnected".into()))),
            Point::Covered => return Ok((StepOutcome::Covered, None)),
            Point::At(..) => {}
        }
        // Atomic with the write: the script checks the origin it runs in, so a document that replaced the observed one
        // between the check in `act` and here still gets no file.
        let allowed = self.session.grant_origins().map(<[String]>::to_vec);
        let payload: Vec<Value> = files
            .iter()
            .map(|f| json!({"name": f.name, "type": f.mime_type, "data": BASE64.encode(&f.content.0)}))
            .collect();
        let script = format!(
            "((id, files, allowed) => {{ const node = window.__fastbrowse?.nodes.get(id); \
             const e = node?.tagName === 'LABEL' ? node.control : node; \
             if (!e?.isConnected || e.type !== 'file' || e.disabled || \
             (allowed && !allowed.includes(location.origin))) return null; \
             const dt = new DataTransfer(); \
             for (const f of files) {{ const bin = atob(f.data); \
             const bytes = Uint8Array.from(bin, c => c.charCodeAt(0)); \
             dt.items.add(new File([bytes], f.name, {{type: f.type}})); }} \
             e.files = dt.files; \
             e.dispatchEvent(new Event('input', {{bubbles: true}})); \
             e.dispatchEvent(new Event('change', {{bubbles: true}})); return true; }})({}, {}, {})",
            target.2,
            py_json(&Value::Array(payload)),
            py_json(&json!(allowed))
        );
        if self.evaluate(&target.0, &script).await?.is_null() {
            return Ok((StepOutcome::Stale, Some("upload target disconnected".into())));
        }
        Ok((StepOutcome::Executed, None))
    }

    /// Move the pointer to a point measured in the DOM, once the browser routes input by the same layout.
    ///
    /// Chrome sends input to a frame by the last frame the page drew, not by the DOM. A fill that scrolled a
    /// cross-origin frame into view left the drawn layout a frame behind, so the click meant for the button below it
    /// went to the old position and the popup it opens never appeared.
    async fn move_to(&self, session_id: &str, (x, y): (f64, f64), drawn: bool) -> Result<(), BrowserError> {
        if !drawn && let Ok(active) = self.session.active_session_id() {
            let _ = timeout(secs(0.5), self.evaluate(&active, PRESENTED_JS)).await;
        }
        let params = json!({"type": "mouseMoved", "x": x, "y": y});
        self.input(self.session.send("Input.dispatchMouseEvent", params, Some(session_id))).await
    }

    async fn click_point(
        &self,
        target: &Target,
        point: (f64, f64),
        prepare_fill: bool,
        drawn: bool,
    ) -> Result<Outcome, BrowserError> {
        let (outcome, detail, (x, y)) = self.stable_point(target, point, prepare_fill, true, drawn).await?;
        if outcome != StepOutcome::Executed {
            return Ok((outcome, detail));
        }
        for kind in ["mousePressed", "mouseReleased"] {
            let params = json!({"type": kind, "x": x, "y": y, "button": "left", "clickCount": 1});
            self.input(self.session.send("Input.dispatchMouseEvent", params, Some(&target.0))).await?;
        }
        Ok((StepOutcome::Executed, None))
    }

    async fn stable_point(
        &self,
        target: &Target,
        mut point: (f64, f64),
        prepare_fill: bool,
        scroll: bool,
        mut drawn: bool,
    ) -> Result<(StepOutcome, Option<String>, (f64, f64)), BrowserError> {
        // Pointer entry can move, cover or repurpose a widget. Press only after the same guarded target stays under the
        // pointer across rendered frames; a moving target never earns a speculative click.
        let deadline = Instant::now() + secs(TARGET_STABILITY_SECONDS);
        loop {
            // Only the first point was measured in a call that waited for its frame; a later one may have scrolled.
            self.move_to(&target.0, point, drawn).await?;
            drawn = false;
            let check = self.before_action(Some(target), true, prepare_fill, true, scroll, false);
            let checked = tokio::select! {
                checked = check => {
                    if self.session.pending_dialog().is_some() {
                        None
                    } else {
                        Some(checked)
                    }
                }
                () = self.session.wait_for_dialog() => None,
            };
            let Some(checked) = checked else {
                return Ok((StepOutcome::Failed, Some("pointer movement opened a dialog before press".into()), point));
            };
            let (_, guard, fresh, _, _) = checked?;
            if guard != target.3 || fresh == Point::Gone {
                return Ok((StepOutcome::Stale, Some("control changed before pointer press".into()), point));
            }
            let Point::At(x, y) = fresh else { return Ok((StepOutcome::Covered, None, point)) };
            if (x, y) == point {
                return Ok((StepOutcome::Executed, None, point));
            }
            if Instant::now() >= deadline {
                return Ok((StepOutcome::Stale, Some("target did not stop moving before pointer press".into()), point));
            }
            point = (x, y);
        }
    }

    /// Drag the source onto the drop target with real pointer input: press, a path of moves, release.
    ///
    /// The travel is what makes it a drag and not a click at the source. HTML5 drag-and-drop and a page's own pointer
    /// handlers both start only after the pointer moves with the button held, and a single jump to the target is read
    /// as a teleport rather than a drag. Chrome turns this same held-button path into a native drag when the element is
    /// `draggable`, so one implementation drives both kinds of drag.
    async fn drag(
        &self,
        target: Option<&Target>,
        point: Point,
        destination_id: Option<&str>,
    ) -> Result<Outcome, BrowserError> {
        let Some(target) = target else { return Ok((StepOutcome::Failed, Some("drag requires a source".into()))) };
        let Some(destination_id) = destination_id else {
            return Ok((StepOutcome::Failed, Some("drag requires a drop target".into())));
        };
        let point = match point {
            Point::Gone | Point::Unsettled => return Ok((StepOutcome::Stale, Some("drag source disconnected".into()))),
            Point::Covered => return Ok((StepOutcome::Covered, None)),
            Point::At(x, y) => (x, y),
        };
        let Some(drop) = self.last().and_then(|last| last.controls.get(destination_id).cloned()) else {
            return Ok((StepOutcome::Stale, Some("unknown drop target id".into())));
        };
        if drop.0 != target.0 {
            return Ok((StepOutcome::Failed, Some("drag across browser frame sessions is not supported".into())));
        }
        // Both ends at one point is a press and release with no travel, which Chrome delivers as a click.
        if drop == *target {
            return Ok((StepOutcome::Failed, Some("drag source and drop target are the same control".into())));
        }
        // The drop target is hit-tested exactly as the source is: its point is only computed when the live guard still
        // matches the observed one, so a target that moved or was covered since the observation is stale.
        let (_, _, drop_point, _, _) = self.before_action(Some(&drop), true, false, false, true, false).await?;
        if !matches!(drop_point, Point::At(..)) {
            return Ok((StepOutcome::Stale, Some("drop target disconnected".into())));
        }
        // Finding the destination can scroll the source away. Scrolling back would invalidate the drop point.
        let (outcome, detail, (start_x, start_y)) = self.stable_point(target, point, false, false, false).await?;
        if outcome != StepOutcome::Executed {
            return Ok((outcome, detail));
        }
        // Pointer entry on the source can repurpose the destination after its first guard check.
        let (_, _, drop_point, _, _) = self.before_action(Some(&drop), true, false, false, false, false).await?;
        let Point::At(end_x, end_y) = drop_point else {
            return Ok((StepOutcome::Stale, Some("drop target changed before pointer press".into())));
        };
        let session_id = target.0.as_str();
        let press = json!({"type": "mousePressed", "x": start_x, "y": start_y, "button": "left", "clickCount": 1, "buttons": 1});
        self.input(self.session.send("Input.dispatchMouseEvent", press, Some(session_id))).await?;
        // A press with no travel is a click; the drag starts on the first move that clears Chrome's threshold.
        sleep(secs(DRAG_STEP_SECONDS)).await;
        // A drag handler can repurpose or cover the destination once the gesture starts, and again as the pointer
        // arrives, so the drop is made only on the control that was checked; any other change cancels the drag.
        for step in 1..=DRAG_STEPS {
            let fraction = f64::from(step) / f64::from(DRAG_STEPS);
            self.drag_move(session_id, start_x + (end_x - start_x) * fraction, start_y + (end_y - start_y) * fraction)
                .await?;
            sleep(secs(DRAG_STEP_SECONDS)).await;
            if step == 1 {
                let (_, _, landed, _, _) = self.before_action(Some(&drop), true, false, false, false, false).await?;
                if !matches!(landed, Point::At(..)) {
                    self.cancel_drag(session_id, (start_x, start_y)).await?;
                    return Ok((StepOutcome::Stale, Some("drop target changed during the drag".into())));
                }
            }
        }
        // Hover styling can shift the destination's point (a border, a nudge, a placeholder above it). The release
        // follows the control, but only to a point the hit test confirms with the pointer already resting on it.
        let mut at = (end_x, end_y);
        for _ in 0..2 {
            // Chrome drops on the target its last dragover reached, not on what lies under the pointer now, and takes a
            // newly entered target only after the dragover a further move sends. Two moves at rest bring Chrome's target
            // up to date before the hit test, or a control that slid back under the pointer since the last move would
            // pass the check while the drop went to the one hovered before it.
            for _ in 0..2 {
                self.drag_move(session_id, at.0, at.1).await?;
                sleep(secs(DRAG_STEP_SECONDS)).await;
            }
            let (_, _, landed, _, _) = self.before_action(Some(&drop), true, false, false, false, false).await?;
            let Point::At(x, y) = landed else { break };
            if (x, y) == at {
                self.release(session_id, at.0, at.1).await?;
                return Ok((StepOutcome::Executed, None));
            }
            at = (x, y);
        }
        self.cancel_drag(session_id, (start_x, start_y)).await?;
        Ok((StepOutcome::Stale, Some("drop target changed during the drag".into())))
    }

    /// Abandon a drag in progress without dropping it anywhere.
    ///
    /// Chrome cancels a native drag outright, so no drop event fires; a page's own pointer drag has no such cancel, and
    /// Escape before letting go where the card was picked up is the convention those libraries honour.
    async fn cancel_drag(&self, session_id: &str, start: (f64, f64)) -> Result<(), BrowserError> {
        self.drag_move(session_id, start.0, start.1).await?;
        self.input(self.session.send("Input.cancelDragging", json!({}), Some(session_id))).await?;
        self.key(None, "Escape", Point::Gone).await?;
        self.release(session_id, start.0, start.1).await
    }

    async fn drag_move(&self, session_id: &str, x: f64, y: f64) -> Result<(), BrowserError> {
        let params = json!({"type": "mouseMoved", "x": x, "y": y, "button": "left", "buttons": 1});
        self.input(self.session.send("Input.dispatchMouseEvent", params, Some(session_id))).await
    }

    async fn release(&self, session_id: &str, x: f64, y: f64) -> Result<(), BrowserError> {
        let params = json!({"type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": 1, "buttons": 0});
        self.input(self.session.send("Input.dispatchMouseEvent", params, Some(session_id))).await
    }

    /// Dispatch an input event without waiting on a handler that a JavaScript dialog is blocking.
    ///
    /// CDP answers an input event only after the page's handlers return, and `confirm()` inside a handler does not
    /// return until the dialog is handled; the event has been delivered either way. Dropping the wait for the answer
    /// does not undo an event already delivered to a dialog handler.
    pub(crate) async fn input(
        &self,
        send: impl Future<Output = Result<Value, BrowserError>>,
    ) -> Result<(), BrowserError> {
        tokio::select! {
            sent = send => sent.map(drop),
            () = self.session.wait_for_dialog() => Ok(()),
        }
    }

    async fn focus(
        &self,
        session_id: &str,
        local_id: i64,
        prepare_fill: bool,
        secret: bool,
        activate: bool,
    ) -> Result<bool, BrowserError> {
        // A pointer click normally focused the document already; background tabs still need activation.
        let script = focus_script(prepare_fill, secret);
        if activate {
            self.session.bring_to_front().await?;
        }
        let mut result =
            self.evaluate(session_id, &format!("{script}({local_id}, {})", py_json(&json!(activate)))).await?;
        if result.is_null() {
            self.session.bring_to_front().await?;
            result = self.evaluate(session_id, &format!("{script}({local_id}, true)")).await?;
        }
        Ok(truthy(&result))
    }

    pub(crate) async fn before_action(
        &self,
        target: Option<&Target>,
        hit_test: bool,
        prepare_fill: bool,
        after_move: bool,
        scroll: bool,
        present: bool,
    ) -> Result<TargetState, BrowserError> {
        let Some(target) = target else { return Ok((self.fingerprint().await?, None, Point::Gone, false, false)) };
        let (session_id, local_id, guard) = (&target.0, target.2, &target.3);
        let active = self.session.active_session_id()?;
        let same_session = *session_id == active;
        // Input routing uses the top frame's drawn layout. An OOPIF cannot wait for that frame in its own renderer, so
        // only that path needs a separate round trip before checking the target.
        if after_move && !same_session {
            self.evaluate(&active, PRESENTED_JS).await?;
        }
        // Guard validation and hit testing share a renderer task, so no page script can swap the verified control
        // between them. Stale controls must never be scrolled into view.
        let mut script = String::from("(async () => { ");
        if after_move && same_session {
            script.push_str(&format!("await {PRESENTED_JS}; "));
        }
        script.push_str("const r = window.__fastbrowse; ");
        let fingerprint = if same_session && !after_move { self.fingerprint_js.as_str() } else { "''" };
        script.push_str(&format!("const fingerprint = {fingerprint}; "));
        script.push_str(&format!("const guard = r?.guardKey ? r.guardKey(r.nodes.get({local_id})) : null; "));
        script.push_str(&format!(
            "const point = {} && guard === {} ? ({HIT_TEST_JS})({local_id}, {}) : null; ",
            py_json(&json!(hit_test)),
            py_json(&json!(guard)),
            py_json(&json!(scroll))
        ));
        if prepare_fill {
            script.push_str(&format!(
                "if (Array.isArray(point)) {{ const e = r.nodes.get({local_id}); \
                 const rect = e.getBoundingClientRect(); \
                 r.handoff = {{doc: e.ownerDocument, root: e.getRootNode(), \
                 x: rect.x + rect.width / 2, y: rect.y + rect.height / 2}}; }} "
            ));
        }
        if present && same_session && !after_move {
            script.push_str(&format!("if (Array.isArray(point)) await {PRESENTED_JS}; "));
        }
        script.push_str(&format!(
            "const e = r?.nodes.get({local_id}); \
             const announced = !!e && e.getAttribute('aria-expanded') !== 'true' && \
             (!!e.getAttribute('aria-haspopup') || e.getAttribute('aria-expanded') === 'false' || \
             !!e.getAttribute('aria-controls') || !!e.getAttribute('aria-owns')); \
             const fresh = name => (name ?? '').trim().toLowerCase() === '_blank'; \
             const link = e?.closest('a[href],area[href]'); \
             const opens = !!e && ((!!link && fresh(link.hasAttribute('target') ? link.getAttribute('target') \
             : e.ownerDocument.querySelector('base[target]')?.getAttribute('target'))) || \
             (!!e.form && ['submit', 'image'].includes(e.type) && \
             fresh(e.getAttribute('formtarget') ?? e.form.getAttribute('target')))); \
             return [fingerprint, guard, point, announced, opens]; }})()"
        ));
        let state = async { parse_target_state(&self.evaluate(session_id, &script).await?) };
        if same_session || after_move {
            return state.await;
        }
        let (fingerprint, (_, live_guard, point, announced, opens)) = tokio::try_join!(self.fingerprint(), state)?;
        Ok((fingerprint, live_guard, point, announced, opens))
    }
}

/// `_landed_js`. The text can land in another field than ours: a framework may swap ours for a hydrated copy while the
/// text is inserted, or focusing ours opens an editor over it that takes focus, a moment too late for the hand-off to
/// see. That field is accepted only when it is focused in the same document and covers the point ours occupied: a field
/// elsewhere holding the same text is not evidence that ours took it.
fn landed_js(local_id: i64, text: &str) -> String {
    format!(
        "((e, text) => {{ const holds = n => !!n && (n.value ?? n.innerText) === text; \
         if (e?.isConnected && holds(e)) return true; \
         const was = window.__fastbrowse?.filled; if (!was) return false; \
         const now = was.doc.activeElement; if (!now || now === e) return false; \
         const r = now.getBoundingClientRect(); \
         const inPlace = was.x >= r.left && was.x <= r.right && was.y >= r.top && was.y <= r.bottom; \
         return inPlace && holds(now); }})(window.__fastbrowse?.nodes.get({local_id}), {})",
        py_json(&json!(text))
    )
}

/// `_focus_script`. Hydration can replace the field during insertion, so its position is kept to verify that the
/// replacement occupies the same place, rather than accepting a different field with the same text.
fn focus_script(prepare_fill: bool, secret: bool) -> String {
    let mask = if secret {
        "e.dataset.fastbrowseSecret = '1'; e.style.setProperty('-webkit-text-security', 'disc', 'important'); "
    } else {
        ""
    };
    let prepare = if prepare_fill {
        format!(
            "const r = e.getBoundingClientRect(); window.__fastbrowse.filled = \
             {{doc: e.ownerDocument, x: r.x + r.width / 2, y: r.y + r.height / 2}}; {}",
            if secret { "" } else { SELECT_TEXT_JS }
        )
    } else {
        String::new()
    };
    // activeElement is set by focus() before it returns, but hasFocus() is answered by the browser's focus controller,
    // which does not run inside the task that called focus(). For a field inside an iframe it therefore reads false for
    // a tick or two, so judging it here in the same task rejects a field that is in fact focused. Poll instead of
    // guessing. Every condition is rechecked on the tick that succeeds: waiting for the focus signal means focus can
    // move meanwhile, and reporting success on a stale activeElement would authorize keystrokes to whatever holds focus.
    format!(
        "((id, activated) => {{ const e = window.__fastbrowse?.nodes.get(id); if (!e?.isConnected) return false; \
         if (!activated && !e.ownerDocument.hasFocus()) return null; \
         {mask}e.ownerDocument.defaultView.focus(); e.focus({{preventScroll: true}}); \
         if (!e.isConnected || e.getRootNode().activeElement !== e) return false; \
         return new Promise(resolve => {{ \
         const deadline = Date.now() + {deadline}; \
         const check = () => {{ if (!e.isConnected || e.getRootNode().activeElement !== e) \
         {{ resolve(false); return; }} \
         if (e.ownerDocument.hasFocus()) {{ {prepare}resolve(true); return; }} \
         if (Date.now() > deadline) {{ resolve(false); return; }} \
         setTimeout(check, 10); }}; check(); }}); }})",
        deadline = py_float(FOCUS_SETTLE_SECONDS * 1000.0),
    )
}

/// What a person reads as closing a cover rather than following it, judged by its label alone:
/// `\b(close|dismiss|skip|hide|no,? thanks|not now)\b|^\s*[x\u00d7\u2715\u2716]\s*$`, ignoring case.
fn dismisses(label: &str) -> bool {
    let trimmed = label.trim();
    if trimmed.chars().count() == 1
        && matches!(trimmed.chars().next(), Some('x' | 'X' | '\u{d7}' | '\u{2715}' | '\u{2716}'))
    {
        return true;
    }
    let lower = label.to_lowercase();
    let word = |c: char| c.is_alphanumeric() || c == '_';
    ["close", "dismiss", "skip", "hide", "no thanks", "no, thanks", "not now"].iter().any(|phrase| {
        lower.match_indices(phrase).any(|(at, _)| {
            let before = lower[..at].chars().next_back();
            let after = lower[at + phrase.len()..].chars().next();
            before.is_none_or(|c| !word(c)) && after.is_none_or(|c| !word(c))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dismiss_labels_match_the_python_pattern() {
        for label in ["Close", "close ad", "No thanks", "no, thanks!", "Not now", " x ", "\u{d7}", "Skip"] {
            assert!(dismisses(label), "{label}");
        }
        for label in ["Closed", "Shop now", "xx", "Hidden gems"] {
            assert!(!dismisses(label), "{label}");
        }
    }

    #[test]
    fn points_parse_as_the_type_adapters_do() {
        assert_eq!(parse_point(&json!([1, 2.5]), false).unwrap(), Point::At(1.0, 2.5));
        assert_eq!(parse_point(&json!("covered"), false).unwrap(), Point::Covered);
        assert!(parse_point(&json!("unsettled"), false).is_err());
        assert_eq!(parse_point(&json!("unsettled"), true).unwrap(), Point::Unsettled);
        assert_eq!(parse_point(&Value::Null, false).unwrap(), Point::Gone);
    }
}
