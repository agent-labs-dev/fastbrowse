//! Port of `src/fastbrowse/cli.py`: run one task from the terminal.
//!
//! ```text
//! fastbrowse "What is the latest version of httpx?" --start https://pypi.org/
//! fastbrowse "Send the form" --start https://example.com/contact --authorize --json
//! ```
//!
//! A Browser Use Cloud browser by default, with a URL printed to watch it live. The answer goes to stdout, progress
//! and warnings to stderr, and the exit code says how the run ended: 0 only for `complete`.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fastbrowse_clients::environment::ConfigurationError;
use fastbrowse_core::models::{Authorization, Event, Limits, LocalChrome, SecretValue, Status};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::json::{self, Style};
use fastbrowse_core::safety::{ScopedSecrets, origin_of};
use fastbrowse_core::telemetry::{TRACE_ENV, Trace, trace_to};
use indexmap::IndexMap;

use crate::argparse::{Exit, Opt, Parsed, Parser, py_float, py_int};
use crate::bitwarden;
use crate::options::{self, HandOver};
use crate::runner::{RunArgs, RunError, Runner};
use crate::serve::SettingsSource;

pub fn exit_code(status: Status) -> i32 {
    match status {
        Status::Complete => 0,
        Status::Error => 1,
        Status::NeedsConfirmation => 3,
        Status::NeedsLogin => 4,
        Status::NeedsInput => 5,
        Status::Blocked => 6,
        Status::BudgetExceeded => 7,
        Status::Unavailable => 8,
        Status::Stuck => 9,
        Status::Unverified => 10,
        Status::ObservationLimit => 11,
    }
}

/// Where the CLI writes, shared with the run's progress callback.
#[derive(Clone)]
pub struct Streams {
    pub out: Arc<Mutex<dyn Write + Send>>,
    pub err: Arc<Mutex<dyn Write + Send>>,
}

impl Streams {
    pub fn stdio() -> Streams {
        Streams { out: Arc::new(Mutex::new(std::io::stdout())), err: Arc::new(Mutex::new(std::io::stderr())) }
    }

    pub fn print(&self, text: &str) {
        let mut out = self.out.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = writeln!(out, "{text}").and_then(|()| out.flush());
    }

    pub fn eprint(&self, text: &str) {
        let mut err = self.err.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = writeln!(err, "{text}").and_then(|()| err.flush());
    }
}

/// What a run of the CLI depends on besides its arguments, so a test can stand in for each.
pub struct Context {
    pub runner: Arc<dyn Runner>,
    pub settings: SettingsSource,
    pub environ: HashMap<String, String>,
    pub streams: Streams,
    pub build: Build,
}

/// Which commit is running, as `fastbrowse.build.identity()` works it out: the Python command passes it in, and the
/// binary on its own knows only its version, so both are None there.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Build {
    pub git_sha: Option<String>,
    pub git_dirty: Option<bool>,
}

impl Build {
    /// `fastbrowse 0.9.0 (e1d478b, dirty)` from a checkout, and `fastbrowse 0.9.0` from a release.
    pub fn version_line(&self) -> String {
        let mut line = format!("fastbrowse {}", env!("CARGO_PKG_VERSION"));
        if let Some(sha) = &self.git_sha {
            let short: String = sha.chars().take(7).collect();
            let dirty = if self.git_dirty == Some(true) { ", dirty" } else { "" };
            line.push_str(&format!(" ({short}{dirty})"));
        }
        line
    }

    fn fields(&self) -> serde_json::Map<String, serde_json::Value> {
        serde_json::Map::from_iter([
            ("fastbrowse_version".into(), env!("CARGO_PKG_VERSION").into()),
            ("git_sha".into(), self.git_sha.clone().into()),
            ("git_dirty".into(), self.git_dirty.into()),
        ])
    }
}

pub fn parser(build: &Build) -> Parser {
    Parser {
        prog: "fastbrowse",
        help: include_str!("../generated/cli.txt"),
        version: Some(build.version_line()),
        options: vec![
            Opt::version(),
            Opt::value("--start"),
            Opt::value("--context"),
            Opt::flag("--cloud"),
            Opt::flag("--local"),
            Opt::flag("--headed"),
            Opt::value("--profile"),
            Opt::value("--cloud-profile"),
            Opt::value("--cdp-url"),
            Opt::value("--cdp-port"),
            Opt::flag("--attach"),
            Opt::value("--target-match"),
            Opt::flag("--foreground"),
            Opt::value("--proxy-country"),
            Opt::flag("--authorize"),
            Opt::append("--secret"),
            Opt::value("--bitwarden"),
            Opt::value("--max-steps"),
            Opt::value("--max-dollars"),
            Opt::value("--downloads"),
            Opt::flag("--json"),
            Opt::value("--record"),
            Opt::flag("--fetch"),
            Opt::flag("--fetch-first"),
            Opt::value("--trace"),
            Opt::flag("--cursor"),
        ],
        positional: Some("task"),
        exclusive: vec![("--cloud", "--local")],
    }
}

/// The parsed command line, every value converted as argparse's `type=` converts it.
pub struct Args {
    pub task: String,
    pub start: Option<String>,
    pub context: Option<String>,
    pub cloud: bool,
    pub local: bool,
    pub headed: bool,
    pub profile: Option<PathBuf>,
    pub cloud_profile: Option<String>,
    pub cdp_url: Option<String>,
    pub cdp_port: Option<i64>,
    pub attach: bool,
    pub target_match: Option<String>,
    pub foreground: bool,
    pub proxy_country: Option<String>,
    pub authorize: bool,
    pub secret: Vec<(String, String, Option<String>)>,
    pub bitwarden: Option<String>,
    pub max_steps: Option<i64>,
    pub max_dollars: Option<f64>,
    pub downloads: Option<PathBuf>,
    pub json: bool,
    pub record: Option<PathBuf>,
    pub fetch: bool,
    pub fetch_first: bool,
    /// Where the run's trace is appended as JSON lines, or `-` for stderr.
    pub trace: Option<String>,
    pub cursor: bool,
}

fn int_value(value: &str) -> Result<(), String> {
    py_int(value).map(drop).ok_or_else(|| format!("invalid int value: {}", fastbrowse_core::py::text::repr(value)))
}

fn float_value(value: &str) -> Result<(), String> {
    py_float(value).map(drop).ok_or_else(|| format!("invalid float value: {}", fastbrowse_core::py::text::repr(value)))
}

pub fn parse(argv: &[String], build: &Build) -> Result<Args, Exit> {
    let parsed: Parsed = parser(build).parse(argv, |flag, value| match flag {
        "--cdp-port" | "--max-steps" => int_value(value),
        "--max-dollars" => float_value(value),
        "--proxy-country" => options::country_code(value).map(drop),
        "--secret" => options::scoped_secret(value).map(drop),
        _ => Ok(()),
    })?;
    let text = |flag: &str| parsed.value(flag).map(str::to_string);
    let path = |flag: &str| parsed.value(flag).map(PathBuf::from);
    Ok(Args {
        task: parsed.positional.clone().unwrap_or_default(),
        start: text("--start"),
        context: text("--context"),
        cloud: parsed.flag("--cloud"),
        local: parsed.flag("--local"),
        headed: parsed.flag("--headed"),
        profile: path("--profile"),
        cloud_profile: text("--cloud-profile"),
        cdp_url: text("--cdp-url"),
        cdp_port: parsed.value("--cdp-port").and_then(py_int),
        attach: parsed.flag("--attach"),
        target_match: text("--target-match"),
        foreground: parsed.flag("--foreground"),
        proxy_country: parsed.value("--proxy-country").and_then(|value| options::country_code(value).ok()),
        authorize: parsed.flag("--authorize"),
        secret: parsed.all("--secret").iter().filter_map(|value| options::scoped_secret(value).ok()).collect(),
        bitwarden: text("--bitwarden"),
        max_steps: parsed.value("--max-steps").and_then(py_int),
        max_dollars: parsed.value("--max-dollars").and_then(py_float),
        downloads: path("--downloads"),
        json: parsed.flag("--json"),
        record: path("--record"),
        fetch: parsed.flag("--fetch"),
        fetch_first: parsed.flag("--fetch-first"),
        trace: text("--trace"),
        cursor: parsed.flag("--cursor"),
    })
}

/// Values read now, so a missing one fails before a browser is opened.
///
/// The scope is never taken from wherever the run has got to: a credential that followed the browser would be typed
/// on any origin a redirect, an ad frame or a link on a compromised page led to. `NAME=ENV_VAR@https://host` states the
/// scope per secret and needs no `--start`; a secret that states none takes the start origin, and one with neither is
/// refused. `--bitwarden` still needs `--start`: the vault item is matched by origin.
pub fn secrets(
    pairs: &[(String, String, Option<String>)],
    bitwarden: Option<&str>,
    start: Option<&str>,
    environ: &HashMap<String, String>,
) -> Result<Option<ScopedSecrets>, ConfigurationError> {
    let mut problems = Vec::new();
    let missing = options::unset_variables(pairs, environ);
    if !missing.is_empty() {
        problems.push(format!("--secret names unset variables: {}", missing.join(", ")));
    }
    let unscoped: Vec<&str> =
        pairs.iter().filter(|(_, _, origin)| origin.is_none()).map(|(name, _, _)| name.as_str()).collect();
    if start.is_none() {
        if !unscoped.is_empty() {
            problems.push(format!(
                "--secret needs an origin for {}: give --start, or NAME=ENV_VAR@https://host",
                unscoped.join(", ")
            ));
        }
        if bitwarden.is_some() {
            problems.push("--bitwarden needs --start: the vault item is matched against its origin".into());
        }
    }
    if !problems.is_empty() {
        return Err(ConfigurationError(problems.join("; ")));
    }
    let fallback = start.map(origin_of);
    let mut scoped: IndexMap<String, (SecretValue, Vec<String>)> = IndexMap::new();
    for (name, variable, origin) in pairs {
        let Some(origin) = origin.clone().or_else(|| fallback.clone()) else {
            // Unreachable while the guard above stands, and here so that relaxing that guard cannot silently produce a
            // secret with no scope, which `per_secret` would then hold for nobody rather than refuse.
            return Err(ConfigurationError(format!("--secret {name} has no origin to be typed on")));
        };
        scoped.insert(name.clone(), (SecretValue::Plain(environ[variable].clone()), vec![origin]));
    }
    if let (Some(item), Some(start)) = (bitwarden, start) {
        let origin = origin_of(start);
        let vault = bitwarden::login(item, &origin).map_err(|error| ConfigurationError(error.0))?;
        let declared: IndexMap<String, SecretValue> =
            scoped.iter().map(|(name, (value, _))| (name.clone(), value.clone())).collect();
        options::merged_secrets(&declared, &vault).map_err(ConfigurationError)?;
        for (name, value) in vault {
            scoped.insert(name, (value, vec![origin.clone()]));
        }
    }
    Ok((!scoped.is_empty()).then(|| ScopedSecrets::per_secret(scoped)))
}

/// The run's limits, with a rejected one refused the way every other bad flag is, as a configuration error.
fn limits(args: &Args) -> Result<Limits, ConfigurationError> {
    let mut bad = Vec::new();
    if args.max_steps.is_some_and(|steps| steps <= 0) {
        bad.push("--max-steps");
    }
    if args.max_dollars.is_some_and(|dollars| dollars <= 0.0) {
        bad.push("--max-dollars");
    }
    if !bad.is_empty() {
        return Err(ConfigurationError(format!("{} must be greater than zero", bad.join(", "))));
    }
    Ok(Limits { max_steps: args.max_steps, max_dollars: args.max_dollars, ..Limits::default() })
}

fn progress(streams: Streams) -> fastbrowse_core::models::EventHandler {
    Arc::new(move |event: Event| {
        let streams = streams.clone();
        Box::pin(async move {
            match event {
                Event::Browser(browser) => {
                    if let Some(live_url) = browser.live_url.filter(|url| !url.is_empty()) {
                        streams.eprint(&format!("  watch live: {live_url}"));
                    }
                }
                Event::Step(event) => {
                    let step = &event.step;
                    let label = options::step_label(step);
                    streams.eprint(&format!("  {:>2} {label} -> {}", step.index, step.outcome.as_str()));
                }
            }
        })
    })
}

/// `cli.run(args)`: the exit code of a run that got as far as starting, or the configuration error that stopped it.
pub async fn run(args: &Args, context: &Context) -> Result<i32, ConfigurationError> {
    // Flags that contradict each other are named before anything about the machine, such as a missing ffmpeg.
    if args.fetch && args.fetch_first {
        return Err(ConfigurationError(
            "--fetch never starts a browser and --fetch-first may start one, so pass one of them".into(),
        ));
    }
    if args.fetch_first {
        // A recording or an attached window has to exist from the first step, and the fetched attempt has neither.
        for (flag, asked) in [
            ("--record", args.record.is_some()),
            ("--attach", args.attach),
            ("--target-match", args.target_match.is_some()),
        ] {
            if asked {
                return Err(ConfigurationError(format!(
                    "--fetch-first may finish with no browser, so it cannot be combined with {flag}"
                )));
            }
        }
    }
    options::recording(args.record.as_deref())?;
    let limits = limits(args)?;
    // The operator's own flags are checked before the browser key: with the cloud browser the default, a missing key
    // would otherwise hide a secret that could never be typed anywhere.
    let secrets = secrets(&args.secret, args.bitwarden.as_deref(), args.start.as_deref(), &context.environ)?;
    let settings = (context.settings)()?;
    if args.cloud && (args.headed || args.profile.is_some()) {
        return Err(ConfigurationError("--cloud cannot be combined with --headed or --profile".into()));
    }
    if args.fetch {
        // No browser is started, so the flags that shape one are refused rather than resolved: a cloud key demanded for
        // a browserless run would be a key never used.
        for (flag, asked) in [
            ("--cloud", args.cloud),
            ("--local", args.local),
            ("--headed", args.headed),
            ("--profile", args.profile.is_some()),
            ("--cloud-profile", args.cloud_profile.is_some()),
            ("--cdp-url", args.cdp_url.is_some()),
            ("--cdp-port", args.cdp_port.is_some()),
            ("--attach", args.attach),
            ("--target-match", args.target_match.is_some()),
            ("--foreground", args.foreground),
            ("--proxy-country", args.proxy_country.is_some()),
            ("--record", args.record.is_some()),
            ("--cursor", args.cursor),
            ("--downloads", args.downloads.is_some()),
        ] {
            if asked {
                return Err(ConfigurationError(format!(
                    "--fetch starts no browser, so it cannot be combined with {flag}"
                )));
            }
        }
    }
    let chrome = if args.cloud {
        LocalChrome::default()
    } else {
        options::chrome(&settings, args.headed, args.profile.as_deref(), None)
    };
    let handed_over = options::handed_over(&HandOver {
        cdp_url: args.cdp_url.as_deref(),
        cdp_port: args.cdp_port,
        attach: args.attach,
        target_match: args.target_match.as_deref(),
        local: args.local,
        cloud_asked: args.cloud,
        chrome: &chrome,
        cloud_profile: args.cloud_profile.as_deref(),
        proxy_country: args.proxy_country.as_deref(),
    })?;
    let browser_api_key = if args.fetch || handed_over {
        None
    } else {
        let on_cloud =
            options::cloud(args.local, &chrome, args.cloud_profile.as_deref(), args.proxy_country.as_deref())?;
        options::browser_key(&settings, on_cloud)?
    };
    let mut call = RunArgs::new(args.task.clone());
    call.start = args.start.clone();
    call.context = args.context.clone();
    call.browser_api_key = browser_api_key;
    call.chrome = (!args.fetch).then_some(chrome);
    call.cloud_profile = args.cloud_profile.clone();
    call.cdp_url = args.cdp_url.clone();
    call.cdp_port = args.cdp_port;
    call.attach = args.attach;
    call.target_match = args.target_match.clone();
    call.foreground = args.foreground;
    call.proxy_country = Some(options::proxy_country(args.proxy_country.as_deref()));
    call.secrets = secrets.map(|secrets| Arc::new(secrets) as _);
    call.limits = Some(limits);
    call.authorization = Some(Authorization { irreversible_actions: args.authorize });
    call.downloads = args.downloads.clone();
    call.on_event = Some(progress(context.streams.clone()));
    call.record = args.record.clone();
    call.cursor = args.cursor;
    call.fetch = args.fetch;
    call.fetch_first = args.fetch_first;
    let mut result = match context.runner.run(call).await {
        Ok(result) => result,
        Err(RunError::Configuration(message)) => return Err(ConfigurationError(message)),
        Err(RunError::Cancelled) => options::error_result("the run was cancelled"),
        // An exception out of `run_task` reaches the terminal as a traceback, exit 1, nothing on stdout.
        Err(RunError::Failed(message)) => {
            context.streams.eprint(&format!("Traceback (most recent call last):\n{message}"));
            return Ok(1);
        }
    };
    if result.status == Status::NeedsLogin {
        result.error = Some(format!(
            "{}. Pass --secret NAME=ENV_VAR@https://host with the password in that environment variable; credentials \
             in task text are not used.",
            result.error.as_deref().unwrap_or("Sign-in credentials required")
        ));
    }
    let streams = &context.streams;
    if args.json {
        streams.print(&json::write(&with_build(result.dump(), &context.build), Style::PYDANTIC.indented(2)));
    } else {
        // A local LLM endpoint reports no price, and printing only the priced calls would read as the whole bill.
        let mut spend = format!("${:.4}", result.cost.known_dollars());
        if result.cost.has_unknown() {
            spend.push_str(" plus calls with no reported cost");
        }
        streams.print(&format!("{} ({spend}, {} steps)", result.status.as_str(), result.steps.len()));
        // A run that stopped short can carry both: why it stopped, and what it had read by then.
        let parts: Vec<&str> = [result.error.as_deref(), result.answer.as_deref()]
            .into_iter()
            .flatten()
            .filter(|part| !part.is_empty())
            .collect();
        streams.print(&parts.join("\n\n"));
    }
    if let Some(frame) = &result.final_frame {
        match options::keep_screenshot(frame, args.downloads.as_deref()) {
            Ok(path) => streams.eprint(&format!("  screenshot: {}", path.display())),
            Err(error) => streams.eprint(&format!("  screenshot not kept: {error}")),
        }
    }
    if args.record.is_some() {
        if result.recordings.is_empty() {
            streams.eprint("  not recorded");
        } else {
            let paths: Vec<String> = result.recordings.iter().map(|path| path.display().to_string()).collect();
            streams.eprint(&format!("  recorded: {}", paths.join(", ")));
        }
    }
    Ok(exit_code(result.status))
}

/// `cli.main()` after the `serve` check: parse, run, and report a configuration error on both streams when `--json`
/// was asked for, so a caller parsing stdout still gets a result.
pub async fn main(argv: &[String], context: &Context) -> i32 {
    let args = match parse(argv, &context.build) {
        Ok(args) => args,
        Err(Exit::Print(text)) => {
            let mut out = context.streams.out.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let _ = out.write_all(text.as_bytes()).and_then(|()| out.flush());
            return 0;
        }
        Err(Exit::Usage(text)) => {
            let mut err = context.streams.err.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let _ = err.write_all(text.as_bytes()).and_then(|()| err.flush());
            return 2;
        }
    };
    let destination = args.trace.as_deref().or(context.environ.get(TRACE_ENV).map(String::as_str));
    if let Some(destination) = destination
        && let Err(error) = trace_to(Some(destination))
    {
        context.streams.eprint(&format!("fastbrowse: cannot write the trace to {destination}: {error}"));
        return 1;
    }
    Trace::disabled().event("build", || context.build.fields());
    match run(&args, context).await {
        Ok(code) => code,
        Err(error) => {
            if args.json {
                context.streams.print(&json::write(
                    &with_build(options::error_result(&error.0).dump(), &context.build),
                    Style::PYDANTIC.indented(2),
                ));
            }
            context.streams.eprint(&format!("fastbrowse: {}", error.0));
            1
        }
    }
}

/// The result `--json` prints, with the build that produced it, so a result file names its commit.
fn with_build(mut result: serde_json::Value, build: &Build) -> serde_json::Value {
    if let serde_json::Value::Object(fields) = &mut result {
        fields.insert("build".into(), build.fields().into());
    }
    result
}

/// A path as `Path.resolve()` would leave it, for messages that name one.
pub fn shown(path: &Path) -> String {
    path.display().to_string()
}
