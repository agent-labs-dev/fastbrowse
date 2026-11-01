//! Chrome on this machine: headless with a throwaway profile by default (`adapters/local_chrome.py`).

use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fastbrowse_core::models::{BrowserConnection, LocalChrome};
use serde_json::{Map, Value};
use tokio::process::{Child, Command};

use crate::adapters::http;
use crate::error::BrowserError;

/// How long a started Chrome has to report its DevTools address.
///
/// A first start reads Chrome's binary and resources from a cold disk: on CI runners that took 7s in one run and
/// just over 30s in another, with every Chrome process blocked on page-in, and printed "DevTools listening" as the
/// old 30s limit failed the whole browser suite.
const DEVTOOLS_TIMEOUT: Duration = Duration::from_secs(90);
/// How long a terminated Chrome has to exit before it is killed.
const CLOSE_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum LocalChromeError {
    /// Python's `RuntimeError`: no Chrome, or a Chrome that exited or never answered. The message carries the tail
    /// of Chrome's own stderr, which is what says why.
    Runtime(String),
    Browser(BrowserError),
    Io(std::io::Error),
}

impl fmt::Display for LocalChromeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(message) => f.write_str(message),
            Self::Browser(error) => error.fmt(f),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for LocalChromeError {}

impl From<std::io::Error> for LocalChromeError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// `override_` is `Settings.chrome`, a name or path that replaces discovery rather than joining it.
pub fn find_chrome(override_: Option<&str>) -> Option<String> {
    find_with(override_, &which, &|name| std::env::var(name).ok())
}

fn find_with(
    override_: Option<&str>,
    which: &dyn Fn(&str) -> Option<String>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    if let Some(name) = override_.filter(|name| !name.is_empty()) {
        return which(name);
    }
    let names = [
        "google-chrome-stable",
        "google-chrome",
        "chromium",
        "chromium-browser",
        "chrome",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ];
    names.iter().map(|name| name.to_string()).chain(windows_installs(env)).find_map(|name| which(&name))
}

/// Chrome's per-machine and per-user installs, which its Windows installer leaves off PATH.
fn windows_installs(env: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"]
        .into_iter()
        .filter_map(|name| env(name).filter(|root| !root.is_empty()))
        .map(|root| Path::new(&root).join("Google").join("Chrome").join("Application").join("chrome.exe"))
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

/// `shutil.which`: a name with a directory part is checked where it points, a bare name is searched for on PATH.
pub fn which(name: &str) -> Option<String> {
    which_in(name, std::env::var_os("PATH"))
}

fn which_in(name: &str, path: Option<OsString>) -> Option<String> {
    let extensions = executable_extensions(name);
    let candidates = |base: PathBuf| -> Option<String> {
        extensions
            .iter()
            .map(|ext| PathBuf::from(format!("{}{ext}", base.display())))
            .find(|p| runnable(p))
            .map(|p| p.to_string_lossy().into_owned())
    };
    if Path::new(name).parent().is_some_and(|parent| !parent.as_os_str().is_empty()) {
        return candidates(PathBuf::from(name));
    }
    // Python falls back to the system's default search path when PATH is unset, and searches nothing when it is
    // empty.
    let path = path.unwrap_or_else(|| OsString::from(if cfg!(windows) { "" } else { "/bin:/usr/bin" }));
    if path.is_empty() {
        return None;
    }
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    if cfg!(windows) {
        dirs.insert(0, PathBuf::from("."));
    }
    let mut seen = Vec::new();
    for dir in dirs {
        if seen.contains(&dir) {
            continue;
        }
        seen.push(dir.clone());
        // `os.path.join("", name)` is the bare name, which resolves against the working directory.
        let base = if dir.as_os_str().is_empty() { PathBuf::from(name) } else { dir.join(name) };
        if let Some(found) = candidates(base) {
            return Some(found);
        }
    }
    None
}

/// The suffixes tried after a name: none on POSIX, PATHEXT's on Windows unless the name already carries one.
fn executable_extensions(name: &str) -> Vec<String> {
    if !cfg!(windows) {
        return vec![String::new()];
    }
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let extensions: Vec<String> = pathext.split(';').filter(|ext| !ext.is_empty()).map(String::from).collect();
    let lower = name.to_lowercase();
    if extensions.iter().any(|ext| lower.ends_with(&ext.to_lowercase())) {
        return vec![String::new()];
    }
    extensions
}

/// `os.path.exists(p) and os.access(p, X_OK) and not os.path.isdir(p)`, with "executable" read from the mode bits.
fn runnable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if metadata.is_dir() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// `Path.expanduser` for `~` and `~/...`; another user's `~name` is left as written.
fn expand_user(path: &Path) -> PathBuf {
    let home = || std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    match path.strip_prefix("~") {
        Ok(rest) => home().map_or_else(|| path.to_path_buf(), |home| home.join(rest)),
        Err(_) => path.to_path_buf(),
    }
}

/// A fresh path in the system's temporary directory, unique within this process and across processes.
fn temp_path(kind: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("fastbrowse-{kind}-{}-{nanos}-{count}", std::process::id()))
}

/// A directory only this user can read, as `tempfile.TemporaryDirectory` makes: the profile holds cookies.
fn temp_dir() -> std::io::Result<PathBuf> {
    let path = temp_path("chrome");
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(&path)?;
    Ok(path)
}

/// Chrome's stderr log, as `tempfile.TemporaryFile`: unlinked at once where the system allows an open file to be,
/// else removed on close. Returns the file and the path still to remove, if any.
fn temp_log() -> std::io::Result<(File, Option<PathBuf>)> {
    let path = temp_path("chrome-log");
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(&path)?;
    let left = std::fs::remove_file(&path).is_err().then_some(path);
    Ok((file, left))
}

/// The command line, flags in Python's order.
fn launch_args(options: &LocalChrome, profile: &Path) -> Vec<String> {
    let mut args = Vec::new();
    if !options.headed {
        args.push("--headless=new".to_owned());
    }
    args.extend([
        // Headless defaults to 800x600, where responsive sites collapse their header into a toggle and the control
        // the agent needs is not in the page at all.
        "--window-size=1280,900".to_owned(),
        "--remote-debugging-port=0".to_owned(),
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".to_owned(),
        "--disable-popup-blocking".to_owned(),
    ]);
    // A headless tab opened beside the startup tab sometimes never presented a frame: one run in five had no
    // screencast and a screenshot that never returned. Without a startup window, the run's tab opens in a window of
    // its own, as headless Chrome's first tab, and that tab always painted.
    args.push(if options.headed { "about:blank" } else { "--no-startup-window" }.to_owned());
    args
}

/// A Chrome launched for one run, terminated on `close` and killed if dropped without it.
pub struct LocalChromeProcess {
    child: Option<Child>,
    /// The throwaway profile, deleted on close; None for a kept profile.
    temp_profile: Option<PathBuf>,
    /// The log's path where it could not be unlinked while open.
    log_path: Option<PathBuf>,
}

impl LocalChromeProcess {
    /// Launch Chrome with `options` and wait for its DevTools address. A launch that fails leaves no process and no
    /// throwaway profile behind.
    pub async fn launch(options: &LocalChrome) -> Result<(Self, BrowserConnection), LocalChromeError> {
        let binary = find_chrome(options.binary.as_deref())
            .ok_or_else(|| LocalChromeError::Runtime("Chrome is not installed".into()))?;
        let mut process = Self { child: None, temp_profile: None, log_path: None };
        match process.start(&binary, options).await {
            Ok(cdp_url) => {
                // Headless Chrome has no window for an activated tab to raise, so it keeps the foreground path.
                let connection =
                    BrowserConnection { foreground: !options.headed, ..BrowserConnection::new(cdp_url, false) };
                Ok((process, connection))
            }
            Err(error) => {
                process.close().await;
                Err(error)
            }
        }
    }

    async fn start(&mut self, binary: &str, options: &LocalChrome) -> Result<String, LocalChromeError> {
        let profile = match &options.profile {
            Some(kept) => expand_user(kept),
            None => {
                let created = temp_dir()?;
                self.temp_profile = Some(created.clone());
                created
            }
        };
        let (mut log, left) = temp_log()?;
        self.log_path = left;
        // Chrome picks its own port and writes it to the profile. Choosing a free port here and handing it over left
        // a window in which something else could take it, and Chrome then never answered.
        let active = profile.join("DevToolsActivePort");
        match std::fs::remove_file(&active) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        quiet_password_manager(&profile)?;
        let child = Command::new(binary)
            .args(launch_args(options, &profile))
            // Chrome never reads stdin, and under `fastbrowse serve --stdio` stdin is the caller's protocol stream.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log.try_clone()?)
            .kill_on_drop(true)
            .spawn()?;
        let child = self.child.insert(child);
        wait_for_ws(&active, child, &mut log, DEVTOOLS_TIMEOUT).await
    }

    /// The browser's process id while it runs.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    /// Terminate Chrome, kill it if it has not exited within ten seconds, and delete a throwaway profile.
    pub async fn close(&mut self) {
        self.close_within(CLOSE_GRACE).await;
    }

    async fn close_within(&mut self, grace: Duration) {
        if let Some(mut child) = self.child.take() {
            terminate(&mut child).await;
            if tokio::time::timeout(grace, child.wait()).await.is_err() {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
        if let Some(profile) = self.temp_profile.take() {
            let _ = tokio::fs::remove_dir_all(profile).await;
        }
        if let Some(log) = self.log_path.take() {
            let _ = tokio::fs::remove_file(log).await;
        }
    }
}

impl Drop for LocalChromeProcess {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
        if let Some(profile) = self.temp_profile.take() {
            let _ = std::fs::remove_dir_all(profile);
        }
        if let Some(log) = self.log_path.take() {
            let _ = std::fs::remove_file(log);
        }
    }
}

/// `Popen.terminate`: SIGTERM, so Chrome flushes a kept profile's cookies before it exits. std and tokio only send
/// SIGKILL, and this crate has no unsafe code to call `kill(2)` with, so the system's `kill` sends it.
async fn terminate(child: &mut Child) {
    // A child already reaped has no id, and its number may belong to another process by now.
    let Some(pid) = child.id() else {
        return;
    };
    if cfg!(unix) {
        let sent = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        if sent.is_ok_and(|status| status.success()) {
            return;
        }
    }
    // Windows has no SIGTERM: `Popen.terminate` is `TerminateProcess` there, which is what this sends.
    let _ = child.start_kill();
}

/// Keep Chrome's password manager out of the profile, whether it is fresh or kept between runs.
///
/// After a sign-in, its save and leaked-password bubbles take the browser's input: the page still renders and hit
/// tests still pass, but no click reaches it, and the bubble is Chrome's own interface, outside the page the agent
/// sees. saucedemo's shared password is a known leak, so every click after its sign-in did nothing. A kept profile
/// keeps every other preference its owner set; these three are set on each launch.
fn quiet_password_manager(profile: &Path) -> Result<(), LocalChromeError> {
    let preferences = profile.join("Default").join("Preferences");
    let held: Option<Value> = match std::fs::read(&preferences) {
        Ok(bytes) => String::from_utf8(bytes).ok().and_then(|text| serde_json::from_str(&text).ok()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(Value::Object(Map::new())),
        Err(error) => return Err(error.into()),
    };
    let Some(Value::Object(mut held)) = held else {
        let message = format!("{} is not a Chrome preferences file", preferences.display());
        return Err(LocalChromeError::Browser(BrowserError::Failed(message)));
    };
    held.insert("credentials_enable_service".into(), Value::Bool(false));
    let mut owned = match held.remove("profile") {
        Some(Value::Object(owned)) => owned,
        _ => Map::new(),
    };
    owned.insert("password_manager_enabled".into(), Value::Bool(false));
    owned.insert("password_manager_leak_detection".into(), Value::Bool(false));
    held.insert("profile".into(), Value::Object(owned));
    if let Some(parent) = preferences.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&preferences, Value::Object(held).to_string())?;
    Ok(())
}

/// Wait for Chrome to report its DevTools address, failing with its own output if it exits or never does.
async fn wait_for_ws(
    active: &Path,
    child: &mut Child,
    log: &mut File,
    timeout: Duration,
) -> Result<String, LocalChromeError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            let message =
                format!("Chrome exited with status {} before DevTools started{}", returncode(status), tail(log));
            return Err(LocalChromeError::Runtime(message));
        }
        // Not written yet, written only partly, or written and not yet listening: each is read again shortly.
        if let Some(url) = devtools_url(active).await? {
            return Ok(url);
        }
        if Instant::now() > deadline {
            let message = format!("Chrome did not start DevTools within {:.0}s{}", timeout.as_secs_f64(), tail(log));
            return Err(LocalChromeError::Runtime(message));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The address `/json/version` names, None while Chrome is not ready to say.
async fn devtools_url(active: &Path) -> Result<Option<String>, LocalChromeError> {
    let Ok(text) = tokio::fs::read_to_string(active).await else {
        return Ok(None);
    };
    let Ok(port) = text.split('\n').next().unwrap_or("").trim().parse::<u16>() else {
        return Ok(None);
    };
    let url = format!("http://127.0.0.1:{port}/json/version");
    let Ok(response) = http::request("GET", &url, &[], None, Duration::from_secs(1)).await else {
        return Ok(None);
    };
    let Some(version) = response.is_success().then(|| response.json().ok()).flatten() else {
        return Ok(None);
    };
    match version.get("webSocketDebuggerUrl") {
        Some(Value::String(url)) => Ok(Some(url.clone())),
        Some(other) if !other.is_null() => Ok(Some(other.to_string())),
        _ => Err(LocalChromeError::Runtime("Chrome's /json/version names no webSocketDebuggerUrl".into())),
    }
}

/// `Popen.returncode`: the exit code, or the negated signal that ended the process.
fn returncode(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return -signal;
    }
    status.code().unwrap_or(-1)
}

/// The last 2000 bytes of Chrome's stderr, introduced for an error message, or nothing when it printed nothing.
fn tail(log: &mut File) -> String {
    let mut bytes = Vec::new();
    if log.seek(SeekFrom::Start(0)).is_err() || log.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    let start = bytes.len().saturating_sub(2000);
    let text = String::from_utf8_lossy(&bytes[start..]);
    let text = text.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c));
    if text.is_empty() { String::new() } else { format!(":\n{text}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test's own.
    ///
    /// `temp_dir()` itself is shared by every test in this module, several of which write a stand-in `chrome`
    /// into it and then run it. Two tests sharing it truncate a script under a test that is already executing it,
    /// and one test deletes the directory when it finishes, taking the others' scripts with it.
    fn scratch() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = temp_dir().unwrap().join(format!(
            "fastbrowse-chrome-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stand-in executable, written by a child shell rather than by this process.
    ///
    /// A descriptor closes on `exec`, not on `fork`, so a child another test forks while this process holds the
    /// script open for writing keeps that descriptor until it execs, and running the script then fails with
    /// `ETXTBSY` ("Text file busy"). A directory of the test's own does not help, since the descriptor is the
    /// process's. The shell's descriptor is gone once it exits, and no other test forks from it.
    #[cfg(unix)]
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", "printf %s \"$1\" > \"$2\" && chmod 755 \"$2\"", "sh"])
            .arg(format!("#!/bin/sh\n{body}"))
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success(), "writing {} failed: {status}", path.display());
        path
    }

    #[test]
    fn chrome_discovery() {
        let names = [
            "google-chrome-stable",
            "google-chrome",
            "chromium",
            "chromium-browser",
            "chrome",
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/custom/chrome",
        ];
        for binary in names {
            let which = |name: &str| (name == binary).then(|| name.to_owned());
            let override_ = (binary == "/custom/chrome").then_some(binary);
            assert_eq!(find_with(override_, &which, &|_| None).as_deref(), Some(binary));
        }
        // An override replaces discovery rather than joining it.
        let which = |name: &str| (name == "chromium").then(|| name.to_owned());
        assert_eq!(find_with(Some("/custom/chrome"), &which, &|_| None), None);
    }

    #[test]
    fn chrome_discovery_finds_a_windows_install_off_path() {
        let installed = Path::new("C:/Users/me/AppData/Local")
            .join("Google")
            .join("Chrome")
            .join("Application")
            .join("chrome.exe")
            .to_string_lossy()
            .into_owned();
        let env = |name: &str| (name == "LOCALAPPDATA").then(|| "C:/Users/me/AppData/Local".to_owned());
        let which = |name: &str| (name == installed).then(|| name.to_owned());
        assert_eq!(find_with(None, &which, &env), Some(installed.clone()));
    }

    #[cfg(unix)]
    #[test]
    fn which_checks_a_path_where_it_points_and_searches_path_for_a_name() {
        let dir = scratch();
        let runnable_path = script(&dir, "fake-chrome", "exit 0\n");
        std::fs::write(dir.join("plain"), "").unwrap();
        let given = runnable_path.to_string_lossy().into_owned();
        assert_eq!(which_in(&given, None), Some(given.clone()));
        assert_eq!(which_in(&dir.join("plain").to_string_lossy(), None), None);
        assert_eq!(which_in(&dir.to_string_lossy(), None), None);
        let path = std::env::join_paths([Path::new("/nonexistent"), &dir]).unwrap();
        assert_eq!(which_in("fake-chrome", Some(path.clone())), Some(given));
        assert_eq!(which_in("plain", Some(path)), None);
        assert_eq!(which_in("fake-chrome", Some(OsString::new())), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn headless_and_headed_launch_with_their_own_flags() {
        let profile = Path::new("/tmp/profile");
        assert_eq!(
            launch_args(&LocalChrome::default(), profile),
            [
                "--headless=new",
                "--window-size=1280,900",
                "--remote-debugging-port=0",
                "--user-data-dir=/tmp/profile",
                "--no-first-run",
                "--disable-popup-blocking",
                "--no-startup-window",
            ]
        );
        let headed = launch_args(&LocalChrome { headed: true, ..Default::default() }, profile);
        assert_eq!(headed.first().map(String::as_str), Some("--window-size=1280,900"));
        assert_eq!(headed.last().map(String::as_str), Some("about:blank"));
    }

    #[test]
    fn the_password_manager_is_quieted_and_other_preferences_kept() {
        let profile = scratch();
        quiet_password_manager(&profile).unwrap();
        let preferences = profile.join("Default").join("Preferences");
        let read = || serde_json::from_slice::<Value>(&std::fs::read(&preferences).unwrap()).unwrap();
        assert_eq!(
            read(),
            serde_json::json!({
                "credentials_enable_service": false,
                "profile": {"password_manager_enabled": false, "password_manager_leak_detection": false},
            })
        );
        std::fs::write(
            &preferences,
            r#"{"homepage": "x", "profile": {"name": "Me", "password_manager_enabled": true}}"#,
        )
        .unwrap();
        quiet_password_manager(&profile).unwrap();
        assert_eq!(
            read(),
            serde_json::json!({
                "homepage": "x",
                "credentials_enable_service": false,
                "profile": {"name": "Me", "password_manager_enabled": false, "password_manager_leak_detection": false},
            })
        );
        for garbage in [&b"[1, 2]"[..], b"not json", b"\xff\xfe"] {
            std::fs::write(&preferences, garbage).unwrap();
            let error = quiet_password_manager(&profile).unwrap_err();
            assert_eq!(error.to_string(), format!("{} is not a Chrome preferences file", preferences.display()));
        }
        std::fs::remove_dir_all(profile).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_chrome_that_exits_at_start_fails_at_once_with_its_own_error() {
        let dir = scratch();
        let binary =
            script(&dir, "chrome", "echo 'Running as root without --no-sandbox is not supported' >&2\nexit 1\n");
        let started = Instant::now();
        let options = LocalChrome { binary: Some(binary.to_string_lossy().into_owned()), ..Default::default() };
        let error = LocalChromeProcess::launch(&options).await.err().unwrap();
        assert!(error.to_string().contains("status 1 before DevTools started:\nRunning as root"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_chrome_is_reported_as_such() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let options = LocalChrome { binary: Some("/nonexistent/chrome".into()), ..Default::default() };
        let error = runtime.block_on(LocalChromeProcess::launch(&options)).err().unwrap();
        assert_eq!(error.to_string(), "Chrome is not installed");
    }

    /// A stand-in Chrome that writes the port of a fake `/json/version` into its profile, as Chrome does.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_launched_chrome_is_found_by_its_profile_and_cleaned_up_on_close() {
        use crate::adapters::http::fake::{FakeServer, json};
        let server = FakeServer::start(|_, _| {
            json(200, serde_json::json!({"webSocketDebuggerUrl": "ws://127.0.0.1:1/devtools/browser/x"}))
        })
        .await;
        let port = server.base.rsplit(':').next().unwrap().to_owned();
        let dir = scratch();
        let body = format!(
            "for arg; do case \"$arg\" in --user-data-dir=*) dir=\"${{arg#--user-data-dir=}}\";; esac; done\n\
             printf '{port}\\n/devtools/browser/x\\n' > \"$dir/DevToolsActivePort\"\nexec sleep 30\n"
        );
        let binary = script(&dir, "chrome", &body);
        for headed in [false, true] {
            let options = LocalChrome { binary: Some(binary.to_string_lossy().into_owned()), headed, profile: None };
            let (mut chrome, connection) = LocalChromeProcess::launch(&options).await.unwrap();
            assert_eq!(connection.cdp_url, "ws://127.0.0.1:1/devtools/browser/x");
            assert!(!connection.remote);
            assert_eq!(connection.foreground, !headed);
            let profile = chrome.temp_profile.clone().unwrap();
            assert!(profile.join("Default").join("Preferences").exists());
            chrome.close().await;
            assert!(!profile.exists());
            assert!(chrome.child.is_none());
        }
        // A kept profile stays, and a stale DevToolsActivePort in it is replaced rather than read.
        let kept = dir.join("kept");
        std::fs::create_dir(&kept).unwrap();
        std::fs::write(kept.join("DevToolsActivePort"), "1\n").unwrap();
        let options = LocalChrome {
            binary: Some(binary.to_string_lossy().into_owned()),
            headed: false,
            profile: Some(kept.clone()),
        };
        let (mut chrome, _) = LocalChromeProcess::launch(&options).await.unwrap();
        chrome.close().await;
        assert!(kept.join("Default").join("Preferences").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn chrome_is_killed_and_reaped_on_shutdown_timeout() {
        let dir = scratch();
        let binary = script(&dir, "chrome", "trap '' TERM\nexec sleep 30\n");
        let child = Command::new(&binary).kill_on_drop(true).spawn().unwrap();
        let mut chrome = LocalChromeProcess { child: Some(child), temp_profile: None, log_path: None };
        let started = Instant::now();
        chrome.close_within(Duration::from_millis(300)).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(chrome.child.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_home_relative_profile_is_expanded() {
        let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
        if let Some(home) = home {
            assert_eq!(expand_user(Path::new("~/profiles/a")), home.join("profiles/a"));
        }
        assert_eq!(expand_user(Path::new("/abs/~")), PathBuf::from("/abs/~"));
    }
}
