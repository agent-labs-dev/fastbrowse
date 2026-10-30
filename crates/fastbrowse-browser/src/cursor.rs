//! Optional on-screen cursor feedback for a visible browser, drawn by the Cua Driver (`browser/cursor.py`).
//!
//! Cua Driver's `move_cursor` moves a synthetic overlay and leaves the user's pointer, focus and the browser alone
//! (only `scope=desktop` would move the real pointer, which is never sent here). It is purely visual: the click
//! itself is still sent over CDP, so nothing this module does can change what an action did, and it is never
//! retried.
//!
//! Every failure turns the feature off for the rest of the run, quietly: a missing binary, a headless machine, an
//! old driver, a window that cannot be mapped. The driver is one `cua-driver mcp` child owned by the run and
//! killed when it ends, so a crashed run cannot leave a daemon behind.
//!
//! Failures are logged by kind only, to stderr and only when `FASTBROWSE_LOG` is set: there is no logging crate
//! in this layer yet, and a run embedded in another process must not write to its stderr unasked.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard};
use std::time::{Duration, Instant};

use fastbrowse_core::BoxFuture;
use futures_util::future::{FutureExt, Shared};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, MutexGuard};
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::{sleep, timeout};

const TOOLS: [&str; 5] = ["start_session", "end_session", "list_windows", "move_cursor", "set_agent_cursor_enabled"];
const CALL_SECONDS: Duration = Duration::from_secs(2);
const START_SECONDS: Duration = Duration::from_secs(5);
const STOP_SECONDS: Duration = Duration::from_secs(1);
// The driver ends an idle session after about five minutes; start_session is idempotent, so it is the keepalive.
const KEEPALIVE_SECONDS: Duration = Duration::from_secs(60);
// The window frame can be a few pixels off from the driver's own report; a different size means a scaled display.
const BOUNDS_TOLERANCE: f64 = 2.0;
// The page sits below the browser's own toolbar. Past this, a docked DevTools panel is likelier than chrome.
const MAX_TOP_INSET: f64 = 300.0;
const MAX_SIDE_INSET: f64 = 40.0;
const OVERLAY_TITLE: &str = "Cua.AgentCursorOverlay";
// asyncio's `limit=2**20`: a reply line longer than this is a broken driver, not a list of windows.
const LINE_LIMIT: usize = 1 << 20;
const WATCH_POLLS: usize = 80;
const WATCH_INTERVAL: Duration = Duration::from_millis(250);

/// What the page itself reports about where its viewport sits in its window, in device-independent pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Geometry {
    #[serde(default)]
    pub browser_pid: Option<i64>,
    pub screen_x: f64,
    pub screen_y: f64,
    pub outer_width: f64,
    pub outer_height: f64,
    /// In CSS pixels, as `window.innerWidth`.
    pub inner_width: f64,
    pub inner_height: f64,
    /// Device-independent pixels per CSS pixel: the browser's page zoom.
    pub zoom: f64,
    pub pinch_scale: f64,
    pub visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub id: i64,
    pub pid: Option<i64>,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub z_index: Option<i64>,
}

/// Reads the page's geometry afresh; None when it cannot be read.
pub type GeometryFn = Arc<dyn Fn() -> BoxFuture<'static, Option<Geometry>> + Send + Sync>;

/// A point in the page's viewport (CSS pixels) as a point on the screen, or None when the mapping is not certain.
///
/// The window's frame is not the page: the toolbar sits above it, and the side borders, where there are any, split
/// the width evenly. Both insets come from the page's own report (`outerHeight - innerHeight`), so nothing here is
/// a guess about a particular browser's chrome; a result that looks wrong draws nothing.
pub fn viewport_point(geometry: &Geometry, x: f64, y: f64) -> Option<(f64, f64)> {
    if !geometry.visible || (geometry.pinch_scale - 1.0).abs() > 0.01 || geometry.zoom <= 0.0 {
        return None;
    }
    let width = geometry.inner_width * geometry.zoom;
    let height = geometry.inner_height * geometry.zoom;
    let side = (geometry.outer_width - width) / 2.0;
    let top = geometry.outer_height - height - side.max(0.0);
    if !((-1.0..=MAX_SIDE_INSET).contains(&side) && (-1.0..=MAX_TOP_INSET).contains(&top)) {
        return None;
    }
    if !((0.0..=geometry.inner_width).contains(&x) && (0.0..=geometry.inner_height).contains(&y)) {
        return None;
    }
    Some((geometry.screen_x + side.max(0.0) + x * geometry.zoom, geometry.screen_y + top.max(0.0) + y * geometry.zoom))
}

/// The browser's own window, when it is the one frontmost, else None.
///
/// Matching by the page's reported frame also proves the driver and the browser share one coordinate scale: on a
/// scaled display the sizes differ and nothing matches. The overlay is dropped when windows are read.
pub fn browser_window(windows: &[Window], geometry: &Geometry) -> Option<Window> {
    let pid = geometry.browser_pid?;
    let mut matches = windows.iter().filter(|window| {
        window.pid == Some(pid)
            && (window.x - geometry.screen_x).abs() <= BOUNDS_TOLERANCE
            && (window.y - geometry.screen_y).abs() <= BOUNDS_TOLERANCE
            && (window.width - geometry.outer_width).abs() <= BOUNDS_TOLERANCE
            && (window.height - geometry.outer_height).abs() <= BOUNDS_TOLERANCE
    });
    let found = *matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    // Null means the stacking order is unknown, so it cannot be shown to be in front.
    let front = found.z_index?;
    let covered = windows.iter().any(|other| other.id != found.id && other.z_index.is_none_or(|z| z > front));
    (!covered).then_some(found)
}

/// Why the feature turned itself off. Only the kind is ever logged: a message could carry what the driver saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    Timeout,
    DriverExited,
    DriverRejected,
    BadReply,
    ToolFailed,
    MissingTools,
    NoWindowScope,
    Io,
    Random,
}

impl Failure {
    fn kind(self) -> &'static str {
        match self {
            Self::Timeout => "Timeout",
            Self::DriverExited => "DriverExited",
            Self::DriverRejected => "DriverRejected",
            Self::BadReply => "BadReply",
            Self::ToolFailed => "ToolFailed",
            Self::MissingTools => "MissingTools",
            Self::NoWindowScope => "NoWindowScope",
            Self::Io => "Io",
            Self::Random => "Random",
        }
    }
}

fn log_warning(message: &str) {
    if std::env::var_os("FASTBROWSE_LOG").is_some() {
        let _ = writeln!(std::io::stderr(), "fastbrowse.browser.cursor: {message}");
    }
}

/// The std mutexes here guard plain values and are never held across an await, so a poisoned one still holds
/// a consistent value.
fn lock<T>(mutex: &StdMutex<T>) -> StdMutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Python truthiness for a JSON value, as `result.get("isError")` and `structuredContent or {}` test it.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

struct Io {
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    /// A line read in part when its request was dropped stays here, so the next read does not start mid-line.
    partial: Vec<u8>,
    next: i64,
}

impl Io {
    async fn send(&mut self, message: &Value) -> Result<(), Failure> {
        let stdin = self.stdin.as_mut().ok_or(Failure::DriverExited)?;
        let mut line = serde_json::to_vec(message).map_err(|_| Failure::BadReply)?;
        line.push(b'\n');
        stdin.write_all(&line).await.map_err(|_| Failure::Io)?;
        stdin.flush().await.map_err(|_| Failure::Io)
    }

    async fn read(&mut self) -> Result<Map<String, Value>, Failure> {
        let room = LINE_LIMIT.saturating_sub(self.partial.len());
        if room == 0 {
            return Err(Failure::BadReply);
        }
        let read =
            (&mut self.stdout).take(room as u64).read_until(b'\n', &mut self.partial).await.map_err(|_| Failure::Io)?;
        if read == 0 && self.partial.is_empty() {
            return Err(Failure::DriverExited);
        }
        if read != 0 && !self.partial.ends_with(b"\n") && self.partial.len() >= LINE_LIMIT {
            return Err(Failure::BadReply);
        }
        let line = std::mem::take(&mut self.partial);
        match serde_json::from_slice(&line) {
            Ok(Value::Object(object)) => Ok(object),
            _ => Err(Failure::BadReply),
        }
    }
}

/// A minimal JSON-RPC line client for `cua-driver mcp`, avoiding an MCP dependency in the shipped binary.
struct Driver {
    io: Mutex<Io>,
    child: Mutex<Child>,
}

impl Driver {
    fn spawn(binary: &Path) -> std::io::Result<Self> {
        let start = || {
            Command::new(binary)
                .arg("mcp")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                // A driver dropped without `close` (a panic, a dropped future) still dies with its owner.
                .kill_on_drop(true)
                .spawn()
        };
        // Linux refuses to run a file that any process holds open for writing (ETXTBSY), and a file just written is
        // held so for a moment by a child another thread is forking. The moment passes, so the start is tried again.
        let mut child = start();
        for _ in 0..20 {
            match &child {
                Err(error) if error.raw_os_error() == Some(26) => {
                    std::thread::sleep(Duration::from_millis(10));
                    child = start();
                }
                _ => break,
            }
        }
        let mut child = child?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or_else(|| std::io::Error::other("no stdout pipe"))?;
        let io = Io { stdin, stdout: BufReader::new(stdout), partial: Vec::new(), next: 0 };
        Ok(Self { io: Mutex::new(io), child: Mutex::new(child) })
    }

    fn running(&self) -> bool {
        match self.child.try_lock() {
            Ok(mut child) => matches!(child.try_wait(), Ok(None)),
            // Held only by `close` while it waits for the child, which has not exited yet.
            Err(_) => true,
        }
    }

    async fn request(&self, method: &str, params: Option<Value>, seconds: Duration) -> Result<Value, Failure> {
        let mut io = self.io.lock().await;
        io.next += 1;
        let ident = io.next;
        let mut message = json!({"jsonrpc": "2.0", "id": ident, "method": method});
        if let Some(params) = params {
            message["params"] = params;
        }
        io.send(&message).await?;
        let reading = async {
            // A reply to a request dropped earlier can still arrive; only this id's reply is this call's.
            loop {
                let line = io.read().await?;
                if line.get("id").and_then(Value::as_i64) == Some(ident) {
                    return Ok(line);
                }
            }
        };
        let mut line = timeout(seconds, reading).await.map_err(|_| Failure::Timeout)??;
        drop(io);
        if line.contains_key("error") {
            return Err(Failure::DriverRejected);
        }
        line.remove("result").ok_or(Failure::BadReply)
    }

    async fn notify(&self, method: &str) -> Result<(), Failure> {
        self.io.lock().await.send(&json!({"jsonrpc": "2.0", "method": method})).await
    }

    async fn tool(&self, name: &str, arguments: Value) -> Result<Value, Failure> {
        let params = json!({"name": name, "arguments": arguments});
        let Value::Object(mut result) = self.request("tools/call", Some(params), CALL_SECONDS).await? else {
            return Err(Failure::BadReply);
        };
        if result.get("isError").is_some_and(truthy) {
            return Err(Failure::ToolFailed);
        }
        Ok(result.remove("structuredContent").filter(truthy).unwrap_or_else(|| Value::Object(Map::new())))
    }

    /// Close its input first so it can clean up, then end it; each step is bounded.
    async fn close(&self) {
        struct Reaper<'a>(MutexGuard<'a, Child>);
        impl Drop for Reaper<'_> {
            fn drop(&mut self) {
                // Also when this future is dropped mid-wait; a child already reaped ignores it.
                let _ = self.0.start_kill();
            }
        }
        let mut child = Reaper(self.child.lock().await);
        if !matches!(child.0.try_wait(), Ok(None)) {
            return;
        }
        // A request still holding the pipe was aborted with its task and lets go at once; this only bounds it.
        if let Ok(mut io) = timeout(STOP_SECONDS, self.io.lock()).await {
            io.stdin = None;
        }
        if timeout(STOP_SECONDS, child.0.wait()).await.is_err() {
            let _ = child.0.start_kill();
            let _ = timeout(STOP_SECONDS, child.0.wait()).await;
        }
    }
}

/// Closes a driver whose opening was abandoned, since the caller that would have closed it is gone.
struct CloseOnDrop(Option<Driver>);

impl CloseOnDrop {
    fn driver(&self) -> &Driver {
        self.0.as_ref().expect("taken only once, at the end")
    }

    fn take(mut self) -> Driver {
        self.0.take().expect("taken only once, at the end")
    }
}

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        if let Some(driver) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move { driver.close().await });
        }
    }
}

struct Tasks {
    /// Bumped by every `follow`, so a move it replaced cannot start a watch after the replacement began.
    generation: u64,
    pending: Option<(AbortHandle, Shared<BoxFuture<'static, ()>>)>,
    watching: Option<JoinHandle<()>>,
}

struct Inner {
    driver: Driver,
    session: String,
    disabled: AtomicBool,
    /// None while a visibility change is in flight or was dropped midway: the next one is sent either way.
    shown: StdMutex<Option<bool>>,
    last_call: StdMutex<Instant>,
    tasks: StdMutex<Tasks>,
}

/// Shows where the agent is about to act. Reads and drawings never touch the page or its focus.
pub struct CursorFeedback {
    inner: Arc<Inner>,
}

impl CursorFeedback {
    fn new(driver: Driver, session: String) -> Self {
        let tasks = Tasks { generation: 0, pending: None, watching: None };
        let inner = Inner {
            driver,
            session,
            disabled: AtomicBool::new(false),
            shown: StdMutex::new(Some(true)),
            last_call: StdMutex::new(Instant::now()),
            tasks: StdMutex::new(tasks),
        };
        Self { inner: Arc::new(inner) }
    }

    pub fn running(&self) -> bool {
        self.inner.driver.running()
    }

    /// Wait for the move in flight, if any.
    pub async fn settled(&self) {
        let pending = lock(&self.inner.tasks).pending.as_ref().map(|(_, done)| done.clone());
        if let Some(done) = pending {
            done.await;
        }
    }

    /// Glide to a point in the viewport without waiting. Only the latest request matters, so it replaces one
    /// still in flight rather than queueing behind it. Must be called inside a tokio runtime.
    pub fn follow(&self, geometry: GeometryFn, point: (f64, f64)) {
        let mut tasks = lock(&self.inner.tasks);
        // Read under the lock `close` takes after setting it, so no move starts once `close` has begun.
        if self.inner.disabled.load(Ordering::SeqCst) {
            return;
        }
        if let Some((abort, _)) = tasks.pending.take() {
            abort.abort();
        }
        if let Some(watching) = tasks.watching.take() {
            watching.abort();
        }
        tasks.generation += 1;
        let generation = tasks.generation;
        let handle = tokio::spawn(Arc::clone(&self.inner).follow(geometry, point, generation));
        let abort = handle.abort_handle();
        let done: BoxFuture<'static, ()> = Box::pin(async move {
            let _ = handle.await;
        });
        tasks.pending = Some((abort, done.shared()));
    }

    /// End the session and stop the driver. It runs on its own task, so it completes even when the caller's
    /// future is dropped (Python shields it); it must be polled once for that task to start.
    pub async fn close(&self) {
        let inner = Arc::clone(&self.inner);
        let _ = tokio::spawn(async move { inner.close().await }).await;
    }
}

impl Inner {
    async fn follow(self: Arc<Self>, geometry: GeometryFn, point: (f64, f64), generation: u64) {
        let moved = timeout(CALL_SECONDS * 3, Arc::clone(&self).glide(geometry, point, generation)).await;
        if let Err(failure) = moved.unwrap_or(Err(Failure::Timeout)) {
            self.fail(failure).await;
        }
    }

    async fn glide(self: Arc<Self>, geometry: GeometryFn, point: (f64, f64), generation: u64) -> Result<(), Failure> {
        let Some(reading) = geometry().await else {
            // A page that has not reported its frame yet is not a reason to take the cursor away: it stays where
            // the last step left it, which is the only place it could still be.
            return Ok(());
        };
        let Some(target) = viewport_point(&reading, point.0, point.1) else {
            // A point this frame cannot place (hidden tab, zoom, a docked panel) leaves the cursor at its last
            // position; only a tab that is not showing at all takes it away.
            if !reading.visible {
                self.show(false).await?;
            }
            return Ok(());
        };
        if *lock(&self.last_call) + KEEPALIVE_SECONDS < Instant::now() {
            self.driver.tool("start_session", json!({"session": self.session})).await?;
        }
        let window = self.browser_window(&reading).await?;
        self.show(window.is_some()).await?;
        if window.is_some() {
            let arguments = json!({"session": self.session, "x": target.0, "y": target.1, "scope": "window"});
            self.driver.tool("move_cursor", arguments).await?;
        }
        *lock(&self.last_call) = Instant::now();
        if window.is_some() {
            let mut tasks = lock(&self.tasks);
            if tasks.generation == generation {
                tasks.watching = Some(tokio::spawn(Arc::clone(&self).watch(geometry)));
            }
        }
        Ok(())
    }

    /// Hide a previous drawing when its page or foreground window changes.
    async fn watch(self: Arc<Self>, geometry: GeometryFn) {
        for _ in 0..WATCH_POLLS {
            sleep(WATCH_INTERVAL).await;
            match timeout(CALL_SECONDS, self.still_shown(&geometry)).await.unwrap_or(Err(Failure::Timeout)) {
                Ok(true) => {}
                Ok(false) => return,
                Err(failure) => return self.fail(failure).await,
            }
        }
    }

    /// Take a drawing away only when the page stops showing or the browser stops being the front window. A frame
    /// that merely differs (a navigation, a scroll, a zoom) leaves the cursor at its last position.
    async fn still_shown(&self, geometry: &GeometryFn) -> Result<bool, Failure> {
        match geometry().await {
            None => Ok(true),
            Some(reading) if !reading.visible => {
                self.show(false).await?;
                Ok(false)
            }
            Some(reading) => match self.browser_window(&reading).await? {
                Some(_) => Ok(true),
                None => {
                    self.show(false).await?;
                    Ok(false)
                }
            },
        }
    }

    async fn browser_window(&self, reading: &Geometry) -> Result<Option<Window>, Failure> {
        let listed = self.driver.tool("list_windows", json!({"on_screen_only": true})).await?;
        let raw = listed.get("windows").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
        let windows: Vec<Window> = raw.iter().filter_map(window).collect();
        Ok(browser_window(&windows, reading))
    }

    async fn show(&self, shown: bool) -> Result<(), Failure> {
        {
            let mut current = lock(&self.shown);
            if *current == Some(shown) {
                return Ok(());
            }
            *current = None;
        }
        self.driver.tool("set_agent_cursor_enabled", json!({"session": self.session, "enabled": shown})).await?;
        *lock(&self.shown) = Some(shown);
        Ok(())
    }

    async fn fail(&self, failure: Failure) {
        log_warning(&format!("cursor feedback turned off for this run ({})", failure.kind()));
        self.disabled.store(true, Ordering::SeqCst);
        self.driver.close().await;
    }

    async fn close(&self) {
        self.disabled.store(true, Ordering::SeqCst);
        let (pending, watching) = {
            let mut tasks = lock(&self.tasks);
            (tasks.pending.take(), tasks.watching.take())
        };
        if let Some((abort, done)) = pending {
            abort.abort();
            done.await;
        }
        if let Some(watching) = watching {
            watching.abort();
            let _ = watching.await;
        }
        let _ = self.driver.tool("end_session", json!({"session": self.session})).await;
        self.driver.close().await;
    }
}

fn window(raw: &Value) -> Option<Window> {
    // The overlay is the cursor itself and never covers the page. A window with no process id can still cover it.
    if raw.get("title").and_then(Value::as_str).is_some_and(|title| title.starts_with(OVERLAY_TITLE)) {
        return None;
    }
    let optional_int = |value: Option<&Value>| match value {
        None | Some(Value::Null) => Some(None),
        Some(value) => value.as_i64().map(Some),
    };
    let bounds = raw.get("bounds")?;
    let float = |key: &str| bounds.get(key).and_then(Value::as_f64);
    Some(Window {
        id: raw.get("window_id")?.as_i64()?,
        pid: optional_int(raw.get("pid"))?,
        x: float("x")?,
        y: float("y")?,
        width: float("width")?,
        height: float("height")?,
        z_index: optional_int(raw.get("z_index"))?,
    })
}

/// The `cua-driver` binary to start, or None where the cursor cannot be placed with certainty.
fn driver_binary(linux: bool, var: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let set = |name: &str| var(name).is_some_and(|value| !value.is_empty());
    // X11 only: native Wayland and XWayland coordinate mapping are not verified.
    if !linux || !set("DISPLAY") || set("WAYLAND_DISPLAY") {
        return None;
    }
    std::env::split_paths(&var("PATH")?).map(|dir| dir.join("cua-driver")).find(|path| executable(path))
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata().is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(path: &Path) -> bool {
    path.is_file()
}

fn session_name() -> Result<String, Failure> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0u8; 4];
    SystemRandom::new().fill(&mut bytes).map_err(|_| Failure::Random)?;
    Ok(format!("fastbrowse-{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

async fn handshake(driver: &Driver, session: &str) -> Result<(), Failure> {
    let hello = json!({
        "protocolVersion": "2025-06-18",
        "capabilities": {},
        "clientInfo": {"name": "fastbrowse", "version": "0"},
    });
    driver.request("initialize", Some(hello), START_SECONDS).await?;
    driver.notify("notifications/initialized").await?;
    let listed = driver.request("tools/list", None, START_SECONDS).await?;
    let tools: HashMap<&str, &Value> = listed
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| Some((tool.get("name")?.as_str()?, tool)))
        .collect();
    if !TOOLS.iter().all(|name| tools.contains_key(name)) {
        return Err(Failure::MissingTools);
    }
    let scope = tools["move_cursor"].pointer("/inputSchema/properties/scope/enum").and_then(Value::as_array);
    if !scope.is_some_and(|scopes| scopes.iter().any(|scope| scope == "window")) {
        return Err(Failure::NoWindowScope);
    }
    driver.tool("start_session", json!({"session": session})).await?;
    // The overlay fades out on its own after a short idle by default, and a run spends most of its time between
    // steps: ask for a cursor that stays where it was left. A driver without this tool keeps its own default.
    if tools.contains_key("set_agent_cursor_motion") {
        let arguments = json!({"session": session, "idle_hide_ms": 0});
        if let Err(failure) = driver.tool("set_agent_cursor_motion", arguments).await {
            log_warning(&format!("the cursor may fade while the run is idle ({})", failure.kind()));
        }
    }
    Ok(())
}

pub(crate) async fn open(binary: &Path) -> Option<CursorFeedback> {
    let driver = match Driver::spawn(binary) {
        Ok(driver) => driver,
        Err(error) => {
            log_warning(&format!("cursor feedback unavailable ({:?}); running without it", error.kind()));
            return None;
        }
    };
    let guard = CloseOnDrop(Some(driver));
    let opened = match session_name() {
        Ok(session) => match timeout(START_SECONDS, handshake(guard.driver(), &session)).await {
            Ok(Ok(())) => Ok(session),
            Ok(Err(failure)) => Err(failure),
            Err(_) => Err(Failure::Timeout),
        },
        Err(failure) => Err(failure),
    };
    match opened {
        Ok(session) => Some(CursorFeedback::new(guard.take(), session)),
        Err(failure) => {
            log_warning(&format!("cursor feedback unavailable ({}); running without it", failure.kind()));
            guard.take().close().await;
            None
        }
    }
}

/// The run's cursor, or None when it is off or cannot work here. The caller awaits `close` when the run ends.
pub async fn cursor_feedback(enabled: bool) -> Option<CursorFeedback> {
    if !enabled {
        return None;
    }
    let Some(binary) = driver_binary(cfg!(target_os = "linux"), |name: &str| std::env::var_os(name)) else {
        log_warning("cursor feedback needs cua-driver and an X11 display; running without it");
        return None;
    };
    open(&binary).await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    const FAKE_DRIVER: &str = r#"
import json, os, sys, time
log = open(LOG, "a")
windows = json.loads(WINDOWS)
tools = ["start_session", "end_session", "list_windows", "move_cursor", "set_agent_cursor_enabled"]
if mode == "old":
    tools.remove("move_cursor")
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    if "id" not in message:
        continue
    if method == "initialize":
        if mode == "hang_initialize":
            log.write(json.dumps(["initializing", {"pid": os.getpid()}]) + "\n")
            log.flush()
            time.sleep(30)
        result = {"capabilities": {}}
    elif method == "tools/list":
        scope = {"enum": ["window", "desktop"] if mode != "unsafe_scope" else ["desktop"]}
        schema = {"properties": {"scope": scope}}
        result = {"tools": [{"name": name, "inputSchema": schema} for name in tools]}
    else:
        name, arguments = message["params"]["name"], message["params"]["arguments"]
        log.write(json.dumps([name, arguments]) + "\n")
        log.flush()
        if mode == "die" and name == "move_cursor":
            sys.exit(3)
        if mode == "garbage" and name == "move_cursor":
            print("not json", flush=True)
            continue
        if mode == "hang_end" and name == "end_session":
            time.sleep(30)
        if mode == "slow_hide" and name == "set_agent_cursor_enabled" and not arguments["enabled"]:
            time.sleep(0.5)
        structured = {"windows": windows} if name == "list_windows" else {}
        result = {"content": [], "structuredContent": structured}
    print(json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}), flush=True)
log.write(json.dumps(["stdin_closed", {}]) + "\n")
log.flush()
if mode == "linger":
    time.sleep(30)
"#;

    const GEOMETRY: Geometry = Geometry {
        browser_pid: Some(5),
        screen_x: 100.0,
        screen_y: 80.0,
        outer_width: 900.0,
        outer_height: 700.0,
        inner_width: 900.0,
        inner_height: 613.0,
        zoom: 1.0,
        pinch_scale: 1.0,
        visible: true,
    };

    fn raw_window(z_index: Option<i64>, x: f64) -> Value {
        json!({
            "window_id": 1,
            "pid": 5,
            "title": "page - Chrome",
            "bounds": {"x": x, "y": 80, "width": 900, "height": 700},
            "z_index": z_index,
        })
    }

    fn win(id: i64, z_index: Option<i64>) -> Window {
        Window { id, pid: Some(5), x: 100.0, y: 80.0, width: 900.0, height: 700.0, z_index }
    }

    /// The Python test's fake `cua-driver`, with its mode and windows written into the script instead of the
    /// environment, since tests share one process environment and run in parallel.
    struct Fake {
        dir: PathBuf,
        binary: PathBuf,
        log: PathBuf,
    }

    impl Fake {
        fn new(mode: &str, windows: Option<Vec<Value>>) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("fastbrowse-cursor-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let binary = dir.join("cua-driver");
            let log = dir.join("log");
            std::fs::write(&log, "").unwrap();
            let windows = Value::Array(windows.unwrap_or_else(|| vec![raw_window(Some(0), 100.0)])).to_string();
            let header = format!(
                "#!/usr/bin/env python3\nLOG = {}\nmode = {}\nWINDOWS = {}\n",
                json!(log.to_str().unwrap()),
                json!(mode),
                json!(windows),
            );
            std::fs::write(&binary, header + FAKE_DRIVER).unwrap();
            set_mode(&binary, 0o755);
            Self { dir, binary, log }
        }

        fn entries(&self) -> Vec<(String, Value)> {
            let text = std::fs::read_to_string(&self.log).unwrap();
            text.lines().map(|line| serde_json::from_str::<(String, Value)>(line).unwrap()).collect()
        }

        fn calls(&self) -> Vec<String> {
            self.entries().into_iter().map(|(name, _)| name).collect()
        }

        fn enabled(&self) -> Vec<bool> {
            self.entries()
                .into_iter()
                .filter(|(name, _)| name == "set_agent_cursor_enabled")
                .map(|(_, arguments)| arguments["enabled"].as_bool().unwrap())
                .collect()
        }

        fn env(&self, pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
            let mut vars: HashMap<String, OsString> =
                pairs.iter().map(|(k, v)| ((*k).to_owned(), OsString::from(v))).collect();
            vars.entry("PATH".into()).or_insert_with(|| self.dir.clone().into_os_string());
            move |name| vars.get(name).cloned()
        }

        async fn open(&self) -> Option<CursorFeedback> {
            open(&self.binary).await
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn fixed(geometry: Option<Geometry>) -> GeometryFn {
        Arc::new(move || Box::pin(async move { geometry }))
    }

    fn reading() -> GeometryFn {
        fixed(Some(GEOMETRY))
    }

    async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
        for _ in 0..100 {
            if check() {
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for {what}");
    }

    fn alive(pid: i64) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn viewport_point_adds_the_toolbar_inset() {
        assert_eq!(viewport_point(&GEOMETRY, 10.0, 20.0), Some((110.0, 80.0 + 87.0 + 20.0)));
    }

    #[test]
    fn viewport_point_follows_page_zoom() {
        let zoomed = Geometry { zoom: 1.5, inner_width: 600.0, inner_height: 613.0 / 1.5, ..GEOMETRY };
        let (x, y) = viewport_point(&zoomed, 100.0, 100.0).unwrap();
        assert!((x - 250.0).abs() < 1e-9 && (y - (80.0 + 87.0 + 150.0)).abs() < 1e-9, "{x} {y}");
    }

    #[test]
    fn viewport_point_declines_what_it_cannot_place() {
        let updates = [
            Geometry { visible: false, ..GEOMETRY },
            Geometry { pinch_scale: 2.0, ..GEOMETRY },
            Geometry { zoom: 0.0, ..GEOMETRY },
            // Taller than the window: a negative inset.
            Geometry { inner_height: 800.0, ..GEOMETRY },
            // A docked panel, not a toolbar.
            Geometry { inner_height: 300.0, ..GEOMETRY },
            // A side panel.
            Geometry { inner_width: 400.0, ..GEOMETRY },
        ];
        for geometry in updates {
            assert_eq!(viewport_point(&geometry, 10.0, 10.0), None, "{geometry:?}");
        }
    }

    #[test]
    fn viewport_point_declines_points_outside_the_viewport() {
        assert_eq!(viewport_point(&GEOMETRY, 10.0, 700.0), None);
        assert_eq!(viewport_point(&GEOMETRY, -1.0, 10.0), None);
    }

    #[test]
    fn viewport_point_excludes_the_bottom_window_border() {
        let geometry = Geometry { inner_width: 892.0, inner_height: 609.0, ..GEOMETRY };
        assert_eq!(viewport_point(&geometry, 10.0, 20.0), Some((114.0, 80.0 + 87.0 + 20.0)));
    }

    #[test]
    fn browser_window_must_match_and_be_frontmost() {
        let beside = |id, z| Window { x: 900.0, ..win(id, z) };
        assert!(browser_window(&[win(1, Some(5))], &GEOMETRY).is_some());
        assert!(browser_window(&[win(1, Some(5)), beside(2, Some(3))], &GEOMETRY).is_some());
        // Covered.
        assert!(browser_window(&[win(1, Some(5)), beside(2, Some(6))], &GEOMETRY).is_none());
        // Stacking unknown.
        assert!(browser_window(&[win(1, None)], &GEOMETRY).is_none());
        assert!(browser_window(&[win(1, Some(5)), beside(2, None)], &GEOMETRY).is_none());
        // Scaled display.
        let scaled = Window { width: 1800.0, height: 1400.0, ..win(1, Some(5)) };
        assert!(browser_window(&[scaled], &GEOMETRY).is_none());
        // Two windows claim the frame.
        assert!(browser_window(&[win(1, Some(5)), win(2, Some(4))], &GEOMETRY).is_none());
    }

    #[test]
    fn matching_bounds_do_not_prove_a_different_process_is_the_browser() {
        let other = Window { pid: Some(42), ..win(1, Some(0)) };
        assert!(browser_window(&[other], &GEOMETRY).is_none());
        assert!(browser_window(&[win(1, Some(0))], &Geometry { browser_pid: None, ..GEOMETRY }).is_none());
    }

    #[tokio::test]
    async fn off_by_default_starts_nothing() {
        assert!(cursor_feedback(false).await.is_none());
    }

    #[test]
    fn x11_with_the_binary_on_path_is_accepted() {
        let fake = Fake::new("ok", None);
        assert_eq!(driver_binary(true, fake.env(&[("DISPLAY", ":99")])), Some(fake.binary.clone()));
        assert_eq!(driver_binary(false, fake.env(&[("DISPLAY", ":99")])), None);
    }

    #[test]
    fn missing_binary_or_display_is_not_an_error() {
        let fake = Fake::new("ok", None);
        let empty = std::env::temp_dir().join(format!("fastbrowse-cursor-empty-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        let path = empty.to_str().unwrap();
        assert_eq!(driver_binary(true, fake.env(&[("DISPLAY", ":99"), ("PATH", path)])), None);
        assert_eq!(driver_binary(true, fake.env(&[])), None);
        assert_eq!(driver_binary(true, fake.env(&[("DISPLAY", "")])), None);
    }

    #[test]
    fn wayland_is_skipped_even_with_an_xwayland_display() {
        let fake = Fake::new("ok", None);
        assert_eq!(driver_binary(true, fake.env(&[("DISPLAY", ":0"), ("WAYLAND_DISPLAY", "wayland-0")])), None);
        assert!(fake.calls().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_old_driver_without_the_tools_is_skipped() {
        let fake = Fake::new("old", None);
        assert!(fake.open().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_without_visual_only_scope_is_skipped() {
        let fake = Fake::new("unsafe_scope", None);
        assert!(fake.open().await.is_none());
        assert!(!fake.calls().contains(&"move_cursor".to_owned()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_spawn_failure_does_not_break_the_run() {
        let fake = Fake::new("ok", None);
        set_mode(&fake.binary, 0o644);
        assert!(fake.open().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn moves_the_overlay_and_ends_its_session() {
        let fake = Fake::new("ok", None);
        let feedback = fake.open().await.unwrap();
        feedback.follow(reading(), (10.0, 20.0));
        feedback.settled().await;
        feedback.close().await;
        let entries = fake.entries();
        let moves: Vec<_> = entries
            .iter()
            .filter(|(name, _)| name == "move_cursor")
            .map(|(_, m)| (m["x"].as_f64().unwrap(), m["y"].as_f64().unwrap(), m["scope"].as_str().unwrap()))
            .collect();
        assert_eq!(moves, [(110.0, 187.0, "window")]);
        let names: Vec<_> = entries.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names[names.len() - 2..], ["end_session", "stdin_closed"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_covered_window_hides_the_cursor_and_a_clear_one_shows_it() {
        let mut cover = raw_window(Some(9), 900.0);
        cover["window_id"] = json!(2);
        let fake = Fake::new("ok", Some(vec![raw_window(Some(1), 100.0), cover]));
        let feedback = fake.open().await.unwrap();
        feedback.follow(reading(), (10.0, 20.0));
        feedback.settled().await;
        feedback.close().await;
        assert_eq!(fake.enabled(), [false]);
        assert!(!fake.calls().contains(&"move_cursor".to_owned()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_overlay_never_counts_as_a_covering_window() {
        let overlay = json!({
            "window_id": 7,
            "pid": null,
            "title": "Cua.AgentCursorOverlay.default",
            "z_index": 9,
            "bounds": {"x": 0, "y": 0, "width": 1280, "height": 900},
        });
        let fake = Fake::new("ok", Some(vec![raw_window(Some(1), 100.0), overlay]));
        let feedback = fake.open().await.unwrap();
        feedback.follow(reading(), (10.0, 20.0));
        feedback.settled().await;
        feedback.close().await;
        assert!(fake.calls().contains(&"move_cursor".to_owned()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_window_without_a_process_id_still_covers_the_page() {
        let pidless = json!({
            "window_id": 8,
            "pid": null,
            "title": "tk",
            "z_index": 8,
            "bounds": {"x": 700, "y": 300, "width": 500, "height": 400},
        });
        let fake = Fake::new("ok", Some(vec![raw_window(Some(1), 100.0), pidless]));
        let feedback = fake.open().await.unwrap();
        feedback.follow(reading(), (10.0, 20.0));
        feedback.settled().await;
        feedback.close().await;
        assert!(!fake.calls().contains(&"move_cursor".to_owned()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_uncertain_mapping_draws_nothing() {
        let fake = Fake::new("ok", None);
        let feedback = fake.open().await.unwrap();
        feedback.follow(fixed(None), (10.0, 20.0));
        feedback.settled().await;
        feedback.close().await;
        let calls = fake.calls();
        assert!(!calls.contains(&"move_cursor".to_owned()));
        assert!(!calls.contains(&"list_windows".to_owned()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failing_driver_turns_the_feature_off_and_is_cleaned_up() {
        for mode in ["die", "garbage"] {
            let fake = Fake::new(mode, None);
            let feedback = fake.open().await.unwrap();
            feedback.follow(reading(), (10.0, 20.0));
            feedback.settled().await;
            // Off for the rest of the run: nothing more is sent.
            feedback.follow(reading(), (11.0, 21.0));
            feedback.settled().await;
            assert_eq!(fake.calls().iter().filter(|name| *name == "move_cursor").count(), 1, "{mode}");
            feedback.close().await;
            assert!(!feedback.running(), "{mode}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn geometry_failure_cannot_escape_the_follower() {
        // Python's geometry raises; a Rust GeometryFn reports failure as None, so the escape left is a panic.
        let fake = Fake::new("ok", None);
        let feedback = fake.open().await.unwrap();
        let broken: GeometryFn = Arc::new(|| Box::pin(async { panic!("secret page text") }));
        feedback.follow(broken, (1.0, 1.0));
        feedback.settled().await;
        feedback.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_still_stops_the_driver() {
        let fake = Fake::new("ok", None);
        let feedback = fake.open().await.unwrap();
        // The caller's future is dropped right after its first poll; the close it started carries on.
        let _ = timeout(Duration::from_millis(1), feedback.close()).await;
        eventually("the driver to stop", || !feedback.running()).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_during_initialization_stops_the_child() {
        let fake = Fake::new("hang_initialize", None);
        let binary = fake.binary.clone();
        let opening = tokio::spawn(async move { open(&binary).await.is_some() });
        eventually("initialization to start", || !fake.calls().is_empty()).await;
        opening.abort();
        assert!(opening.await.unwrap_err().is_cancelled());
        let pid = fake.entries()[0].1["pid"].as_i64().unwrap();
        eventually("the child to be reaped", || !alive(pid)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_page_that_stops_showing_hides_the_previous_cursor() {
        let fake = Fake::new("ok", None);
        let changed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&changed);
        let geometry: GeometryFn = Arc::new(move || {
            let hidden = flag.load(Ordering::SeqCst);
            Box::pin(async move { Some(Geometry { visible: !hidden, ..GEOMETRY }) })
        });
        let feedback = fake.open().await.unwrap();
        feedback.follow(geometry, (10.0, 20.0));
        feedback.settled().await;
        changed.store(true, Ordering::SeqCst);
        let watching = lock(&feedback.inner.tasks).watching.take().expect("a drawn cursor is watched");
        timeout(Duration::from_secs(3), watching).await.unwrap().unwrap();
        feedback.close().await;
        assert_eq!(fake.enabled(), [false]);
        assert_eq!(fake.calls().iter().filter(|name| *name == "move_cursor").count(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_frame_that_blinks_out_leaves_the_cursor_where_it_was() {
        // A page mid-navigation stops reporting its frame for a moment. That is not the browser going away, so
        // the cursor stays drawn at its last position instead of blinking out between steps.
        let fake = Fake::new("ok", None);
        let gone = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&gone);
        let geometry: GeometryFn = Arc::new(move || {
            let missing = flag.load(Ordering::SeqCst);
            Box::pin(async move { (!missing).then_some(GEOMETRY) })
        });
        let feedback = fake.open().await.unwrap();
        feedback.follow(geometry, (10.0, 20.0));
        feedback.settled().await;
        gone.store(true, Ordering::SeqCst);
        assert!(lock(&feedback.inner.tasks).watching.is_some());
        sleep(WATCH_INTERVAL * 3).await;
        feedback.close().await;
        // Nothing hid it: the frame went quiet, the page did not.
        assert!(fake.enabled().is_empty());
        assert_eq!(fake.calls().iter().filter(|name| *name == "move_cursor").count(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_visibility_reply_is_resent_before_the_next_move() {
        // The fake answers a hide late, so the hide is dropped before its reply and that reply reaches the next
        // request, which skips it by id.
        let fake = Fake::new("slow_hide", None);
        let feedback = fake.open().await.unwrap();
        assert!(timeout(Duration::from_millis(100), feedback.inner.show(false)).await.is_err());
        feedback.inner.show(true).await.unwrap();
        feedback.close().await;
        assert_eq!(fake.enabled(), [false, true]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_session_cleanup_still_reaps_the_driver() {
        let fake = Fake::new("hang_end", None);
        let feedback = fake.open().await.unwrap();
        let closing = timeout(Duration::from_millis(100), feedback.close());
        assert!(closing.await.is_err());
        assert!(fake.calls().contains(&"end_session".to_owned()));
        eventually("the driver to stop", || !feedback.running()).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_driver_close_reaps_the_child() {
        // This child ignores its closed input, so the drop lands in close's first wait.
        let fake = Fake::new("linger", None);
        let driver = Driver::spawn(&fake.binary).unwrap();
        assert!(timeout(Duration::from_millis(300), driver.close()).await.is_err());
        eventually("the child to be killed", || !driver.running()).await;
    }
}
