//! The rules an entry point applies to what its operator asked for, written once
//! for the CLI, the MCP server and `serve`.
//!
//! A rule with two copies is a rule that can come to mean two things: the MCP server would keep letting `--headed`
//! unset `FASTBROWSE_HEADED` long after the CLI stopped, and nothing would fail.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fastbrowse_clients::environment::{ConfigurationError, Settings};
use fastbrowse_core::models::{CostBreakdown, LocalChrome, RunResult, SecretValue, Status, StepResult};
use fastbrowse_core::protocol::RunTask;
use fastbrowse_core::py::{self, text};
use indexmap::IndexMap;
use serde_json::Value;

fn refuse<T>(message: impl Into<String>) -> Result<T, ConfigurationError> {
    Err(ConfigurationError(message.into()))
}

/// `fastbrowse_run::on_cloud` for a task the entry point has already given its Chrome.
pub fn on_cloud(task: &RunTask) -> bool {
    fastbrowse_run::on_cloud(task, &task.chrome.clone().unwrap_or_default())
}

/// What the machine and the settings must hold for `task`, found before a browser opens: ffmpeg for a recording,
/// Chrome for a local run, and the model keys. Returns the cloud browser's key when the run starts one.
pub fn environment(task: &RunTask, settings: &Settings) -> Result<Option<String>, ConfigurationError> {
    if task.record.is_some() && which("ffmpeg").is_none() {
        return refuse("record needs ffmpeg on PATH");
    }
    let cloud = on_cloud(task);
    let handed = task.cdp_url.is_some() || task.cdp_port.is_some();
    let binary = task.chrome.as_ref().and_then(|chrome| chrome.binary.as_deref());
    if !(cloud || handed || task.fetch) && find_chrome(binary).is_none() {
        return refuse("Chrome was not found: install it, or name it in FASTBROWSE_CHROME or in `chrome.binary`");
    }
    let key = if cloud { Some(settings.browser_key()?.to_string()) } else { None };
    check_model_keys(settings)?;
    Ok(key)
}

/// The flags add to FASTBROWSE_HEADED and FASTBROWSE_PROFILE; they cannot unset them. `binary` replaces
/// FASTBROWSE_CHROME instead: each names one program, and a caller who names one for this run means that one.
pub fn chrome(settings: &Settings, headed: bool, profile: Option<&Path>, binary: Option<&str>) -> LocalChrome {
    let local = settings.local_chrome();
    LocalChrome {
        binary: binary.filter(|binary| !binary.is_empty()).map(str::to_string).or(local.binary),
        headed: headed || local.headed,
        profile: profile.map(Path::to_path_buf).or(local.profile),
    }
}

/// `NAME=ENV_VAR`, as an argparse type.
pub fn env_secret(pair: &str) -> Result<(String, String), String> {
    match pair.split_once('=') {
        Some((name, variable)) if !name.is_empty() && !variable.is_empty() => {
            Ok((name.to_string(), variable.to_string()))
        }
        _ => Err(format!("expected NAME=ENV_VAR, got {}", text::repr(pair))),
    }
}

/// `NAME=ENV_VAR` or `NAME=ENV_VAR@ORIGIN`, with the origin absent when none was given.
///
/// The name is taken first: a secret may be named for the account it belongs to, and `user@example.com=PW@...` has an
/// `@` in its name before the one that introduces the origin. Absent means "not stated here", never "any origin".
pub fn scoped_secret(value: &str) -> Result<(String, String, Option<String>), String> {
    let (named, equals, rest) = match value.split_once('=') {
        Some((named, rest)) => (named, "=", rest),
        None => (value, "", ""),
    };
    let (variable, origin) = match rest.split_once('@') {
        Some((variable, origin)) => (variable, Some(origin)),
        None => (rest, None),
    };
    let (name, variable) = env_secret(&format!("{named}{equals}{variable}"))?;
    let Some(origin) = origin else { return Ok((name, variable, None)) };
    let parts = SplitUrl::of(origin);
    if !matches!(parts.scheme.as_str(), "http" | "https") || parts.hostname().is_empty() {
        return Err(format!("expected NAME=ENV_VAR@https://host, got {}", text::repr(value)));
    }
    if !matches!(parts.path, "" | "/") || !parts.query.is_empty() || !parts.fragment.is_empty() {
        return Err(format!("{} is not an origin: drop everything after the host", text::repr(origin)));
    }
    Ok((name, variable, Some(origin.to_string())))
}

/// `urllib.parse.urlsplit`, for the parts a flag's origin is checked by.
pub struct SplitUrl<'a> {
    pub scheme: String,
    pub netloc: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub fragment: &'a str,
}

impl<'a> SplitUrl<'a> {
    pub fn of(url: &'a str) -> SplitUrl<'a> {
        let url = url.trim_start_matches(|c: char| c <= ' ');
        let (mut scheme, mut rest) = (String::new(), url);
        if let Some((head, tail)) = url.split_once(':') {
            let mut chars = head.chars();
            let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
                && chars.all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
            if valid {
                scheme = head.to_ascii_lowercase();
                rest = tail;
            }
        }
        let (rest, fragment) = rest.split_once('#').unwrap_or((rest, ""));
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (netloc, path) = match rest.strip_prefix("//") {
            Some(after) => after.split_at(after.find('/').unwrap_or(after.len())),
            None => ("", rest),
        };
        SplitUrl { scheme, netloc, path, query, fragment }
    }

    /// `.hostname`: the host without credentials or port, lowercased, an IPv6 address without its brackets.
    pub fn hostname(&self) -> String {
        let host = self.netloc.rsplit_once('@').map_or(self.netloc, |(_, host)| host);
        let host = match host.strip_prefix('[') {
            Some(bracketed) => bracketed.split(']').next().unwrap_or(""),
            None => host.split(':').next().unwrap_or(""),
        };
        host.to_lowercase()
    }
}

/// The variables a `--secret` names that the environment does not hold, so a run fails before a browser opens.
pub fn unset_variables(pairs: &[(String, String, Option<String>)], environ: &HashMap<String, String>) -> Vec<String> {
    let missing: BTreeSet<&String> =
        pairs.iter().map(|(_, variable, _)| variable).filter(|variable| !environ.contains_key(*variable)).collect();
    missing.into_iter().cloned().collect()
}

/// The process environment, as `os.environ` is read.
pub fn environ() -> HashMap<String, String> {
    std::env::vars_os().filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?))).collect()
}

/// Declared secrets and a vault item's, refusing a name both supply rather than letting one win silently. Each entry
/// point reports the error in its own terms.
pub fn merged_secrets(
    values: &IndexMap<String, SecretValue>,
    vault: &IndexMap<String, SecretValue>,
) -> Result<IndexMap<String, SecretValue>, String> {
    let clash: BTreeSet<&String> = values.keys().filter(|name| vault.contains_key(*name)).collect();
    if !clash.is_empty() {
        let names: Vec<&str> = clash.into_iter().map(String::as_str).collect();
        return Err(format!("a declared secret and the vault item both set {}", names.join(", ")));
    }
    let mut merged = values.clone();
    merged.extend(vault.iter().map(|(name, value)| (name.clone(), value.clone())));
    Ok(merged)
}

/// A run with nothing to report but why it failed, in the shape of any other, so a caller branches on `status`.
pub fn error_result(error: impl Into<String>) -> RunResult {
    RunResult {
        status: Status::Error,
        budget: None,
        answer: None,
        data: Value::Null,
        evidence: Vec::new(),
        steps: Vec::new(),
        cost: CostBreakdown { lines: Vec::new() },
        artifacts: Vec::new(),
        citations: Vec::new(),
        error: Some(error.into()),
        final_url: None,
        final_frame: None,
        recordings: Vec::new(),
        wall: None,
    }
}

/// One step as a line: what was done, and to what.
pub fn step_label(step: &StepResult) -> String {
    format!("{} {}", step.operation.as_str(), step.target.as_deref().unwrap_or("")).trim().to_string()
}

/// Write a run's final-page image where its caller can open it, and return the path.
///
/// The image travels only in `RunResult.final_frame`, which JSON leaves out. Without a downloads directory it goes to a
/// fresh temporary one, since the run's own scratch space is deleted when the run ends.
pub fn keep_screenshot(frame: &[u8], directory: Option<&Path>) -> std::io::Result<PathBuf> {
    let root = match directory {
        Some(directory) => std::fs::canonicalize(directory).or_else(|_| std::path::absolute(directory))?,
        None => temporary_directory("fastbrowse-")?,
    };
    // Content-addressed like downloads, so a second run never overwrites the first one's image.
    let path = root.join("screenshots").join(&py::sha256_hex(frame)[..16]).join("screenshot.png");
    std::fs::create_dir_all(path.parent().unwrap_or(&root))?;
    std::fs::write(&path, frame)?;
    Ok(path)
}

/// `tempfile.mkdtemp(prefix=...)`: a new directory only this call created.
fn temporary_directory(prefix: &str) -> std::io::Result<PathBuf> {
    let base = std::env::temp_dir();
    let seed = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos());
    for attempt in 0..100u128 {
        let tag = py::sha256_hex(format!("{seed}-{}-{attempt}", std::process::id()).as_bytes());
        let path = base.join(format!("{prefix}{}", &tag[..8]));
        match std::fs::create_dir(&path) {
            Ok(()) => return std::fs::canonicalize(&path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "no free temporary directory name"))
}

/// `shutil.which(name)`: a path with a separator is checked as given, a bare name looked up on PATH.
pub fn which(name: &str) -> Option<PathBuf> {
    fastbrowse_run::which(name).map(PathBuf::from)
}

/// Chrome, from `Settings.chrome` (a name or path that replaces discovery rather than joining it) or the usual names.
pub fn find_chrome(binary: Option<&str>) -> Option<PathBuf> {
    if let Some(binary) = binary.filter(|binary| !binary.is_empty()) {
        return which(binary);
    }
    let mut names: Vec<String> =
        ["google-chrome-stable", "google-chrome", "chromium", "chromium-browser", "chrome"].map(String::from).to_vec();
    names.push("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into());
    // Chrome's per-machine and per-user installs, which its Windows installer leaves off PATH.
    for root in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
        if let Some(root) = std::env::var_os(root).filter(|root| !root.is_empty()) {
            names.push(
                Path::new(&root)
                    .join("Google")
                    .join("Chrome")
                    .join("Application")
                    .join("chrome.exe")
                    .display()
                    .to_string(),
            );
        }
    }
    names.iter().find_map(|name| which(name))
}

/// The checks a run makes on its first model calls, made before it starts: a missing key is a configuration error
/// now rather than a failed run later. The clients are built and dropped; nothing is sent.
pub fn check_model_keys(settings: &Settings) -> Result<(), ConfigurationError> {
    use fastbrowse_clients::http::{Http, Request, Response, Transport, TransportError};
    use fastbrowse_core::BoxFuture;

    struct Unsent;
    impl Transport for Unsent {
        fn send(&self, _: Request) -> BoxFuture<'static, Result<Response, TransportError>> {
            Box::pin(async { Err(TransportError::Fatal("UnsupportedProtocol")) })
        }
    }
    let http = Http::new(std::sync::Arc::new(Unsent));
    settings.llm(&http)?;
    settings.decision(&http)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_flag_takes_the_name_before_the_origin() {
        assert_eq!(
            scoped_secret("user@example.com=LOGIN@https://shop.example").unwrap(),
            ("user@example.com".into(), "LOGIN".into(), Some("https://shop.example".into()))
        );
        assert_eq!(scoped_secret("p=V").unwrap(), ("p".into(), "V".into(), None));
        assert_eq!(scoped_secret("abc").unwrap_err(), "expected NAME=ENV_VAR, got 'abc'");
        assert_eq!(scoped_secret("=V@https://a.example").unwrap_err(), "expected NAME=ENV_VAR, got '=V'");
        assert_eq!(
            scoped_secret("p=V@https://a.example/login").unwrap_err(),
            "'https://a.example/login' is not an origin: drop everything after the host"
        );
        assert_eq!(scoped_secret("p=V@ftp://a").unwrap_err(), "expected NAME=ENV_VAR@https://host, got 'p=V@ftp://a'");
    }
}
