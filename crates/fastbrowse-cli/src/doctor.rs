//! `fastbrowse doctor`: one pass over what a run needs, printed as `ok`, `FAIL` or `skip` lines, exiting 1 when any
//! check failed. A setup problem that used to surface as a bare CDP error now names itself here first.
//!
//! It reports names and paths only, never a secret's value: a missing key is named by its variable, never printed.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

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

async fn probe_version(command: &mut Command, limit: Duration) -> Result<String, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("could not run --version: {error}"))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut bytes = Vec::new();
    // A wrapper may keep stdout open after exiting, so the deadline covers reading and waiting together.
    let result = tokio::time::timeout(limit, async { tokio::try_join!(child.wait(), stdout.read_to_end(&mut bytes)) });
    let status = match result.await {
        Ok(Ok((status, _))) => status,
        failure => {
            drop(stdout);
            // Kill and reap the child even if its I/O failed before the deadline.
            let _ = child.kill().await;
            return Err(match failure {
                Ok(Err(error)) => format!("could not read --version: {error}"),
                _ => format!("--version timed out after {} ms", limit.as_millis()),
            });
        }
    };
    if !status.success() {
        return Err(format!("--version exited with {status}"));
    }
    let text = String::from_utf8_lossy(&bytes);
    let version = text.trim();
    if version.is_empty() {
        return Err("--version returned no output".into());
    }
    Ok(version.into())
}

/// Whether the temporary directory can be written to, which a download, a recording or an attachment needs.
fn temp_space() -> (Level, String) {
    let dir = std::env::temp_dir();
    match std::fs::File::create(dir.join(format!("fastbrowse-doctor-{}", std::process::id()))) {
        Ok(_) => {
            let _ = std::fs::remove_file(dir.join(format!("fastbrowse-doctor-{}", std::process::id())));
            (Level::Ok, format!("temp space: {} is writable", dir.display()))
        }
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
