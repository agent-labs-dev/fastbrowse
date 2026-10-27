//! Calling Python from the run's threads: each call holds the GIL only to start it, and a coroutine it returns runs on
//! the caller's event loop while the run awaits it.

use std::sync::{Arc, Mutex, OnceLock};

use fastbrowse_cli::embed::HostCall;
use pyo3::prelude::*;
use pyo3::types::{PyCFunction, PyTuple};
use tokio::sync::oneshot;

/// The module's own runtime. Its threads run the agent while the event loop's thread runs Python.
pub fn runtime() -> &'static tokio::runtime::Runtime {
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
pub fn message_of(py: Python<'_>, error: &PyErr) -> String {
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
pub fn host_call<A, R>(
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
