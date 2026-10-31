//! Spend and call accounting against `Limits`, checked before each call rather
//! than discovered after, and the trace a run keeps of why it went the way it did.
//!
//! A `Ledger`'s counters sit behind a mutex so the clients a run calls side by side can reserve against it from any
//! thread. A reservation is checked against the receipts recorded when it is made.

use std::future::Future;
use std::io::Write;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use serde_json::{Map, Value};

use crate::models::{BudgetResource, BudgetStop, CostBreakdown, CostComponent, CostLine, Limits, Status};
use crate::py::json::float_str;
use crate::safety::Redactor;

/// A limit as someone typed it: fixed-point, where `str` would print `1e-05` for a small one. This is
/// `format(Decimal(repr(amount)), "f")`.
pub fn dollars(amount: f64) -> String {
    let written = float_str(amount);
    let Some((mantissa, exponent)) = written.split_once('e') else { return written };
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let (whole, fraction) = mantissa.trim_start_matches('-').split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{whole}{fraction}");
    let point = whole.len() as i32 + exponent;
    let body = if point <= 0 {
        format!("0.{}{digits}", "0".repeat((-point) as usize))
    } else if point as usize >= digits.len() {
        format!("{digits}{}", "0".repeat(point as usize - digits.len()))
    } else {
        format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
    };
    format!("{}{body}", if negative { "-" } else { "" })
}

/// A limit was reached; the run ends `budget_exceeded`, naming which.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{reason}")]
pub struct BudgetExceeded {
    pub reason: String,
    pub budget: BudgetStop,
}

impl BudgetExceeded {
    fn new(reason: String, resource: BudgetResource, limit: f64) -> Self {
        BudgetExceeded { reason, budget: BudgetStop { resource, limit } }
    }

    pub fn status(&self) -> Status {
        Status::BudgetExceeded
    }
}

/// The counters a ledger keeps.
#[derive(Debug, Clone, Default)]
pub struct LedgerState {
    pub lines: Vec<CostLine>,
    pub steps: i64,
    pub jev_calls: i64,
    pub llm_calls: i64,
}

#[derive(Debug)]
pub struct Ledger {
    pub limits: Limits,
    /// When the run's clock started; None while it has not, so no call can run over a time limit before the run
    /// takes the ledger over.
    started: Mutex<Option<Instant>>,
    state: Mutex<LedgerState>,
}

impl Ledger {
    pub fn new(limits: Limits) -> Ledger {
        Ledger { limits, started: Mutex::new(Some(Instant::now())), state: Mutex::new(LedgerState::default()) }
    }

    pub fn started(&self) -> Option<Instant> {
        *self.started.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Start the clock at `at`, or stop it with None.
    pub fn set_started(&self, at: Option<Instant>) {
        *self.started.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = at;
    }

    /// The counters, for a caller that counts a step or reads a total.
    pub fn state(&self) -> MutexGuard<'_, LedgerState> {
        // A panic while the lock was held leaves counters that are still the best record there is.
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Refuse calls that would break a limit; `record` the actual lines afterwards.
    ///
    /// Calls sent together are reserved together, so a limit reached partway counts none of them.
    pub fn reserve(&self, component: CostComponent, estimate_dollars: f64, calls: i64) -> Result<(), BudgetExceeded> {
        let mut state = self.state();
        match component {
            CostComponent::Jev => {
                if let Some(max) = self.limits.max_jev_calls.filter(|max| state.jev_calls + calls > *max) {
                    return Err(BudgetExceeded::new(
                        format!("Jev call limit {max} reached"),
                        BudgetResource::JevCalls,
                        max as f64,
                    ));
                }
            }
            CostComponent::Llm => {
                if let Some(max) = self.limits.max_llm_calls.filter(|max| state.llm_calls + calls > *max) {
                    return Err(BudgetExceeded::new(
                        format!("LLM call limit {max} reached"),
                        BudgetResource::LlmCalls,
                        max as f64,
                    ));
                }
            }
            CostComponent::Browser | CostComponent::Proxy => {}
        }
        self.check_state(&state, estimate_dollars)?;
        if let Some(max) = self.limits.max_dollars
            && breakdown(&state).known_dollars() >= max
        {
            return Err(spend_reached(max));
        }
        // Failed requests still consume a call, including retries after an input-size rejection.
        match component {
            CostComponent::Jev => state.jev_calls += calls,
            CostComponent::Llm => state.llm_calls += calls,
            CostComponent::Browser | CostComponent::Proxy => {}
        }
        Ok(())
    }

    /// Whether a dollar or call cap is set. A call is checked against the receipts recorded when it reserves, so two
    /// calls sent side by side each pass before either records: under a cap, a call that waited on a sibling's
    /// receipt must keep waiting, or the sibling's spend goes uncounted.
    pub fn capped(&self) -> bool {
        let limits = &self.limits;
        limits.max_dollars.is_some() || limits.max_llm_calls.is_some() || limits.max_jev_calls.is_some()
    }

    pub fn check(&self, extra_dollars: f64) -> Result<(), BudgetExceeded> {
        self.check_state(&self.state(), extra_dollars)
    }

    fn check_state(&self, state: &LedgerState, extra_dollars: f64) -> Result<(), BudgetExceeded> {
        let limits = &self.limits;
        if let Some(max) = limits.max_seconds
            && self.started().is_some_and(|started| started.elapsed().as_secs_f64() > max)
        {
            return Err(BudgetExceeded::new(
                format!("time limit {}s reached", float_str(max)),
                BudgetResource::Seconds,
                max,
            ));
        }
        check_spend(limits, state, extra_dollars)?;
        if let Some(max) = limits.max_steps.filter(|max| state.steps >= *max) {
            return Err(BudgetExceeded::new(format!("step limit {max} reached"), BudgetResource::Steps, max as f64));
        }
        Ok(())
    }

    /// Check settled costs without treating a finished step allowance as another operation.
    pub fn check_spend(&self, extra_dollars: f64) -> Result<(), BudgetExceeded> {
        check_spend(&self.limits, &self.state(), extra_dollars)
    }

    /// Keep the lines, then check: a receipt that breaks a limit is still recorded, so the result reports it.
    pub fn record(&self, lines: impl IntoIterator<Item = CostLine>) -> Result<(), BudgetExceeded> {
        let mut state = self.state();
        state.lines.extend(lines);
        self.check_state(&state, 0.0)
    }

    pub fn breakdown(&self) -> CostBreakdown {
        breakdown(&self.state())
    }
}

fn breakdown(state: &LedgerState) -> CostBreakdown {
    CostBreakdown { lines: state.lines.clone() }
}

fn spend_reached(max: f64) -> BudgetExceeded {
    BudgetExceeded::new(format!("spend limit ${} reached", dollars(max)), BudgetResource::Dollars, max)
}

fn check_spend(limits: &Limits, state: &LedgerState, extra_dollars: f64) -> Result<(), BudgetExceeded> {
    if let Some(max) = limits.max_dollars {
        let spent = breakdown(state);
        // An unpriced call could have spent anything, so a dollar cap cannot be enforced past it.
        if spent.has_unknown() {
            return Err(BudgetExceeded::new(
                format!("spend limit ${} cannot be enforced: a call reported no cost", dollars(max)),
                BudgetResource::Dollars,
                max,
            ));
        }
        if spent.known_dollars() + extra_dollars > max {
            return Err(spend_reached(max));
        }
    }
    Ok(())
}

/// Seconds since the first trace in this process: the clock `began` and `ended` are read on, as `monotonic()`.
pub fn monotonic() -> f64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// `TRACE_ENV`: where a run writes its trace as JSON lines: `-` or `1` for stderr, otherwise a file path appended to.
pub const TRACE_ENV: &str = "FASTBROWSE_TRACE";

type Sink = Mutex<Box<dyn std::io::Write + Send>>;

/// The process's trace destination once [`trace_to`] set one.
static SINK: OnceLock<Sink> = OnceLock::new();

/// Write every trace event as one JSON line, to stderr for `-` or `1` and otherwise appended to the file
/// named.
///
/// The events say why a run went the way it did: each model call with its purpose, schema and timing, which reader
/// answered a read and why, every check's scores, recoveries and their reasons. They carry ids, verdicts, redacted
/// addresses, requirement texts and control labels, not page captures; treat the file as you would the task. With no
/// destination the `FASTBROWSE_TRACE` environment variable is used, and an unset or empty one changes nothing.
pub fn trace_to(destination: Option<&str>) -> std::io::Result<()> {
    let destination = match destination {
        Some(destination) => destination.to_string(),
        None => std::env::var(TRACE_ENV).unwrap_or_default(),
    };
    if destination.is_empty() || SINK.get().is_some() {
        return Ok(());
    }
    let stream: Box<dyn std::io::Write + Send> = if destination == "-" || destination == "1" {
        Box::new(std::io::stderr())
    } else {
        Box::new(std::fs::OpenOptions::new().create(true).append(true).open(&destination)?)
    };
    let _ = SINK.set(Mutex::new(stream));
    Ok(())
}

/// One event as a JSON line: when it was made, then the event.
fn write_line(sink: &Sink, record: &Map<String, Value>) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |at| at.as_secs_f64());
    let mut line = Map::new();
    line.insert("at".into(), crate::py::round(now, 3).into());
    line.extend(record.iter().map(|(key, value)| (key.clone(), value.clone())));
    let mut stream = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // A trace that cannot be written is the trace's loss, never the run's.
    let _ = writeln!(stream, "{}", crate::py::json::dumps(&Value::Object(line))).and_then(|()| stream.flush());
}

/// Why a run went the way it did, one record per judgement, for evals to keep beside each result: ids, verdicts,
/// scores and redacted addresses. The `http` events hold each model request and reply in full, page text included.
///
/// A run owns its `Trace` and hands it to what it calls, so runs that overlap keep separate traces. A disabled one
/// builds nothing unless [`trace_to`] set a destination for the process, which every trace writes to.
#[derive(Debug, Clone, Default)]
pub struct Trace {
    events: Option<Arc<Mutex<Vec<Value>>>>,
    /// The run's secrets. Every clone of a run's trace shares them, so an event made anywhere in the run, by the loop,
    /// a reader or a check, is redacted as it is written rather than wherever its fields were built.
    redactor: Option<Arc<Mutex<Redactor>>>,
}

impl Trace {
    /// A trace that keeps nothing.
    pub fn disabled() -> Trace {
        Trace { events: None, redactor: None }
    }

    /// `traced()`: a trace that keeps every record made through it or a clone of it.
    pub fn collecting() -> Trace {
        Trace { events: Some(Arc::new(Mutex::new(Vec::new()))), redactor: None }
    }

    /// This trace, redacting with `redactor` from now on.
    pub fn redacting(self, redactor: Arc<Mutex<Redactor>>) -> Trace {
        Trace { redactor: Some(redactor), ..self }
    }

    pub fn enabled(&self) -> bool {
        self.events.is_some() || SINK.get().is_some()
    }

    /// Record one event. The fields are built only when the trace keeps or writes them.
    pub fn event(&self, event: &str, fields: impl FnOnce() -> Map<String, Value>) {
        let sink = SINK.get();
        if self.events.is_none() && sink.is_none() {
            return;
        }
        let mut record = Map::new();
        record.insert("event".into(), event.into());
        record.extend(fields());
        if let Some(redactor) = &self.redactor {
            let redactor = redactor.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            record.values_mut().for_each(|value| redactor.redact_value(value));
        }
        if let Some(sink) = sink {
            write_line(sink, &record);
        }
        if let Some(events) = &self.events {
            events.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(Value::Object(record));
        }
    }

    /// Every record kept so far.
    pub fn events(&self) -> Vec<Value> {
        self.events
            .as_ref()
            .map_or_else(Vec::new, |events| events.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone())
    }

    /// Trace one model call's span on every exit, with how it ended.
    ///
    /// Calls overlap, so summed seconds per kind of call cannot say which of them a run was waiting on; each span can.
    /// A failed or cancelled call is billed too, and left out of the trace it made the time look free. A future
    /// dropped before it finished is the cancelled one.
    pub async fn call_span<T, E, F>(&self, call: &str, fields: Map<String, Value>, future: F) -> Result<T, E>
    where
        F: Future<Output = Result<T, E>>,
    {
        let mut span = Span { trace: self, call, fields: Some(fields), began: monotonic(), outcome: "cancelled" };
        let result = future.await;
        span.outcome = if result.is_ok() { "ok" } else { "error" };
        result
    }

    /// Trace how long the run waited on one phase of a step, on the clock `call` spans use, so a run's time can be
    /// split into what it waited on: a browser round trip is in no model call's span. A phase dropped before it
    /// finished is traced too, as a redraw drops one.
    pub async fn phase<F: Future>(&self, phase: &'static str, future: F) -> F::Output {
        let _phase = Phase { trace: self, phase, began: monotonic() };
        future.await
    }
}

struct Phase<'a> {
    trace: &'a Trace,
    phase: &'static str,
    began: f64,
}

impl Drop for Phase<'_> {
    fn drop(&mut self) {
        self.trace.event("phase", || {
            let mut record = Map::new();
            record.insert("phase".into(), self.phase.into());
            record.insert("began".into(), self.began.into());
            record.insert("ended".into(), monotonic().into());
            record
        });
    }
}

struct Span<'a> {
    trace: &'a Trace,
    call: &'a str,
    fields: Option<Map<String, Value>>,
    began: f64,
    outcome: &'static str,
}

impl Drop for Span<'_> {
    fn drop(&mut self) {
        let fields = self.fields.take().unwrap_or_default();
        self.trace.event("call", || {
            let mut record = Map::new();
            record.insert("call".into(), self.call.into());
            record.extend(fields);
            record.insert("began".into(), self.began.into());
            record.insert("ended".into(), monotonic().into());
            record.insert("outcome".into(), self.outcome.into());
            record
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::CostBasis;
    use crate::testing::block_on;
    use serde_json::json;

    #[test]
    fn test_the_spend_limit_is_printed_as_a_currency_amount() {
        for (limit, shown) in [(0.00001, "0.00001"), (0.25, "0.25"), (1.0, "1.0"), (0.5, "0.5"), (12.5, "12.5")] {
            let ledger = Ledger::new(Limits { max_dollars: Some(limit), ..Limits::default() });
            let error =
                ledger.record([CostLine::new(CostComponent::Llm, CostBasis::Metered, Some(limit + 1.0))]).unwrap_err();
            assert_eq!(error.to_string(), format!("spend limit ${shown} reached"));
        }
        assert_eq!(dollars(1e16), "10000000000000000");
        assert_eq!(dollars(1.5e-7), "0.00000015");
    }

    #[test]
    fn test_budget_stop_names_the_exhausted_limit() {
        let ledger = Ledger::new(Limits { max_steps: Some(1), ..Limits::default() });
        ledger.state().steps = 1;
        let error = ledger.check(0.0).unwrap_err();
        assert_eq!(error.budget.resource, BudgetResource::Steps);
        assert_eq!(error.budget.limit, 1.0);
        assert_eq!(error.status(), Status::BudgetExceeded);
    }

    #[test]
    fn calls_sent_together_are_reserved_together() {
        let ledger = Ledger::new(Limits { max_jev_calls: Some(3), ..Limits::default() });
        ledger.reserve(CostComponent::Jev, 0.0, 2).unwrap();
        let error = ledger.reserve(CostComponent::Jev, 0.0, 2).unwrap_err();
        assert_eq!(error.to_string(), "Jev call limit 3 reached");
        assert_eq!(ledger.state().jev_calls, 2);
        assert!(ledger.capped());
    }

    #[test]
    fn an_unpriced_call_makes_a_dollar_cap_unenforceable() {
        let ledger = Ledger::new(Limits { max_dollars: Some(1.0), ..Limits::default() });
        let error = ledger.record([CostLine::new(CostComponent::Llm, CostBasis::Unknown, None)]).unwrap_err();
        assert_eq!(error.to_string(), "spend limit $1.0 cannot be enforced: a call reported no cost");
        assert_eq!(ledger.breakdown().lines.len(), 1, "the receipt is kept");
    }

    #[test]
    fn a_span_records_how_the_call_ended() {
        let trace = Trace::collecting();
        let ok: Result<u8, ()> = block_on(trace.call_span("llm:read", Map::new(), async { Ok(1) }));
        assert_eq!(ok, Ok(1));
        let _: Result<u8, &str> = block_on(trace.call_span("jev", Map::new(), async { Err("no") }));
        drop(trace.call_span("jev", Map::new(), async { Ok::<u8, ()>(1) }));
        let outcomes: Vec<Value> = trace.events().iter().map(|e| e["outcome"].clone()).collect();
        assert_eq!(outcomes, [json!("ok"), json!("error")], "a span never polled was never begun");
        assert!(Trace::disabled().events().is_empty());
    }

    #[test]
    fn a_dropped_call_is_cancelled() {
        let trace = Trace::collecting();
        let mut future = Box::pin(trace.call_span("jev", Map::new(), std::future::pending::<Result<(), ()>>()));
        let waker = std::task::Waker::noop();
        assert!(future.as_mut().poll(&mut std::task::Context::from_waker(waker)).is_pending());
        drop(future);
        assert_eq!(trace.events()[0]["outcome"], "cancelled");
    }
}
