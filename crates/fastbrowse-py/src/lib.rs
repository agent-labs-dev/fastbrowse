//! `fastbrowse._native`: the Rust core in-process for Python.
//!
//! `origin_of`, `parse_origin` and `text_fragment` stand in for their namesakes in `fastbrowse.origins` and
//! `fastbrowse.citations`, and `run_task`, `connect_cdp` and `cli_main` for `fastbrowse.run_task`, `connect_cdp`
//! and the commands. This is the only core as of 0.9.0 (docs/adr/0004).
//!
//! Strings cross as USVStrings, the way WebIDL hands them to a URL parser.

mod bridge;
mod connect;
mod host;
mod options;

use std::sync::Arc;

use fastbrowse_cli::embed::{HostCall, HostResolve};
use fastbrowse_cli::runner::Cancel;
use fastbrowse_clients::http::{Http, Request, Response};
use fastbrowse_core::models::Event;
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::telemetry::Trace;
use fastbrowse_core::{citations, origins};
use fastbrowse_run::RunError;
use pyo3::exceptions::{PyException, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyCFunction, PyString, PyTuple};

use crate::bridge::{Context, Failure, HostJev, HostLlm, HostSecrets, HostTransport, LlmCall, Sent};
use crate::host::{host_call, runtime};
use crate::options::Options;

pyo3::create_exception!(
    fastbrowse._native,
    RunRefused,
    PyException,
    "A run that ended without a result: `args` is why (`invalid`, `configuration`, `recording`, `cancelled` or `callback`) and \
     the message."
);

/// The string as a USVString: each lone surrogate becomes one U+FFFD (PyO3's lossy conversion makes three).
fn usv(s: &Bound<'_, PyString>) -> PyResult<String> {
    if let Ok(utf8) = s.to_str() {
        return Ok(utf8.to_owned());
    }
    let bytes: Vec<u8> = s.call_method1("encode", ("utf-16-le", "surrogatepass"))?.extract()?;
    let units: Vec<u16> = bytes.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
    Ok(String::from_utf16_lossy(&units))
}

#[pyfunction]
fn origin_of(url: &Bound<'_, PyString>) -> PyResult<Option<String>> {
    Ok(origins::origin_of(&usv(url)?))
}

#[pyfunction]
fn parse_origin(value: &Bound<'_, PyString>) -> PyResult<String> {
    origins::parse_origin(&usv(value)?).map_err(|e| PyValueError::new_err(e.to_string()))
}

#[pyfunction]
fn text_fragment(url: &Bound<'_, PyString>, text: &Bound<'_, PyString>) -> PyResult<String> {
    citations::text_fragment(&usv(url)?, &usv(text)?).map_err(|e| PyValueError::new_err(e.to_string()))
}

fn json_argument(py: Python<'_>, value: serde_json::Value) -> PyResult<Bound<'_, PyTuple>> {
    PyTuple::new(py, [value.to_string()])
}

fn bytes_argument(py: Python<'_>, frame: Vec<u8>) -> PyResult<Bound<'_, PyTuple>> {
    PyTuple::new(py, [PyBytes::new(py, &frame)])
}

fn str_argument(py: Python<'_>, url: String) -> PyResult<Bound<'_, PyTuple>> {
    PyTuple::new(py, [url])
}

fn nothing(_: &Bound<'_, PyAny>) -> PyResult<()> {
    Ok(())
}

/// `TypeAdapter(bool).validate_python(answer, strict=True)`: anything but a bool is refused, as `serve` refuses it.
fn strict_bool(answer: &Bound<'_, PyAny>) -> PyResult<bool> {
    answer
        .cast::<pyo3::types::PyBool>()
        .map(|value| value.is_true())
        .map_err(|_| PyValueError::new_err("the result cannot be used: Input should be a valid boolean"))
}

fn resolver(event_loop: Arc<Py<PyAny>>, function: Py<PyAny>) -> HostResolve {
    let call = host_call(
        event_loop,
        function,
        |py, (name, origin): (String, String)| PyTuple::new(py, [name, origin]),
        |answer| answer.extract::<Option<String>>(),
    );
    Arc::new(move |name: String, origin: String| call((name, origin)))
}

/// A run under way. Awaiting it gives the `RunResult` as JSON; cancelling the task that awaits it stops the run.
#[pyclass(frozen)]
struct Run {
    result: Py<PyAny>,
    finished: Py<PyAny>,
    cancel: Cancel,
    trace: Trace,
}

#[pymethods]
impl Run {
    fn __await__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.result.bind(py).call_method0("__await__")
    }

    /// Stop the run. It closes its browser, then `finished` resolves.
    fn cancel(&self) {
        self.cancel.trip();
    }

    /// The trace events the run made so far, as a JSON list; empty unless it was started with `trace=True`. Their
    /// `began` and `ended` are on this module's `monotonic()` clock.
    fn trace_events(&self) -> String {
        serde_json::Value::Array(self.trace.events()).to_string()
    }

    /// An asyncio future that resolves with None once the run has returned and its browser is closed.
    #[getter]
    fn finished(&self, py: Python<'_>) -> Py<PyAny> {
        self.finished.clone_ref(py)
    }
}

fn pair_argument(py: Python<'_>, (first, second): (String, String)) -> PyResult<Bound<'_, PyTuple>> {
    PyTuple::new(py, [first, second])
}

fn no_argument(py: Python<'_>, (): ()) -> PyResult<Bound<'_, PyTuple>> {
    Ok(PyTuple::empty(py))
}

fn llm_argument(py: Python<'_>, call: LlmCall) -> PyResult<Bound<'_, PyTuple>> {
    let context = Bound::new(py, call.context)?;
    (call.purpose, call.messages, call.name, call.schema, call.max_output_tokens, context).into_pyobject(py)
}

fn request_argument(py: Python<'_>, request: Request) -> PyResult<Bound<'_, PyTuple>> {
    let body = PyBytes::new(py, &request.body);
    (request.method.as_str(), request.url, request.headers, body, request.max_bytes).into_pyobject(py)
}

fn text(answer: &Bound<'_, PyAny>) -> PyResult<String> {
    answer.extract()
}

fn kind(answer: &Bound<'_, PyAny>) -> PyResult<Option<String>> {
    answer.extract()
}

/// `("ok", status, headers, body)`, or `(kind, exception type name)` for what the client raised.
fn sent(answer: &Bound<'_, PyAny>) -> PyResult<Sent> {
    let tag: String = answer.get_item(0)?.extract()?;
    if tag != "ok" {
        return Ok(Sent::Failed(tag, answer.get_item(1)?.extract()?));
    }
    let (_, status, headers, body): (String, u16, Vec<(String, String)>, Vec<u8>) = answer.extract()?;
    let mut response = Response::new(status, body);
    response.headers = headers;
    Ok(Sent::Response(response))
}

fn refused(kind: &str, message: String) -> PyErr {
    RunRefused::new_err((kind.to_string(), message))
}

/// Start a run on the running event loop. `options` is `run_task`'s data arguments as JSON (`fastbrowse.native`
/// writes them), and each callback is a Python function: `on_event` gets each event as JSON, `on_frame` each JPEG as
/// bytes, `until` the final URL, `resolve_secret` a name and an origin. `check_access`, `jev`, `llm` and `http` are the
/// caller's own objects wrapped by `fastbrowse.native` into functions that answer JSON (see `bridge`).
///
/// Each is called with the GIL held, and a coroutine one returns is run on the loop and awaited. An event, frame,
/// until or secret callback that raises stops the run, which then refuses with `callback`.
#[pyfunction]
#[pyo3(signature = (
    task, options, *, on_event=None, on_frame=None, until=None, resolve_secret=None, check_access=None, jev=None,
    llm=None, http=None, trace=false,
))]
#[allow(clippy::too_many_arguments)]
fn run_task(
    py: Python<'_>,
    task: String,
    options: &str,
    on_event: Option<Py<PyAny>>,
    on_frame: Option<Py<PyAny>>,
    until: Option<Py<PyAny>>,
    resolve_secret: Option<Py<PyAny>>,
    check_access: Option<Py<PyAny>>,
    jev: Option<Py<PyAny>>,
    llm: Option<Py<PyAny>>,
    http: Option<Py<PyAny>>,
    trace: bool,
) -> PyResult<Run> {
    let mut options: Options =
        serde_json::from_str(options).map_err(|error| PyValueError::new_err(error.to_string()))?;
    let refs = options.secrets.take();
    let mut run = options.task().map_err(PyValueError::new_err)?;
    let event_loop = py.import("asyncio")?.call_method0("get_running_loop")?;
    let result = event_loop.call_method0("create_future")?;
    let finished = event_loop.call_method0("create_future")?;
    let cancel = Cancel::default();
    let stop = cancel.clone();
    // An awaiting task that is cancelled cancels the future it waits on, and that stops the run.
    let on_cancelled = PyCFunction::new_closure(py, None, None, move |args: &Bound<'_, PyTuple>, _| -> PyResult<()> {
        if args.get_item(0)?.call_method0("cancelled")?.is_truthy()? {
            stop.trip();
        }
        Ok(())
    })?;
    result.call_method1("add_done_callback", (on_cancelled,))?;
    let event_loop = Arc::new(event_loop.unbind());
    let failure = Arc::new(Failure::new(cancel.clone()));
    if let Some(function) = on_event {
        let (call, failure) = (host_call(event_loop.clone(), function, json_argument, nothing), failure.clone());
        run.on_event = Some(Arc::new(move |event: Event| {
            let (call, failure) = (call.clone(), failure.clone());
            Box::pin(async move {
                if call(event.dump()).await.is_err() {
                    failure.fail();
                }
            })
        }));
    }
    if let Some(function) = on_frame {
        let (call, failure) = (host_call(event_loop.clone(), function, bytes_argument, nothing), failure.clone());
        run.on_frame = Some(Arc::new(move |frame: Vec<u8>| {
            let (call, failure) = (call.clone(), failure.clone());
            Box::pin(async move {
                if call(frame).await.is_err() {
                    failure.fail();
                }
            })
        }));
    }
    if let Some(function) = until {
        let (call, failure) = (host_call(event_loop.clone(), function, str_argument, strict_bool), failure.clone());
        run.until = Some(Arc::new(move |url: String| {
            let (call, failure) = (call.clone(), failure.clone());
            Box::pin(async move {
                call(url).await.unwrap_or_else(|_| {
                    failure.fail();
                    false
                })
            })
        }));
    }
    match (refs, resolve_secret) {
        (None, _) => {}
        (Some(refs), Some(function)) => {
            let resolve = resolver(event_loop.clone(), function);
            run.secrets = Some(Arc::new(HostSecrets { refs, resolve, failure: failure.clone() }));
        }
        (Some(_), None) => return Err(PyValueError::new_err("secrets are named and no resolver was given")),
    }
    if let Some(function) = check_access {
        run.check_access = Some(bridge::check_access(host_call(event_loop.clone(), function, no_argument, kind)));
    }
    if let Some(function) = jev {
        let call: HostCall<(String, String), String> = host_call(event_loop.clone(), function, pair_argument, text);
        run.jev = Some(Arc::new(HostJev { call }));
    }
    if let Some(function) = llm {
        let call: HostCall<LlmCall, String> = host_call(event_loop.clone(), function, llm_argument, text);
        run.llm = Some(Arc::new(HostLlm { call }));
    }
    if let Some(function) = http {
        let call: HostCall<Request, Sent> = host_call(event_loop.clone(), function, request_argument, sent);
        run.http = Some(Http::new(Arc::new(HostTransport { call })));
    }
    // Kept by the run's model clients, which trace their transient failures as Python's do, for a caller collecting them.
    let trace = if trace { Trace::collecting() } else { Trace::disabled() };
    if trace.enabled() {
        run.http = Some(run.http.take().unwrap_or_else(Http::hyper).with_trace(trace.clone()));
    }
    let stopped = cancel.clone();
    run.cancel = Some(Arc::new(move || {
        let stopped = stopped.clone();
        Box::pin(async move { stopped.tripped().await })
    }));
    let handle = Run { result: result.clone().unbind(), finished: finished.clone().unbind(), cancel, trace };
    let (result, finished) = (result.unbind(), finished.unbind());
    runtime().spawn(async move {
        let outcome = fastbrowse_run::run_task(&task, run).await;
        Python::attach(|py| {
            let settle =
                PyCFunction::new_closure(py, None, None, move |args: &Bound<'_, PyTuple>, _| -> PyResult<()> {
                    let py = args.py();
                    let result = result.bind(py);
                    if !result.call_method0("done")?.is_truthy()? {
                        // A callback that failed stopped the run, though the run may have ended on its own in the
                        // same moment; the callback's exception is what the caller hears either way.
                        let error = match &outcome {
                            _ if failure.failed() => Some(refused("callback", String::new())),
                            Ok(answered) => {
                                result.call_method1("set_result", (answered.dump_json(),))?;
                                None
                            }
                            Err(RunError::Cancelled) => Some(refused("cancelled", "the run was cancelled".into())),
                            Err(RunError::Invalid(message)) => Some(refused("invalid", message.clone())),
                            Err(RunError::Configuration(message)) => Some(refused("configuration", message.clone())),
                            Err(RunError::Recording(message)) => Some(refused("recording", message.clone())),
                        };
                        if let Some(error) = error {
                            result.call_method1("set_exception", (error.value(py),))?;
                        }
                    }
                    let finished = finished.bind(py);
                    if !finished.call_method0("done")?.is_truthy()? {
                        finished.call_method1("set_result", (py.None(),))?;
                    }
                    Ok(())
                });
            // A loop that has closed has nobody left to tell.
            if let Ok(settle) = settle {
                let _ = event_loop.bind(py).call_method1("call_soon_threadsafe", (settle,));
            }
        });
    });
    Ok(handle)
}

/// Open a page on a browser that is already running: `connect_cdp`'s data arguments as JSON, and the caller's own
/// `check_access` and `ArtifactSink` wrapped by `fastbrowse.native`. Awaiting it gives a `Page`.
#[pyfunction]
#[pyo3(signature = (options, *, check_access=None, artifact_sink=None))]
fn connect_cdp(
    py: Python<'_>,
    options: &str,
    check_access: Option<Py<PyAny>>,
    artifact_sink: Option<Py<PyAny>>,
) -> PyResult<Py<PyAny>> {
    let event_loop = Arc::new(py.import("asyncio")?.call_method0("get_running_loop")?.unbind());
    let check_access =
        check_access.map(|function| bridge::check_access(host_call(event_loop.clone(), function, no_argument, kind)));
    let artifact_sink = artifact_sink.map(|function| -> Arc<dyn fastbrowse_core::models::ArtifactSink> {
        Arc::new(connect::HostSink { call: host_call(event_loop.clone(), function, artifact_argument, text) })
    });
    connect::connect(py, options, check_access, artifact_sink)
}

fn artifact_argument(
    py: Python<'_>,
    (kind, name, mime_type, content): (String, String, String, Vec<u8>),
) -> PyResult<Bound<'_, PyTuple>> {
    (kind, name, mime_type, PyBytes::new(py, &content)).into_pyobject(py)
}

/// Run the `fastbrowse` command (or `fastbrowse-mcp`, by `program`'s name) with `args` and return its exit code. The
/// command writes to this process's own stdout and stderr and reads its stdin, as the binary does.
#[pyfunction]
fn cli_main(py: Python<'_>, program: String, args: Vec<String>) -> i32 {
    py.detach(move || {
        let code = fastbrowse_cli::entry::main(&program, &args);
        // The binary's `process::exit` flushes what it printed; Python exits without asking this side to.
        let _ = std::io::Write::flush(&mut std::io::stdout());
        code
    })
}

/// Seconds on the clock the core's trace events are read on.
#[pyfunction]
fn monotonic() -> f64 {
    fastbrowse_core::telemetry::monotonic()
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(origin_of, m)?)?;
    m.add_function(wrap_pyfunction!(parse_origin, m)?)?;
    m.add_function(wrap_pyfunction!(text_fragment, m)?)?;
    m.add_function(wrap_pyfunction!(run_task, m)?)?;
    m.add_function(wrap_pyfunction!(connect_cdp, m)?)?;
    m.add_function(wrap_pyfunction!(cli_main, m)?)?;
    m.add_function(wrap_pyfunction!(monotonic, m)?)?;
    m.add_class::<connect::Page>()?;
    m.add("PageFailed", m.py().get_type::<connect::PageFailed>())?;
    m.add_class::<Run>()?;
    m.add_class::<Context>()?;
    m.add("RunRefused", m.py().get_type::<RunRefused>())?;
    m.add("PROTOCOL_VERSION", fastbrowse_core::protocol::PROTOCOL_VERSION)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
