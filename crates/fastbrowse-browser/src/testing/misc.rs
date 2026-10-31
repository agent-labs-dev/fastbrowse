//! Test seams added for the misc suites; see the module docs of `testing`.

use crate::page::CdpPage;

/// The snapshot script, called with a mode (`'snapshot'`, `'fingerprint'`).
pub fn page_js(page: &CdpPage) -> &str {
    &page.page_js
}

/// The fingerprint script.
pub fn fingerprint_js(page: &CdpPage) -> &str {
    &page.fingerprint_js
}

/// How many times a read whose document was destroyed is taken again.
pub fn rereads() -> u32 {
    crate::page::REREADS
}
