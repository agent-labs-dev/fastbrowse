//! Seams for the integration tests, standing in for the private attributes tests/browser reaches into
//! (`session._background`, `session._on_target_created(...)`, `page._last`). Nothing here changes behavior: each
//! function reads state, feeds the session an event as Chrome would, or runs one internal step on its own.

use std::sync::Arc;

use serde_json::Value;

use crate::cdp::Event;
use crate::error::BrowserError;
use crate::page::CdpPage;
use crate::session::{BrowserSession, Frame, Shared, adopt_popup, deliver_frames};

pub mod browser;
pub mod dispatch;
pub mod lifecycle;
pub mod misc;
pub mod safety;

fn shared(session: &BrowserSession) -> &Arc<Shared> {
    &session.0
}

/// Hand the session an event as if Chrome had sent it on `session_id` (`None`: the browser's own).
pub fn dispatch(session: &BrowserSession, method: &str, params: Value, session_id: Option<&str>) {
    let event = Event { method: method.to_owned(), session: session_id.map(str::to_owned), params };
    Shared::on_event(shared(session), &event);
}

/// `await asyncio.gather(*session._background)`: wait for the background work started so far.
pub async fn settle_background(session: &BrowserSession) {
    loop {
        let tasks = std::mem::take(&mut shared(session).state.lock().unwrap().background);
        if tasks.is_empty() {
            return;
        }
        for task in tasks {
            let _ = task.await;
        }
    }
}

/// How many background tasks are still running.
pub fn background_len(session: &BrowserSession) -> usize {
    let mut state = shared(session).state.lock().unwrap();
    state.background.retain(|task| !task.is_finished());
    state.background.len()
}

pub fn owned(session: &BrowserSession) -> Vec<String> {
    let mut owned: Vec<String> = shared(session).state.lock().unwrap().owned.iter().cloned().collect();
    owned.sort();
    owned
}

pub fn keepers(session: &BrowserSession) -> Vec<(String, String)> {
    let mut keepers: Vec<(String, String)> =
        shared(session).state.lock().unwrap().keepers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    keepers.sort();
    keepers
}

/// `session._popups`: (popup target id, opener target id, adopted if known), in the order they opened.
pub fn popups(session: &BrowserSession) -> Vec<(String, String, Option<bool>)> {
    shared(session)
        .state
        .lock()
        .unwrap()
        .popups
        .iter()
        .map(|p| (p.target_id.clone(), p.opener_id.clone(), *p.adopted.borrow()))
        .collect()
}

/// `session._popups[target] = (opener, future)` then `await session._adopt_popup(target, opener)`.
pub async fn adopt(session: &BrowserSession, target_id: &str, opener_id: &str) {
    {
        let shared = shared(session);
        let mut state = shared.state.lock().unwrap();
        if !state.popups.iter().any(|p| p.target_id == target_id) {
            state.popups.push(crate::session::Popup {
                target_id: target_id.to_owned(),
                opener_id: opener_id.to_owned(),
                adopted: tokio::sync::watch::channel(None).0,
            });
        }
    }
    adopt_popup(shared(session), target_id, opener_id, None).await;
}

pub async fn open_owned_tab(session: &BrowserSession, url: &str) -> Result<(), BrowserError> {
    session.open_tab(url, true).await
}

pub async fn prepare_session(session: &BrowserSession, session_id: &str) -> Result<(), BrowserError> {
    session.prepare_session(session_id).await
}

pub fn has_pending_frame(session: &BrowserSession) -> bool {
    shared(session).state.lock().unwrap().pending_frame.is_some()
}

pub fn frame_scheduled(session: &BrowserSession) -> bool {
    shared(session).state.lock().unwrap().frame_scheduled
}

/// `session._pending_frame = _Frame(data, ack, session_id); session._frame_scheduled = True;
/// session._spawn(session._deliver_frames())`.
pub fn deliver_frame(session: &BrowserSession, data: &str, ack_id: i64, session_id: Option<&str>) {
    let shared = shared(session).clone();
    let mut state = shared.state.lock().unwrap();
    state.pending_frame =
        Some(Frame { data: data.to_owned(), ack_id: ack_id.into(), session_id: session_id.map(str::to_owned) });
    state.frame_scheduled = true;
    let delivery = shared.clone();
    state.delivery = Some(tokio::spawn(async move { deliver_frames(&delivery).await }));
}

/// Wait for the frame delivery task, if one runs.
pub async fn settle_delivery(session: &BrowserSession) {
    let delivery = shared(session).state.lock().unwrap().delivery.take();
    if let Some(delivery) = delivery {
        let _ = delivery.await;
    }
}

/// `page._last.page_key`.
pub fn last_page_key(page: &CdpPage) -> Option<String> {
    page.state.lock().unwrap().last.as_ref().map(|l| l.page_key.clone())
}

/// `page._last.controls[control_id]`: (session id, frame key, local node id, guard digest).
pub fn last_target(page: &CdpPage, control_id: &str) -> Option<(String, String, i64, Option<String>)> {
    page.state.lock().unwrap().last.as_ref().and_then(|l| l.controls.get(control_id).cloned())
}

/// `page._back_to`.
pub fn back_to(page: &CdpPage) -> Vec<(String, String)> {
    page.state.lock().unwrap().back_to.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// `page._status_worlds`.
pub fn status_worlds(page: &CdpPage) -> Vec<(String, String)> {
    page.state.lock().unwrap().status_worlds.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}
