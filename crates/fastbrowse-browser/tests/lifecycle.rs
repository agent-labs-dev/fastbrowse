//! tests/browser/test_lifecycle.py: failure and cancellation paths that need deterministic transport responses.
//!
//! Each test drives a `BrowserSession` against `FakeChrome`, test_lifecycle.py's `CdpTransport` behind a socket.
//!
//! Ported as unit tests in adapters, src/adapters/browser_use_cloud.rs:
//! - test_cloud_setup_failure_stops_created_browser
//! - test_cloud_cancellation_waits_for_billable_requests, as a_dropped_start_still_lets_stop_find_and_stop_its_browser
//!   and a_browser_dropped_mid_create_is_stopped_anyway
//! - test_cloud_teardown_preserves_original_error_and_cost, as cloud_teardown_keeps_the_cost_it_last_read
//! - test_a_stop_the_cloud_could_not_take_is_sent_again
//! - test_a_throttled_stop_waits_as_long_as_the_cloud_asks
//! - test_a_throttled_stop_waits_no_longer_than_its_bound
//! - test_a_browser_that_could_not_be_stopped_is_logged, as a_browser_that_could_not_be_stopped_says_so_without_the_key
//! - test_cloud_extensions_pick_the_api_version
//! - test_cloud_extensions_are_validated_before_any_request
//! - test_a_run_asks_the_cloud_for_the_browser_lifetime_it_needs,
//!   test_a_cloud_profile_starts_the_browser_signed_in_as_that_profile and
//!   test_a_cloud_browser_allows_resizing_only_when_asked, all three as the_create_body_keeps_pythons_key_order
//!
//! Ported as unit tests in adapters, src/adapters/local_chrome.rs:
//! - test_chrome_is_killed_and_reaped_on_shutdown_timeout
//! - test_a_chrome_that_exits_at_start_fails_at_once_with_its_own_error
//! - test_chrome_discovery
//! - test_chrome_discovery_finds_a_windows_install_off_path
//!
//! Not ported, as they test the run layer (`run._browser`, `run_task`, `connect_cdp`, `resolve_cdp_port`, `Agent`):
//! - test_local_chrome_comes_forward_only_headless_or_when_asked
//! - test_a_handed_over_browser_stays_in_the_background_unless_asked
//! - test_a_connection_lost_while_opening_returns_an_unavailable_result
//! - test_a_teardown_error_replaces_the_budget_with_its_status
//! - test_cloud_extensions_need_a_cloud_browser
//! - test_a_cloud_profile_without_a_cloud_browser_is_refused
//! - test_a_browser_handed_over_is_attached_to_and_left_running
//! - test_a_browser_handed_over_refuses_what_belongs_to_one_we_start
//! - test_a_browser_handed_over_with_no_page_named_still_works_one_out_from_the_task
//! - test_connect_cdp_reads_the_port_and_attaches
//!
//! Not ported: test_local_chrome_threads_startup_and_shutdown, since Python moves a blocking context manager onto a
//! thread and the Rust launch is async, with no thread to check.
//!
//! The deadline tests run on the real `CDP_REPLY_SECONDS`, `CDP_ALIVE_SECONDS` and `CDP_COMMAND_SECONDS`, which are
//! constants a test cannot shorten as Python's monkeypatch does, so they take minutes and are ignored by default: run
//! them with `--ignored`.

mod common;
mod lifecycle_support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use fastbrowse_browser::error::BrowserError;
use fastbrowse_browser::page::CdpPage;
use fastbrowse_browser::session::{BrowserSession, SessionOptions};
use fastbrowse_browser::testing;
use fastbrowse_core::config::Config;
use fastbrowse_core::models::{BrowserConnection, StepOutcome};
use lifecycle_support::{FakeChrome, Reply, SECRET};
use serde_json::{Value, json};

fn keep_painting() -> Value {
    json!({"format": "jpeg", "quality": 1, "maxWidth": 16, "maxHeight": 16, "everyNthFrame": 1000})
}

fn page(session: &BrowserSession) -> CdpPage {
    CdpPage::new(session.clone(), Config::default())
}

/// Navigation waits for a loaded document, then for its DOM to go quiet.
fn loaded() -> Value {
    json!({"result": {"value": "complete"}})
}

fn settled() -> Value {
    json!({"result": {"value": [true, "fingerprint"]}})
}

fn history(entries: &[&str], current: i64) -> Value {
    let entries: Vec<Value> = entries.iter().enumerate().map(|(i, url)| json!({"id": i, "url": url})).collect();
    json!({"currentIndex": current, "entries": entries})
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_run_in_a_visible_window_never_activates_its_target() {
    // Chrome raises and focuses the whole window of a tab that is activated or opened in front.
    for attach in [false, true] {
        let fake = FakeChrome::start().await;
        fake.queue(
            "Target.getTargets",
            [json!({"targetInfos": [
                {"targetId": "owned", "type": "page", "url": "https://example.com/", "title": "Open"},
            ]})],
        );
        fake.queue(
            "Target.getTargetInfo",
            [json!({"targetInfo": {"url": "https://example.com/pop", "title": "Popup"}})],
        );
        let session = fake.session(BrowserConnection { attach, ..fake.background() }).await;
        testing::adopt(&session, "popup", "owned").await;
        session.switch_tab("popup").await.unwrap();
        session.bring_to_front().await.unwrap();
        session.close().await.unwrap();
        let script = fake.script();
        assert!(!script.calls.iter().any(|m| m == "Target.activateTarget" || m == "Page.bringToFront"));
        let created = script.params_of("Target.createTarget");
        let expected = if attach { vec![] } else { vec![json!({"url": "about:blank", "background": true})] };
        assert_eq!(created, expected, "attach={attach}");
        // Both tabs render and take typing where they are instead: focus emulation, and a cast that keeps them
        // painting.
        for (method, params) in [
            ("Emulation.setFocusEmulationEnabled", json!({"enabled": true})),
            ("Page.startScreencast", keep_painting()),
        ] {
            assert_eq!(script.requested(method, &params, Some("session")), 2, "{method} attach={attach}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_keeper_that_cannot_attach_leaves_the_tab_to_the_run() {
    // The cast that keeps a background tab painting only speeds up captures, so its attach never holds up setup.
    let fake = FakeChrome::start().await;
    fake.queue("Target.attachToTarget", [json!({"sessionId": "session"})]);
    fake.block("Target.attachToTarget");
    let session = tokio::time::timeout(Duration::from_secs(5), fake.session(fake.background())).await.expect("setup");
    assert_eq!(session.active_session_id().unwrap(), "session");
    assert!(testing::keepers(&session).is_empty());
    // `transport.finished`: the keeper's attach was given up on, so no call still waits for it.
    assert_eq!(session.client().unwrap().pending(), 0);
    session.close().await.unwrap();
    assert!(!fake.calls().iter().any(|m| m == "Page.startScreencast"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_run_nobody_sits_in_front_of_activates_its_target() {
    let fake = FakeChrome::start().await;
    let session = fake.session(fake.connection()).await;
    session.switch_tab("owned").await.unwrap();
    session.close().await.unwrap();
    let script = fake.script();
    assert_eq!(script.count("Target.activateTarget"), 2);
    assert_eq!(script.requested("Target.createTarget", &json!({"url": "about:blank"}), None), 1);
    assert!(!script.calls.iter().any(|m| m == "Emulation.setFocusEmulationEnabled" || m == "Page.startScreencast"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_session_setup_sends_independent_commands_at_once() {
    // Each command is a cloud round trip, and discovery and foregrounding need not wait for the tab or its domains.
    let fake = FakeChrome::start().await;
    for method in ["Target.setDiscoverTargets", "Target.activateTarget", "Runtime.enable"] {
        fake.delay(method, 0.3);
    }
    let began = Instant::now();
    let session = fake.session(fake.connection()).await;
    let took = began.elapsed();
    session.close().await.unwrap();
    // In sequence the three take 0.9s.
    assert!(took < Duration::from_secs_f64(0.6), "took {took:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_popup_activation_and_preparation_overlap_and_drain_on_cancellation() {
    let fake = FakeChrome::start().await;
    let session = fake.session(fake.connection()).await;
    let (activated, enabled) = {
        let script = fake.script();
        (script.count("Target.activateTarget"), script.count("Runtime.enable"))
    };
    fake.block("Target.activateTarget");
    fake.block("Runtime.enable");
    let adopting = tokio::spawn({
        let session = session.clone();
        async move { testing::adopt(&session, "popup", "owned").await }
    });
    fake.wait_until("activation and preparation", |s| {
        s.count("Target.activateTarget") > activated && s.count("Runtime.enable") > enabled
    })
    .await;
    adopting.abort();
    assert!(adopting.await.unwrap_err().is_cancelled());
    assert_eq!(testing::popups(&session), [("popup".to_owned(), "owned".to_owned(), Some(false))]);
    assert_eq!(session.client().unwrap().pending(), 0);
    session.close().await.unwrap();
}

const SETUP_METHODS: [&str; 4] =
    ["Target.setDiscoverTargets", "Target.attachToTarget", "Runtime.enable", "Fetch.enable"];

/// test_session_setup_rolls_back with `cancelled=False`.
#[tokio::test(flavor = "multi_thread")]
async fn test_session_setup_rolls_back() {
    for method in SETUP_METHODS {
        let fake = FakeChrome::start().await;
        fake.fail(method, SECRET);
        let session = fake.new_session(fake.connection(), SessionOptions::default());
        let error = session.open().await.unwrap_err();
        assert!(!error.to_string().contains("secret"), "{error}");
        fake.stopped().await;
        let calls = fake.calls();
        assert_eq!(calls.last().map(String::as_str), Some("stop"));
        assert_eq!(calls.iter().any(|m| m == "Target.closeTarget"), method != "Target.setDiscoverTargets", "{method}");
    }
}

/// test_session_setup_rolls_back with `cancelled=True`: in Rust a cancelled setup is a dropped `open` future.
#[tokio::test(flavor = "multi_thread")]
async fn test_session_setup_rolls_back_when_cancelled() {
    for method in SETUP_METHODS {
        let fake = FakeChrome::start().await;
        fake.block(method);
        let session = fake.new_session(fake.connection(), SessionOptions::default());
        let opening = tokio::spawn({
            let session = session.clone();
            async move { session.open().await }
        });
        fake.sent(method).await;
        opening.abort();
        assert!(opening.await.unwrap_err().is_cancelled());
        // The dropped `open` closes the session in the background, so nothing is left pending once it has stopped.
        fake.stopped().await;
        if let Ok(client) = session.client() {
            assert_eq!(client.pending(), 0, "{method}");
        }
        let calls = fake.calls();
        assert_eq!(calls.last().map(String::as_str), Some("stop"));
        assert_eq!(calls.iter().any(|m| m == "Target.closeTarget"), method != "Target.setDiscoverTargets", "{method}");
    }
}

/// A DevTools address whose server reads the handshake, writes `reply` and closes the connection.
async fn handshake_server(reply: &'static [u8]) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request).await;
            let _ = stream.write_all(reply).await;
            let _ = stream.shutdown().await;
        }
    });
    address.to_string()
}

async fn assert_outage(name: &str, url: String, outage: bool) {
    let connection = BrowserConnection { foreground: true, ..BrowserConnection::new(url, false) };
    let session =
        BrowserSession::new(connection, Arc::new(common::RecordingArtifactSink::default()), Default::default());
    let error = session.open().await.unwrap_err();
    assert_eq!(error.is_unavailable(), outage, "{name}: {error}");
}

/// test_a_browser_unreachable_at_start_is_an_outage's "refused" and "malformed reply" cases.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_browser_unreachable_at_start_is_an_outage() {
    let refused = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().to_string()
    };
    assert_outage("refused", format!("ws://{refused}/devtools/browser/x"), true).await;
    let malformed = handshake_server(b"HTTP/1.0 401 Unauthorized\r\n\r\n").await;
    assert_outage("malformed reply", format!("ws://{malformed}/devtools/browser/x"), false).await;
}

/// test_a_browser_unreachable_at_start_is_an_outage's "dropped handshake" and "bad certificate" cases. Python's bad
/// certificate fails verification; here the server speaks no TLS at all, which fails in the same TLS layer without a
/// certificate to ship with the test.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_browser_unreachable_at_start_is_an_outage_dropped_or_bad_certificate() {
    let dropped = handshake_server(b"").await;
    assert_outage("dropped handshake", format!("ws://{dropped}/devtools/browser/x"), true).await;
    let plain = handshake_server(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
    assert_outage("bad certificate", format!("wss://{plain}/devtools/browser/x"), false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cdp_errors_are_typed_with_safe_messages() {
    for failure in [SECRET, Reply::Close] {
        let fake = FakeChrome::start().await;
        let session = fake.session(fake.connection()).await;
        fake.fail("Page.navigate", failure.clone());
        let error = page(&session).navigate("https://example.test/secret", None).await.unwrap_err();
        // Python also checks `__cause__` is the transport's exception; a Rust error carries no cause.
        assert!(error.to_string().starts_with("Page.navigate failed ("), "{failure:?}: {error}");
        assert!(!error.to_string().contains("secret"), "{failure:?}: {error}");
        session.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_failed_navigation_is_tried_again() {
    let cases: [(Vec<&str>, Option<&str>); 5] = [
        (vec!["net::ERR_TUNNEL_CONNECTION_FAILED"], None),
        (vec!["net::ERR_TIMED_OUT"; 3], None),
        (
            vec!["net::ERR_TUNNEL_CONNECTION_FAILED"; 4],
            Some("Page.navigate failed (net::ERR_TUNNEL_CONNECTION_FAILED)"),
        ),
        (vec!["secret https://example.test/secret"; 4], Some("Page.navigate failed (NavigationError)")),
        (vec!["net::ERR_NAME_NOT_RESOLVED"; 4], Some("Page.navigate failed (net::ERR_NAME_NOT_RESOLVED)")),
    ];
    // Python sets `_NAVIGATE_RETRY_SECONDS` to 0; the Rust waits between tries are constants, up to seven seconds in
    // all, so the cases run side by side.
    let runs = cases.into_iter().map(|(errors, raised)| async move {
        let fake = FakeChrome::start().await;
        fake.queue("Page.navigate", errors.iter().map(|error| json!({"errorText": error})));
        fake.queue("Runtime.evaluate", [loaded(), settled()]);
        let session = fake.session(fake.connection()).await;
        let navigating = page(&session).navigate("https://example.test", None).await;
        match raised {
            None => navigating.unwrap(),
            Some(raised) => {
                let caught = navigating.unwrap_err();
                assert!(caught.to_string().contains(raised), "{caught}");
                // A real navigation error (a net:: failure, or one the page reports with no error text at all) is
                // never mistaken for a timeout: only the readiness wait exhausting its budget classifies as one.
                assert!(!matches!(caught, BrowserError::NavigationTimeout(_)));
                // The site or connection dropping it is an outage; a name that never resolved can be a typo.
                assert_eq!(matches!(caught, BrowserError::SiteUnreachable(_)), raised.contains("TUNNEL"));
            }
        }
        session.close().await.unwrap();
        assert_eq!(fake.script().count("Page.navigate"), (errors.len() + 1).min(4));
    });
    futures_util::future::join_all(runs).await;
}

/// A run that never reached its first page reports which address failed, not only Chrome's error name.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_failed_navigation_names_the_address() {
    let fake = FakeChrome::start().await;
    fake.queue("Page.navigate", (0..4).map(|_| json!({"errorText": "net::ERR_NAME_NOT_RESOLVED"})));
    let session = fake.session(fake.connection()).await;
    let error = page(&session).navigate("https://unresolved.example.test/start", None).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "Page.navigate failed (net::ERR_NAME_NOT_RESOLVED) for https://unresolved.example.test/start"
    );
    session.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_page_that_never_loads_is_tried_again() {
    let fake = FakeChrome::start().await;
    let session = fake.session(fake.connection()).await;
    let error = page(&session).navigate_within("https://example.test", 0.1, None).await.unwrap_err();
    assert!(matches!(error, BrowserError::NavigationTimeout(_)), "{error:?}");
    assert!(error.to_string().contains("Page.navigate failed (TimeoutError)"));
    fake.queue("Runtime.evaluate", [loaded(), settled()]);
    page(&session).navigate_within("https://example.test", 0.1, None).await.unwrap();
    session.close().await.unwrap();
    assert_eq!(fake.script().count("Page.navigate"), 4 + 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_can_go_back_is_false_without_a_same_origin_predecessor() {
    let cases: [(&[&str], i64); 3] = [
        // A blank predecessor.
        (&["about:blank", "https://example.test/wizard"], 1),
        // A cross-origin predecessor.
        (&["https://other.test/", "https://example.test/"], 1),
        // An empty history.
        (&["https://example.test/"], 0),
    ];
    for (entries, current) in cases {
        let fake = FakeChrome::start().await;
        fake.queue("Page.getNavigationHistory", [history(entries, current)]);
        let session = fake.session(fake.connection()).await;
        assert!(!page(&session).history().await.0, "{entries:?}");
        session.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_failed_history_read_offers_no_back() {
    let fake = FakeChrome::start().await;
    fake.fail("Page.getNavigationHistory", SECRET);
    let session = fake.session(fake.connection()).await;
    assert!(!page(&session).history().await.0);
    session.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_can_go_back_is_true_with_a_same_origin_predecessor() {
    let fake = FakeChrome::start().await;
    fake.queue("Page.getNavigationHistory", [history(&["https://example.test/start", "https://example.test/next"], 1)]);
    let session = fake.session(fake.connection()).await;
    assert!(page(&session).history().await.0);
    session.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_back_refuses_to_navigate_when_the_predecessor_is_no_longer_same_origin() {
    // History can change between the observation that offered BACK and this dispatch, so `back` re-checks the live
    // history rather than trusting a guard the caller computed from a stale observation.
    let fake = FakeChrome::start().await;
    fake.queue("Page.getNavigationHistory", [history(&["about:blank", "https://example.test/"], 1)]);
    let session = fake.session(fake.connection()).await;
    let (outcome, detail) = testing::lifecycle::back(&page(&session)).await.unwrap();
    session.close().await.unwrap();
    assert_eq!(outcome, StepOutcome::Failed);
    assert_eq!(detail.as_deref(), Some("no same-origin earlier history entry"));
    assert!(!fake.calls().iter().any(|m| m == "Page.navigateToHistoryEntry"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_back_navigates_to_a_same_origin_predecessor_and_can_be_taken_twice() {
    let fake = FakeChrome::start().await;
    let urls = ["https://example.test/a", "https://example.test/b", "https://example.test/c"];
    fake.queue("Page.getNavigationHistory", [history(&urls, 2), history(&urls, 1)]);
    let session = fake.session(fake.connection()).await;
    let page = page(&session);
    let first = testing::lifecycle::back(&page).await.unwrap();
    let second = testing::lifecycle::back(&page).await.unwrap();
    session.close().await.unwrap();
    assert_eq!(first, (StepOutcome::Executed, None));
    assert_eq!(second, (StepOutcome::Executed, None));
    assert_eq!(fake.script().count("Page.navigateToHistoryEntry"), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_back_from_a_page_opened_in_place_of_the_start_page_opens_the_start_page() {
    // A shortcut is loaded instead of the start page, so the tab holds no entry for BACK to return to.
    let fake = FakeChrome::start().await;
    let session = fake.session(fake.connection()).await;
    let page = page(&session);
    // Python writes `page._back_to` directly; the navigation that opens the shortcut is what records it here.
    fake.queue("Runtime.evaluate", [loaded(), settled()]);
    page.navigate("https://example.test/deep", Some("https://example.test/")).await.unwrap();
    let before = fake.script().params_of("Page.navigate").len();
    let entries = history(&["about:blank", "https://example.test/deep"], 1);
    fake.queue("Page.getNavigationHistory", [entries.clone(), entries]);
    // Python stubs `page.navigate` out; the Rust one goes to the fake, which answers it as loaded.
    fake.queue("Runtime.evaluate", [loaded(), settled()]);
    assert!(page.history().await.0);
    assert_eq!(testing::lifecycle::back(&page).await.unwrap(), (StepOutcome::Executed, None));
    session.close().await.unwrap();
    let opened: Vec<Value> =
        fake.script().params_of("Page.navigate")[before..].iter().map(|p| p["url"].clone()).collect();
    assert_eq!(opened, [json!("https://example.test/")]);
    assert!(!fake.calls().iter().any(|m| m == "Page.navigateToHistoryEntry"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_background_finalizers_finish_before_socket_stops() {
    let fake = FakeChrome::start().await;
    fake.block("Fetch.getResponseBody");
    let session = fake.session(fake.connection()).await;
    testing::dispatch(
        &session,
        "Fetch.requestPaused",
        json!({
            "requestId": "download",
            "responseHeaders": [{"name": "content-disposition", "value": "attachment"}],
            "request": {"url": "https://example.test/file"},
        }),
        Some("session"),
    );
    fake.sent("Fetch.getResponseBody").await;
    session.close().await.unwrap();
    fake.stopped().await;
    // Python also checks the body read was cancelled; `close` awaits every task it stops before it sends anything.
    let script = fake.script();
    assert!(script.index("Fetch.continueRequest") < script.index("stop"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cancelling_page_wait_drains_cdp_task() {
    for method in ["Input.dispatchKeyEvent", "Page.captureScreenshot"] {
        let fake = FakeChrome::start().await;
        fake.block(method);
        let session = fake.session(fake.connection()).await;
        let page = Arc::new(page(&session));
        let session_id = session.active_session_id().unwrap();
        let task = tokio::spawn({
            let page = page.clone();
            async move {
                if method == "Page.captureScreenshot" {
                    page.screenshot().await.map(drop)
                } else {
                    testing::lifecycle::input_enter(&page, &session_id).await
                }
            }
        });
        fake.sent(method).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(session.client().unwrap().pending(), 0, "{method}");
        session.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_background_tab_that_paints_nothing_has_no_screenshot() {
    // Only bringing the window forward forces a frame, so a capture that stalls there ends instead of hanging.
    let fake = FakeChrome::start().await;
    fake.block("Page.captureScreenshot");
    let session = fake.session(fake.background()).await;
    // Python shortens the screenshot waits to 0.01s; the Rust ones are constants, six seconds in all.
    let error = page(&session).screenshot().await.unwrap_err();
    assert!(matches!(error, BrowserError::ScreenshotsUnavailable(_)), "{error:?}");
    assert_eq!(session.client().unwrap().pending(), 0);
    session.close().await.unwrap();
    let script = fake.script();
    assert!(!script.calls.iter().any(|m| m == "Target.activateTarget"));
    assert_eq!(script.requested("Page.startScreencast", &keep_painting(), Some("session")), 2);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: waits out the real CDP_REPLY_SECONDS and CDP_ALIVE_SECONDS, about 70s"]
async fn test_a_command_the_browser_never_answers_is_an_outage() {
    let fake = FakeChrome::start().await;
    let session = fake.session(fake.connection()).await;
    fake.block("Page.navigate");
    fake.block("Browser.getVersion");
    let error = page(&session).navigate("https://example.test/", None).await.unwrap_err();
    session.close().await.unwrap();
    assert!(matches!(error, BrowserError::BrowserUnresponsive(_)), "{error:?}");
    assert!(error.is_unavailable() && error.to_string().contains("Page.navigate got no reply"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: waits out the real CDP_REPLY_SECONDS, about 110s"]
async fn test_a_slow_command_from_a_live_browser_is_waited_for() {
    let fake = FakeChrome::start().await;
    let session = fake.session(fake.connection()).await;
    // Python shortens the reply wait to 0.05s against the two-minute command deadline, so the browser is asked many
    // times. The Rust waits are 60s and 120s, which leave room for one probe before the deadline.
    fake.delay("Target.getTargets", 110.0);
    session.client().unwrap().send("Target.getTargets", json!({}), None).await.unwrap();
    session.close().await.unwrap();
    assert!(fake.script().count("Browser.getVersion") >= 1, "the browser was asked whether it was there");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: waits out the real CDP_REPLY_SECONDS and CDP_ALIVE_SECONDS, about 70s"]
async fn test_a_reply_that_lands_during_the_liveness_probe_is_kept() {
    let fake = FakeChrome::start().await;
    let session = fake.session(fake.connection()).await;
    // The probe starts at 60s and gives up at 70s; the reply lands between.
    fake.delay("Target.getTargets", 65.0);
    fake.block("Browser.getVersion");
    session.client().unwrap().send("Target.getTargets", json!({}), None).await.unwrap();
    session.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cancelled_cdp_reply_is_drained_without_touching_other_requests() {
    for failed in [false, true] {
        let fake = FakeChrome::start().await;
        let session = fake.session(fake.connection()).await;
        let client = session.client().unwrap();
        fake.block("Target.getTargets");
        fake.block("Runtime.evaluate");
        // Python's request 999: another call waiting on the same connection.
        let waiting = tokio::spawn({
            let client = client.clone();
            async move { client.call("Target.getTargets", json!({}), None).await }
        });
        fake.sent("Target.getTargets").await;
        let request = tokio::spawn({
            let client = client.clone();
            async move { client.send("Runtime.evaluate", json!({"expression": "1"}), None).await }
        });
        fake.sent("Runtime.evaluate").await;
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        fake.release("Runtime.evaluate", if failed { Reply::Error(-32000) } else { Reply::Result(json!({})) });
        // Replies come in socket order, so once this one is back the late one has been read.
        client.send("Browser.getVersion", json!({}), None).await.unwrap();
        assert_eq!(client.pending(), 1, "failed={failed}");
        assert!(!waiting.is_finished());
        waiting.abort();
        session.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_attach_existing_tab_and_non_destructive_teardown() {
    // An attached window is driven where it is and left open on exit.
    let fake = FakeChrome::start().await;
    fake.queue(
        "Target.getTargets",
        [json!({"targetInfos": [
            {"targetId": "devtools", "type": "page", "url": "devtools://devtools/bundled/x.html", "title": "x"},
            {"targetId": "target_tab_1", "type": "page", "url": "http://example.com/app", "title": "My App"},
        ]})],
    );
    let connection = BrowserConnection { attach: true, ..BrowserConnection::new(fake.url.clone(), false) };
    let session = fake.session(connection).await;
    assert_eq!(session.active_target_id(), "target_tab_1");
    assert_eq!(session.tabs().into_iter().map(|t| t.id).collect::<Vec<_>>(), ["target_tab_1"]);
    {
        let script = fake.script();
        assert!(!script.calls.iter().any(|m| m == "Target.createTarget"));
        // Discovery replays every open window as created, so it may start only once they are known.
        assert!(script.index("Target.getTargets") < script.index("Target.setDiscoverTargets"));
        // The window's document loaded before the session, so its scripts run in it now as well as on navigation.
        assert!(script.calls.iter().any(|m| m == "Runtime.evaluate"));
    }
    assert!(testing::owned(&session).is_empty());
    session.close().await.unwrap();
    assert!(!fake.calls().iter().any(|m| m == "Target.closeTarget"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_attach_adopts_its_popups_and_windows_without_opener_only_when_matched() {
    // A window with no opener is an Electron app's, or a tab its user opened by hand in a browser.
    let cases: [(Option<&str>, &[&str]); 2] = [(None, &["popup"]), (Some("App"), &["popup", "window"])];
    for (target_match, adopted) in cases {
        let fake = FakeChrome::start().await;
        fake.queue(
            "Target.getTargets",
            [json!({"targetInfos": [
                {"targetId": "app", "type": "page", "url": "app://main", "title": "App"},
                {"targetId": "other", "type": "page", "url": "app://other", "title": "Other"},
            ]})],
        );
        fake.queue(
            "Target.getTargetInfo",
            [
                json!({"targetInfo": {"url": "app://popup", "title": "Popup"}}),
                json!({"targetInfo": {"url": "app://window", "title": "Window"}}),
            ],
        );
        let connection = BrowserConnection {
            attach: true,
            target_match: target_match.map(str::to_owned),
            ..BrowserConnection::new(fake.url.clone(), false)
        };
        let session = fake.session(connection).await;
        // Discovery replays "other", which was open before the session and has no opener either.
        for (target_id, opener) in
            [("other", None), ("popup", Some("app")), ("window", None), ("elsewhere", Some("unrelated"))]
        {
            let mut info = json!({"targetId": target_id, "type": "page", "url": "", "title": ""});
            if let Some(opener) = opener {
                info["openerId"] = opener.into();
            }
            testing::dispatch(&session, "Target.targetCreated", json!({"targetInfo": info}), None);
        }
        assert_eq!(session.popups(), adopted, "{target_match:?}");
        assert!(testing::popups(&session).iter().all(|(_, opener, _)| opener == "app"));
        testing::settle_background(&session).await;
        let mut tabs: Vec<String> = session.tabs().into_iter().map(|t| t.id).collect();
        tabs.sort();
        let mut expected: Vec<String> =
            std::iter::once("app").chain(adopted.iter().copied()).map(str::to_owned).collect();
        expected.sort();
        assert_eq!(tabs, expected);
        assert!(testing::owned(&session).is_empty());
        session.close().await.unwrap();
        assert!(!fake.calls().iter().any(|m| m == "Target.closeTarget"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_attach_target_matching_by_title_or_url() {
    let fake = FakeChrome::start().await;
    let pages = json!({"targetInfos": [
        {"targetId": "tab_1", "type": "page", "url": "http://example.com/blank", "title": "Blank Page"},
        {"targetId": "tab_2", "type": "page", "url": "http://example.com/editor", "title": "Main Editor"},
    ]});
    fake.queue("Target.getTargets", [pages.clone(), pages]);
    let connection = BrowserConnection {
        attach: true,
        target_match: Some("Editor".into()),
        ..BrowserConnection::new(fake.url.clone(), false)
    };
    let session = fake.session(connection.clone()).await;
    assert_eq!(session.active_target_id(), "tab_2");
    session.close().await.unwrap();

    let missing = BrowserConnection { target_match: Some("nonexistent_window".into()), ..connection };
    let error = fake.new_session(missing, SessionOptions::default()).open().await.unwrap_err();
    assert!(error.to_string().contains("no page matching 'nonexistent_window'"), "{error}");
}
