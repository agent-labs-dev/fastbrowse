//! Test seams added for the misc suites; see the module docs of `testing`.

use crate::page::CdpPage;

/// `page._page_js`: the snapshot script, called with a mode (`'snapshot'`, `'fingerprint'`).
pub fn page_js(page: &CdpPage) -> &str {
    &page.page_js
}

/// `page._fingerprint_js`.
pub fn fingerprint_js(page: &CdpPage) -> &str {
    &page.fingerprint_js
}

/// `page_module._REREADS`: how many times a read whose document was destroyed is taken again.
pub fn rereads() -> u32 {
    crate::page::REREADS
}

/// Replace `page._page_js`, as a test that has the snapshot send every row does.
pub fn set_page_js(page: &mut CdpPage, page_js: String) {
    page.page_js = page_js;
}

/// One observed control as `act` finds it again: (control id, session id, frame key, node id, guard digest).
pub type ObservedControl = (String, String, String, i64, Option<String>);

/// `page._last.controls`, sorted by control id.
pub fn last_controls(page: &CdpPage) -> Option<Vec<ObservedControl>> {
    let state = page.state.lock().unwrap();
    let last = state.last.as_ref()?;
    let mut controls: Vec<_> = last
        .controls
        .iter()
        .map(|(id, (session, frame, node, guard))| (id.clone(), session.clone(), frame.clone(), *node, guard.clone()))
        .collect();
    controls.sort();
    Some(controls)
}
