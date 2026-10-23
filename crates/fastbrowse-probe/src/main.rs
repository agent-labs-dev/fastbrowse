//! Spike: launch or attach to Chrome, open a tab, navigate, and take one snapshot.js observation over raw CDP.
//!
//! Prints the snapshot's controls as JSON on stdout and phase timings on stderr. It answers what binary size and
//! start-up a Rust core gets on its riskiest dependency; it is not a port of `fastbrowse.browser`.
//!
//! Usage: fastbrowse-probe <url> [--cdp-url ws://...] [--chrome <name-or-path>]
//!
//! `FASTBROWSE_PROBE_T0_NS` (Unix epoch nanoseconds) moves the timing origin to before the process was spawned,
//! so a harness can time the Rust and Python paths from the same point.

use std::collections::VecDeque;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const SNAPSHOT_JS: &str = include_str!("../../../src/fastbrowse/browser/snapshot.js");

/// page.py's `_CAPTURE_LOADING_SECONDS` and `_SETTLE_POLL_SECONDS`.
const LOADING_WAIT_MS: u32 = 3000;
const LOADING_POLL_MS: u32 = 100;

type Error = Box<dyn std::error::Error>;
type Result<T> = std::result::Result<T, Error>;

fn now_ns() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos())
}

struct Clock(u128);

impl Clock {
    fn mark(&self, phase: &str) {
        eprintln!("{phase}: {:.1} ms", (now_ns() - self.0) as f64 / 1e6);
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let clock = Clock(env::var("FASTBROWSE_PROBE_T0_NS").ok().and_then(|v| v.parse().ok()).unwrap_or_else(now_ns));
    if let Err(err) = run(&clock).await {
        eprintln!("fastbrowse-probe: {err}");
        std::process::exit(1);
    }
}

async fn run(clock: &Clock) -> Result<()> {
    let mut url = None;
    let mut cdp_url = None;
    let mut chrome = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--cdp-url" => cdp_url = Some(args.next().ok_or("--cdp-url needs a value")?),
            "--chrome" => chrome = Some(args.next().ok_or("--chrome needs a value")?),
            _ if url.is_none() => url = Some(arg),
            _ => return Err(format!("unexpected argument {arg}").into()),
        }
    }
    let url = url.ok_or("usage: fastbrowse-probe <url> [--cdp-url ws://...] [--chrome <name-or-path>]")?;

    let launched = match cdp_url {
        Some(_) => None,
        None => Some(LocalChrome::launch(chrome.as_deref())?),
    };
    let result = async {
        let ws = match (&cdp_url, &launched) {
            (Some(ws), _) => ws.clone(),
            (None, Some(local)) => local.wait_for_ws().await?,
            (None, None) => unreachable!(),
        };
        let mut cdp = Cdp::connect(&ws).await?;
        clock.mark("connected");
        let controls = observe(&mut cdp, &url, clock).await?;
        println!("{}", serde_json::to_string(&controls)?);
        Ok(())
    }
    .await;
    if let Some(local) = launched {
        local.close();
    }
    result
}

async fn observe(cdp: &mut Cdp, url: &str, clock: &Clock) -> Result<Value> {
    let target = cdp.call("Target.createTarget", json!({"url": "about:blank"}), None).await?;
    let target_id = target["targetId"].as_str().ok_or("createTarget returned no targetId")?;
    let attached = cdp.call("Target.attachToTarget", json!({"targetId": target_id, "flatten": true}), None).await?;
    let session = attached["sessionId"].as_str().ok_or("attachToTarget returned no sessionId")?.to_owned();
    let session = Some(session.as_str());
    cdp.call("Page.enable", json!({}), session).await?;
    clock.mark("tab");

    let navigated = cdp.call("Page.navigate", json!({"url": url}), session).await?;
    if let Some(error) = navigated["errorText"].as_str() {
        return Err(format!("Page.navigate failed ({error})").into());
    }
    cdp.wait_event("Page.loadEventFired", session, Duration::from_secs(30)).await?;
    clock.mark("navigated");

    // page.py's `_page_js` with no origin grant, called as `_snapshot_all_frames` calls it on the main frame:
    // wrapped in `_loaded_script`, which waits up to 3s for a visible loading indicator to go.
    let page_js = format!("(mode => ({SNAPSHOT_JS})(mode, null))");
    let loaded = format!(
        "new Promise(resolve => {{ const sample = {page_js}; \
         const deadline = performance.now() + {LOADING_WAIT_MS}; \
         const poll = () => {{ const state = sample('fingerprint'); \
         if (!state.loading || state.hidden || performance.now() >= deadline) {{ resolve(null); return; }} \
         setTimeout(poll, {LOADING_POLL_MS}); }}; poll(); }})"
    );
    let expression = format!("(async () => {{ await {loaded}; return ({page_js}('snapshot')); }})()");
    let out = cdp
        .call(
            "Runtime.evaluate",
            json!({"expression": expression, "returnByValue": true, "awaitPromise": true}),
            session,
        )
        .await?;
    if out.get("exceptionDetails").is_some() {
        return Err(format!("Runtime.evaluate failed: {}", out["exceptionDetails"]).into());
    }
    clock.mark("snapshot");
    // An attached browser outlives the probe, so the tab it opened is closed, as BrowserSession closes its own.
    cdp.call("Target.closeTarget", json!({"targetId": target_id}), None).await?;
    let snapshot = &out["result"]["value"];
    Ok(json!({
        "url": snapshot["url"],
        "title": snapshot["title"],
        "inaccessible_frames": snapshot["inaccessible_frames"],
        "controls": snapshot["controls"],
    }))
}

/// One browser-level CDP connection. Calls are sequential: events read while waiting for a reply are queued for
/// `wait_event`. A real port needs a reader task routing replies by id and events by session.
struct Cdp {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    events: VecDeque<Value>,
}

impl Cdp {
    async fn connect(url: &str) -> Result<Self> {
        let (ws, _) = connect_async(url).await?;
        Ok(Self { ws, next_id: 1, events: VecDeque::new() })
    }

    async fn call(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        self.ws.send(Message::text(message.to_string())).await?;
        loop {
            let mut message = self.read().await?;
            if message["id"].as_u64() == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(format!("{method} failed: {error}").into());
                }
                return Ok(message["result"].take());
            }
            if message.get("method").is_some() {
                self.events.push_back(message);
            }
        }
    }

    async fn wait_event(&mut self, method: &str, session: Option<&str>, limit: Duration) -> Result<Value> {
        let matches = |m: &Value| m["method"] == method && m["sessionId"].as_str() == session;
        if let Some(at) = self.events.iter().position(matches) {
            return Ok(self.events.remove(at).unwrap_or_default());
        }
        timeout(limit, async {
            loop {
                let message = self.read().await?;
                if matches(&message) {
                    return Ok(message);
                }
            }
        })
        .await
        .map_err(|_| format!("no {method} within {limit:?}"))?
    }

    async fn read(&mut self) -> Result<Value> {
        loop {
            match self.ws.next().await.ok_or("the browser closed the connection")?? {
                Message::Text(text) => return Ok(serde_json::from_str(&text)?),
                Message::Close(_) => return Err("the browser closed the connection".into()),
                _ => {}
            }
        }
    }
}

/// Chrome launched as `adapters/local_chrome.py` launches it: headless, a throwaway profile, port chosen by Chrome.
struct LocalChrome {
    child: Child,
    profile: PathBuf,
}

impl LocalChrome {
    fn launch(binary: Option<&str>) -> Result<Self> {
        let binary = find_chrome(binary).ok_or("Chrome is not installed")?;
        let profile = env::temp_dir().join(format!("fastbrowse-probe-{}-{}", std::process::id(), now_ns()));
        quiet_password_manager(&profile)?;
        let child = Command::new(binary)
            .args([
                "--headless=new",
                "--window-size=1280,900",
                "--remote-debugging-port=0",
                &format!("--user-data-dir={}", profile.display()),
                "--no-first-run",
                "--disable-popup-blocking",
                "--no-startup-window",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Self { child, profile })
    }

    /// DevToolsActivePort holds the port and the browser target's path, so no HTTP request is needed.
    async fn wait_for_ws(&self) -> Result<String> {
        let active = self.profile.join("DevToolsActivePort");
        for _ in 0..9000 {
            if let Ok(text) = fs::read_to_string(&active) {
                let mut lines = text.lines();
                if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                    let ws = format!("ws://127.0.0.1:{port}{path}");
                    // Written before it listens, so a refused connect is retried.
                    if TcpStream::connect(format!("127.0.0.1:{port}")).await.is_ok() {
                        return Ok(ws);
                    }
                }
            }
            sleep(Duration::from_millis(10)).await;
        }
        Err("Chrome did not start DevTools within 90s".into())
    }

    fn close(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.profile);
    }
}

fn find_chrome(binary: Option<&str>) -> Option<PathBuf> {
    if let Some(binary) = binary {
        return which(binary);
    }
    [
        "google-chrome-stable",
        "google-chrome",
        "chromium",
        "chromium-browser",
        "chrome",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ]
    .into_iter()
    .find_map(which)
}

fn which(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.components().count() > 1 {
        return path.is_file().then(|| path.to_owned());
    }
    env::split_paths(&env::var_os("PATH")?).map(|dir| dir.join(name)).find(|p| p.is_file())
}

fn quiet_password_manager(profile: &Path) -> Result<()> {
    let default = profile.join("Default");
    fs::create_dir_all(&default)?;
    let preferences = json!({
        "credentials_enable_service": false,
        "profile": {"password_manager_enabled": false, "password_manager_leak_detection": false},
    });
    fs::write(default.join("Preferences"), preferences.to_string())?;
    Ok(())
}
