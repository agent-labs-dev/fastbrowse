mod common;
mod misc_support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{
    Harness, chrome, eval_value, find, harness, harness_on, harness_with, js, observe_until, sites, try_eval,
    wait_until,
};
use fastbrowse_browser::error::BrowserError;
use fastbrowse_browser::session::SessionOptions;
use fastbrowse_browser::testing::misc::rereads;
use fastbrowse_core::config::Config;
use fastbrowse_core::models::{BrowserConnection, Operation, StepOutcome};
use fastbrowse_core::page::Action;
use relay::{Relay, Verdict};
use serde_json::{Value, json};
use tokio::time::timeout;

/// A DevTools proxy between a session and the shared Chrome, standing in for the Python tests' monkeypatches of
/// `page.navigate`, `page._evaluate`, `page._history` and `page._snapshot_all_frames`: a rule sees each command the
/// session sends and forwards it, answers it in Chrome's place, holds it, or cuts the connection.
mod relay {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use fastbrowse_core::BoxFuture;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{mpsc, oneshot};
    use tokio_tungstenite::tungstenite::Message;

    /// Ids the relay gives its own commands: far above any the session's client reaches, within the 32-bit integer
    /// Chrome requires of an id.
    const OWN_IDS: u64 = 1 << 30;

    pub enum Verdict {
        Forward,
        /// Chrome's reply in its place: `{"result": ...}` or `{"error": ...}`.
        Answer(Value),
        /// The reply this work returns, once it is done.
        Later(BoxFuture<'static, Value>),
        /// The browser goes away mid-command.
        Cut,
    }

    pub type Rule = Arc<dyn Fn(&Value, &Relay) -> Verdict + Send + Sync>;

    #[derive(Clone)]
    pub struct Relay {
        chrome: mpsc::UnboundedSender<Message>,
        waiting: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
        next: Arc<AtomicU64>,
    }

    impl Relay {
        /// Send `command` to Chrome as the relay's own and return Chrome's reply.
        pub async fn call(&self, command: &Value) -> Value {
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            let mut command = command.clone();
            command["id"] = id.into();
            let (tx, rx) = oneshot::channel();
            self.waiting.lock().unwrap().insert(id, tx);
            let _ = self.chrome.send(Message::text(command.to_string()));
            rx.await.unwrap_or(Value::Null)
        }
    }

    /// Listen on a local port and return the DevTools address that reaches Chrome through `rule`.
    pub async fn start(upstream: String, rule: Rule) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, upstream.clone(), rule.clone()));
            }
        });
        format!("ws://{address}/devtools/browser/relay")
    }

    async fn serve(stream: TcpStream, upstream: String, rule: Rule) {
        let Ok(client) = tokio_tungstenite::accept_async(stream).await else { return };
        let Ok((chrome, _)) = tokio_tungstenite::connect_async(upstream.as_str()).await else { return };
        let (mut client_sink, mut client_stream) = client.split();
        let (mut chrome_sink, mut chrome_stream) = chrome.split();
        let (to_client, mut client_out) = mpsc::unbounded_channel::<Message>();
        let (to_chrome, mut chrome_out) = mpsc::unbounded_channel::<Message>();
        tokio::spawn(async move {
            while let Some(message) = client_out.recv().await {
                let close = matches!(message, Message::Close(_));
                if client_sink.send(message).await.is_err() || close {
                    break;
                }
            }
            let _ = client_sink.close().await;
        });
        tokio::spawn(async move {
            while let Some(message) = chrome_out.recv().await {
                let close = matches!(message, Message::Close(_));
                if chrome_sink.send(message).await.is_err() || close {
                    break;
                }
            }
            let _ = chrome_sink.close().await;
        });
        let relay =
            Relay { chrome: to_chrome.clone(), waiting: Arc::default(), next: Arc::new(AtomicU64::new(OWN_IDS)) };
        let (waiting, back) = (relay.waiting.clone(), to_client.clone());
        tokio::spawn(async move {
            while let Some(Ok(message)) = chrome_stream.next().await {
                let Message::Text(text) = &message else { continue };
                if let Ok(reply) = serde_json::from_str::<Value>(text.as_str())
                    && let Some(id) = reply["id"].as_u64().filter(|id| *id >= OWN_IDS)
                {
                    if let Some(tx) = waiting.lock().unwrap().remove(&id) {
                        let _ = tx.send(reply);
                    }
                    continue;
                }
                let _ = back.send(message);
            }
            let _ = back.send(Message::Close(None));
        });
        while let Some(Ok(message)) = client_stream.next().await {
            let Message::Text(text) = &message else { continue };
            let Ok(command) = serde_json::from_str::<Value>(text.as_str()) else { continue };
            let reply_to = |mut reply: Value| {
                reply["id"] = command["id"].clone();
                if let Some(session) = command.get("sessionId") {
                    reply["sessionId"] = session.clone();
                }
                Message::text(reply.to_string())
            };
            match rule(&command, &relay) {
                Verdict::Forward => {
                    let _ = to_chrome.send(message);
                }
                Verdict::Answer(reply) => {
                    let _ = to_client.send(reply_to(reply));
                }
                Verdict::Later(work) => {
                    let (to_client, id) = (to_client.clone(), command["id"].clone());
                    tokio::spawn(async move {
                        // Chrome answered the relay's own id; the session waits on its own.
                        let mut reply = work.await;
                        reply["id"] = id;
                        let _ = to_client.send(Message::text(reply.to_string()));
                    });
                }
                Verdict::Cut => {
                    let _ = to_client.send(Message::Close(None));
                    let _ = to_chrome.send(Message::Close(None));
                    return;
                }
            }
        }
    }
}

/// A session on the shared Chrome whose every command passes `rule` first; None without Chrome.
async fn relayed(rule: impl Fn(&Value, &Relay) -> Verdict + Send + Sync + 'static) -> Option<Harness> {
    let connection = chrome()?.clone();
    let cdp_url = relay::start(connection.cdp_url.clone(), Arc::new(rule)).await;
    harness_on(BrowserConnection { cdp_url, ..connection }, Config::default(), SessionOptions::default()).await
}

fn method(command: &Value) -> &str {
    command["method"].as_str().unwrap_or_default()
}

fn expression(command: &Value) -> &str {
    command["params"]["expression"].as_str().unwrap_or_default()
}

fn action(operation: Operation) -> Action {
    Action::new(operation)
}

fn navigate_to(url: &str) -> Action {
    Action { url: Some(url.to_owned()), ..Action::new(Operation::Navigate) }
}

fn click(target_id: &str) -> Action {
    Action { target_id: Some(target_id.to_owned()), ..Action::new(Operation::Click) }
}

// -- test_navigation.py: directed navigation to a caller-supplied address, through the real browser -------------

#[tokio::test(flavor = "multi_thread")]
async fn test_navigate_crosses_origins_and_reports_the_landing() {
    let Some(h) = harness().await else { return };
    h.goto("/").await;
    let before = h.observe().await;
    let target = format!("{}/iframe.html", sites().iframe);
    let result = h.page.act(&navigate_to(&target), &before).await.expect("act");
    assert!(result.outcome == StepOutcome::Executed && result.page_changed);
    let after = h.observe().await;
    assert_eq!(h.page.address().await.unwrap(), target);
    assert_eq!(after.url, target);
    assert_ne!(after.page_key, before.page_key);
    assert!(h.page.document_changed(&before).await.unwrap());
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_navigate_to_the_current_address_changes_nothing() {
    let Some(h) = harness().await else { return };
    let here = format!("{}/index.html", sites().main);
    h.page.navigate(&here, None).await.expect("navigate");
    let before = h.observe().await;
    let result = h.page.act(&navigate_to(&here), &before).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Executed);
    assert_eq!(h.page.address().await.unwrap(), here);
    find(&h.observe().await, "Confirm");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_navigate_refuses_a_missing_or_non_http_address() {
    for url in [None, Some("javascript:alert(1)"), Some("file:///etc/passwd"), Some("data:text/html,hi")] {
        let Some(h) = harness().await else { return };
        h.goto("/").await;
        let before = h.observe().await;
        let navigate = Action { url: url.map(str::to_owned), ..Action::new(Operation::Navigate) };
        let result = h.page.act(&navigate, &before).await.expect("act");
        assert!(result.outcome == StepOutcome::Failed && !result.page_changed, "{url:?}");
        assert_eq!(h.page.address().await.unwrap(), format!("{}/", sites().main), "{url:?}");
        h.close().await;
    }
}

async fn navigate_from_a_dialog(h: &Harness) -> (String, StepOutcome, bool) {
    h.goto("/").await;
    let obs = h.observe().await;
    h.page.act(&click(&find(&obs, "Confirm").id), &obs).await.expect("click");
    let session = &h.session;
    wait_until(|| async move { session.pending_dialog().is_some() }, 5.0).await;
    let blocked = h.observe().await;
    assert!(blocked.dialog.is_some());
    let target = format!("{}/iframe.html", sites().iframe);
    let result = timeout(Duration::from_secs(30), h.page.act(&navigate_to(&target), &blocked))
        .await
        .expect("in time")
        .expect("act");
    (target, result.outcome, result.page_changed)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_navigate_from_a_pending_dialog_lands_on_the_address() {
    let Some(h) = harness().await else { return };
    let (target, outcome, changed) = navigate_from_a_dialog(&h).await;
    assert!(outcome == StepOutcome::Executed && changed);
    assert_eq!(timeout(Duration::from_secs(10), h.page.address()).await.expect("in time").unwrap(), target);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_navigating_away_clears_the_dialog_of_the_old_document() {
    let Some(h) = harness().await else { return };
    navigate_from_a_dialog(&h).await;
    assert!(h.session.pending_dialog().is_none());
    assert!(timeout(Duration::from_secs(10), h.page.observe()).await.expect("in time").unwrap().dialog.is_none());
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_navigation_cannot_leave_the_callers_origin_grant() {
    let Some(connection) = chrome() else { return };
    // test_safety.py's `scoped`: a session granted the main site's origin alone.
    let scoped = BrowserConnection { allowed_origins: Some(vec![sites().main.clone()]), ..connection.clone() };
    let Some(h) = harness_on(scoped, Config::default(), SessionOptions::default()).await else { return };
    h.goto("/dispatch.html").await;
    let before = h.observe().await;
    let refused = h.page.act(&navigate_to(&format!("{}/iframe.html", sites().iframe)), &before).await;
    assert_eq!(refused, Err(BrowserError::OriginNotAllowed));
    assert_eq!(h.page.address().await.unwrap(), before.url);
    h.close().await;
}

/// `monkeypatch.setattr(page, "navigate", broken)`: Chrome's answer to `Page.navigate` is replaced, once armed, by
/// a navigation that failed with `error_text`.
async fn failed_navigation(error_text: &'static str, expected: &str) {
    let armed = Arc::new(AtomicBool::new(false));
    let rule_armed = armed.clone();
    let Some(h) = relayed(move |command, _| {
        if rule_armed.load(Ordering::SeqCst) && method(command) == "Page.navigate" {
            return Verdict::Answer(json!({"result": {"frameId": "", "errorText": error_text}}));
        }
        Verdict::Forward
    })
    .await
    else {
        return;
    };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    let before = h.observe().await;
    armed.store(true, Ordering::SeqCst);
    let result = h.page.act(&navigate_to(&format!("{}/missing", sites().main)), &before).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Failed);
    assert!(result.detail.as_deref().unwrap_or_default().contains(expected), "{:?}", result.detail);
    assert_eq!(h.page.address().await.unwrap(), before.url);
    h.close().await;
}

// The Python test fakes each failure type with one message. Here each comes from the real path, which names only
// Chrome's error: a refused connection is `SiteUnreachable`, any other net error a plain `BrowserError`. The
// `timeout` case is skipped: a real `NavigationTimeout` takes four 15-second load waits.
#[tokio::test(flavor = "multi_thread")]
async fn test_failed_navigation_is_a_recoverable_step_unreachable() {
    failed_navigation("net::ERR_CONNECTION_REFUSED", "ERR_CONNECTION_REFUSED").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_failed_navigation_is_a_recoverable_step_browser() {
    failed_navigation("net::ERR_CERT_AUTHORITY_INVALID", "ERR_CERT_AUTHORITY_INVALID").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_navigation_does_not_swallow_a_lost_browser() {
    let armed = Arc::new(AtomicBool::new(false));
    let rule_armed = armed.clone();
    let Some(h) = relayed(move |command, _| {
        if rule_armed.load(Ordering::SeqCst) && method(command) == "Page.navigate" {
            return Verdict::Cut;
        }
        Verdict::Forward
    })
    .await
    else {
        return;
    };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    let before = h.observe().await;
    armed.store(true, Ordering::SeqCst);
    // The browser going mid-command is `BrowserUnresponsive`, an `Unavailable` as Python's `BrowserUnavailable` is.
    let error = h.page.act(&navigate_to(&format!("{}/other", sites().main)), &before).await.expect_err("raises");
    assert!(error.is_unavailable(), "{error:?}");
    h.close().await;
}

/// The run's lineage table (`Agent._note_lineage`) rests on these: a navigation and pushState make an entry, a
/// reload and replaceState keep it, and Back and Forward return to the entries they left.
#[tokio::test(flavor = "multi_thread")]
async fn test_the_history_entry_a_page_is_follows_the_browser_not_the_address() {
    let Some(h) = harness().await else { return };
    let session_id = h.session.active_session_id().unwrap();
    let (page, session, sid) = (&h.page, &h.session, session_id.as_str());
    let entry = || async move { page.observe().await.expect("observe").history_entry };

    h.page.navigate(&sites().main, None).await.expect("navigate");
    let first = entry().await;
    h.goto("/dispatch.html").await;
    let second = entry().await;
    assert!(first.is_some() && second.is_some() && first != second);
    // settled("location.reload()"); a read across the reload fails until the new document answers.
    eval_value(session, sid, "window.marker = 1; location.reload()").await;
    wait_until(
        || async move { try_eval(session, sid, "window.marker === undefined").await.is_ok_and(|v| v == true) },
        5.0,
    )
    .await;
    assert_eq!(entry().await, second);
    eval_value(session, sid, "history.pushState({}, '', '/dispatch.html?pushed')").await;
    let pushed = entry().await;
    assert!(pushed != first && pushed != second);
    eval_value(session, sid, "history.replaceState({}, '', '/dispatch.html?replaced')").await;
    assert_eq!(entry().await, pushed);
    let observed = h.observe().await;
    assert_eq!(h.page.act(&action(Operation::Back), &observed).await.unwrap().outcome, StepOutcome::Executed);
    let second = &second;
    wait_until(|| async move { entry().await == *second }, 5.0).await;
    eval_value(session, sid, "history.forward()").await;
    let pushed = &pushed;
    wait_until(|| async move { entry().await == *pushed }, 5.0).await;
    h.close().await;
}

/// History and the snapshot are read at once. A pushState landing between them paired the new page with the old
/// entry, which then took the new page's lineage. The pair is read again until both name one address, and a pair
/// that never agrees names no entry.
async fn push_state_between_history_and_snapshot(pushes: u32) {
    let armed = Arc::new(AtomicBool::new(false));
    let tab: Arc<Mutex<String>> = Arc::default();
    let snapped = Arc::new(tokio::sync::Notify::new());
    let pushed = Arc::new(AtomicU32::new(0));
    let (rule_armed, rule_tab) = (armed.clone(), tab.clone());
    let Some(h) = relayed(move |command, relay| {
        if !rule_armed.load(Ordering::SeqCst) || command["sessionId"].as_str() != Some(&*rule_tab.lock().unwrap()) {
            return Verdict::Forward;
        }
        let (command, relay, snapped) = (command.clone(), relay.clone(), snapped.clone());
        if method(&command) == "Runtime.evaluate" && expression(&command).contains(")('snapshot')") {
            // snapshot_first: the snapshot's reply is let through before history is read.
            return Verdict::Later(Box::pin(async move {
                let reply = relay.call(&command).await;
                snapped.notify_one();
                reply
            }));
        }
        if method(&command) == "Page.getNavigationHistory" {
            // history_after_a_push.
            let pushed = pushed.clone();
            return Verdict::Later(Box::pin(async move {
                snapped.notified().await;
                if pushed.load(Ordering::SeqCst) < pushes {
                    let step = pushed.fetch_add(1, Ordering::SeqCst) + 1;
                    let push = format!("history.pushState({{}}, '', '/dispatch.html?step={step}')");
                    relay
                        .call(&json!({"method": "Runtime.evaluate", "sessionId": command["sessionId"],
                            "params": {"expression": push, "returnByValue": true}}))
                        .await;
                }
                relay.call(&command).await
            }));
        }
        Verdict::Forward
    })
    .await
    else {
        return;
    };
    *tab.lock().unwrap() = h.session.active_session_id().unwrap();
    h.goto("/dispatch.html").await;
    armed.store(true, Ordering::SeqCst);
    let observed = h.observe().await;
    armed.store(false, Ordering::SeqCst);
    if pushes == 1 {
        // Read again once the push had landed: the page and its entry agree.
        assert!(observed.url.ends_with("/dispatch.html?step=1"), "{}", observed.url);
        assert!(observed.history_entry.is_some());
        assert_eq!(observed.history_entry, h.observe().await.history_entry);
    } else {
        assert!(observed.history_entry.is_none());
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_push_state_between_history_and_snapshot_never_pairs_a_page_with_another_entry() {
    push_state_between_history_and_snapshot(1).await;
    push_state_between_history_and_snapshot(10).await;
}

/// A guessed page that rewrote its own address on a timer just after a click was taken for the site answering the
/// click, and the guess was cleared. Only a new document after the action, or an address its own input event
/// changed, is a move the action made.
#[tokio::test(flavor = "multi_thread")]
async fn test_an_action_reports_only_the_moves_it_made_itself() {
    let cases = [
        // A form submitted to the same path loads a new document after the click.
        ("this.form.submit()", true),
        // A single-page app answering the click in its own task, its promise reactions included.
        ("history.pushState({}, '', '?q=corrected')", true),
        ("Promise.resolve().then(() => history.pushState({}, '', '?q=corrected'))", true),
        ("history.replaceState({}, '', '?q=corrected')", true),
        // A rewrite the page makes later, on a timer or after a fetch, cannot be tied to the click.
        ("setTimeout(() => history.pushState({}, '', '?q=later'), 40)", false),
        ("", false),
    ];
    for (handler, moved) in cases {
        let Some(h) = harness().await else { return };
        h.goto("/?q=guess").await;
        let session_id = h.session.active_session_id().unwrap();
        let form = format!(
            r#"<form action="/" method=get><input name=q value=corrected><button type=button onclick="{handler}">Search</button></form>"#
        );
        assert_eq!(
            eval_value(&h.session, &session_id, &format!("document.body.innerHTML = {}; true", js(&form))).await,
            true
        );
        let obs = h.observe().await;
        let result = h.page.act(&click(&find(&obs, "Search").id), &obs).await.expect("act");
        assert_eq!(result.outcome, StepOutcome::Executed, "{handler}");
        assert_eq!(result.moved, moved, "{handler}");
        h.close().await;
    }
}

async fn accept_dialog(h: &Harness) -> fastbrowse_core::page::ActResult {
    let session = &h.session;
    wait_until(|| async move { session.pending_dialog().is_some() }, 5.0).await;
    let dialog = h.observe().await;
    let accept = Action { accept_dialog: Some(true), ..Action::new(Operation::Dialog) };
    timeout(Duration::from_secs(10), h.page.act(&accept, &dialog)).await.expect("in time").expect("act")
}

/// A dialog blocks the page, so no mark of its document could be taken before accepting it, and a confirm that
/// navigated once accepted was never reported as the run's move.
#[tokio::test(flavor = "multi_thread")]
async fn test_accepting_a_dialog_that_navigates_is_a_move_the_run_made() {
    let Some(h) = harness().await else { return };
    h.goto("/?q=guess").await;
    let session_id = h.session.active_session_id().unwrap();
    let form = "<button onclick=\"if (confirm('Search again?')) location.href = '/inner.html'\">Search</button>";
    assert_eq!(
        eval_value(&h.session, &session_id, &format!("document.body.innerHTML = {}; true", js(form))).await,
        true
    );
    let obs = h.observe().await;
    let clicked = h.page.act(&click(&find(&obs, "Search").id), &obs).await.expect("act");
    assert_eq!(clicked.outcome, StepOutcome::Executed);
    let accepted = accept_dialog(&h).await;
    assert_eq!(accepted.outcome, StepOutcome::Executed);
    assert!(accepted.moved);
    h.close().await;
}

/// The page moved its own address on the same document after the run observed it. The dialog the click opened
/// carried the new address, so a mark taken against the observed one no longer matched and the accepted confirm's
/// navigation was not reported as the run's move.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_confirm_after_a_same_document_navigation_is_still_the_runs_move() {
    let Some(h) = harness().await else { return };
    h.goto("/?q=guess").await;
    let session_id = h.session.active_session_id().unwrap();
    let form = "<button onclick=\"if (confirm('Search again?')) location.href = '/inner.html'\">Search</button>";
    assert_eq!(
        eval_value(&h.session, &session_id, &format!("document.body.innerHTML = {}; true", js(form))).await,
        true
    );
    let obs = h.observe().await;
    assert_eq!(eval_value(&h.session, &session_id, "history.pushState(null, '', '/?q=later'); true").await, true);
    let clicked = h.page.act(&click(&find(&obs, "Search").id), &obs).await.expect("act");
    assert_eq!(clicked.outcome, StepOutcome::Executed);
    let accepted = accept_dialog(&h).await;
    assert_eq!(accepted.outcome, StepOutcome::Executed);
    assert!(accepted.moved);
    h.close().await;
}

/// A click led to a page that opened an alert as it loaded. Accepting it was judged against the mark taken on the
/// page the click left, so the new document read as a move the acceptance made, and a guess could be cleared.
#[tokio::test(flavor = "multi_thread")]
async fn test_accepting_a_dialog_a_new_page_opened_as_it_loaded_is_no_move_of_the_runs() {
    let Some(h) = harness().await else { return };
    h.goto("/?q=guess").await;
    let session_id = h.session.active_session_id().unwrap();
    let link = r#"<a href="/alert-on-load.html">Results</a>"#;
    assert_eq!(
        eval_value(&h.session, &session_id, &format!("document.body.innerHTML = {}; true", js(link))).await,
        true
    );
    let obs = h.observe().await;
    let _ = h.page.act(&click(&find(&obs, "Results").id), &obs).await.expect("act");
    let accepted = accept_dialog(&h).await;
    assert_eq!(accepted.outcome, StepOutcome::Executed);
    assert!(!accepted.moved);
    h.close().await;
}

/// A page script that set its location after a delay loaded a new document during an unrelated click, and the new
/// document was read as the click's move, so a guess could be cleared.
#[tokio::test(flavor = "multi_thread")]
async fn test_only_a_navigation_the_runs_input_began_is_its_move() {
    let cases = [
        (r#"<a href="/inner.html">Go</a>"#, true),
        (r#"<form action="/inner.html"><button>Go</button></form>"#, true),
        ("<button onclick=\"location.href = '/inner.html'\">Go</button>", true),
        // The page sends itself elsewhere later, on a timer or after a fetch, not as the click's own doing.
        ("<button onclick=\"setTimeout(() => location.href = '/inner.html', 60)\">Go</button>", false),
    ];
    for (markup, moved) in cases {
        let Some(h) = harness().await else { return };
        h.goto("/?q=guess").await;
        let session_id = h.session.active_session_id().unwrap();
        let script = format!(
            "{{ const range = document.createRange(); range.selectNode(document.body); \
             document.body.replaceChildren(range.createContextualFragment({})); }} true",
            js(markup)
        );
        assert_eq!(eval_value(&h.session, &session_id, &script).await, true);
        let obs = h.observe().await;
        let result = h.page.act(&click(&find(&obs, "Go").id), &obs).await.expect("act");
        assert_eq!(result.outcome, StepOutcome::Executed, "{markup}");
        assert_eq!(result.moved, moved, "{markup}");
        h.close().await;
    }
}

/// A binding or a global in the page's own world is a fingerprint any site can read; the input watch and its
/// binding live in an isolated world, and still report a move the run's click made.
#[tokio::test(flavor = "multi_thread")]
async fn test_the_input_watch_leaves_nothing_the_page_can_see() {
    let Some(h) = harness().await else { return };
    h.goto("/?q=guess").await;
    let session_id = h.session.active_session_id().unwrap();
    let button = "<button onclick=\"history.pushState({}, '', '?q=corrected')\">Go</button>";
    assert_eq!(
        eval_value(&h.session, &session_id, &format!("document.body.innerHTML = {}; true", js(button))).await,
        true
    );
    let obs = h.observe().await;
    let result = h.page.act(&click(&find(&obs, "Go").id), &obs).await.expect("act");
    assert!(result.moved);
    let seen = eval_value(
        &h.session,
        &session_id,
        "[Object.getOwnPropertyNames(window).filter(name => /input|watching/i.test(name) && /fastbrowse|watching/i\
         .test(name)), '__fastbrowseInputNavigation' in window, typeof globalThis.__watching]",
    )
    .await;
    assert_eq!(seen, json!([[], false, "undefined"]));
    h.close().await;
}

/// A choice is set by code and its change event is code's own, not trusted, so a handler that moved the page on the
/// change was never counted as the run's move.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_select_whose_change_handler_moves_the_page_is_the_runs_move() {
    for (handler, moved) in [("history.pushState({}, '', '?trip=one')", true), ("", false)] {
        let Some(h) = harness().await else { return };
        h.goto("/?q=guess").await;
        let session_id = h.session.active_session_id().unwrap();
        let select = format!(
            r#"<select aria-label=Trip onchange="{handler}"><option>Round trip</option><option>One way</option></select>"#
        );
        assert_eq!(
            eval_value(&h.session, &session_id, &format!("document.body.innerHTML = {}; true", js(&select))).await,
            true
        );
        let obs = h.observe().await;
        let choose = Action {
            target_id: Some(find(&obs, "Trip").id.clone()),
            text: Some("One way".into()),
            ..Action::new(Operation::Select)
        };
        let result = h.page.act(&choose, &obs).await.expect("act");
        assert_eq!(result.outcome, StepOutcome::Executed, "{handler}");
        assert_eq!(result.moved, moved, "{handler}");
        h.close().await;
    }
}

// -- test_replaced_document.py: a page that replaces its document while it is being read, as a self-clearing bot
// check does, through real Chrome ---------------------------------------------------------------------------------

/// What a Runtime.evaluate whose document was replaced mid-command gets back from Chrome.
fn destroyed() -> Verdict {
    Verdict::Answer(json!({"error": {"code": -32000, "message": "Execution context was destroyed."}}))
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_page_that_never_stops_replacing_itself_ends_with_what_happened() {
    let armed = Arc::new(AtomicBool::new(false));
    let status_reads = Arc::new(AtomicU32::new(0));
    let (rule_armed, rule_reads) = (armed.clone(), status_reads.clone());
    let Some(h) = relayed(move |command, _| {
        // The status read, in its isolated world by context id: Python's `context_id is not None`.
        if rule_armed.load(Ordering::SeqCst)
            && method(command) == "Runtime.evaluate"
            && command["params"].get("contextId").is_some()
            && expression(command).contains("getEntriesByType('navigation')")
        {
            rule_reads.fetch_add(1, Ordering::SeqCst);
            return destroyed();
        }
        Verdict::Forward
    })
    .await
    else {
        return;
    };
    h.goto("/release.html").await;
    armed.store(true, Ordering::SeqCst);
    let lost = h.page.observe().await.expect_err("raises");
    assert!(lost.is_context_lost(), "{lost:?}");
    assert_eq!(status_reads.load(Ordering::SeqCst), rereads() + 1);
    assert!(lost.to_string().starts_with("Runtime.evaluate failed (CDP -32000) on "), "{lost}");
    assert!(lost.to_string().contains("reads in a row"));
    armed.store(false, Ordering::SeqCst);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_javascript_error_is_not_read_again() {
    let armed = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU32::new(0));
    let (rule_armed, rule_reads) = (armed.clone(), reads.clone());
    let Some(h) = relayed(move |command, _| {
        if rule_armed.load(Ordering::SeqCst) && method(command) == "Runtime.evaluate" {
            rule_reads.fetch_add(1, Ordering::SeqCst);
            return Verdict::Answer(json!({"result": {
                "result": {"type": "object", "subtype": "error"},
                "exceptionDetails": {"exceptionId": 1, "text": "Uncaught", "lineNumber": 0, "columnNumber": 0},
            }}));
        }
        Verdict::Forward
    })
    .await
    else {
        return;
    };
    h.goto("/release.html").await;
    armed.store(true, Ordering::SeqCst);
    let failed = h.page.capture().await.expect_err("raises");
    assert!(!failed.is_context_lost(), "{failed:?}");
    // One read, never a re-read after waiting for the page.
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    armed.store(false, Ordering::SeqCst);
    h.close().await;
}

// -- test_walls.py: what the agent calls a wall it cannot pass, and the pager a long listing must still offer ----

#[tokio::test(flavor = "multi_thread")]
async fn test_the_pager_and_load_more_at_the_foot_of_a_long_listing_are_still_offered() {
    let Some(h) = harness_with(Config::default(), SessionOptions::default()).await else { return };
    h.goto("/long-list.html").await;
    let observation = observe_until(&h.page, "Book 1").await;
    let labels: Vec<&str> = observation.controls.iter().map(|c| c.label.as_str()).collect();
    assert!(labels.contains(&"next"));
    assert!(labels.contains(&"Show more books"));
    h.close().await;
}
