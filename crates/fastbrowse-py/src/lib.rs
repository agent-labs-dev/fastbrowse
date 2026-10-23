//! `fastbrowse_native`: the Rust core in-process for Python. Each function stands in for its namesake in
//! `fastbrowse.origins` / `fastbrowse.citations` when `FASTBROWSE_RUST_CORE=1`.
//!
//! Strings cross as USVStrings, the way WebIDL hands them to a URL parser.

use fastbrowse_core::{citations, origins};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyString;

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

#[pymodule]
fn fastbrowse_native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(origin_of, m)?)?;
    m.add_function(wrap_pyfunction!(parse_origin, m)?)?;
    m.add_function(wrap_pyfunction!(text_fragment, m)?)?;
    Ok(())
}
