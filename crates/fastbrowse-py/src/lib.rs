//! `fastbrowse._native`: the Rust core in-process for Python.
//!
//! `run_task`, `connect_cdp`, `open_cloud_browser`, `stop_cloud_browser` and `cli_main` are what their namesakes in
//! `fastbrowse` and the commands run.

mod bridge;
mod cloud;
mod connect;
mod host;

use std::path::PathBuf;
use std::sync::Arc;

use fastbrowse_browser::helper::HelperCommand;
use fastbrowse_cli::embed::{HostCall, HostResolve};
use fastbrowse_clients::environment::Settings;
use fastbrowse_clients::http::{Http, Request, Response};
use fastbrowse_core::config::Config;
use fastbrowse_core::models::Event;
use fastbrowse_core::models::SecretRef;
use fastbrowse_core::protocol::CancellationReceipt;
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::telemetry::Trace;
use fastbrowse_run::{Cancel, Host, RunError, RunTask};
use indexmap::IndexMap;
use pyo3::exceptions::{PyException, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyCFunction, PyTuple};

use crate::bridge::{Context, Failure, HostJev, HostLlm, HostSecrets, HostTransport, LlmCall, Sent};
use crate::host::{host_call, runtime};

pyo3::create_exception!(
    fastbrowse._native,
    RunRefused,
    PyException,
    "A run that ended without a result: `args` is why (`invalid`, `configuration`, `recording`, `cancelled` or `callback`) and \
     the message."
);

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

/// An `until` answer: anything but a bool is refused, as `serve` refuses it.
fn strict_bool(answer: &Bound<'_, PyAny>) -> PyResult<bool> {
    answer
        .cast::<pyo3::types::PyBool>()
        .map(|value| value.is_true())
        .map_err(|_| PyValueError::new_err("the result cannot be used: expected true or false"))
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

/// A run under way. Awaiting it gives the `RunResult` as JSON and its final frame's bytes (or None); cancelling the
/// task that awaits it stops the run.
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

    /// An asyncio future that resolves once the run has returned and its browser is closed: with the
    /// accounting receipt as JSON for a completed run, cancellation or refusal after spending, and None otherwise.
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
    (call.purpose, call.messages, call.name, call.schema, context).into_pyobject(py)
}

fn request_argument(py: Python<'_>, request: Request) -> PyResult<Bound<'_, PyTuple>> {
    let body = PyBytes::new(py, &request.body);
    (request.method.as_str(), request.url, request.headers, body, request.max_bytes, request.timeout.as_secs_f64())
        .into_pyobject(py)
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

/// Start a run on the running event loop. `task` is the run as JSON, the `RunTask` every entry point shares
/// (`fastbrowse.native` writes it); `browser_api_key`, `secrets` (the refs the resolver offers, as JSON), `config`
/// (JSON) and `settings` (environment variable names to values, as JSON, read instead of the environment and `.env`)
/// are the host's side. Each callback is a Python function: `on_event` gets each event as JSON, `on_frame` each JPEG as
/// bytes, `until` the final URL, `resolve_secret` a name and an origin. `open_browser` answers the address of the browser it opened; `page_ready` is told when the first page is open. `check_access`, `jev`,
/// `llm` and `http` are the caller's own objects wrapped by `fastbrowse.native` into functions that answer JSON (see `bridge`).
///
/// Each is called with the GIL held, and a coroutine one returns is run on the loop and awaited. An event, frame,
/// until or secret callback that raises stops the run, which then refuses with `callback`.
#[pyfunction]
#[pyo3(signature = (
    task, *, browser_api_key=None, secrets=None, config=None, settings=None, on_event=None, on_frame=None, until=None,
    resolve_secret=None, check_access=None, open_browser=None, page_ready=None, jev=None, llm=None, http=None, trace=false,
))]
#[allow(clippy::too_many_arguments)]
fn run_task(
    py: Python<'_>,
    task: &str,
    browser_api_key: Option<String>,
    secrets: Option<&str>,
    config: Option<&str>,
    settings: Option<&str>,
    on_event: Option<Py<PyAny>>,
    on_frame: Option<Py<PyAny>>,
    until: Option<Py<PyAny>>,
    resolve_secret: Option<Py<PyAny>>,
    check_access: Option<Py<PyAny>>,
    open_browser: Option<Py<PyAny>>,
    page_ready: Option<Py<PyAny>>,
    jev: Option<Py<PyAny>>,
    llm: Option<Py<PyAny>>,
    http: Option<Py<PyAny>>,
    trace: bool,
) -> PyResult<Run> {
    let invalid = |error: serde_json::Error| refused("invalid", error.to_string());
    let task: RunTask = serde_json::from_str(task).map_err(invalid)?;
    let refs: Option<Vec<SecretRef>> = secrets.map(serde_json::from_str).transpose().map_err(invalid)?;
    let config: Option<Config> = config.map(serde_json::from_str).transpose().map_err(invalid)?;
    let settings: Option<IndexMap<String, String>> = settings.map(serde_json::from_str).transpose().map_err(invalid)?;
    let settings = settings
        .map(|settings| {
            let pairs: Vec<(String, String)> = settings.into_iter().collect();
            Settings::from_sources(&pairs, None)
        })
        .transpose()
        .map_err(|error| refused("configuration", error.0))?;
    let mut run = Host { browser_api_key, config, settings, ..Host::default() };
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
    if let Some(function) = open_browser {
        let call: HostCall<(), String> = host_call(event_loop.clone(), function, no_argument, text);
        run.open_browser = Some(Arc::new(move || {
            let call = call.clone();
            Box::pin(async move { call(()).await })
        }));
    }
    if let Some(function) = page_ready {
        let (call, failure) = (host_call(event_loop.clone(), function, no_argument, nothing), failure.clone());
        run.page_ready = Some(Arc::new(move || {
            let (call, failure) = (call.clone(), failure.clone());
            Box::pin(async move {
                if call(()).await.is_err() {
                    failure.fail();
                }
            })
        }));
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
    // Transient client failures must remain inspectable when a later retry succeeds.
    let trace = if trace { Trace::collecting() } else { Trace::disabled() };
    if trace.enabled() {
        run.http = Some(run.http.take().unwrap_or_else(Http::hyper).with_trace(trace.clone()));
    }
    run.cancel = cancel.clone();
    let handle = Run { result: result.clone().unbind(), finished: finished.clone().unbind(), cancel, trace };
    let (result, finished) = (result.unbind(), finished.unbind());
    runtime().spawn(async move {
        let outcome = fastbrowse_run::run_task(task, run).await;
        Python::attach(|py| {
            let settle =
                PyCFunction::new_closure(py, None, None, move |args: &Bound<'_, PyTuple>, _| -> PyResult<()> {
                    let py = args.py();
                    let result = result.bind(py);
                    if !result.call_method0("done")?.is_truthy()? {
                        // A callback that failed stopped the run, though the run may have ended on its own in the
                        // same moment; the callback's exception is what the caller hears either way.
                        let error = match outcome.as_ref().map_err(RunError::cause) {
                            _ if failure.failed() => Some(refused("callback", String::new())),
                            Ok(answered) => {
                                // The final frame is left out of the JSON, so it travels beside it as bytes.
                                let frame = answered.final_frame.as_deref().map(|frame| PyBytes::new(py, frame));
                                result.call_method1("set_result", ((answered.dump_json(), frame),))?;
                                None
                            }
                            Err(RunError::Cancelled(receipt)) => Some(refused("cancelled", receipt.why().to_owned())),
                            Err(RunError::Invalid(message) | RunError::UnsupportedSchema(message)) => {
                                Some(refused("invalid", message.clone()))
                            }
                            Err(RunError::Configuration(message)) => Some(refused("configuration", message.clone())),
                            Err(RunError::Recording(message)) => Some(refused("recording", message.clone())),
                            Err(RunError::AfterSpending { .. }) => unreachable!("cause unwraps accounting envelopes"),
                        };
                        if let Some(error) = error {
                            if let Err(refusal) = &outcome
                                && let Some(receipt) = refusal.receipt()
                            {
                                error.value(py).setattr("receipt", receipt.dump_json())?;
                            }
                            result.call_method1("set_exception", (error.value(py),))?;
                        }
                    }
                    // Python can cancel before delivery or after it while the awaiting task has not resumed,
                    // so every finished outcome keeps its receipt until the caller has heard it.
                    let receipt = match &outcome {
                        Err(error) => match error.receipt() {
                            Some(receipt) => receipt.dump_json().into_pyobject(py)?.into_any(),
                            None => py.None().into_bound(py),
                        },
                        Ok(answered) => CancellationReceipt {
                            steps: answered.steps.clone(),
                            cost: answered.cost.clone(),
                            recordings: answered.recordings.clone(),
                            browser: answered.browser.clone().map(Box::new),
                            reason: None,
                        }
                        .dump_json()
                        .into_pyobject(py)?
                        .into_any(),
                    };
                    let finished = finished.bind(py);
                    if !finished.call_method0("done")?.is_truthy()? {
                        finished.call_method1("set_result", (receipt,))?;
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
/// `prefix`, `installer` and `checkout` describe the environment the package is installed in, for `self-update`.
#[pyfunction]
#[pyo3(signature = (program, args, git_sha, git_dirty, prefix=None, installer=None, checkout=false))]
#[allow(clippy::too_many_arguments)]
fn cli_main(
    py: Python<'_>,
    program: String,
    args: Vec<String>,
    git_sha: Option<String>,
    git_dirty: Option<bool>,
    prefix: Option<std::path::PathBuf>,
    installer: Option<String>,
    checkout: bool,
) -> i32 {
    let wheel = prefix.map(|prefix| fastbrowse_cli::update::Wheel { prefix, installer, checkout });
    py.detach(move || {
        let build = fastbrowse_cli::cli::Build { git_sha, git_dirty, wheel };
        let code = fastbrowse_cli::entry::main(&program, &args, build);
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

/// Validate and normalize a recipe using the file loader's own parser.
#[pyfunction]
fn validate_recipe(text: &str) -> PyResult<String> {
    let recipe = fastbrowse_core::recipe::Recipe::from_json(text).map_err(PyValueError::new_err)?;
    serde_json::to_string(&recipe).map_err(|error| PyValueError::new_err(error.to_string()))
}

/// The recipe file schema, described by the Rust types.
#[pyfunction]
fn recipe_schema() -> String {
    fastbrowse_core::recipe::schema().to_string()
}

/// `fastbrowse.routing.openrouter_routing`: the OpenRouter provider routing a run sends for `model`, as JSON.
#[pyfunction]
fn openrouter_routing(model: &str) -> String {
    fastbrowse_clients::openai_compatible::openrouter_routing_for(model).to_string()
}

/// `fastbrowse.origins.origin_of`: the origin the document grant compares, or None for no http(s) address.
#[pyfunction]
fn origin_of(url: &str) -> Option<String> {
    fastbrowse_core::origins::origin_of(url)
}

/// `fastbrowse.origins.parse_origin`: the normalized origin a grant entry names, or `ValueError`.
#[pyfunction]
fn parse_origin(value: &str) -> PyResult<String> {
    fastbrowse_core::origins::parse_origin(value).map_err(|error| PyValueError::new_err(error.to_string()))
}

/// `fastbrowse.safety.origin_of`: the origin a secret is declared for and typed on, `*.` patterns included.
#[pyfunction]
fn secret_origin(url: &str) -> String {
    fastbrowse_core::safety::origin_of(url)
}

/// `fastbrowse.safety.secret_allowed`, by the rule the run itself applies before it asks for a value.
#[pyfunction]
fn secret_allowed(origins: Vec<String>, origin: &str) -> bool {
    let secret = fastbrowse_core::models::SecretRef { name: String::new(), origins };
    fastbrowse_core::safety::secret_allowed(&secret, origin)
}

/// `fastbrowse.totp`: the code an authenticator key gives at unix time `when`, or `ValueError` naming why it cannot.
#[pyfunction]
fn totp_at(key: &str, when: f64) -> PyResult<String> {
    fastbrowse_cli::totp::totp_at(key, when).map_err(|error| PyValueError::new_err(error.0))
}

/// `fastbrowse.totp`: how many seconds each of the key's codes lasts, or `ValueError`.
#[pyfunction]
fn totp_period(key: &str) -> PyResult<u64> {
    fastbrowse_cli::totp::period_of(key).map_err(|error| PyValueError::new_err(error.0))
}

/// The command that reads PDFs: this interpreter running `fastbrowse/_helper.py`, or None where this process is no
/// interpreter that can be run again.
///
/// A process whose arguments were parsed as Python can be run again as Python. That parsing means `sys.argv`
/// lacks the program and interpreter options `sys.orig_argv` keeps. An application that embeds the interpreter (a
/// web server, a Rust or C program) leaves `orig_argv` empty, or equal to `argv` when it hands its own arguments over
/// unparsed, and running it with Python's arguments would start that application for every PDF. A frozen application
/// has no interpreter at all.
///
/// The program is this process's own executable, not `sys.executable`: a Windows venv's `python.exe` is a launcher
/// that starts the real interpreter as its child, and killing the launcher would leave that child reading. `-S` keeps
/// the interpreter's `site` and the `.pth` files it runs out, which the script needs none of, since it loads this
/// module by path; `-I` keeps out the environment's `PYTHON*` settings, the user's site and the current directory.
fn pdf_helper(py: Python<'_>) -> PyResult<Option<HelperCommand>> {
    let sys = py.import("sys")?;
    if sys.hasattr("frozen")? || !sys.hasattr("argv")? {
        return Ok(None);
    }
    let original = sys.getattr("orig_argv")?;
    if original.len()? == 0 || original.eq(sys.getattr("argv")?)? {
        return Ok(None);
    }
    let Some(package) = sys.getattr("modules")?.get_item("fastbrowse").ok() else { return Ok(None) };
    // Every directory the package spans, not the first: another installation can put its own ahead of this one, and
    // a path, not a str, since a name that is no UTF-8 still names a directory.
    let path = package.getattr("__path__")?;
    let directories = path.try_iter()?.flatten().filter_map(|directory| directory.extract::<PathBuf>().ok());
    let Some(script) = directories.map(|directory| directory.join("_helper.py")).find(|script| script.is_file()) else {
        return Ok(None);
    };
    let Ok(interpreter) = std::env::current_exe() else { return Ok(None) };
    Ok(Some(HelperCommand::new(interpreter).arg("-I").arg("-S").arg(script)))
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Reading no PDF text is said on each PDF a run meets; failing the import would take every run down with it.
    if let Ok(Some(command)) = pdf_helper(m.py()) {
        fastbrowse_browser::helper::register(command);
    }
    m.add_function(wrap_pyfunction!(run_task, m)?)?;
    m.add_function(wrap_pyfunction!(connect_cdp, m)?)?;
    m.add_function(wrap_pyfunction!(cloud::open_cloud_browser, m)?)?;
    m.add_function(wrap_pyfunction!(cloud::stop_cloud_browser, m)?)?;
    m.add_function(wrap_pyfunction!(cli_main, m)?)?;
    m.add_function(wrap_pyfunction!(monotonic, m)?)?;
    m.add_function(wrap_pyfunction!(validate_recipe, m)?)?;
    m.add_function(wrap_pyfunction!(recipe_schema, m)?)?;
    m.add_function(wrap_pyfunction!(openrouter_routing, m)?)?;
    m.add_function(wrap_pyfunction!(origin_of, m)?)?;
    m.add_function(wrap_pyfunction!(parse_origin, m)?)?;
    m.add_function(wrap_pyfunction!(secret_origin, m)?)?;
    m.add_function(wrap_pyfunction!(secret_allowed, m)?)?;
    m.add_function(wrap_pyfunction!(totp_at, m)?)?;
    m.add_function(wrap_pyfunction!(totp_period, m)?)?;
    m.add_class::<connect::Page>()?;
    m.add("PageFailed", m.py().get_type::<connect::PageFailed>())?;
    m.add("CloudFailed", m.py().get_type::<cloud::CloudFailed>())?;
    m.add_class::<Run>()?;
    m.add_class::<Context>()?;
    m.add("RunRefused", m.py().get_type::<RunRefused>())?;
    m.add("PROTOCOL_VERSION", fastbrowse_core::protocol::PROTOCOL_VERSION)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
