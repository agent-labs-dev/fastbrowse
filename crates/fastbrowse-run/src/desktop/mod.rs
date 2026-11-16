//! A native desktop app as a `Page`, driven through deskpilot (<https://github.com/cjber/deskpilot>).
//!
//! The run starts the app itself with deskpilot's `launch`, as a deskpilot agent of its own, so the window opens on
//! that agent's hidden workspace and closes when the run's `deskpilot mcp` ends. deskpilot reads the window's
//! accessibility tree and sends its input in hidden mode, which it allows only on a window its agent launched: to the
//! window wherever it is, without moving the user's pointer or keyboard focus. Each observation is one deskpilot
//! snapshot, whose element ids deskpilot itself refuses once a newer snapshot replaces it or the element changed, so
//! `act` needs no guard of its own beyond naming the snapshot it was offered.

mod mcp;
pub mod tree;

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use fastbrowse_core::BoxFuture;
use fastbrowse_core::models::{Artifact, Operation, StepOutcome};
use fastbrowse_core::page::{ActResult, Action, BrowserError, Capture, Observation, Page, PageResult};
use fastbrowse_core::safety::origin_of;

pub use mcp::{Deskpilot, ToolError};
use tree::{Element, Window};

/// How long an action's effect is waited for before it is reported as having changed nothing.
const SETTLE_SECONDS: f64 = 0.5;
const POLL: Duration = Duration::from_millis(100);
/// Wheel clicks one SCROLL turns.
const SCROLL_CLICKS: i64 = 5;

/// The screenshot `click_viewport` points into: deskpilot binds a point to the shot it was read from.
#[derive(Clone)]
struct Shot {
    id: String,
    width: f64,
    height: f64,
}

/// Why a desktop page could not be opened: an app the caller has to fix, or a deskpilot that cannot run.
#[derive(Debug)]
pub enum OpenError {
    Invalid(String),
    Unavailable(String),
}

pub struct DeskpilotPage {
    client: Deskpilot,
    window: Mutex<Window>,
    /// The snapshot the latest observation took, the only one whose ids may be acted on.
    latest: Mutex<Option<String>>,
    shot: Mutex<Option<Shot>>,
    generation: AtomicU64,
}

fn unavailable(error: ToolError) -> BrowserError {
    BrowserError::Unavailable(format!("deskpilot: {}", error.message()))
}

fn read_window(value: &Value) -> Option<Window> {
    let field = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_string);
    Some(Window { address: field("address")?, class: field("class")?, title: field("title").unwrap_or_default() })
}

impl DeskpilotPage {
    /// Starts deskpilot as an agent of this page's own and launches `command` (a program and its arguments, run
    /// without a shell) through it, driving the window `launch` returns. A command deskpilot cannot start, that exits
    /// before it opens a window, or opens none in time is refused here, before any step.
    pub async fn open(program: &str, command: &[String]) -> Result<DeskpilotPage, OpenError> {
        // An agent of its own, so two runs in one `fastbrowse serve` never share a hidden workspace or a cursor.
        let agent = fastbrowse_browser::cursor::deskpilot_agent();
        let client =
            Deskpilot::start(program, &agent).await.map_err(|error| OpenError::Unavailable(error.message().into()))?;
        if !client.offers("launch") {
            return Err(OpenError::Unavailable(
                "this deskpilot has no launch tool; a desktop run needs a deskpilot with launch \
                 (https://github.com/cjber/deskpilot/pull/86)"
                    .into(),
            ));
        }
        let shown = command.join(" ");
        let found = client.call_json("launch", json!({"command": command})).await.map_err(|error| {
            let message = format!("could not start {shown}: {}", error.message());
            match error {
                // deskpilot's codes for a command it cannot start, that exits before a window, or opens none in time:
                // the caller's to fix.
                ToolError::Refused { code, .. } if matches!(code.as_str(), "invalid" | "refused" | "timeout") => {
                    OpenError::Invalid(message)
                }
                _ => OpenError::Unavailable(message),
            }
        })?;
        if found.get("xwayland").and_then(Value::as_bool) == Some(true) {
            return Err(OpenError::Invalid(format!(
                "{shown} opened an XWayland window; fastbrowse drives desktop windows in deskpilot's hidden mode, \
                 which needs a native Wayland window"
            )));
        }
        let window = read_window(&found).ok_or_else(|| {
            OpenError::Unavailable(format!("deskpilot described the window of {shown} without an address"))
        })?;
        Ok(DeskpilotPage {
            client,
            window: Mutex::new(window),
            latest: Mutex::new(None),
            shot: Mutex::new(None),
            generation: AtomicU64::new(0),
        })
    }

    fn address(&self) -> String {
        self.window.lock().unwrap().address.clone()
    }

    /// The window as it is now: its title follows the app's state. A window that closed ends the run's page.
    async fn refresh(&self) -> PageResult<Window> {
        let address = self.address();
        let found =
            self.client.call_json("wait_for", json!({"window": address, "timeout": 0})).await.map_err(|error| {
                BrowserError::Unavailable(format!("the desktop window {address} is gone: {}", error.message()))
            })?;
        let window = read_window(&found).ok_or_else(|| BrowserError::Failed("deskpilot described no window".into()))?;
        *self.window.lock().unwrap() = window.clone();
        Ok(window)
    }

    /// The window's tree. A peek reads it without replacing the snapshot the latest observation's ids belong to.
    async fn tree(&self, peek: bool) -> PageResult<(Window, Element)> {
        let window = self.refresh().await?;
        let value = self
            .client
            .call_json("elements", json!({"window": window.address, "hidden": true, "peek": peek}))
            .await
            .map_err(|error| match error {
                ToolError::Refused { code, message } if code == "no_tree" => BrowserError::Failed(message),
                error => unavailable(error),
            })?;
        let root: Element = serde_json::from_value(value)
            .map_err(|error| BrowserError::Failed(format!("deskpilot sent a tree fastbrowse cannot read: {error}")))?;
        Ok((window, tree::in_reading_order(root)))
    }

    /// Whether the window now differs from `observation`, in `target_id` alone when one is given, read without
    /// replacing its snapshot. Ids are numbered by the tree's order, so a tree that changed shape renumbers them and
    /// counts as changed.
    async fn differs(&self, observation: &Observation, target_id: Option<&str>) -> PageResult<bool> {
        let (window, root) = self.tree(true).await?;
        let now = tree::observation(&window, &root);
        Ok(match target_id {
            Some(id) => {
                let find = |o: &Observation| o.controls.iter().find(|c| c.id == id).cloned();
                find(observation) != find(&now)
            }
            None => {
                now.controls != observation.controls
                    || now.viewport_text != observation.viewport_text
                    || now.offscreen_text != observation.offscreen_text
                    || now.title != observation.title
            }
        })
    }

    /// Waits up to `seconds` for the window to differ from `observation`, and once it does, for it to stop changing.
    async fn changed_within(
        &self,
        observation: &Observation,
        seconds: f64,
        target_id: Option<&str>,
    ) -> PageResult<bool> {
        let deadline = seconds.is_finite().then(|| Instant::now() + Duration::from_secs_f64(seconds.max(0.0)));
        loop {
            if self.differs(observation, target_id).await? {
                // A continuously updating window must not spend the run settling after the wait found its change.
                let settle = Instant::now() + Duration::from_secs_f64(SETTLE_SECONDS);
                self.settle(deadline.map_or(settle, |deadline| deadline.min(settle))).await?;
                return Ok(true);
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(false);
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Waits until two reads of the tree agree, or the deadline.
    async fn settle(&self, deadline: Instant) -> PageResult<()> {
        let mut last = self.tree(true).await?.1;
        while Instant::now() < deadline {
            tokio::time::sleep(POLL).await;
            let now = self.tree(true).await?.1;
            if now == last {
                break;
            }
            last = now;
        }
        Ok(())
    }

    /// Calls an input tool, in hidden mode wherever the connected deskpilot's schema offers it for that tool.
    async fn tool(&self, name: &str, arguments: Value) -> Result<(), ToolError> {
        self.client.call(name, self.client.hidden(name, arguments)).await.map(|_| ())
    }

    async fn dispatch(&self, action: &Action, observation: &Observation) -> Result<Option<String>, ToolError> {
        let window = self.address();
        // The id as the observation's snapshot numbered it: deskpilot refuses it if that snapshot was replaced or the
        // element changed since.
        let bound = action.target_id.as_deref().map(|id| format!("{}:{id}", observation.page_key));
        let target = bound.as_deref();
        match action.operation {
            Operation::Click => {
                self.tool("click", json!({"window": window, "element": target, "hidden": true})).await?
            }
            Operation::Hover => {
                self.tool("hover", json!({"window": window, "element": target, "hidden": true, "wait": SETTLE_SECONDS}))
                    .await?
            }
            Operation::Fill => {
                let text = action.text.as_deref().unwrap_or_default();
                if action.secret && action.secret_origin.as_deref() != Some(origin_of(&observation.url).as_str()) {
                    return Ok(Some("the secret is not for this app".into()));
                }
                // The click focuses the field; everything after it goes to that field's window and replaces its text.
                self.tool("click", json!({"window": window, "element": target, "hidden": true})).await?;
                self.tool("key", json!({"combo": "ctrl+a", "window": window})).await?;
                if text.is_empty() {
                    self.tool("key", json!({"combo": "BackSpace", "window": window})).await?;
                } else {
                    // Typed as a person types, so the field takes the value as it takes theirs: a spin button commits
                    // typed digits and not pasted ones. Text the keymap cannot type is pasted instead, from the
                    // window's own clipboard, never the user's; selecting all first replaces whatever was typed.
                    // deskpilot refuses untypable text with its catch-all `refused`, before any key; a window that
                    // went away (`gone`) or a plugin that stopped (`unavailable`) is reported, not pasted into.
                    match self.tool("type_text", json!({"text": text, "window": window})).await {
                        Err(ToolError::Refused { code, .. }) if code == "refused" => {
                            self.tool("key", json!({"combo": "ctrl+a", "window": window})).await?;
                            self.tool("paste_text", json!({"text": text, "window": window})).await?;
                        }
                        typed => typed?,
                    }
                }
            }
            Operation::Enter => {
                let focused = observation
                    .controls
                    .iter()
                    .any(|c| Some(c.id.as_str()) == action.target_id.as_deref() && c.focused == Some(true));
                if target.is_some() && !focused {
                    self.tool("click", json!({"window": window, "element": target, "hidden": true})).await?;
                }
                self.tool("key", json!({"combo": "Return", "window": window})).await?;
            }
            Operation::Press => {
                let focused = observation
                    .controls
                    .iter()
                    .any(|c| Some(c.id.as_str()) == action.target_id.as_deref() && c.focused == Some(true));
                if target.is_some() && !focused {
                    self.tool("click", json!({"window": window, "element": target, "hidden": true})).await?;
                }
                let Some(combo) = action.text.as_deref().filter(|combo| !combo.is_empty()) else {
                    return Ok(Some("press needs a key or key chord".into()));
                };
                self.tool("key", json!({"combo": combo, "window": window})).await?;
            }
            Operation::Escape => self.tool("key", json!({"combo": "Escape", "window": window})).await?,
            Operation::Scroll | Operation::ScrollUp => {
                // The root is the window itself, so with no target the scroll lands in its middle.
                let at = target.map(str::to_string).unwrap_or_else(|| format!("{}:e1", observation.page_key));
                let dy = if action.operation == Operation::Scroll { -SCROLL_CLICKS } else { SCROLL_CLICKS };
                self.tool("scroll", json!({"window": window, "element": at, "dy": dy, "hidden": true})).await?
            }
            Operation::Select => {
                let wanted = action.text.as_deref().unwrap_or_default();
                let Some(id) = action.target_id.as_deref() else { return Ok(Some("select needs a target".into())) };
                let control = observation.controls.iter().find(|c| c.id == id);
                let offered = control.map(|c| c.options.as_slice()).unwrap_or_default();
                if !offered.iter().any(|option| option == wanted) {
                    return Ok(Some(format!("no option labelled {wanted:?}")));
                }
                // The click gives the combo box the window's keyboard focus.
                self.tool("click", json!({"window": window, "element": target, "hidden": true})).await?;
                return self.choose(&window, id, wanted, offered).await;
            }
            Operation::Wait => {}
            other => return Ok(Some(format!("{} is not something a desktop window can do", other.as_str()))),
        }
        Ok(None)
    }

    /// The window's tree, read without replacing the snapshot.
    async fn peeked(&self, window: &str) -> Result<Element, ToolError> {
        let value = self.client.call_json("elements", json!({"window": window, "hidden": true, "peek": true})).await?;
        serde_json::from_value(value)
            .map_err(|error| ToolError::Transport(format!("deskpilot sent a tree fastbrowse cannot read: {error}")))
    }

    /// The item combo box `id` has chosen, and whether its list is open, as the window says now.
    async fn choice(&self, window: &str, id: &str) -> Result<Option<(Option<String>, bool)>, ToolError> {
        let root = self.peeked(window).await?;
        Ok(tree::find(&root, id).map(|element| (tree::chosen(element).map(str::to_string), tree::list_open(element))))
    }

    /// Moves a focused combo box's choice to `wanted` one arrow key at a time, as a person picks in a list they
    /// cannot see, reading the choice after each key. Its list is a popup that cannot be clicked in the background,
    /// and while it is open the keys would move only a highlight, so an open list is closed first; the box keeps the
    /// focus. The keys go the way `offered` places `wanted` from the current choice, and turn back once at the end of
    /// the list, so an order that differs from the one the keys follow still reaches it.
    async fn choose(
        &self,
        window: &str,
        id: &str,
        wanted: &str,
        offered: &[String],
    ) -> Result<Option<String>, ToolError> {
        let gone = || Ok(Some("the combo box is gone".to_string()));
        let Some((mut now, open)) = self.choice(window, id).await? else { return gone() };
        if open {
            self.tool("key", json!({"combo": "Escape", "window": window})).await?;
            let Some((after, _)) = self.choice(window, id).await? else { return gone() };
            now = after;
        }
        let at = |name: &str| offered.iter().position(|option| option == name);
        let mut down = now.as_deref().and_then(at).is_none_or(|from| at(wanted).is_some_and(|to| from < to));
        let mut turned = false;
        for _ in 0..2 * offered.len() {
            if now.as_deref() == Some(wanted) {
                return Ok(None);
            }
            self.tool("key", json!({"combo": if down { "Down" } else { "Up" }, "window": window})).await?;
            // The app reports the new choice a moment after the key; a key at the end of the list changes nothing.
            let deadline = Instant::now() + Duration::from_secs_f64(SETTLE_SECONDS);
            let after = loop {
                let Some((after, _)) = self.choice(window, id).await? else { return gone() };
                if after != now || Instant::now() >= deadline {
                    break after;
                }
                tokio::time::sleep(POLL).await;
            };
            if after == now {
                if turned {
                    break;
                }
                (turned, down) = (true, !down);
            }
            now = after;
        }
        if now.as_deref() == Some(wanted) {
            return Ok(None);
        }
        Ok(Some(format!(
            "the combo box kept {} instead of {wanted:?}",
            now.map_or("nothing".into(), |n| format!("{n:?}"))
        )))
    }

    async fn measured(
        &self,
        sent: Result<Option<String>, ToolError>,
        observation: &Observation,
        seconds: f64,
    ) -> PageResult<ActResult> {
        let outcome =
            |outcome, detail: Option<String>| ActResult { outcome, page_changed: false, detail, moved: false };
        match sent {
            Ok(Some(refused)) => Ok(outcome(StepOutcome::Failed, Some(refused))),
            Ok(None) => {
                self.generation.fetch_add(1, Ordering::Relaxed);
                let changed = self.changed_within(observation, seconds, None).await?;
                let detail = (!changed).then(|| "the window did not change".to_string());
                Ok(ActResult { outcome: StepOutcome::Executed, page_changed: changed, detail, moved: false })
            }
            Err(ToolError::Refused { code, message }) => match code.as_str() {
                "stale" => Ok(outcome(StepOutcome::Stale, Some(message))),
                "covered" => Ok(outcome(StepOutcome::Covered, Some(message))),
                "gone" | "not_found" => Err(BrowserError::Unavailable(message)),
                _ => Ok(outcome(StepOutcome::Failed, Some(message))),
            },
            Err(error) => Err(unavailable(error)),
        }
    }

    async fn act_within(&self, action: &Action, observation: &Observation, seconds: f64) -> PageResult<ActResult> {
        // Only the latest snapshot's ids are live; deskpilot refuses the rest too, but a glance has no snapshot.
        let latest = self.latest.lock().unwrap().clone();
        if observation.page_key.is_empty() || latest.as_deref() != Some(observation.page_key.as_str()) {
            return Ok(ActResult {
                outcome: StepOutcome::Stale,
                page_changed: false,
                detail: Some("the window was read again since this observation".to_string()),
                moved: false,
            });
        }
        let sent = self.dispatch(action, observation).await;
        self.measured(sent, observation, seconds).await
    }
}

impl Page for DeskpilotPage {
    /// Nothing is downloaded from a desktop window.
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        Box::pin(async move {
            let (window, root) = self.tree(false).await?;
            *self.latest.lock().unwrap() = root.snapshot.clone();
            Ok(tree::observation(&window, &root))
        })
    }

    /// A peek: it binds no ids and leaves the latest observation's snapshot standing.
    fn glance(&self) -> BoxFuture<'_, PageResult<Observation>> {
        Box::pin(async move {
            let (window, root) = self.tree(true).await?;
            Ok(tree::observation(&window, &root))
        })
    }

    /// The window's title names its document, as an editor's names the file it holds.
    fn document_changed<'a>(&'a self, observation: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async move { Ok(self.refresh().await?.title != observation.title) })
    }

    fn navigate<'a>(&'a self, _url: &'a str, _back_to: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        Box::pin(async { Err(BrowserError::Failed("a desktop window has no addresses to open".into())) })
    }

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        Box::pin(async move {
            let (window, root) = self.tree(true).await?;
            Ok(tree::capture(&window, &root))
        })
    }

    fn act<'a>(&'a self, action: &'a Action, observation: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        Box::pin(async move {
            let seconds = if action.operation == Operation::Wait { f64::INFINITY } else { SETTLE_SECONDS };
            self.act_within(action, observation, seconds).await
        })
    }

    fn wait<'a>(
        &'a self,
        action: &'a Action,
        observation: &'a Observation,
        seconds: f64,
    ) -> BoxFuture<'a, PageResult<ActResult>> {
        Box::pin(async move { self.act_within(action, observation, seconds).await })
    }

    fn redrawn<'a>(
        &'a self,
        observation: &'a Observation,
        timeout_seconds: f64,
        target_id: Option<&'a str>,
    ) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async move { self.changed_within(observation, timeout_seconds, target_id).await })
    }

    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        Box::pin(async move {
            let window = self.address();
            let content = self
                .client
                .call("shot", json!({"window": window, "hidden": true}))
                .await
                .map_err(|error| BrowserError::ScreenshotsUnavailable(error.message().to_string()))?;
            let taken = mcp::json_block(&content)
                .ok_or_else(|| BrowserError::ScreenshotsUnavailable("deskpilot described no shot".into()))?;
            let field = |name: &str| taken.get(name).cloned().unwrap_or(Value::Null);
            let (Some(id), Some(path), Some(width), Some(height)) = (
                field("shot").as_str().map(str::to_string),
                field("path").as_str().map(str::to_string),
                field("width").as_f64(),
                field("height").as_f64(),
            ) else {
                return Err(BrowserError::ScreenshotsUnavailable("deskpilot described its shot without a file".into()));
            };
            let png = tokio::fs::read(&path)
                .await
                .map_err(|error| BrowserError::ScreenshotsUnavailable(format!("reading deskpilot's shot: {error}")))?;
            *self.shot.lock().unwrap() = Some(Shot { id, width, height });
            Ok(png)
        })
    }

    fn click_viewport(&self, x: f64, y: f64) -> BoxFuture<'_, PageResult<ActResult>> {
        Box::pin(async move {
            let Some(shot) = self.shot.lock().unwrap().clone() else {
                return Err(BrowserError::ScreenshotsUnavailable("no screenshot of the window was taken".into()));
            };
            let before = self.glance().await?;
            let point = json!({
                "window": self.address(), "x": x * shot.width, "y": y * shot.height, "shot": shot.id, "hidden": true,
            });
            let sent = self.tool("click", point).await.map(|()| None);
            self.measured(sent, &before, SETTLE_SECONDS).await
        })
    }

    /// deskpilot films nothing, so there are no frames to hold back.
    fn withhold_frames(&self, _withheld: bool) {}

    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        Box::pin(async move { Ok(self.refresh().await?.url()) })
    }

    fn response_status(&self) -> BoxFuture<'_, PageResult<Option<i64>>> {
        Box::pin(async { Ok(None) })
    }

    fn lifecycle_mark(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// A window loads no documents, so no load event is ever coming.
    fn wait_for_lifecycle(&self, _mark: u64, _seconds: f64) -> BoxFuture<'_, bool> {
        Box::pin(async {
            tokio::task::yield_now().await;
            false
        })
    }
}
