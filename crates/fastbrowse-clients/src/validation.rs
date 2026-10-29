//! Port of `clients/validation.py`: shared wire encoding and strict validation for both Jev transports, and the retry
//! and accounting every model request goes through.
//!
//! Python cancels a call by raising `CancelledError` into it and lets each frame settle its receipts on the way out.
//! Here a call is cancelled by dropping its future, so what Python does in an `except CancelledError` is done by a
//! guard's `Drop`. Drop cannot wait, which is the one place this differs: Python lets an abandoned LLM request run to
//! its attempt deadline so its receipt can be read, where a dropped request here is charged as unknown at once.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use fastbrowse_core::BoxFuture;
use fastbrowse_core::config::TokenBudget;
use fastbrowse_core::decision::{
    Answer, ChoiceAnswer, DecisionError, Evaluation, NoulAnswer, Question, RetriesExhausted, ScoreAnswer,
};
use fastbrowse_core::models::{CostBasis, CostComponent, CostLine};
use fastbrowse_core::py::json::{dumps, float_str};
use fastbrowse_core::py::lax::float_of;
use fastbrowse_core::py::{self, text};
use fastbrowse_core::telemetry::{BudgetExceeded, Ledger, LedgerState, Trace, monotonic};
use indexmap::IndexMap;
use serde_json::{Map, Value};
use tokio::time::Instant;

use crate::http::{Http, Response, TransportError};

pub fn wire_questions(questions: &IndexMap<String, Question>, gateway: bool) -> Value {
    let mut result = Map::new();
    for (key, question) in questions {
        let mut body = Map::new();
        match question {
            Question::Choice(q) => {
                body.insert("instructions".into(), q.instructions.clone().into());
                body.insert("type".into(), "choice".into());
                body.insert("criteria".into(), Value::Object(q.criteria.clone().into_iter().collect()));
            }
            Question::Noul(q) => {
                body.insert("instructions".into(), q.instructions.clone().into());
                body.insert("type".into(), if gateway { "boolean" } else { "noul" }.into());
                let mut criteria = Map::new();
                if let Some(yes) = &q.r#true {
                    criteria.insert("true".into(), yes.clone().into());
                }
                if let Some(no) = &q.r#false {
                    criteria.insert("false".into(), no.clone().into());
                }
                if !criteria.is_empty() {
                    body.insert("criteria".into(), Value::Object(criteria));
                }
            }
            Question::Score(q) => {
                body.insert("instructions".into(), q.instructions.clone().into());
                body.insert("type".into(), "score".into());
                body.insert("criteria".into(), q.criteria.clone().into());
            }
        }
        result.insert(key.clone(), Value::Object(body));
    }
    Value::Object(result)
}

pub fn object_value(value: Option<&Value>) -> Result<&Map<String, Value>, String> {
    value.and_then(Value::as_object).ok_or_else(|| "expected an object".to_string())
}

pub fn number(value: Option<&Value>) -> Result<f64, String> {
    match value.and_then(Value::as_f64) {
        Some(n) if n.is_finite() => Ok(n),
        _ => Err("expected a finite number".into()),
    }
}

pub fn probability(value: Option<&Value>) -> Result<f64, String> {
    let result = number(value)?;
    if !(0.0..=1.0).contains(&result) {
        return Err("probability outside [0, 1]".into());
    }
    Ok(result)
}

pub fn token_count(value: Option<&Value>) -> Result<i64, String> {
    match value.and_then(Value::as_i64) {
        Some(n) if n >= 0 => Ok(n),
        _ => Err("expected a nonnegative integer token count".into()),
    }
}

pub fn dollars(value: Option<&Value>) -> Result<f64, String> {
    let result = match value {
        // `float(value)`: Python reads " 1e-3 " and "1_000" as numbers, and "inf" too, which the check below refuses.
        Some(Value::String(text)) => float_of(&Value::String(text.clone()))
            .map_err(|_| format!("could not convert string to float: {}", text::repr(text)))?,
        other => number(other)?,
    };
    if !result.is_finite() || result < 0.0 {
        return Err("expected a finite nonnegative cost".into());
    }
    Ok(result)
}

// pydantic's error text has one home, fastbrowse-core's validation, so a re-ask quotes it word for word.
pub use fastbrowse_core::validation::{errors_json, json_invalid, line_error};

/// `TypeAdapter(dict[str, JsonValue]).validate_json(response.content)`, failing with pydantic's error text.
pub fn json_object(response: &Response) -> Result<Map<String, Value>, String> {
    match serde_json::from_slice::<Value>(&response.body) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(errors_json(vec![line_error("dict_type", vec![], "Input should be an object", None)])),
        Err(error) => Err(errors_json(vec![json_invalid(&String::from_utf8_lossy(&response.body), &error)])),
    }
}

/// The start of a response body for an error message, minus the credential it was requested with.
///
/// Some providers echo the rejected key back in an authentication error, and error text is logged.
pub fn body_excerpt(response: &Response) -> String {
    text::prefix(&scrubbed(response, &response.text()), 400).to_string()
}

fn scrubbed(response: &Response, text: &str) -> String {
    if response.credential.is_empty() { text.to_string() } else { text.replace(&response.credential, "[api key]") }
}

fn field<'a>(value: Option<&'a Value>, keys: &[&str]) -> Option<&'a Value> {
    let mut value = value?;
    for key in keys {
        value = value.as_object()?.get(*key)?;
    }
    Some(value)
}

/// The provider reason inside the gateway's serialized API error.
fn wrapped_reason(body: Option<&Value>) -> Option<&str> {
    ["message", "error"].into_iter().find_map(|key| {
        field(body, &["error", "param", key]).and_then(Value::as_str).filter(|reason| !reason.is_empty())
    })
}

/// A failed response as one line someone can act on: the status, the provider's own error type and message, what the
/// provider said when a gateway only repeats its status, and the upstream a gateway routed to. The start of the body
/// when it is not the usual error JSON.
pub fn describe(response: &Response) -> String {
    let body = response.json_value();
    let Some(message) = field(body.as_ref(), &["error", "message"]).and_then(Value::as_str) else {
        let excerpt = body_excerpt(response);
        return format!("HTTP {}: {}", response.status, if excerpt.is_empty() { "(empty body)" } else { &excerpt });
    };
    let kind = field(body.as_ref(), &["error", "type"]).and_then(Value::as_str);
    let upstream =
        field(body.as_ref(), &["providerMetadata", "gateway", "routing", "resolvedProvider"]).and_then(Value::as_str);
    // Scrubbed before it is cut: a cut through an echoed key leaves a fragment no exact replacement matches.
    let mut text = format!("HTTP {}{}: ", response.status, kind.map(|kind| format!(" {kind}")).unwrap_or_default());
    text += text::prefix(&scrubbed(response, message), 300);
    if let Some(upstream) = upstream {
        text += &format!(" (via {upstream})");
    }
    if let Some(reason) = wrapped_reason(body.as_ref())
        && reason != message
    {
        text += &format!("; the provider said: {}", text::prefix(&scrubbed(response, reason), 200));
    }
    scrubbed(response, &text)
}

pub fn response_error(response: &Response, detail: &str) -> DecisionError {
    // `detail` can quote the body, as a validation error's input does.
    DecisionError::Failed(format!("{}; {}", text::prefix(&scrubbed(response, detail), 300), describe(response)))
}

/// About 22s in all. With 6s, a Jev 503 ended 5 of 311 eval runs, and each time the next run, started 0 to 15s later,
/// got through: the outages are brief, and a run lost to one costs far more than the wait. Only a longer outage moves
/// the run to the backup provider (`failover`).
pub const RETRY_DELAYS_SECONDS: [f64; 5] = [0.5, 1.5, 4.0, 8.0, 8.0];
/// Statuses that say nothing about the request, so a repeat can clear them. 520 to 524 are Cloudflare's edge failing to
/// reach the provider behind it: a 520 arrived seconds after a retried 503 from the same outage and, unlisted, failed
/// its eval row for good. A status still listed once the retries run out ends the run unavailable, not in error, so the
/// eval harness retries the row too.
pub const RETRYABLE_STATUS: [u16; 12] = [408, 429, 500, 502, 503, 504, 520, 521, 522, 523, 524, 529];

/// Accept retryable HTTP statuses and additional failures the gateway marks transient.
pub fn retryable(response: &Response) -> bool {
    if RETRYABLE_STATUS.contains(&response.status) {
        return true;
    }
    if response.is_success() {
        return false;
    }
    let body = response.json_value();
    let flagged = |keys: &[&str]| field(body.as_ref(), keys) == Some(&Value::Bool(true));
    flagged(&["error", "param", "isRetryable"]) || flagged(&["error", "isRetryable"])
}

const MAX_BACKOFF_SECONDS: f64 = 10.0;
/// Jev answers in about a second, so an attempt this old is stuck upstream, and a retry beats waiting on it.
pub const JEV_ATTEMPT_SECONDS: f64 = 15.0;
/// A Jev call slower than this is the provider's outage: 45 measured gateway calls from 10 to 600 controls (670 to 25k
/// input tokens) took 0.40 to 0.48s median and 0.93s at worst. Retries and backoff count toward it.
pub const JEV_SLOW_SECONDS: f64 = 2.0;
/// Leaves room for long reads and verification while bounding upstream stalls; flash-lite plans take about 0.8s.
pub const LLM_ATTEMPT_SECONDS: f64 = 30.0;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestUsage {
    pub requests: i64,
    pub consecutive_5xx: i64,
    /// Requests that may have been billed but whose usage was not returned to the caller.
    pub unaccounted_requests: i64,
    /// Why each request that came back unusable failed, in order: its status and reason, or its transport error.
    pub failures: Vec<String>,
}

impl RequestUsage {
    /// How the call went before it gave up, so an error says whether it was one blip or a sustained outage.
    pub fn history(&self, seconds: f64) -> String {
        attempts(self.requests, seconds)
    }
}

/// A call's usage, shared with the request in flight so a dropped request can still be counted.
#[derive(Debug, Clone, Default)]
pub struct Usage(Arc<Mutex<RequestUsage>>);

impl Usage {
    pub fn lock(&self) -> MutexGuard<'_, RequestUsage> {
        // A panic while the lock was held leaves counts that are still the best record there is.
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn snapshot(&self) -> RequestUsage {
        self.lock().clone()
    }
}

pub fn attempts(requests: i64, seconds: f64) -> String {
    // Every request sent: counting only this request's failures, a batch that split into singles
    // reported "1 failed requests" for a call that had sent five.
    format!("{requests} request{} in {seconds:.1}s", if requests == 1 { "" } else { "s" })
}

pub fn exhausted(requests: i64, seconds: f64, last: &str) -> String {
    format!("Jev request failed after {}; last: {last}", attempts(requests, seconds))
}

/// A call's cost including requests sent but not delivered: each carried the same input, so each is charged as the one
/// that answered, as an estimate. A bound holds for any request with that input, so it stays a bound.
pub fn with_discarded(cost: CostLine, usage: &RequestUsage) -> CostLine {
    if usage.unaccounted_requests == 0 {
        return cost;
    }
    let sent = 1 + usage.unaccounted_requests;
    let basis =
        if matches!(cost.basis, CostBasis::Unknown | CostBasis::Bounded) { cost.basis } else { CostBasis::Estimated };
    CostLine {
        basis: if cost.dollars.is_none() { CostBasis::Unknown } else { basis },
        dollars: cost.dollars.map(|dollars| dollars * sent as f64),
        input_tokens: cost.input_tokens * sent,
        ..cost
    }
}

/// Why a request loop stopped without a response to hand back.
#[derive(Debug)]
pub(crate) enum RetryStop {
    /// `before_retry` refused another request.
    Budget(BudgetExceeded),
    /// A transport failure no repeat can clear, named as httpx names it.
    Fatal(&'static str),
    /// A batch failed twice with a 5xx: its questions go one at a time, within the requests still available.
    Split { start_attempt: usize, usage: RequestUsage },
}

pub(crate) type BeforeRetry<'a> = Option<&'a (dyn Fn() -> Result<(), BudgetExceeded> + Sync)>;

/// How a request is repeated.
pub(crate) struct Attempts<'a> {
    pub call: String,
    pub attempt_seconds: f64,
    pub before_retry: BeforeRetry<'a>,
    pub start_attempt: usize,
    pub split_batch: bool,
}

/// Retry an overloaded or dropped request, which produced nothing and is always safe to repeat.
///
/// Each attempt sends one request and waits for it: a provider's slowest calls are stalls, and the attempt deadline
/// bounds them, so the time they cost is excluded from evals rather than hidden by racing a duplicate that doubles the
/// bill. `before_retry` runs ahead of every repeat, so a budget counts each request actually sent. `usage` counts
/// timed-out or dropped requests that may still have been billed.
/// Returns `None` when the transport never completed, leaving each client to name its own failure. Every retry is
/// logged as a warning naming `call`, the failure and the wait, and each failure is kept on `usage`.
pub(crate) async fn post_with_retry(
    http: &Http,
    url: &str,
    body: &Value,
    headers: &[(String, String)],
    attempts: &Attempts<'_>,
    usage: &Usage,
) -> Result<Option<Response>, RetryStop> {
    let retries = RETRY_DELAYS_SECONDS.len();
    let began = monotonic();
    let start_attempt = attempts.start_attempt;
    let request_limit = (start_attempt > 0).then(|| (retries + 1 - start_attempt) as i64);
    let mut response = None;
    for attempt in start_attempt..=retries {
        let delay = RETRY_DELAYS_SECONDS.get(attempt).copied();
        if attempt > 0
            && let Some(before_retry) = attempts.before_retry
        {
            before_retry().map_err(RetryStop::Budget)?;
        }
        let attempted = monotonic();
        response = send(http, url, body, headers, attempts.attempt_seconds, usage).await.map_err(RetryStop::Fatal)?;
        if let Some(answered) = &response
            && !retryable(answered)
        {
            if attempt > 0 {
                transient(&http.trace, &attempts.call, began, attempted);
            }
            return Ok(response);
        }
        let (requests, consecutive_5xx) = {
            let usage = usage.lock();
            (usage.requests, usage.consecutive_5xx)
        };
        if request_limit.is_some_and(|limit| requests >= limit) {
            break;
        }
        if attempts.split_batch && consecutive_5xx >= 2 && delay.is_some() {
            // Each attempt consumes a request, so a split inherits only the requests still available.
            let next_attempt = start_attempt + requests as usize;
            if next_attempt > retries {
                break;
            }
            transient(&http.trace, &attempts.call, began, monotonic());
            return Err(RetryStop::Split { start_attempt: next_attempt, usage: usage.snapshot() });
        }
        if let Some(delay) = delay {
            let wait = backoff(response.as_ref(), delay);
            let reason = usage.lock().failures.last().cloned().unwrap_or_else(|| "no usable response".into());
            log::warn!("{}: {reason}; retry {} of {retries} in {wait:.1}s", attempts.call, attempt + 1);
            http.trace.event("request_retry", || {
                let mut fields = Map::new();
                fields.insert("call".into(), attempts.call.clone().into());
                fields.insert("attempt".into(), (attempt + 1).into());
                fields.insert("reason".into(), reason.clone().into());
                fields.insert("wait".into(), py::round(wait, 2).into());
                fields
            });
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }
    }
    transient(&http.trace, &attempts.call, began, monotonic());
    Ok(response)
}

/// The stretch a provider's transient failures cost this call: its failed attempts and the waits between them, up to
/// the attempt that answered or to giving up. Evals leave it out of a run's time; it says nothing about the agent.
fn transient(trace: &Trace, call: &str, began: f64, until: f64) {
    trace.event("request_transient", || {
        let mut fields = Map::new();
        fields.insert("call".into(), call.into());
        fields.insert("began".into(), began.into());
        fields.insert("ended".into(), until.into());
        fields.insert("seconds".into(), py::round(until - began, 2).into());
        fields
    });
}

/// The server's own `Retry-After` when it sends one on a 429 or 529, as TypeSafe's SDK honours it; capped so an
/// overloaded provider cannot hold a run past its budget.
fn backoff(response: Option<&Response>, delay: f64) -> f64 {
    let Some(response) = response else { return delay };
    for (header, scale) in [("retry-after-ms", 0.001), ("retry-after", 1.0)] {
        let Some(raw) = response.header(header) else { continue };
        // Python stops at the first header it cannot read rather than trying the other.
        return match float_of(&Value::String(raw.to_string())) {
            // A NaN delay panics a tokio sleep, so a header reading NaN waits the scheduled delay instead.
            Ok(seconds) if seconds.is_nan() => delay,
            Ok(seconds) => (seconds * scale).clamp(0.0, MAX_BACKOFF_SECONDS),
            Err(_) => delay,
        };
    }
    delay
}

/// Counts a dispatched request that never finished, so a dropped one is charged rather than free.
struct InFlight<'a> {
    usage: &'a Usage,
    finished: bool,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.usage.lock().unaccounted_requests += 1;
        }
    }
}

async fn send(
    http: &Http,
    url: &str,
    body: &Value,
    headers: &[(String, String)],
    attempt_seconds: f64,
    usage: &Usage,
) -> Result<Option<Response>, &'static str> {
    usage.lock().requests += 1;
    let mut flight = InFlight { usage, finished: false };
    let sent = http.post_json(url, body, headers, attempt_seconds).await;
    flight.finished = true;
    let (response, failure) = match sent {
        Ok(response) if !retryable(&response) => return Ok(Some(response)),
        Ok(response) => {
            let failure = describe(&response);
            (Some(response), failure)
        }
        Err(TransportError::Timeout(name)) => (None, format!("no response within {attempt_seconds:.0}s ({name})")),
        // The type names the failure; its text can quote the request, credential included.
        Err(TransportError::Transient(name)) => (None, format!("no response ({name})")),
        Err(TransportError::Fatal(name)) => {
            usage.lock().unaccounted_requests += 1;
            return Err(name);
        }
    };
    let mut usage = usage.lock();
    usage.consecutive_5xx =
        if response.as_ref().is_some_and(|response| response.status >= 500) { usage.consecutive_5xx + 1 } else { 0 };
    usage.failures.push(failure);
    if response.is_none() {
        usage.unaccounted_requests += 1;
    }
    Ok(response)
}

/// Why one Jev request failed: an error to report, or a batch to split.
#[derive(Debug)]
pub(crate) enum AskError {
    Jev(DecisionError),
    Split { start_attempt: usize, usage: RequestUsage },
}

impl From<DecisionError> for AskError {
    fn from(error: DecisionError) -> Self {
        AskError::Jev(error)
    }
}

impl AskError {
    fn into_jev(self) -> DecisionError {
        match self {
            AskError::Jev(error) => error,
            // Only a batch asked with `split_batch` splits, and asking_split catches that one.
            AskError::Split { .. } => DecisionError::Failed("Jev batch split outside asking_split".into()),
        }
    }
}

/// Characters `json.dumps` writes for `body`, the measure Python prices a request by.
fn dumped_chars(body: &Value) -> usize {
    dumps(body).len()
}

/// Records the input estimate of a Jev request dropped in flight, as Python's `except CancelledError` does.
struct CancelledJev<'a> {
    spend: Option<JevSpend>,
    body: &'a Value,
    price: Option<f64>,
    usage: &'a Usage,
    trace: &'a Trace,
    armed: bool,
}

impl Drop for CancelledJev<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Cancellation loses reported usage, so retain the dispatched input estimate rather than a free call.
        let tokens = (dumped_chars(self.body) as f64 / TokenBudget::default().chars_per_token).ceil() as i64;
        let unaccounted = self.usage.lock().unaccounted_requests;
        if let Some(spend) = &self.spend {
            spend.record((0..unaccounted).map(|_| estimated_cost(self.price, tokens, 0)));
        }
        if unaccounted > 0 {
            self.trace.event("cancelled_request_cost", || {
                let mut fields = Map::new();
                fields.insert("call".into(), "jev".into());
                fields.insert("requests".into(), unaccounted.into());
                fields.insert("estimated_input_tokens".into(), tokens.into());
                fields.insert("dollars_per_input_token".into(), self.price.into());
                fields
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn post(
    http: &Http,
    url: &str,
    api_key: &str,
    body: &Value,
    headers: &[(&str, &str)],
    usage: &Usage,
    start_attempt: usize,
    split_batch: bool,
    price: Option<f64>,
) -> Result<Response, AskError> {
    let spend = JevSpend::current();
    // Priced before sending, so a split single or a retry cannot carry the run past its dollar cap.
    let estimate = body_dollars(body, price);
    let check = || check_jev_spend(spend.as_ref(), estimate);
    check().map_err(DecisionError::Budget)?;
    let started = Instant::now();
    let mut auth = vec![("Authorization".to_string(), format!("Bearer {api_key}"))];
    auth.extend(headers.iter().map(|(name, value)| (name.to_string(), value.to_string())));
    let attempts = Attempts {
        call: "jev".into(),
        attempt_seconds: JEV_ATTEMPT_SECONDS,
        before_retry: Some(&check),
        start_attempt,
        split_batch,
    };
    let mut cancelled = CancelledJev { spend: spend.clone(), body, price, usage, trace: &http.trace, armed: true };
    let sent = post_with_retry(http, url, body, &auth, &attempts, usage).await;
    cancelled.armed = false;
    let response = match sent {
        Ok(response) => response,
        Err(RetryStop::Budget(error)) => return Err(DecisionError::Budget(error).into()),
        Err(RetryStop::Fatal(name)) => {
            return Err(DecisionError::Failed(format!("Jev request could not be sent ({name})")).into());
        }
        Err(RetryStop::Split { start_attempt, usage }) => return Err(AskError::Split { start_attempt, usage }),
    };
    let seconds = started.elapsed().as_secs_f64();
    if seconds > JEV_SLOW_SECONDS {
        // Evals rerun an attempt that saw one, so a slow provider never counts against the agent.
        http.trace.event("request_slow", || {
            let mut fields = Map::new();
            fields.insert("call".into(), "jev".into());
            fields.insert("seconds".into(), py::round(seconds, 2).into());
            fields
        });
    }
    let usage = usage.snapshot();
    let Some(response) = response else {
        let last = usage.failures.last().cloned().unwrap_or_default();
        return Err(DecisionError::TransportFailed(format!(
            "Jev transport failed after {}; last: {last}",
            usage.history(seconds)
        ))
        .into());
    };
    if retryable(&response) {
        let last = describe(&response);
        return Err(DecisionError::RetriesExhausted(Box::new(RetriesExhausted {
            message: exhausted(usage.requests, seconds, &last),
            seconds,
            unaccounted_requests: usage.unaccounted_requests,
            requests: usage.requests,
            answered: Vec::new(),
            last,
        }))
        .into());
    }
    let text = response.text();
    if response.status == 400 && ["max_tokens_exceeded", "request body too large"].iter().any(|r| text.contains(r)) {
        return Err(DecisionError::InputTooLarge(format!("Jev input too large; {}", describe(&response))).into());
    }
    if !response.is_success() {
        return Err(response_error(&response, "Jev request failed").into());
    }
    Ok(response)
}

/// `math.fsum`: the correctly rounded sum, which a tolerance check at its edge depends on.
pub fn fsum(values: impl IntoIterator<Item = f64>) -> f64 {
    let mut partials: Vec<f64> = Vec::new();
    for mut x in values {
        let mut kept = 0;
        for i in 0..partials.len() {
            let mut y = partials[i];
            if x.abs() < y.abs() {
                std::mem::swap(&mut x, &mut y);
            }
            let high = x + y;
            let low = y - (high - x);
            if low != 0.0 {
                partials[kept] = low;
                kept += 1;
            }
            x = high;
        }
        partials.truncate(kept);
        partials.push(x);
    }
    // Round the exact sum of the partials once, halfway cases included, as CPython does.
    let mut n = partials.len();
    if n == 0 {
        return 0.0;
    }
    n -= 1;
    let mut high = partials[n];
    let mut low = 0.0;
    while n > 0 {
        let x = high;
        n -= 1;
        let y = partials[n];
        high = x + y;
        let yr = high - x;
        low = y - yr;
        if low != 0.0 {
            break;
        }
    }
    if n > 0 && ((low < 0.0 && partials[n - 1] < 0.0) || (low > 0.0 && partials[n - 1] > 0.0)) {
        let y = low * 2.0;
        let x = high + y;
        if y == x - high {
            high = x;
        }
    }
    high
}

fn probabilities(value: Option<&Value>, keys: &[String]) -> Result<IndexMap<String, f64>, String> {
    let raw = object_value(value)?;
    if keys.is_empty() || raw.len() != keys.len() || !raw.keys().all(|key| keys.contains(key)) {
        return Err("probability keys differ from criteria".into());
    }
    let mut result = IndexMap::new();
    for (key, value) in raw {
        result.insert(key.clone(), probability(Some(value))?);
    }
    // Each probability is rounded to two decimals on the wire, so the sum can drift by up to half a unit per option
    // (the AI SDK's own check); a fixed 0.02 rejected valid answers over many options.
    let tolerance = f64::max(0.02, 0.005 * result.len() as f64);
    if (fsum(result.values().copied()) - 1.0).abs() > tolerance + 1e-12 {
        return Err(format!("probabilities do not sum to one within {}", float_str(tolerance)));
    }
    Ok(result)
}

/// The wire's two-decimal probability step.
const ROUNDING_UNIT: f64 = 0.01;

pub fn parse_answers(
    value: Option<&Value>,
    questions: &IndexMap<String, Question>,
    gateway: bool,
    confidence: Option<&Map<String, Value>>,
) -> Result<IndexMap<String, Answer>, String> {
    let raw = object_value(value)?;
    if raw.len() != questions.len() || !raw.keys().all(|key| questions.contains_key(key)) {
        return Err("answer ids differ from question ids".into());
    }
    let mut answers = IndexMap::new();
    for (key, question) in questions {
        let answer = object_value(raw.get(key))?;
        let expected_type = match question {
            Question::Choice(_) => "choice",
            Question::Noul(_) if gateway => "boolean",
            Question::Noul(_) => "noul",
            Question::Score(_) => "score",
        };
        if answer.get("type").and_then(Value::as_str) != Some(expected_type) {
            return Err(format!("incorrect answer type for {key}"));
        }
        let confidence_value = if gateway { confidence.and_then(|c| c.get(key)) } else { answer.get("confidence") };
        let parsed = match question {
            Question::Choice(q) => {
                let criteria: Vec<String> = q.criteria.keys().cloned().collect();
                let probs = probabilities(answer.get("probabilities"), &criteria)?;
                let choice = match answer.get("choice").and_then(Value::as_str) {
                    Some(choice) if probs.contains_key(choice) => choice.to_string(),
                    _ => return Err(format!("choice outside criteria for {key}")),
                };
                // Probabilities are rounded to sum to one, which can move an option by one unit and put a near-tie a
                // hundredth the wrong way round: a live run failed on escalate 0.40 chosen over read 0.41.
                let top = probs.values().copied().fold(f64::NEG_INFINITY, f64::max);
                if probs[&choice] < top - ROUNDING_UNIT - 1e-6 {
                    return Err(format!("chosen option is not maximal for {key}"));
                }
                let confidence = probability(confidence_value)?;
                Answer::Choice(ChoiceAnswer { kind: Default::default(), choice, probabilities: probs, confidence })
            }
            // v1 of the direct API names P(yes) `noul` (https://docs.typesafe.ai/migrating-to-v1.md); the gateway maps
            // it to a boolean answer's `probability`.
            Question::Noul(_) => Answer::Noul(NoulAnswer {
                kind: Default::default(),
                probability: probability(answer.get(if gateway { "probability" } else { "noul" }))?,
            }),
            Question::Score(q) => {
                let levels: Vec<String> = (0..q.criteria.len()).map(|i| i.to_string()).collect();
                let probs = probabilities(answer.get("probabilities"), &levels)?;
                let score = number(answer.get("score"))?;
                if !(0.0..=(q.criteria.len() - 1) as f64).contains(&score) {
                    return Err(format!("score outside levels for {key}"));
                }
                let confidence = probability(confidence_value)?;
                Answer::Score(ScoreAnswer { kind: Default::default(), score, probabilities: probs, confidence })
            }
        };
        answers.insert(key.clone(), parsed);
    }
    Ok(answers)
}

tokio::task_local! {
    static SPEND: JevSpend;
}

/// Cost lines a caller collects. Python passes a list and appends to it in place; this is that list, shared.
#[derive(Debug, Clone, Default)]
pub struct Paid(Arc<Mutex<Vec<CostLine>>>);

impl Paid {
    fn lock(&self) -> MutexGuard<'_, Vec<CostLine>> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn lines(&self) -> Vec<CostLine> {
        self.lock().clone()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

/// Where a spend scope keeps the lines it is handed: a caller's own list, or the ledger's, as the run loop passes
/// `ledger.lines` itself.
#[derive(Debug, Clone)]
pub enum Lines {
    Ledger(Arc<Ledger>),
    Paid(Paid),
}

impl Lines {
    fn id(&self) -> usize {
        match self {
            Lines::Ledger(ledger) => Arc::as_ptr(ledger) as *const () as usize,
            Lines::Paid(paid) => Arc::as_ptr(&paid.0) as *const () as usize,
        }
    }

    fn extend(&self, costs: impl IntoIterator<Item = CostLine>) {
        match self {
            Lines::Ledger(ledger) => ledger.state().lines.extend(costs),
            Lines::Paid(paid) => paid.lock().extend(costs),
        }
    }

    fn snapshot(&self) -> Vec<CostLine> {
        match self {
            Lines::Ledger(ledger) => ledger.state().lines.clone(),
            Lines::Paid(paid) => paid.lines(),
        }
    }
}

#[derive(Debug)]
struct Frame {
    lines: Lines,
    ledger: Option<Arc<Ledger>>,
    pending: Arc<Mutex<Vec<Lines>>>,
}

/// The `jev_spend` scope a Jev call runs in: where its paid and cancelled requests are recorded, and the ledger a
/// request is priced against before it is sent.
///
/// Python keeps it in a context variable, which every task copies when it starts. A tokio task-local is not inherited
/// by a spawned task, so a caller that spawns Jev calls hands them [`JevSpend::current`] through [`JevSpend::scope`].
#[derive(Debug, Clone)]
pub struct JevSpend(Arc<Frame>);

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl JevSpend {
    pub fn current() -> Option<JevSpend> {
        SPEND.try_with(Clone::clone).ok()
    }

    pub async fn scope<F: Future>(self, future: F) -> F::Output {
        SPEND.scope(self, future).await
    }

    /// `record_jev_spend`.
    pub fn record(&self, costs: impl IntoIterator<Item = CostLine>) {
        self.0.lines.extend(costs);
    }
}

/// Removes a scope's list from the shared pending set when the scope ends, however it ends.
struct Registration {
    pending: Arc<Mutex<Vec<Lines>>>,
    id: usize,
    owned: bool,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if self.owned {
            locked(&self.pending).retain(|lines| lines.id() != self.id);
        }
    }
}

/// `jev_spend(lines, ledger=ledger)`: keep answered singles billable when another single fails or the caller cancels
/// the batch. A scope without a ledger inherits its parent's.
pub async fn jev_spend<F: Future>(lines: Lines, ledger: Option<Arc<Ledger>>, future: F) -> F::Output {
    let parent = JevSpend::current();
    let ledger = ledger.or_else(|| parent.as_ref().and_then(|parent| parent.0.ledger.clone()));
    let same_ledger = |other: &Option<Arc<Ledger>>| match (other, &ledger) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    };
    let pending = match &parent {
        Some(parent) if same_ledger(&parent.0.ledger) => parent.0.pending.clone(),
        _ => Arc::new(Mutex::new(Vec::new())),
    };
    let id = lines.id();
    let owned = !locked(&pending).iter().any(|held| held.id() == id);
    if owned {
        locked(&pending).push(lines.clone());
    }
    let _registration = Registration { pending: pending.clone(), id, owned };
    SPEND.scope(JevSpend(Arc::new(Frame { lines, ledger, pending })), future).await
}

/// `record_jev_spend`: add to the current scope's lines, if there is a scope.
pub fn record_jev_spend(costs: impl IntoIterator<Item = CostLine>) {
    if let Some(spend) = JevSpend::current() {
        spend.record(costs);
    }
}

/// A request's input estimate at `price`; an unpriced model reserves nothing, and its receipt says what it cost.
fn body_dollars(body: &Value, price: Option<f64>) -> f64 {
    dumped_chars(body) as f64 / TokenBudget::default().chars_per_token * price.unwrap_or(0.0)
}

/// Refuse a request whose estimate, beside every receipt held so far, would break the ledger's limits.
pub fn check_jev_spend(spend: Option<&JevSpend>, estimate_dollars: f64) -> Result<(), BudgetExceeded> {
    let Some(frame) = spend.map(|spend| &spend.0) else { return Ok(()) };
    let Some(ledger) = &frame.ledger else { return Ok(()) };
    let own = Lines::Ledger(ledger.clone()).id();
    let held: Vec<Lines> = locked(&frame.pending).clone();
    let state = ledger.state().clone();
    let mut lines = state.lines.clone();
    // Nested splits and failover hold paid answers until their result can include them once.
    for paid in held.iter().filter(|paid| paid.id() != own) {
        lines.extend(paid.snapshot());
    }
    let probe = Ledger::new(ledger.limits.clone());
    probe.set_started(ledger.started());
    *probe.state() = LedgerState { lines, ..state };
    probe.reserve(CostComponent::Jev, estimate_dollars, 0)
}

pub fn merged_cost(lines: &[CostLine]) -> CostLine {
    let basis = [CostBasis::Unknown, CostBasis::Estimated]
        .into_iter()
        .find(|basis| lines.iter().any(|line| line.basis == *basis))
        .unwrap_or(CostBasis::Metered);
    CostLine {
        component: CostComponent::Jev,
        basis,
        dollars: if lines.iter().any(|line| line.dollars.is_none()) {
            None
        } else {
            Some(py::sum(lines.iter().map(|line| line.dollars.unwrap_or(0.0))))
        },
        purpose: None,
        input_tokens: lines.iter().map(|line| line.input_tokens).sum(),
        output_tokens: lines.iter().map(|line| line.output_tokens).sum(),
        seconds: Some(py::sum(lines.iter().map(|line| line.seconds.unwrap_or(0.0)))),
    }
}

pub(crate) type Ask<'a> =
    dyn Fn(IndexMap<String, Question>, usize, bool) -> BoxFuture<'a, Result<Evaluation, AskError>> + Sync + 'a;

/// Records a split's paid singles into the enclosing scope when the split is dropped, as Python's `except
/// BaseException` does on cancellation.
struct PaidOnDrop {
    paid: Paid,
    parent: Option<JevSpend>,
    armed: bool,
}

impl Drop for PaidOnDrop {
    fn drop(&mut self) {
        if self.armed
            && let Some(parent) = &self.parent
        {
            parent.record(self.paid.lines());
        }
    }
}

/// Ask a batch, then its questions one at a time within the retries left once it failed twice with a 5xx.
pub(crate) async fn asking_split(
    questions: IndexMap<String, Question>,
    ask: &Ask<'_>,
    price: Option<f64>,
) -> Result<Evaluation, DecisionError> {
    if questions.len() < 2 {
        return ask(questions, 0, false).await.map_err(AskError::into_jev);
    }
    let started = Instant::now();
    let (mut start_attempt, usage) = match ask(questions.clone(), 0, true).await {
        Err(AskError::Split { start_attempt, usage }) => (start_attempt, usage),
        other => return other.map_err(AskError::into_jev),
    };
    let paid = Paid::default();
    let mut guard = PaidOnDrop { paid: paid.clone(), parent: JevSpend::current(), armed: true };
    let mut answered: Vec<Evaluation> = Vec::new();
    let singles = jev_spend(Lines::Paid(paid.clone()), None, async {
        for (key, question) in &questions {
            let result = ask(IndexMap::from([(key.clone(), question.clone())]), start_attempt, false)
                .await
                .map_err(AskError::into_jev)?;
            record_jev_spend([result.cost.clone()]);
            // Each request consumes a retry too; later questions retain only their first attempt for free.
            start_attempt =
                RETRY_DELAYS_SECONDS.len().min((start_attempt + result.requests as usize).saturating_sub(1));
            answered.push(result);
        }
        Ok::<(), DecisionError>(())
    })
    .await;
    guard.armed = false;
    if let Err(error) = singles {
        if let Some(parent) = &guard.parent {
            parent.record(paid.lines());
        }
        if let DecisionError::RetriesExhausted(exhausted_error) = &error {
            let requests =
                usage.requests + answered.iter().map(|result| result.requests).sum::<i64>() + exhausted_error.requests;
            let seconds = started.elapsed().as_secs_f64();
            return Err(DecisionError::RetriesExhausted(Box::new(RetriesExhausted {
                message: exhausted(requests, seconds, &exhausted_error.last),
                seconds,
                unaccounted_requests: exhausted_error.unaccounted_requests + usage.unaccounted_requests,
                requests,
                answered,
                last: exhausted_error.last.clone(),
            })));
        }
        return Err(error);
    }
    let costs: Vec<CostLine> = answered.iter().map(|result| result.cost.clone()).collect();
    let mut cost = merged_cost(&costs);
    let input_tokens: i64 = answered.iter().map(|result| result.input_tokens).sum();
    if usage.unaccounted_requests > 0 {
        // The failed batch carried all questions; estimate its unanswered requests from the answered singles.
        let discarded = estimated_cost(price, input_tokens * usage.unaccounted_requests, 0);
        cost = merged_cost(&[cost, discarded]);
    }
    Ok(Evaluation {
        model: answered[0].model.clone(),
        answers: answered.iter().flat_map(|result| result.answers.clone()).collect(),
        input_tokens,
        cost: CostLine { seconds: Some(started.elapsed().as_secs_f64()), ..cost },
        requests: usage.requests + answered.iter().map(|result| result.requests).sum::<i64>(),
    })
}

/// Answer each choice of one option here and send `ask` only the rest.
///
/// Jev began refusing a choice with a single option ("choice requires at least 2 options"), which the typesafe-ai
/// route reported as a 503: wiki-godel's shortcut opened the article with one field to fill, and the run retried that
/// refusal as an outage for hours. Such a choice was never in doubt, so it is not asked.
pub(crate) async fn asking_open<F>(
    questions: &IndexMap<String, Question>,
    ask: impl FnOnce(IndexMap<String, Question>) -> F,
    model: &str,
    price: Option<f64>,
) -> Result<Evaluation, DecisionError>
where
    F: Future<Output = Result<Evaluation, DecisionError>>,
{
    let forced: IndexMap<String, Answer> = questions
        .iter()
        .filter_map(|(key, question)| match question {
            Question::Choice(q) if q.criteria.len() == 1 => {
                let only = q.criteria.keys().next()?.clone();
                Some((
                    key.clone(),
                    Answer::Choice(ChoiceAnswer {
                        kind: Default::default(),
                        choice: only.clone(),
                        probabilities: IndexMap::from([(only, 1.0)]),
                        confidence: 1.0,
                    }),
                ))
            }
            _ => None,
        })
        .collect();
    if forced.is_empty() {
        return ask(questions.clone()).await;
    }
    let rest: IndexMap<String, Question> =
        questions.iter().filter(|(key, _)| !forced.contains_key(*key)).map(|(k, q)| (k.clone(), q.clone())).collect();
    if rest.is_empty() {
        return Ok(Evaluation {
            model: model.to_string(),
            answers: forced,
            input_tokens: 0,
            cost: estimated_cost(price, 0, 0),
            requests: 0,
        });
    }
    let mut evaluation = ask(rest).await?;
    evaluation.answers.extend(forced);
    Ok(evaluation)
}

pub fn reported_cost(
    value: Option<&Value>,
    input_tokens: i64,
    output_tokens: i64,
    price: Option<f64>,
) -> Result<CostLine, String> {
    let cost = match value {
        None | Some(Value::Null) => None,
        Some(value) => Some(dollars(Some(value))?),
    };
    // Providers have reported $0 while tokens flow, so missing or zero charges use list price.
    match cost {
        Some(cost) if cost != 0.0 || input_tokens == 0 => Ok(CostLine {
            component: CostComponent::Jev,
            basis: CostBasis::Metered,
            dollars: Some(cost),
            purpose: None,
            input_tokens,
            output_tokens,
            seconds: None,
        }),
        _ => Ok(estimated_cost(price, input_tokens, output_tokens)),
    }
}

/// `input_tokens` at the model's list `price`; without one the cost is unknown, never zero.
pub fn estimated_cost(price: Option<f64>, input_tokens: i64, output_tokens: i64) -> CostLine {
    CostLine {
        component: CostComponent::Jev,
        basis: if price.is_some() { CostBasis::Estimated } else { CostBasis::Unknown },
        dollars: price.map(|price| input_tokens as f64 * price),
        purpose: None,
        input_tokens,
        output_tokens,
        seconds: None,
    }
}

/// The parse of a Jev response: a value error's text, or pydantic's for a body that is not a JSON object.
pub(crate) fn invalid_response(response: &Response, what: &str, detail: &str) -> AskError {
    AskError::Jev(response_error(response, &format!("{what} ({detail})")))
}

/// A Jev response's `cost.seconds`: the call's wall time.
pub(crate) fn timed(cost: CostLine, usage: &RequestUsage, started: Instant) -> CostLine {
    CostLine { seconds: Some(started.elapsed().as_secs_f64()), ..with_discarded(cost, usage) }
}
