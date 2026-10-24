//! The Rust core in-process for Node, loaded as a `.node` addon.
//!
//! napi converts a JS string to UTF-8 with U+FFFD for a lone surrogate, as WebIDL does for a USVString.

use fastbrowse_core::{citations, origins};
use napi::bindgen_prelude::*;
use napi_derive::napi;

#[napi]
pub fn origin_of(url: String) -> Option<String> {
    origins::origin_of(&url)
}

#[napi]
pub fn parse_origin(value: String) -> Result<String> {
    origins::parse_origin(&value).map_err(|e| Error::new(Status::InvalidArg, e.to_string()))
}

#[napi]
pub fn parse_origins(values: Vec<String>) -> Result<Vec<String>> {
    origins::parse_origins(values).map_err(|e| Error::new(Status::InvalidArg, e.to_string()))
}

#[napi]
pub fn text_fragment(url: String, text: String) -> Result<String> {
    citations::text_fragment(&url, &text).map_err(|e| Error::new(Status::InvalidArg, e.to_string()))
}

#[napi]
pub struct OriginGrant {
    inner: origins::OriginGrant,
}

#[napi]
impl OriginGrant {
    #[napi(constructor)]
    pub fn new(origins: Vec<String>) -> Self {
        Self { inner: origins::OriginGrant::new(origins) }
    }

    #[napi]
    pub fn allows(&self, url: String) -> bool {
        self.inner.allows(&url)
    }
}
