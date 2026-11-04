//! `fastbrowse.cloud`: a Browser Use Cloud browser the caller starts and stops itself, outside any run.
//!
//! An embedder that keeps one browser per user session starts it here, hands its DevTools address to each
//! `run_task(cdp_url=...)` or `connect_cdp`, and stops it when the session ends. It is the adapter a run starts its own
//! cloud browser with, so the request, the cancellation handling and the metered cost are the run's.

use std::sync::Arc;

use fastbrowse_browser::adapters::browser_use_cloud::{
    API_ORIGIN, BrowserUseCloudBrowser, BrowserUseCloudError, BrowserUseCloudOptions,
};
use fastbrowse_clients::environment::Settings;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use crate::host::{awaitable, runtime};

pyo3::create_exception!(
    fastbrowse._native,
    CloudFailed,
    PyException,
    "A cloud browser that could not be made, started or stopped: `args` is the kind (`invalid`, `configuration`, \
     `error` or `unavailable`), the message, and the seconds the service asked to be left alone for, or None."
);

fn refused(kind: &str, message: String) -> PyErr {
    CloudFailed::new_err((kind.to_string(), message, None::<f64>))
}

fn failed(error: BrowserUseCloudError) -> PyErr {
    let kind = if error.is_unavailable() { "unavailable" } else { "error" };
    CloudFailed::new_err((kind.to_string(), error.to_string(), error.retry_after()))
}

/// The options `run_task` shapes its own cloud browser with.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Options {
    proxy_country: Option<String>,
    timeout_minutes: u32,
    profile: Option<String>,
    viewport: Option<(u32, u32)>,
    extensions: Vec<String>,
}

/// One cloud browser. `start` answers its connection as JSON, `stop` stops it, `cost` reads its metered lines.
#[pyclass(frozen)]
pub struct CloudBrowser {
    // Taken in `drop`, so the adapter's own drop, which stops a browser still running, finds the runtime it spawns on.
    browser: Option<Arc<Mutex<BrowserUseCloudBrowser>>>,
}

impl CloudBrowser {
    fn browser(&self) -> Arc<Mutex<BrowserUseCloudBrowser>> {
        self.browser.clone().expect("the browser is taken only on drop")
    }
}

#[pymethods]
impl CloudBrowser {
    /// `api_key` falls back to `BROWSER_USE_API_KEY` from the environment, then `.env`, as a run's does. `origin`
    /// serves the API from somewhere else, for a test.
    #[new]
    #[pyo3(signature = (options, *, api_key=None, origin=None))]
    fn new(options: &str, api_key: Option<String>, origin: Option<String>) -> PyResult<Self> {
        let options: Options = serde_json::from_str(options).map_err(|error| refused("invalid", error.to_string()))?;
        if options.viewport.is_some_and(|(width, height)| width == 0 || height == 0) {
            return Err(refused("invalid", "viewport width and height must be greater than zero".into()));
        }
        let proxy_country = options
            .proxy_country
            .map(|country| fastbrowse_run::country_code(&country))
            .transpose()
            .map_err(|error| refused("invalid", format!("proxy_country: {error}")))?;
        let api_key = match api_key {
            Some(key) => key,
            None => Settings::load()
                .and_then(|settings| settings.browser_key().map(str::to_owned))
                .map_err(|error| refused("configuration", error.0))?,
        };
        let options = BrowserUseCloudOptions {
            proxy_country,
            timeout_minutes: options.timeout_minutes,
            profile: options.profile,
            viewport: options.viewport,
            extensions: options.extensions,
            ..BrowserUseCloudOptions::default()
        };
        let origin = origin.as_deref().unwrap_or(API_ORIGIN);
        let browser = BrowserUseCloudBrowser::at(&api_key, options, origin).map_err(failed)?;
        Ok(Self { browser: Some(Arc::new(Mutex::new(browser))) })
    }

    /// Create the browser and find its DevTools address: `{cdp_url, live_url, browser_id}` as JSON. A browser created
    /// by a start that then fails is stopped before the error is raised.
    fn start<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let browser = self.browser();
        awaitable(py, async move { browser.lock().await.start().await }, |py, started| {
            let connection = started.map_err(failed)?;
            let answer = json!({
                "cdp_url": connection.cdp_url, "live_url": connection.live_url, "browser_id": connection.browser_id,
            });
            Ok(answer.to_string().into_pyobject(py)?.into_any().unbind())
        })
    }

    /// Stop the browser, which bills until stopped or timed out. A start still in flight is waited for first, so its
    /// browser is stopped too; the request runs on if the caller stops waiting.
    fn stop<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let browser = self.browser();
        awaitable(py, async move { browser.lock().await.stop().await }, |py, stopped| {
            stopped.map_err(failed)?;
            Ok(py.None())
        })
    }

    /// The browser and proxy lines the service metered, as JSON; empty before any answer, and while a start or stop
    /// holds the browser.
    fn cost(&self) -> PyResult<String> {
        let lines = self.browser().try_lock().map(|browser| browser.cost()).unwrap_or_default();
        serde_json::to_string(&lines).map_err(|error| refused("error", error.to_string()))
    }
}

impl Drop for CloudBrowser {
    fn drop(&mut self) {
        // Python drops this on its own thread, outside the runtime, where a browser still running could not be
        // stopped and would bill until its timeout.
        let _entered = runtime().enter();
        drop(self.browser.take());
    }
}
