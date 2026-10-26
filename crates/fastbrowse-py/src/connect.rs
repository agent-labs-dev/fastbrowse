//! `connect_cdp` on the Rust core: a page on a browser already running, for a caller to drive without the agent.
//!
//! Each method takes and answers JSON (the models `fastbrowse.native` validates), or bytes for a screenshot. A browser
//! failure raises `PageFailed` with the Python class name and the message, which `fastbrowse.native` raises as that
//! class.

use std::path::PathBuf;
use std::sync::Arc;

use fastbrowse_browser::error::BrowserError;
use fastbrowse_browser::session::CheckAccess;
use fastbrowse_cli::embed::HostCall;
use fastbrowse_core::BoxFuture;
use fastbrowse_core::config::Config;
use fastbrowse_core::models::{Artifact, ArtifactKind, ArtifactSink};
use fastbrowse_core::page::{Action, Observation};
use fastbrowse_run::{ConnectCdp, Connected};
use pyo3::exceptions::{PyException, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::host::awaitable;

pyo3::create_exception!(
    fastbrowse._native,
    PageFailed,
    PyException,
    "A browser failure: `args` is the Python class it stands for and its message."
);

fn failed(error: BrowserError) -> PyErr {
    PageFailed::new_err((error.kind().to_string(), error.to_string()))
}

fn parsed<T: DeserializeOwned>(text: &str) -> PyResult<T> {
    serde_json::from_str(text).map_err(|error| PyValueError::new_err(error.to_string()))
}

fn json<T: serde::Serialize>(py: Python<'_>, value: &T) -> PyResult<Py<PyAny>> {
    let text = serde_json::to_string(value).map_err(|error| PyValueError::new_err(error.to_string()))?;
    Ok(text.into_pyobject(py)?.into_any().unbind())
}

/// `connect_cdp`'s data arguments.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Options {
    port: Option<u16>,
    cdp_url: Option<String>,
    host: String,
    target_match: Option<String>,
    attach: bool,
    foreground: bool,
    allowed_origins: Option<Vec<String>>,
    config: Option<Config>,
    downloads: Option<PathBuf>,
}

/// The caller's `ArtifactSink`. Its wrapper answers the `Artifact` as JSON.
pub struct HostSink {
    pub call: HostCall<(String, String, String, Vec<u8>), String>,
}

impl ArtifactSink for HostSink {
    fn put<'a>(
        &'a self,
        kind: ArtifactKind,
        name: &'a str,
        mime_type: &'a str,
        content: &'a [u8],
    ) -> BoxFuture<'a, std::io::Result<Artifact>> {
        Box::pin(async move {
            let arguments = (kind.as_str().to_string(), name.to_string(), mime_type.to_string(), content.to_vec());
            let text = (self.call)(arguments).await.map_err(std::io::Error::other)?;
            serde_json::from_str(&text).map_err(std::io::Error::other)
        })
    }
}

pub fn connect(
    py: Python<'_>,
    options: &str,
    check_access: Option<CheckAccess>,
    artifact_sink: Option<Arc<dyn ArtifactSink>>,
) -> PyResult<Py<PyAny>> {
    let options: Options = parsed(options)?;
    let connect = ConnectCdp {
        port: options.port,
        cdp_url: options.cdp_url,
        host: options.host,
        target_match: options.target_match,
        attach: options.attach,
        foreground: options.foreground,
        allowed_origins: options.allowed_origins,
        check_access,
        config: options.config,
        artifact_sink,
        downloads: options.downloads,
    };
    let opened = awaitable(py, fastbrowse_run::connect_cdp(connect), |py, connected| {
        let page = Page { connected: Arc::new(connected.map_err(failed)?) };
        Ok(Bound::new(py, page)?.into_any().unbind())
    })?;
    Ok(opened.unbind())
}

/// A page `connect_cdp` opened. `close` closes its session; an attached window is left open.
#[pyclass(frozen)]
pub struct Page {
    connected: Arc<Connected>,
}

#[pymethods]
impl Page {
    fn observe<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let connected = self.connected.clone();
        awaitable(py, async move { connected.page.observe().await }, |py, done| json(py, &done.map_err(failed)?))
    }

    fn capture<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let connected = self.connected.clone();
        awaitable(py, async move { connected.page.capture().await }, |py, done| json(py, &done.map_err(failed)?))
    }

    fn act<'py>(&self, py: Python<'py>, action: &str, observation: &str) -> PyResult<Bound<'py, PyAny>> {
        let (action, observation): (Action, Observation) = (parsed(action)?, parsed(observation)?);
        let connected = self.connected.clone();
        awaitable(py, async move { connected.page.act(&action, &observation).await }, |py, done| {
            json(py, &done.map_err(failed)?)
        })
    }

    #[pyo3(signature = (url, load_timeout_seconds=15.0, back_to=None))]
    fn navigate<'py>(
        &self,
        py: Python<'py>,
        url: String,
        load_timeout_seconds: f64,
        back_to: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let connected = self.connected.clone();
        let navigating =
            async move { connected.page.navigate_within(&url, load_timeout_seconds, back_to.as_deref()).await };
        awaitable(py, navigating, |py, done| {
            done.map_err(failed)?;
            Ok(py.None())
        })
    }

    fn screenshot<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let connected = self.connected.clone();
        awaitable(py, async move { connected.page.screenshot().await }, |py, done| {
            Ok(PyBytes::new(py, &done.map_err(failed)?).into_any().unbind())
        })
    }

    fn origin<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let connected = self.connected.clone();
        awaitable(py, async move { connected.page.origin().await }, |py, done| {
            Ok(done.map_err(failed)?.into_pyobject(py)?.into_any().unbind())
        })
    }

    fn address<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let connected = self.connected.clone();
        awaitable(py, async move { connected.page.address().await }, |py, done| {
            Ok(done.map_err(failed)?.into_pyobject(py)?.into_any().unbind())
        })
    }

    fn document_changed<'py>(&self, py: Python<'py>, observation: &str) -> PyResult<Bound<'py, PyAny>> {
        let observation: Observation = parsed(observation)?;
        let connected = self.connected.clone();
        awaitable(py, async move { connected.page.document_changed(&observation).await }, |py, done| {
            Ok(pyo3::types::PyBool::new(py, done.map_err(failed)?).to_owned().into_any().unbind())
        })
    }

    fn response_status<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let connected = self.connected.clone();
        awaitable(py, async move { connected.page.response_status().await }, |py, done| {
            Ok(done.map_err(failed)?.into_pyobject(py)?.into_any().unbind())
        })
    }

    /// The artifacts kept so far, as JSON.
    fn artifacts(&self) -> PyResult<String> {
        serde_json::to_string(&self.connected.page.artifacts())
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }

    fn withhold_frames(&self, withheld: bool) {
        self.connected.page.withhold_frames(withheld);
    }

    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let connected = self.connected.clone();
        awaitable(py, async move { connected.session.close().await }, |py, done| {
            done.map_err(failed)?;
            Ok(py.None())
        })
    }
}
