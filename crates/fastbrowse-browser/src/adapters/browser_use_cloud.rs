//! A remote Chrome from Browser Use Cloud, with its metered browser and proxy cost.
//!
//! ```text
//! let mut cloud = BrowserUseCloudBrowser::new(api_key, BrowserUseCloudOptions::default())?;
//! let connection = cloud.start().await?;
//! // ... a BrowserSession on `connection` ...
//! cloud.stop().await?;
//! cloud.cost(); // metered lines, available after stop
//! ```
//!
//! Creating and stopping the browser both have to survive cancellation. Rust cannot run async work in a dropped
//! future, so both requests run as spawned tasks: a `start` dropped mid-create leaves its task
//! behind for `stop` to wait on, and a browser still known when this value drops is stopped by a task spawned then.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fastbrowse_core::BoxFuture;
use fastbrowse_core::models::{BrowserConnection, CostBasis, CostComponent, CostLine};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::adapters::http::{self, Response};

pub const API_V4: &str = "https://api.browser-use.com/api/v4";
const API_ORIGIN: &str = "https://api.browser-use.com";
/// https://docs.browser-use.com/cloud/api-v4/browsers/create-browser-session: "up to 3 ready extensions".
pub const MAX_EXTENSIONS: usize = 3;
/// The screen a browser starts on when the caller names none. Left to the service a browser may come up 1920 wide, past
/// Jev's 1568 pixel side (`jev_takes_png`), and Jev would get no screenshot of any page.
const DEFAULT_SCREEN: (u32, u32) = (1120, 780);
/// A browser whose stop request failed bills until its own timeout, up to four hours, so a stop the service could
/// not take (a 5xx, a 429, a dropped connection) is sent again rather than abandoned after one try.
const STOP_ATTEMPTS: u32 = 4;
/// The first backoff between stop attempts, doubling after each, when the service names no delay of its own.
const STOP_RETRY_SECONDS: f64 = 1.0;
/// The most a teardown waits, in all, between stop attempts. A throttled service names its delay and the stop is
/// held for it: a project throttle asks for seconds, an edge throttle for five minutes, and a fixed backoff spent
/// every attempt inside that window and gave up. The browser bills through the wait either way, so ten minutes, two
/// edge delays, is worth spending to stop it; past that the run ends and the browser runs on to its timeout, logged.
const STOP_WAIT_SECONDS: f64 = 600.0;
// 520 to 524 are Cloudflare's edge failing to reach the provider behind it.
pub use crate::cdp::RETRYABLE_STATUS;

/// Monotonic seconds, as Python's `time.monotonic`.
pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;
pub type Sleep = Arc<dyn Fn(f64) -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Debug, Clone, PartialEq)]
pub enum BrowserUseCloudError {
    Error(String),
    /// `BrowserUseCloudUnavailable`: Browser Use Cloud could not be reached, or answered with a retryable status.
    /// `retry_after` is the seconds the service asked to be left alone for, when it said.
    Unavailable {
        message: String,
        retry_after: Option<f64>,
    },
}

impl BrowserUseCloudError {
    /// Whether Python's `isinstance(error, Unavailable)` holds: an outage, not a failed task.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }

    pub fn retry_after(&self) -> Option<f64> {
        match self {
            Self::Unavailable { retry_after, .. } => *retry_after,
            Self::Error(_) => None,
        }
    }

    fn unavailable(message: String) -> Self {
        Self::Unavailable { message, retry_after: None }
    }
}

impl fmt::Display for BrowserUseCloudError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Error(message) | Self::Unavailable { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for BrowserUseCloudError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserUseCloudOptions {
    pub proxy_country: Option<String>,
    pub timeout_minutes: u32,
    pub profile: Option<String>,
    pub viewport: Option<(u32, u32)>,
    pub extensions: Vec<String>,
    /// The longest one request may take.
    pub http_timeout: Duration,
}

impl Default for BrowserUseCloudOptions {
    fn default() -> Self {
        Self {
            proxy_country: Some("us".into()),
            timeout_minutes: 15,
            profile: None,
            viewport: None,
            extensions: vec![],
            http_timeout: Duration::from_secs(60),
        }
    }
}

struct Api {
    root: String,
    api_key: String,
    http_timeout: Duration,
    timeout_minutes: u32,
    clock: Clock,
    sleep: Sleep,
}

#[derive(Default)]
struct State {
    browser_id: Option<String>,
    cost: Vec<CostLine>,
}

type Shared = Arc<Mutex<State>>;

struct View {
    id: String,
    cdp_url: Option<String>,
    live_url: Option<String>,
    browser_cost: Option<f64>,
    proxy_cost: Option<f64>,
}

pub struct BrowserUseCloudBrowser {
    api: Arc<Api>,
    body: String,
    state: Shared,
    creating: Option<JoinHandle<Result<View, BrowserUseCloudError>>>,
    stopping: Option<JoinHandle<Result<(), BrowserUseCloudError>>>,
    connection: Option<BrowserConnection>,
}

impl BrowserUseCloudBrowser {
    /// Validate the options and build the create body; nothing is sent until `start`.
    pub fn new(api_key: &str, options: BrowserUseCloudOptions) -> Result<Self, BrowserUseCloudError> {
        let clock: Clock = Arc::new(fastbrowse_core::telemetry::monotonic);
        let sleep: Sleep = Arc::new(|seconds| Box::pin(tokio::time::sleep(Duration::from_secs_f64(seconds.max(0.0)))));
        Self::with_seams(api_key, options, API_ORIGIN, clock, sleep)
    }

    /// `new` against another origin, with the clock and sleep swapped, so a five-minute delay the service asks for
    /// is checked without being waited out.
    pub fn with_seams(
        api_key: &str,
        options: BrowserUseCloudOptions,
        origin: &str,
        clock: Clock,
        sleep: Sleep,
    ) -> Result<Self, BrowserUseCloudError> {
        let mut body: Vec<(&str, Value)> = vec![("timeout", json!(options.timeout_minutes))];
        if !options.extensions.is_empty() {
            body.push(("extensionIds", json!(extension_ids(&options.extensions)?)));
        }
        if let Some(country) = &options.proxy_country {
            body.push(("proxyCountryCode", json!(country)));
        }
        // The service's own CAPTCHA solver: its documented default, stated so a run never depends on that default.
        body.push(("solveCaptchas", json!(true)));
        // A cloud profile is the remote counterpart of `LocalChrome.profile`: the browser starts with the cookies
        // the profile already holds, so a site signed into once stays signed in. Whoever runs the task never sees
        // those cookies, which is the point of naming a profile rather than typing a secret.
        if let Some(profile) = &options.profile {
            body.push(("profileId", json!(profile)));
        }
        // Headless defaults are small enough that a responsive site collapses its header into a toggle and the
        // control the run needs is not in the page at all, which is why local Chrome sets a size too.
        let (width, height) = options.viewport.unwrap_or(DEFAULT_SCREEN);
        body.push(("browserScreenWidth", json!(width)));
        body.push(("browserScreenHeight", json!(height)));
        let api = Api {
            root: API_V4.replacen(API_ORIGIN, origin, 1),
            api_key: api_key.to_owned(),
            http_timeout: options.http_timeout,
            timeout_minutes: options.timeout_minutes,
            clock,
            sleep,
        };
        Ok(Self {
            api: Arc::new(api),
            body: ordered_object(&body),
            state: Shared::default(),
            creating: None,
            stopping: None,
            connection: None,
        })
    }

    pub fn connection(&self) -> Result<&BrowserConnection, BrowserUseCloudError> {
        self.connection.as_ref().ok_or_else(|| BrowserUseCloudError::Error("the cloud browser is not running".into()))
    }

    /// The browser and proxy lines the service metered, from its latest answer; empty before one. The proxy line is
    /// unsettled: the service meters a browser's proxy data about 30 seconds after it stops, and a run does not hold
    /// its result for that. A later `GET /browsers/{id}` reads the settled figure.
    pub fn cost(&self) -> Vec<CostLine> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).cost.clone()
    }

    /// Create the browser and find its DevTools address. A browser created by a start that then fails is stopped
    /// before the error returns.
    pub async fn start(&mut self) -> Result<BrowserConnection, BrowserUseCloudError> {
        let (api, state, body) = (self.api.clone(), self.state.clone(), self.body.clone());
        // A dropped POST can still create a billable browser, so the request runs on until its id is known.
        self.creating = Some(tokio::spawn(async move { create(&api, &state, &body).await }));
        match self.connect().await {
            Ok(connection) => {
                self.connection = Some(connection.clone());
                Ok(connection)
            }
            Err(error) => {
                let _ = self.stop().await;
                Err(error)
            }
        }
    }

    async fn connect(&mut self) -> Result<BrowserConnection, BrowserUseCloudError> {
        let Some(creating) = self.creating.as_mut() else {
            return Err(BrowserUseCloudError::Error("the cloud browser is not running".into()));
        };
        let created = creating.await;
        self.creating = None;
        let browser =
            created.map_err(|_| BrowserUseCloudError::Error("Browser Use Cloud POST /browsers failed".into()))??;
        let Some(cdp_url) = browser.cdp_url else {
            return Err(BrowserUseCloudError::Error(format!("browser {} started without a CDP URL", browser.id)));
        };
        let ws_url = if cdp_url.starts_with("ws://") || cdp_url.starts_with("wss://") {
            cdp_url
        } else {
            let url = format!("{cdp_url}/json/version");
            let version = match http::request("GET", &url, &[], None, self.api.http_timeout).await {
                Ok(version) => version,
                Err(error) => {
                    let message = format!("browser {} did not answer ({})", browser.id, error.name());
                    return Err(if error.is_transient() {
                        BrowserUseCloudError::unavailable(message)
                    } else {
                        BrowserUseCloudError::Error(message)
                    });
                }
            };
            let answered = format!("browser {} answered HTTP {}", browser.id, version.status);
            if RETRYABLE_STATUS.contains(&version.status) {
                return Err(BrowserUseCloudError::unavailable(answered));
            }
            if !version.is_success() {
                return Err(BrowserUseCloudError::Error(answered));
            }
            let found = version
                .json()
                .ok()
                .and_then(|v| v.get("webSocketDebuggerUrl").and_then(Value::as_str).map(String::from));
            found.ok_or_else(|| {
                BrowserUseCloudError::Error(format!("browser {} answered without a webSocketDebuggerUrl", browser.id))
            })?
        };
        Ok(BrowserConnection {
            live_url: browser.live_url,
            browser_id: Some(browser.id),
            foreground: true,
            ..BrowserConnection::new(ws_url, true)
        })
    }

    /// Stop the browser (it bills until stopped or timed out) and record what it cost. A creation still in flight
    /// is waited for first, so its browser is stopped too. A failed stop is logged and returned, and may be retried.
    pub async fn stop(&mut self) -> Result<(), BrowserUseCloudError> {
        self.connection = None;
        let stopping = match self.stopping.take() {
            Some(stopping) => stopping,
            None => tokio::spawn(finish(self.api.clone(), self.state.clone(), self.creating.take())),
        };
        // The remote browser keeps billing until this request finishes, so a dropped `stop` leaves it running on.
        let stopping = self.stopping.insert(stopping);
        let stopped = stopping.await;
        self.stopping = None;
        stopped.unwrap_or_else(|_| Err(BrowserUseCloudError::Error("Browser Use Cloud PATCH failed".into())))
    }
}

impl Drop for BrowserUseCloudBrowser {
    fn drop(&mut self) {
        let known = self.state.lock().map(|state| state.browser_id.is_some()).unwrap_or(false);
        if self.stopping.is_some() || (!known && self.creating.is_none()) {
            return;
        }
        // A browser left running bills until its timeout, up to four hours; stopping it late beats never.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(finish(self.api.clone(), self.state.clone(), self.creating.take()));
        }
    }
}

/// A JSON object with its keys in the order given, as Python's dict keeps them; serde_json's map sorts them.
fn ordered_object(fields: &[(&str, Value)]) -> String {
    let members: Vec<String> = fields.iter().map(|(key, value)| format!("{}:{value}", json!(key))).collect();
    format!("{{{}}}", members.join(","))
}

async fn create(api: &Api, state: &Shared, body: &str) -> Result<View, BrowserUseCloudError> {
    let created = call(api, "POST", "/browsers", body).await?;
    let raw: Value = serde_json::from_slice(&created.body)
        .map_err(|_| BrowserUseCloudError::Error("Browser Use Cloud POST /browsers did not answer JSON".into()))?;
    // Validation of optional fields must not lose the id needed for rollback.
    if let Some(id) = raw.get("id").and_then(Value::as_str) {
        lock(state).browser_id = Some(id.to_owned());
    }
    let browser = view(&raw).ok_or_else(|| {
        BrowserUseCloudError::Error("Browser Use Cloud POST /browsers answered with an unreadable browser".into())
    })?;
    record_cost(state, &browser);
    Ok(browser)
}

async fn finish(
    api: Arc<Api>,
    state: Shared,
    creating: Option<JoinHandle<Result<View, BrowserUseCloudError>>>,
) -> Result<(), BrowserUseCloudError> {
    if let Some(creating) = creating {
        let _ = creating.await;
    }
    let Some(browser_id) = lock(&state).browser_id.clone() else {
        return Ok(());
    };
    // Forget the browser only once the stop succeeded, so a failed stop can be retried rather than left billing.
    let response = match request_stop(&api, &browser_id).await {
        Ok(response) => response,
        Err(error) => {
            // A run that already failed, a failed start and a cancelled run all drop this error for the one they
            // carry, so a browser left running is said here or nowhere.
            crate::warn(&format!(
                "Browser Use Cloud browser {browser_id} was not stopped ({error}); it runs until its {}-minute timeout",
                api.timeout_minutes
            ));
            return Err(error);
        }
    };
    lock(&state).browser_id = None;
    let stopped = serde_json::from_slice(&response.body).ok().as_ref().and_then(view).ok_or_else(|| {
        BrowserUseCloudError::Error(format!(
            "Browser Use Cloud PATCH /browsers/{browser_id} answered with an unreadable browser"
        ))
    })?;
    record_cost(&state, &stopped);
    Ok(())
}

async fn request_stop(api: &Api, browser_id: &str) -> Result<Response, BrowserUseCloudError> {
    let path = format!("/browsers/{browser_id}");
    let body = r#"{"action":"stop"}"#;
    let give_up = (api.clock)() + STOP_WAIT_SECONDS;
    for attempt in 0..STOP_ATTEMPTS - 1 {
        match call(api, "PATCH", &path, body).await {
            Ok(response) => return Ok(response),
            Err(error @ BrowserUseCloudError::Unavailable { .. }) => {
                let remaining = give_up - (api.clock)();
                if remaining <= 0.0 {
                    return Err(error);
                }
                // The service's own delay outranks the backoff: retrying a throttle early is only throttled again.
                let delay = error.retry_after().unwrap_or(STOP_RETRY_SECONDS * 2f64.powi(attempt as i32));
                (api.sleep)(delay.min(remaining)).await;
            }
            Err(error) => return Err(error),
        }
    }
    call(api, "PATCH", &path, body).await
}

fn lock(state: &Shared) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

fn record_cost(state: &Shared, browser: &View) {
    // A cost the service left out is unknown, not a metered zero.
    let line = |component, basis, dollars: Option<f64>| {
        CostLine::new(component, if dollars.is_some() { basis } else { CostBasis::Unknown }, dollars)
    };
    lock(state).cost = vec![
        line(CostComponent::Browser, CostBasis::Metered, browser.browser_cost),
        line(CostComponent::Proxy, CostBasis::Unsettled, browser.proxy_cost),
    ];
}

async fn call(api: &Api, method: &str, path: &str, body: &str) -> Result<Response, BrowserUseCloudError> {
    let url = format!("{}{path}", api.root);
    let headers = [("X-Browser-Use-API-Key", api.api_key.as_str())];
    let response = match http::request(method, &url, &headers, Some(body), api.http_timeout).await {
        Ok(response) => response,
        // The request carries the API key header; the message names the call and nothing the request held.
        Err(error) if error.is_transient() => {
            return Err(BrowserUseCloudError::unavailable(format!("Browser Use Cloud {method} {path} failed")));
        }
        Err(_) => return Err(BrowserUseCloudError::Error(format!("Browser Use Cloud {method} {path} failed"))),
    };
    let status = response.status;
    if RETRYABLE_STATUS.contains(&status) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64();
        return Err(BrowserUseCloudError::Unavailable {
            message: format!("Browser Use Cloud {method} {path}: HTTP {status}"),
            retry_after: retry_after(&response, now),
        });
    }
    if !response.is_success() {
        return Err(BrowserUseCloudError::Error(format!("Browser Use Cloud {method} {path}: HTTP {status}")));
    }
    Ok(response)
}

/// `_BrowserView`: the id, addresses and costs; other fields are ignored. Keys are read by alias, then by field
/// name, as pydantic's `populate_by_name` does.
fn view(raw: &Value) -> Option<View> {
    let object = raw.as_object()?;
    let field = |alias: &str, name: &str| object.get(alias).or_else(|| object.get(name));
    let text = |alias: &str, name: &str| match field(alias, name) {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(text)) => Some(Some(text.clone())),
        Some(_) => None,
    };
    let dollars = |alias: &str, name: &str| match field(alias, name) {
        None => Some(None),
        Some(value) => decimal(value).map(Some),
    };
    Some(View {
        id: object.get("id")?.as_str()?.to_owned(),
        cdp_url: text("cdpUrl", "cdp_url")?,
        live_url: text("liveUrl", "live_url")?,
        browser_cost: dollars("browserCost", "browser_cost")?,
        proxy_cost: dollars("proxyCost", "proxy_cost")?,
    })
}

/// A `Decimal` field as pydantic takes it, a number or a decimal string, then as a float. Infinity and NaN are
/// refused, as pydantic refuses them for a Decimal.
fn decimal(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => {
            let text = text.trim();
            let digits = text.bytes().any(|b| b.is_ascii_digit());
            if !digits || !text.bytes().all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b)) {
                return None;
            }
            text.parse::<f64>().ok().filter(|dollars| dollars.is_finite())
        }
        _ => None,
    }
}

/// The delay a throttled response names: `Retry-After` in seconds or as a date, or the JSON body's
/// `retry_after_seconds`, the larger when both are given. A value that does not parse names nothing.
fn retry_after(response: &Response, now: f64) -> Option<f64> {
    let mut delays: Vec<f64> = Vec::new();
    let header = response.header("Retry-After").unwrap_or("").trim();
    if !header.is_empty() {
        if let Some(seconds) = py_float(header) {
            delays.push(seconds);
        } else if let Some(at) = http_date(header) {
            delays.push(at - now);
        }
    }
    if let Ok(Value::Object(body)) = response.json()
        && let Some(Value::Number(seconds)) = body.get("retry_after_seconds")
        && let Some(seconds) = seconds.as_f64()
    {
        delays.push(seconds);
    }
    delays.retain(|delay| delay.is_finite());
    if delays.is_empty() { None } else { Some(delays.into_iter().fold(0.0, f64::max)) }
}

/// Python's `float(text)`: a decimal literal with underscores between digits, or infinity or NaN by name.
fn py_float(text: &str) -> Option<f64> {
    let text = text.trim();
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text).to_ascii_lowercase();
    if ["inf", "infinity", "nan"].contains(&unsigned.as_str()) {
        return text.parse().ok();
    }
    let bytes = text.as_bytes();
    let underscores_between_digits = bytes.iter().enumerate().all(|(i, &b)| {
        b != b'_' || (i > 0 && i + 1 < bytes.len() && bytes[i - 1].is_ascii_digit() && bytes[i + 1].is_ascii_digit())
    });
    let plain: String = text.chars().filter(|&c| c != '_').collect();
    if !underscores_between_digits || !plain.bytes().all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b)) {
        return None;
    }
    plain.parse().ok()
}

/// `email.utils.parsedate_to_datetime` as unix seconds, for a date with a zone. A date without one, or with
/// `-0000`, is naive in Python and cannot be subtracted from an aware now, so it names nothing here either.
fn http_date(text: &str) -> Option<f64> {
    const MONTHS: [&str; 24] = [
        "jan",
        "feb",
        "mar",
        "apr",
        "may",
        "jun",
        "jul",
        "aug",
        "sep",
        "oct",
        "nov",
        "dec",
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    let mut data: Vec<String> = text.split_whitespace().map(String::from).collect();
    let first = data.first()?.clone();
    if first.ends_with(',') || DAYS.contains(&first.to_lowercase().as_str()) {
        data.remove(0);
    } else if let Some(comma) = first.rfind(',') {
        data[0] = first[comma + 1..].to_owned();
    }
    if data.len() == 3 {
        let stuff: Vec<String> = data[0].split('-').map(String::from).collect();
        if stuff.len() == 3 {
            data = stuff.into_iter().chain(data[1..].iter().cloned()).collect();
        }
    }
    if data.len() == 4 {
        let s = data[3].clone();
        match s.find('+').or_else(|| s.find('-')).filter(|&i| i > 0) {
            Some(i) => {
                data.truncate(3);
                data.extend([s[..i].to_owned(), s[i..].to_owned()]);
            }
            None => data.push(String::new()),
        }
    }
    if data.len() < 5 {
        return None;
    }
    let [mut dd, mut mm, mut yy, mut tm, mut tz] = <[String; 5]>::try_from(data[..5].to_vec()).ok()?;
    if dd.is_empty() || mm.is_empty() || yy.is_empty() {
        return None;
    }
    mm = mm.to_lowercase();
    if !MONTHS.contains(&mm.as_str()) {
        (dd, mm) = (mm, dd.to_lowercase());
        if !MONTHS.contains(&mm.as_str()) {
            return None;
        }
    }
    let month = MONTHS.iter().position(|name| *name == mm)? % 12 + 1;
    if let Some(stripped) = dd.strip_suffix(',') {
        dd = stripped.to_owned();
    }
    if yy.find(':').is_some_and(|i| i > 0) {
        std::mem::swap(&mut yy, &mut tm);
    }
    if let Some(stripped) = yy.strip_suffix(',') {
        yy = stripped.to_owned();
        if yy.is_empty() {
            return None;
        }
    }
    if !yy.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        std::mem::swap(&mut yy, &mut tz);
    }
    if let Some(stripped) = tm.strip_suffix(',') {
        tm = stripped.to_owned();
    }
    let mut clock: Vec<&str> = tm.split(':').collect();
    if clock.len() == 1 && tm.contains('.') {
        clock = tm.split('.').collect();
    }
    let (hh, mi, ss) = match clock[..] {
        [hh, mi] => (hh, mi, "0"),
        [hh, mi, ss] => (hh, mi, ss),
        _ => return None,
    };
    let number = |text: &str| text.trim().parse::<i64>().ok();
    let (mut year, day, hour, minute, second) = (number(&yy)?, number(&dd)?, number(hh)?, number(mi)?, number(ss)?);
    if year < 100 {
        year += if year > 68 { 1900 } else { 2000 };
    }
    let tz = tz.to_uppercase();
    let zones = [
        ("UT", 0),
        ("UTC", 0),
        ("GMT", 0),
        ("Z", 0),
        ("AST", -400),
        ("ADT", -300),
        ("EST", -500),
        ("EDT", -400),
        ("CST", -600),
        ("CDT", -500),
        ("MST", -700),
        ("MDT", -600),
        ("PST", -800),
        ("PDT", -700),
    ];
    let offset = match zones.iter().find(|(name, _)| *name == tz) {
        Some((_, offset)) => *offset,
        None => match tz.parse::<i64>() {
            Ok(0) if tz.starts_with('-') => return None,
            Ok(offset) => offset,
            Err(_) => return None,
        },
    };
    let offset = offset.signum() * ((offset.abs() / 100) * 3600 + (offset.abs() % 100) * 60);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let valid = (1..=9999).contains(&year)
        && (1..=month_days[month - 1]).contains(&day)
        && (0..24).contains(&hour)
        && (0..60).contains(&minute)
        && (0..60).contains(&second)
        && offset.abs() < 86_400;
    if !valid {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let (y, m) = (if month <= 2 { year - 1 } else { year }, month as i64);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hour * 3600 + minute * 60 + second - offset) as f64)
}

/// `str(uuid.UUID(text))` for each extension, as Python validates them: hyphens optional, braces and a `urn:uuid:`
/// prefix accepted, written back in lowercase canonical form.
fn extension_ids(extensions: &[String]) -> Result<Vec<String>, BrowserUseCloudError> {
    let ids =
        extensions.iter().map(|extension| uuid(extension)).collect::<Option<Vec<String>>>().ok_or_else(|| {
            BrowserUseCloudError::Error("cloud_extensions takes extension IDs, which are UUIDs".into())
        })?;
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    if unique.len() != ids.len() {
        return Err(BrowserUseCloudError::Error("cloud_extensions names the same extension twice".into()));
    }
    if ids.len() > MAX_EXTENSIONS {
        return Err(BrowserUseCloudError::Error(format!("a cloud browser takes at most {MAX_EXTENSIONS} extensions")));
    }
    Ok(ids)
}

/// `uuid.UUID(hex)`: strip `urn:` and `uuid:` wherever they appear, braces at either end and every hyphen, then
/// read the 32 characters left as `int(hex, 16)` does, which allows a sign, a `0x` prefix, surrounding whitespace
/// and underscores between digits.
fn uuid(text: &str) -> Option<String> {
    let hex = text.replace("urn:", "").replace("uuid:", "");
    let hex = hex.trim_matches(['{', '}']).replace('-', "");
    if hex.chars().count() != 32 {
        return None;
    }
    let value = hex.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c));
    let (negative, value) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    let value = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .map_or(value, |v| v.strip_prefix('_').unwrap_or(v));
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.first() == Some(&b'_') || bytes.last() == Some(&b'_') || value.contains("__") {
        return None;
    }
    let digits: String = value.chars().filter(|&c| c != '_').collect();
    let number = u128::from_str_radix(&digits, 16).ok().filter(|_| digits.bytes().all(|b| b.is_ascii_hexdigit()))?;
    // `0 <= int < 1 << 128`: a negative value is out of range unless it is zero.
    if negative && number != 0 {
        return None;
    }
    let hex = format!("{number:032x}");
    Some(format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;
    use crate::adapters::http::fake::{Canned, FakeServer, Recorded, json as reply};

    const EXTENSION_A: &str = "6f1c2d3e-4a5b-4c6d-8e7f-0a1b2c3d4e5f";
    const EXTENSION_B: &str = "11111111-2222-4333-8444-555555555555";

    /// Stands in for the adapter's sleep and clock: records each wait and moves the clock by it, at once.
    #[derive(Clone, Default)]
    struct Waits {
        now: Arc<StdMutex<f64>>,
        waits: Arc<StdMutex<Vec<f64>>>,
    }

    impl Waits {
        fn clock(&self) -> Clock {
            let now = self.now.clone();
            Arc::new(move || *now.lock().unwrap())
        }

        fn sleep(&self) -> Sleep {
            let (now, waits) = (self.now.clone(), self.waits.clone());
            Arc::new(move |seconds| {
                waits.lock().unwrap().push(seconds);
                *now.lock().unwrap() += seconds;
                Box::pin(async {})
            })
        }

        fn waits(&self) -> Vec<f64> {
            self.waits.lock().unwrap().clone()
        }
    }

    fn cloud(server: &FakeServer, options: BrowserUseCloudOptions, waits: &Waits) -> BrowserUseCloudBrowser {
        BrowserUseCloudBrowser::with_seams("key", options, &server.base, waits.clock(), waits.sleep()).unwrap()
    }

    /// A browser whose CDP address is the fake server itself, which answers `/json/version` too.
    fn created(base: &str, browser_cost: &str) -> Canned {
        reply(
            200,
            serde_json::json!({
                "id": "created", "cdpUrl": base, "webSocketDebuggerUrl": "ws://cdp.test", "browserCost": browser_cost,
                "proxyCost": "0.10",
            }),
        )
    }

    fn methods(server: &FakeServer) -> Vec<String> {
        server.requests().into_iter().map(|r| r.method).collect()
    }

    fn patches(requests: &[Recorded]) -> usize {
        requests.iter().filter(|r| r.method == "PATCH").count()
    }

    fn dollars(cloud: &BrowserUseCloudBrowser) -> Vec<Option<f64>> {
        cloud.cost().iter().map(|line| line.dollars).collect()
    }

    #[tokio::test]
    async fn a_run_creates_connects_and_stops_with_its_key_and_cost() {
        let server = FakeServer::start(|r, base| created(base, if r.method == "POST" { "0.25" } else { "0.50" })).await;
        let mut cloud = cloud(&server, BrowserUseCloudOptions::default(), &Waits::default());
        let connection = cloud.start().await.unwrap();
        assert_eq!(cloud.connection().unwrap(), &connection);
        assert_eq!(connection.cdp_url, "ws://cdp.test");
        assert_eq!(connection.browser_id.as_deref(), Some("created"));
        assert!(connection.remote && connection.foreground);
        cloud.stop().await.unwrap();
        assert!(cloud.connection().is_err());
        let requests = server.requests();
        let paths: Vec<_> = requests.iter().map(|r| format!("{} {}", r.method, r.path)).collect();
        assert_eq!(paths, ["POST /api/v4/browsers", "GET /json/version", "PATCH /api/v4/browsers/created"]);
        let keyed = |r: &Recorded| r.headers.iter().any(|(n, v)| n == "X-Browser-Use-API-Key" && v == "key");
        assert!(keyed(&requests[0]) && !keyed(&requests[1]) && keyed(&requests[2]));
        assert_eq!(requests[2].body, br#"{"action":"stop"}"#);
        assert_eq!(dollars(&cloud), [Some(0.50), Some(0.10)]);
        // The proxy reading at the stop is labelled as what it is, and is not an unknown a dollar cap would stop on.
        assert_eq!(
            cloud.cost().iter().map(|line| line.basis).collect::<Vec<_>>(),
            [CostBasis::Metered, CostBasis::Unsettled]
        );
    }

    #[tokio::test]
    async fn cloud_setup_failure_stops_created_browser() {
        for failure in ["validation", "unexpected", "no cdp"] {
            let server = FakeServer::start(move |r, base| match (r.method.as_str(), failure) {
                ("POST", "validation") => reply(200, serde_json::json!({"id": "created", "cdpUrl": 123})),
                ("POST", "no cdp") => reply(200, serde_json::json!({"id": "created"})),
                ("GET", _) => (200, vec![], b"not json".to_vec()),
                _ => reply(
                    200,
                    serde_json::json!({"id": "created", "cdpUrl": base, "browserCost": "0.25", "proxyCost": 0.1}),
                ),
            })
            .await;
            let mut cloud = cloud(&server, BrowserUseCloudOptions::default(), &Waits::default());
            let error = cloud.start().await.unwrap_err();
            assert!(!error.is_unavailable(), "{failure}: {error}");
            assert_eq!(methods(&server).last().map(String::as_str), Some("PATCH"), "{failure}");
            assert_eq!(dollars(&cloud), [Some(0.25), Some(0.1)], "{failure}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dropped_start_still_lets_stop_find_and_stop_its_browser() {
        let server = FakeServer::start(|r, base| {
            if r.method == "POST" {
                std::thread::sleep(Duration::from_millis(300));
            }
            created(base, "0.25")
        })
        .await;
        let mut cloud = cloud(&server, BrowserUseCloudOptions::default(), &Waits::default());
        assert!(tokio::time::timeout(Duration::from_millis(50), cloud.start()).await.is_err());
        cloud.stop().await.unwrap();
        assert_eq!(methods(&server), ["POST", "PATCH"]);
        assert_eq!(cloud.cost()[0].dollars, Some(0.25));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_browser_dropped_mid_create_is_stopped_anyway() {
        let server = FakeServer::start(|r, base| {
            if r.method == "POST" {
                std::thread::sleep(Duration::from_millis(200));
            }
            created(base, "0.25")
        })
        .await;
        let mut cloud = cloud(&server, BrowserUseCloudOptions::default(), &Waits::default());
        assert!(tokio::time::timeout(Duration::from_millis(50), cloud.start()).await.is_err());
        drop(cloud);
        for _ in 0..100 {
            if methods(&server).contains(&"PATCH".to_owned()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(methods(&server), ["POST", "PATCH"]);
    }

    #[tokio::test]
    async fn cloud_teardown_keeps_the_cost_it_last_read() {
        for stop_fails in [false, true] {
            let server = FakeServer::start(move |r, base| match r.method.as_str() {
                "PATCH" if stop_fails => (503, vec![], vec![]),
                "POST" => created(base, "0.25"),
                _ => created(base, "0.50"),
            })
            .await;
            let mut cloud = cloud(&server, BrowserUseCloudOptions::default(), &Waits::default());
            cloud.start().await.unwrap();
            let stopped = cloud.stop().await;
            assert_eq!(stopped.is_err(), stop_fails);
            assert_eq!(cloud.cost()[0].dollars, Some(if stop_fails { 0.25 } else { 0.50 }));
        }
    }

    /// One 503 on the stop left the browser billing until its timeout, four hours for a benchmark run.
    #[tokio::test]
    async fn a_stop_the_cloud_could_not_take_is_sent_again() {
        let patches_seen = Arc::new(StdMutex::new(0));
        let server = FakeServer::start(move |r, base| {
            if r.method == "PATCH" {
                let mut seen = patches_seen.lock().unwrap();
                *seen += 1;
                if *seen == 1 {
                    return (503, vec![], vec![]);
                }
                return created(base, "0.50");
            }
            created(base, "0.25")
        })
        .await;
        let waits = Waits::default();
        let mut cloud = cloud(&server, BrowserUseCloudOptions::default(), &waits);
        cloud.start().await.unwrap();
        cloud.stop().await.unwrap();
        assert_eq!(methods(&server), ["POST", "GET", "PATCH", "PATCH"]);
        assert_eq!(waits.waits(), [STOP_RETRY_SECONDS]);
        assert_eq!(cloud.cost()[0].dollars, Some(0.50));
    }

    async fn throttled_then_stopped(throttle: Canned, times: usize) -> FakeServer {
        let seen = Arc::new(StdMutex::new(0));
        FakeServer::start(move |r, base| {
            if r.method == "PATCH" {
                let mut seen = seen.lock().unwrap();
                *seen += 1;
                if *seen <= times {
                    return throttle.clone();
                }
            }
            reply(200, serde_json::json!({"id": "created", "cdpUrl": base, "webSocketDebuggerUrl": "ws://cdp.test"}))
        })
        .await
    }

    /// A fixed one, two, four second backoff spent every attempt inside a five-minute throttle and gave up.
    #[tokio::test]
    async fn a_throttled_stop_waits_as_long_as_the_cloud_asks() {
        let header = (429, vec![("Retry-After".into(), "300".into())], br#"{"error":"rate_limited"}"#.to_vec());
        let body =
            reply(429, serde_json::json!({"error": "rate_limited", "limit_rps": 25, "retry_after_seconds": 300}));
        for throttle in [header, body] {
            let server = throttled_then_stopped(throttle, 1).await;
            let waits = Waits::default();
            let mut cloud = cloud(&server, BrowserUseCloudOptions::default(), &waits);
            cloud.start().await.unwrap();
            cloud.stop().await.unwrap();
            assert_eq!(methods(&server), ["POST", "GET", "PATCH", "PATCH"]);
            assert_eq!(waits.waits(), [300.0]);
        }
    }

    /// A service that asks for more and more time cannot hold a run's teardown open past ten minutes.
    #[tokio::test]
    async fn a_throttled_stop_waits_no_longer_than_its_bound() {
        let throttle = (429, vec![("Retry-After".into(), "400".into())], vec![]);
        let server = throttled_then_stopped(throttle, STOP_ATTEMPTS as usize).await;
        let waits = Waits::default();
        let mut cloud = cloud(&server, BrowserUseCloudOptions::default(), &waits);
        cloud.start().await.unwrap();
        let error = cloud.stop().await.unwrap_err();
        assert!(error.is_unavailable());
        assert_eq!(waits.waits(), [400.0, 200.0]);
        assert_eq!(waits.waits().iter().sum::<f64>(), STOP_WAIT_SECONDS);
        assert_eq!(patches(&server.requests()), 3);
    }

    #[tokio::test]
    async fn a_browser_that_could_not_be_stopped_says_so_without_the_key() {
        for failed_start in [false, true] {
            let server = FakeServer::start(move |r, base| match r.method.as_str() {
                "PATCH" => (503, vec![], vec![]),
                "GET" if failed_start => (404, vec![], vec![]),
                _ => reply(200, serde_json::json!({"id": "created", "cdpUrl": base, "webSocketDebuggerUrl": "ws://x"})),
            })
            .await;
            let options = BrowserUseCloudOptions { timeout_minutes: 240, ..Default::default() };
            let mut cloud = cloud(&server, options, &Waits::default());
            if failed_start {
                let error = cloud.start().await.unwrap_err();
                assert!(error.to_string().starts_with("browser created answered HTTP 404"), "{error}");
                assert!(!error.is_unavailable());
            } else {
                cloud.start().await.unwrap();
                let error = cloud.stop().await.unwrap_err();
                assert_eq!(error.to_string(), "Browser Use Cloud PATCH /browsers/created: HTTP 503");
                assert!(!error.to_string().contains("key"));
            }
            assert_eq!(patches(&server.requests()), STOP_ATTEMPTS as usize);
        }
    }

    #[tokio::test]
    async fn a_retryable_create_is_unavailable_and_an_unreachable_service_too() {
        let server = FakeServer::start(|_, _| (529, vec![("Retry-After".into(), "2".into())], vec![])).await;
        let mut cloud = cloud(&server, BrowserUseCloudOptions::default(), &Waits::default());
        let error = cloud.start().await.unwrap_err();
        assert_eq!(
            error,
            BrowserUseCloudError::Unavailable {
                message: "Browser Use Cloud POST /browsers: HTTP 529".into(),
                retry_after: Some(2.0),
            }
        );
        assert_eq!(methods(&server), ["POST"]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let waits = Waits::default();
        let mut gone =
            BrowserUseCloudBrowser::with_seams("secret-key", Default::default(), &origin, waits.clock(), waits.sleep())
                .unwrap();
        let error = gone.start().await.unwrap_err();
        assert_eq!(error.to_string(), "Browser Use Cloud POST /browsers failed");
        assert!(error.is_unavailable());
    }

    #[tokio::test]
    async fn cloud_extensions_go_on_the_create_call() {
        let server = FakeServer::start(move |_, _| {
            reply(200, serde_json::json!({"id": "created", "cdpUrl": "wss://cdp.test/browser/created"}))
        })
        .await;
        let extensions = vec![EXTENSION_A.to_owned(), EXTENSION_B.to_owned()];
        let options = BrowserUseCloudOptions { extensions: extensions.clone(), ..Default::default() };
        let mut cloud = cloud(&server, options, &Waits::default());
        let connection = cloud.start().await.unwrap();
        cloud.stop().await.unwrap();
        assert_eq!(connection.cdp_url, "wss://cdp.test/browser/created");
        assert_eq!(server.requests()[0].json().get("extensionIds"), Some(&serde_json::json!(extensions)));
        assert_eq!(methods(&server), ["POST", "PATCH"]);
    }

    #[tokio::test]
    async fn the_create_body_keeps_pythons_key_order() {
        let cases = [
            (
                BrowserUseCloudOptions::default(),
                concat!(
                    r#"{"timeout":15,"proxyCountryCode":"us","solveCaptchas":true,"#,
                    r#""browserScreenWidth":1120,"browserScreenHeight":780}"#,
                ),
            ),
            (
                BrowserUseCloudOptions {
                    timeout_minutes: 240,
                    profile: Some("profile-42".into()),
                    ..Default::default()
                },
                concat!(
                    r#"{"timeout":240,"proxyCountryCode":"us","solveCaptchas":true,"profileId":"profile-42","#,
                    r#""browserScreenWidth":1120,"browserScreenHeight":780}"#,
                ),
            ),
            (
                BrowserUseCloudOptions {
                    proxy_country: None,
                    viewport: Some((1280, 900)),
                    extensions: vec![EXTENSION_A.to_uppercase()],
                    ..Default::default()
                },
                concat!(
                    r#"{"timeout":15,"extensionIds":["6f1c2d3e-4a5b-4c6d-8e7f-0a1b2c3d4e5f"],"#,
                    r#""solveCaptchas":true,"browserScreenWidth":1280,"browserScreenHeight":900}"#,
                ),
            ),
        ];
        for (options, expected) in cases {
            let server = FakeServer::start(|_, _| reply(200, serde_json::json!({"id": "x", "cdpUrl": "ws://x"}))).await;
            let mut cloud = cloud(&server, options, &Waits::default());
            cloud.start().await.unwrap();
            cloud.stop().await.unwrap();
            assert_eq!(String::from_utf8(server.requests()[0].body.clone()).unwrap(), expected);
        }
    }

    #[test]
    fn cloud_extensions_are_validated_before_any_request() {
        let upper = EXTENSION_A.to_uppercase();
        let cases: [(&[&str], &str); 5] = [
            (&["not-a-uuid"], "cloud_extensions takes extension IDs, which are UUIDs"),
            (&[""], "cloud_extensions takes extension IDs, which are UUIDs"),
            (&[EXTENSION_A, EXTENSION_A], "cloud_extensions names the same extension twice"),
            (&[EXTENSION_A, &upper], "cloud_extensions names the same extension twice"),
            (
                &[
                    EXTENSION_A,
                    EXTENSION_B,
                    "22222222-2222-4222-8222-222222222222",
                    "33333333-3333-4333-8333-333333333333",
                ],
                "a cloud browser takes at most 3 extensions",
            ),
        ];
        for (extensions, message) in cases {
            let options = BrowserUseCloudOptions {
                extensions: extensions.iter().map(|e| e.to_string()).collect(),
                ..Default::default()
            };
            let error = BrowserUseCloudBrowser::new("key", options).err().unwrap();
            assert_eq!(error, BrowserUseCloudError::Error(message.into()));
        }
    }

    #[test]
    fn uuids_parse_as_python_does() {
        let canonical = Some(EXTENSION_A.to_owned());
        for text in [
            "6F1C2D3E4A5B4C6D8E7F0A1B2C3D4E5F",
            "{6f1c2d3e-4a5b-4c6d-8e7f-0a1b2c3d4e5f}",
            "urn:uuid:6f1c2d3e-4a5b-4c6d-8e7f-0a1b2c3d4e5f",
            "6f1c-2d3e4a5b4c6d8e7f0a1b2c3d4e5f",
        ] {
            assert_eq!(uuid(text), canonical, "{text}");
        }
        assert_eq!(uuid("0x000000000000000000000000000001").as_deref(), Some("00000000-0000-0000-0000-000000000001"));
        assert_eq!(uuid("+0000000000000000000000000000001").as_deref(), Some("00000000-0000-0000-0000-000000000001"));
        assert_eq!(uuid("-0000000000000000000000000000001"), None);
        assert_eq!(uuid("6f1c2d3e4a5b4c6d8e7f0a1b2c3d4e5"), None);
        assert_eq!(uuid("6f1c2d3e4a5b4c6d8e7f0a1b2c3d4e5g"), None);
    }

    fn throttled(header: Option<&str>, body: &str) -> Response {
        let headers = header.map(|h| vec![("retry-after".to_owned(), h.to_owned())]).unwrap_or_default();
        Response { status: 429, headers, body: body.as_bytes().to_vec() }
    }

    #[test]
    fn retry_after_reads_seconds_dates_and_the_body_and_keeps_the_larger() {
        // 2026-10-23T12:00:00Z.
        let now = 1_792_756_800.0;
        let cases = [
            (Some("300"), "", Some(300.0)),
            (Some(" 1.5 "), "", Some(1.5)),
            (Some("1_0"), "", Some(10.0)),
            (Some("-5"), "", Some(0.0)),
            (Some("nan"), "", None),
            (Some("inf"), r#"{"retry_after_seconds": 7}"#, Some(7.0)),
            (Some("soon"), "", None),
            (Some("Fri, 23 Oct 2026 12:01:00 GMT"), "", Some(60.0)),
            (Some("Fri, 23 Oct 2026 08:02:00 -0400"), "", Some(120.0)),
            (Some("23 Oct 2026 12:00:30 EDT"), "", Some(14_430.0)),
            (Some("Fri, 23 Oct 2026 12:01:00 -0000"), "", None),
            (Some("Fri, 23 Oct 2026 12:01:00"), "", None),
            (Some("Fri, 23 Oct 2026 11:00:00 GMT"), "", Some(0.0)),
            (Some("10"), r#"{"retry_after_seconds": 300}"#, Some(300.0)),
            (Some("400"), r#"{"retry_after_seconds": 300}"#, Some(400.0)),
            (None, r#"{"retry_after_seconds": true}"#, None),
            (None, r#"{"retry_after_seconds": "30"}"#, None),
            (None, r#"[1]"#, None),
            (None, "", None),
        ];
        for (header, body, expected) in cases {
            assert_eq!(retry_after(&throttled(header, body), now), expected, "{header:?} {body}");
        }
    }

    #[test]
    fn costs_read_decimal_strings_and_numbers() {
        let read = |raw: Value| view(&raw).map(|v| (v.browser_cost, v.proxy_cost));
        assert_eq!(
            read(serde_json::json!({"id": "x", "browserCost": "0.25", "proxyCost": 0.1})),
            Some((Some(0.25), Some(0.1)))
        );
        assert_eq!(read(serde_json::json!({"id": "x"})), Some((None, None)));
        assert_eq!(read(serde_json::json!({"id": "x", "browser_cost": "1E-2"})), Some((Some(0.01), None)));
        assert_eq!(read(serde_json::json!({"id": "x", "browserCost": "NaN"})), None);
        assert_eq!(read(serde_json::json!({"id": "x", "browserCost": null})), None);
        assert_eq!(read(serde_json::json!({"id": 1})), None);
    }
}
