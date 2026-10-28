mod common;
mod safety_support;

// tests/browser/test_safety.py: safety boundaries exercised through real Chrome. The Python tests drive the agent
// with scripted models; what is ported here is each test's browser-layer part, and the agent's own gates (the
// secret resolver, the irreversible-action and download checks, model-metadata masking) stay with the agent.
//
// The serve section at the end is tests/browser/test_serve.py's browser-layer part.
//
// Not ported, being the agent's alone: test_secret_resolution_uses_receiving_origin (the resolver is never asked),
// test_a_task_that_rules_out_downloads_never_clicks_a_download_link (the task's download rule), and
// test_resolved_secret_is_masked_in_model_metadata_but_raw_url_is_preserved (what the models are shown).

use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{RecordingArtifactSink, chrome, harness, sites, sites_dir};
use fastbrowse_browser::adapters::local_chrome::{LocalChromeProcess, find_chrome};
use fastbrowse_browser::error::BrowserError;
use fastbrowse_browser::page::CdpPage;
use fastbrowse_browser::session::{BoxError, BrowserSession, CheckAccess, SessionOptions};
use fastbrowse_browser::testing;
use fastbrowse_core::config::Config;
use fastbrowse_core::models::{
    Attachment, BrowserConnection, Bytes, FrameHandler, LocalChrome, Operation, StepOutcome,
};
use fastbrowse_core::page::Action;
use safety_support::{
    Reply, eval_active, eval_value, find, observe_until, one_at_a_time, query_param, quote, serve_on, wait_until,
};
use serde_json::json;
use tokio::time::sleep;

fn secret_fill(target_id: &str, text: &str, origin: &str) -> Action {
    Action {
        target_id: Some(target_id.to_owned()),
        text: Some(text.to_owned()),
        secret: true,
        secret_origin: Some(origin.to_owned()),
        ..Action::new(Operation::Fill)
    }
}

fn targeted(operation: Operation, target_id: &str) -> Action {
    Action { target_id: Some(target_id.to_owned()), ..Action::new(operation) }
}

fn fill(target_id: &str, text: &str) -> Action {
    Action { text: Some(text.to_owned()), ..targeted(Operation::Fill, target_id) }
}

/// `text[start:end]` with Python's code point offsets, as `Block` carries them.
fn slice(text: &str, start: i64, end: i64) -> String {
    text.chars().skip(start.max(0) as usize).take((end - start).max(0) as usize).collect()
}

fn session_of(session: &BrowserSession, frame_id: Option<&str>) -> String {
    match frame_id {
        Some(frame_id) => session
            .frame_sessions()
            .into_iter()
            .find(|(f, _)| f == frame_id)
            .map(|(_, s)| s)
            .expect("the frame has a session"),
        None => session.active_session_id().expect("active session"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_window_origin_cannot_spoof_secret_scope() {
    let _serial = one_at_a_time().await;
    for label in ["Username", "Frame field"] {
        let Some(h) = harness().await else { return };
        h.page.navigate(&sites().main, None).await.expect("navigate");
        let observed = observe_until(&h.page, label).await;
        let target = find(&observed, label);
        let session_id = session_of(&h.session, target.frame_id.as_deref());
        assert_eq!(
            eval_value(&h.session, &session_id, "window.origin = 'https://bank.example'").await,
            json!("https://bank.example")
        );
        let obs = h.observe().await;
        let target = find(&obs, label);
        let expected = if target.frame_id.is_some() { &sites().iframe } else { &sites().main };
        assert_eq!(target.frame_origin.as_deref(), Some(expected.as_str()));
        // The Python test first runs the agent with a secret scoped to the spoofed origin and checks the resolver is
        // never asked; the resolver is the agent's, so only the page's own refusal is ported.
        let target_id = target.id.clone();
        let obs = h.observe().await;
        let result =
            h.page.act(&secret_fill(&target_id, "do-not-leak", "https://bank.example"), &obs).await.expect("act");
        assert_eq!(result.outcome, StepOutcome::Failed, "{label}");
        assert_eq!(find(&h.observe().await, label).value.as_deref(), Some(""), "{label}");
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_page_rechecks_secret_origin_before_insertion() {
    let _serial = one_at_a_time().await;
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    let obs = observe_until(&h.page, "Frame field").await;
    let target = find(&obs, "Frame field");
    let result = h.page.act(&secret_fill(&target.id, "do-not-leak", &sites().main), &obs).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Failed);
    assert_eq!(find(&h.observe().await, "Frame field").value.as_deref(), Some(""));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_enter_uses_form_semantics_and_focuses_verified_target() {
    let _serial = one_at_a_time().await;
    // Only the authorized case is the page's: unauthorized, the agent asks for confirmation before any input.
    let Some(h) = harness().await else { return };
    h.goto("/safety.html").await;
    let obs = h.observe().await;
    let field = find(&obs, "Account name");
    assert!(field.submit_semantics.as_deref().is_some_and(|s| s.contains("Delete account")));
    eval_active(&h.session, "document.getElementById('other').focus()").await;
    let result = h.page.act(&targeted(Operation::Enter, &field.id), &obs).await.expect("act");
    let submitted = eval_active(&h.session, "document.body.dataset.submitted").await;
    assert!(submitted == json!("delete") && result.outcome == StepOutcome::Executed, "{submitted} {result:?}");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_task_that_asks_for_a_download_still_downloads() {
    let _serial = one_at_a_time().await;
    let Some(h) = harness().await else { return };
    h.goto("/release.html").await;
    let obs = h.observe().await;
    let link = find(&obs, "Download 2.1.0");
    let result = h.page.act(&targeted(Operation::Click, &link.id), &obs).await.expect("act");
    assert!(h.session.download_settled(0, 3.0).await);
    assert_eq!(h.session.artifacts().iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["addon-2.1.0.zip"]);
    assert_eq!(result.outcome, StepOutcome::Executed);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_dialog_acceptance_has_its_own_gate() {
    let _serial = one_at_a_time().await;
    // ("accept", unauthorized) is the agent's gate and is not ported; the other two cases reach the page.
    for choice in ["accept", "dismiss"] {
        let Some(h) = harness().await else { return };
        h.goto("/safety.html").await;
        let obs = h.observe().await;
        h.page.act(&targeted(Operation::Click, &find(&obs, "Continue").id), &obs).await.expect("act");
        let obs = h.observe().await;
        let action = Action { accept_dialog: Some(choice == "accept"), ..Action::new(Operation::Dialog) };
        let result = h.page.act(&action, &obs).await.expect("act");
        assert_eq!(result.outcome, StepOutcome::Executed, "{choice}");
        assert_eq!(
            eval_active(&h.session, "document.body.dataset.accepted").await,
            json!((choice == "accept").to_string()),
            "{choice}"
        );
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_covered_controls_are_not_modified() {
    let _serial = one_at_a_time().await;
    for operation in [Operation::Select, Operation::Upload] {
        let Some(h) = harness().await else { return };
        h.goto("/safety.html").await;
        eval_active(&h.session, "document.getElementById('cover').style.display = 'block'").await;
        let obs = h.observe().await;
        let target = find(&obs, if operation == Operation::Select { "Destination" } else { "Attachment" });
        let action = Action {
            text: Some("Second".into()),
            files: vec![Attachment {
                name: "data.txt".into(),
                mime_type: "text/plain".into(),
                content: Bytes(b"data".to_vec()),
            }],
            ..targeted(operation, &target.id)
        };
        let result = h.page.act(&action, &obs).await.expect("act");
        assert_eq!(result.outcome, StepOutcome::Covered, "{operation:?}");
        assert_eq!(
            eval_active(
                &h.session,
                "[document.querySelector('select').value, document.querySelector('[type=file]').files.length]"
            )
            .await,
            json!(["First", 0])
        );
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_iframe_focus_hit_testing_and_capture_scope() {
    let _serial = one_at_a_time().await;
    let Some(h) = harness().await else { return };
    h.goto("/safety.html").await;
    wait_until(
        || async {
            eval_active(&h.session, "document.getElementById('same').contentDocument?.querySelector('input') !== null")
                .await
                == json!(true)
        },
        5.0,
    )
    .await;
    let obs = h.observe().await;
    let field = find(&obs, "Inner field").clone();
    assert!(field.frame_id.is_some());
    assert_eq!(h.page.act(&fill(&field.id, "landed"), &obs).await.expect("act").outcome, StepOutcome::Executed);
    assert_eq!(find(&h.observe().await, "Inner field").value.as_deref(), Some("landed"));
    let obs = h.observe().await;
    let button = find(&obs, "Inner button");
    assert_eq!(
        h.page.act(&targeted(Operation::Click, &button.id), &obs).await.expect("act").outcome,
        StepOutcome::Executed
    );
    find(&h.observe().await, "Inner clicked");
    let capture = h.page.capture().await.expect("capture");
    assert!(capture.text.contains("Same-origin evidence") && capture.text.contains("Shadow evidence"));
    assert_eq!(capture.inaccessible_frames, 0);
    assert!(
        capture
            .blocks
            .iter()
            .filter(|b| slice(&capture.text, b.start, b.end).contains("evidence"))
            .all(|b| b.frame_id == field.frame_id)
    );
    assert!(
        capture
            .blocks
            .iter()
            .filter(|b| slice(&capture.text, b.start, b.end).contains("Shadow evidence"))
            .any(|b| b.source_id.contains("shadow:"))
    );

    h.page.navigate(&sites().main, None).await.expect("navigate");
    let obs = observe_until(&h.page, "Frame field").await;
    let field = find(&obs, "Frame field");
    assert_eq!(h.page.act(&fill(&field.id, "frame value"), &obs).await.expect("act").outcome, StepOutcome::Executed);
    assert_eq!(find(&h.observe().await, "Frame field").value.as_deref(), Some("frame value"));
    let obs = h.observe().await;
    let opened = h.page.act(&targeted(Operation::Click, &find(&obs, "Open popup").id), &obs).await.expect("act");
    assert_eq!(opened.outcome, StepOutcome::Executed, "{opened:?}");
    wait_until(|| async { h.session.tabs().iter().any(|t| t.url.contains("popup.html")) }, 5.0).await;
    let popup = h.session.tabs().into_iter().find(|t| t.url.contains("popup.html")).expect("popup");
    h.session.switch_tab(&popup.id).await.expect("switch");
    assert!(h.session.frame_sessions().is_empty());
    assert!(h.observe().await.controls.iter().all(|c| !c.label.contains("Frame field")));
    assert!(!h.page.capture().await.expect("capture").text.contains("Inside the frame"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fill_does_not_claim_execution_when_page_rejects_value() {
    let _serial = one_at_a_time().await;
    let Some(h) = harness().await else { return };
    h.goto("/safety.html").await;
    eval_active(
        &h.session,
        "document.getElementById('other').addEventListener('input', e => { e.target.value = ''; });",
    )
    .await;
    let obs = h.observe().await;
    let result = h.page.act(&fill(&find(&obs, "Other field").id, "rejected"), &obs).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Failed);
    h.close().await;
}

// -- allowed_origins: documents outside the grant never load, are never read, and never leak ----------------

const SECRET_TEXT: &str = "FOREIGN-SECRET-TEXT";
const SECRET_QUERY: &str = "token=hunter2";

/// A granted site and a foreign one that counts every request it gets: the foreign log is the arbiter.
struct OriginSites {
    granted: String,
    foreign_port: u16,
    hits: Arc<Mutex<Vec<String>>>,
}

impl OriginSites {
    fn foreign(&self, host: &str) -> String {
        format!("http://{host}:{}", self.foreign_port)
    }

    fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
}

fn origin_sites() -> OriginSites {
    let hits: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = hits.clone();
    let foreign_port = serve_on(
        TcpListener::bind("127.0.0.1:0").expect("bind"),
        Arc::new(move |path: &str| {
            log.lock().unwrap().push(path.to_owned());
            Reply::html(format!("<title>Foreign title</title><h1>{SECRET_TEXT}</h1>"))
        }),
    );
    let granted = serve_on(
        TcpListener::bind("127.0.0.1:0").expect("bind"),
        Arc::new(|full: &str| {
            let path = full.split('?').next().unwrap_or("/");
            let to = query_param(full, "to");
            if path == "/sw.js" {
                // Answers /swtarget itself, so without a bypass the request never reaches the network or Fetch.
                let script = [
                    "self.addEventListener('install', () => self.skipWaiting());",
                    "self.addEventListener('activate', e => e.waitUntil(clients.claim()));",
                    "self.addEventListener('fetch', e => { const u = new URL(e.request.url);",
                    "  if (u.pathname === '/swtarget')",
                    "    e.respondWith(Response.redirect(u.searchParams.get('to'), 302)); });",
                ]
                .join("\n");
                return Reply::of("text/javascript", script);
            }
            if path == "/redirect" {
                return Reply { status: 302, headers: vec![("Location", to)], ..Reply::html("") };
            }
            let page = match path {
                "/" => "<title>Granted home</title><h1>Granted home</h1><input type=file aria-label=Resume>".to_owned(),
                "/sw" => {
                    "<h1>Granted worker</h1><script>navigator.serviceWorker.register('/sw.js')</script>".to_owned()
                }
                "/swtarget" => "<title>Granted real page</title><h1>Granted real page</h1>".to_owned(),
                "/ok" => "<title>Granted popup</title><h1>Granted popup</h1>".to_owned(),
                "/refresh" => format!("<meta http-equiv=refresh content='0;url={to}'><h1>Granted refresh</h1>"),
                "/script" => format!("<h1>Granted script</h1><script>location = {}</script>", json!(to)),
                "/frame" => {
                    format!("<title>Granted frame page</title><h1>Granted frame page</h1><iframe src='{to}'></iframe>")
                }
                _ => String::new(),
            };
            Reply { status: if page.is_empty() { 404 } else { 200 }, ..Reply::html(page) }
        }),
    );
    OriginSites { granted: format!("http://127.0.0.1:{granted}"), foreign_port, hits }
}

#[test]
fn a_preconnect_is_not_counted_as_a_hit() {
    let sites = origin_sites();
    let address = ("127.0.0.1", sites.foreign_port);
    // What Chrome does beside a meta refresh: a socket opened and closed with nothing sent.
    drop(std::net::TcpStream::connect(address).expect("connect"));
    std::thread::sleep(Duration::from_millis(100));
    let mut request = std::net::TcpStream::connect(address).expect("connect");
    std::io::Write::write_all(&mut request, b"GET /page HTTP/1.1\r\n\r\n").expect("write");
    std::io::Read::read_to_end(&mut request, &mut Vec::new()).expect("read");
    assert_eq!(sites.hits(), ["/page"]);
}

struct Scoped {
    session: BrowserSession,
    page: CdpPage,
}

impl Scoped {
    async fn close(self) {
        let _ = self.session.close().await;
    }
}

/// `scoped`: a session on the shared Chrome whose grant is `origin`, attached to an open window if `attach`.
async fn scoped_on(
    connection: &BrowserConnection,
    origin: &str,
    attach: bool,
    target_match: Option<&str>,
    options: SessionOptions,
) -> Result<Scoped, BrowserError> {
    let connection = BrowserConnection {
        allowed_origins: Some(vec![origin.to_owned()]),
        attach,
        target_match: target_match.map(str::to_owned),
        ..connection.clone()
    };
    let session = BrowserSession::new(connection, Arc::new(RecordingArtifactSink::default()), options);
    session.open().await?;
    let page = CdpPage::new(session.clone(), Config::default());
    Ok(Scoped { session, page })
}

async fn scoped(
    connection: &BrowserConnection,
    origin: &str,
    attach: bool,
    target_match: Option<&str>,
) -> Result<Scoped, BrowserError> {
    scoped_on(connection, origin, attach, target_match, SessionOptions::default()).await
}

/// Wait for the browser's report of a foreign document to reach the guard.
async fn until_refused(session: &BrowserSession) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Err(BrowserError::OriginNotAllowed) = session.assert_clear().await {
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the foreign document was refused in time");
}

/// Long enough for a request that was going to leave to have left.
async fn settle() {
    sleep(Duration::from_secs(1)).await;
}

fn refused<T>(result: Result<T, BrowserError>) -> BrowserError {
    match result {
        Ok(_) => panic!("expected a refusal"),
        Err(error) => error,
    }
}

fn not_allowed<T>(result: Result<T, BrowserError>) {
    let error = refused(result);
    assert_eq!(error, BrowserError::OriginNotAllowed);
}

fn tab_text(session: &BrowserSession) -> String {
    session.tabs().iter().map(|t| format!("{}{}", t.url, t.title)).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn test_navigation_and_redirects_never_reach_a_foreign_origin() {
    let _serial = one_at_a_time().await;
    let Some(connection) = chrome() else { return };
    for route in ["direct", "redirect", "refresh", "script"] {
        let sites = origin_sites();
        let foreign = format!("{}/page?{SECRET_QUERY}", sites.foreign("127.0.0.1"));
        let s = scoped(connection, &sites.granted, false, None).await.expect("scoped session");
        let start = if route == "direct" {
            foreign.clone()
        } else {
            format!("{}/{route}?to={}", sites.granted, quote(&foreign))
        };
        let refusing = matches!(route, "direct" | "redirect");
        if refusing {
            let error = refused(s.page.navigate(&start, None).await);
            assert_eq!(error, BrowserError::OriginNotAllowed, "{route}");
            assert!(
                !error.to_string().contains(SECRET_QUERY)
                    && !error.to_string().contains(&sites.foreign_port.to_string())
            );
        } else {
            s.page.navigate(&start, None).await.expect("navigate");
        }
        settle().await;
        assert_eq!(sites.hits(), Vec::<String>::new(), "{route}");
        if refusing {
            not_allowed(s.page.capture().await);
            not_allowed(s.page.observe().await);
        } else {
            assert!(!s.page.capture().await.expect("capture").text.contains(SECRET_TEXT), "{route}");
            assert!(!s.page.observe().await.expect("observe").viewport_text.contains(SECRET_TEXT), "{route}");
        }
        assert!(!tab_text(&s.session).contains(SECRET_QUERY), "{route}");
        s.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_frames_of_a_foreign_origin_never_load() {
    let _serial = one_at_a_time().await;
    let Some(connection) = chrome() else { return };
    // 127.0.0.1 shares the page's process; localhost is put in one of its own.
    for host in ["127.0.0.1", "localhost"] {
        let sites = origin_sites();
        let s = scoped(connection, &sites.granted, false, None).await.expect("scoped session");
        let url = format!("{}/frame?to={}", sites.granted, quote(&format!("{}/page", sites.foreign(host))));
        s.page.navigate(&url, None).await.expect("navigate");
        settle().await;
        assert_eq!(sites.hits(), Vec::<String>::new(), "{host}");
        assert!(!s.page.capture().await.expect("capture").text.contains(SECRET_TEXT), "{host}");
        assert!(matches!(refused(s.page.screenshot().await), BrowserError::ScreenshotsUnavailable(_)), "{host}");
        s.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_popups_are_adopted_only_inside_the_grant() {
    let _serial = one_at_a_time().await;
    let Some(connection) = chrome() else { return };
    for opened in ["foreign", "redirect", "granted"] {
        let sites = origin_sites();
        let foreign = format!("{}/page?{SECRET_QUERY}", sites.foreign("127.0.0.1"));
        let url = match opened {
            "foreign" => foreign.clone(),
            "redirect" => format!("{}/redirect?to={}", sites.granted, quote(&foreign)),
            _ => format!("{}/ok", sites.granted),
        };
        let s = scoped(connection, &sites.granted, false, None).await.expect("scoped session");
        s.page.navigate(&format!("{}/", sites.granted), None).await.expect("navigate");
        let active = s.session.active_session_id().expect("active");
        s.session
            .send(
                "Runtime.evaluate",
                json!({"expression": format!("window.open({})", json!(url)), "userGesture": true}),
                Some(&active),
            )
            .await
            .expect("window.open");
        settle().await;
        assert_eq!(sites.hits(), Vec::<String>::new(), "{opened}");
        let urls: Vec<String> = s.session.tabs().into_iter().map(|t| t.url).collect();
        assert_eq!(urls.contains(&format!("{}/ok", sites.granted)), opened == "granted", "{opened} {urls:?}");
        assert!(!urls.concat().contains(SECRET_QUERY), "{opened}");
        s.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_an_attached_window_keeps_its_foreign_frames_and_other_windows_out() {
    let _serial = one_at_a_time().await;
    let Some(connection) = chrome() else { return };
    let Some(h) = harness().await else { return };
    let sites = origin_sites();
    // The window is the person's, already open with a foreign OOPIF in it before the run attaches.
    let owner = &h.page;
    let url = format!("{}/frame?to={}", sites.granted, quote(&format!("{}/page", sites.foreign("localhost"))));
    owner.navigate(&url, None).await.expect("navigate");
    wait_until(|| async { !sites.hits().is_empty() }, 5.0).await;
    let before = eval_active(&h.session, "location.href").await;
    let s = scoped(connection, &sites.granted, true, Some("Granted frame")).await.expect("attached");
    assert!(!s.page.capture().await.expect("capture").text.contains(SECRET_TEXT));
    assert!(!s.page.observe().await.expect("observe").viewport_text.contains(SECRET_TEXT));
    assert!(matches!(refused(s.page.screenshot().await), BrowserError::ScreenshotsUnavailable(_)));
    s.close().await;
    // A window that is not the grant's is neither matched nor named, and nothing navigates the person's window.
    owner.navigate(&format!("{}/page", sites.foreign("127.0.0.1")), None).await.expect("navigate");
    let missing = refused(scoped(connection, &sites.granted, true, Some("/page")).await);
    let message = missing.to_string();
    assert!(message.contains("no page matching"), "{message}");
    assert!(!message.contains("Foreign title") && !message.contains(&sites.foreign_port.to_string()), "{message}");
    owner.navigate(before.as_str().expect("an address"), None).await.expect("navigate");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_an_attached_browser_with_no_granted_window_gets_a_window_of_its_own() {
    let _serial = one_at_a_time().await;
    let Some(connection) = chrome() else { return };
    let Some(h) = harness().await else { return };
    let sites = origin_sites();
    // A fresh cloud browser shows only about:blank, which no grant covers; the run must still start, in a window it
    // opens, without touching the person's foreign one.
    h.page.navigate(&format!("{}/page", sites.foreign("127.0.0.1")), None).await.expect("navigate");
    let s = scoped(connection, &sites.granted, true, None).await.expect("attached");
    let opened = s.session.active_target_id();
    assert_ne!(opened, h.session.active_target_id());
    assert!(!testing::owned(&s.session).contains(&opened));
    s.page.navigate(&format!("{}/ok", sites.granted), None).await.expect("navigate");
    assert!(s.page.capture().await.expect("capture").text.contains("Granted popup"));
    s.close().await;
    // The window stands in for the one the run would have attached to, so it outlives the run.
    let targets = h.session.send("Target.getTargets", json!({}), None).await.expect("targets");
    let ids: Vec<&str> =
        targets["targetInfos"].as_array().expect("targets").iter().filter_map(|t| t["targetId"].as_str()).collect();
    assert!(ids.contains(&opened.as_str()), "{ids:?}");
    h.session.send("Target.closeTarget", json!({"targetId": opened}), None).await.expect("close");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_an_attached_browser_with_only_an_empty_window_takes_it_over() {
    let _serial = one_at_a_time().await;
    let Some(connection) = chrome() else { return };
    let Some(h) = harness().await else { return };
    let sites = origin_sites();
    // A fresh cloud browser shows one about:blank window; the run takes it over instead of opening a second beside
    // it, so the person watching sees one window.
    h.page.navigate(&format!("{}/page", sites.foreign("127.0.0.1")), None).await.expect("navigate");
    let created = h.session.send("Target.createTarget", json!({"url": "about:blank"}), None).await.expect("create");
    let blank = created["targetId"].as_str().expect("a target").to_owned();
    let target_ids = |targets: serde_json::Value| -> Vec<String> {
        let mut ids: Vec<String> = targets["targetInfos"]
            .as_array()
            .expect("targets")
            .iter()
            .filter_map(|t| t["targetId"].as_str().map(str::to_owned))
            .collect();
        ids.sort();
        ids
    };
    let before = target_ids(h.session.send("Target.getTargets", json!({}), None).await.expect("targets"));
    let s = scoped(connection, &sites.granted, true, None).await.expect("attached");
    assert_eq!(s.session.active_target_id(), blank);
    s.page.navigate(&format!("{}/ok", sites.granted), None).await.expect("navigate");
    assert!(s.page.capture().await.expect("capture").text.contains("Granted popup"));
    s.close().await;
    let after = target_ids(h.session.send("Target.getTargets", json!({}), None).await.expect("targets"));
    assert_eq!(after, before);
    h.session.send("Target.closeTarget", json!({"targetId": blank}), None).await.expect("close");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_document_the_network_never_saw_is_not_read_or_controlled() {
    let _serial = one_at_a_time().await;
    let Some(connection) = chrome() else { return };
    let sites = origin_sites();
    let s = scoped(connection, &sites.granted, false, None).await.expect("scoped session");
    s.page.navigate(&format!("{}/", sites.granted), None).await.expect("navigate");
    let observed = s.page.observe().await.expect("observe");
    // A data: document is not a request, so Fetch cannot refuse it; every read and mutation must.
    let active = s.session.active_session_id().expect("active");
    s.session
        .send("Page.navigate", json!({"url": format!("data:text/html,<h1>{SECRET_TEXT}</h1>")}), Some(&active))
        .await
        .expect("Page.navigate");
    until_refused(&s.session).await;
    assert!(!tab_text(&s.session).contains(SECRET_TEXT));
    not_allowed(s.page.observe().await);
    not_allowed(s.page.capture().await);
    not_allowed(s.page.screenshot().await);
    not_allowed(s.page.address().await);
    not_allowed(s.page.origin().await);
    not_allowed(s.page.act(&Action::new(Operation::Scroll), &observed).await);
    s.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_an_upload_script_checks_the_origin_it_runs_in() {
    let _serial = one_at_a_time().await;
    let Some(connection) = chrome() else { return };
    let sites = origin_sites();
    let s = scoped(connection, &sites.granted, false, None).await.expect("scoped session");
    s.page.navigate(&format!("{}/", sites.granted), None).await.expect("navigate");
    let control = find(&s.page.observe().await.expect("observe"), "Resume").clone();
    let files = [Attachment { name: "cv.txt".into(), mime_type: "text/plain".into(), content: Bytes(b"cv".to_vec()) }];
    let (outcome, _) = testing::safety::upload(&s.page, &control.id, &files, None).await.expect("upload");
    assert_eq!(outcome, StepOutcome::Executed);
    // The document changed between the check in `act` and the write: the grant no longer names this origin.
    let grant = Some(vec![sites.foreign("127.0.0.1")]);
    let (outcome, _) = testing::safety::upload(&s.page, &control.id, &files, grant).await.expect("upload");
    assert_eq!(outcome, StepOutcome::Stale);
    s.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_service_worker_cannot_answer_a_navigation_past_the_gate() {
    let _serial = one_at_a_time().await;
    let Some(connection) = chrome() else { return };
    let sites = origin_sites();
    let foreign = format!("{}/page?{SECRET_QUERY}", sites.foreign("127.0.0.1"));
    let s = scoped(connection, &sites.granted, false, None).await.expect("scoped session");
    s.page.navigate(&format!("{}/sw", sites.granted), None).await.expect("navigate");
    // The worker is registered by the page itself, from the granted origin, which is allowed.
    let active = s.session.active_session_id().expect("active");
    s.session
        .send(
            "Runtime.evaluate",
            json!({"expression": "navigator.serviceWorker.ready.then(() => true)", "awaitPromise": true}),
            Some(&active),
        )
        .await
        .expect("worker ready");
    // The page is controlled by the worker, which would redirect this navigation to a foreign origin. Bypassed, the
    // request reaches the server, which has a page there, and the worker never answers it.
    s.page.navigate(&format!("{}/swtarget?to={}", sites.granted, quote(&foreign)), None).await.expect("navigate");
    settle().await;
    assert!(s.session.denials() == 0 && sites.hits().is_empty());
    assert!(s.page.address().await.expect("address").starts_with(&format!("{}/swtarget", sites.granted)));
    assert!(!s.page.capture().await.expect("capture").text.contains(SECRET_TEXT));
    s.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_scoped_run_delivers_no_live_frames() {
    let _serial = one_at_a_time().await;
    let Some(connection) = chrome() else { return };
    let sites = origin_sites();
    let frames: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let received = frames.clone();
    let receive: FrameHandler = Arc::new(move |frame| {
        received.lock().unwrap().push(frame);
        Box::pin(async {})
    });
    let options = SessionOptions { on_frame: Some(receive), ..SessionOptions::default() };
    let s = scoped_on(connection, &sites.granted, false, None, options).await.expect("scoped session");
    s.page.navigate(&format!("{}/", sites.granted), None).await.expect("navigate");
    settle().await;
    assert!(s.session.frames_withheld() && frames.lock().unwrap().is_empty());
    s.close().await;
}

#[cfg(feature = "recording")]
#[tokio::test(flavor = "multi_thread")]
async fn test_a_scoped_session_refuses_to_record() {
    let _serial = one_at_a_time().await;
    let Some(connection) = chrome() else { return };
    let sites = origin_sites();
    let s = scoped(connection, &sites.granted, false, None).await.expect("scoped session");
    let refused =
        fastbrowse_browser::recording::Recording::start(s.session.clone(), std::path::Path::new("unused.mp4")).await;
    let Err(error) = refused else { panic!("a scoped session recorded") };
    assert!(error.to_string().contains("allowed_origins"), "{error}");
    s.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_check_access_runs_before_every_browser_touch() {
    let _serial = one_at_a_time().await;
    let Some(connection) = chrome() else { return };
    for scope in [false, true] {
        let sites = origin_sites();
        let calls = Arc::new(AtomicUsize::new(0));
        let allowed = Arc::new(AtomicBool::new(true));
        let check: CheckAccess = {
            let (calls, allowed) = (calls.clone(), allowed.clone());
            Arc::new(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                let allowed = allowed.load(Ordering::SeqCst);
                Box::pin(async move {
                    if allowed { Ok(()) } else { Err(BoxError::from("secret detail the caller put in its error")) }
                })
            })
        };
        let connection =
            BrowserConnection { allowed_origins: scope.then(|| vec![sites.granted.clone()]), ..connection.clone() };
        let options = SessionOptions { check_access: Some(check), ..SessionOptions::default() };
        let session = BrowserSession::new(connection, Arc::new(RecordingArtifactSink::default()), options);
        session.open().await.expect("session opens");
        let page = CdpPage::new(session.clone(), Config::default());
        page.navigate(&format!("{}/", sites.granted), None).await.expect("navigate");
        let observed = page.observe().await.expect("observe");
        allowed.store(false, Ordering::SeqCst);
        macro_rules! touch {
            ($touch:expr) => {{
                let before = calls.load(Ordering::SeqCst);
                let message = refused($touch.await).to_string();
                assert!(message.contains("caller supplied"), "scope {scope}: {message}");
                assert!(
                    calls.load(Ordering::SeqCst) == before + 1 && !message.contains("secret detail"),
                    "scope {scope}"
                );
            }};
        }
        touch!(page.observe());
        touch!(page.capture());
        touch!(page.screenshot());
        touch!(page.address());
        touch!(page.origin());
        touch!(page.response_status());
        touch!(page.navigate(&format!("{}/ok", sites.granted), None));
        touch!(page.act(&Action::new(Operation::Scroll), &observed));
        touch!(page.document_changed(&observed));
        touch!(page.redrawn(&observed, 0.1, None));
        allowed.store(true, Ordering::SeqCst);
        let _ = session.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_blank_popup_inherits_no_foreign_document_grant() {
    let _serial = one_at_a_time().await;
    let Some(connection) = chrome() else { return };
    let Some(h) = harness().await else { return };
    let sites = origin_sites();
    h.page.navigate(&format!("{}/page", sites.foreign("127.0.0.1")), None).await.expect("navigate");
    eval_active(
        &h.session,
        "(() => { const popup = window.open('about:blank'); \
         popup.document.write('<title>Inherited foreign document</title>outside-grant-secret'); return true; })()",
    )
    .await;
    settle().await;
    let attached = scoped(connection, &sites.granted, true, Some("Inherited foreign document")).await;
    if let Ok(s) = attached {
        s.close().await;
        panic!("A blank popup must not inherit a grant from its foreign opener");
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_opaque_child_documents_never_enter_a_scoped_read() {
    let _serial = one_at_a_time().await;
    let Some(connection) = chrome() else { return };
    for source in ["blank", "srcdoc", "blob"] {
        let Some(h) = harness().await else { return };
        let sites = origin_sites();
        h.page.navigate(&format!("{}/ok", sites.granted), None).await.expect("navigate");
        let script = format!(
            "(kind => {{ const frame = document.createElement('iframe'); \
             const html = '<p>{SECRET_TEXT}</p><button>Opaque child action</button>'; \
             if (kind === 'srcdoc') frame.srcdoc = html; \
             if (kind === 'blob') frame.src = URL.createObjectURL(new Blob([html], {{type:'text/html'}})); \
             document.body.append(frame); if (kind === 'blank') frame.contentDocument.write(html); return true; }})({})",
            json!(source)
        );
        eval_active(&h.session, &script).await;
        settle().await;
        assert!(h.page.capture().await.expect("capture").text.contains(SECRET_TEXT), "{source}");
        let s = scoped(connection, &sites.granted, true, Some("/ok")).await.expect("attached");
        assert!(!s.page.capture().await.expect("capture").text.contains(SECRET_TEXT), "{source}");
        let observed = s.page.observe().await.expect("observe");
        assert!(!observed.viewport_text.contains(SECRET_TEXT), "{source}");
        assert!(observed.controls.iter().all(|c| c.label != "Opaque child action"), "{source}");
        s.close().await;
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_access_is_checked_before_connecting_to_a_browser() {
    let _serial = one_at_a_time().await;
    // Python enters through `run.connect_cdp`; the browser layer's own entry is the session's open, which asks
    // before it connects, so no browser needs to be there.
    let check: CheckAccess = Arc::new(|| Box::pin(async { Err(BoxError::from("private caller detail")) }));
    let connection = BrowserConnection::new("ws://127.0.0.1:1", false);
    let options = SessionOptions { check_access: Some(check), ..SessionOptions::default() };
    let session = BrowserSession::new(connection, Arc::new(RecordingArtifactSink::default()), options);
    let failure = refused(session.open().await).to_string();
    assert!(failure.contains("caller supplied"), "{failure}");
    assert!(!failure.contains("private caller detail"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_scoped_screenshots_are_refused_even_on_a_clean_page() {
    let _serial = one_at_a_time().await;
    let Some(connection) = chrome() else { return };
    let sites = origin_sites();
    let s = scoped(connection, &sites.granted, false, None).await.expect("scoped session");
    s.page.navigate(&format!("{}/ok", sites.granted), None).await.expect("navigate");
    let message = refused(s.page.screenshot().await).to_string();
    assert!(message.contains("screenshots are unavailable"), "{message}");
    s.close().await;
}

// -- tests/browser/test_serve.py: what the browser layer does in a served run ------------------------------

const TOKEN: &str = "s3cr3t-T0ken-225"; // gitleaks:allow
/// Chrome's helper processes go a moment after the browser process.
const GONE_WITHIN: Duration = Duration::from_secs(15);

/// The process `root` and every process under it.
fn process_tree(root: u32) -> Vec<u32> {
    let listed = std::process::Command::new("ps").args(["-axo", "pid=,ppid="]).output().expect("ps");
    let rows: Vec<(u32, u32)> = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
        })
        .collect();
    let mut found = vec![root];
    let mut ahead = vec![root];
    while let Some(parent) = ahead.pop() {
        for &(pid, ppid) in &rows {
            if ppid == parent && !found.contains(&pid) {
                found.push(pid);
                ahead.push(pid);
            }
        }
    }
    found
}

fn alive(pids: &[u32]) -> Vec<u32> {
    pids.iter()
        .copied()
        .filter(|pid| {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
        .collect()
}

async fn gone(pids: &[u32]) -> bool {
    let deadline = tokio::time::Instant::now() + GONE_WITHIN;
    while !alive(pids).is_empty() && tokio::time::Instant::now() < deadline {
        sleep(Duration::from_millis(100)).await;
    }
    alive(pids).is_empty()
}

#[tokio::test(flavor = "multi_thread")]
async fn test_one_run_types_a_secret_the_client_holds_and_leaves_no_chrome_behind() {
    let _serial = one_at_a_time().await;
    // The served run's browser part: a Chrome of its own, the secret typed only into the origin it was given for,
    // and no Chrome process left once the session and the browser are closed. The JSON-RPC exchange, the scripted
    // run and the events it reports are `fastbrowse serve`'s.
    if find_chrome(None).is_none() {
        return;
    }
    let posted: Arc<Mutex<Vec<String>>> = Arc::default();
    let origin = format!("http://127.0.0.1:{}", common::serve(sites_dir().join("main"), None, posted.clone()));
    let (mut process, connection) = LocalChromeProcess::launch(&LocalChrome::default()).await.expect("Chrome launches");
    let started = process_tree(process.pid().expect("Chrome runs"));
    let session =
        BrowserSession::new(connection, Arc::new(RecordingArtifactSink::default()), SessionOptions::default());
    session.open().await.expect("session opens");
    let page = CdpPage::new(session.clone(), Config::default());
    page.navigate(&format!("{origin}/token.html"), None).await.expect("navigate");
    let obs = page.observe().await.expect("observe");
    let filled = page.act(&secret_fill(&find(&obs, "Token").id, TOKEN, &origin), &obs).await.expect("fill");
    assert_eq!(filled.outcome, StepOutcome::Executed, "{filled:?}");
    let obs = page.observe().await.expect("observe");
    assert!(!obs.viewport_text.contains(TOKEN) && obs.controls.iter().all(|c| c.value.as_deref() != Some(TOKEN)));
    let saved = page.act(&targeted(Operation::Click, &find(&obs, "Save").id), &obs).await.expect("click");
    assert_eq!(saved.outcome, StepOutcome::Executed, "{saved:?}");
    wait_until(|| async { !posted.lock().unwrap().is_empty() }, 5.0).await;
    let _ = session.close().await;
    process.close().await;
    assert!(gone(&started).await, "Chrome processes left: {:?}", alive(&started));
    assert_eq!(*posted.lock().unwrap(), [TOKEN]);
}
