//! What the dispatch and form suites share (`eval_value`, `find`, `observe_until`), plus a CDP proxy that stands in for the monkeypatched `page._input` and `page._evaluate`.

#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fastbrowse_browser::page::CdpPage;
use fastbrowse_browser::session::BrowserSession;
use fastbrowse_core::models::Operation;
use fastbrowse_core::page::{Action, Control, Observation};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// `eval_value`: evaluate in `session_id` and return the value, or null when the expression threw or returned none.
pub async fn eval_value(session: &BrowserSession, session_id: &str, expression: &str) -> Value {
    let out = session
        .send("Runtime.evaluate", json!({"expression": expression, "returnByValue": true}), Some(session_id))
        .await
        .expect("Runtime.evaluate");
    out["result"].get("value").cloned().unwrap_or(Value::Null)
}

/// `eval_value` in the active tab, the session nearly every test evaluates in.
pub async fn eval_active(page: &CdpPage, expression: &str) -> Value {
    let session = page.session();
    eval_value(session, &session.active_session_id().expect("active session"), expression).await
}

/// `find`: the first control whose label contains `label`.
pub fn find<'a>(observation: &'a Observation, label: &str) -> &'a Control {
    observation.controls.iter().find(|c| c.label.contains(label)).unwrap_or_else(|| {
        panic!(
            "no control labelled like {label:?} in {:?}",
            observation.controls.iter().map(|c| &c.label).collect::<Vec<_>>()
        )
    })
}

/// `observe_until`: observe until a control is actually there, since an attached frame session predates its document.
pub async fn observe_until(page: &CdpPage, label: &str) -> Observation {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let observation = page.observe().await.expect("observe");
        if observation.controls.iter().any(|c| c.label.contains(label)) {
            return observation;
        }
        assert!(tokio::time::Instant::now() < deadline, "condition was not met in time");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub fn action(operation: Operation, target: &Control) -> Action {
    Action { target_id: Some(target.id.clone()), ..Action::new(operation) }
}

pub fn fill(target: &Control, text: &str) -> Action {
    Action { text: Some(text.to_owned()), ..action(Operation::Fill, target) }
}

/// The error code the proxy answers a doomed command with: a plain browser error, not CDP's -32000 (context lost).
pub const PROXY_ERROR: i64 = -31999;

/// A websocket proxy in front of Chrome's browser endpoint, for the Python tests that monkeypatch a send to fail
/// after it went out. Every command is forwarded; one `doomed` picks has its real reply swapped for a CDP error, so
/// Chrome acts on it and the caller sees it fail, as the patched coroutine did. Returns the proxy's `cdp_url`.
pub async fn failing_proxy(upstream: String, doomed: impl FnMut(&Value) -> bool + Send + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let doomed = Arc::new(Mutex::new(doomed));
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (upstream, doomed) = (upstream.clone(), doomed.clone());
            tokio::spawn(async move {
                let Ok(client) = tokio_tungstenite::accept_async(stream).await else { return };
                let Ok((chrome, _)) = tokio_tungstenite::connect_async(upstream.as_str()).await else { return };
                let (mut to_client, mut from_client) = client.split();
                let (mut to_chrome, mut from_chrome) = chrome.split();
                let failed: Arc<Mutex<HashSet<u64>>> = Arc::default();
                let marked = failed.clone();
                tokio::spawn(async move {
                    while let Some(Ok(message)) = from_client.next().await {
                        if let Message::Text(text) = &message
                            && let Ok(command) = serde_json::from_str::<Value>(text)
                            && let Some(id) = command["id"].as_u64()
                            && (doomed.lock().unwrap())(&command)
                        {
                            marked.lock().unwrap().insert(id);
                        }
                        if to_chrome.send(message).await.is_err() {
                            return;
                        }
                    }
                    let _ = to_chrome.close().await;
                });
                while let Some(Ok(mut message)) = from_chrome.next().await {
                    if let Message::Text(text) = &message
                        && let Ok(reply) = serde_json::from_str::<Value>(text)
                        && let Some(id) = reply["id"].as_u64()
                        && failed.lock().unwrap().remove(&id)
                    {
                        let mut error = json!({"id": id, "error": {"code": PROXY_ERROR, "message": "lost"}});
                        if let Some(session) = reply.get("sessionId") {
                            error["sessionId"] = session.clone();
                        }
                        message = Message::Text(error.to_string().into());
                    }
                    if to_client.send(message).await.is_err() {
                        return;
                    }
                }
                let _ = to_client.close().await;
            });
        }
    });
    format!("ws://127.0.0.1:{port}/devtools/browser/proxy")
}
