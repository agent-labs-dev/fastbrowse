//! `fastbrowse_native`: the Rust core in-process for Python.
//!
//! `origin_of`, `parse_origin` and `text_fragment` stand in for their namesakes in `fastbrowse.origins` and
//! `fastbrowse.citations` when `FASTBROWSE_RUST_CORE=1`; `run_task` stands in for `fastbrowse.run_task` then.
//!
//! Strings cross as USVStrings, the way WebIDL hands them to a URL parser.

use std::sync::{Arc, Mutex, OnceLock};

use fastbrowse_cli::embed::{Embedded, Host, HostCall, HostResolve};
use fastbrowse_cli::runner::Cancel;
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::{citations, origins};
use pyo3::exceptions::{PyException, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyCFunction, PyString, PyTuple};
use tokio::sync::oneshot;

pyo3::create_exception!(
    fastbrowse_native,
    RunRefused,
    PyException,
    "A run refused before it began, as `serve` refuses it: `args` is the protocol's error code and the message."
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

/// The module's own runtime. Its threads run the agent while the event loop's thread runs Python.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("fastbrowse")
            .enable_all()
            .build()
            .expect("a tokio runtime starts")
    })
}

/// What a raised exception says: `str(exc)`, or its type's name when that is empty.
fn message_of(py: Python<'_>, error: &PyErr) -> String {
    let value = error.value(py);
    match value.str().map(|text| text.to_string_lossy().into_owned()) {
        Ok(text) if !text.is_empty() => text,
        _ => value.get_type().name().map(|name| name.to_string()).unwrap_or_else(|_| "exception".into()),
    }
}

/// A concurrent future the run is waiting on, cancelled if the run stops waiting: a callback still running when its
/// run is cancelled is cancelled with it, as it would be inside Python's own `run_task`.
struct Pending(Option<Py<PyAny>>);

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(future) = self.0.take() {
            Python::attach(|py| {
                let _ = future.bind(py).call_method0("cancel");
            });
        }
    }
}

/// Call `function` with the GIL held. A coroutine it returns is run on `event_loop` and awaited from here.
fn host_call<A, R>(
    event_loop: Arc<Py<PyAny>>,
    function: Py<PyAny>,
    arguments: fn(Python<'_>, A) -> PyResult<Bound<'_, PyTuple>>,
    answer: fn(&Bound<'_, PyAny>) -> PyResult<R>,
) -> HostCall<A, R>
where
    A: Send + 'static,
    R: Send + 'static,
{
    let function = Arc::new(function);
    Arc::new(move |value: A| {
        let (event_loop, function) = (event_loop.clone(), function.clone());
        Box::pin(async move {
            let (sender, receiver) = oneshot::channel::<Result<R, String>>();
            let sender = Arc::new(Mutex::new(Some(sender)));
            let started = Python::attach(|py| -> Result<Option<Py<PyAny>>, String> {
                let call = || -> PyResult<Option<Py<PyAny>>> {
                    let returned = function.bind(py).call1(arguments(py, value)?)?;
                    let asyncio = py.import("asyncio")?;
                    if !asyncio.call_method1("iscoroutine", (&returned,))?.is_truthy()? {
                        let done = answer(&returned);
                        if let Some(sender) = sender.lock().unwrap_or_else(|p| p.into_inner()).take() {
                            let _ = sender.send(done.map_err(|error| message_of(py, &error)));
                        }
                        return Ok(None);
                    }
                    let future = asyncio.call_method1("run_coroutine_threadsafe", (returned, event_loop.bind(py)))?;
                    let settle = sender.clone();
                    let on_done = PyCFunction::new_closure(py, None, None, move |args: &Bound<'_, PyTuple>, _| {
                        let py = args.py();
                        let done = args.get_item(0).and_then(|future| future.call_method0("result"));
                        let done = match done {
                            Ok(returned) => answer(&returned).map_err(|error| message_of(py, &error)),
                            Err(error) => Err(message_of(py, &error)),
                        };
                        if let Some(sender) = settle.lock().unwrap_or_else(|p| p.into_inner()).take() {
                            let _ = sender.send(done);
                        }
                    })?;
                    future.call_method1("add_done_callback", (on_done,))?;
                    Ok(Some(future.unbind()))
                };
                call().map_err(|error| message_of(py, &error))
            });
            let _pending = Pending(started?);
            receiver.await.unwrap_or_else(|_| Err("the callback was cancelled".into()))
        })
    })
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

    /// An asyncio future that resolves with None once the run has returned and its browser is closed.
    #[getter]
    fn finished(&self, py: Python<'_>) -> Py<PyAny> {
        self.finished.clone_ref(py)
    }
}

/// Start a run on the running event loop. `params` is a `run` request's params as JSON, the shape `serve` reads.
///
/// `on_event` gets each event as JSON, `on_frame` each JPEG as bytes, `until` the final URL, and `resolve_secret`
/// a name and an origin. Each is called with the GIL held, and a coroutine one returns is run on the loop and awaited.
/// A callback that raises ends the run, whose result is then status `error`, as `serve` ends it.
#[pyfunction]
#[pyo3(signature = (params, *, on_event=None, on_frame=None, until=None, resolve_secret=None))]
fn run_task(
    py: Python<'_>,
    params: String,
    on_event: Option<Py<PyAny>>,
    on_frame: Option<Py<PyAny>>,
    until: Option<Py<PyAny>>,
    resolve_secret: Option<Py<PyAny>>,
) -> PyResult<Run> {
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
    let host = Host {
        on_event: on_event.map(|f| host_call(event_loop.clone(), f, json_argument, nothing)),
        on_frame: on_frame.map(|f| host_call(event_loop.clone(), f, bytes_argument, nothing)),
        until: until.map(|f| host_call(event_loop.clone(), f, str_argument, strict_bool)),
        resolve_secret: resolve_secret.map(|f| resolver(event_loop.clone(), f)),
    };
    let run = Run { result: result.clone().unbind(), finished: finished.clone().unbind(), cancel: cancel.clone() };
    let (result, finished) = (result.unbind(), finished.unbind());
    runtime().spawn(async move {
        let outcome = Embedded::new(Vec::new()).run(&params, host, cancel).await;
        Python::attach(|py| {
            let settle =
                PyCFunction::new_closure(py, None, None, move |args: &Bound<'_, PyTuple>, _| -> PyResult<()> {
                    let py = args.py();
                    let result = result.bind(py);
                    if !result.call_method0("done")?.is_truthy()? {
                        match &outcome {
                            Ok(answered) => result.call_method1("set_result", (answered.dump_json(),))?,
                            Err(refused) => {
                                let error = RunRefused::new_err((refused.code.code(), refused.message.clone()));
                                result.call_method1("set_exception", (error.value(py),))?
                            }
                        };
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
    Ok(run)
}

#[pymodule]
fn fastbrowse_native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(origin_of, m)?)?;
    m.add_function(wrap_pyfunction!(parse_origin, m)?)?;
    m.add_function(wrap_pyfunction!(text_fragment, m)?)?;
    m.add_function(wrap_pyfunction!(run_task, m)?)?;
    m.add_class::<Run>()?;
    m.add("RunRefused", m.py().get_type::<RunRefused>())?;
    m.add("PROTOCOL_VERSION", fastbrowse_core::protocol::PROTOCOL_VERSION)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
