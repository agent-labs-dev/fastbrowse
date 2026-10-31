//! Test seams added for the safety, speed and serve suites; see the module docs of `testing`.

use std::sync::{Arc, Mutex};

use fastbrowse_core::config::Config;
use fastbrowse_core::models::{Attachment, StepOutcome};
use fastbrowse_core::origins::OriginGrant;
use serde_json::json;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::cdp::secs;
use crate::error::BrowserError;
use crate::page::act::Point;
use crate::page::{CdpPage, FRAME_READ_SECONDS, FRAME_RECHECK_SECONDS, SETTLE_SECONDS};
use crate::session::{BrowserSession, Shared, SnapshotScript, State, py_json};

/// The longest an action waits for the page to settle.
pub fn settle_seconds() -> f64 {
    SETTLE_SECONDS
}

/// A child frame's full read budget.
pub fn frame_read_seconds() -> f64 {
    FRAME_READ_SECONDS
}

/// As if `FRAME_RECHECK_SECONDS` had passed since each frame missed its deadline, so the next read
/// gives every such frame its full budget again.
pub fn expire_frame_rechecks(page: &CdpPage) {
    let past = Instant::now().checked_sub(secs(FRAME_RECHECK_SECONDS)).expect("monotonic clock far enough along");
    for missed in page.state.lock().unwrap().unanswered.values_mut() {
        *missed = past;
    }
}

/// Upload `files` to the last observation's `control_id`.
///
/// With `grant`, the upload runs as if the session held that origin grant: through a page on
/// a session that shares this one's socket, tabs and active tab, and differs only in its grant.
pub async fn upload(
    page: &CdpPage,
    control_id: &str,
    files: &[Attachment],
    grant: Option<Vec<String>>,
) -> Result<(StepOutcome, Option<String>), BrowserError> {
    let target = page.state.lock().unwrap().last.as_ref().and_then(|l| l.controls.get(control_id).cloned());
    let target = target.expect("the control was observed");
    let Some(grant) = grant else { return page.upload(Some(&target), files, Point::At(1.0, 1.0)).await };
    let shared = &page.session().0;
    let mut origins = grant.clone();
    origins.sort();
    let snapshot = SnapshotScript::new(&py_json(&json!(origins)));
    let state = {
        let state = shared.state.lock().unwrap();
        State { tabs: state.tabs.clone(), active_target_id: state.active_target_id.clone(), ..State::default() }
    };
    let mut connection = shared.connection.clone();
    connection.allowed_origins = Some(grant.clone());
    let sibling = BrowserSession(Arc::new(Shared {
        connection,
        check_access: None,
        grant: Some(OriginGrant::new(grant)),
        origins,
        artifact_sink: shared.artifact_sink.clone(),
        max_download_bytes: shared.max_download_bytes,
        new_document_scripts: vec![],
        on_frame: None,
        client: Mutex::new(shared.client.lock().unwrap().clone()),
        state: Mutex::new(state),
        dialog_opened: watch::channel(0).0,
        lifecycle: watch::channel(0).0,
        snapshot,
        popup_signal: watch::channel(0).0,
        screencast_lock: tokio::sync::Mutex::new(()),
    }));
    CdpPage::new(sibling, Config::default()).upload(Some(&target), files, Point::At(1.0, 1.0)).await
}
