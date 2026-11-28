//! `--open-chrome`: start the user's Chrome when no browser has the extension connected, then wait for it to connect.
//!
//! An extension run with no connected browser ends `unavailable`, and a Chrome that closed between two identical
//! commands sent the user away to open it for something the CLI can do (#1510). This runs before the run, so the run
//! itself is unchanged: it finds the host the same way it always does, and ends `unavailable` when none connects.

use std::path::{Path, PathBuf};
use std::time::Duration;

use fastbrowse_browser::extension::protocol::{connected_hosts, names_host};
use fastbrowse_clients::environment::ConfigurationError;

/// How long a started Chrome has to load its profile and connect the extension's host. It bounds a Chrome that never
/// connects, such as one whose profile lacks the extension; a connecting one ends the wait as soon as it does.
const CONNECT_WITHIN: Duration = Duration::from_secs(60);

/// The host records are files and sockets, so a connection is seen by reading the directory again.
const POLL_EVERY: Duration = Duration::from_millis(250);

/// Whether a window can open here. A Linux session with neither a X nor a Wayland display is a headless shell, where
/// Chrome has nothing to draw on and the run's own refusal stands.
pub fn desktop_session(environ: &std::collections::HashMap<String, String>) -> bool {
    !cfg!(target_os = "linux")
        || ["DISPLAY", "WAYLAND_DISPLAY"].iter().any(|name| environ.get(*name).is_some_and(|v| !v.is_empty()))
}

/// Start `chrome` with no arguments, so the browser opens the profile the user signs in to and the extension loads
/// there, and leave it running after this process ends.
pub fn start(chrome: &Path) -> Result<(), ConfigurationError> {
    let mut command = std::process::Command::new(chrome);
    command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command
        .spawn()
        .map(drop)
        .map_err(|error| ConfigurationError(format!("--open-chrome could not start {}: {error}", chrome.display())))
}

/// What `ensure_connected` did, for the line the CLI prints.
#[derive(Debug, PartialEq)]
pub enum Opened {
    /// A browser was already connected, or this is a headless shell: nothing was started.
    Nothing,
    /// Chrome was started and its extension connected.
    Connected,
    /// Chrome was started and no extension connected within the wait; the run reports it.
    NotConnected,
}

/// Start a browser with `launch` when no host is connected in `dir` and a window can open, and wait until one is.
/// A host this process cannot reach is a browser that is running, which opening another does not fix. The wait ends
/// when a host the run can use connects: the one `wanted` names, as `pick_host` matches it, else any.
pub async fn ensure_connected(
    dir: &Path,
    wanted: Option<&str>,
    desktop: bool,
    launch: impl FnOnce() -> Result<(), ConfigurationError>,
    within: Duration,
) -> Result<Opened, ConfigurationError> {
    let scan = connected_hosts(dir);
    if !scan.connected.is_empty() || !scan.unreachable.is_empty() || !desktop {
        return Ok(Opened::Nothing);
    }
    launch()?;
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if connected_hosts(dir).connected.iter().any(|(_, record)| wanted.is_none_or(|name| names_host(record, name))) {
            return Ok(Opened::Connected);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(Opened::NotConnected);
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
}

/// The Chrome to start: `binary` when the settings name one, else the usual installs.
pub fn chrome(binary: Option<&str>) -> Result<PathBuf, ConfigurationError> {
    crate::options::find_chrome(binary).ok_or_else(|| {
        ConfigurationError(
            "--open-chrome found no Chrome to start: install it, or name it in FASTBROWSE_CHROME".to_owned(),
        )
    })
}

/// `ensure_connected` for the running CLI, with the real directory, Chrome and wait.
pub async fn open(
    environ: &std::collections::HashMap<String, String>,
    binary: Option<&str>,
    wanted: Option<&str>,
) -> Result<Opened, ConfigurationError> {
    let dir = fastbrowse_browser::extension::protocol::host_dir();
    // Chrome is looked for only once a start is needed, so a machine with only Edge connected still runs.
    let launch = || start(&chrome(binary)?);
    ensure_connected(&dir, wanted, desktop_session(environ), launch, CONNECT_WITHIN).await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use fastbrowse_browser::extension::protocol::{HostRecord, PROTOCOL};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fbx-open-chrome-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A host record and its listening socket, as a connected extension's host leaves them.
    fn connect(dir: &Path, label: &str) -> std::os::unix::net::UnixListener {
        let socket = dir.join(format!("id{label}.sock"));
        let record = HostRecord {
            browser_id: format!("id{label}"),
            label: label.into(),
            browser: "Chrome".into(),
            version: "150".into(),
            protocol: PROTOCOL,
            pid: 1,
            socket: socket.clone(),
        };
        std::fs::write(dir.join(format!("id{label}.json")), serde_json::to_vec(&record).unwrap()).unwrap();
        std::os::unix::net::UnixListener::bind(&socket).unwrap()
    }

    #[tokio::test]
    async fn a_connected_browser_is_left_alone() {
        let dir = scratch("connected");
        let _host = connect(&dir, "Home");
        let opened =
            ensure_connected(&dir, None, true, || panic!("started a second Chrome"), Duration::from_secs(1)).await;
        assert_eq!(opened.unwrap(), Opened::Nothing);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_headless_shell_starts_nothing_and_keeps_the_runs_refusal() {
        let dir = scratch("headless");
        let opened =
            ensure_connected(&dir, None, false, || panic!("started Chrome with no display"), Duration::from_secs(1))
                .await;
        assert_eq!(opened.unwrap(), Opened::Nothing);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn chrome_is_started_and_waited_for_until_its_host_connects() {
        let dir = scratch("starts");
        let host = std::sync::Mutex::new(None);
        let started = ensure_connected(
            &dir,
            None,
            true,
            || {
                *host.lock().unwrap() = Some(connect(&dir, "Home"));
                Ok(())
            },
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(started.unwrap(), Opened::Connected);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Starts a task that publishes each host after its delay, as browsers connect one after another once Chrome starts.
    fn connect_later(
        dir: &Path,
        hosts: &[(&str, u64)],
    ) -> tokio::task::JoinHandle<Vec<std::os::unix::net::UnixListener>> {
        let dir = dir.to_owned();
        let hosts: Vec<_> = hosts.iter().map(|(label, at)| (label.to_string(), *at)).collect();
        tokio::spawn(async move {
            let start = tokio::time::Instant::now();
            let mut listeners = Vec::new();
            for (label, at) in hosts {
                tokio::time::sleep_until(start + Duration::from_secs(at)).await;
                listeners.push(connect(&dir, &label));
            }
            listeners
        })
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_that_connects_after_the_first_scan_ends_the_wait_before_its_deadline() {
        let dir = scratch("late");
        let started = tokio::time::Instant::now();
        let mut later = None;
        let opened = ensure_connected(
            &dir,
            None,
            true,
            || {
                later = Some(connect_later(&dir, &[("Home", 3)]));
                Ok(())
            },
            CONNECT_WITHIN,
        )
        .await;
        assert_eq!(opened.unwrap(), Opened::Connected);
        assert!(started.elapsed() < CONNECT_WITHIN, "waited out the deadline: {:?}", started.elapsed());
        drop(later.unwrap().await.unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn an_unrelated_browser_connecting_first_does_not_end_the_wait_for_the_named_one() {
        let dir = scratch("named");
        let started = tokio::time::Instant::now();
        let mut later = None;
        let opened = ensure_connected(
            &dir,
            Some("work"),
            true,
            || {
                later = Some(connect_later(&dir, &[("Home", 2), ("Work", 10)]));
                Ok(())
            },
            CONNECT_WITHIN,
        )
        .await;
        assert_eq!(opened.unwrap(), Opened::Connected);
        assert!(started.elapsed() >= Duration::from_secs(10), "ended on Home: {:?}", started.elapsed());
        drop(later.unwrap().await.unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_chrome_that_never_connects_ends_the_wait_and_leaves_the_run_to_report_it() {
        let dir = scratch("never");
        let opened = ensure_connected(&dir, None, true, || Ok(()), CONNECT_WITHIN).await;
        assert_eq!(opened.unwrap(), Opened::NotConnected);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_chrome_that_cannot_start_is_a_configuration_error() {
        let dir = scratch("fails");
        let error =
            ensure_connected(&dir, None, true, || start(Path::new("/nonexistent/fbx-chrome")), Duration::from_secs(1))
                .await
                .unwrap_err();
        assert!(error.to_string().contains("--open-chrome could not start /nonexistent/fbx-chrome"), "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_linux_session_needs_a_display() {
        let env = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        if cfg!(target_os = "linux") {
            assert!(!desktop_session(&env(&[])));
            assert!(!desktop_session(&env(&[("DISPLAY", "")])));
            assert!(desktop_session(&env(&[("WAYLAND_DISPLAY", "wayland-0")])));
        } else {
            assert!(desktop_session(&env(&[])));
        }
    }
}
