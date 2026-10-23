//! Spike: launch or attach to Chrome, open (or attach to) a tab, navigate, and observe it as `CdpPage.observe`
//! does: the main frame and every cross-origin iframe in its own session, merged into one control list.
//!
//! Prints `{"steps": [observation, ...]}` on stdout and phase timings on stderr.
//!
//! Usage: fastbrowse-probe [url] [--cdp-url ws://...] [--attach <url-or-title-part>] [--chrome <name-or-path>]
//!            [--timeout-ms N] [--click <js>] [--accept-dialog] [--pause-new-targets]
//!
//! `--click` evaluates a script after the first observation (a click that may open a dialog) and observes again;
//! `--accept-dialog` then accepts the pending dialog and observes once more. `--timeout-ms` and Ctrl-C cancel the
//! run wherever it is; the tab is closed if the probe opened it and left open if it attached.
//!
//! `FASTBROWSE_PROBE_T0_NS` (Unix epoch nanoseconds) moves the timing origin to before the process was spawned,
//! so a harness can time the Rust and Python paths from the same point.

mod cdp;
mod page;
mod session;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

use crate::cdp::Cdp;
use crate::page::{evaluate, observe, until_dialog};
use crate::session::{Options, Session};

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

#[derive(Default)]
struct Args {
    url: Option<String>,
    cdp_url: Option<String>,
    attach: Option<String>,
    chrome: Option<String>,
    timeout_ms: Option<u64>,
    click: Option<String>,
    accept_dialog: bool,
    pause_new_targets: bool,
}

fn parse_args() -> Result<Args> {
    let mut parsed = Args::default();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--cdp-url" => parsed.cdp_url = Some(value()?),
            "--attach" => parsed.attach = Some(value()?),
            "--chrome" => parsed.chrome = Some(value()?),
            "--timeout-ms" => parsed.timeout_ms = Some(value()?.parse()?),
            "--click" => parsed.click = Some(value()?),
            "--accept-dialog" => parsed.accept_dialog = true,
            "--pause-new-targets" => parsed.pause_new_targets = true,
            _ if parsed.url.is_none() && !arg.starts_with("--") => parsed.url = Some(arg),
            _ => return Err(format!("unexpected argument {arg}").into()),
        }
    }
    if parsed.url.is_none() && parsed.attach.is_none() {
        return Err("usage: fastbrowse-probe [url] [--cdp-url ws://...] [--attach <match>] [--timeout-ms N] ...".into());
    }
    Ok(parsed)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let clock = Clock(env::var("FASTBROWSE_PROBE_T0_NS").ok().and_then(|v| v.parse().ok()).unwrap_or_else(now_ns));
    match run(&clock).await {
        Ok(true) => {}
        Ok(false) => std::process::exit(130),
        Err(err) => {
            eprintln!("fastbrowse-probe: {err}");
            std::process::exit(1);
        }
    }
}

/// Ok(false) when the run was cancelled.
async fn run(clock: &Clock) -> Result<bool> {
    let args = parse_args()?;
    let launched = match args.cdp_url {
        Some(_) => None,
        None => Some(LocalChrome::launch(args.chrome.as_deref())?),
    };
    let result = async {
        let ws = match (&args.cdp_url, &launched) {
            (Some(ws), _) => ws.clone(),
            (None, Some(local)) => local.wait_for_ws().await?,
            (None, None) => unreachable!(),
        };
        let cdp = Cdp::connect(&ws).await?;
        clock.mark("connected");
        let options = Options { pause_new_targets: args.pause_new_targets };
        let session = match &args.attach {
            Some(needle) => Session::attach(&cdp, needle, options).await?,
            None => Session::open(&cdp, options).await?,
        };
        clock.mark("tab");
        let limit = args.timeout_ms.map_or(Duration::MAX, Duration::from_millis);
        let mut work = Box::pin(steps(&session, &args, clock));
        let outcome = tokio::select! {
            out = &mut work => Ok(out),
            () = sleep(limit) => Err("timeout"),
            _ = tokio::signal::ctrl_c() => Err("interrupt"),
        };
        if let Err(why) = outcome {
            let in_flight = cdp.pending();
            // Dropping the run drops every call it had in flight; their replies are discarded when they come.
            drop(work);
            eprintln!("cancelled ({why}) with {in_flight} calls in flight; pending once dropped: {}", cdp.pending());
        }
        session.close().await;
        eprintln!("calls pending after close: {}", cdp.pending());
        match outcome {
            Ok(steps) => {
                println!("{}", serde_json::to_string(&steps?)?);
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }
    .await;
    if let Some(local) = launched {
        local.close();
    }
    result
}

async fn steps(session: &Session, args: &Args, clock: &Clock) -> Result<Value> {
    if let Some(url) = &args.url {
        navigate(session, url).await?;
        clock.mark("navigated");
    }
    let mut steps = vec![serde_json::to_value(observe(session).await?)?];
    clock.mark("snapshot");
    if let Some(script) = &args.click {
        // A click whose handler opens a dialog holds the evaluate until the dialog is handled.
        match until_dialog(session, evaluate(session, &session.session_id, script)).await {
            Some(result) => drop(result?),
            None => eprintln!("the click opened a dialog"),
        }
        steps.push(serde_json::to_value(observe(session).await?)?);
        if args.accept_dialog {
            session.handle_dialog(true, None).await?;
            steps.push(serde_json::to_value(observe(session).await?)?);
        }
    }
    Ok(json!({"steps": steps, "input_navigations": session.input_navigations()}))
}

/// Navigate and wait for the load event, or for a dialog that blocks it.
async fn navigate(session: &Session, url: &str) -> Result<()> {
    let sid = Some(session.session_id.as_str());
    let mut loaded = session.cdp.subscribe("Page.loadEventFired", sid);
    let navigated = session.cdp.call("Page.navigate", json!({"url": url}), sid).await?;
    if let Some(error) = navigated["errorText"].as_str() {
        return Err(format!("Page.navigate failed ({error})").into());
    }
    match timeout(Duration::from_secs(30), until_dialog(session, loaded.recv())).await {
        Err(_) => Err("no Page.loadEventFired within 30s".into()),
        Ok(Some(None)) => Err("the browser closed the connection".into()),
        Ok(_) => Ok(()),
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
