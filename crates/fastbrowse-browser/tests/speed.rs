mod common;
mod safety_support;

// tests/browser/test_speed.py: reproducible local timings and settling regressions, without model API calls. The
// Rust layer keeps each Python bound. Where Python shortens a module constant with monkeypatch, the Rust bound is
// the same expression of the real constant, read through a seam.
//
// Not ported: test_cancelled_browser_check_drains_renderer_and_dialog_waits and
// test_cancelled_observation_drains_all_browser_reads check that a cancelled asyncio task awaits the CDP calls it
// had in flight. A Rust future is cancelled by dropping it, which cannot await anything.

use std::net::TcpListener;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use common::{Harness, chrome, harness, harness_on, sites};
use data_encoding::HEXLOWER;
use fastbrowse_browser::adapters::local_chrome::LocalChromeProcess;
use fastbrowse_browser::page::scripts::PRESENTED_JS;
use fastbrowse_browser::session::SessionOptions;
use fastbrowse_browser::shared::{Action, BrowserConnection, Config, LocalChrome, Operation, StepOutcome};
use fastbrowse_browser::testing::safety as seams;
use safety_support::{
    CdpProxy, Reply, cdp_proxy, eval_active, find, has, observe_until, one_at_a_time, serve_on, wait_until,
};
use serde_json::json;
use tokio::time::{sleep, timeout};

fn targeted(operation: Operation, target_id: &str) -> Action {
    Action { target_id: Some(target_id.to_owned()), ..Action::new(operation) }
}

/// A session on the shared Chrome whose every CDP message goes through a logging proxy.
async fn proxied() -> Option<(Harness, CdpProxy)> {
    let connection = chrome()?;
    let proxy = cdp_proxy(&connection.cdp_url).await;
    let proxied = BrowserConnection { cdp_url: proxy.url.clone(), ..connection.clone() };
    let h = harness_on(proxied, Config::default(), SessionOptions::default()).await?;
    Some((h, proxy))
}

/// (resourceType, request url) of each `Fetch.requestPaused` the browser sent from the log's `start`.
fn paused_since(proxy: &CdpProxy, start: usize) -> Vec<(String, String)> {
    proxy
        .events_since(start, "Fetch.requestPaused")
        .iter()
        .map(|e| {
            let params = &e["params"];
            (
                params["resourceType"].as_str().unwrap_or_default().to_owned(),
                params["request"]["url"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn test_frame_wait_includes_later_callbacks_in_the_same_frame() {
    let _serial = one_at_a_time().await;
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    let script = format!(
        "(async () => {{ let changed = false; const presented = {PRESENTED_JS}; \
         requestAnimationFrame(() => requestAnimationFrame(() => {{ changed = true; }})); \
         await presented; return changed; }})()"
    );
    let active = h.session.active_session_id().expect("active");
    assert_eq!(h.page.evaluate(&active, &script).await.expect("evaluate"), json!(true));
    h.close().await;
}

/// The settling fixture, served once per binary as Python's module-scoped fixture is.
fn settling_site() -> &'static str {
    static SITE: OnceLock<String> = OnceLock::new();
    SITE.get_or_init(|| {
        let port = serve_on(
            TcpListener::bind("127.0.0.1:0").expect("bind"),
            Arc::new(|path: &str| match path {
                "/slow.svg" => Reply {
                    delay: Duration::from_secs_f64(1.2),
                    ..Reply::of("image/svg+xml", r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"/>"#)
                },
                "/parser.js" => Reply {
                    delay: Duration::from_secs_f64(0.35),
                    ..Reply::of("application/javascript", "document.title = 'Parsed';")
                },
                "/parsing" => Reply::html(r#"<!doctype html><script src="/parser.js"></script><button>Ready</button>"#),
                "/ready" => Reply::html(
                    r#"<!doctype html><title>Settling fixture</title>
                <button id="state">Stage 0</button><img src="/slow.svg">
                <script>
                for (const [i, delay] of [100, 280, 460].entries()) {
                  setTimeout(() => {
                    document.querySelector('button').textContent = 'Stage ' + (i + 1);
                  }, delay);
                }
                </script>"#,
                ),
                _ => Reply::html(r#"<!doctype html><title>Start</title><a href="/ready">Navigate</a>"#),
            }),
        );
        format!("http://127.0.0.1:{port}")
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn test_navigation_pauses_only_documents() {
    let _serial = one_at_a_time().await;
    let Some((h, proxy)) = proxied().await else { return };
    let start = proxy.len();
    h.page.navigate(&sites().main, None).await.expect("navigate");
    wait_until(|| async { !h.session.frame_sessions().is_empty() }, 5.0).await;
    sleep(Duration::from_secs_f64(0.25)).await;
    let paused = paused_since(&proxy, start);
    let continued = proxy.sent_since(start, "Fetch.continueRequest").len();
    assert!(paused.len() == continued && continued == 2, "{paused:?} {continued}");
    assert!(paused.iter().all(|(kind, _)| kind == "Document"), "{paused:?}");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_download_resource_types() {
    let _serial = one_at_a_time().await;
    let Some((h, proxy)) = proxied().await else { return };
    let start = proxy.len();
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(&h.session, "document.querySelector('#download-link').download = 'named.bin'").await;
    let obs = h.observe().await;
    h.page.act(&targeted(Operation::Click, &find(&obs, "Download attachment").id), &obs).await.expect("act");
    wait_until(|| async { !h.session.artifacts().is_empty() }, 5.0).await;
    let digest =
        ring::digest::digest(&ring::digest::SHA256, b"fastbrowse fixture attachment bytes for download checksum test");
    assert_eq!(h.session.artifacts()[0].sha256, HEXLOWER.encode(digest.as_ref()));
    let paused = paused_since(&proxy, start);
    assert!(paused.contains(&("Document".to_owned(), format!("{}/download", sites().main))), "{paused:?}");
    assert!(paused.iter().all(|(kind, _)| kind == "Document"), "{paused:?}");
    h.close().await;
}

async fn activate(h: &Harness) {
    h.session
        .send("Target.activateTarget", json!({"targetId": h.session.active_target_id()}), None)
        .await
        .expect("activate");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_settling_waits_out_staged_hydration() {
    let _serial = one_at_a_time().await;
    let Some(h) = harness().await else { return };
    activate(&h).await;
    h.page.navigate(settling_site(), None).await.expect("navigate");
    let obs = h.observe().await;
    let result = h.page.act(&targeted(Operation::Click, &find(&obs, "Navigate").id), &obs).await.expect("act");
    assert!(result.outcome == StepOutcome::Executed && result.page_changed, "{result:?}");
    find(&h.observe().await, "Stage 3");
    // The slow image is still loading: settling no longer waits for readyState complete.
    assert_eq!(eval_active(&h.session, "document.readyState").await, json!("interactive"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_readiness_waits_for_the_parser_in_one_call() {
    let _serial = one_at_a_time().await;
    let Some((h, proxy)) = proxied().await else { return };
    let session_id = h.session.active_session_id().expect("active");
    h.session
        .send("Page.navigate", json!({"url": format!("{}/parsing", settling_site())}), Some(&session_id))
        .await
        .expect("Page.navigate");
    let start = proxy.len();
    assert!(h.page.ready(&session_id, 2.0).await.expect("ready"));
    assert_eq!(proxy.page_evaluations_since(start), 1);
    let observation = h.observe().await;
    assert_eq!(observation.title, "Parsed");
    find(&observation, "Ready");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_settling_tracks_same_length_mutations() {
    let _serial = one_at_a_time().await;
    for nested in ["document", "shadow", "iframe"] {
        let Some(h) = harness().await else { return };
        activate(&h).await;
        h.page.navigate(settling_site(), None).await.expect("navigate");
        let root = match nested {
            "document" => "document.querySelector('div')",
            "shadow" => "document.querySelector('div').attachShadow({mode: 'open'})",
            _ => "document.querySelector('iframe').contentDocument.body",
        };
        eval_active(
            &h.session,
            &format!(
                "document.body.innerHTML = '<button>Change</button><div></div><iframe></iframe>'; \
                 const root = {root}; root.innerHTML = '<p>State 0</p>'; \
                 document.querySelector('button').onclick = () => {{ \
                 for (const [i, delay] of [100, 280, 460].entries()) \
                 setTimeout(() => {{ root.querySelector('p').textContent = 'State ' + (i + 1); }}, delay); }}; \
                 window.fixtureRoot = root;"
            ),
        )
        .await;
        let obs = h.observe().await;
        let result = h.page.act(&targeted(Operation::Click, &find(&obs, "Change").id), &obs).await.expect("act");
        assert_eq!(result.outcome, StepOutcome::Executed, "{nested}");
        assert!(result.page_changed, "{nested}");
        assert_eq!(eval_active(&h.session, "window.fixtureRoot.textContent").await, json!("State 3"), "{nested}");
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_redraw_after_settling_is_seen_before_acting() {
    let _serial = one_at_a_time().await;
    // Google Flights' date picker draws its prices about 400ms after it has gone quiet, which changes the text
    // authorizing its Done button: a decision on the quiet picker would be refused as stale.
    let Some(h) = harness().await else { return };
    h.page.navigate(settling_site(), None).await.expect("navigate");
    eval_active(
        &h.session,
        "document.body.innerHTML = '<div role=\"dialog\"><span>19</span><button>Done</button></div>'; \
         setTimeout(() => { document.querySelector('span').textContent = '19 $500'; }, 400);",
    )
    .await;
    let blank = h.observe().await;
    assert!(h.page.redrawn(&blank, 3.0, Some(&find(&blank, "Done").id)).await.expect("redrawn"));
    let priced = h.observe().await;
    assert!(!h.page.redrawn(&priced, 0.3, None).await.expect("redrawn"));
    let done = h.page.act(&targeted(Operation::Click, &find(&priced, "Done").id), &priced).await.expect("act");
    assert_eq!(done.outcome, StepOutcome::Executed, "{:?}", done.detail);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_observe_still_reads_hydration_after_settling() {
    let _serial = one_at_a_time().await;
    let Some(h) = harness().await else { return };
    h.page.navigate(settling_site(), None).await.expect("navigate");
    eval_active(
        &h.session,
        "document.body.innerHTML = '<button>Hydrate later</button>'; \
         document.querySelector('button').onclick = () => setTimeout(() => { \
         document.querySelector('button').outerHTML = '<button>Hydrated now</button>'; }, 700);",
    )
    .await;
    let before = h.observe().await;
    let target = find(&before, "Hydrate later").clone();
    h.page.act(&targeted(Operation::Click, &target.id), &before).await.expect("act");
    let after = observe_until(&h.page, "Hydrated now").await;
    assert_ne!(after.page_key, before.page_key);
    assert_ne!(find(&after, "Hydrated now").id, target.id);
    assert_eq!(
        h.page.act(&targeted(Operation::Click, &target.id), &before).await.expect("act").outcome,
        StepOutcome::Stale
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_navigation_after_observe_rejects_old_control() {
    let _serial = one_at_a_time().await;
    let Some(h) = harness().await else { return };
    h.page.navigate(settling_site(), None).await.expect("navigate");
    let obs = h.observe().await;
    let target = find(&obs, "Navigate").clone();
    h.page.navigate(&format!("{}/?new-document", settling_site()), None).await.expect("navigate");
    let result = h.page.act(&targeted(Operation::Click, &target.id), &obs).await.expect("act");
    assert!(result.outcome == StepOutcome::Stale && !result.page_changed, "{result:?}");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_hidden_tab_requires_dom_quiet() {
    let _serial = one_at_a_time().await;
    let Some(h) = harness().await else { return };
    h.page.navigate(settling_site(), None).await.expect("navigate");
    h.observe().await;
    let other = h
        .session
        .send("Target.createTarget", json!({"url": "about:blank", "background": false}), None)
        .await
        .expect("createTarget");
    let checked = async {
        assert_eq!(eval_active(&h.session, "document.hidden").await, json!(true));
        eval_active(&h.session, "document.body.setAttribute('data-state', 'new')").await;
        let (stable, fingerprint) = h.page.settled_fingerprint(1.0).await.expect("fingerprint");
        assert!(!stable && fingerprint.is_some());
        sleep(Duration::from_secs_f64(0.25)).await;
        let (stable, current) = h.page.settled_fingerprint(1.0).await.expect("fingerprint");
        assert!(stable && current == fingerprint);
    };
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(checked)).await;
    let _ = h.session.send("Target.closeTarget", json!({"targetId": other["targetId"]}), None).await;
    h.close().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_continuous_mutations_are_bounded() {
    let _serial = one_at_a_time().await;
    // Python shortens _SETTLE_SECONDS to 0.5 and bounds the act at 1.5s; the bound here is the same, settle plus
    // one second, on the real constant.
    let Some(h) = harness().await else { return };
    activate(&h).await;
    h.page.navigate(settling_site(), None).await.expect("navigate");
    eval_active(
        &h.session,
        "document.body.innerHTML = '<button>Animate</button>'; \
         document.querySelector('button').onclick = () => { \
         window.animation = setInterval(() => { document.body.dataset.tick = performance.now(); }, 30); };",
    )
    .await;
    let obs = h.observe().await;
    let started = Instant::now();
    let result = h.page.act(&targeted(Operation::Click, &find(&obs, "Animate").id), &obs).await.expect("act");
    let elapsed = started.elapsed().as_secs_f64();
    eval_active(&h.session, "clearInterval(window.animation)").await;
    assert!(result.outcome == StepOutcome::Executed && result.page_changed, "{result:?}");
    assert!(elapsed < seams::settle_seconds() + 1.0, "{elapsed}");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_deferred_input_changes_are_settled_before_observation() {
    let _serial = one_at_a_time().await;
    for operation in [Operation::Click, Operation::Hover, Operation::Enter] {
        let Some(h) = harness().await else { return };
        h.page.navigate(&sites().main, None).await.expect("navigate");
        let handler = if operation == Operation::Hover { "onpointerenter" } else { "onclick" };
        eval_active(
            &h.session,
            &format!(
                "document.body.innerHTML = '<button>Change</button><p>Before</p>'; \
                 const change = () => setTimeout(() => {{ document.querySelector('p').textContent = 'After'; }}, 150); \
                 const button = document.querySelector('button'); \
                 button.{handler} = change;"
            ),
        )
        .await;
        let obs = h.observe().await;
        sleep(Duration::from_secs_f64(0.25)).await;
        let result = h.page.act(&targeted(operation, &find(&obs, "Change").id), &obs).await.expect("act");
        assert!(result.outcome == StepOutcome::Executed && result.page_changed, "{operation:?} {result:?}");
        assert!(h.observe().await.viewport_text.contains("After"), "{operation:?}");
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_noop_input_still_waits_for_quiet() {
    let _serial = one_at_a_time().await;
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(&h.session, "document.body.innerHTML = '<button>Idle</button>'").await;
    let obs = h.observe().await;
    let result = h.page.act(&targeted(Operation::Click, &find(&obs, "Idle").id), &obs).await.expect("act");
    assert!(result.outcome == StepOutcome::Executed && !result.page_changed, "{result:?}");
    let quiet = eval_active(&h.session, "performance.now() - window.__fastbrowse.lastMutation").await;
    let quiet = quiet.as_f64().expect("a number");
    assert!(quiet >= seams::settle_quiet_seconds() * 1000.0, "{quiet}");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_capture_waits_for_loading_in_the_same_renderer_call() {
    let _serial = one_at_a_time().await;
    let Some((h, proxy)) = proxied().await else { return };
    h.goto("/dispatch.html").await;
    eval_active(
        &h.session,
        "document.body.innerHTML = '<p aria-busy=true>Loading</p>'; \
         setTimeout(() => { const p = document.querySelector('p'); \
         p.removeAttribute('aria-busy'); p.textContent = 'Ready to read'; }, 150);",
    )
    .await;
    let start = proxy.len();
    let capture = h.page.capture().await.expect("capture");
    assert!(capture.text.contains("Ready to read") && !capture.text.contains("Loading"), "{}", capture.text);
    assert_eq!(proxy.page_evaluations_since(start), 1);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_observe_waits_for_loading_before_reading_controls() {
    let _serial = one_at_a_time().await;
    let Some(h) = harness().await else { return };
    h.goto("/dispatch.html").await;
    eval_active(
        &h.session,
        "document.body.innerHTML = '<div aria-busy=true>Loading results</div>'; \
         setTimeout(() => { const d = document.querySelector('div'); d.removeAttribute('aria-busy'); \
         d.innerHTML = '<button>Nonstop result</button>'; }, 150);",
    )
    .await;
    let observation = h.observe().await;
    assert_eq!(observation.controls.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["Nonstop result"]);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_observe_reports_a_dialog_that_opens_while_it_waits_for_loading() {
    let _serial = one_at_a_time().await;
    let Some(h) = harness().await else { return };
    h.goto("/dispatch.html").await;
    eval_active(
        &h.session,
        "document.body.innerHTML = '<div aria-busy=true>Loading results</div>'; \
         setTimeout(() => alert('Session expired'), 150);",
    )
    .await;
    // The dialog blocks the snapshot's evaluate, which otherwise waits out the 120s command deadline.
    let observation = timeout(Duration::from_secs(10), h.page.observe()).await.expect("in time").expect("observe");
    assert_eq!(observation.dialog.as_ref().map(|d| d.message.as_str()), Some("Session expired"));
    h.session.handle_dialog(true, None).await.expect("dialog");
    h.close().await;
}

/// A page whose cross-site frame stops answering: its renderer spins once the test asks it to.
fn hung_frame_site() -> &'static str {
    static SITE: OnceLock<String> = OnceLock::new();
    SITE.get_or_init(|| {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        serve_on(
            listener,
            Arc::new(move |path: &str| match path {
                // A tab that has stopped drawing calls no animation frame callback, however long it is waited on.
                "/no-frames" => Reply::html(
                    "<!doctype html><script>window.requestAnimationFrame = () => 0;</script>\
                     <button onclick=\"this.textContent = 'Opened'\">Open</button>",
                ),
                "/frame" => Reply::html(
                    "<!doctype html><p>Advertisement</p><script>\
                     window.addEventListener('message', () => { while (true) {} });</script>",
                ),
                _ => Reply::html(format!(
                    "<!doctype html><title>Sign in</title><label>Email <input id=email></label>\
                     <button>Sign in</button><iframe src='http://localhost:{port}/frame'></iframe>"
                )),
            }),
        );
        format!("http://127.0.0.1:{port}/")
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_frame_that_stops_answering_costs_one_short_deadline() {
    let _serial = one_at_a_time().await;
    // On a Chrome of its own: every localhost frame shares a renderer, and the spinning one would stall the frames of
    // the tests running beside it.
    if chrome().is_none() {
        return;
    }
    let (mut process, connection) = LocalChromeProcess::launch(&LocalChrome::default()).await.expect("Chrome launches");
    let h = harness_on(connection, Config::default(), SessionOptions::default()).await.expect("harness");
    h.page.navigate(hung_frame_site(), None).await.expect("navigate");
    wait_until(|| async { !h.session.frame_sessions().is_empty() }, 5.0).await;
    observe_until(&h.page, "Sign in").await;
    // The frame's renderer is busy from here on, as a stuck advertisement's is.
    eval_active(&h.session, "document.querySelector('iframe').contentWindow.postMessage(1, '*')").await;
    let started = Instant::now();
    let observation = h.observe().await;
    let elapsed = started.elapsed().as_secs_f64();
    assert!(elapsed < seams::frame_read_seconds() + 2.0, "{elapsed}");
    // The page's own controls are still read, and the frame is reported unread rather than silently empty.
    assert!(has(&observation, "Sign in") && observation.inaccessible_frames == 1, "{observation:?}");
    let capture = h.page.capture().await.expect("capture");
    assert!(capture.text.contains("Sign in") && capture.inaccessible_frames == 1);
    // Once a frame has missed its deadline, later reads do not wait on it again.
    let started = Instant::now();
    h.observe().await;
    let elapsed = started.elapsed().as_secs_f64();
    assert!(elapsed < 1.5, "{elapsed}");
    // Past the recheck window it gets the full budget again, so a frame that recovered slow is still read. Python
    // sets the window to zero and the budget to 2s; here the window is made to have passed, and the budget is the
    // real one.
    seams::expire_frame_rechecks(&h.page);
    let started = Instant::now();
    h.observe().await;
    let elapsed = started.elapsed().as_secs_f64();
    assert!(elapsed >= seams::frame_read_seconds(), "{elapsed}");
    h.close().await;
    process.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_settling_does_not_wait_on_a_frame_that_never_comes() {
    let _serial = one_at_a_time().await;
    let Some(h) = harness().await else { return };
    timeout(Duration::from_secs(15), async {
        h.page.navigate(&format!("{}no-frames", hung_frame_site()), None).await.expect("navigate");
        let observation = h.observe().await;
        let result =
            h.page.act(&targeted(Operation::Click, &find(&observation, "Open").id), &observation).await.expect("act");
        assert!(result.outcome == StepOutcome::Executed && result.page_changed, "{result:?}");
        let opened = h.observe().await;
        find(&opened, "Opened");
        eval_active(&h.session, "document.querySelector('button').textContent = 'Shut'").await;
        assert!(h.page.redrawn(&opened, 1.0, None).await.expect("redrawn"));
    })
    .await
    .expect("in time");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_page_whose_timers_never_fire_cannot_hold_the_pointer() {
    let _serial = one_at_a_time().await;
    for timer in ["() => 0", "() => { throw new Error('no timers'); }"] {
        let Some(h) = harness().await else { return };
        h.page.navigate(&format!("{}no-frames", hung_frame_site()), None).await.expect("navigate");
        let observation = h.observe().await;
        // The frame wait before the pointer moves runs on page timers, which a page is free to replace.
        eval_active(&h.session, &format!("window.setTimeout = {timer}; true")).await;
        let action = targeted(Operation::Hover, &find(&observation, "Open").id);
        let hover = h.page.act(&action, &observation);
        let result = timeout(Duration::from_secs(15), hover).await.expect("in time").expect("act");
        assert_eq!(result.outcome, StepOutcome::Executed, "{timer}");
        h.close().await;
    }
}
