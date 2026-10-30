//! Where a browser and a login come from: `src/fastbrowse/adapters/`, ported.

#[cfg(feature = "cloud")]
pub mod browser_use_cloud;
pub mod http;
pub mod local_chrome;
