mod common;

use std::sync::{Arc, Mutex};

use common::{RecordingArtifactSink, chrome, harness, sites, turn};
use fastbrowse_browser::error::BrowserError;
use fastbrowse_browser::session::{BrowserSession, SessionOptions};
use fastbrowse_core::models::BrowserConnection;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;

#[derive(Default)]
struct DisableGate {
    started: Notify,
    release: Notify,
    paused: Notify,
}

// Socket shutdown is queued, so hold that transport step while checking what close has already released in Chrome.
async fn held_disconnect(
    upstream: String,
    gate: Option<Arc<DisableGate>>,
    fail_setup: bool,
) -> (String, tokio::task::JoinHandle<()>, Arc<Mutex<Vec<Value>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let events = Arc::new(Mutex::new(Vec::new()));
    let recorded = events.clone();
    let commands = events.clone();
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept");
        let client = tokio_tungstenite::accept_async(socket).await.expect("client websocket");
        let (browser, _) = tokio_tungstenite::connect_async(&upstream).await.expect("Chrome websocket");
        let (mut to_client, mut from_client) = client.split();
        let (mut to_browser, mut from_browser) = browser.split();
        let disable = gate.clone();
        let failed = Arc::new(Mutex::new(None));
        let failure = failed.clone();
        let outgoing = async move {
            while let Some(Ok(message)) = from_client.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
                if let Message::Text(text) = &message {
                    let command: Value = serde_json::from_str(text).expect("command");
                    if command["method"] == "Network.setBypassServiceWorker" || command["method"] == "Fetch.disable" {
                        commands.lock().unwrap().push(command);
                    }
                }
                if fail_setup && let Message::Text(text) = &message {
                    let command: Value = serde_json::from_str(text).expect("command");
                    if command["method"] == "Runtime.enable" {
                        *failure.lock().unwrap() = Some(command["id"].clone());
                    }
                }
                if let (Some(gate), Message::Text(text)) = (&disable, &message) {
                    let command: Value = serde_json::from_str(text).expect("command");
                    if command["method"] == "Fetch.disable" {
                        gate.started.notify_one();
                        gate.release.notified().await;
                    }
                }
                to_browser.send(message).await.expect("forward command");
            }
            // Keep the writer alive until the test aborts this task, without forwarding the close frame.
            std::future::pending::<()>().await;
            drop(to_browser);
        };
        let incoming = async move {
            while let Some(Ok(mut message)) = from_browser.next().await {
                if let Message::Text(text) = &message {
                    let mut event: Value = serde_json::from_str(text).expect("CDP message");
                    if event.get("id").is_some() && event.get("id") == failed.lock().unwrap().as_ref() {
                        event.as_object_mut().expect("reply").remove("result");
                        event["error"] = json!({"code": -31999, "message": "setup failed"});
                        message = Message::text(event.to_string());
                    }
                    if event["method"] == "Fetch.requestPaused"
                        && let Some(gate) = &gate
                    {
                        gate.paused.notify_one();
                    }
                    recorded.lock().unwrap().push(event);
                }
                let _ = to_client.send(message).await;
            }
        };
        tokio::join!(outgoing, incoming);
    });
    (format!("ws://{address}"), task, events)
}

#[tokio::test(flavor = "multi_thread")]
async fn attached_close_releases_the_page_before_socket_disconnect() {
    let _turn = turn().await;
    let Some(connection) = chrome() else { return };
    let Some(owner) = harness().await else { return };
    owner.page.navigate(&format!("{}/", sites().main), None).await.expect("owner navigate");
    let target = owner.session.active_target_id();
    let (url, proxy, events) = held_disconnect(connection.cdp_url.clone(), None, false).await;
    let session = attached(connection, url).await.expect("attach");
    assert_eq!(session.active_target_id(), target);
    let children = session.frame_sessions();
    assert!(!children.is_empty(), "the attached page has an existing out-of-process iframe");
    session.close().await.expect("close");
    for (_, child) in children {
        assert!(
            events.lock().unwrap().iter().any(|event| {
                event["method"] == "Target.detachedFromTarget" && event["params"]["sessionId"] == child
            }),
            "close did not detach iframe session {child}"
        );
    }
    let start = events.lock().unwrap().len();
    let active = owner.session.active_session_id().expect("owner session");
    let navigated = owner.page.navigate(&format!("{}/signin.html", sites().main), None).await;
    let popup = owner
        .session
        .send(
            "Runtime.evaluate",
            json!({"expression": "window.open('about:blank'); void 0", "userGesture": true}),
            Some(&active),
        )
        .await;
    fastbrowse_browser::testing::settle_background(&owner.session).await;
    let paused: Vec<Value> = events.lock().unwrap()[start..]
        .iter()
        .filter(|event| event["method"] == "Fetch.requestPaused" || event["method"] == "Target.attachedToTarget")
        .cloned()
        .collect();
    proxy.abort();
    let _ = proxy.await;
    owner.close().await;
    assert!(navigated.is_ok(), "owner navigation after close: {navigated:?}; late events: {paused:?}");
    assert!(popup.is_ok(), "owner popup after close: {popup:?}; late events: {paused:?}");
    assert!(paused.is_empty(), "closed session still intercepted the page: {paused:?}");
}

async fn attached(connection: &BrowserConnection, url: String) -> Result<BrowserSession, BrowserError> {
    let session = BrowserSession::new(
        BrowserConnection {
            cdp_url: url,
            attach: true,
            target_match: Some(sites().main.clone()),
            allowed_origins: Some(vec![sites().main.clone()]),
            ..connection.clone()
        },
        Arc::new(RecordingArtifactSink::default()),
        SessionOptions::default(),
    );
    session.open().await?;
    Ok(session)
}

#[tokio::test(flavor = "multi_thread")]
async fn attached_close_releases_a_request_arriving_during_shutdown() {
    let _turn = turn().await;
    let Some(connection) = chrome() else { return };
    let Some(owner) = harness().await else { return };
    owner.page.navigate(&format!("{}/signin.html", sites().main), None).await.expect("owner navigate");
    let gate = Arc::new(DisableGate::default());
    let (url, proxy, _) = held_disconnect(connection.cdp_url.clone(), Some(gate.clone()), false).await;
    let session = attached(connection, url).await.expect("attach");
    let closing = session.close();
    tokio::pin!(closing);
    tokio::select! {
        _ = gate.started.notified() => {},
        result = &mut closing => panic!("close returned without releasing interception: {result:?}"),
    }
    let active = owner.session.active_session_id().expect("owner session");
    let owner_session = owner.session.clone();
    let navigation =
        owner_session.send("Page.navigate", json!({"url": format!("{}/release.html", sites().main)}), Some(&active));
    tokio::pin!(navigation);
    // The close future has stopped the handlers but has not yet delivered Fetch.disable to Chrome.
    tokio::select! {
        _ = gate.paused.notified() => {},
        result = &mut navigation => panic!("navigation was not intercepted: {result:?}"),
    }
    gate.release.notify_one();
    let (closed, navigated) = tokio::join!(closing, navigation);
    proxy.abort();
    let _ = proxy.await;
    owner.close().await;
    closed.expect("close");
    let reply = navigated.expect("disable releases the late request");
    assert!(reply.get("errorText").is_none(), "{reply:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_attach_releases_interception_before_socket_disconnect() {
    let _turn = turn().await;
    let Some(connection) = chrome() else { return };
    let Some(owner) = harness().await else { return };
    owner.page.navigate(&format!("{}/signin.html", sites().main), None).await.expect("owner navigate");
    let (url, proxy, events) = held_disconnect(connection.cdp_url.clone(), None, true).await;
    let attached = attached(connection, url).await;
    let navigated = owner.page.navigate(&format!("{}/release.html", sites().main), None).await;
    proxy.abort();
    let _ = proxy.await;
    owner.close().await;
    assert!(attached.is_err(), "setup must fail after Fetch is enabled");
    navigated.expect("failed attach releases the owner's page");
    let events = events.lock().unwrap();
    let bypass: Vec<_> = events.iter().filter(|event| event["method"] == "Network.setBypassServiceWorker").collect();
    assert_eq!(bypass.len(), 2, "failed attachment must restore service workers: {bypass:?}");
    assert_eq!(bypass[0]["params"]["bypass"], true);
    assert_eq!(bypass[1]["params"]["bypass"], false);
    assert_eq!(bypass[0]["sessionId"], bypass[1]["sessionId"]);
    let restored = events.iter().position(|event| *event == *bypass[1]).expect("restored");
    let disabled = events.iter().position(|event| event["method"] == "Fetch.disable").expect("Fetch disabled");
    assert!(restored < disabled, "service workers restored before interception is disabled");
}
