//! `fastbrowse doctor`: one pass over what a run needs, printed as `ok`, `FAIL` or `skip` lines, exiting 1 when any
//! check failed. A setup problem that used to surface as a bare CDP error now names itself here first.
//!
//! It reports names and paths only, never a secret's value: a missing key is named by its variable, never printed.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

#[cfg(not(windows))]
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use fastbrowse_clients::environment::Settings;

use crate::cli::Build;
use crate::options;

/// How a check ended. `Skip` is a check that cannot be made on this machine, not a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Ok,
    Fail,
    Skip,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Fail => "FAIL",
            Level::Skip => "skip",
        }
    }
}

/// The report and the exit code: 1 when any line failed, 0 otherwise.
fn report(lines: &[(Level, String)]) -> (String, i32) {
    let mut out = String::new();
    for (level, message) in lines {
        out.push_str(&format!("{:<4}  {message}\n", level.label()));
    }
    let failed = lines.iter().any(|(level, _)| *level == Level::Fail);
    (out, i32::from(failed))
}

/// `fastbrowse doctor`: build the checks, run them, and report. `program` is the command this install runs as, which
/// the extension host registration is keyed on.
pub async fn command(build: &Build, settings: &Settings, program: &str) -> (String, i32) {
    let mut lines: Vec<(Level, String)> = Vec::new();
    lines.push((Level::Ok, build.version_line()));

    // The model keys, named by their variable and never by value. A key that is missing is why a run stops before it
    // starts, and the error already names the one to set.
    match options::check_model_keys(settings) {
        Ok(()) => lines.push((Level::Ok, "model keys: present".into())),
        Err(error) => lines.push((Level::Fail, format!("model keys: {}", error.0))),
    }

    // Chrome, which a run that starts its own browser needs and which the extension also lives inside.
    match options::find_chrome(settings.chrome.as_deref()) {
        Some(path) => lines.push(check_chrome(&path).await),
        None => lines.push((Level::Fail, "chrome: not found; install Chrome, or name it in FASTBROWSE_CHROME".into())),
    }

    // The extension's host, as `fastbrowse extension status` reports it. Registration is what a bare CDP error is
    // usually missing; the check reuses that command so the two never disagree.
    let (status, code) = crate::extension::command(program, &["status".to_string()]);
    let status = status.trim();
    if code == 0 {
        lines.push((Level::Ok, format!("extension host: {status}")));
    } else {
        lines.push((Level::Fail, format!("extension host: {status}")));
    }

    lines.push(temp_space());

    match memory() {
        Some(message) => lines.push((Level::Ok, message)),
        None => lines.push((Level::Skip, "memory: not reported on this system".into())),
    }

    // Orphan browsers are not counted here: reading the process table portably needs a dependency this binary does not
    // carry. Said out loud rather than passed off as clean.
    lines.push((Level::Skip, "orphan browsers: not checked".into()));

    report(&lines)
}

async fn check_chrome(binary: &Path) -> (Level, String) {
    let mut command = Command::new(binary);
    command.arg("--version");
    match probe_version(&mut command, Duration::from_secs(5)).await {
        Ok(version) => (Level::Ok, format!("chrome: {} ({version})", binary.display())),
        Err(error) => (Level::Fail, format!("chrome: {} {error}", binary.display())),
    }
}

#[cfg(not(windows))]
async fn probe_version(command: &mut Command, limit: Duration) -> Result<String, String> {
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("could not run --version: {error}"))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut bytes = Vec::new();
    // Read before waiting: a wrapper can leave stdout open in a helper after exiting, and reaping it first would
    // release the PID that names the process group we need to signal if the read times out.
    let result = tokio::time::timeout(limit, async {
        stdout.read_to_end(&mut bytes).await?;
        child.wait().await
    });
    let status = match result.await {
        Ok(Ok(status)) => status,
        failure => {
            drop(stdout);
            #[cfg(unix)]
            if let Some(pid) = child.id().and_then(|id| libc::pid_t::try_from(id).ok()) {
                // SAFETY: the unreaped child still owns this PID, and process_group(0) made it the group leader.
                // A negative PID signals only that group, including helpers the wrapper left behind.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
            // Reap the child after the group signal, or kill it directly where there is no group signal.
            let _ = child.kill().await;
            return Err(match failure {
                Ok(Err(error)) => format!("could not read --version: {error}"),
                _ => format!("--version timed out after {} ms", limit.as_millis()),
            });
        }
    };
    version_text(status, &bytes)
}

#[cfg(windows)]
async fn probe_version(command: &mut Command, limit: Duration) -> Result<String, String> {
    use std::io::Read;

    // A wrapper's helper can keep a stdout pipe open after the wrapper exits. Tokio's Windows pipe reader waits
    // on a blocking thread, so cancelling that read would still leave the CLI waiting during runtime shutdown.
    let output = tempfile::NamedTempFile::new().map_err(|error| format!("could not capture --version: {error}"))?;
    let writer = output.reopen().map_err(|error| format!("could not capture --version: {error}"))?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer))
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("could not run --version: {error}"))?;
    let status = match tokio::time::timeout(limit, child.wait()).await {
        Ok(Ok(status)) => status,
        failure => {
            let _ = child.kill().await;
            return Err(match failure {
                Ok(Err(error)) => format!("could not wait for --version: {error}"),
                _ => format!("--version timed out after {} ms", limit.as_millis()),
            });
        }
    };
    // Reopen an independent file cursor. A helper still holding the inherited handle can change its cursor and
    // append data, so read only the bytes present when the wrapper exited.
    let bytes = (|| {
        let reader = output.reopen()?;
        let length = reader.metadata()?.len();
        let mut bytes = Vec::new();
        reader.take(length).read_to_end(&mut bytes)?;
        Ok::<_, std::io::Error>(bytes)
    })()
    .map_err(|error| format!("could not read --version: {error}"))?;
    version_text(status, &bytes)
}

fn version_text(status: std::process::ExitStatus, bytes: &[u8]) -> Result<String, String> {
    if !status.success() {
        return Err(format!("--version exited with {status}"));
    }
    let text = String::from_utf8_lossy(bytes);
    let version = text.trim();
    if version.is_empty() {
        return Err("--version returned no output".into());
    }
    Ok(version.into())
}

/// Whether the temporary directory can be written to, which a download, a recording or an attachment needs.
fn temp_space() -> (Level, String) {
    let dir = std::env::temp_dir();
    match tempfile::NamedTempFile::new_in(&dir) {
        Ok(_) => (Level::Ok, format!("temp space: {} is writable", dir.display())),
        Err(error) => (Level::Fail, format!("temp space: {} is not writable: {error}", dir.display())),
    }
}

/// Total and available memory from `/proc/meminfo`, where the kernel offers it.
fn memory() -> Option<String> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let field = |name: &str| {
        text.lines().find_map(|line| {
            let (key, rest) = line.split_once(':')?;
            (key == name).then(|| rest.split_whitespace().next().map(str::to_string)).flatten()
        })
    };
    let total = field("MemTotal")?;
    let available = field("MemAvailable")?;
    Some(format!("memory: {available} kB available of {total} kB"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[tokio::test]
    async fn chrome_probe_captures_windows_command_output() {
        let mut shell = Command::new("cmd.exe");
        shell.args(["/d", "/c", "echo Chrome 123"]);
        assert_eq!(probe_version(&mut shell, Duration::from_secs(5)).await.unwrap(), "Chrome 123");
    }

    #[cfg(windows)]
    #[test]
    fn chrome_probe_runtime_exits_while_helper_holds_stdout() {
        let fixture = tempfile::tempdir().unwrap();
        let stop = fixture.path().join("stop");
        let done = fixture.path().join("done");
        let fixture_path = fixture.path().to_path_buf();
        let (send, receive) = std::sync::mpsc::channel();
        let probe = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--ignored", "--exact", "doctor::tests::chrome_probe_windows_fixture", "--nocapture"])
                .env("FASTBROWSE_DOCTOR_FIXTURE", &fixture_path)
                .env_remove("FASTBROWSE_DOCTOR_HELPER");
            let result = runtime.block_on(probe_version(&mut command, Duration::from_secs(3)));
            drop(runtime);
            send.send(result).unwrap();
        });

        let result = receive.recv_timeout(Duration::from_secs(5));
        std::fs::write(&stop, "").unwrap();
        probe.join().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !done.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(done.exists(), "the fixture helper exited");
        assert!(result.expect("the probe and runtime exited before the helper").unwrap().contains("Chrome 123"));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "launched by the Windows version-probe regression"]
    fn chrome_probe_windows_fixture() {
        let directory = std::path::PathBuf::from(std::env::var_os("FASTBROWSE_DOCTOR_FIXTURE").unwrap());
        if std::env::var_os("FASTBROWSE_DOCTOR_HELPER").is_some() {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !directory.join("stop").exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            std::fs::write(directory.join("done"), "").unwrap();
        } else {
            let helper = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--ignored", "--exact", "doctor::tests::chrome_probe_windows_fixture", "--nocapture"])
                .env("FASTBROWSE_DOCTOR_HELPER", "1")
                .spawn()
                .unwrap();
            // Simulate a wrapper exiting without closing the helper's copy of stdout.
            drop(helper);
            println!("Chrome 123");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn chrome_probe_requires_output_and_success() {
        let limit = Duration::from_secs(5);
        let mut shell = Command::new("/bin/sh");
        shell.args(["-c", "printf 'Chrome 123\n'"]);
        assert_eq!(probe_version(&mut shell, limit).await.unwrap(), "Chrome 123");
        let mut shell = Command::new("/bin/sh");
        shell.args(["-c", "printf 'Chrome 123\n'; exit 7"]);
        assert!(probe_version(&mut shell, limit).await.unwrap_err().contains("exit status: 7"));
        let mut shell = Command::new("/bin/sh");
        shell.args(["-c", "exit 0"]);
        assert_eq!(probe_version(&mut shell, limit).await.unwrap_err(), "--version returned no output");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn chrome_probe_times_out_after_stdout_closes() {
        let mut shell = Command::new("/bin/sh");
        // The shell itself stalls, so this test leaves no separate process to clean up.
        shell.args(["-c", "exec 1>&-; while :; do :; done"]);
        let error = probe_version(&mut shell, Duration::from_millis(50)).await.unwrap_err();
        assert_eq!(error, "--version timed out after 50 ms");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn timed_out_chrome_probe_kills_helper_after_wrapper_exits() {
        let pid_file = std::env::temp_dir().join(format!("fastbrowse-doctor-helper-{}", std::process::id()));
        let mut shell = Command::new("/bin/sh");
        // The helper inherits the pipe. It exits on its own after a few seconds even if group cleanup regresses.
        shell.args(["-c", r#"sleep 5 & printf '%s' "$!" > "$1"; exit 0"#, "wrapper"]).arg(&pid_file);
        let error = probe_version(&mut shell, Duration::from_millis(300)).await.unwrap_err();
        let pid = std::fs::read_to_string(&pid_file);
        std::fs::remove_file(&pid_file).unwrap();
        let stat = format!("/proc/{}/stat", pid.unwrap().parse::<u32>().unwrap());
        assert_eq!(error, "--version timed out after 300 ms");
        tokio::time::timeout(Duration::from_secs(1), async {
            // A killed helper may remain a zombie until its new Unix parent reaps it.
            while std::fs::read_to_string(&stat)
                .is_ok_and(|text| text.rsplit_once(") ").is_some_and(|(_, state)| !state.starts_with('Z')))
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the timeout kills the wrapper's helper");
    }

    #[test]
    fn a_failed_line_makes_the_exit_code_one() {
        let (text, code) = report(&[(Level::Ok, "a".into()), (Level::Fail, "b".into())]);
        assert_eq!(code, 1);
        assert!(text.contains("FAIL  b"));
    }

    #[test]
    fn skips_and_oks_alone_exit_zero() {
        let (text, code) = report(&[(Level::Ok, "a".into()), (Level::Skip, "c".into())]);
        assert_eq!(code, 0);
        assert!(text.contains("skip  c"));
    }
}
