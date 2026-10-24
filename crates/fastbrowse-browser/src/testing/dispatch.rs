//! Test seams added for the dispatch suites; see the module docs of `testing`.

use crate::page::CdpPage;

/// `await page._form_unchanged(target, text)`, with `target` as (session id, frame key, local node id, guard digest).
pub async fn form_unchanged(page: &CdpPage, target: (String, String, i64, Option<String>), text: &str) -> bool {
    page.form_unchanged(&target, text).await
}
