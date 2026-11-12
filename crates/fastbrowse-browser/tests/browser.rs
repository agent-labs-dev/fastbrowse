mod common;

// End-to-end tests of the browser layer against a real headless Chrome and fixture sites.
//
// Where the Python tests replace a session's handler or method (`_prepare_session`, `_on_target_created`,
// `send_raw`, `_before_action`), these put a websocket proxy between the session and Chrome that holds back,
// rewrites, drops or records the CDP messages that code would have seen, so the library runs unchanged.

use std::collections::HashSet;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{
    Harness, RecordingArtifactSink, chrome, eval_value, find, harness, harness_on, observe_until, sites, turn,
};
use data_encoding::HEXLOWER;
use fastbrowse_browser::adapters::local_chrome::{LocalChromeProcess, find_chrome};
use fastbrowse_browser::error::BrowserError;
use fastbrowse_browser::page::CdpPage;
use fastbrowse_browser::session::{BrowserSession, SessionOptions, str_of};
use fastbrowse_browser::testing;
use fastbrowse_core::config::Config;
use fastbrowse_core::models::{Attachment, BrowserConnection, Bytes, LocalChrome, Operation, StepOutcome};
use fastbrowse_core::page::{ActResult, Action, BlockKind, Control, Observation};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep, timeout};

// -- helpers ----------------------------------------------------------------------------------------------------

#[track_caller]
fn wait_until<F, Fut>(predicate: F) -> impl Future<Output = ()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    common::wait_until(predicate, 5.0)
}

async fn eval_active(h: &Harness, expression: &str) -> Value {
    eval_value(&h.session, &active(h), expression).await
}

fn active(h: &Harness) -> String {
    h.session.active_session_id().expect("active session")
}

/// Python's `bool(value)` for what an evaluation returned.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn sha256_hex(content: &[u8]) -> String {
    HEXLOWER.encode(ring::digest::digest(&ring::digest::SHA256, content).as_ref())
}

fn click(target: &Control) -> Action {
    Action { target_id: Some(target.id.clone()), ..Action::new(Operation::Click) }
}

fn hover(target: &Control) -> Action {
    Action { target_id: Some(target.id.clone()), ..Action::new(Operation::Hover) }
}

fn fill(target: &Control, text: &str) -> Action {
    Action { target_id: Some(target.id.clone()), text: Some(text.to_owned()), ..Action::new(Operation::Fill) }
}

async fn act(page: &CdpPage, action: Action, observation: &Observation) -> ActResult {
    page.act(&action, observation).await.expect("act")
}

/// The `loaded_page` fixture: a fresh session on the main site's index.
async fn loaded() -> Option<Harness> {
    let h = harness().await?;
    h.goto("/").await;
    Some(h)
}

// -- a CDP proxy ------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Toward {
    Browser,
    Client,
}

/// What to do with one CDP message: `None` drops it, else it is sent on after the delay.
type Hook = Arc<dyn Fn(Toward, Value) -> Option<(Value, Duration)> + Send + Sync>;

mod proxy {
    use futures_util::{SinkExt, Stream, StreamExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
    use tokio_tungstenite::tungstenite::{Error as WsError, Message};

    use super::{Hook, Toward, Value};

    /// Listen for one client at a time and relay each to `upstream` through `hook`, returning the address to dial.
    pub async fn start(upstream: String, hook: Hook) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                tokio::spawn(pump(tcp, upstream.clone(), hook.clone()));
            }
        });
        format!("ws://127.0.0.1:{port}/devtools/browser/proxy")
    }

    async fn pump(tcp: TcpStream, upstream: String, hook: Hook) {
        let Ok(client) = tokio_tungstenite::accept_async(tcp).await else { return };
        let Ok((browser, _)) = tokio_tungstenite::connect_async(upstream.as_str()).await else { return };
        let (mut client_sink, client_stream) = client.split();
        let (mut browser_sink, browser_stream) = browser.split();
        let (to_client, mut client_out) = unbounded_channel::<Message>();
        let (to_browser, mut browser_out) = unbounded_channel::<Message>();
        tokio::spawn(async move {
            while let Some(message) = client_out.recv().await {
                if client_sink.send(message).await.is_err() {
                    return;
                }
            }
        });
        tokio::spawn(async move {
            while let Some(message) = browser_out.recv().await {
                if browser_sink.send(message).await.is_err() {
                    return;
                }
            }
        });
        tokio::spawn(relay(client_stream, to_browser.clone(), Toward::Browser, hook.clone(), to_client.clone()));
        relay(browser_stream, to_client.clone(), Toward::Client, hook, to_browser).await;
    }

    async fn relay(
        mut from: impl Stream<Item = Result<Message, WsError>> + Unpin,
        to: UnboundedSender<Message>,
        toward: Toward,
        hook: Hook,
        back: UnboundedSender<Message>,
    ) {
        while let Some(Ok(message)) = from.next().await {
            let text = match message {
                Message::Text(text) => text,
                // Each side answers its own keepalive.
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) | Message::Binary(_) => continue,
                Message::Close(_) => break,
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
            match hook(toward, value) {
                None => {}
                Some((value, delay)) => {
                    // A hook that turns a command into a reply answers it itself, and Chrome never hears of it.
                    let answered = toward == Toward::Browser && value.get("method").is_none();
                    let to = if answered { back.clone() } else { to.clone() };
                    if delay.is_zero() {
                        let _ = to.send(Message::text(value.to_string()));
                        continue;
                    }
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = to.send(Message::text(value.to_string()));
                    });
                }
            }
        }
        let _ = to.send(Message::Close(None));
        let _ = back.send(Message::Close(None));
    }
}

/// A session and page on the shared Chrome, reached through a proxy running `hook`.
async fn proxied(connection: BrowserConnection, hook: Hook) -> Option<Harness> {
    let url = proxy::start(connection.cdp_url.clone(), hook).await;
    harness_on(BrowserConnection { cdp_url: url, ..connection }, Config::default(), SessionOptions::default()).await
}

fn method(value: &Value) -> &str {
    value["method"].as_str().unwrap_or_default()
}

/// `_announce_late`: hold back Chrome's announcement of new targets, once armed, as a loaded machine did for over two
/// seconds. With `quiet_windows` also armed, the session never hears a window open, as when its handler is replaced.
fn announce_late(late: Arc<AtomicBool>, quiet_windows: Arc<AtomicBool>, seconds: f64) -> Hook {
    Arc::new(move |toward, value| {
        if toward == Toward::Client && late.load(Ordering::SeqCst) {
            if matches!(method(&value), "Target.targetCreated" | "Target.targetInfoChanged") {
                return Some((value, Duration::from_secs_f64(seconds)));
            }
            if method(&value) == "Page.windowOpen" && quiet_windows.load(Ordering::SeqCst) {
                return None;
            }
        }
        Some((value, Duration::ZERO))
    })
}

/// Every command the session sends, with its params.
fn recording(sent: Arc<Mutex<Vec<(String, Value)>>>) -> Hook {
    Arc::new(move |toward, value| {
        if toward == Toward::Browser {
            sent.lock().unwrap().push((method(&value).to_owned(), value["params"].clone()));
        }
        Some((value, Duration::ZERO))
    })
}

// -- tests ------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_scroll_can_return_to_content_above_the_viewport() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/long-list.html").await;
    let session_id = active(&h);
    let down = act(&h.page, Action::new(Operation::Scroll), &h.observe().await).await;
    assert_eq!(down.outcome, StepOutcome::Executed);
    let (s, sid) = (&h.session, session_id.as_str());
    let scroll_y = || async move { eval_value(s, sid, "window.scrollY").await.as_f64().unwrap_or_default() };
    wait_until(|| async { scroll_y().await > 200.0 }).await;
    let before = scroll_y().await;
    let up = act(&h.page, Action::new(Operation::ScrollUp), &h.observe().await).await;
    assert_eq!(up.outcome, StepOutcome::Executed);
    wait_until(|| async { scroll_y().await < before - 200.0 }).await;
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_scroll_lands_inside_a_small_window() {
    let Some(h) = harness().await else { return };
    h.goto("/long-list.html").await;
    let session_id = active(&h);
    let small = json!({"width": 400, "height": 300, "deviceScaleFactor": 1, "mobile": false});
    h.session.send("Emulation.setDeviceMetricsOverride", small, Some(&session_id)).await.expect("resize");
    let down = act(&h.page, Action::new(Operation::Scroll), &h.observe().await).await;
    assert_eq!(down.outcome, StepOutcome::Executed);
    let (s, sid) = (&h.session, session_id.as_str());
    wait_until(|| async { eval_value(s, sid, "window.scrollY").await.as_f64().unwrap_or_default() > 200.0 }).await;
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_observe_lists_controls_and_masks_password() {
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    let obs = h.observe().await;
    let sign_in = find(&obs, "Sign in");
    assert!(sign_in.operations.contains(&Operation::Click));

    let password = find(&obs, "Password");
    assert!(password.sensitive);
    let filled = act(&h.page, fill(password, "s3cr3t"), &obs).await;
    assert_eq!(filled.outcome, StepOutcome::Executed);

    let obs2 = h.observe().await;
    let password2 = find(&obs2, "Password");
    assert_eq!(password2.value.as_deref(), Some("\u{2022}".repeat("s3cr3t".len()).as_str()));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_secret_typed_into_text_field_is_masked_but_preserves_value() {
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    let obs = h.observe().await;
    let username = find(&obs, "Username");
    let action = Action { secret: true, secret_origin: username.frame_origin.clone(), ..fill(username, "tok-123") };
    assert_eq!(act(&h.page, action, &obs).await.outcome, StepOutcome::Executed);

    let after = h.observe().await;
    let field = find(&after, "Username");
    assert!(field.sensitive && field.value.as_deref() == Some("\u{2022}".repeat("tok-123".len()).as_str()));
    assert_eq!(eval_active(&h, "document.getElementById('username').value").await, json!("tok-123"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fill_and_click_submits_form_and_page_changed() {
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    let obs = h.observe().await;
    let username = find(&obs, "Username");
    let result = act(&h.page, fill(username, "alice"), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Executed);

    let obs2 = h.observe().await;
    let submit = find(&obs2, "Sign in");
    let result2 = act(&h.page, click(submit), &obs2).await;
    assert_eq!(result2.outcome, StepOutcome::Executed);
    assert!(result2.page_changed);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_click_the_page_answers_over_fetch_is_judged_by_the_response() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    for (label, changed) in [("Add to cart", true), ("Track", false)] {
        h.goto("/quick-add.html").await;
        let obs = h.observe().await;
        let result = act(&h.page, click(find(&obs, label)), &obs).await;
        assert_eq!((result.outcome, result.page_changed), (StepOutcome::Executed, changed), "{label}");
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_navigation_that_starts_during_the_settle_wait_is_a_change() {
    // #516: the page reads quiet and unchanged while the action's request is open, then the response starts a
    // navigation, which destroys every later read's execution context until the deadline. That is a change, not an
    // action that left the page as it was.
    let _turn = turn().await;
    let Some(connection) = chrome().cloned() else { return };
    let armed = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));
    let destroyed = Arc::new(Mutex::new(Vec::<i64>::new()));
    let (arm, count, ids) = (armed.clone(), reads.clone(), destroyed.clone());
    let hook: Hook = Arc::new(move |toward, value| {
        if !arm.load(Ordering::SeqCst) {
            return Some((value, Duration::ZERO));
        }
        let settle_read = method(&value) == "Runtime.evaluate"
            && value["params"]["expression"].as_str().is_some_and(|e| e.contains("fastbrowse:settle"));
        if toward == Toward::Browser && settle_read && count.fetch_add(1, Ordering::SeqCst) > 0 {
            ids.lock().unwrap().extend(value["id"].as_i64());
        }
        if toward == Toward::Client && value["id"].as_i64().is_some_and(|id| ids.lock().unwrap().contains(&id)) {
            let mut failed =
                json!({"id": value["id"], "error": {"code": -32000, "message": "Execution context was destroyed."}});
            if let Some(session) = value.get("sessionId") {
                failed["sessionId"] = session.clone();
            }
            return Some((failed, Duration::ZERO));
        }
        Some((value, Duration::ZERO))
    });
    let Some(h) = proxied(connection, hook).await else { return };
    h.goto("/").await;
    let before = h.page.fingerprint().await.expect("fingerprint");
    let requests = h.session.requests_mark();
    // The action's request outlasts the settle; a later, quicker one wakes the settle to read the page again.
    eval_active(&h, "fetch('/slow?ms=8000'); setTimeout(() => fetch('/slow?ms=100'), 1000); 0").await;
    wait_until(|| async { h.session.requests_open_since(requests) }).await;
    armed.store(true, Ordering::SeqCst);
    let changed = h.page.changed_since(&before, Some(requests)).await.expect("settled");
    assert!(reads.load(Ordering::SeqCst) >= 2, "the settle never read the page again");
    assert!(changed);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_covered_click_dispatches_nothing() {
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    let obs = h.observe().await;
    let opened = act(&h.page, click(find(&obs, "Open modal")), &obs).await;
    assert_eq!(opened.outcome, StepOutcome::Executed);

    let obs2 = h.observe().await;
    let result = act(&h.page, click(find(&obs2, "Covered target")), &obs2).await;
    assert_eq!(result.outcome, StepOutcome::Covered);
    assert!(!result.page_changed);

    let clicked = eval_active(&h, "document.getElementById('covered-target').dataset.clicked || null").await;
    assert_eq!(clicked, Value::Null);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_visually_hidden_radio_is_pressed_through_its_label() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/swatches.html").await;
    let obs = h.observe().await;
    let result = act(&h.page, click(find(&obs, "Blue")), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Executed, "{:?}", result.detail);
    assert_eq!(eval_active(&h, "document.getElementById('blue').checked").await, json!(true));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_small_fixed_child_does_not_hide_full_window_cover_from_hit_test() {
    let _turn = turn().await;
    for shadow in [false, true] {
        let Some(h) = loaded().await else { return };
        let session_id = active(&h);
        let html = if shadow {
            concat!(
                "(()=>{document.body.innerHTML = '<button id=target>Covered target</button><x-cover></x-cover>'; ",
                "const host = document.querySelector('x-cover'); ",
                "host.style.cssText = 'display:block;position:fixed;inset:0;z-index:9999;transform:translateZ(0)'; ",
                "const root = host.attachShadow({mode:'open'}); ",
                "root.innerHTML = '<div class=child style=\"position:fixed;background:#000\"></div>'; ",
                "const r = document.getElementById('target').getBoundingClientRect(); ",
                "const child = root.querySelector('.child'); child.style.left = r.left+'px'; ",
                "child.style.top = r.top+'px'; child.style.width = r.width+'px'; ",
                "child.style.height = r.height+'px'; return true})()",
            )
        } else {
            concat!(
                "document.body.innerHTML = '<button id=target>Covered target</button>' + ",
                "'<div style=\"position:fixed;inset:0;z-index:9999;transform:translateZ(0)\">' + ",
                "'<div style=\"position:fixed;left:0;top:0;width:128px;height:128px;background:#000\"></div>' + ",
                "'</div>'; true",
            )
        };
        eval_value(&h.session, &session_id, html).await;
        let obs = h.observe().await;
        let target = find(&obs, "Covered target");
        let result = act(&h.page, click(target), &obs).await;
        assert_eq!(result.outcome, StepOutcome::Covered, "shadow={shadow}");
        let clicked =
            eval_value(&h.session, &session_id, "document.getElementById('target').dataset.clicked || null").await;
        assert_eq!(clicked, Value::Null, "shadow={shadow}");
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_dom_swap_after_observe_makes_act_stale() {
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    let obs = h.observe().await;
    let target = find(&obs, "Swap target");

    eval_active(&h, "document.getElementById('swap-trigger').click(); true").await;

    let result = act(&h.page, click(target), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Stale);
    assert!(!result.page_changed);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_upload_bytes_match_sha256() {
    let _turn = turn().await;
    use ring::rand::SecureRandom;

    let Some(h) = loaded().await else { return };
    let obs = h.observe().await;
    let file_input = obs.controls.iter().find(|c| c.input_type.as_deref() == Some("file")).expect("file input");
    let mut content = vec![0; 4096];
    ring::rand::SystemRandom::new().fill(&mut content).expect("random bytes");
    let attachment = Attachment {
        name: "payload.bin".into(),
        mime_type: "application/octet-stream".into(),
        content: Bytes(content.clone()),
    };

    let action =
        Action { target_id: Some(file_input.id.clone()), files: vec![attachment], ..Action::new(Operation::Upload) };
    let result = act(&h.page, action, &obs).await;
    assert_eq!(result.outcome, StepOutcome::Executed);

    let sha = "document.getElementById('upload-sha').textContent";
    let hr = &h;
    wait_until(move || async move { truthy(&eval_active(hr, sha).await) }).await;
    assert_eq!(eval_active(&h, sha).await, json!(sha256_hex(&content)));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_download_becomes_artifact_with_checksum() {
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    let obs = h.observe().await;
    let result = act(&h.page, click(find(&obs, "Download attachment")), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Executed);

    let sink = &h.sink;
    wait_until(move || async move { !sink.artifacts.lock().unwrap().is_empty() }).await;
    let artifact = h.sink.artifacts.lock().unwrap()[0].clone();
    let expected = sha256_hex(b"fastbrowse fixture attachment bytes for download checksum test");
    assert_eq!(artifact.sha256, expected);
    assert_eq!(h.session.artifacts(), vec![artifact]);
    h.close().await;
}

/// The orders-report shape: a link whose target is a file, which Chrome downloads instead of navigating. The tab keeps
/// its page, so the click must count the download as its effect, and the next observation must carry its rows.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_click_that_downloads_is_a_page_change_not_a_dead_click() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h,
        "document.body.innerHTML = \"<p>Download the <a href='/report.csv?short'>Orders report</a>.</p>\";",
    )
    .await;
    let obs = observe_until(&h.page, "Orders report").await;
    let result = act(&h.page, click(find(&obs, "Orders report")), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Executed);
    assert!(result.page_changed, "a click that downloaded a file moved the run, not the page");
    let glance = h.page.glance().await.expect("glance after download");
    assert!(!glance.viewport_text.contains("139.79"), "a frame check leaves the download notice pending");
    let landed = h.observe().await;
    assert!(landed.viewport_text.contains("139.79"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_saving_the_same_download_again_is_not_new_progress() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(&h, "document.body.innerHTML = \"<a href='/report.csv?short'>Orders report</a>\";").await;
    let before = observe_until(&h.page, "Orders report").await;
    let first = act(&h.page, click(find(&before, "Orders report")), &before).await;
    let after = h.observe().await;
    let second = act(&h.page, click(find(&after, "Orders report")), &after).await;
    h.close().await;
    assert_eq!(first.outcome, StepOutcome::Executed);
    assert!(first.page_changed, "the first file supplies new evidence");
    assert_eq!(second.outcome, StepOutcome::Executed);
    assert!(!second.page_changed, "saving an identical file again must not renew progress");
    assert_eq!(second.detail.as_deref(), Some("downloaded the same file an earlier step already saved"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cross_origin_iframe_control_is_observed_and_clickable() {
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    let obs = observe_until(&h.page, "Frame button").await;
    let frame_button = find(&obs, "Frame button");
    let frame_id = frame_button.frame_id.clone().expect("a frame's control names its frame");

    let result = act(&h.page, click(frame_button), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Executed);

    let frame_session_id =
        h.session.frame_sessions().into_iter().find(|(f, _)| *f == frame_id).map(|(_, s)| s).expect("frame session");
    let value = eval_value(&h.session, &frame_session_id, "document.getElementById('frame-button').textContent").await;
    assert_eq!(value, json!("Clicked"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_click_that_opens_a_tab_continues_there() {
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    let obs = h.observe().await;
    let opener_tab = obs.tabs.iter().find(|t| t.active).expect("active tab").clone();
    let result = act(&h.page, click(find(&obs, "Open popup")), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Executed);
    assert!(result.page_changed);

    let s = &h.session;
    wait_until(move || async move { s.tabs().iter().any(|t| t.url.contains("popup.html")) }).await;
    let obs2 = h.observe().await;
    assert!(obs2.url.contains("popup.html"));
    find(&obs2, "Popup button");

    let switch = Action { tab_id: Some(opener_tab.id.clone()), ..Action::new(Operation::SwitchTab) };
    let back = act(&h.page, switch, &obs2).await;
    assert_eq!(back.outcome, StepOutcome::Executed);
    find(&h.observe().await, "Open popup");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_viewport_is_applied_to_popup_tabs() {
    let _turn = turn().await;
    let Some(connection) = chrome().cloned() else { return };
    let Some(h) = common::harness_on(
        connection,
        Config::default(),
        SessionOptions { viewport: Some((640, 480)), ..Default::default() },
    )
    .await
    else {
        return;
    };
    h.goto("/").await;
    let obs = h.observe().await;
    let result = act(&h.page, click(find(&obs, "Open popup")), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Executed);
    let s = &h.session;
    wait_until(move || async move { s.tabs().iter().any(|t| t.url.contains("popup.html")) }).await;
    assert_eq!(
        eval_value(&h.session, &h.session.active_session_id().unwrap(), "innerWidth + 'x' + innerHeight").await,
        json!("640x480")
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_new_tab_slow_to_set_up_is_still_followed() {
    let _turn = turn().await;
    let Some(connection) = chrome().cloned() else { return };
    let slow = Arc::new(AtomicBool::new(false));
    let armed = slow.clone();
    // `Runtime.enable` is sent by `prepare_session` alone, so holding it back makes every later setup slow.
    let hook: Hook = Arc::new(move |toward, value| {
        if toward == Toward::Browser && armed.load(Ordering::SeqCst) && method(&value) == "Runtime.enable" {
            // A loaded browser took seconds to answer a new tab's setup, past any fixed allowance for it.
            return Some((value, Duration::from_secs_f64(2.5)));
        }
        Some((value, Duration::ZERO))
    });
    let Some(h) = proxied(connection, hook).await else { return };
    h.goto("/").await;
    let obs = h.observe().await;
    slow.store(true, Ordering::SeqCst);
    let result = act(&h.page, click(find(&obs, "Open popup")), &obs).await;
    assert!(
        result.outcome == StepOutcome::Executed && result.detail.as_deref() == Some("opened a new tab, now active")
    );
    find(&h.observe().await, "Popup button");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_new_tab_announced_late_is_still_followed() {
    let _turn = turn().await;
    for opener in ["window.open", "link"] {
        let Some(connection) = chrome().cloned() else { return };
        let (late, quiet) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let Some(h) = proxied(connection, announce_late(late.clone(), quiet.clone(), 2.5)).await else { return };
        h.goto("/").await;
        eval_active(
            &h,
            "document.body.insertAdjacentHTML('beforeend', '<a href=\"/popup.html\" target=\"_blank\">New window \
             link</a>')",
        )
        .await;
        let obs = h.observe().await;
        late.store(true, Ordering::SeqCst);
        if opener == "link" {
            // The link names a new browsing context, which is enough to wait for its tab without Chrome saying so.
            quiet.store(true, Ordering::SeqCst);
        }
        let label = if opener == "window.open" { "Open popup" } else { "New window link" };
        let result = act(&h.page, click(find(&obs, label)), &obs).await;
        assert!(
            result.outcome == StepOutcome::Executed && result.detail.as_deref() == Some("opened a new tab, now active"),
            "{opener}: {result:?}"
        );
        find(&h.observe().await, "Popup button");
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_new_tab_whose_opener_is_reported_late_is_still_followed() {
    let _turn = turn().await;
    let Some(connection) = chrome().cloned() else { return };
    let armed = Arc::new(AtomicBool::new(false));
    let strip = armed.clone();
    // Chrome can name the opener only in a later update of the target's info.
    let hook: Hook = Arc::new(move |toward, mut value| {
        if toward == Toward::Client
            && strip.load(Ordering::SeqCst)
            && method(&value) == "Target.targetCreated"
            && let Some(info) = value["params"]["targetInfo"].as_object_mut()
        {
            info.remove("openerId");
        }
        Some((value, Duration::ZERO))
    });
    let Some(h) = proxied(connection, hook).await else { return };
    h.goto("/").await;
    let obs = h.observe().await;
    armed.store(true, Ordering::SeqCst);
    let result = act(&h.page, click(find(&obs, "Open popup")), &obs).await;
    assert!(
        result.outcome == StepOutcome::Executed && result.detail.as_deref() == Some("opened a new tab, now active")
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_click_that_opens_no_window_does_not_wait_for_one() {
    let _turn = turn().await;
    let Some(connection) = chrome().cloned() else { return };
    let late = Arc::new(AtomicBool::new(false));
    let Some(h) = proxied(connection, announce_late(late.clone(), Arc::default(), 2.5)).await else { return };
    h.goto("/").await;
    let obs = h.observe().await;
    late.store(true, Ordering::SeqCst);
    let started = Instant::now();
    let result = act(&h.page, click(find(&obs, "Shadow button")), &obs).await;
    assert!(result.outcome == StepOutcome::Executed && result.detail.is_none());
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_draw_wait_timeout_does_not_queue_a_second_page_evaluation() {
    let _turn = turn().await;
    let Some(connection) = chrome().cloned() else { return };
    let armed = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let (hold, counted) = (armed.clone(), calls.clone());
    // The measuring evaluation is held unanswered, as a renderer synchronously blocked past the draw wait leaves it.
    let hook: Hook = Arc::new(move |toward, value| {
        if toward == Toward::Browser
            && hold.load(Ordering::SeqCst)
            && method(&value) == "Runtime.evaluate"
            && value["params"]["expression"].as_str().is_some_and(|e| e.contains("const announced = "))
        {
            counted.fetch_add(1, Ordering::SeqCst);
            return None;
        }
        Some((value, Duration::ZERO))
    });
    let Some(h) = proxied(connection, hook).await else { return };
    h.goto("/").await;
    let obs = h.observe().await;
    armed.store(true, Ordering::SeqCst);
    let error = h.page.act(&click(find(&obs, "Shadow button")), &obs).await.expect_err("the page did not draw");
    assert!(matches!(&error, BrowserError::PageUnresponsive(m) if m.contains("did not draw")), "{error:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_tab_opened_from_a_cross_site_frame_is_followed() {
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    // A frame session attaches before its document loads, so wait for the frame's own control first.
    observe_until(&h.page, "Frame button").await;
    let frame_session = h.session.frame_sessions()[0].1.clone();
    let main = &sites().main;
    let inserted = eval_value(
        &h.session,
        &frame_session,
        &format!(
            "document.body.insertAdjacentHTML('beforeend', '<a href=\"{main}/popup.html\" target=\"_blank\">Frame \
             window link</a>'); true"
        ),
    )
    .await;
    assert_eq!(inserted, json!(true), "the link was not inserted into the frame");
    let obs = observe_until(&h.page, "Frame window link").await;
    let result = act(&h.page, click(find(&obs, "Frame window link")), &obs).await;
    assert!(
        result.outcome == StepOutcome::Executed && result.detail.as_deref() == Some("opened a new tab, now active")
    );
    find(&h.observe().await, "Popup button");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_confirm_dialog_handled_via_dialog_operation() {
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    let obs = h.observe().await;
    let result = act(&h.page, click(find(&obs, "Confirm")), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Executed);

    let s = &h.session;
    wait_until(move || async move { s.pending_dialog().is_some() }).await;

    let obs2 = h.observe().await;
    let dialog = obs2.dialog.as_ref().expect("dialog");
    assert_eq!(dialog.kind, "confirm");

    let accept = Action { accept_dialog: Some(true), ..Action::new(Operation::Dialog) };
    let handled = act(&h.page, accept, &obs2).await;
    assert_eq!(handled.outcome, StepOutcome::Executed);

    let value = eval_active(&h, "document.getElementById('dialog-result').textContent").await;
    assert_eq!(value, json!("confirmed"));
    h.close().await;
}

/// Deleting a draft opened a confirm dialog, and the read that followed evaluated the blocked page until the command
/// deadline, so the run ended unavailable before it could answer the dialog.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_capture_under_a_dialog_returns_without_reading_the_blocked_page() {
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    let obs = h.observe().await;
    let result = act(&h.page, click(find(&obs, "Confirm")), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Executed);
    let s = &h.session;
    wait_until(move || async move { s.pending_dialog().is_some() }).await;
    let capture = tokio::time::timeout(std::time::Duration::from_secs(5), h.page.capture())
        .await
        .expect("the capture did not wait on the blocked page")
        .expect("a capture");
    assert!(capture.text.is_empty(), "{:?}", capture.text);
    assert!(h.session.pending_dialog().is_some());
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_unnamed_card_controls_are_named_by_their_icon_and_card() {
    let _turn = turn().await;
    // A gallery's icon-only trash controls read "button" beside an edit link, so the choice picked the edit link or
    // refused the trash control it could not tie to an image.
    let Some(h) = harness().await else { return };
    h.goto("/gallery.html").await;
    let obs = h.observe().await;
    let buttons: Vec<(&str, Option<&str>)> =
        obs.controls.iter().filter(|c| c.role == "button").map(|c| (c.label.as_str(), c.context.as_deref())).collect();
    assert_eq!(
        buttons,
        vec![
            ("trash icon", Some("old-banner.png")),
            ("trash icon", Some("new-banner.png")),
            ("trash can icon", Some("old-logo.png")),
            ("Delete image", None),
            ("delete icon", Some("old-icon.png")),
            ("Remove from gallery", Some("new-icon.png")),
            // The card's action row has its own caption, which every card repeats and which names no image.
            ("x circle icon", Some("Alpha")),
        ]
    );
    let trash = obs.controls.iter().find(|c| c.label == "trash can icon").expect("trash can");
    let result = act(&h.page, click(trash), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Executed);
    let s = &h.session;
    wait_until(move || async move { s.pending_dialog().is_some() }).await;
    let confirm = h.observe().await;
    assert!(confirm.dialog.as_ref().is_some_and(|d| d.message == "Delete old-logo.png?"));
    let accept = Action { accept_dialog: Some(true), ..Action::new(Operation::Dialog) };
    let handled = timeout(Duration::from_secs(10), act(&h.page, accept, &confirm)).await.expect("in time");
    assert_eq!(handled.outcome, StepOutcome::Executed);
    let status = "document.getElementById('status').textContent";
    assert_eq!(eval_active(&h, status).await, json!("Deleted c3"));
    let after = h.observe().await;
    assert!(after.controls.iter().filter(|c| c.label == "trash can icon").map(|c| &c.context).next().is_none());
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_identical_labels_carry_their_card_as_context() {
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    let obs = h.observe().await;
    let adds: Vec<Option<&str>> =
        obs.controls.iter().filter(|c| c.label == "Add to cart").map(|c| c.context.as_deref()).collect();
    assert_eq!(adds, vec![Some("Brass Kettle"), Some("Copper Pan")]);
    // The price sits in a wrapper closer to the button than the card is; the card is what tells them apart.
    assert_eq!(find(&obs, "Sign in").context, None);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lazily_built_menu_is_observed_after_the_click_that_opens_it() {
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    let obs = h.observe().await;
    let opener = find(&obs, "Search or jump to");
    assert_eq!(act(&h.page, click(opener), &obs).await.outcome, StepOutcome::Executed);
    // No re-observation loop here: the menu must be there on the next observation the agent takes.
    assert!(h.observe().await.controls.iter().any(|c| c.label == "Jump to a repository"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_shadow_dom_button_is_clickable() {
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    let obs = h.observe().await;
    let result = act(&h.page, click(find(&obs, "Shadow button")), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Executed);

    assert_eq!(eval_active(&h, "window.__shadowClicked === true").await, json!(true));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_record_keeps_a_value_it_only_draws() {
    let Some(h) = loaded().await else { return };
    let card = |rating: &str, title: &str| {
        format!(
            "<article class='card'><p class='rating {rating}'><i></i><i></i></p><h3>{title}</h3><p>Stock</p></article>"
        )
    };
    let html = [card("One", "First"), card("Five", "Second"), card("One", "Third")].concat();
    eval_active(&h, &format!("document.body.innerHTML = {}", json!(html))).await;
    let capture = h.page.capture().await.expect("capture");
    assert!(capture.text.contains("First") && capture.text.contains("[rating One: no text]"), "{}", capture.text);
    assert!(capture.text.contains("[rating Five: no text]"), "{}", capture.text);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_capture_offsets_slice_exactly_to_each_block() {
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    let capture = h.page.capture().await.expect("capture");
    assert!(!capture.blocks.is_empty(), "expected at least one block");
    for block in &capture.blocks {
        assert!(!capture.block_text(block).is_empty(), "block {} sliced to empty text", block.source_id);
    }
    let headings: Vec<_> = capture.blocks.iter().filter(|b| b.kind == BlockKind::Heading).collect();
    assert!(headings.iter().any(|h| capture.block_text(h) == "Table"));
    let tables: Vec<_> = capture.blocks.iter().filter(|b| b.kind == BlockKind::Table).collect();
    assert!(!tables.is_empty());
    assert!(capture.block_text(tables[0]).contains("Ada"));
    let mut links = capture.blocks.iter().filter(|b| b.kind == BlockKind::Link);
    assert!(links.any(|link| link.href.as_deref() == Some("https://example.com/docs")));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_link_to_another_site_keeps_its_whole_address() {
    // A recipe and a followed click join the href to the page, so host and path alone became a path on this site (#830).
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    let observation = h.observe().await;
    let hrefs: Vec<_> = observation.controls.iter().filter_map(|control| control.href.as_deref()).collect();
    assert!(hrefs.contains(&"https://example.com/docs"), "{hrefs:?}");
    h.close().await;
}

/// Wikipedia's search box swaps itself for a hydrated copy mid-insertion; the text still landed.
#[tokio::test(flavor = "multi_thread")]
async fn test_fill_succeeds_when_the_field_is_replaced_while_typing() {
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    let obs = h.observe().await;
    let result = act(&h.page, fill(find(&obs, "Hydrating field"), "godel"), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Executed);

    let obs2 = h.observe().await;
    assert_eq!(find(&obs2, "Hydrating field").value.as_deref(), Some("godel"));
    assert!(truthy(&eval_active(&h, "!document.getElementById('hydrating')").await));
    h.close().await;
}

/// The replacement must be the target's own: a decoy holding the same text does not vouch for it.
#[tokio::test(flavor = "multi_thread")]
async fn test_fill_fails_when_another_field_holds_the_text_we_could_not_insert() {
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    let obs = h.observe().await;
    let result = act(&h.page, fill(find(&obs, "Rejecting field"), "godel"), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Failed);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_an_observation_keeps_every_control_and_the_whole_viewport_text() {
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    eval_active(
        &h,
        "document.body.replaceChildren(); const text = document.createElement('div'); text.textContent = \
         'x'.repeat(8000); document.body.append(text); for (let i = 0; i < 400; i++) { const b = \
         document.createElement('button'); b.textContent = 'Button ' + i; b.style.cssText = \
         'position:absolute;left:20px;top:' + (10000+i*40) + 'px'; document.body.append(b); }",
    )
    .await;
    let observation = h.observe().await;
    assert_eq!(observation.controls.iter().filter(|c| c.role == "button").count(), 400);
    assert_eq!(observation.viewport_text, "x".repeat(8000));
    h.close().await;
}

/// A long list's rows below the viewport are not in its text, so the observation says how much a read would add.
#[tokio::test(flavor = "multi_thread")]
async fn test_an_observation_counts_the_page_text_outside_the_viewport() {
    let Some(h) = loaded().await else { return };
    eval_active(
        &h,
        "document.body.replaceChildren(); const first = document.createElement('p'); first.textContent = 'First row'; \
         const below = document.createElement('p'); below.textContent = 'Mongolia 3086918'; \
         below.style.cssText = 'position:absolute;top:20000px'; const hidden = document.createElement('p'); \
         hidden.textContent = 'never shown'; hidden.style.cssText = 'display:none'; document.body.append(first, below, \
         hidden); scrollTo(0, 0);",
    )
    .await;
    let observation = h.observe().await;
    assert_eq!(observation.viewport_text, "First row");
    assert_eq!(observation.offscreen_text, "Mongolia 3086918");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_page_exception_is_typed_and_does_not_echo_page_text() {
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    eval_active(&h, "Object.defineProperty(document, 'title', {get() { throw new Error('echoed-secret'); }});").await;
    let raised = h.page.observe().await.expect_err("the title getter throws");
    assert!(raised.to_string().contains("Runtime.evaluate failed (JavaScriptError)"), "{raised:?}");
    assert!(!raised.to_string().contains("echoed-secret"));
    h.close().await;
}

/// Zara's listing draws "VIEW 1 2" as focusable text whose click is a script listener: no role, no href, and no
/// pointer cursor, so the run could not reach page 2 and answered with page 1's five products (BU Bench bu2-082).
#[tokio::test(flavor = "multi_thread")]
async fn test_a_text_page_number_with_a_tabindex_is_offered_and_pressed() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(
        &h,
        "document.body.innerHTML = '<ul><li><span id=\\'p1\\' tabindex=\\'0\\'>1</span></li>' + \
         '<li><span id=\\'p2\\' tabindex=\\'0\\'>2</span></li></ul>'; \
         window.picked = ''; \
         document.getElementById('p2').addEventListener('click', () => { window.picked = '2'; });",
    )
    .await;
    let obs = h.observe().await;
    let two = obs.controls.iter().find(|c| c.label == "2").expect("page 2 is offered");
    assert!(two.operations.contains(&Operation::Click), "{:?}", two.operations);
    let target = Action { target_id: Some(two.id.clone()), ..Action::new(Operation::Click) };
    let result = h.page.act(&target, &obs).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Executed);
    assert_eq!(eval_active(&h, "window.picked").await, json!("2"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_focusable_menu_with_a_child_label_is_offered_and_opens() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/").await;
    eval_active(
        &h,
        r#"document.body.innerHTML = '<nav><ul><li id="menu" tabindex="0"><div>Projects</div><ul hidden><li><a href="/wind">Wind</a></li></ul></li></ul></nav>';
        document.getElementById('menu').addEventListener('click', event => {
            event.currentTarget.querySelector('ul').hidden = false;
        });"#,
    )
    .await;
    let obs = h.observe().await;
    let menu = obs.controls.iter().find(|control| control.label == "Projects").expect("menu label is offered");
    assert!(menu.operations.contains(&Operation::Click));
    assert!(!obs.controls.iter().any(|control| control.label == "Wind"));
    eval_active(&h, "document.getElementById('menu').setAttribute('aria-label', 'Project categories')").await;
    let named = h.observe().await;
    assert!(named.controls.iter().any(|control| control.label == "Project categories"));
    eval_active(&h, "document.getElementById('menu').removeAttribute('aria-label')").await;
    let fresh = h.observe().await;
    let menu = fresh.controls.iter().find(|control| control.label == "Projects").unwrap();
    let result = h.page.act(&click(menu), &fresh).await.unwrap();
    assert_eq!(result.outcome, StepOutcome::Executed);
    let after = h.observe().await;
    assert!(after.controls.iter().any(|control| control.label == "Wind"));
    assert!(!after.controls.iter().any(|control| control.label == "Projects"));
    h.close().await;
}

/// Unassociated sibling labels left todo toggles absent from the choices offered to the policy.
#[tokio::test(flavor = "multi_thread")]
async fn test_transparent_native_choices_keep_todo_context_and_change_only_state() {
    let _turn = turn().await;
    for kind in ["radio", "checkbox"] {
        let Some(h) = harness().await else { return };
        h.goto("/todos.html").await;
        eval_active(&h, &format!("document.querySelectorAll('input').forEach(e => e.type = '{kind}')")).await;
        let before = h.observe().await;
        let choices: Vec<&Control> = before.controls.iter().filter(|c| c.role == kind).collect();
        assert_eq!(
            choices.iter().map(|c| c.context.as_deref()).collect::<Vec<_>>(),
            vec![Some("buy oat milk"), Some("walk the dog"), Some("ship fastbrowse 0.5")],
            "{kind}"
        );
        assert!(choices.iter().all(|c| c.label == "Toggle Todo" && c.checked == Some(false)), "{kind}");
        let target = choices[1];
        let result = act(&h.page, click(target), &before).await;
        assert!(result.outcome == StepOutcome::Executed && result.page_changed, "{kind}");
        let after = h.observe().await;
        assert_eq!(
            after.controls.iter().filter(|c| c.role == kind).map(|c| c.checked).collect::<Vec<_>>(),
            vec![Some(false), Some(true), Some(false)],
            "{kind}"
        );
        assert_eq!(after.viewport_text, before.viewport_text, "{kind}");
        // Python also asserts `effect(before, after, target).set_something`; `effects.py` is the policy layer's and
        // is not ported, so that check stays with the Python suite.
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_transparent_choice_exception_does_not_admit_hidden_or_disabled_controls() {
    let _turn = turn().await;
    let changes = [
        "e.style.display = 'none'",
        "e.style.visibility = 'hidden'",
        "e.style.width = '0'",
        "e.parentElement.setAttribute('aria-hidden', 'true')",
        "e.parentElement.inert = true",
        "e.parentElement.style.opacity = '0'",
        "e.disabled = true",
        "e.parentElement.setAttribute('aria-disabled', 'true')",
        "e.type = 'text'",
    ];
    for change in changes {
        let Some(h) = harness().await else { return };
        h.goto("/todos.html").await;
        let before = h.observe().await;
        let target = before.controls.iter().find(|c| c.context.as_deref() == Some("walk the dog")).expect("toggle");
        eval_active(&h, &format!("const e = document.querySelectorAll('input')[1]; {change}")).await;
        let result = act(&h.page, click(target), &before).await;
        assert_eq!(result.outcome, StepOutcome::Stale, "{change}");
        assert!(!h.observe().await.controls.iter().any(|c| c.id == target.id), "{change}");
        assert!(!truthy(&eval_active(&h, "e.checked").await), "{change}");
        h.close().await;
    }
}

/// A `display: contents` parent has no layout box, yet hides nothing.
#[tokio::test(flavor = "multi_thread")]
async fn test_transparent_choice_under_a_boxless_parent_is_offered() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/todos.html").await;
    eval_active(&h, "document.querySelectorAll('input')[1].parentElement.style.display = 'contents'").await;
    assert!(h.observe().await.controls.iter().any(|c| c.context.as_deref() == Some("walk the dog")));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_styled_choice_uses_visible_label_and_native_state() {
    let _turn = turn().await;
    let styles = [
        "opacity:0",
        "display:none",
        "opacity:0;pointer-events:none",
        // TodoMVC's "Mark all as complete": only its label can be clicked.
        "opacity:0;width:1px;height:1px;position:absolute;right:100%",
    ];
    for style in styles {
        for kind in ["radio", "checkbox"] {
            let Some(h) = harness().await else { return };
            h.goto("").await;
            eval_active(
                &h,
                &format!(
                    "document.body.innerHTML = '<input id=choice type={kind} style=\"{style}\"><label \
                     for=choice>Direct service</label>'; true"
                ),
            )
            .await;
            let obs = h.observe().await;
            let target = find(&obs, "Direct service");
            assert!(target.role == kind && target.checked == Some(false), "{style}-{kind}");
            assert_eq!(obs.controls.iter().filter(|c| c.role == kind).count(), 1, "{style}-{kind}");
            assert_eq!(act(&h.page, click(target), &obs).await.outcome, StepOutcome::Executed, "{style}-{kind}");
            assert_eq!(find(&h.observe().await, "Direct service").checked, Some(true), "{style}-{kind}");

            eval_active(&h, "document.querySelector('input').disabled = true").await;
            assert!(!h.observe().await.controls.iter().any(|c| c.label == "Direct service"), "{style}-{kind}");
            h.close().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_styled_choice_rejects_reassociated_label() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h,
        "document.body.innerHTML = '<input id=a type=checkbox style=\"opacity:0\"><input id=b type=checkbox \
         style=\"opacity:0\"><label for=a>Direct service</label>'; true",
    )
    .await;
    let obs = h.observe().await;
    let target = find(&obs, "Direct service");
    eval_active(&h, "document.querySelector('label').htmlFor = 'b'").await;
    assert_eq!(act(&h.page, click(target), &obs).await.outcome, StepOutcome::Stale);
    assert!(!truthy(&eval_active(&h, "document.getElementById('b').checked").await));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_link_label_ignores_nested_style_and_script() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h,
        r#"document.body.innerHTML = '<a href="/pen"><style>.tw-reset * { box-sizing: border-box; }</style>'
          + '<script>window.x = 1;</script><span>Parker Jotter</span></a>';
        true"#,
    )
    .await;
    assert_eq!(find(&h.observe().await, "Parker Jotter").label, "Parker Jotter");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fill_activates_picker_before_typing_and_offers_suggestion() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h,
        r#"document.body.innerHTML = '<input aria-label="Station" role="combobox" aria-controls="choices">'
          + '<ul id="choices" role="listbox" hidden><li role="option">York Central</li></ul>';
        const input = document.querySelector('input');
        const choices = document.querySelector('ul');
        input.onclick = () => { choices.hidden = false; };
        choices.onclick = () => { input.value = 'York Central'; choices.hidden = true; };
        true"#,
    )
    .await;
    let obs = h.observe().await;
    let target = find(&obs, "Station");
    assert_eq!(act(&h.page, fill(target, "York"), &obs).await.outcome, StepOutcome::Executed);
    let obs = h.observe().await;
    assert_eq!(find(&obs, "Station").value.as_deref(), Some("York"));
    let option = find(&obs, "York Central");
    assert_eq!(act(&h.page, click(option), &obs).await.outcome, StepOutcome::Executed);
    assert_eq!(find(&h.observe().await, "Station").value.as_deref(), Some("York Central"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_frameset_pages_are_read_through_their_frames() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/frames.html").await;
    // A frameset document has no text of its own: everything a user sees is in its frames.
    assert!(h.observe().await.viewport_text.contains("Left pane"));
    let text = h.page.capture().await.expect("capture").text;
    assert!(text.contains("Left pane"));
    assert!(text.contains("Right pane"));
    h.close().await;
}

/// A frame whose document is still loading when its parent is ready is part of the page the navigation opened, so the
/// first observation holds its controls and the next step's effect does not claim them.
async fn a_frame_that_loads_after_its_page_is_in_the_first_observation(path: &str, scoped: bool) {
    let _turn = turn().await;
    let Some(connection) = chrome() else { return };
    // A scoped run holds each new frame paused until its gate is on, so its frame session attaches before the load.
    let allowed_origins = scoped.then(|| vec![sites().main.clone(), sites().iframe.clone()]);
    let connection = BrowserConnection { allowed_origins, ..connection.clone() };
    let Some(h) = harness_on(connection, Config::default(), SessionOptions::default()).await else { return };
    let started = Instant::now();
    h.goto(path).await;
    let navigated = started.elapsed();
    let observation = h.observe().await;
    eprintln!("{path}: navigate {navigated:?}, observe {:?}", started.elapsed() - navigated);
    find(&observation, "Late frame button");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_same_site_frame_that_loads_after_its_page_is_in_the_first_observation() {
    a_frame_that_loads_after_its_page_is_in_the_first_observation("/late-frame.html", false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_cross_site_frame_that_loads_after_its_page_is_in_the_first_observation() {
    a_frame_that_loads_after_its_page_is_in_the_first_observation("/late-cross-frame.html", false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_scoped_run_waits_for_a_cross_site_frame_that_loads_after_its_page() {
    a_frame_that_loads_after_its_page_is_in_the_first_observation("/late-cross-frame.html", true).await;
}

/// An element whose only sign of being pressable is a pointer listener a script added is offered, and clicking it
/// works; a container whose listener hears its children's clicks, or one holding a control, is not.
#[tokio::test(flavor = "multi_thread")]
async fn test_an_element_with_only_a_script_click_listener_is_offered() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/listened.html").await;
    let started = Instant::now();
    let obs = h.observe().await;
    eprintln!("listened.html: observe {:?}", started.elapsed());
    let labels: Vec<&str> = obs.controls.iter().map(|c| c.label.as_str()).collect();
    for label in ["Remove Combat", "Delegated row", "Plain button", "Dropped listener", "Shadow action"] {
        assert!(labels.contains(&label), "{label:?} not offered in {labels:?}");
    }
    for container in ["Box text", "Script listeners"] {
        assert!(!labels.iter().any(|l| l.contains(container)), "{container:?} offered in {labels:?}");
    }
    let remove = find(&obs, "Remove Combat");
    assert_eq!(remove.role, "button");
    assert_eq!(act(&h.page, click(remove), &obs).await.outcome, StepOutcome::Executed);
    assert_eq!(eval_active(&h, "document.getElementById('status').textContent").await, "Nothing chosen");
    let shadow = h.observe().await;
    act(&h.page, click(find(&shadow, "Shadow action")), &shadow).await;
    assert_eq!(eval_active(&h, "document.title").await, "Shadow pressed");
    // A listener the page took away takes its control with it.
    eval_active(&h, "dropListener()").await;
    let labels: Vec<String> = h.observe().await.controls.into_iter().map(|c| c.label).collect();
    assert!(!labels.iter().any(|l| l == "Dropped listener"), "{labels:?}");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_table_cell_drawn_with_an_icon_is_not_read_as_blank() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    let text = h.page.capture().await.expect("capture").text;
    assert!(text.contains("| First | [flag icon] | [yes] |"), "{text}");
    // A size or weight class shares the icon font's prefix without naming the glyph.
    assert!(text.contains("| Second |  | [xmark icon] |"), "{text}");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_menu_shown_on_the_next_animation_frame_opens() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/animated.html").await;
    let obs = h.observe().await;
    let opener = find(&obs, "Trip type");
    assert_eq!(act(&h.page, click(opener), &obs).await.outcome, StepOutcome::Executed);
    assert!(h.observe().await.controls.iter().any(|c| c.label == "One way"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_content_shown_only_under_the_pointer_is_reached_by_hovering() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/hovers.html").await;
    let obs = h.observe().await;
    let hovers: Vec<&Control> = obs.controls.iter().filter(|c| c.operations.contains(&Operation::Hover)).collect();
    // The two avatars and the menu reveal hidden content; the link's hover rule only recolours it.
    let mut labels: Vec<&str> = hovers.iter().map(|c| c.label.as_str()).collect();
    labels.sort_unstable();
    assert_eq!(labels, vec!["Products", "User Avatar", "User Avatar"]);
    let avatars = |controls: &[&Control]| -> Vec<(String, Option<String>)> {
        controls.iter().filter(|c| c.label == "User Avatar").map(|c| (c.id.clone(), c.context.clone())).collect()
    };
    assert_eq!(
        hovers.iter().filter(|c| c.label == "User Avatar").map(|c| c.context.as_deref()).collect::<Vec<_>>(),
        vec![Some("1 of 2"), Some("2 of 2")]
    );
    assert!(!obs.viewport_text.contains("grace"));
    let second = hovers.iter().find(|c| c.context.as_deref() == Some("2 of 2")).expect("second avatar");
    let result = act(&h.page, hover(second), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Executed);
    assert!(result.page_changed);
    let after = h.observe().await;
    assert!(after.viewport_text.contains("name: grace"));
    assert!(!after.viewport_text.contains("name: ada"));
    assert!(after.controls.iter().any(|c| c.label == "View profile"));
    let captured = h.page.capture().await.expect("capture");
    assert!(captured.text.contains("name: grace"));
    let revealed =
        captured.blocks.iter().find(|b| captured.block_text(b).contains("name: grace")).expect("revealed block");
    let context = revealed.control_context.as_ref().expect("control context");
    assert_eq!(context.label, "User Avatar");
    assert_eq!(context.context.as_deref(), Some("2 of 2"));
    assert!(captured.blocks.iter().filter(|b| captured.block_text(b) == "Team").all(|b| b.control_context.is_none()));
    let after_controls: Vec<&Control> = after.controls.iter().collect();
    assert_eq!(avatars(&after_controls), avatars(&hovers));
    let first = after.controls.iter().find(|c| c.context.as_deref() == Some("1 of 2")).expect("first avatar");
    let switched_result = act(&h.page, hover(first), &after).await;
    assert_eq!(switched_result.outcome, StepOutcome::Executed);
    let switched = h.observe().await;
    assert!(h.page.capture().await.expect("capture").text.contains("name: ada"));
    let switched_controls: Vec<&Control> = switched.controls.iter().collect();
    assert_eq!(avatars(&switched_controls), avatars(&hovers));
    h.close().await;
}

/// A media page's delete buttons were rendered by script only while a real pointer was over an image, so no stylesheet
/// rule named them, nothing offered a hover, and the run reported the image it could see as one it could not find.
#[tokio::test(flavor = "multi_thread")]
async fn test_controls_a_script_adds_under_the_pointer_are_reached_by_hovering_their_card() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/hover-gallery.html").await;
    let obs = h.observe().await;
    assert!(!obs.controls.iter().any(|c| c.label == "delete" || c.label == "edit"));
    let card = obs
        .controls
        .iter()
        .find(|c| c.operations.contains(&Operation::Hover) && c.label == "The quest log grouped by zone")
        .unwrap_or_else(|| panic!("no hover target for the second card in {:?}", obs.controls));
    assert_eq!(act(&h.page, hover(card), &obs).await.outcome, StepOutcome::Executed);
    let revealed = h.observe().await;
    // Neither revealed control has a name or an icon class, so each is named from its own class.
    for label in ["edit", "delete"] {
        let control = revealed
            .controls
            .iter()
            .find(|c| c.label == label)
            .unwrap_or_else(|| panic!("no control labelled {label:?} in {:?}", revealed.controls));
        assert_eq!(control.role, "button");
        assert_eq!(control.context.as_deref(), Some("The quest log grouped by zone"), "{label}");
    }
    let delete = find(&revealed, "delete");
    // A control whose class changes what it says it does is no longer the one observed.
    eval_active(&h, "document.querySelector('.delete-button').className = 'share-button'").await;
    assert_eq!(act(&h.page, click(delete), &revealed).await.outcome, StepOutcome::Stale);
    eval_active(&h, "document.querySelector('.share-button').className = 'delete-button'").await;
    assert_eq!(eval_active(&h, "document.getElementById('status').textContent").await, json!(""));
    assert_eq!(act(&h.page, click(delete), &revealed).await.outcome, StepOutcome::Executed);
    assert_eq!(eval_active(&h, "document.getElementById('status').textContent").await, json!("Deleted m2"));
    h.close().await;
}

/// GitHub's repository list draws each card's license as a law icon beside "BSD-3-Clause" in a row of twin spans.
/// The icon is aria-hidden decoration, yet the badge was offered as a hover target on every card, and a run asked
/// for httpx's license hovered one instead of opening the repository.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_label_beside_a_decorative_icon_is_not_a_hover_target() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(
        &h,
        r##"const icon = '<svg aria-hidden="true" width="16" height="16" viewBox="0 0 16 16"><path d="M2 2h12v12H2z"/></svg>';
        document.body.innerHTML = '<ul>' + ['httpx', 'httpcore'].map(name => `<li><h3><a href="#${name}">${name}</a></h3>
          <div><span class="mr-3">Python</span><a class="mr-3 Link" href="#${name}-stars">${icon}15</a>
          <span class="mr-3">${icon}BSD-3-Clause</span></div></li>`).join('') + '</ul>';"##,
    )
    .await;
    let obs = h.observe().await;
    let hovers: Vec<&Control> = obs.controls.iter().filter(|c| c.operations.contains(&Operation::Hover)).collect();
    assert!(hovers.is_empty(), "decorative icons offered hover targets: {hovers:?}");
    find(&obs, "httpx");
    h.close().await;
}

/// A gallery's buttons faded in on hover from opacity 0, so the observation taken right after the hover saw only the
/// transition's starting value and offered none of them.
#[tokio::test(flavor = "multi_thread")]
async fn test_controls_fading_in_under_the_pointer_are_offered_at_once() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/hover-fade.html").await;
    let obs = h.observe().await;
    assert!(!obs.controls.iter().any(|c| c.label == "delete" || c.label == "edit"));
    let card = |obs: &Observation, caption: &str| -> Control {
        obs.controls
            .iter()
            .find(|c| c.operations.contains(&Operation::Hover) && c.label == caption)
            .unwrap_or_else(|| panic!("no hover target for {caption:?} in {:?}", obs.controls))
            .clone()
    };
    let offered = |obs: &Observation, caption: &str| -> Vec<String> {
        obs.controls.iter().filter(|c| c.context.as_deref() == Some(caption)).map(|c| c.label.clone()).collect()
    };
    let second = "The quest log grouped by zone";
    assert_eq!(act(&h.page, hover(&card(&obs, second)), &obs).await.outcome, StepOutcome::Executed);
    let revealed = h.observe().await;
    let labels = offered(&revealed, second);
    assert!(labels.contains(&"delete".to_owned()) && labels.contains(&"edit".to_owned()), "{:?}", revealed.controls);
    let delete = revealed
        .controls
        .iter()
        .find(|c| c.label == "delete" && c.context.as_deref() == Some(second))
        .expect("delete for the second card");
    assert_eq!(act(&h.page, click(delete), &revealed).await.outcome, StepOutcome::Executed);
    assert_eq!(eval_active(&h, "document.getElementById('status').textContent").await, json!("Deleted m2"));

    // Moving to the third card fades its controls in and the second card's out.
    let third = "The settings panel";
    let before = h.observe().await;
    assert_eq!(act(&h.page, hover(&card(&before, third)), &before).await.outcome, StepOutcome::Executed);
    let moved = h.observe().await;
    let labels = offered(&moved, third);
    assert!(labels.contains(&"delete".to_owned()) && labels.contains(&"edit".to_owned()), "{:?}", moved.controls);
    let left = offered(&moved, second);
    assert!(!left.contains(&"delete".to_owned()) && !left.contains(&"edit".to_owned()), "{:?}", moved.controls);
    h.close().await;
}

/// A control that fades in but stays unclickable until the fade ends is not offered while it fades.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_fading_control_that_ignores_the_pointer_is_not_offered() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/hover-fade.html").await;
    eval_active(&h, "document.body.classList.add('late')").await;
    let obs = h.observe().await;
    let second = "The quest log grouped by zone";
    let card = obs
        .controls
        .iter()
        .find(|c| c.operations.contains(&Operation::Hover) && c.label == second)
        .unwrap_or_else(|| panic!("no hover target for the second card in {:?}", obs.controls));
    assert_eq!(act(&h.page, hover(card), &obs).await.outcome, StepOutcome::Executed);
    let revealed = h.observe().await;
    assert!(
        !revealed.controls.iter().any(|c| c.context.as_deref() == Some(second) && c.label == "delete"),
        "{:?}",
        revealed.controls
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_visible_option_is_clicked_without_scrolling_its_menu_shut() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/menu.html").await;
    let obs = h.observe().await;
    let trip = obs.controls.iter().find(|c| c.label == "Round trip" && c.role == "button").expect("trip type");
    act(&h.page, click(trip), &obs).await;
    let obs = h.observe().await;
    let option = obs.controls.iter().find(|c| c.label == "One way").expect("option");
    let result = act(&h.page, click(option), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Executed);
    let after = h.observe().await;
    assert!(after.controls.iter().any(|c| c.role == "button" && c.label == "One way"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_fill_follows_focus_to_the_editor_its_click_opened() {
    let _turn = turn().await;
    for opens in ["", "?late"] {
        let Some(h) = harness().await else { return };
        h.goto(&format!("/overlay.html{opens}")).await;
        let obs = h.observe().await;
        let field = obs.controls.iter().find(|c| c.label == "Where from?").expect("field");
        let result = act(&h.page, fill(field, "Lond"), &obs).await;
        assert_eq!(result.outcome, StepOutcome::Executed, "{opens:?}");
        let after = h.observe().await;
        assert_eq!(
            after.controls.iter().filter(|c| c.role == "option").map(|c| c.label.as_str()).collect::<Vec<_>>(),
            vec!["London", "Londonderry"],
            "{opens:?}"
        );
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_an_anchor_with_no_destination_but_click_semantics_is_offered_as_a_button() {
    let _turn = turn().await;
    // jQuery UI's datepicker appends its Next/Prev anchors to <body> once the field is focused, with a click
    // handler and no href: they carry no destination, so they must never be offered, or capped, as a pager.
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(
        &h,
        "document.body.innerHTML = '<a class=\"ui-datepicker-next\" data-handler=\"next\" data-event=\"click\" \
         title=\"Next\" onclick=\"this.dataset.clicked = String(1)\">Next</a>'",
    )
    .await;
    let obs = observe_until(&h.page, "Next").await;
    let control = find(&obs, "Next");
    assert_eq!(control.role, "button");
    assert_eq!(control.href, None);
    let result = act(&h.page, click(control), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Executed);
    let clicked = eval_active(&h, "document.querySelector('.ui-datepicker-next').dataset.clicked").await;
    assert_eq!(clicked, json!("1"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_what_a_form_holds_is_quotable_but_a_password_is_not() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(
        &h,
        concat!(
            "document.body.innerHTML = '<h2>Booking</h2>",
            "<label>Start Date<input type=\"date\" value=\"2026-09-28\"></label>",
            "<p><label for=\"end\">End Date</label><input id=\"end\" type=\"date\" value=\"2026-10-07\"></p>",
            "<select aria-label=\"Size\"><option>Small</option><option selected>Large</option></select>",
            "<label>Password<input type=\"password\" value=\"hunter2\"></label>",
            "<input type=\"text\" value=\"\" placeholder=\"Empty\">",
            "<table><tr><td><label for=\"guests\">Guests</label></td><td><input id=\"guests\" value=\"2\"></td></tr>",
            "</table>'",
        ),
    )
    .await;
    let text = h.page.capture().await.expect("capture").text;
    assert!(text.contains("Guests: 2"), "{text}");
    assert!(text.contains("Start Date: 2026-09-28"), "{text}");
    assert!(text.contains("End Date: 2026-10-07"), "{text}");
    assert!(text.contains("Size: Large"), "{text}");
    assert!(!text.contains("hunter2"));
    assert!(!text.contains("Empty"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_uniquely_named_date_field_still_gets_its_section_heading() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(
        &h,
        concat!(
            "document.body.innerHTML = '<section><h2>Date Picker 3</h2>",
            "<label>Start Date<input type=\"date\" id=\"start\"></label>",
            "<label>End Date<input type=\"date\" id=\"end\"></label></section>'",
        ),
    )
    .await;
    let obs = observe_until(&h.page, "Start Date").await;
    let start = find(&obs, "Start Date");
    assert_eq!(start.input_type.as_deref(), Some("date"));
    assert_eq!(start.context.as_deref(), Some("Date Picker 3"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_an_unrelated_sibling_heading_does_not_name_a_unique_control() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    for panel in [
        "aside",
        "section",
        "nav",
        "main",
        "form",
        "fieldset",
        "dialog open",
        "search",
        "div role=region",
        "div role=complementary",
        "div role=navigation",
        "div role=main",
        "div role=form",
        "div role=dialog",
        "div role=alertdialog",
        "div role=search",
        "div role=group",
    ] {
        let tag = panel.split_whitespace().next().unwrap();
        let html = format!(
            "<div id=app><{panel}><h2>Weather</h2><span>100 mi</span></{tag}>\
             <main><label for=q>Search</label><input id=q></main></div>"
        );
        eval_active(&h, &format!("document.body.innerHTML = {}", json!(html))).await;
        let obs = observe_until(&h.page, "Search").await;
        assert_eq!(find(&obs, "Search").context.as_deref(), None, "{panel}");
        eval_active(&h, "document.querySelector('h2').textContent = 'Updated weather'").await;
        let updated = h.observe().await;
        assert_eq!(find(&updated, "Search").context, find(&obs, "Search").context, "{panel}");
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_pointer_styled_anchor_without_href_is_offered_and_clicked() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(
        &h,
        "document.body.innerHTML = '<p id=status>idle</p><a class=btn style=cursor:pointer>More</a>'; document.querySelector('a').addEventListener('click', () => document.getElementById('status').textContent = 'clicked')",
    )
    .await;
    let observation = h.observe().await;
    let more = find(&observation, "More");
    assert_eq!(more.role, "button");
    assert!(more.operations.contains(&Operation::Click));
    let result = act(&h.page, click(more), &observation).await;
    assert_eq!(result.outcome, StepOutcome::Executed);
    assert_eq!(eval_active(&h, "document.getElementById('status').textContent").await, json!("clicked"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_native_form_constraints_mark_invalid_controls() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(
        &h,
        concat!(
            "document.body.innerHTML = '<form id=f><label>Email<input id=email type=email required value=a@example.test></label>",
            "<label>Last name<input id=last required value=Tester></label>",
            "<label>Read only<input id=readonly required readonly></label><button>Submit</button></form>'",
        ),
    )
    .await;
    let before = h.observe().await;
    assert_eq!(find(&before, "Email").invalid, None);
    assert_eq!(find(&before, "Last name").invalid, None);
    eval_active(
        &h,
        "document.getElementById('email').setCustomValidity('Use a valid email'); \
         document.getElementById('last').value = ''; document.getElementById('readonly').setCustomValidity('Not editable'); document.getElementById('f').requestSubmit()",
    )
    .await;
    let validity = eval_active(&h, "[email.validity.valid, last.validity.valid, email.validationMessage, document.getElementById('readonly').willValidate, document.getElementById('readonly').validity.valid]").await;
    assert_eq!(validity, json!([false, false, "Use a valid email", false, false]));
    let after = h.observe().await;
    assert_eq!(find(&after, "Email").invalid, Some(true));
    assert_eq!(find(&after, "Last name").invalid, Some(true));
    assert_eq!(find(&after, "Read only").invalid, None);
    h.close().await;
}

/// An empty required field the page has just shown is not one the browser refused: it says so only after an edit or a
/// submit, and told it earlier a run that had submitted a wizard read the form it reset to as still to fill.
#[tokio::test(flavor = "multi_thread")]
async fn test_an_untouched_required_field_is_not_refused_until_a_submit() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(&h, "document.body.innerHTML = '<form id=f><label>Name<input id=name required></label><button>Go</button></form>'; 0")
        .await;
    let untouched = h.observe().await;
    assert_eq!(find(&untouched, "Name").invalid, None);
    assert_eq!(find(&untouched, "Name").required, Some(true));
    eval_active(&h, "document.getElementById('f').requestSubmit(); 0").await;
    assert_eq!(find(&h.observe().await, "Name").invalid, Some(true));
    h.close().await;
}

/// A refused submit put no page text on screen, so the run saw only that a field was invalid and refilled the one
/// the task named instead of the empty one the browser was complaining about.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_field_the_browser_refuses_carries_its_validation_message() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(
        &h,
        "document.body.innerHTML = '<form id=f><label>Zip<input id=zip required value=12345></label>\
         <label>Last name<input id=last required></label><button>Continue</button></form>'; 0",
    )
    .await;
    let before = h.observe().await;
    let message =
        |o: &Observation, label: &str| serde_json::to_value(find(o, label)).unwrap()["validation_message"].clone();
    assert_eq!(message(&before, "Zip"), Value::Null);
    let browser_says = eval_active(&h, "document.getElementById('f').requestSubmit(); last.validationMessage").await;
    assert!(browser_says.as_str().is_some_and(|text| !text.is_empty()), "{browser_says}");
    let after = h.observe().await;
    assert_eq!(message(&after, "Last name"), browser_says);
    assert_eq!(message(&after, "Zip"), Value::Null);
    h.close().await;
}

/// A field named `method` makes `form.method` return that field, and a React fiber on it points back at it, so
/// serialising the form's submission as if every property were text threw and ended the run as a browser error.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_form_property_that_is_an_element_does_not_end_observe() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(
        &h,
        "document.body.innerHTML = '<form id=f><input type=hidden name=method><label>Search<input type=search></label>\
         <button>Go</button></form>'; const field = document.querySelector('[name=method]'); field.fiber = {field}; 0",
    )
    .await;
    assert_eq!(
        eval_active(&h, "document.getElementById('f').method === document.querySelector('[name=method]')").await,
        json!(true)
    );
    let observation = h.observe().await;
    let go = find(&observation, "Go");
    let semantics: Value = serde_json::from_str(go.submit_semantics.as_deref().unwrap()).unwrap();
    assert_eq!(semantics["method"], "");
    h.close().await;
}

/// A post's heading ("Data Entry Form") named a date picker's Submit, and the verifier read the wrong form.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_control_is_named_by_the_title_nearest_before_it_not_the_posts_first_heading() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(
        &h,
        concat!(
            "document.body.innerHTML = '<article><h3>Data Entry Form</h3><p>Pick dates below.</p>",
            "<label>Date Picker 3</label><div><label>Start Date<input type=\"date\"></label>",
            "<button>Submit</button></div></article>",
            "<form><h3>Contact</h3><label for=\"n\">Name</label><input id=\"n\"><button>Submit</button></form>'",
        ),
    )
    .await;
    let obs = observe_until(&h.page, "Start Date").await;
    assert_eq!(find(&obs, "Start Date").context.as_deref(), Some("Date Picker 3"));
    let mut submits: Vec<&str> =
        obs.controls.iter().filter(|c| c.label == "Submit").map(|c| c.context.as_deref().unwrap_or_default()).collect();
    submits.sort_unstable();
    assert_eq!(submits, vec!["Contact", "Date Picker 3"]);
    h.close().await;
}

/// A shared "Options" heading, a hidden label and a label holding an output are not what names a card.
#[tokio::test(flavor = "multi_thread")]
async fn test_twins_keep_their_card_names_past_shared_hidden_and_control_labels() {
    let _turn = turn().await;
    let card = |name: &str| {
        format!(
            "<div><h2>{name}</h2><label hidden>Unavailable</label><label>Total <output>12</output></label>\
             <h3>Options</h3><label>Quantity<input></label><button>Add to cart</button></div>"
        )
    };
    let contexts = |obs: &Observation, label: &str| -> Vec<Option<String>> {
        obs.controls.iter().filter(|c| c.label == label).map(|c| c.context.clone()).collect()
    };
    let twins = vec![Some("Brass Kettle".to_owned()), Some("Copper Pan".to_owned())];
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(&h, &format!("document.body.innerHTML = '{}{}'", card("Brass Kettle"), card("Copper Pan"))).await;
    let obs = observe_until(&h.page, "Add to cart").await;
    for label in ["Add to cart", "Quantity"] {
        assert_eq!(contexts(&obs, label), twins, "{label}");
    }
    // One card's transparent label is not shared, so the shared-title fallback cannot be what skips it.
    eval_active(
        &h,
        "document.body.innerHTML = '<div><h2>Brass Kettle</h2><label style=\"opacity:0\">Sold out</label>\
         <button>Add to cart</button></div><div><h2>Copper Pan</h2><button>Add to cart</button></div>'",
    )
    .await;
    let obs = observe_until(&h.page, "Add to cart").await;
    assert_eq!(contexts(&obs, "Add to cart"), twins);
    h.close().await;
}

/// Options that are direct children of one list share every ancestor, and an aria-label hides the text inside
/// them, so two "Springfield" options read as "1 of 2" and "2 of 2" and the run picked one blind.
#[tokio::test(flavor = "multi_thread")]
async fn test_same_named_siblings_are_told_apart_by_their_own_visible_descriptor() {
    let _turn = turn().await;
    let contexts = |obs: &Observation| -> Vec<Option<String>> {
        obs.controls.iter().filter(|c| c.label == "Springfield").map(|c| c.context.clone()).collect()
    };
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(
        &h,
        r#"document.body.innerHTML = '<ul role=listbox>'
        + '<li role=option aria-label="Springfield"><span>Springfield</span><small>City in Illinois</small></li>'
        + '<li role=option aria-label="Springfield"><span>Springfield</span><small>State park in Oregon</small></li>'
        + '</ul>'"#,
    )
    .await;
    let obs = observe_until(&h.page, "Springfield").await;
    assert_eq!(contexts(&obs), vec![Some("City in Illinois".into()), Some("State park in Oregon".into())]);
    // Identical descriptors tell nothing apart, so position stays the fallback.
    eval_active(
        &h,
        r#"document.body.innerHTML = '<ul role=listbox>'
        + '<li role=option aria-label="Springfield"><span>Springfield</span></li>'
        + '<li role=option aria-label="Springfield"><span>Springfield</span></li></ul>'"#,
    )
    .await;
    let obs = observe_until(&h.page, "Springfield").await;
    assert_eq!(contexts(&obs), vec![Some("1 of 2".into()), Some("2 of 2".into())]);
    h.close().await;
}

/// A list's "More" button, named after the last product above it, read as that product's own and was not the list's.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_card_title_does_not_name_a_control_past_the_card() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(
        &h,
        r#"document.body.innerHTML = '<main><h2>Tablets</h2><div>'
        + '<div class="card"><h4>Fire 7</h4></div><div class="card"><h4>Memo Pad</h4></div>'
        + '</div><button>More</button></main>'"#,
    )
    .await;
    let obs = observe_until(&h.page, "More").await;
    let more = obs.controls.iter().find(|c| c.label == "More").unwrap();
    assert_eq!(more.context.as_deref(), Some("Tablets"));
    h.close().await;
}

/// A jQuery UI datepicker's Next and days were named "Su", its weekday header, so every month read alike: the
/// run could not tell which month it was on and went back and forth past the one it wanted.
#[tokio::test(flavor = "multi_thread")]
async fn test_calendar_twins_are_named_by_the_month_they_show() {
    let _turn = turn().await;
    let calendar = |month: &str| {
        format!(
            "<div><div><a style=\"float:left\">Prev</a><a style=\"float:right\">Next</a>\
             <div><span>{month}</span> <span>2026</span></div></div>\
             <table><thead><tr><th>Su</th><th>Mo</th></tr></thead><tbody><tr><td><a>1</a></td><td><a>2</a></td></tr>\
             </tbody></table></div>"
        )
    };
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(
        &h,
        &format!(
            "document.body.innerHTML = '{}{}';for (const a of document.querySelectorAll('a')) a.href = '#';",
            calendar("September"),
            calendar("October")
        ),
    )
    .await;
    let obs = observe_until(&h.page, "Next").await;
    let contexts = |label: &str| -> Vec<Option<&str>> {
        obs.controls.iter().filter(|c| c.label == label).map(|c| c.context.as_deref()).collect()
    };
    for label in ["Prev", "Next"] {
        assert_eq!(contexts(label), vec![Some("September 2026"), Some("October 2026")], "{label}");
    }
    assert_eq!(contexts("2"), vec![Some("September 2026; column: Mo"), Some("October 2026; column: Mo")]);
    h.close().await;
}

/// A calendar with no month heading whose grid repeats days from the next month: a week row's text is one line,
/// "26 27 28 29 30 1 2", so day 26 was offered with its week's other numbers as its context (#648).
#[tokio::test(flavor = "multi_thread")]
async fn test_a_calendar_day_is_not_named_after_the_other_days_in_its_week() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    let week = |days: std::ops::RangeInclusive<u32>| {
        days.map(|day| format!("<td><button>{}</button></td>", (day - 1) % 31 + 1)).collect::<String>()
    };
    let rows: String = [26..=32, 33..=39, 40..=46, 54..=60, 62..=68]
        .into_iter()
        .map(|days| format!("<tr>{}</tr>", week(days)))
        .collect();
    eval_active(
        &h,
        &format!(
            "document.body.innerHTML = '<table><thead><tr><th>Po</th><th>Ut</th><th>St</th><th>Ct</th><th>Pa</th>\
             <th>So</th><th>Ne</th></tr></thead><tbody>{rows}</tbody></table>';"
        ),
    )
    .await;
    let obs = observe_until(&h.page, "26").await;
    let days: Vec<_> = obs.controls.iter().filter(|c| c.label.chars().all(|ch| ch.is_ascii_digit())).collect();
    assert!(days.len() > 30, "{}", days.len());
    for day in days {
        let context = day.context.as_deref().unwrap_or("");
        // A twin's ordinal ("1 of 2") tells repeated days apart; any other number is another day's label.
        let section = context.split(';').next().unwrap_or("");
        let ordinal = section.split(' ').collect::<Vec<_>>();
        let names_twin =
            matches!(ordinal.as_slice(), [n, "of", m] if n.parse::<u32>().is_ok() && m.parse::<u32>().is_ok());
        assert!(
            names_twin || !section.chars().any(|ch| ch.is_ascii_digit()),
            "day {} has context {context:?}",
            day.label
        );
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_uniquely_labelled_control_is_given_its_section_heading() {
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(
        &h,
        r#"document.body.innerHTML = `
        <section><h2>Billing</h2><div><label>Card <input></label><button>Pay now</button></div></section>
        <section><h2>Delivery</h2><div><a href="/track">Track parcel</a></div></section>`"#,
    )
    .await;
    let obs = observe_until(&h.page, "Track parcel").await;
    assert_eq!(find(&obs, "Card").context.as_deref(), Some("Billing"));
    assert_eq!(find(&obs, "Pay now").context.as_deref(), Some("Billing"));
    assert_eq!(find(&obs, "Track parcel").context.as_deref(), Some("Delivery"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_table_controls_keep_columns_across_empty_cells_and_spans() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(
        &h,
        r#"document.body.innerHTML = `
        <table><thead><tr><th rowspan="2">Week</th><th colspan="2">October 2026</th></tr>
        <tr><th><span title="Thursday">Th</span></th><th><abbr title="Friday">Fr</abbr></th></tr></thead>
        <tbody><tr><th scope="row" rowspan="2">A</th><td></td><td><button>2</button></td></tr>
        <tr><td><button>8</button></td><td><button>9</button></td></tr></tbody></table>
        <table><tr><th id="other">Price</th><th>Quantity</th></tr>
        <tr><td><button>Buy</button></td><td headers="other"><button>Quote</button></td></tr></table>
        <table><thead><tr><th>Product</th><th>Price</th></tr></thead>
        <tbody><tr><th scope="row" rowspan="0">A</th><td>10</td></tr></tbody>
        <tbody><tr><td>B</td><td><button>Offer</button></td></tr></tbody></table>`"#,
    )
    .await;
    let obs = observe_until(&h.page, "Quote").await;
    assert_eq!(find(&obs, "2").context.as_deref(), Some("A; column: October 2026 / Friday"));
    assert_eq!(find(&obs, "8").context.as_deref(), Some("column: October 2026 / Thursday"));
    assert_eq!(find(&obs, "9").context.as_deref(), Some("column: October 2026 / Friday"));
    assert_eq!(find(&obs, "Buy").context.as_deref(), Some("column: Price"));
    assert_eq!(find(&obs, "Quote").context.as_deref(), Some("column: Price"));
    assert_eq!(find(&obs, "Offer").context.as_deref(), Some("column: Price"));
    h.close().await;
}

/// The whole point of `cdp_url`: drive a browser someone else started, and leave it running.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_browser_handed_over_by_cdp_url_drives_and_survives_the_run() {
    let _turn = turn().await;
    let Some(chrome_connection) = chrome().cloned() else { return };
    let sink = Arc::new(RecordingArtifactSink::default());
    let handed = BrowserConnection::new(chrome_connection.cdp_url.clone(), true);
    let session = BrowserSession::new(handed, sink.clone(), SessionOptions::default());
    session.open().await.expect("session opens");
    let page = CdpPage::new(session.clone(), Config::default());
    page.navigate(&sites().main, None).await.expect("navigate");
    let observation = page.observe().await.expect("observe");
    assert!(!observation.controls.is_empty(), "a browser reached over cdp_url must index like any other");
    session.close().await.expect("close");
    // The session closed its own tab; the browser it was handed is still answering.
    let after = BrowserSession::new(chrome_connection, sink, SessionOptions::default());
    after.open().await.expect("session opens");
    CdpPage::new(after.clone(), Config::default()).observe().await.expect("observe");
    after.close().await.expect("close");
}

/// A tab an irreversible action was sent from is the user's once the run ends: a payment still redirecting there
/// would be aborted by closing it (#748).
#[tokio::test(flavor = "multi_thread")]
async fn test_a_tab_handed_over_after_a_commit_stays_open_when_the_run_ends() {
    let _turn = turn().await;
    let Some(chrome_connection) = chrome().cloned() else { return };
    let sink = Arc::new(RecordingArtifactSink::default());
    let handed = BrowserConnection::new(chrome_connection.cdp_url.clone(), true);
    let session = BrowserSession::new(handed, sink.clone(), SessionOptions::default());
    session.open().await.expect("session opens");
    let page = CdpPage::new(session.clone(), Config::default());
    page.navigate(&sites().main, None).await.expect("navigate");
    let committed = session.active_target_id();
    fastbrowse_core::page::Page::hand_over_tab(&page);
    session.close().await.expect("close");
    let after = BrowserSession::new(chrome_connection, sink, SessionOptions::default());
    after.open().await.expect("session opens");
    let targets = after.send("Target.getTargets", json!({}), None).await.expect("targets");
    let open = targets["targetInfos"].as_array().expect("target infos").iter().any(|t| t["targetId"] == committed);
    let _ = after.send("Target.closeTarget", json!({"targetId": committed}), None).await;
    after.close().await.expect("close");
    assert!(open, "the tab a commit was sent from must outlive the run");
}

/// A run in a visible window clicks, types and captures behind the window's current tab, and activates nothing.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_background_run_drives_its_tab_without_bringing_it_forward() {
    let _turn = turn().await;
    let Some(chrome_connection) = chrome().cloned() else { return };
    let sent = Arc::new(Mutex::new(vec![]));
    let background = BrowserConnection { foreground: false, ..chrome_connection };
    let Some(h) = proxied(background, recording(sent.clone())).await else { return };
    h.page
        .navigate(
            "data:text/html,<title>Idle</title><button onclick=\"document.title='Clicked'\">Go</button><input \
             aria-label='Name'>",
            None,
        )
        .await
        .expect("navigate");
    let observation = h.observe().await;
    act(&h.page, click(find(&observation, "Go")), &observation).await;
    let observation = h.observe().await;
    assert_eq!(observation.title, "Clicked");
    let filled = act(&h.page, fill(find(&observation, "Name"), "Ada"), &observation).await;
    assert_eq!(filled.outcome, StepOutcome::Executed);
    assert_eq!(eval_active(&h, "document.querySelector('input').value").await, json!("Ada"));
    assert!(truthy(&eval_active(&h, "document.visibilityState === 'visible' && document.hasFocus()").await));
    // A tab behind another that has gone idle paints nothing new, and a capture would wait on its next frame.
    sleep(Duration::from_secs(1)).await;
    let png = timeout(Duration::from_secs(2), h.page.screenshot()).await.expect("in time").expect("screenshot");
    assert!(png.starts_with(b"\x89PNG"));
    h.close().await;
    let sent = sent.lock().unwrap();
    assert!(!sent.iter().any(|(m, _)| m == "Target.activateTarget" || m == "Page.bringToFront"));
    let created: Vec<&Value> = sent.iter().filter(|(m, _)| m == "Target.createTarget").map(|(_, p)| p).collect();
    assert_eq!(created, vec![&json!({"url": "about:blank", "background": true})]);
}

/// A native dialog holds its renderer, so setup sent to a page behind one, focus emulation included, waits
/// for the answer. The run must still be able to answer it, and setup must then finish rather than hang.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_background_run_sets_up_tabs_behind_an_open_confirm_once_it_is_answered() {
    let _turn = turn().await;
    let Some(chrome_connection) = chrome().cloned() else { return };
    let background = BrowserConnection { foreground: false, ..chrome_connection };
    let Some(h) = harness_on(background, Config::default(), SessionOptions::default()).await else { return };
    h.goto("/").await;
    let opener = active(&h);
    // The popup shares the opener's renderer, which the confirm holds until it is answered.
    h.session
        .send(
            "Runtime.evaluate",
            json!({"expression": "setTimeout(() => { window.open('/popup.html'); confirm('Remove?') }, 0)"}),
            Some(&opener),
        )
        .await
        .expect("evaluate");
    let s = &h.session;
    wait_until(move || async move { s.pending_dialog().is_some() && !s.popups().is_empty() }).await;
    let popup = h.session.popups().last().cloned().expect("popup");
    let prepared = {
        let (session, opener) = (h.session.clone(), opener.clone());
        tokio::spawn(async move { testing::prepare_session(&session, &opener).await })
    };
    sleep(Duration::from_millis(500)).await;
    timeout(Duration::from_secs(5), async {
        h.session.handle_dialog(true, None).await.expect("dialog answered");
        assert!(testing::browser::popup_adopted(&h.session, &popup).await);
        prepared.await.expect("join").expect("prepared");
    })
    .await
    .expect("in time");
    h.session.switch_tab(&popup).await.expect("switch");
    assert!(truthy(&eval_active(&h, "document.hasFocus()").await));
    h.close().await;
}

/// Answer `Fetch.enable` on a new window's session with an error Chrome never hears of, so the window stays paused
/// as Chrome opened it, waiting for the run to resume it.
fn refusing_popup_fetch() -> Hook {
    let popups: Arc<Mutex<HashSet<String>>> = Arc::default();
    Arc::new(move |toward, value| {
        let params = &value["params"];
        match toward {
            Toward::Client
                if method(&value) == "Target.attachedToTarget"
                    && params["waitingForDebugger"] == true
                    && params["targetInfo"]["openerId"].as_str().is_some_and(|o| !o.is_empty()) =>
            {
                popups.lock().unwrap().insert(params["sessionId"].as_str().unwrap_or_default().to_owned());
            }
            Toward::Browser
                if method(&value) == "Fetch.enable"
                    && value["sessionId"].as_str().is_some_and(|s| popups.lock().unwrap().contains(s)) =>
            {
                let error = json!({"code": -31999, "message": "refused"});
                return Some((
                    json!({"id": value["id"], "sessionId": value["sessionId"], "error": error}),
                    Duration::ZERO,
                ));
            }
            _ => {}
        }
        Some((value, Duration::ZERO))
    })
}

/// Open a popup on `url` from the active tab and confirm straight after, which Chrome holds until the popup runs.
async fn open_popup_and_confirm(h: &Harness, url: &str) {
    let expression = format!("setTimeout(() => {{ window.open({url:?}); confirm('Remove?') }}, 0)");
    h.session.send("Runtime.evaluate", json!({"expression": expression}), Some(&active(h))).await.expect("evaluate");
}

async fn target_info(h: &Harness, target_id: &str) -> Option<Value> {
    let targets = h.session.send("Target.getTargets", json!({}), None).await.expect("targets");
    targets["targetInfos"].as_array().expect("targets").iter().find(|t| t["targetId"] == target_id).cloned()
}

/// A popup whose setup fails must not stay paused, since its opener's script waits in `window.open` until it runs.
/// The run owns this one, and its scope gate and capture may not be on, so it is closed.
#[tokio::test(flavor = "multi_thread")]
async fn test_an_owned_popup_whose_setup_fails_is_closed_and_frees_its_opener() {
    let _turn = turn().await;
    let Some(connection) = chrome().cloned() else { return };
    // Behind the window's current tab, so the popup gets a paint keeper that must go with it.
    let background = BrowserConnection { foreground: false, ..connection };
    let Some(h) = proxied(background, refusing_popup_fetch()).await else { return };
    h.goto("/").await;
    // The popup's address counts the connections made to it, since a popup resumed ungated would load it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = format!("http://{}/popup.html", listener.local_addr().expect("addr"));
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = requests.clone();
    tokio::spawn(async move {
        while listener.accept().await.is_ok() {
            counted.fetch_add(1, Ordering::SeqCst);
        }
    });
    open_popup_and_confirm(&h, &address).await;
    let s = &h.session;
    wait_until(move || async move { s.pending_dialog().is_some() && !s.popups().is_empty() }).await;
    let popup = h.session.popups().last().cloned().expect("popup");
    assert!(!testing::browser::popup_adopted(&h.session, &popup).await);
    wait_until(|| async { target_info(&h, &popup).await.is_none() }).await;
    assert!(!testing::keepers(&h.session).iter().any(|(target, _)| *target == popup));
    assert_eq!(requests.load(Ordering::SeqCst), 0, "the closed popup loaded nothing");
    h.session.handle_dialog(true, None).await.expect("dialog answered");
    h.close().await;
}

/// A popup of an attached window is not the run's to close: it is resumed and let go, and the window stays open.
#[tokio::test(flavor = "multi_thread")]
async fn test_an_attached_windows_popup_whose_setup_fails_is_let_go_and_left_running() {
    let _turn = turn().await;
    let Some(connection) = chrome().cloned() else { return };
    // The person's window, open before the run attaches and with no other session on it.
    let window = {
        let Some(owner) = harness().await else { return };
        let url = format!("{}/?person-658", sites().main);
        let created = owner.session.send("Target.createTarget", json!({"url": url}), None).await.expect("create");
        owner.close().await;
        str_of(&created["targetId"])
    };
    let url = proxy::start(connection.cdp_url.clone(), refusing_popup_fetch()).await;
    let attached = BrowserConnection {
        cdp_url: url,
        attach: true,
        target_match: Some("person-658".into()),
        foreground: false,
        ..connection
    };
    let Some(h) = harness_on(attached, Config::default(), SessionOptions::default()).await else { return };
    assert_eq!(h.session.active_target_id(), window);
    open_popup_and_confirm(&h, "/popup.html").await;
    let s = &h.session;
    wait_until(move || async move { s.pending_dialog().is_some() && !s.popups().is_empty() }).await;
    let popup = h.session.popups().last().cloned().expect("popup");
    assert!(!testing::browser::popup_adopted(&h.session, &popup).await);
    assert!(!testing::keepers(&h.session).iter().any(|(target, _)| *target == popup));
    // The popup shares its opener's renderer, so it loads once the confirm is answered.
    h.session.handle_dialog(true, None).await.expect("dialog answered");
    wait_until(|| async {
        target_info(&h, &popup)
            .await
            .is_some_and(|t| t["attached"] == false && str_of(&t["url"]).contains("popup.html"))
    })
    .await;
    h.close().await;
    let Some(cleanup) = harness().await else { return };
    let ids: Vec<String> = {
        let targets = cleanup.session.send("Target.getTargets", json!({}), None).await.expect("targets");
        targets["targetInfos"].as_array().expect("targets").iter().map(|t| str_of(&t["targetId"])).collect()
    };
    assert!(ids.contains(&window), "the person's window stays open");
    for target in [&window, &popup] {
        let _ = cleanup.session.send("Target.closeTarget", json!({"targetId": target}), None).await;
    }
    cleanup.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_csv_download_is_read_row_by_row_and_cites_the_response() {
    let _turn = turn().await;
    // A CSV file is shown as a page table is: one block per record under its header, so a total or count cites each
    // record it rests on. A file in any other format stays one block, marked whole when the server sent all of it.
    for path in ["/report.csv", "/export-report", "/report.csv?short", "/report.csv?partial"] {
        let Some(h) = harness().await else { return };
        h.goto("").await;
        h.goto(path).await;
        let capture = h.page.capture().await.expect("capture");
        let rows: Vec<_> = capture.blocks.iter().filter(|b| b.source_id.starts_with("download/")).collect();
        let expected = if path.contains('?') { 1 } else { 10001 };
        assert_eq!(rows.len(), expected, "{path}");
        for block in &rows {
            assert_eq!(block.kind, BlockKind::Table, "{path}");
            assert!(!block.complete_source, "{path}");
            assert_eq!(block.source_url.as_deref(), Some(format!("{}{path}", sites().main).as_str()));
            assert!(capture.block_text(block).starts_with("name,total\n"), "{path}");
        }
        assert_eq!(capture.block_text(rows[0]), "name,total\nReport,139.79", "{path}");
        let partial = rows[0].heading_path[0].contains("only part");
        assert_eq!(partial, path == "/report.csv?partial", "{path}");
        h.close().await;
    }
}

/// A try restarts the load, so a page whose parse waits on a script slower than one try's wait was never ready on any.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_page_slower_than_one_try_is_waited_for() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    let url = format!("{}/slow-page?ms=1200", sites().main);
    h.page.navigate_within(&url, 0.4, None).await.expect("a slow page becomes ready on a later, longer try");
    assert!(h.observe().await.viewport_text.contains("Loaded"));
    h.close().await;
}

/// Not a test: the PDF helper, when this binary is run as one.
#[test]
fn pdf_helper() {
    testing::serve_if_helper();
}

fn read_pdfs() {
    fastbrowse_browser::helper::register(testing::test_binary_helper("pdf_helper"));
}

/// Chrome's PDF viewer keeps a PDF's text out of the DOM, so the tab read as an empty page and a run never saw a map
/// whose every label is text in the file. A PDF tab is kept as a downloaded file is, and its text reaches the run so.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_pdf_tab_shows_and_captures_the_text_the_pdf_holds() {
    read_pdfs();
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/map.pdf").await;
    let url = format!("{}/map.pdf", sites().main);
    let text = h.observe().await.viewport_text;
    assert!(text.contains(&format!("[Downloaded file: map.pdf from {url}]\nFairbanks Wind")), "{text}");
    let capture = h.page.capture().await.expect("capture");
    let pdf = capture.blocks.iter().find(|block| block.source_url.as_deref() == Some(url.as_str())).expect("pdf block");
    assert_eq!(capture.block_text(pdf), "Fairbanks Wind");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_pdf_popup_is_readable_from_its_first_response() {
    read_pdfs();
    let _turn = turn().await;
    let Some(h) = loaded().await else { return };
    eval_active(
        &h,
        r#"document.body.insertAdjacentHTML('beforeend', '<a href="/map.pdf" target="_blank">Open PDF</a>')"#,
    )
    .await;
    let obs = h.observe().await;
    let result = act(&h.page, click(find(&obs, "Open PDF")), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Executed);
    let capture = h.page.capture().await.expect("capture");
    let url = format!("{}/map.pdf", sites().main);
    let pdf =
        capture.blocks.iter().find(|block| block.source_url.as_deref() == Some(url.as_str())).expect("popup PDF text");
    assert_eq!(capture.block_text(pdf), "Fairbanks Wind");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_http_error_status_travels_with_the_observed_document() {
    let _turn = turn().await;
    for status in [403u16, 419, 429, 503, 999] {
        let Some(h) = harness().await else { return };
        h.goto(&format!("/http-error/{status}")).await;
        let failed = h.observe().await;
        assert_eq!(failed.response_status, Some(i64::from(status)));
        assert_eq!(failed.viewport_text, "Sorry");
        h.goto("").await;
        assert_eq!(h.observe().await.response_status, Some(200));
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_http_status_is_not_attributed_to_a_client_side_route() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/http-error/404").await;
    assert_eq!(h.observe().await.response_status, Some(404));
    eval_active(&h, "history.pushState({}, '', '/recovered')").await;
    let recovered = h.observe().await;
    assert!(recovered.url.ends_with("/recovered"));
    assert_eq!(recovered.response_status, None);
    eval_active(&h, "history.back()").await;
    let hr = &h;
    wait_until(move || async move { hr.observe().await.url.ends_with("/http-error/404") }).await;
    assert_eq!(h.observe().await.response_status, Some(404));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_page_javascript_cannot_forge_the_http_status() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h,
        "Performance.prototype.getEntriesByType = performance.getEntriesByType = () => [{name: location.href, \
         responseStatus: 401}]; Object.defineProperty(PerformanceNavigationTiming.prototype, 'responseStatus', {get: \
         () => 401}); true",
    )
    .await;
    assert_eq!(h.observe().await.response_status, Some(200));
    assert_eq!(h.page.response_status().await.expect("status"), Some(200));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_an_unchanged_document_is_not_read_for_its_status_again() {
    let _turn = turn().await;
    let Some(connection) = chrome().cloned() else { return };
    let sent = Arc::new(Mutex::new(vec![]));
    let Some(h) = proxied(connection, recording(sent.clone())).await else { return };
    h.goto("").await;
    assert_eq!(h.observe().await.response_status, Some(200));
    // A request finishing moves the lifecycle generation, as a late networkIdle or a frame's load does, on the same
    // document: what the page does after it is read is not a new document.
    let mark = h.page.lifecycle_mark();
    eval_active(&h, "fetch(location.href).then(r => r.text()); true").await;
    let hr = &h;
    wait_until(move || async move { hr.page.lifecycle_mark() != mark }).await;
    sent.lock().unwrap().clear();
    // The document is the one just read, so its status is the read already made: no command asks again.
    assert_eq!(h.observe().await.response_status, Some(200));
    {
        let calls = sent.lock().unwrap();
        assert!(!calls.iter().any(|(m, _)| m == "Page.getFrameTree" || m == "Page.createIsolatedWorld"), "{calls:?}");
        assert!(
            !calls.iter().any(|(m, p)| m == "Runtime.evaluate"
                && (p.get("contextId").is_some() || p.get("uniqueContextId").is_some())),
            "{calls:?}"
        );
    }
    // A replaced document is read again, and a client-side route carries no status, as before.
    h.goto("/http-error/404").await;
    assert_eq!(h.observe().await.response_status, Some(404));
    eval_active(&h, "history.pushState({}, '', '/recovered')").await;
    let recovered = h.observe().await;
    assert!(recovered.url.ends_with("/recovered"));
    assert_eq!(recovered.response_status, None);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_the_viewport_a_measurement_read_is_not_asked_for_again() {
    let _turn = turn().await;
    let Some(connection) = chrome().cloned() else { return };
    let sent = Arc::new(Mutex::new(vec![]));
    let Some(h) = proxied(connection, recording(sent.clone())).await else { return };
    h.goto("/").await;
    let viewport_reads = |sent: &Arc<Mutex<Vec<(String, Value)>>>| {
        sent.lock()
            .unwrap()
            .iter()
            .filter(|(m, p)| {
                m == "Runtime.evaluate" && matches!(p["expression"].as_str(), Some("[innerWidth, innerHeight]"))
            })
            .count()
    };
    // A scroll lands where the viewport centre is, which the pre-action measurement already returned.
    sent.lock().unwrap().clear();
    let obs = h.observe().await;
    let scrolled = act(&h.page, Action::new(Operation::Scroll), &obs).await;
    assert_eq!(scrolled.outcome, StepOutcome::Executed);
    let wheel = sent
        .lock()
        .unwrap()
        .iter()
        .find(|(m, p)| m == "Input.dispatchMouseEvent" && p["type"] == "mouseWheel")
        .map(|(_, p)| p.clone())
        .expect("a wheel event");
    let (width, height) = (wheel["x"].as_f64().expect("x"), wheel["y"].as_f64().expect("y"));
    assert!(width > 0.0 && height > 0.0, "{wheel:?}");
    assert_eq!(viewport_reads(&sent), 0, "{:?}", sent.lock().unwrap().iter().map(|(m, _)| m).collect::<Vec<_>>());

    // Pointer marks need the viewport too, and a recording run has them on: the same measurement answers.
    let marks = Arc::new(Mutex::new(vec![]));
    let marked = CdpPage::new(h.session.clone(), Config::default()).with_pointer_marks(marks.clone());
    h.goto("/").await;
    let obs = marked.observe().await.expect("observe");
    sent.lock().unwrap().clear();
    let clicked = act(&marked, click(find(&obs, "Sign in")), &obs).await;
    assert_eq!(clicked.outcome, StepOutcome::Executed);
    assert_eq!(viewport_reads(&sent), 0, "a click re-read the viewport");
    assert_eq!(marks.lock().unwrap().len(), 1, "the click left no pointer mark");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_http_status_world_is_made_once_per_document() {
    let _turn = turn().await;
    let Some(connection) = chrome().cloned() else { return };
    let sent = Arc::new(Mutex::new(vec![]));
    let Some(h) = proxied(connection, recording(sent.clone())).await else { return };
    for (path, status) in [("", 200), ("/http-error/404", 404), ("", 200)] {
        h.goto(path).await;
        assert_eq!(h.observe().await.response_status, Some(status));
        sent.lock().unwrap().clear();
        assert_eq!(h.observe().await.response_status, Some(status));
        let calls = sent.lock().unwrap();
        assert!(!calls.iter().any(|(m, _)| m == "Page.createIsolatedWorld" || m == "Page.getFrameTree"), "{path:?}");
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_the_kept_status_world_is_addressed_only_by_its_unique_id() {
    let _turn = turn().await;
    let Some(connection) = chrome().cloned() else { return };
    let sent = Arc::new(Mutex::new(vec![]));
    let Some(h) = proxied(connection, recording(sent.clone())).await else { return };
    let world = |h: &Harness, session_id: &str| {
        testing::status_worlds(&h.page).into_iter().find(|(s, _)| s == session_id).map(|(_, w)| w)
    };
    h.goto("").await;
    assert_eq!(h.observe().await.response_status, Some(200));
    let session_id = active(&h);
    let kept = world(&h, &session_id).expect("a kept status world");
    // Chrome reuses context ids across renderer processes, so a kept context id could reach the page's own world,
    // where this forgery would be read; a unique id reaches the status world or nothing.
    eval_value(
        &h.session,
        &session_id,
        "Performance.prototype.getEntriesByType = performance.getEntriesByType = () => [{name: location.href, \
         responseStatus: 401}]; true",
    )
    .await;
    // The document's status is kept once read; dropped, it is read again in the kept world.
    testing::forget_document(&h.page);
    sent.lock().unwrap().clear();
    assert_eq!(h.observe().await.response_status, Some(200));
    let addressed: Vec<Value> = sent
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, p)| {
            m == "Runtime.evaluate" && (p.get("contextId").is_some() || p.get("uniqueContextId").is_some())
        })
        .map(|(_, p)| p.clone())
        .collect();
    assert_eq!(addressed.len(), 1, "{addressed:?}");
    assert_eq!(
        addressed,
        vec![json!({"expression": addressed[0]["expression"], "returnByValue": true, "awaitPromise": true,
            "uniqueContextId": kept})]
    );
    // A new document's world replaces the kept one, which then names nothing.
    h.goto("/http-error/404").await;
    assert_eq!(h.observe().await.response_status, Some(404));
    assert_ne!(world(&h, &session_id), Some(kept));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fields_carry_what_the_page_says_about_them() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("").await;
    let markup = concat!(
        "<form><label>First<input name=\"first\" required></label>",
        "<label>Last<input name=\"last\" aria-required=\"true\" aria-invalid=\"true\"></label>",
        "<label>Zip<input name=\"zip\" value=\"12345\"></label></form>",
    );
    let session_id = active(&h);
    let script = format!("document.body.innerHTML = {}", serde_json::to_string(markup).unwrap());
    h.page.evaluate(&session_id, &script).await.expect("evaluate");
    let observed = h.observe().await;
    let marks = |label: &str| (find(&observed, label).required, find(&observed, label).invalid);
    // Empty and required, but the browser holds that against it only after an edit or a submit; the page's own
    // aria-invalid stands as the page says it.
    assert_eq!(marks("First"), (Some(true), None));
    assert_eq!(marks("Last"), (Some(true), Some(true)));
    assert_eq!(marks("Zip"), (None, None));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_screenshots_match_the_png_frame_contract() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("").await;
    assert!(h.page.screenshot().await.expect("screenshot").starts_with(b"\x89PNG\r\n\x1a\n"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_icon_only_buttons_keep_their_named_glyphs() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h,
        concat!(
            "document.body.innerHTML = '",
            "<button><svg class=\"icon icon-tabler icon-tabler-plus\" aria-hidden=\"true\"></svg></button>",
            "<button aria-label=\"Remove item\"><svg class=\"lucide lucide-minus\" aria-hidden=\"true\"></svg></button>",
            "<button><svg class=\"lucide lucide-icon lucide-minus\" aria-hidden=\"true\"></svg></button>",
            "<button><svg class=\"icon icon-tabler icon-tabler-sm\" aria-hidden=\"true\"></svg></button>",
            "<button><svg class=\"tabler-icon tabler-icon-plus\"></svg></button>",
            "<button><svg class=\"lucide lucide-minus\" style=\"display:none\"></svg>",
            "<svg class=\"lucide lucide-plus\"></svg></button>",
            "<button><svg class=\"lucide lucide-minus\"></svg><svg class=\"lucide lucide-plus\"></svg></button>",
            "<button title=\"Add item\"><svg class=\"lucide lucide-plus\"></svg></button>",
            "<a href=\"/product\">Product<button><svg class=\"lucide lucide-heart\"></svg></button></a>';",
        ),
    )
    .await;
    let observation = observe_until(&h.page, "Remove item").await;
    assert_eq!(
        observation.controls.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
        vec![
            "plus icon",
            "Remove item",
            "minus icon",
            "button",
            "plus icon",
            "plus icon",
            "button",
            "Add item",
            "Product",
            "heart icon",
        ]
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_control_is_named_by_its_visible_content_only() {
    // AliExpress result cards hold a hidden "Report fraud item" button, so every card was offered under that name (#745).
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h,
        concat!(
            "document.body.innerHTML = '",
            "<a href=\"/item\"><button aria-label=\"Report fraud item\" style=\"display:none\"></button>",
            "<span>N52 disc magnet</span><span style=\"opacity:0\">See preview</span></a>",
            "<span id=\"draft\" hidden><b>Save</b></span><button aria-labelledby=\"draft\"></button>';",
        ),
    )
    .await;
    let observation = observe_until(&h.page, "N52 disc magnet").await;
    assert_eq!(
        observation.controls.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
        vec!["N52 disc magnet", "Save"]
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_button_wrapped_in_a_link_is_offered_once() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h,
        "document.body.innerHTML = '<a href=\"/icons.html\"><button type=\"button\">Continue</button></a>';",
    )
    .await;
    let observation = observe_until(&h.page, "Continue").await;
    let controls: Vec<&Control> = observation.controls.iter().filter(|c| c.label == "Continue").collect();
    assert_eq!(controls.len(), 1);
    let result = act(&h.page, click(controls[0]), &observation).await;
    assert_eq!(result.outcome, StepOutcome::Executed);
    assert!(h.observe().await.url.ends_with("/icons.html"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_hover_quote_context_does_not_attach_to_unrelated_or_reordered_content() {
    let _turn = turn().await;
    for mutation in ["outside", "siblings", "mixed", "label"] {
        let Some(h) = harness().await else { return };
        h.goto("/hovers.html").await;
        let session_id = active(&h);
        if mutation == "mixed" {
            let markup = concat!(
                "<style>.card .caption{display:none}.card:hover .caption{display:inline}</style>",
                "<span class=\"card\"><img alt=\"Avatar\" width=\"80\" height=\"80\">",
                "<span class=\"caption\">Revealed name</span></span>",
                "<span>Unrelated name</span>",
            );
            let script = format!("document.body.innerHTML = {}", serde_json::to_string(markup).unwrap());
            h.page.evaluate(&session_id, &script).await.expect("evaluate");
        }
        let observation = h.observe().await;
        let target =
            observation.controls.iter().find(|c| c.operations.contains(&Operation::Hover)).expect("hover target");
        act(&h.page, hover(target), &observation).await;
        h.observe().await;
        let script = match mutation {
            "outside" => "document.body.insertAdjacentHTML('beforeend', '<p>Outside name</p>')",
            "label" => "document.querySelector('.figure img').alt = 'Changed avatar'",
            "siblings" => "document.querySelector('.figure').parentNode.appendChild(document.createElement('section'))",
            _ => "undefined",
        };
        h.page.evaluate(&session_id, script).await.expect("evaluate");
        let captured = h.page.capture().await.expect("capture");
        if mutation == "outside" {
            let outside =
                captured.blocks.iter().find(|b| captured.block_text(b) == "Outside name").expect("outside block");
            assert!(outside.control_context.is_none(), "{mutation}");
        } else {
            assert!(captured.blocks.iter().all(|b| b.control_context.is_none()), "{mutation}");
        }
        if mutation == "mixed" {
            assert!(captured.text.contains("Revealed name") && captured.text.contains("Unrelated name"));
        }
        h.close().await;
    }
}

/// A tab opened beside headless Chrome's startup tab sometimes never presented a frame, so its screencast sent
/// nothing and a screenshot of it never returned.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_headless_run_tab_has_a_window_of_its_own_and_paints() {
    let _turn = turn().await;
    if find_chrome(None).is_none() {
        eprintln!("Chrome is not installed; skipping");
        return;
    }
    let (mut process, connection) = LocalChromeProcess::launch(&LocalChrome::default()).await.expect("Chrome launches");
    let session =
        BrowserSession::new(connection, Arc::new(RecordingArtifactSink::default()), SessionOptions::default());
    session.open().await.expect("session opens");
    let targets = session.send("Target.getTargets", json!({}), None).await.expect("targets");
    let pages: Vec<&str> = targets["targetInfos"]
        .as_array()
        .expect("target infos")
        .iter()
        .filter(|t| t["type"] == "page")
        .map(|t| t["targetId"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(pages, vec![session.active_target_id().as_str()]);
    let session_id = session.active_session_id().expect("active session");
    timeout(Duration::from_secs(5), session.send("Page.captureScreenshot", json!({}), Some(&session_id)))
        .await
        .expect("in time")
        .expect("screenshot");
    session.close().await.expect("close");
    process.close().await;
}

/// A list's next page is marked by the page's own pager markup: `rel="next"`, or a `next` class on the link or on its
/// list item, which most sites use instead. The word "next" in prose, or a class that only starts with it, is not.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_next_page_is_marked_by_rel_or_a_next_class_in_pagination_and_not_by_the_word() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/pager.html").await;
    let obs = h.observe().await;
    let marked = |label: &str| {
        let exact = obs.controls.iter().find(|c| c.label == label);
        exact.unwrap_or_else(|| find(&obs, label)).next_page == Some(true)
    };
    assert!(marked("Older"), "rel=next");
    assert!(marked("Next"), "a link inside li.next");
    assert!(marked("More"), "a link with the next class in a pagination container");
    assert!(!marked("Next slide"), "a next class in a carousel");
    assert!(!marked("Read on"), "li.next outside any pagination container");
    assert!(!marked("Elsewhere"), "a next link to another site");
    assert!(!marked("Previous"));
    assert!(!marked("Next article"), "a class that only starts with next");
    assert!(!marked("Next steps"), "the word alone");
    h.close().await;
}
