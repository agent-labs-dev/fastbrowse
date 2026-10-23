//! page.py's observation path: snapshot.js in the main frame, then in every cross-origin frame's own session at
//! once, merged into one control list with `observe`'s ids (`main:<id>`, `<target id>:<id>`) and frame ids.

use std::time::Duration;

use futures_util::future::join_all;
use serde_json::{Map, Value, json};
use tokio::time::timeout;

use crate::cdp::CdpError;
use crate::session::{Dialog, Session};

const SNAPSHOT_JS: &str = include_str!("../../../src/fastbrowse/browser/snapshot.js");
/// page.py's `_CAPTURE_LOADING_SECONDS`, `_SETTLE_POLL_SECONDS` and `_FRAME_READ_SECONDS`.
const LOADING_WAIT_MS: u32 = 3000;
const LOADING_POLL_MS: u32 = 100;
const FRAME_READ: Duration = Duration::from_secs(5);
const MAIN: &str = "main";
const REREADS: u32 = 10;

#[derive(Debug)]
pub enum ReadError {
    Cdp(CdpError),
    Script,
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cdp(e) => e.fmt(f),
            Self::Script => f.write_str("Runtime.evaluate failed (JavaScriptError)"),
        }
    }
}

impl std::error::Error for ReadError {}

impl From<CdpError> for ReadError {
    fn from(e: CdpError) -> Self {
        Self::Cdp(e)
    }
}

#[derive(serde::Serialize)]
pub struct Observation {
    pub url: String,
    pub title: String,
    pub inaccessible_frames: i64,
    pub dialog: Option<Dialog>,
    pub frames: usize,
    pub controls: Vec<Value>,
}

/// page.py's `_page_js` with no origin grant.
fn page_js() -> String {
    format!("(mode => ({SNAPSHOT_JS})(mode, null))")
}

pub async fn evaluate(session: &Session, sid: &str, expression: &str) -> Result<Value, ReadError> {
    let params = json!({"expression": expression, "returnByValue": true, "awaitPromise": true});
    let mut out = session.cdp.call("Runtime.evaluate", params, Some(sid)).await?;
    if out.get("exceptionDetails").is_some() {
        return Err(ReadError::Script);
    }
    Ok(out["result"]["value"].take())
}

/// page.py's `_until_dialog`: `work`'s result, or `None` once a dialog blocks the page. `work` is dropped then,
/// which cancels its pending calls.
pub async fn until_dialog<T>(session: &Session, work: impl Future<Output = T>) -> Option<T> {
    tokio::select! {
        biased;
        out = work => Some(out),
        () = session.wait_for_dialog() => None,
    }
}

/// page.py's `_reread`: read again when a navigation destroyed the document mid-read (CDP -32000). Python waits
/// for the next document to settle between tries; this only pauses a poll interval.
pub async fn observe(session: &Session) -> Result<Observation, ReadError> {
    let mut rereads = 0;
    loop {
        match observe_once(session).await {
            Err(ReadError::Cdp(e)) if e.context_lost() && rereads < REREADS => rereads += 1,
            other => return other,
        }
        tokio::time::sleep(Duration::from_millis(LOADING_POLL_MS.into())).await;
    }
}

/// `CdpPage._observe_once` without history, response status or the caps.
async fn observe_once(session: &Session) -> Result<Observation, ReadError> {
    if let Some((_, dialog)) = session.pending_dialog() {
        // A dialog blocks the renderer, so any page evaluate would hang until it is handled.
        return Ok(Observation {
            url: String::new(),
            title: String::new(),
            inaccessible_frames: 0,
            dialog: Some(dialog),
            frames: 0,
            controls: vec![],
        });
    }
    let page_js = page_js();
    let expression = format!("{page_js}('snapshot')");
    let loaded = format!(
        "new Promise(resolve => {{ const sample = {page_js}; \
         const deadline = performance.now() + {LOADING_WAIT_MS}; \
         const poll = () => {{ const state = sample('fingerprint'); \
         if (!state.loading || state.hidden || performance.now() >= deadline) {{ resolve(null); return; }} \
         setTimeout(poll, {LOADING_POLL_MS}); }}; poll(); }})"
    );
    let waited = format!("(async () => {{ await {loaded}; return ({expression}); }})()");
    let main_sid = session.session_id.clone();
    // Python reads the main frame without racing a dialog; a dialog opened mid-read would hold it to the command
    // deadline. Here it ends the read, and the observation reports the dialog.
    let main = match until_dialog(session, evaluate(session, &main_sid, &waited)).await {
        None => return Box::pin(observe_once(session)).await,
        Some(Ok(main)) => main,
        // A navigation can discard the loading wait; read the new document without it.
        Some(Err(_)) => evaluate(session, &main_sid, &expression).await?,
    };
    // Loading can attach or replace child frames, so they are listed only after the main frame's wait.
    let sources = session.frame_sessions();
    let reads = sources.iter().map(|(_, sid)| async {
        match timeout(FRAME_READ, until_dialog(session, evaluate(session, sid, &expression))).await {
            Ok(Some(Ok(raw))) if raw.is_object() => Some(raw),
            _ => None,
        }
    });
    let children = join_all(reads).await;
    if session.pending_dialog().is_some() {
        return Box::pin(observe_once(session)).await;
    }

    let mut frames = vec![(MAIN.to_owned(), main_sid, main)];
    frames.extend(sources.into_iter().zip(children).filter_map(|((key, sid), raw)| Some((key, sid, raw?))));
    let mut controls = vec![];
    for (key, _, raw) in &frames {
        let frame_id = (key != MAIN).then(|| key.clone());
        for control in raw["controls"].as_array().into_iter().flatten() {
            let mut control: Map<String, Value> = control.as_object().cloned().unwrap_or_default();
            let local = control.remove("id").unwrap_or_default();
            let path =
                control.remove("frame_path").and_then(|p| p.as_str().filter(|p| !p.is_empty()).map(String::from));
            let fid = match path {
                Some(path) => Some(format!("{key}/{path}")),
                None => frame_id.clone(),
            };
            let mut merged = Map::new();
            merged.insert("id".into(), format!("{key}:{local}").into());
            merged.insert("frame_id".into(), fid.into());
            merged.extend(control);
            controls.push(Value::Object(merged));
        }
    }
    // `observe` lists on-screen controls first, each group in frame then document order.
    let (mut onscreen, offscreen): (Vec<_>, Vec<_>) = controls.into_iter().partition(|c| c["offscreen"] != true);
    onscreen.extend(offscreen);
    let controls = onscreen;
    let inaccessible = inaccessible_frames(session, &frames);
    let main = &frames[0].2;
    Ok(Observation {
        url: main["url"].as_str().unwrap_or_default().into(),
        title: main["title"].as_str().unwrap_or_default().into(),
        inaccessible_frames: inaccessible,
        dialog: None,
        frames: frames.len(),
        controls,
    })
}

/// page.py's `_inaccessible_frames`: each frame's unreadable children, less those read in a session of their own.
fn inaccessible_frames(session: &Session, frames: &[(String, String, Value)]) -> i64 {
    let mut missing = 0;
    for (_, sid, raw) in frames {
        let count = raw["inaccessible_frames"].as_i64().unwrap_or(0);
        let captured =
            frames.iter().filter(|(_, child, _)| session.frame_parent(child).as_deref() == Some(sid)).count();
        missing += (count - captured as i64).max(0);
    }
    missing
}
