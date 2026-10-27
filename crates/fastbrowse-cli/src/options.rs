//! Port of `src/fastbrowse/options.py`: the rules an entry point applies to what its operator asked for, written once
//! for the CLI, the MCP server and `serve`.
//!
//! A rule with two copies is a rule that can come to mean two things: the MCP server would keep letting `--headed`
//! unset `FASTBROWSE_HEADED` long after the CLI stopped, and nothing would fail.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fastbrowse_clients::environment::{ConfigurationError, Settings};
use fastbrowse_core::models::{CostBreakdown, LocalChrome, RunResult, SecretValue, Status, StepResult};
use fastbrowse_core::py::{self, text};
use indexmap::IndexMap;
use serde_json::Value;

fn refuse<T>(message: impl Into<String>) -> Result<T, ConfigurationError> {
    Err(ConfigurationError(message.into()))
}

/// Browser Use Cloud unless the operator asked for local Chrome: `--local`, or a headed window or kept profile, which
/// only local Chrome has, whether from its flag or from FASTBROWSE_HEADED / FASTBROWSE_PROFILE.
///
/// A cloud profile or a country asked of local Chrome is refused: the run would start signed out, or browse from this
/// machine's address, and report neither.
pub fn cloud(
    local: bool,
    chrome: &LocalChrome,
    cloud_profile: Option<&str>,
    proxy_country: Option<&str>,
) -> Result<bool, ConfigurationError> {
    let on_cloud = !(local || chrome.headed || chrome.profile.is_some());
    for (flag, value) in [("--cloud-profile", cloud_profile), ("--proxy-country", proxy_country)] {
        if value.is_some() && !on_cloud {
            return refuse(format!(
                "{flag} needs the cloud browser: drop it, or drop --local, --headed and --profile \
                 (and FASTBROWSE_HEADED / FASTBROWSE_PROFILE)"
            ));
        }
    }
    Ok(on_cloud)
}

/// What `handed_over` weighs besides the browser it is handed.
pub struct HandOver<'a> {
    pub cdp_url: Option<&'a str>,
    pub cdp_port: Option<i64>,
    pub attach: bool,
    pub target_match: Option<&'a str>,
    pub local: bool,
    pub cloud_asked: bool,
    pub chrome: &'a LocalChrome,
    pub cloud_profile: Option<&'a str>,
    pub proxy_country: Option<&'a str>,
}

/// Whether the run drives a browser the operator already runs, named by URL or by port.
///
/// Attaching replaces every option that shapes a started browser, so combining them is refused the way
/// `--cloud-profile` on local Chrome is: as a configuration error, before any browser work.
pub fn handed_over(asked: &HandOver<'_>) -> Result<bool, ConfigurationError> {
    if asked.cdp_url.is_some() && asked.cdp_port.is_some() {
        return refuse("--cdp-url and --cdp-port both name a browser to attach to; pass one");
    }
    if asked.cdp_url.is_none() && asked.cdp_port.is_none() {
        if asked.attach || asked.target_match.is_some() {
            return refuse("--attach and --target-match need --cdp-url or --cdp-port");
        }
        return Ok(false);
    }
    let flag = if asked.cdp_url.is_some() { "--cdp-url" } else { "--cdp-port" };
    let conflicts: Vec<&str> = [
        ("--local", asked.local),
        ("--cloud", asked.cloud_asked),
        ("--headed", asked.chrome.headed),
        ("--profile", asked.chrome.profile.is_some()),
        ("--cloud-profile", asked.cloud_profile.is_some()),
        ("--proxy-country", asked.proxy_country.is_some()),
    ]
    .into_iter()
    .filter_map(|(conflict, on)| on.then_some(conflict))
    .collect();
    if !conflicts.is_empty() {
        return refuse(format!("{flag} attaches to a browser already running; drop {}", conflicts.join(", ")));
    }
    if let Some(url) = asked.cdp_url
        && !(url.starts_with("ws://") || url.starts_with("wss://"))
    {
        return refuse(format!("--cdp-url expects a ws:// or wss:// URL, got {}", text::repr(url)));
    }
    if let Some(port) = asked.cdp_port
        && !(0 < port && port < 65536)
    {
        return refuse(format!("--cdp-port expects a port from 1 to 65535, got {port}"));
    }
    Ok(true)
}

/// A recording is encoded by ffmpeg, and without it the run would browse to the end and save nothing.
pub fn recording(record: Option<&Path>) -> Result<(), ConfigurationError> {
    if record.is_some() && which("ffmpeg").is_none() {
        return refuse("--record needs ffmpeg on PATH");
    }
    Ok(())
}

/// A two-letter code, lowercased as Browser Use Cloud expects; a shop serves the visitor's country from it. The error
/// is argparse's `ArgumentTypeError` text.
pub fn country_code(value: &str) -> Result<String, String> {
    let code = text::strip(value).to_lowercase();
    if code.chars().count() != 2 || !code.chars().all(char::is_alphabetic) {
        return Err(format!("{} is not a two-letter country code, e.g. uk", text::repr(value)));
    }
    if code == "gb" {
        // ISO says gb; Browser Use's code for the United Kingdom is uk, and it would reject gb only at browser start.
        return Err("Browser Use's code for the United Kingdom is uk, not gb".into());
    }
    Ok(code)
}

/// The country a cloud browser browses from: the one asked for, and the United States when none was.
pub fn proxy_country(asked: Option<&str>) -> String {
    asked.unwrap_or("us").to_string()
}

/// The cloud browser's key when the run wants one; None runs local Chrome.
pub fn browser_key(settings: &Settings, on_cloud: bool) -> Result<Option<String>, ConfigurationError> {
    if on_cloud { settings.browser_key().map(|key| Some(key.to_string())) } else { Ok(None) }
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
        would_fire: Vec::new(),
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
    let executable = |path: &Path| {
        let Ok(metadata) = path.metadata() else { return false };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            metadata.is_file()
        }
    };
    if name.contains(std::path::MAIN_SEPARATOR) || name.contains('/') {
        let path = PathBuf::from(name);
        return executable(&path).then_some(path);
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|directory| directory.join(name)).find(|path| executable(path))
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
    settings.jev(&http)?;
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

    #[test]
    fn a_country_is_two_letters_and_never_gb() {
        assert_eq!(country_code(" UK ").unwrap(), "uk");
        assert_eq!(country_code("gb").unwrap_err(), "Browser Use's code for the United Kingdom is uk, not gb");
        assert_eq!(country_code("usa").unwrap_err(), "'usa' is not a two-letter country code, e.g. uk");
    }

    #[test]
    fn a_handed_over_browser_refuses_what_shapes_a_started_one() {
        let chrome = LocalChrome { binary: None, headed: true, profile: None };
        let asked = HandOver {
            cdp_url: Some("ws://b"),
            cdp_port: None,
            attach: false,
            target_match: None,
            local: true,
            cloud_asked: false,
            chrome: &chrome,
            cloud_profile: None,
            proxy_country: Some("uk"),
        };
        assert_eq!(
            handed_over(&asked).unwrap_err().0,
            "--cdp-url attaches to a browser already running; drop --local, --headed, --proxy-country"
        );
    }
}
