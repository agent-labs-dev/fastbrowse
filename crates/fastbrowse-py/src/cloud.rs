//! `open_cloud_browser` and `stop_cloud_browser` on the Rust core: a Browser Use Cloud browser that outlives a run,
//! started with the request `run_task` sends for its own, and stopped later by its id.

use fastbrowse_browser::adapters::browser_use_cloud::{
    API_ORIGIN, BrowserUseCloudBrowser, BrowserUseCloudError, BrowserUseCloudOptions, real_time,
};
use pyo3::exceptions::{PyException, PyValueError};
use pyo3::prelude::*;
use serde::Deserialize;
use serde_json::json;

use crate::host::{awaitable, runtime};

pyo3::create_exception!(
    fastbrowse._native,
    CloudFailed,
    PyException,
    "A Browser Use Cloud failure: `args` is whether the service was unavailable and the message."
);

fn failed(error: BrowserUseCloudError) -> PyErr {
    CloudFailed::new_err((error.is_unavailable(), error.to_string()))
}

/// `open_cloud_browser`'s arguments. `origin` stands in for Browser Use Cloud's, for tests.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Options {
    browser_api_key: String,
    profile: Option<String>,
    extensions: Vec<String>,
    timeout_minutes: i64,
    proxy_country: Option<String>,
    viewport: Option<(i64, i64)>,
    origin: Option<String>,
}

fn origin(given: Option<String>) -> String {
    given.unwrap_or_else(|| API_ORIGIN.to_owned())
}

fn out_of_range(name: &str) -> PyErr {
    PyValueError::new_err(format!("{name} is out of range"))
}

/// A started browser on its way to the caller, stopped if the caller stopped waiting before it arrived.
struct Unclaimed(Option<BrowserUseCloudBrowser>);

impl Drop for Unclaimed {
    fn drop(&mut self) {
        if let Some(mut cloud) = self.0.take() {
            runtime().spawn(async move { cloud.stop().await });
        }
    }
}

/// Start a cloud browser that no run owns: `options` as JSON. Awaiting it gives the browser's id, CDP and live view
/// addresses as JSON. A start that fails or is cancelled stops what it created.
#[pyfunction]
pub fn open_cloud_browser(py: Python<'_>, options: &str) -> PyResult<Py<PyAny>> {
    let options: Options = serde_json::from_str(options).map_err(|error| PyValueError::new_err(error.to_string()))?;
    let timeout_minutes = u32::try_from(options.timeout_minutes).map_err(|_| out_of_range("timeout_minutes"))?;
    let viewport = match options.viewport {
        None => None,
        Some((width, height)) => Some((
            u32::try_from(width).map_err(|_| out_of_range("viewport"))?,
            u32::try_from(height).map_err(|_| out_of_range("viewport"))?,
        )),
    };
    let cloud_options = BrowserUseCloudOptions {
        proxy_country: options.proxy_country,
        timeout_minutes,
        profile: options.profile,
        viewport,
        extensions: options.extensions,
        ..BrowserUseCloudOptions::default()
    };
    let (clock, sleep) = real_time();
    let mut cloud = BrowserUseCloudBrowser::with_seams(
        &options.browser_api_key,
        cloud_options,
        &origin(options.origin),
        clock,
        sleep,
    )
    .map_err(failed)?;
    let opening = async move {
        // A cancelled start drops this future, and the adapter's own drop stops a browser created by then.
        cloud.start().await.map(|_| Unclaimed(Some(cloud)))
    };
    let opened = awaitable(py, opening, |py, started| {
        let mut cloud = started.map_err(failed)?.0.take().expect("a browser is claimed once");
        let connection = cloud.keep().map_err(failed)?;
        let browser = json!({
            "id": connection.browser_id,
            "cdp_url": connection.cdp_url,
            "live_url": connection.live_url,
        });
        Ok(browser.to_string().into_pyobject(py)?.into_any().unbind())
    })?;
    Ok(opened.unbind())
}

/// Stop the cloud browser `browser_id`. Awaiting it gives None once the service took the stop.
#[pyfunction]
#[pyo3(signature = (browser_api_key, browser_id, *, origin=None))]
pub fn stop_cloud_browser(
    py: Python<'_>,
    browser_api_key: &str,
    browser_id: &str,
    origin: Option<String>,
) -> PyResult<Py<PyAny>> {
    let (clock, sleep) = real_time();
    let mut cloud =
        BrowserUseCloudBrowser::running_with_seams(browser_api_key, browser_id, &self::origin(origin), clock, sleep);
    let stopping = async move { cloud.stop().await };
    let stopped = awaitable(py, stopping, |py, stopped| {
        stopped.map_err(failed)?;
        Ok(py.None())
    })?;
    Ok(stopped.unbind())
}
