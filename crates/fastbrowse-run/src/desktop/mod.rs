//! A native desktop app as a `Page`, driven through deskpilot (<https://github.com/cjber/deskpilot>).
//!
//! The page reads and acts on an app only through its accessibility tree, so an app that exposes none, such as a
//! terminal or an app that draws its own interface, cannot be driven: its first read fails as `deskpilot/no_tree`.
//!
//! The run starts its apps itself with deskpilot's `launch`, as a deskpilot agent of its own, so their windows open on
//! that agent's hidden workspace and close when the run closes its `deskpilot mcp`. Every window on that workspace,
//! the apps' first windows and the dialogs and windows they open later, is a tab of the page; one is active, and the
//! page reads and acts on that one. deskpilot reads the window's
//! accessibility tree and sends its input in hidden mode, which it allows only on a window its agent launched: to the
//! window wherever it is, without moving the user's pointer or keyboard focus. Each observation is one deskpilot
//! snapshot, whose element ids deskpilot itself refuses once a newer snapshot replaces it or the element changed, so
//! `act` needs no guard of its own beyond naming the snapshot it was offered.
//!
//! A filmed page shoots its window with deskpilot's `shot` when the app opens and after each action settles, and
//! hands each frame to the run's recording: deskpilot casts no live frames, and a shot is the one picture of a hidden
//! window it can take without bringing the window on screen. deskpilot's own agent cursor is never in a window shot,
//! so the page also tells the recording where each click, fill and hover landed, for it to draw the cursor there.

mod mcp;
pub mod tree;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use fastbrowse_browser::pointer::PointerMark;
use fastbrowse_core::BoxFuture;
use fastbrowse_core::models::{Artifact, Operation, StepFailure, StepOutcome};
use fastbrowse_core::page::{ActResult, Action, BrowserError, Capture, Observation, Page, PageResult};
use fastbrowse_core::safety::origin_of;
use fastbrowse_core::telemetry::Trace;

pub use mcp::{Deskpilot, ToolError, View};
use tree::{Element, Window};

/// How long an action's effect is waited for before it is reported as having changed nothing.
const SETTLE_SECONDS: f64 = 0.5;
/// How long a WAIT watches the window when the run gives it no deadline: as long as one deskpilot call may take, as a
/// browser's WAIT watches for one CDP reply. Unbounded, a run with no time limit polled a window that never changed
/// until it was killed, spending nothing, so `--max-dollars` never ended it and the stall rule never counted the wait
/// (#1406). Handed back unchanged, the step is counted, and Jev may wait again.
const WATCH_SECONDS: f64 = 60.0;

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

/// Where a filmed page sends what its recording shows.
pub trait Film: Send + Sync {
    /// A PNG of the window, in the order taken.
    fn frame(&self, png: Vec<u8>);
    /// Where an action is about to land, in the pixels of a window `mark.width` by `mark.height`.
    fn point(&self, mark: PointerMark);
}

impl Film for fastbrowse_browser::recording::FrameFeed {
    fn frame(&self, png: Vec<u8>) {
        self.show(png);
    }

    fn point(&self, mark: PointerMark) {
        self.mark(mark);
    }
}

/// Why a desktop page could not be opened: an app the caller has to fix, or a deskpilot that cannot run.
#[derive(Debug)]
pub enum OpenError {
    Invalid(String),
    Unavailable(String),
}

pub struct DeskpilotPage {
    client: Arc<Deskpilot>,
    /// The agent's hidden workspace, where every window of the run is.
    workspace: String,
    /// The active window, which the page reads and acts on.
    window: Mutex<Window>,
    /// The windows made active, last the latest: when one closes, the one before it is active again, as a dialog's
    /// window is once the dialog is dismissed.
    visited: Mutex<Vec<String>>,
    /// Every window seen on the workspace, so a window an action opened is told from one already there.
    seen: Mutex<HashSet<String>>,
    /// Set when a step's window lookup timed out, so a window it opened is not yet known: the next observation leaves
    /// the windows unseen for the next step's `follow_opened`, instead of taking the dialog for one that was there.
    lookup_missed: AtomicBool,
    /// The snapshot the latest observation took, the only one whose ids may be acted on.
    latest: Mutex<Option<String>>,
    shot: Mutex<Option<Shot>>,
    generation: AtomicU64,
    film: Option<Arc<dyn Film>>,
    /// The tree the latest observation read, where its ids' elements are.
    observed: Mutex<Option<Element>>,
    /// Set while the window may show a resolved secret, which a frame cannot mask: no frame is filmed then.
    withheld: AtomicBool,
    /// The latest frame, taken when the app opens or after an action, held until the next observation shows no
    /// secret: the action may be what put one on screen, and only that reading can tell.
    pending: Mutex<Option<Vec<u8>>>,
    /// The latest action sent since an observation last found a window open, named as a step names it: when every
    /// window is gone after it, that action is what closed the app.
    sent: Mutex<Option<String>>,
    /// Set once every window of the run closed after `sent`. A dialog closes when the run presses the button the task
    /// asked for, and calling that a lost connection ended a finished task `unavailable` before its last step was
    /// recorded (#1405); the page now shows a closed app, which Jev reads like any other state.
    closed: Mutex<Option<String>>,
    /// The size of the latest frame filmed, in pixels: where a mark lies in the film.
    framed: Mutex<Option<(f64, f64)>>,
    /// How long a WAIT with no deadline watches the window: `WATCH_SECONDS`.
    watch: f64,
    /// The run's trace, where each input sent to an element is recorded with what the page took that element to be.
    trace: Trace,
}

fn unavailable(error: ToolError) -> BrowserError {
    BrowserError::Unavailable(format!("deskpilot: {}", error.message()))
}

/// `action` as a step names it: its operation and the label of its target in `observation`.
fn step_name(action: &Action, observation: &Observation) -> String {
    let operation = action.operation.as_str().replace('_', " ");
    let target = action.target_id.as_deref().and_then(|id| observation.controls.iter().find(|c| c.id == id));
    match target {
        Some(control) => format!("{operation} {:?}", control.label),
        None => operation,
    }
}

fn read_window(value: &Value) -> Option<Window> {
    let field = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_string);
    Some(Window { address: field("address")?, class: field("class")?, title: field("title").unwrap_or_default() })
}

/// Starts `program mcp` as a deskpilot agent of the run's own: two runs in one `fastbrowse serve` never share a hidden
/// workspace or a cursor.
pub async fn connect(program: &str, view: View) -> Result<Arc<Deskpilot>, OpenError> {
    let agent = fastbrowse_browser::cursor::deskpilot_agent();
    let client = Deskpilot::start(program, &agent, view)
        .await
        .map_err(|error| OpenError::Unavailable(error.message().into()))?;
    Ok(Arc::new(client))
}

impl DeskpilotPage {
    /// Launches each of `commands` (a program and its arguments, run without a shell) through `client`, in order, and
    /// makes the first one's window active. A command deskpilot cannot start, that exits before it opens a window, or
    /// opens none in time is refused here, before any step.
    pub async fn open(
        client: Arc<Deskpilot>,
        commands: &[Vec<String>],
        view: View,
    ) -> Result<DeskpilotPage, OpenError> {
        if !client.offers("launch") {
            return Err(OpenError::Unavailable(
                "this deskpilot has no launch tool; a desktop run needs a deskpilot with launch \
                 (https://github.com/cjber/deskpilot/pull/86)"
                    .into(),
            ));
        }
        let mut launched = Vec::new();
        let mut workspace = None;
        for command in commands {
            let shown = command.join(" ");
            let found = client.call_json("launch", json!({"command": command})).await.map_err(|error| {
                let message = format!("could not start {shown}: {}", error.message());
                match error {
                    // deskpilot's codes for a command it cannot start, that exits before a window, or opens none in
                    // time: the caller's to fix.
                    ToolError::Refused { code, .. } if matches!(code.as_str(), "invalid" | "refused" | "timeout") => {
                        OpenError::Invalid(message)
                    }
                    _ => OpenError::Unavailable(message),
                }
            })?;
            if view == View::Hidden && found.get("xwayland").and_then(Value::as_bool) == Some(true) {
                return Err(OpenError::Invalid(format!(
                    "{shown} opened an XWayland window; fastbrowse drives desktop windows in deskpilot's hidden mode, \
                     which needs a native Wayland window"
                )));
            }
            let window = read_window(&found).ok_or_else(|| {
                OpenError::Unavailable(format!("deskpilot described the window of {shown} without an address"))
            })?;
            // Every app of the agent opens on its one workspace, the one the page lists windows from.
            workspace = workspace.or_else(|| found.get("workspace").and_then(Value::as_str).map(str::to_string));
            launched.push(window);
        }
        let Some(window) = launched.first().cloned() else {
            return Err(OpenError::Invalid("a desktop run needs an app command".into()));
        };
        let workspace = workspace.ok_or_else(|| {
            OpenError::Unavailable("deskpilot did not say which workspace its launched window is on".into())
        })?;
        Ok(DeskpilotPage {
            client,
            workspace,
            visited: Mutex::new(vec![window.address.clone()]),
            seen: Mutex::new(launched.into_iter().map(|window| window.address).collect()),
            window: Mutex::new(window),
            lookup_missed: AtomicBool::new(false),
            latest: Mutex::new(None),
            shot: Mutex::new(None),
            generation: AtomicU64::new(0),
            film: None,
            observed: Mutex::new(None),
            withheld: AtomicBool::new(false),
            pending: Mutex::new(None),
            sent: Mutex::new(None),
            trace: Trace::disabled(),
            closed: Mutex::new(None),
            framed: Mutex::new(None),
            watch: WATCH_SECONDS,
        })
    }

    /// Films the window to `film` from now on, starting with the window as the app opened it.
    pub async fn filmed(mut self, film: Arc<dyn Film>) -> Self {
        self.film = Some(film);
        // An app can open on a secret it already shows, so its first frame waits for the first observation too.
        self.hold_frame().await;
        self
    }

    /// Takes a frame of the window as it is now, for the next observation to film or drop.
    async fn hold_frame(&self) {
        // A shot that hangs costs the video a frame, never the step its time.
        let png = self.frame().await;
        *self.pending.lock().unwrap() = png;
    }

    /// One frame of the window for a filmed page, unless frames are withheld. A shot that fails costs the video a
    /// frame, never the run a step.
    async fn frame(&self) -> Option<Vec<u8>> {
        if self.film.is_none() || self.withheld.load(Ordering::SeqCst) {
            return None;
        }
        let client = Arc::clone(&self.client);
        let window = self.address();
        // A late reply still names a file we must remove, even after the recording has dropped this frame.
        let capture = tokio::spawn(async move {
            let (shot, png, _cleanup) = Self::take_shot(&client, window).await?;
            Ok::<_, BrowserError>((shot, png))
        });
        let (shot, png) =
            tokio::time::timeout(Duration::from_secs_f64(SETTLE_SECONDS), capture).await.ok()?.ok()?.ok()?;
        *self.framed.lock().unwrap() = Some((shot.width, shot.height));
        // Withheld while the shot was taken: a secret fill may have just begun.
        (!self.withheld.load(Ordering::SeqCst)).then_some(png)
    }

    /// Tells the film where `operation` lands: window pixels in a window `width` by `height`. Only a click or the
    /// start of a drag is drawn as a press: deskpilot fills, hovers and selects through the element, not a press at
    /// this point.
    fn point(&self, operation: Operation, (x, y, width, height): (f64, f64, f64, f64)) {
        if let Some(film) = &self.film {
            // The film shows the window in the pixels of its shots, which need not be the window's own: the mark is put
            // where that frame has it, as the recording fits frames, not stretches them, to its video.
            let (x, y, width, height) = match *self.framed.lock().unwrap() {
                Some((frame_width, frame_height)) => {
                    (x * frame_width / width, y * frame_height / height, frame_width, frame_height)
                }
                None => (x, y, width, height),
            };
            let pressed = matches!(
                operation,
                Operation::Click | Operation::RightClick | Operation::DoubleClick | Operation::Drag
            );
            film.point(PointerMark { at: Instant::now(), x, y, operation, pressed, width, height });
        }
    }

    /// A shot of the window, as deskpilot binds later points to it, its PNG and the file deskpilot wrote it to.
    async fn shoot(&self) -> PageResult<(Shot, Vec<u8>, String)> {
        let (shot, png, mut cleanup) = Self::take_shot(&self.client, self.address()).await?;
        Ok((shot, png, cleanup.0.take().expect("shot file is still owned")))
    }

    async fn take_shot(client: &Deskpilot, window: String) -> PageResult<(Shot, Vec<u8>, RemoveOnDrop)> {
        let content = client
            .call("shot", client.placed("shot", json!({"window": window})))
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
        // From here the file is ours to remove however this ends: a read that fails, or a run cancelled mid-read.
        let cleanup = RemoveOnDrop(Some(path.clone()));
        let png = tokio::fs::read(&path)
            .await
            .map_err(|error| BrowserError::ScreenshotsUnavailable(format!("reading deskpilot's shot: {error}")))?;
        Ok((Shot { id, width, height }, png, cleanup))
    }

    fn address(&self) -> String {
        self.window.lock().unwrap().address.clone()
    }

    /// The windows on the run's workspace, in deskpilot's order.
    async fn windows(&self) -> PageResult<Vec<Window>> {
        let listed = self.client.call_list("windows", json!({})).await.map_err(unavailable)?;
        let here = |value: &&Value| value.get("workspace").and_then(Value::as_str) == Some(self.workspace.as_str());
        Ok(listed.iter().filter(here).filter_map(read_window).collect())
    }

    /// Makes `window` the active one. A different window invalidates every earlier observation: its ids name elements
    /// of the window before, and must not be sent to this one.
    fn activate(&self, window: Window) {
        if self.window.lock().unwrap().address != window.address {
            *self.latest.lock().unwrap() = None;
        }
        let mut visited = self.visited.lock().unwrap();
        visited.retain(|address| *address != window.address);
        visited.push(window.address.clone());
        self.seen.lock().unwrap().insert(window.address.clone());
        *self.window.lock().unwrap() = window;
    }

    /// The window a new one an action opened, the latest to appear, made active as a browser follows the tab a click
    /// opens: a dialog the app opened is where the run goes on. None when no window opened.
    async fn follow_opened(&self) -> PageResult<Option<Window>> {
        let windows = self.windows().await?;
        self.lookup_missed.store(false, Ordering::SeqCst);
        let opened = {
            let mut seen = self.seen.lock().unwrap();
            windows.into_iter().filter(|window| seen.insert(window.address.clone())).last()
        };
        if let Some(window) = &opened {
            self.activate(window.clone());
        }
        Ok(opened)
    }

    /// The active window as it is now: its title follows the app's state. When it closed, the window active before
    /// it is active again, or another window of the run. None while every window is closed after an action the run sent;
    /// every window closing with no action since the last observation ends the page. A closed app is looked at again on
    /// every read: an app may replace a window, as it does a startup or unlock one, and the run goes on in the new one.
    async fn current(&self) -> PageResult<Option<Window>> {
        let address = self.address();
        if let Ok(found) = self.client.call_json("wait_for", json!({"window": address, "timeout": 0})).await {
            let window = read_window(&found)
                .ok_or_else(|| BrowserError::failed("deskpilot/no-window", "deskpilot described no window"))?;
            *self.window.lock().unwrap() = window.clone();
            *self.closed.lock().unwrap() = None;
            return Ok(Some(window));
        }
        let windows = self.windows().await?;
        let visited = self.visited.lock().unwrap().clone();
        let back = visited.iter().rev().find_map(|address| windows.iter().find(|window| window.address == *address));
        let Some(window) = back.or(windows.first()).cloned() else {
            let sent = self.sent.lock().unwrap().clone();
            let closed = self.closed.lock().unwrap().clone();
            let Some(action) = sent.or(closed) else {
                return Err(BrowserError::Unavailable(format!(
                    "the desktop window {address} is gone, and no other window of the run is open"
                )));
            };
            *self.closed.lock().unwrap() = Some(action);
            return Ok(None);
        };
        self.visited.lock().unwrap().retain(|visited| *visited != address);
        self.activate(window.clone());
        *self.closed.lock().unwrap() = None;
        Ok(Some(window))
    }

    /// Whether every window is still closed, read again if it was: a window the app opened since reopens the run.
    async fn still_closed(&self) -> PageResult<bool> {
        if self.closed.lock().unwrap().is_none() {
            return Ok(false);
        }
        Ok(self.current().await?.is_none())
    }

    /// Why nothing more can be done in the run's windows: an action closed the last of them. Said so, since an error
    /// that only called the window gone left the user to work out that the run's own click closed it (#1399).
    fn ended(&self) -> BrowserError {
        let action = self.closed.lock().unwrap().clone().unwrap_or_default();
        BrowserError::Unavailable(format!(
            "every window of the run is closed: the last one closed after the run's {action}, so nothing more can be \
             done in the app"
        ))
    }

    /// The closed app as a window tree: the last window the run read, holding only the sentence that says every
    /// window closed and after which action, so an observation and a capture of it state that as the window's text.
    fn closed_app(&self) -> (Window, Element) {
        let action = self.closed.lock().unwrap().clone().unwrap_or_default();
        let element = |id: &str, role: &str, text: Option<String>, children| Element {
            id: id.into(),
            role: role.into(),
            name: String::new(),
            states: vec!["showing".into()],
            text,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            children,
            snapshot: None,
        };
        let said = format!("Every window of this app is closed. The last one closed after the run's {action}.");
        let root = element("e1", "frame", None, vec![element("e2", "label", Some(said), Vec::new())]);
        (self.window.lock().unwrap().clone(), root)
    }

    /// The active window's tree, or None once the app closed after an action.
    async fn read(&self, peek: bool) -> PageResult<Option<(Window, Element)>> {
        match self.current().await? {
            Some(window) => Ok(Some(self.tree(window, peek).await?)),
            None => Ok(None),
        }
    }

    /// `window`'s tree. A peek reads it without replacing the snapshot the latest observation's ids belong to.
    async fn tree(&self, window: Window, peek: bool) -> PageResult<(Window, Element)> {
        let value = self
            .client
            .call_json("elements", self.client.placed("elements", json!({"window": window.address, "peek": peek})))
            .await
            .map_err(|error| match error {
                // deskpilot's words are about making a browser or Electron app expose its tree, and read as if a flag
                // fixed every app; for a terminal or an app that draws its own interface nothing does (#1400). The
                // run says what it needs first, and quotes deskpilot, which names the case a variable fixes.
                ToolError::Refused { code, message } if code == "no_tree" => BrowserError::failed(
                    "deskpilot/no_tree",
                    format!(
                        "{} exposes no accessibility tree, and a desktop run reads and acts on an app only through \
                         its accessibility tree, so it cannot drive this app as it runs now. deskpilot says: {message}",
                        window.class
                    ),
                ),
                // Two windows share the title deskpilot finds the window's tree by: a run's own windows, or an app
                // left open by an earlier one. deskpilot works, so this is not an outage (#1345).
                ToolError::Refused { code, message } if code == "ambiguous" => BrowserError::failed(
                    "deskpilot/ambiguous",
                    format!(
                        "deskpilot cannot tell the window {} from another window with the same title: {message}",
                        window.address
                    ),
                ),
                error => unavailable(error),
            })?;
        let root: Element = serde_json::from_value(value).map_err(|error| {
            BrowserError::failed(
                "deskpilot/unreadable-tree",
                format!("deskpilot sent a tree fastbrowse cannot read: {error}"),
            )
        })?;
        Ok((window, tree::in_reading_order(root)))
    }

    /// Whether the window now differs from `observation`, in `target_id` alone when one is given, read without
    /// replacing its snapshot. Ids are numbered by the tree's order, so a tree that changed shape renumbers them and
    /// counts as changed.
    async fn differs(&self, observation: &Observation, target_id: Option<&str>) -> PageResult<bool> {
        // A window the app opened is a change even where the active window's tree stays as it was.
        let opened = {
            let windows = self.windows().await?;
            let seen = self.seen.lock().unwrap();
            windows.iter().any(|window| !seen.contains(&window.address))
        };
        if opened {
            return Ok(true);
        }
        // Every window closing is a change, whatever the window showed.
        let Some((window, root)) = self.read(true).await? else { return Ok(true) };
        let now = tree::observation(&window, &root, &[]);
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

    /// `work` unless `deadline` comes first, None then.
    async fn until<T>(deadline: Option<Instant>, work: impl Future<Output = T>) -> Option<T> {
        match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline.into(), work).await.ok(),
            None => Some(work.await),
        }
    }

    /// Waits up to `seconds` for the window to differ from `observation`, and once it does, for it to stop changing.
    async fn changed_within(
        &self,
        observation: &Observation,
        seconds: f64,
        target_id: Option<&str>,
    ) -> PageResult<bool> {
        let deadline = seconds.is_finite().then(|| Instant::now() + Duration::from_secs_f64(seconds.max(0.0)));
        // A poll is several deskpilot calls, each of which may take as long as the app is slow to answer: the deadline
        // bounds the poll itself, not only the gap between two.
        loop {
            let Some(differs) = Self::until(deadline, self.differs(observation, target_id)).await else {
                return Ok(false);
            };
            if differs? {
                // A continuously updating window must not spend the run settling after the wait found its change.
                let settle = Instant::now() + Duration::from_secs_f64(SETTLE_SECONDS);
                if let Some(settled) =
                    Self::until(deadline, self.settle(deadline.map_or(settle, |deadline| deadline.min(settle)))).await
                {
                    settled?;
                }
                return Ok(true);
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(false);
            }
            let next = Instant::now() + POLL;
            tokio::time::sleep_until(deadline.map_or(next, |deadline| deadline.min(next)).into()).await;
        }
    }

    /// Waits until two reads of the tree agree, or the deadline.
    async fn settle(&self, deadline: Instant) -> PageResult<()> {
        let Some((_, mut last)) = self.read(true).await? else { return Ok(()) };
        while Instant::now() < deadline {
            tokio::time::sleep(POLL).await;
            let Some((_, now)) = self.read(true).await? else { return Ok(()) };
            if now == last {
                break;
            }
            last = now;
        }
        Ok(())
    }

    /// Calls an input tool, in the run's view wherever the connected deskpilot's schema offers it for that tool.
    async fn tool(&self, name: &str, arguments: Value) -> Result<(), ToolError> {
        let arguments = self.client.placed(name, arguments);
        self.trace_input(name, &arguments);
        self.client.call(name, arguments).await.map(|_| ())
    }

    /// Records an input sent to an element: the snapshot and id it names, the snapshot the page last read, and the
    /// element that id is in the tree the page last read. A click that landed on another element than the one chosen
    /// could not be traced to either side, since only deskpilot's replies were logged (#1399).
    fn trace_input(&self, name: &str, arguments: &Value) {
        let Some(element) = arguments.get("element").and_then(Value::as_str) else { return };
        self.trace.event("desktop_input", || {
            let (snapshot, id) = element.rsplit_once(':').unwrap_or(("", element));
            let observed = self.observed.lock().unwrap();
            let found = observed.as_ref().and_then(|root| tree::find(root, id));
            serde_json::Map::from_iter([
                ("tool".into(), name.into()),
                ("window".into(), arguments.get("window").cloned().unwrap_or_default()),
                ("snapshot".into(), snapshot.into()),
                ("element".into(), id.into()),
                ("latest".into(), json!(*self.latest.lock().unwrap())),
                ("role".into(), json!(found.map(|element| &element.role))),
                ("name".into(), json!(found.map(|element| &element.name))),
            ])
        });
    }

    /// This page, watching the window `seconds` for a WAIT with no deadline instead of `WATCH_SECONDS`.
    pub fn watching(mut self, seconds: f64) -> Self {
        self.watch = seconds;
        self
    }

    /// The seconds a WAIT given `seconds` watches the window for.
    fn watched(&self, seconds: f64) -> f64 {
        if seconds.is_finite() { seconds } else { self.watch }
    }

    /// This page, recording each input it sends to an element in `trace`.
    pub fn traced(mut self, trace: Trace) -> Self {
        self.trace = trace;
        self
    }

    async fn dispatch(&self, action: &Action, observation: &Observation) -> Result<Option<String>, ToolError> {
        let window = self.address();
        // The pointer actions a browser recording marks; a secret field stays unmarked, as it does there.
        if matches!(
            action.operation,
            Operation::Click
                | Operation::RightClick
                | Operation::DoubleClick
                | Operation::Drag
                | Operation::Fill
                | Operation::Hover
                | Operation::Select
        ) && !action.secret
            && let Some(id) = action.target_id.as_deref()
            && let Some(aim) = self.observed.lock().unwrap().as_ref().and_then(|root| tree::aim(root, id))
        {
            self.point(action.operation, aim);
        }
        // The id as the observation's snapshot numbered it: deskpilot refuses it if that snapshot was replaced or the
        // element changed since.
        let bound = action.target_id.as_deref().map(|id| format!("{}:{id}", observation.page_key));
        let target = bound.as_deref();
        match action.operation {
            Operation::Click => self.tool("click", json!({"window": window, "element": target})).await?,
            Operation::RightClick => {
                self.tool("click", json!({"window": window, "element": target, "button": "right"})).await?
            }
            Operation::DoubleClick => {
                self.tool("click", json!({"window": window, "element": target, "double": true})).await?
            }
            Operation::Drag => return self.drag(&window, action).await,
            Operation::Hover => {
                self.tool("hover", json!({"window": window, "element": target, "wait": SETTLE_SECONDS})).await?
            }
            Operation::Fill => {
                let text = action.text.as_deref().unwrap_or_default();
                if action.secret && action.secret_origin.as_deref() != Some(origin_of(&observation.url).as_str()) {
                    return Ok(Some("the secret is not for this app".into()));
                }
                // The click focuses the field; everything after it goes to that field's window and replaces its text.
                self.tool("click", json!({"window": window, "element": target})).await?;
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
                    self.tool("click", json!({"window": window, "element": target})).await?;
                }
                self.tool("key", json!({"combo": "Return", "window": window})).await?;
            }
            Operation::Press => {
                let focused = observation
                    .controls
                    .iter()
                    .any(|c| Some(c.id.as_str()) == action.target_id.as_deref() && c.focused == Some(true));
                if target.is_some() && !focused {
                    self.tool("click", json!({"window": window, "element": target})).await?;
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
                self.tool("scroll", json!({"window": window, "element": at, "dy": dy})).await?
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
                self.tool("click", json!({"window": window, "element": target})).await?;
                return self.choose(&window, id, wanted, offered).await;
            }
            Operation::Wait => {}
            Operation::SwitchTab => {
                let Some(id) = action.tab_id.as_deref() else { return Ok(Some("switch_tab requires tab_id".into())) };
                let windows = self.client.call_list("windows", json!({})).await?;
                // Only a window on the run's own workspace is a tab: it is driven without the user's focus or view.
                let Some(window) = windows
                    .iter()
                    .filter(|value| value.get("workspace").and_then(Value::as_str) == Some(self.workspace.as_str()))
                    .filter_map(read_window)
                    .find(|window| window.address == id)
                else {
                    return Ok(Some(format!("no open window {id}")));
                };
                // The page acts on the window it makes active; nothing on the desktop moves.
                self.activate(window);
            }
            other => return Ok(Some(format!("{} is not something a desktop window can do", other.as_str()))),
        }
        Ok(None)
    }

    /// Presses on the action's target, moves to its destination and lets go there. deskpilot's drag takes points, not
    /// elements, in the pixels of a shot, so both centres are placed in a shot taken now: the model's last shot may be
    /// scaled differently or be of the window before it changed.
    async fn drag(&self, window: &str, action: &Action) -> Result<Option<String>, ToolError> {
        let (Some(source), Some(destination)) = (action.target_id.as_deref(), action.destination_id.as_deref()) else {
            return Ok(Some("drag needs an element to drag and one to drop it onto".into()));
        };
        // Ids are numbered by the tree's order, so the window may have reordered since the observation: a file manager
        // that sorted a new file in renumbers its items, and a stale id then names another file. Both ends are read in
        // the window now, and each must still be the element the run chose, or the step is stale. An end whose role and
        // name another element shares cannot be told from it, so it is stale too: the agent observes again.
        let fresh = self.peeked(window).await?;
        let ends = {
            let observed = self.observed.lock().unwrap();
            let chosen = |id: &str| observed.as_ref()?.still_in(&fresh, id).then(|| tree::aim(&fresh, id)).flatten();
            (chosen(source), chosen(destination))
        };
        let (Some((x1, y1, width, height)), Some((x2, y2, _, _))) = ends else {
            return Err(ToolError::Refused {
                code: "stale".into(),
                message: "the window changed since this observation: an end of the drag is no longer where it was"
                    .into(),
            });
        };
        let shot = match self.shoot().await {
            Ok((shot, _, path)) => {
                // Only its id is wanted, to place the points: the PNG is not film and must not pile up.
                let _ = tokio::fs::remove_file(&path).await;
                shot
            }
            Err(error) => return Ok(Some(format!("no shot to place the drag in: {error}"))),
        };
        let (sx, sy) = (shot.width / width, shot.height / height);
        self.tool(
            "drag",
            json!({
                "window": window, "shot": shot.id,
                "x1": x1 * sx, "y1": y1 * sy, "x2": x2 * sx, "y2": y2 * sy,
            }),
        )
        .await?;
        Ok(None)
    }

    /// The window's tree, read without replacing the snapshot.
    async fn peeked(&self, window: &str) -> Result<Element, ToolError> {
        let value = self
            .client
            .call_json("elements", self.client.placed("elements", json!({"window": window, "peek": true})))
            .await?;
        serde_json::from_value(value)
            .map_err(|error| ToolError::Transport(format!("deskpilot sent a tree fastbrowse cannot read: {error}")))
    }

    /// The item combo box `id` has chosen, and whether its list is open, as the window says now.
    async fn choice(&self, window: &str, id: &str) -> Result<Option<(Option<String>, bool)>, ToolError> {
        let root = self.peeked(window).await?;
        Ok(tree::find(&root, id).map(|element| {
            (tree::chosen(element).map(str::to_string), tree::list_open(element, tree::Size::of(&root)))
        }))
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
        let outcome = |outcome, detail: Option<String>| ActResult {
            failure: (outcome == StepOutcome::Failed).then_some(StepFailure::BrowserError),
            outcome,
            page_changed: false,
            detail,
            moved: false,
            landed: None,
        };
        match sent {
            Ok(Some(refused)) => Ok(outcome(StepOutcome::Failed, Some(refused))),
            Ok(None) => {
                self.generation.fetch_add(1, Ordering::Relaxed);
                let changed = self.changed_within(observation, seconds, None).await?;
                let detail = if self.closed.lock().unwrap().is_some() {
                    Some("every window of the app closed".to_string())
                } else {
                    (!changed).then(|| "the window did not change".to_string())
                };
                Ok(ActResult {
                    outcome: StepOutcome::Executed,
                    failure: None,
                    page_changed: changed,
                    detail,
                    moved: false,
                    landed: None,
                })
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
        let deadline =
            (action.operation == Operation::Wait).then(|| Instant::now() + Duration::from_secs_f64(seconds.max(0.0)));
        match Self::until(deadline, self.still_closed()).await {
            Some(closed) => {
                if closed? {
                    return Err(self.ended());
                }
            }
            None => {
                return Ok(ActResult {
                    outcome: StepOutcome::Executed,
                    failure: None,
                    page_changed: false,
                    detail: Some("the window did not change".to_string()),
                    moved: false,
                    landed: None,
                });
            }
        }
        // A window the last step opened, which its lookup was too slow to find, is followed before anything is sent: the
        // action was chosen in the window before it, and a modal it opened would refuse or misdirect the action.
        if self.lookup_missed.load(Ordering::SeqCst) {
            let settle = Instant::now() + Duration::from_secs_f64(SETTLE_SECONDS);
            let limit = deadline.map_or(settle, |deadline| deadline.min(settle));
            let opened = tokio::time::timeout_at(limit.into(), self.follow_opened()).await;
            if let Ok(Ok(Some(_))) = opened {
                self.generation.fetch_add(1, Ordering::Relaxed);
                return Ok(ActResult {
                    outcome: StepOutcome::Stale,
                    failure: None,
                    page_changed: true,
                    detail: Some("a window opened since this observation".to_string()),
                    moved: false,
                    landed: None,
                });
            }
            if let Ok(Err(error)) = opened {
                return Err(error);
            }
            // The list still does not answer: a modal may sit over the window, so nothing is sent until it is read.
            if self.lookup_missed.load(Ordering::SeqCst) {
                return Ok(ActResult {
                    outcome: StepOutcome::Stale,
                    failure: None,
                    page_changed: false,
                    detail: Some("the window list did not answer".to_string()),
                    moved: false,
                    landed: None,
                });
            }
        }
        // Only the latest snapshot's ids are live; deskpilot refuses the rest too, but a glance has no snapshot.
        let latest = self.latest.lock().unwrap().clone();
        if observation.page_key.is_empty() || latest.as_deref() != Some(observation.page_key.as_str()) {
            return Ok(ActResult {
                outcome: StepOutcome::Stale,
                failure: None,
                page_changed: false,
                detail: Some("the window was read again since this observation".to_string()),
                moved: false,
                landed: None,
            });
        }
        // Only an action that sends input to the app can be what closes it: a wait sends nothing, and a tab switch
        // only chooses which window is read, so a window lost during either is not the run's own close.
        let sends_input = !matches!(action.operation, Operation::Wait | Operation::SwitchTab);
        *self.sent.lock().unwrap() = None;
        let sent = self.dispatch(action, observation).await;
        if sends_input && matches!(sent, Ok(None)) {
            *self.sent.lock().unwrap() = Some(step_name(action, observation));
        }
        let measured = if action.operation == Operation::SwitchTab && matches!(sent, Ok(None)) {
            // Another window is read next, so the step changed what the run sees whatever that window shows.
            self.generation.fetch_add(1, Ordering::Relaxed);
            Ok(ActResult {
                outcome: StepOutcome::Executed,
                failure: None,
                page_changed: true,
                detail: None,
                moved: false,
                landed: None,
            })
        } else {
            let remaining = |deadline: Instant| deadline.saturating_duration_since(Instant::now());
            let watch = deadline.map_or(seconds, |deadline| remaining(deadline).as_secs_f64());
            let measured = self.measured(sent, observation, watch).await;
            // A WAIT's window discovery is bound by what is left of its own deadline, a step's by a settle.
            let discovery = deadline.map_or(Duration::from_secs_f64(SETTLE_SECONDS), remaining);
            self.followed(measured, discovery).await
        };
        self.hold_frame().await;
        measured
    }

    /// `measured`, moved to a window the action opened: the step changed what the run sees.
    async fn followed(&self, measured: PageResult<ActResult>, limit: Duration) -> PageResult<ActResult> {
        let measured = measured?;
        if measured.outcome != StepOutcome::Executed {
            return Ok(measured);
        }
        // The step's own time is spent by now: a window list that does not answer within `limit` leaves the step as
        // measured, and a window it opened is followed by the next read of the list.
        let opened = tokio::time::timeout(limit, self.follow_opened()).await;
        if !matches!(opened, Ok(Ok(Some(_)))) {
            match opened {
                Ok(Err(error)) => return Err(error),
                Err(_) => self.lookup_missed.store(true, Ordering::SeqCst),
                Ok(Ok(_)) => {}
            }
            return Ok(measured);
        }
        Ok(ActResult { page_changed: true, detail: None, ..measured })
    }

    /// The observation of the active window with every window of the run as a tab.
    async fn observed(&self, peek: bool) -> PageResult<(Element, Observation)> {
        let Some((window, root)) = self.read(peek).await? else {
            let (window, root) = self.closed_app();
            // No window is open, so none is a tab to switch to.
            let observation = Observation { tabs: Vec::new(), ..tree::observation(&window, &root, &[]) };
            return Ok((root, observation));
        };
        let windows = self.windows().await?;
        // A window that appeared without an action, such as a second window an app opens as it starts, is listed
        // and is not an action's later. After a lookup that timed out, a new window may be the action's: it stays unseen.
        if !self.lookup_missed.load(Ordering::SeqCst) {
            self.seen.lock().unwrap().extend(windows.iter().map(|window| window.address.clone()));
        }
        let observation = tree::observation(&window, &root, &windows);
        Ok((root, observation))
    }
}

impl Page for DeskpilotPage {
    /// deskpilot's `key` tool owns its own key names, such as `Return`, and refuses a combo it cannot press.
    fn key_error(&self, _key: &str) -> Option<String> {
        None
    }

    /// Nothing is downloaded from a desktop window.
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        Box::pin(async move {
            let (root, observation) = self.observed(false).await?;
            // A window is open again to act in, so a later closing of every window is not this action's doing.
            if self.closed.lock().unwrap().is_none() {
                *self.sent.lock().unwrap() = None;
            }
            *self.latest.lock().unwrap() = root.snapshot.clone();
            *self.observed.lock().unwrap() = Some(root);
            Ok(observation)
        })
    }

    /// A peek: it binds no ids and leaves the latest observation's snapshot standing.
    fn glance(&self) -> BoxFuture<'_, PageResult<Observation>> {
        Box::pin(async move { Ok(self.observed(true).await?.1) })
    }

    /// The window's title names its document, as an editor's names the file it holds; another window is another
    /// document.
    fn document_changed<'a>(&'a self, observation: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async move {
            Ok(match self.current().await? {
                Some(window) => window.address != observation.document_key || window.title != observation.title,
                None => true,
            })
        })
    }

    fn navigate<'a>(&'a self, _url: &'a str, _back_to: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        Box::pin(async { Err(BrowserError::failed("desktop/navigate", "a desktop window has no addresses to open")) })
    }

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        Box::pin(async move {
            let (window, root) = self.read(true).await?.unwrap_or_else(|| self.closed_app());
            Ok(tree::capture(&window, &root))
        })
    }

    fn act<'a>(&'a self, action: &'a Action, observation: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        Box::pin(async move {
            let seconds = if action.operation == Operation::Wait { self.watch } else { SETTLE_SECONDS };
            self.act_within(action, observation, seconds).await
        })
    }

    fn wait<'a>(
        &'a self,
        action: &'a Action,
        observation: &'a Observation,
        seconds: f64,
    ) -> BoxFuture<'a, PageResult<ActResult>> {
        Box::pin(async move { self.act_within(action, observation, self.watched(seconds)).await })
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
            // Only the model's shots bind its points: a film frame taken since does not move them to another shot.
            let (shot, png, _) = self.shoot().await?;
            *self.shot.lock().unwrap() = Some(shot);
            Ok(png)
        })
    }

    /// deskpilot hit-tests no point against a control, so a click held to one is refused.
    fn click_viewport<'a>(&'a self, x: f64, y: f64, within: Option<&'a str>) -> BoxFuture<'a, PageResult<ActResult>> {
        Box::pin(async move {
            if within.is_some() {
                return Ok(ActResult {
                    outcome: StepOutcome::Covered,
                    failure: None,
                    page_changed: false,
                    detail: Some("deskpilot cannot tell which control a point is on".into()),
                    moved: false,
                    landed: None,
                });
            }
            if self.still_closed().await? {
                return Err(self.ended());
            }
            let Some(shot) = self.shot.lock().unwrap().clone() else {
                return Err(BrowserError::ScreenshotsUnavailable("no screenshot of the window was taken".into()));
            };
            let before = self.glance().await?;
            *self.sent.lock().unwrap() = None;
            let point = json!({
                "window": self.address(), "x": x * shot.width, "y": y * shot.height, "shot": shot.id,
            });
            self.point(Operation::Click, (x * shot.width, y * shot.height, shot.width, shot.height));
            let sent = self.tool("click", point).await.map(|()| {
                *self.sent.lock().unwrap() = Some("click on the screenshot".into());
                None
            });
            let measured = self
                .followed(self.measured(sent, &before, SETTLE_SECONDS).await, Duration::from_secs_f64(SETTLE_SECONDS))
                .await;
            self.hold_frame().await;
            measured
        })
    }

    /// Holds back the film's frames while the window may show a secret; the recording keeps its last clean frame.
    /// Each observation says whether it shows one, which films or drops the frame taken after the action before it.
    fn withhold_frames(&self, withheld: bool) {
        self.withheld.store(withheld, Ordering::SeqCst);
        let pending = self.pending.lock().unwrap().take();
        if let (false, Some(png), Some(film)) = (withheld, pending, &self.film) {
            film.frame(png);
        }
    }

    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        Box::pin(async move {
            let window = self.current().await?.unwrap_or_else(|| self.window.lock().unwrap().clone());
            Ok(window.url())
        })
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

/// Removes a shot's file when dropped, unless the caller took it over.
struct RemoveOnDrop(Option<String>);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}
