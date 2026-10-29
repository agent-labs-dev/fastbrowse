mod common;

// End-to-end tests of the browser layer against a real headless Chrome and fixture sites: tests/browser/test_browser.py.
//
// Where the Python tests replace a session's handler or method (`_prepare_session`, `_on_target_created`,
// `send_raw`, `_before_action`), these put a websocket proxy between the session and Chrome that holds back,
// rewrites, drops or records the CDP messages that code would have seen, so the library runs unchanged.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Harness, RecordingArtifactSink, chrome, harness, harness_on, sites};
use data_encoding::HEXLOWER;
use fastbrowse_browser::adapters::local_chrome::{LocalChromeProcess, find_chrome};
use fastbrowse_browser::error::BrowserError;
use fastbrowse_browser::page::CdpPage;
use fastbrowse_browser::session::{BrowserSession, SessionOptions};
use fastbrowse_browser::testing;
use fastbrowse_core::config::{Config, ObservationLimits};
use fastbrowse_core::models::{Attachment, BrowserConnection, Bytes, LocalChrome, Operation, StepOutcome};
use fastbrowse_core::page::{ActResult, Action, BlockKind, Capture, Control, Observation};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep, timeout};

// -- helpers ----------------------------------------------------------------------------------------------------

fn find<'a>(observation: &'a Observation, label: &str) -> &'a Control {
    observation.controls.iter().find(|c| c.label.contains(label)).unwrap_or_else(|| {
        panic!(
            "no control labelled like {label:?} in {:?}",
            observation.controls.iter().map(|c| &c.label).collect::<Vec<_>>()
        )
    })
}

async fn wait_until<F, Fut>(predicate: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    wait_until_for(predicate, 5.0).await;
}

async fn wait_until_for<F, Fut>(mut predicate: F, timeout_seconds: f64)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_seconds);
    while Instant::now() < deadline {
        if predicate().await {
            return;
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("condition was not met in time");
}

/// Observe until a control is actually there: an attached frame session predates its document.
async fn observe_until(page: &CdpPage, label: &str) -> Observation {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let observation = page.observe().await.expect("observe");
        if observation.controls.iter().any(|c| c.label.contains(label)) {
            return observation;
        }
        assert!(Instant::now() < deadline, "condition was not met in time");
        sleep(Duration::from_millis(50)).await;
    }
}

async fn eval_value(session: &BrowserSession, session_id: &str, expression: &str) -> Value {
    let out = session
        .send("Runtime.evaluate", json!({"expression": expression, "returnByValue": true}), Some(session_id))
        .await
        .expect("Runtime.evaluate");
    out["result"].get("value").cloned().unwrap_or(Value::Null)
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

/// `capture.text[start:end]`: block offsets count code points, as Python indexes a `str`.
fn slice(capture: &Capture, start: i64, end: i64) -> String {
    capture.text.chars().skip(start.max(0) as usize).take((end - start).max(0) as usize).collect()
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

/// One test at a time, as the Python suite runs. Tests share one Chrome, and with even two tabs at work a menu shown
/// on the next animation frame was read before it drew and a plain click outlasted its two-second bound.
async fn gate() -> tokio::sync::SemaphorePermit<'static> {
    static GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
    GATE.acquire().await.expect("the gate is never closed")
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
                Some((value, delay)) if delay.is_zero() => {
                    let _ = to.send(Message::text(value.to_string()));
                }
                Some((value, delay)) => {
                    let to = to.clone();
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
    let _gate = gate().await;
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
async fn test_observe_lists_controls_and_masks_password() {
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
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
async fn test_covered_click_dispatches_nothing() {
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let landed = h.observe().await;
    assert!(landed.viewport_text.contains("139.79"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cross_origin_iframe_control_is_observed_and_clickable() {
    let _gate = gate().await;
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
    let _gate = gate().await;
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
async fn test_a_new_tab_slow_to_set_up_is_still_followed() {
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
    let Some(h) = loaded().await else { return };
    let s = &h.session;
    wait_until(move || async move { !s.frame_sessions().is_empty() }).await;
    let frame_session = h.session.frame_sessions()[0].1.clone();
    let main = &sites().main;
    eval_value(
        &h.session,
        &frame_session,
        &format!(
            "document.body.insertAdjacentHTML('beforeend', '<a href=\"{main}/popup.html\" target=\"_blank\">Frame \
             window link</a>')"
        ),
    )
    .await;
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
    let _gate = gate().await;
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

#[tokio::test(flavor = "multi_thread")]
async fn test_unnamed_card_controls_are_named_by_their_icon_and_card() {
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
    let Some(h) = loaded().await else { return };
    let obs = h.observe().await;
    let result = act(&h.page, click(find(&obs, "Shadow button")), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Executed);

    assert_eq!(eval_active(&h, "window.__shadowClicked === true").await, json!(true));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_capture_offsets_slice_exactly_to_each_block() {
    let _gate = gate().await;
    let Some(h) = loaded().await else { return };
    let capture = h.page.capture().await.expect("capture");
    assert!(!capture.blocks.is_empty(), "expected at least one block");
    for block in &capture.blocks {
        assert!(!slice(&capture, block.start, block.end).is_empty(), "block {} sliced to empty text", block.source_id);
    }
    let headings: Vec<_> = capture.blocks.iter().filter(|b| b.kind == BlockKind::Heading).collect();
    assert!(headings.iter().any(|h| slice(&capture, h.start, h.end) == "Table"));
    let tables: Vec<_> = capture.blocks.iter().filter(|b| b.kind == BlockKind::Table).collect();
    assert!(!tables.is_empty());
    assert!(slice(&capture, tables[0].start, tables[0].end).contains("Ada"));
    let mut links = capture.blocks.iter().filter(|b| b.kind == BlockKind::Link);
    assert!(links.any(|link| link.href.as_deref() == Some("https://example.com/docs")));
    h.close().await;
}

/// Wikipedia's search box swaps itself for a hydrated copy mid-insertion; the text still landed.
#[tokio::test(flavor = "multi_thread")]
async fn test_fill_succeeds_when_the_field_is_replaced_while_typing() {
    let _gate = gate().await;
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
    let _gate = gate().await;
    let Some(h) = loaded().await else { return };
    let obs = h.observe().await;
    let result = act(&h.page, fill(find(&obs, "Rejecting field"), "godel"), &obs).await;
    assert_eq!(result.outcome, StepOutcome::Failed);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_observation_limits_can_exceed_old_javascript_caps() {
    let _gate = gate().await;
    let Some(h) = loaded().await else { return };
    eval_active(
        &h,
        "document.body.replaceChildren(); const text = document.createElement('div'); text.textContent = \
         'x'.repeat(8000); document.body.append(text); for (let i = 0; i < 60; i++) { const b = \
         document.createElement('button'); b.textContent = 'Button ' + i; b.style.cssText = \
         'position:absolute;left:20px;top:' + (10000+i*40) + 'px'; document.body.append(b); }",
    )
    .await;
    let default = h.observe().await;
    assert!(default.controls.len() == 60 && default.omitted_controls == 0);
    assert_eq!(
        default.viewport_text,
        format!("{}\n[Viewport text cut: 2000 characters omitted; read the page for the rest]", "x".repeat(6000))
    );

    let limits = ObservationLimits {
        max_controls: 100,
        max_offscreen_controls: 55,
        viewport_text_chars: 7500,
        ..Default::default()
    };
    let page = CdpPage::new(h.session.clone(), Config { observation: limits, ..Config::default() });
    let expanded = page.observe().await.expect("observe");
    assert!(expanded.controls.len() == 55 && expanded.omitted_controls == 5);
    assert_eq!(
        expanded.viewport_text,
        format!("{}\n[Viewport text cut: 500 characters omitted; read the page for the rest]", "x".repeat(7500))
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_page_exception_is_typed_and_does_not_echo_page_text() {
    let _gate = gate().await;
    let Some(h) = loaded().await else { return };
    eval_active(&h, "Object.defineProperty(document, 'title', {get() { throw new Error('echoed-secret'); }});").await;
    let raised = h.page.observe().await.expect_err("the title getter throws");
    assert!(raised.to_string().contains("Runtime.evaluate failed (JavaScriptError)"), "{raised:?}");
    assert!(!raised.to_string().contains("echoed-secret"));
    h.close().await;
}

/// Unassociated sibling labels left todo toggles absent from the choices offered to the policy.
#[tokio::test(flavor = "multi_thread")]
async fn test_transparent_native_choices_keep_todo_context_and_change_only_state() {
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
    let Some(h) = harness().await else { return };
    h.goto("/todos.html").await;
    eval_active(&h, "document.querySelectorAll('input')[1].parentElement.style.display = 'contents'").await;
    assert!(h.observe().await.controls.iter().any(|c| c.context.as_deref() == Some("walk the dog")));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_styled_choice_uses_visible_label_and_native_state() {
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
    let Some(h) = harness().await else { return };
    h.goto("/frames.html").await;
    // A frameset document has no text of its own: everything a user sees is in its frames.
    assert!(h.observe().await.viewport_text.contains("Left pane"));
    let text = h.page.capture().await.expect("capture").text;
    assert!(text.contains("Left pane"));
    assert!(text.contains("Right pane"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_table_cell_drawn_with_an_icon_is_not_read_as_blank() {
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let revealed = captured
        .blocks
        .iter()
        .find(|b| slice(&captured, b.start, b.end).contains("name: grace"))
        .expect("revealed block");
    let context = revealed.control_context.as_ref().expect("control context");
    assert_eq!(context.label, "User Avatar");
    assert_eq!(context.context.as_deref(), Some("2 of 2"));
    assert!(
        captured
            .blocks
            .iter()
            .filter(|b| slice(&captured, b.start, b.end) == "Team")
            .all(|b| b.control_context.is_none())
    );
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

#[tokio::test(flavor = "multi_thread")]
async fn test_a_visible_option_is_clicked_without_scrolling_its_menu_shut() {
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
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

/// A post's heading ("Data Entry Form") named a date picker's Submit, and the verifier read the wrong form.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_control_is_named_by_the_title_nearest_before_it_not_the_posts_first_heading() {
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
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

/// A jQuery UI datepicker's Next and days were named "Su", its weekday header, so every month read alike: the
/// run could not tell which month it was on and went back and forth past the one it wanted.
#[tokio::test(flavor = "multi_thread")]
async fn test_calendar_twins_are_named_by_the_month_they_show() {
    let _gate = gate().await;
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

#[tokio::test(flavor = "multi_thread")]
async fn test_table_controls_keep_columns_across_empty_cells_and_spans() {
    let _gate = gate().await;
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
    assert_eq!(find(&obs, "2").context.as_deref(), Some("column: October 2026 / Friday"));
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
    let _gate = gate().await;
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

/// A run in a visible window clicks, types and captures behind the window's current tab, and activates nothing.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_background_run_drives_its_tab_without_bringing_it_forward() {
    let _gate = gate().await;
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
    let _gate = gate().await;
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

#[tokio::test(flavor = "multi_thread")]
async fn test_download_evidence_is_bounded_and_cites_the_response() {
    let _gate = gate().await;
    // The Python test quotes the download through retrieval's `chunk` and `_cited`, which are not ported; with one
    // chunk holding the whole capture, the quote is the block's text, its url the block's source or the capture's,
    // and a run across frames or sources cites nothing, so those are asserted on the block itself.
    for path in ["/report.csv", "/export-report", "/report.csv?short", "/report.csv?partial"] {
        let Some(h) = harness().await else { return };
        h.goto("").await;
        h.goto(path).await;
        let capture = h.page.capture().await.expect("capture");
        assert!(capture.text.chars().count() < 70000, "{path}");
        let block = capture.blocks.iter().find(|b| b.source_id.starts_with("download/")).expect("download block");
        assert_eq!(block.kind, BlockKind::Table, "{path}");
        let url = block.source_url.as_deref().unwrap_or(&capture.url);
        assert_eq!(url, format!("{}{path}", sites().main));
        let quote = slice(&capture, block.start, block.end);
        assert!(quote.contains("name,total"), "{path}");
        assert!(quote.contains("139.79"), "{path}");
        assert_eq!(quote.contains("omitted"), ["/report.csv", "/export-report"].contains(&path), "{path}");
        assert_eq!(block.complete_source, path == "/report.csv?short", "{path}");
        let first = &capture.blocks[0];
        assert_ne!(
            (first.frame_id.as_deref(), first.source_url.as_deref().unwrap_or(&capture.url)),
            (block.frame_id.as_deref(), url),
            "{path}"
        );
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_http_error_status_travels_with_the_observed_document() {
    let _gate = gate().await;
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
    let _gate = gate().await;
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
    let _gate = gate().await;
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
async fn test_http_status_world_is_made_once_per_document() {
    let _gate = gate().await;
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
    let _gate = gate().await;
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
async fn test_required_radios_use_their_form_group_validity() {
    let _gate = gate().await;
    let Some(h) = harness().await else { return };
    h.goto("").await;
    let markup = concat!(
        "<form><label>First<input type=\"radio\" name=\"choice\" required checked></label>",
        "<label>Second<input type=\"radio\" name=\"choice\" required></label>",
        "<label>Terms<input type=\"checkbox\" required></label></form>",
        "<form><label>Other form<input type=\"radio\" name=\"choice\" required></label></form>",
    );
    let session_id = active(&h);
    let script = format!("document.body.innerHTML = {}", serde_json::to_string(markup).unwrap());
    h.page.evaluate(&session_id, &script).await.expect("evaluate");
    let observed = h.observe().await;
    let blocking = |obs: &Observation, label: &str| find(obs, label).blocking == Some(true);
    assert!(!blocking(&observed, "First"));
    assert!(!blocking(&observed, "Second"));
    assert!(blocking(&observed, "Terms"));
    assert!(blocking(&observed, "Other form"));
    h.page.evaluate(&session_id, "document.querySelector('input').checked = false").await.expect("evaluate");
    let observed = h.observe().await;
    assert!(blocking(&observed, "First"));
    assert!(blocking(&observed, "Second"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_screenshots_match_the_png_frame_contract() {
    let _gate = gate().await;
    let Some(h) = harness().await else { return };
    h.goto("").await;
    assert!(h.page.screenshot().await.expect("screenshot").starts_with(b"\x89PNG\r\n\x1a\n"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_icon_only_buttons_keep_their_named_glyphs() {
    let _gate = gate().await;
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
async fn test_a_button_wrapped_in_a_link_is_offered_once() {
    let _gate = gate().await;
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
    let _gate = gate().await;
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
            let outside = captured
                .blocks
                .iter()
                .find(|b| slice(&captured, b.start, b.end) == "Outside name")
                .expect("outside block");
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
    let _gate = gate().await;
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
