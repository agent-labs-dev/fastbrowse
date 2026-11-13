mod common;
mod safety_support;

// Reproducible local timings and settling regressions, without model API calls. The
// Rust layer keeps each Python bound. Where Python shortens a module constant with monkeypatch, the Rust bound is
// the same expression of the real constant, read through a seam.
//
// Not ported: test_cancelled_browser_check_drains_renderer_and_dialog_waits and
// test_cancelled_observation_drains_all_browser_reads check that a cancelled asyncio task awaits the CDP calls it
// had in flight. A Rust future is cancelled by dropping it, which cannot await anything.

use std::net::TcpListener;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use common::{Harness, chrome, eval_active, eval_value, find, harness, harness_on, sites, turn, wait_until};
use data_encoding::HEXLOWER;
use fastbrowse_browser::adapters::local_chrome::LocalChromeProcess;
use fastbrowse_browser::page::scripts::PRESENTED_JS;
use fastbrowse_browser::session::SessionOptions;
use fastbrowse_browser::testing::safety as seams;
use fastbrowse_core::config::Config;
use fastbrowse_core::models::{BrowserConnection, LocalChrome, Operation, StepOutcome};
use fastbrowse_core::page::{Action, Observation};
use safety_support::{CdpProxy, Reply, cdp_proxy, has, observe_until, serve_on};
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
    let _turn = turn().await;
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
    let _turn = turn().await;
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
    let _turn = turn().await;
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
async fn test_readiness_waits_for_the_parser_in_one_call() {
    let _turn = turn().await;
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
async fn test_redraw_after_settling_is_seen_before_acting() {
    let _turn = turn().await;
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
    let _turn = turn().await;
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
    let _turn = turn().await;
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
async fn test_hidden_tab_answers_with_no_idle_period() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.page.navigate(settling_site(), None).await.expect("navigate");
    h.observe().await;
    let other = h
        .session
        .send("Target.createTarget", json!({"url": "about:blank", "background": false}), None)
        .await
        .expect("createTarget");
    // A run keeps its tabs visible with focus emulation, which a tab loses only when that is off.
    let active = h.session.active_session_id().expect("active session");
    h.session.send("Emulation.setFocusEmulationEnabled", json!({"enabled": false}), Some(&active)).await.expect("off");
    let checked = async {
        assert_eq!(eval_active(&h.session, "document.hidden").await, json!(true));
        eval_active(&h.session, "document.body.setAttribute('data-state', 'new')").await;
        let (stable, fingerprint) = h.page.settled_fingerprint().await.expect("fingerprint");
        assert!(stable && fingerprint.is_some());
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
    let _turn = turn().await;
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
    assert_eq!(result.outcome, StepOutcome::Executed, "{result:?}");
    assert!(elapsed < seams::settle_seconds() + 1.0, "{elapsed}");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_noop_input_reports_no_change() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(&h.session, "document.body.innerHTML = '<button>Idle</button>'").await;
    let obs = h.observe().await;
    let result = h.page.act(&targeted(Operation::Click, &find(&obs, "Idle").id), &obs).await.expect("act");
    assert!(result.outcome == StepOutcome::Executed && !result.page_changed, "{result:?}");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_settle_tracks_click_work_when_release_is_a_later_task() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.page.navigate(settling_site(), None).await.expect("navigate");
    eval_active(
        &h.session,
        "document.body.innerHTML = '<button>Start</button>'; \
         document.querySelector('button').onclick = () => setTimeout(() => { \
         document.querySelector('button').textContent = 'Finished'; }, 150);",
    )
    .await;
    h.observe().await;
    let point = eval_active(&h.session, "(() => { const r = document.querySelector('button').getBoundingClientRect(); return {x:r.x+r.width/2,y:r.y+r.height/2}; })()").await;
    let active = h.session.active_session_id().expect("active");
    for event in ["mousePressed", "mouseReleased"] {
        h.session
            .send(
                "Input.dispatchMouseEvent",
                json!({"type":event,"x":point["x"],"y":point["y"],"button":"left","clickCount":1}),
                Some(&active),
            )
            .await
            .expect("mouse event");
        if event == "mousePressed" {
            h.page
                .evaluate(&active, "new Promise(resolve => __fastbrowse.nativeSetTimeout(resolve, 0))")
                .await
                .expect("next task");
            assert_eq!(eval_active(&h.session, "__fastbrowse.turn").await, json!(null));
        }
    }
    h.page.settled_fingerprint().await.expect("settled");
    assert_eq!(eval_active(&h.session, "document.querySelector('button').textContent").await, json!("Finished"));
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
    let _turn = turn().await;
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
    // The frame's renderer is busy from here on, as a stuck advertisement's is. The message is delivered later, so the
    // read is timed only once the frame has stopped answering.
    let (_, frame) = h.session.frame_sessions().into_iter().next().expect("the frame's session");
    eval_active(&h.session, "document.querySelector('iframe').contentWindow.postMessage(1, '*')").await;
    let answers = || async { timeout(Duration::from_millis(250), eval_value(&h.session, &frame, "1")).await.is_ok() };
    wait_until(|| async { !answers().await }, 5.0).await;
    let started = Instant::now();
    let observation = h.observe().await;
    let elapsed = started.elapsed().as_secs_f64();
    assert!(elapsed < seams::frame_read_seconds() + 2.0, "{elapsed}");
    // The page's own controls are still read, and the frame is reported unread rather than silently empty.
    assert!(has(&observation, "Sign in") && observation.inaccessible_frames == 1, "{observation:?}");
    assert!(
        observation
            .viewport_text
            .ends_with("[1 frame on this page could not be read: its text and controls are not shown]"),
        "{:?}",
        observation.viewport_text
    );
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
    let _turn = turn().await;
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
    let _turn = turn().await;
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

#[tokio::test(flavor = "multi_thread")]
async fn test_an_endless_animation_a_click_starts_does_not_hold_the_action() {
    let Some(h) = harness().await else { return };
    common::eval_active(
        &h.session,
        "document.body.innerHTML = `<canvas></canvas><button>Start</button>`; \
         document.querySelector('button').onclick = () => { const spin = () => requestAnimationFrame(spin); spin(); \
         document.querySelector('canvas').dataset.on = '1'; };",
    )
    .await;
    let obs = h.observe().await;
    let started = std::time::Instant::now();
    h.page.act(&targeted(Operation::Click, &find(&obs, "Start").id), &obs).await.expect("act");
    assert!(started.elapsed().as_secs_f64() < 2.0, "{:?}", started.elapsed());
    h.close().await;
}

/// Settle reads the page's own events: a read sent to the page resolves when the page itself reports going quiet.
fn settle_reads_since(proxy: &CdpProxy, start: usize) -> usize {
    proxy
        .sent_since(start, "Runtime.evaluate")
        .iter()
        .filter(|m| m["params"]["expression"].as_str().is_some_and(|e| e.contains("fastbrowse:settle")))
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_settle_does_not_re_read_for_each_of_the_pages_writes() {
    let _turn = turn().await;
    // #946: settlement is the page's event stream, not a sample repeated until two reads agree. A page that writes
    // twenty times over 300ms after the click costs one wait in the page, whatever its write count; the poll this
    // replaces read once per sample, so a longer write burst cost more reads than a short one.
    let Some((h, proxy)) = proxied().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        "document.body.innerHTML = '<button>Write</button>'; \
         document.querySelector('button').onclick = () => { \
           for (let i = 1; i <= 20; i++) \
             setTimeout(() => { document.querySelector('button').textContent = 'Wrote ' + i; }, i * 15); };",
    )
    .await;
    let obs = h.observe().await;
    let start = proxy.len();
    let begun = Instant::now();
    let result = h.page.act(&targeted(Operation::Click, &find(&obs, "Write").id), &obs).await.expect("act");
    let elapsed = begun.elapsed().as_secs_f64();
    assert_eq!(result.outcome, StepOutcome::Executed, "{result:?}");
    assert!(result.page_changed, "{result:?}");
    // The last write was 300ms after the click, and the quiet window runs from it.
    assert!(elapsed >= 0.3, "settled before the page's last write: {elapsed}");
    assert!(elapsed < 2.0, "{elapsed}");
    // A lifecycle event can wake the loop once more; what must not happen is a read per write.
    let reads = settle_reads_since(&proxy, start);
    assert!(reads <= 2, "the settle re-read the page {reads} times for 20 writes");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_page_generated_click_does_not_claim_background_work_as_input() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        "document.body.innerHTML = '<button>Background</button>'; \
         document.querySelector('button').onclick = () => { \
           window.backgroundTimer = setTimeout(() => {}, 60000); };",
    )
    .await;
    h.observe().await;
    let before = eval_active(&h.session, "[__fastbrowse.action, __fastbrowse.lastInput]").await;
    eval_active(&h.session, "document.querySelector('button').click()").await;
    let after = eval_active(&h.session, "[__fastbrowse.action, __fastbrowse.lastInput]").await;
    let pending = eval_active(&h.session, "__fastbrowse.pendingWork.size").await;
    eval_active(&h.session, "clearTimeout(window.backgroundTimer)").await;
    h.close().await;
    assert_eq!(after, before, "a page-generated click is not the run's input");
    assert_eq!(pending, json!(0), "background work must not become action work");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_settle_waits_for_work_the_click_scheduled_before_its_first_mutation() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        "document.body.innerHTML = '<button>Delayed</button>'; \
         document.querySelector('button').onclick = () => { \
           setTimeout(() => { document.querySelector('button').textContent = 'Finished'; }, 150); };",
    )
    .await;
    let obs = h.observe().await;
    let result = h.page.act(&targeted(Operation::Click, &find(&obs, "Delayed").id), &obs).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Executed, "{result:?}");
    assert!(result.page_changed, "the click's scheduled work was still pending: {result:?}");
    let after = h.observe().await;
    assert!(after.controls.iter().any(|control| control.label == "Finished"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_settle_waits_for_nested_input_work_and_ignores_cancelled_timers() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        "document.body.innerHTML = '<button>Nested</button>'; \
         document.querySelector('button').onclick = () => { \
           const cancelled = setTimeout(() => { throw new Error('cancelled'); }, 10000); \
           clearInterval(String(cancelled)); \
           requestAnimationFrame(() => { \
             setTimeout(() => { \
               Promise.resolve().then(() => { \
                 setTimeout(value => { document.querySelector('button').textContent = value; }, 150, 'Nested finished'); \
               }); \
             }, 0); \
           }); \
         };",
    )
    .await;
    let obs = h.observe().await;
    let result = h.page.act(&targeted(Operation::Click, &find(&obs, "Nested").id), &obs).await.expect("act");
    assert!(result.page_changed, "{result:?}");
    let after = h.observe().await;
    assert!(after.controls.iter().any(|control| control.label == "Nested finished"));
    let pending = eval_active(&h.session, "window.__fastbrowse.pendingWork.size").await;
    assert_eq!(pending, json!(0), "cancelled timers must release their pending work");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_loop_an_earlier_click_started_does_not_hold_a_later_idle_click() {
    let _turn = turn().await;
    // A poll that reschedules itself and a frame chain are the first click's work: the second click scheduled
    // nothing, so its settle answers at once, as on an idle page.
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        "document.body.innerHTML = '<button>Loop</button><button>Idle</button>'; \
         document.querySelector('button').onclick = () => { \
           const poll = () => setTimeout(poll, 50); poll(); \
           const spin = () => requestAnimationFrame(spin); spin(); };",
    )
    .await;
    let obs = h.observe().await;
    h.page.act(&targeted(Operation::Click, &find(&obs, "Loop").id), &obs).await.expect("loop");
    let obs = h.observe().await;
    let begun = Instant::now();
    let result = h.page.act(&targeted(Operation::Click, &find(&obs, "Idle").id), &obs).await.expect("idle");
    let elapsed = begun.elapsed().as_secs_f64();
    assert_eq!(result.outcome, StepOutcome::Executed, "{result:?}");
    assert!(!result.page_changed, "{result:?}");
    assert!(elapsed < 0.25, "the earlier click's loop held this one: {elapsed}");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_settle_reads_a_write_two_frames_after_the_click() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        "document.body.innerHTML = '<button>Frames</button>'; \
         document.querySelector('button').onclick = () => requestAnimationFrame(() => requestAnimationFrame(() => { \
           document.querySelector('button').textContent = 'Drawn'; }));",
    )
    .await;
    let obs = h.observe().await;
    let result = h.page.act(&targeted(Operation::Click, &find(&obs, "Frames").id), &obs).await.expect("act");
    assert!(result.page_changed, "{result:?}");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_settle_does_not_wait_for_a_timer_due_after_its_deadline() {
    let _turn = turn().await;
    // A timer due after the settle's deadline cannot change what the settle reads, so it holds nothing.
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        "document.body.innerHTML = '<button>Later</button>'; \
         document.querySelector('button').onclick = () => setTimeout(() => {}, 60000);",
    )
    .await;
    let obs = h.observe().await;
    let begun = Instant::now();
    let result = h.page.act(&targeted(Operation::Click, &find(&obs, "Later").id), &obs).await.expect("act");
    let elapsed = begun.elapsed().as_secs_f64();
    assert!(!result.page_changed, "{result:?}");
    assert!(elapsed < 0.25, "a timer due after the deadline held the settle: {elapsed}");
    assert_eq!(eval_active(&h.session, "window.__fastbrowse.pendingWork.size").await, json!(1));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_settle_of_an_idle_page_pays_no_quiet_window() {
    let _turn = turn().await;
    // A page that was already quiet before the click has nothing to wait for, so the settle answers at once rather
    // than charging every action a fixed quiet window.
    let Some((h, proxy)) = proxied().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(&h.session, "document.body.innerHTML = '<button>Idle</button>'; setTimeout(() => {}, 10000);").await;
    let obs = h.observe().await;
    sleep(Duration::from_millis(300)).await;
    let start = proxy.len();
    let begun = Instant::now();
    let result = h.page.act(&targeted(Operation::Click, &find(&obs, "Idle").id), &obs).await.expect("act");
    let elapsed = begun.elapsed().as_secs_f64();
    assert_eq!(result.outcome, StepOutcome::Executed, "{result:?}");
    assert!(!result.page_changed, "{result:?}");
    assert!(elapsed < 0.25, "an idle page paid a quiet window: {elapsed}");
    assert_eq!(settle_reads_since(&proxy, start), 1, "the settle re-read the page");
    h.close().await;
}

// -- an unchanged frame is served from the read it kept, and never serves a page that has moved ---------------

/// Wrap the installed snapshot function so a test can count the reads the page makes and how many of them the page
/// answered from the read it kept. `CdpPage` looks the function up in that slot on every read, so the wrapper sees them
/// all. Called after an observation, which is what installs the script in the document.
async fn count_reads(h: &Harness) {
    common::eval_active(
        &h.session,
        "(() => { const scripts = globalThis.__fastbrowseScripts; const key = Object.keys(scripts)[0]; \
         const installed = scripts[key]; globalThis.__fbReads = { total: 0, unchanged: 0, delta: 0 }; \
         scripts[key] = (mode, base) => { const read = installed(mode, base); globalThis.__fbReads.total++; \
         if (read && read.unchanged) globalThis.__fbReads.unchanged++; \
         if (read && read.control_delta) globalThis.__fbReads.delta++; return read; }; })()",
    )
    .await;
}

/// How many reads the main frame has made, and how many it answered from the read it kept.
async fn reads(h: &Harness) -> (i64, i64) {
    let value = common::eval_active(&h.session, "globalThis.__fbReads").await;
    (value["total"].as_i64().unwrap_or(-1), value["unchanged"].as_i64().unwrap_or(-1))
}

#[tokio::test(flavor = "multi_thread")]
async fn test_an_unchanged_page_is_served_from_the_read_it_kept() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    common::eval_active(&h.session, "document.body.innerHTML = '<button>One</button><button>Two</button>';").await;
    let first = h.observe().await;
    count_reads(&h).await;

    let second = h.observe().await;
    let (total, unchanged) = reads(&h).await;
    assert_eq!(total, 1, "one frame was read");
    assert_eq!(unchanged, 1, "the second observation of an unchanged page was read again: {:?}", reads(&h).await);
    // What a reuse answers with is the read it kept, so the two observations say the same thing.
    assert_eq!(
        first.controls.iter().map(|c| (c.label.clone(), c.id.clone())).collect::<Vec<_>>(),
        second.controls.iter().map(|c| (c.label.clone(), c.id.clone())).collect::<Vec<_>>()
    );
    assert_eq!(first.page_key, second.page_key);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_changed_card_keeps_its_siblings_structural_queries() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(&h.session, r#"document.body.innerHTML = '<article id=changed><h2>First</h2><button>Open</button></article><article id=stable><h2>Second</h2><button>Open</button></article>';globalThis.stableQueries = 0;const original = Element.prototype.querySelectorAll;Element.prototype.querySelectorAll = function(selector) {if(this.id === 'stable') globalThis.stableQueries++; return original.call(this, selector)}"#).await;
    let before = h.observe().await;
    assert!(eval_active(&h.session, "globalThis.stableQueries").await.as_u64().unwrap() > 0);
    eval_active(
        &h.session,
        "globalThis.stableQueries = 0; document.querySelector('#changed h2').textContent = 'Updated'",
    )
    .await;
    let after = h.observe().await;
    assert!(before.controls.iter().any(|c| c.context.as_deref() == Some("First")));
    assert!(after.controls.iter().any(|c| c.context.as_deref() == Some("Updated")));
    assert!(after.controls.iter().any(|c| c.context.as_deref() == Some("Second")));
    assert_eq!(eval_active(&h.session, "globalThis.stableQueries").await, json!(0));
    eval_active(&h.session, "globalThis.savedCard = document.querySelector('#stable'); savedCard.remove(); true").await;
    eval_active(&h.session, "savedCard.innerHTML = '<p>Wrong fallback</p><h2>Replacement</h2><button>Open</button>'")
        .await;
    eval_active(&h.session, "document.body.appendChild(savedCard); true").await;
    let reinserted = h.observe().await;
    assert!(reinserted.controls.iter().any(|c| c.context.as_deref() == Some("Replacement")));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_changed_subtree_reuses_the_document_walk() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(&h.session, r#"document.body.innerHTML = '<article id=changed><button>First</button></article><article id=stable><button>Second</button></article>';globalThis.documentWalks = 0;globalThis.stableChildReads = 0;const original = Document.prototype.querySelectorAll;Document.prototype.querySelectorAll = function(selector) {if(selector === '*') documentWalks++;return original.call(this,selector)};const children = Object.getOwnPropertyDescriptor(Element.prototype,'firstElementChild');Object.defineProperty(Element.prototype,'firstElementChild',{...children,get(){if(this.id === 'stable')stableChildReads++;return children.get.call(this)}});true"#).await;
    h.observe().await;
    assert!(eval_active(&h.session, "stableChildReads").await.as_u64().unwrap() > 0);
    for script in [
        "document.querySelector('#changed').insertAdjacentHTML('beforeend','<button>Added</button>')",
        "document.querySelector('#changed').prepend(document.querySelector('#changed').lastChild)",
        "document.querySelector('#changed').firstChild.remove()",
        "document.querySelector('#changed button').firstChild.replaceWith(document.createTextNode('Replacement'))",
        "document.querySelector('#changed').insertAdjacentHTML('beforeend','<a id=changing-link>Link</a>')",
        "document.querySelector('#changing-link').setAttribute('href','#destination')",
        "document.querySelector('#changing-link').removeAttribute('href')",
        "globalThis.detachedCard = document.querySelector('#changed');detachedCard.remove();true",
        "detachedCard.innerHTML = '<span role=button>Detached replacement</span>';document.body.prepend(detachedCard)",
        "globalThis.shadowHost = document.createElement('div');document.body.append(shadowHost);shadowHost.attachShadow({mode:'open'}).innerHTML = '<button>Shadow first</button>'",
        "shadowHost.shadowRoot.prepend(Object.assign(document.createElement('button'),{textContent:'Shadow added'}))",
        "document.documentElement.append(Object.assign(document.createElement('button'),{textContent:'After body'}))",
        "document.body.replaceWith(Object.assign(document.createElement('body'),{innerHTML:'<button>New body</button>'}))",
    ] {
        eval_active(&h.session, "documentWalks = 0;stableChildReads = 0").await;
        eval_active(&h.session, script).await;
        let after = h.observe().await;
        assert_eq!(eval_active(&h.session, "documentWalks").await, json!(0), "{script}");
        assert_eq!(eval_active(&h.session, "stableChildReads").await, json!(0), "{script}");
        let other = fastbrowse_browser::page::CdpPage::new(h.session.clone(), Config::default());
        let mut full = other.observe().await.expect("full read");
        full.captured_at = after.captured_at;
        assert_eq!(after, full, "{script}");
        h.observe().await;
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_changed_name_does_not_derive_an_unrelated_name_again() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(&h.session, r#"document.body.innerHTML = '<button id=changed-name>First</button><button id=stable-name>Second</button>';globalThis.stableNameReads = 0;const original = Object.getOwnPropertyDescriptor(Node.prototype, 'childNodes');Object.defineProperty(Node.prototype, 'childNodes', {...original, get() {if(this.id === 'stable-name') globalThis.stableNameReads++;return original.get.call(this)}});true"#).await;
    h.observe().await;
    assert!(eval_active(&h.session, "globalThis.stableNameReads").await.as_u64().unwrap() > 0);
    eval_active(
        &h.session,
        "globalThis.stableNameReads = 0; document.querySelector('#changed-name').firstChild.data = 'Updated'",
    )
    .await;
    let after = h.observe().await;
    assert!(after.controls.iter().any(|c| c.label == "Updated"));
    assert!(after.controls.iter().any(|c| c.label == "Second"));
    assert_eq!(eval_active(&h.session, "globalThis.stableNameReads").await, json!(0));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_widget_keys_follow_virtual_focus_without_dom_changes() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(&h.session, r#"document.body.innerHTML = '<div id=grid role=grid tabindex=0 aria-label=Sheet aria-activedescendant=item><span id=item role=gridcell>Cell</span></div><button id=other>Other</button>';true"#).await;
    let before = h.observe().await;
    assert!(!find(&before, "Cell").operations.contains(&Operation::Press));
    for target in ["grid", "grid", "other", "grid"] {
        eval_active(&h.session, &format!("document.getElementById('{target}').focus()")).await;
        let after = h.observe().await;
        let cell = find(&after, "Cell");
        assert_eq!(cell.focused, Some(target == "grid"));
        assert_eq!(cell.operations.iter().filter(|op| **op == Operation::Press).count(), usize::from(target == "grid"));
        let other = fastbrowse_browser::page::CdpPage::new(h.session.clone(), Config::default());
        let mut full = other.observe().await.expect("full read");
        full.captured_at = after.captured_at;
        assert_eq!(after, full, "{target}");
        h.observe().await;
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cached_structure_matches_full_reads_when_external_dependencies_change() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(&h.session, r#"document.body.innerHTML = '<nav id=pager><a class=next href=/next>Continue</a></nav><span id=external-name>First name</span><input id=owned form=external-form aria-labelledby=external-name><form id=external-form><button>Send</button></form><select id=choices aria-label=Choice><option>One</option><option>Two</option></select><x-link id=relative-link role=link style=display:block>Relative destination</x-link>';document.querySelector('#relative-link').href='./target';globalThis.optionLabel='One';Object.defineProperty(document.querySelector('option'),'label',{get(){return optionLabel}});true"#).await;
    h.observe().await;
    for script in [
        "document.querySelector('#pager').removeAttribute('id');document.querySelector('nav').replaceWith(Object.assign(document.createElement('div'),{id:'pager',innerHTML:document.querySelector('nav').innerHTML}))",
        "document.querySelector('#pager').className = 'pagination'",
        "document.querySelector('#external-name').firstChild.data = 'Changed name'",
        "document.querySelector('#external-form').id = 'other-form'",
        "document.querySelector('#other-form').id = 'external-form'",
        "document.querySelector('#owned').required = true",
        "document.querySelector('#owned').focus()",
        "globalThis.optionLabel = 'Current option'",
        "document.querySelector('#choices').selectedIndex = 1",
        "history.pushState(null,'','/new-base/')",
        "document.body.style.minHeight = '4000px';scrollTo(0,1000)",
        "scrollTo(0,1500)",
    ] {
        eval_active(&h.session, script).await;
        let after = h.observe().await;
        let other = fastbrowse_browser::page::CdpPage::new(h.session.clone(), Config::default());
        let mut full = other.observe().await.expect("full read");
        full.captured_at = after.captured_at;
        assert_eq!(after, full, "{script}");
        h.observe().await;
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_context_lines_do_not_normalize_an_unchanged_heading_again() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(&h.session, r#"document.body.innerHTML = '<article><h2 id=changed-heading>First heading</h2><button>Open</button></article><article><h2>Second heading</h2><button>Open</button></article>';globalThis.stableNormalizations = 0;const original = String.prototype.replace;String.prototype.replace = function(...args) {if(String(this) === 'Second heading')stableNormalizations++;return original.apply(this,args)};true"#).await;
    h.observe().await;
    assert!(eval_active(&h.session, "stableNormalizations").await.as_u64().unwrap() > 0);
    for script in [
        "document.querySelector('#changed-heading').firstChild.data = 'Current heading'",
        "document.querySelector('#changed-heading').style.display = 'none'",
    ] {
        eval_active(&h.session, "stableNormalizations = 0").await;
        eval_active(&h.session, script).await;
        let after = h.observe().await;
        assert_eq!(eval_active(&h.session, "stableNormalizations").await, json!(0));
        let other = fastbrowse_browser::page::CdpPage::new(h.session.clone(), Config::default());
        let mut full = other.observe().await.expect("full read");
        full.captured_at = after.captured_at;
        assert_eq!(after, full, "{script}");
        h.observe().await;
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cached_names_match_full_reads_when_dom_css_and_live_inputs_change() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(&h.session, r#"document.head.insertAdjacentHTML('beforeend', '<style id=name-css>.name-hidden{display:none}.name-trigger:has(.flag) + button .conditional{display:none}</style>');document.body.innerHTML = '<span id=external>Original reference</span><button aria-labelledby=external></button><input id=native-button type=button value=Original><input id=late-field><div id=wrapper><input id=wrapped-field></div><label for=wrapper>Wrapped name</label><div class=name-trigger></div><button id=styled-name><span>Always</span><span class=conditional>Conditional</span></button><button id=css-name><span class=name-hidden>Hidden</span><span>Shown</span></button>'"#).await;
    h.observe().await;
    count_reads(&h).await;
    for script in [
        "document.querySelector('#external').firstChild.data = 'Changed reference'",
        "document.querySelector('#external').id = 'old-reference';document.body.insertAdjacentHTML('beforeend', '<span id=external>Replacement reference</span>')",
        "document.querySelector('#native-button').value = 'Live value'",
        "document.body.insertAdjacentHTML('beforeend', '<label for=late-field>Late name</label>')",
        "document.querySelector('.name-trigger').appendChild(document.createElement('span')).className = 'flag'",
        "document.querySelector('#name-css').sheet.cssRules[0].style.display = 'inline'",
        "document.querySelector('#wrapper').hidden = true",
        "document.querySelector('#wrapper').hidden = false;document.querySelector('#wrapper').insertAdjacentHTML('beforeend', '<input>')",
        "document.querySelector('#wrapper input:last-child').type = 'hidden'",
    ] {
        eval_active(&h.session, "globalThis.__fbReads = {total:0, unchanged:0, delta:0}").await;
        eval_active(&h.session, script).await;
        let observed = h.observe().await;
        let counts = eval_active(&h.session, "globalThis.__fbReads").await;
        assert_eq!(counts["total"], json!(1), "{script}: {counts}");
        assert_eq!(counts["delta"], json!(1), "{script}: {counts}");
        let other = fastbrowse_browser::page::CdpPage::new(h.session.clone(), Config::default());
        let mut full = other.observe().await.expect("full read");
        full.captured_at = observed.captured_at;
        assert_eq!(observed, full, "cached name differed from a full read after {script}");
        h.observe().await;
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_custom_element_upgrade_renews_native_label_ownership() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(&h.session, "document.body.innerHTML = '<label id=owner-label>Shared name<x-name id=custom-field></x-name><input id=native-field></label>'").await;
    let before = h.observe().await;
    assert!(before.controls.iter().any(|c| c.label == "Shared name"));
    count_reads(&h).await;
    eval_active(
        &h.session,
        "customElements.define('x-name', class extends HTMLElement {static formAssociated = true}); true",
    )
    .await;
    let after = h.observe().await;
    assert_eq!(eval_active(&h.session, "document.querySelector('label').control.localName").await, json!("x-name"));
    let counts = eval_active(&h.session, "globalThis.__fbReads").await;
    assert_eq!(counts["total"], json!(1), "{counts}");
    assert_eq!(counts["delta"], json!(1), "{counts}");
    let other = fastbrowse_browser::page::CdpPage::new(h.session.clone(), Config::default());
    let mut full = other.observe().await.expect("full read");
    full.captured_at = after.captured_at;
    assert_eq!(after, full);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_glance_cannot_consume_a_snapshots_custom_element_upgrade() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(&h.session, "document.body.innerHTML = '<article><label for=x>Alpha</label><x-field id=x></x-field><button>Open</button></article>'").await;
    let before = h.observe().await;
    h.page.glance().await.expect("first glance");
    eval_active(&h.session, "customElements.define('x-field', class extends HTMLElement {static formAssociated = true; constructor() {super(); this.attachInternals()}});true").await;
    h.page.glance().await.expect("glance after upgrade");
    let after = h.observe().await;
    assert_ne!(before.controls, after.controls);
    let other = fastbrowse_browser::page::CdpPage::new(h.session.clone(), Config::default());
    let mut full = other.observe().await.expect("full read");
    full.captured_at = after.captured_at;
    assert_eq!(after, full);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_detached_card_rebuilds_its_queries_in_a_new_shadow_root() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(&h.session, "document.body.innerHTML = '<article id=card><label for=w>Wrapped name</label><div id=w><input id=f></div></article>'").await;
    h.observe().await;
    eval_active(&h.session, "globalThis.savedCard = document.querySelector('#card'); savedCard.remove(); true").await;
    eval_active(&h.session, "savedCard.querySelector('#w').appendChild(document.createElement('input'));true").await;
    eval_active(&h.session, "document.body.appendChild(document.createElement('div')).attachShadow({mode:'open'}).appendChild(savedCard);true").await;
    let after = h.observe().await;
    let other = fastbrowse_browser::page::CdpPage::new(h.session.clone(), Config::default());
    let mut full = other.observe().await.expect("full read");
    full.captured_at = after.captured_at;
    assert_eq!(after, full);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_shadow_root_attached_without_a_mutation_renews_the_observation() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(&h.session, "document.body.innerHTML = '<article id=host><button>Light</button></article>'").await;
    h.observe().await;
    count_reads(&h).await;
    eval_active(
        &h.session,
        "document.querySelector('#host').attachShadow({mode:'open'}).innerHTML = '<button>Shadow buy</button>'",
    )
    .await;
    let after = h.observe().await;
    assert!(after.controls.iter().any(|c| c.label == "Shadow buy"));
    assert_eq!(reads(&h).await, (1, 0));
    let other = fastbrowse_browser::page::CdpPage::new(h.session.clone(), Config::default());
    let mut full = other.observe().await.expect("full read");
    full.captured_at = after.captured_at;
    assert_eq!(after, full);
    h.close().await;
}

/// Let a change's own events land first: a scroll dispatches its event on the next frame, which would otherwise mark
/// the read after the one under test as changed.
async fn settle_events(h: &Harness) {
    h.session
        .send(
            "Runtime.evaluate",
            json!({"expression": "new Promise(r => requestAnimationFrame(() => requestAnimationFrame(() => setTimeout(r))))", "awaitPromise": true}),
            Some(&h.session.active_session_id().unwrap()),
        )
        .await
        .expect("frames");
}

/// `observed`, read in full by a handle that keeps nothing, so each field can be compared.
async fn full_read(h: &Harness, observed: &Observation) -> Observation {
    let other = fastbrowse_browser::page::CdpPage::new(h.session.clone(), Config::default());
    let mut full = other.observe().await.expect("full read");
    full.captured_at = observed.captured_at;
    full
}

/// Make `script`'s change, then check the next read derives it in full and the one after is kept again.
async fn assert_the_change_is_read(h: &Harness, script: &str) {
    eval_active(&h.session, "globalThis.__fbReads = {total:0, unchanged:0, delta:0}").await;
    h.session
        .send(
            "Runtime.evaluate",
            json!({"expression": script, "awaitPromise": true}),
            Some(&h.session.active_session_id().unwrap()),
        )
        .await
        .expect("change");
    settle_events(h).await;
    let observed = h.observe().await;
    assert_eq!(reads(h).await, (1, 0), "{script} was served from the kept read");
    eval_active(&h.session, "globalThis.__fbReads = {total:0, unchanged:0, delta:0}").await;
    h.observe().await;
    assert_eq!(reads(h).await, (1, 1), "the read after {script} was not kept");
    assert_eq!(observed, full_read(h, &observed).await, "{script}");
    // Another handle's read replaced the one this handle kept, so the next change is judged against this one's.
    h.observe().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_read_inside_shadow_roots_is_kept_and_never_serves_a_changed_tree() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(
        &h.session,
        "document.body.innerHTML = '<article id=host><button>Light</button></article>'; \
         const root = document.querySelector('#host').attachShadow({mode:'open'}); \
         root.innerHTML = '<button id=buy>Buy</button><input id=qty><div id=inner></div><div id=pane style=\"height:40px;overflow:auto\"><p style=\"height:400px\">Long</p></div><span id=later></span>'; \
         root.querySelector('#inner').attachShadow({mode:'open'}).innerHTML = '<a href=#deep>Deep</a>'; true",
    )
    .await;
    h.observe().await;
    count_reads(&h).await;
    let unchanged = h.observe().await;
    assert_eq!(reads(&h).await, (1, 1), "an unchanged page with shadow roots was derived again");
    assert_eq!(unchanged, full_read(&h, &unchanged).await);
    h.observe().await;
    let shadow = "document.querySelector('#host').shadowRoot";
    for script in [
        format!("{shadow}.querySelector('#buy').textContent = 'Buy now'"),
        format!("{shadow}.querySelector('#buy').setAttribute('aria-label', 'Purchase')"),
        format!("{shadow}.querySelector('#inner').shadowRoot.querySelector('a').textContent = 'Deeper'"),
        // A value a script sets with no event, and a scroll inside the tree, which does not cross into the document.
        format!("{shadow}.querySelector('#qty').value = '3'"),
        format!("{shadow}.querySelector('#pane').scrollTop = 200"),
        // A tree attached inside a shadow root, which no observer records.
        format!(
            "{shadow}.querySelector('#later').attachShadow({{mode:'open'}}).innerHTML = '<button>Inner buy</button>'"
        ),
    ] {
        assert_the_change_is_read(&h, &script).await;
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_registry_an_earlier_version_set_up_is_set_up_again() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    // What an earlier version left on a page it read: change tracking with no watch on other roots.
    eval_active(
        &h.session,
        "document.body.innerHTML = '<div id=host></div>'; \
         document.querySelector('#host').attachShadow({mode:'open'}).innerHTML = '<button>Shadow</button>'; \
         window.__fastbrowse = { ids: new WeakMap(), nodes: new Map(), next: 1, revTracking: true, rev: 7, reads: 3 }; true",
    )
    .await;
    let first = h.observe().await;
    assert!(first.controls.iter().any(|c| c.label == "Shadow"), "{:?}", first.controls);
    count_reads(&h).await;
    h.observe().await;
    assert_eq!(reads(&h).await, (1, 1));
    eval_active(
        &h.session,
        "document.querySelector('#host').shadowRoot.querySelector('button').textContent = 'Changed'",
    )
    .await;
    let changed = h.observe().await;
    assert!(changed.controls.iter().any(|c| c.label == "Changed"), "{:?}", changed.controls);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_read_inside_a_child_document_is_kept_and_never_serves_a_changed_frame() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(
        &h.session,
        "new Promise(resolve => { document.body.innerHTML = '<button>Top</button><iframe id=child></iframe>'; \
         const frame = document.querySelector('#child'); frame.onload = () => resolve(true); \
         frame.srcdoc = '<button id=save>Save</button><input id=note><span id=later></span><p style=\"height:3000px\">Tall</p>'; })",
    )
    .await;
    let first = h.observe().await;
    assert!(first.controls.iter().any(|c| c.label == "Save"), "{:?}", first.controls);
    count_reads(&h).await;
    let unchanged = h.observe().await;
    assert_eq!(reads(&h).await, (1, 1), "an unchanged page with a child document was derived again");
    assert_eq!(unchanged, full_read(&h, &unchanged).await);
    h.observe().await;
    let child = "document.querySelector('#child').contentDocument";
    for script in [
        format!("{child}.querySelector('#save').textContent = 'Save draft'"),
        format!("{child}.querySelector('#note').value = 'typed by script'"),
        format!("{child}.defaultView.scrollTo(0, 500)"),
        format!("{child}.title = 'Retitled'"),
        format!(
            "{child}.querySelector('#later').attachShadow({{mode:'open'}}).innerHTML = '<button>Shadow save</button>'"
        ),
        format!("{child}.open(); {child}.write('<button>Rewritten</button>'); {child}.close(); true"),
        // The frame shows a new document with no mutation in its parent.
        "new Promise(resolve => { const frame = document.querySelector('#child'); frame.onload = () => resolve(true); \
         frame.contentWindow.location = 'about:blank'; })"
            .to_owned(),
    ] {
        assert_the_change_is_read(&h, &script).await;
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pointer_listener_changes_renew_a_saved_observation() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(&h.session, "document.body.innerHTML = '<div id=target>New target</div><button>Open</button>';globalThis.handler = () => {};true").await;
    h.observe().await;
    count_reads(&h).await;
    for (script, present) in [
        ("document.querySelector('#target').addEventListener('click', handler)", true),
        ("document.querySelector('#target').removeEventListener('click', handler)", false),
        (
            "globalThis.controller = new AbortController(); document.querySelector('#target').addEventListener('pointerdown', handler, {signal: controller.signal})",
            true,
        ),
        ("controller.abort()", false),
    ] {
        eval_active(&h.session, "globalThis.__fbReads = {total:0, unchanged:0, delta:0}").await;
        eval_active(&h.session, script).await;
        let after = h.observe().await;
        assert_eq!(after.controls.iter().any(|c| c.label == "New target"), present, "{script}");
        assert_eq!(reads(&h).await, (1, 0), "{script}");
        let other = fastbrowse_browser::page::CdpPage::new(h.session.clone(), Config::default());
        let mut full = other.observe().await.expect("full read");
        full.captured_at = after.captured_at;
        assert_eq!(after, full, "{script}");
        h.observe().await;
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_saved_read_checks_the_callers_native_listener_capability() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    let source = serde_json::to_string(include_str!("../js/snapshot.js")).expect("script");
    let setup = format!(
        "document.body.innerHTML = '<div id=target>New target</div><button>Open</button>';globalThis.probeRead = (0,eval)({source});globalThis.noAPI = {{listeners:null}};globalThis.withAPI = {{listeners:getEventListeners}};probeRead('snapshot-reuse',null,noAPI);true"
    );
    let active = h.session.active_session_id().expect("active session");
    let result = h
        .session
        .send(
            "Runtime.evaluate",
            json!({"expression":setup,"returnByValue":true,"includeCommandLineAPI":true}),
            Some(&active),
        )
        .await
        .expect("setup");
    assert!(result.get("exceptionDetails").is_none(), "{result}");
    eval_active(&h.session, "document.querySelector('#target').addEventListener('click', () => {})").await;
    let after = eval_active(&h.session, "probeRead('snapshot-reuse',null,withAPI)").await;
    assert!(after.get("unchanged").is_none(), "{after}");
    assert!(after["controls"].as_array().expect("controls").iter().any(|c| c["label"] == "New target"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_an_action_guard_reads_pointer_listeners_after_the_observation() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(&h.session, "document.body.innerHTML = '<div id=target>New target</div>';globalThis.handler = () => {};document.querySelector('#target').addEventListener('click', handler)").await;
    let observed = h.observe().await;
    let id = find(&observed, "New target").id.clone();
    eval_active(&h.session, "document.querySelector('#target').removeEventListener('click', handler)").await;
    let result = h.page.act(&targeted(Operation::Click, &id), &observed).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Stale, "{result:?}");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_render_changes_without_dom_mutations_match_full_observations() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(&h.session, r#"document.head.insertAdjacentHTML('beforeend', '<style>x-panel{display:block}#target:state(gone){display:none}#target:state(faint){opacity:0}#target:state(moved){transform:translateY(2000px)}#target:state(short-name) .label{display:none}#hover{width:120px;height:40px}#hover .tip{display:none}#hover:state(armed):hover .tip{display:block}</style>');globalThis.internals = new WeakMap();customElements.define('x-panel',class extends HTMLElement{constructor(){super();internals.set(this,this.attachInternals())}});document.body.innerHTML = '<x-panel id=target><button>Visible <span class=label>target</span></button></x-panel><x-panel id=hover><span class=tip>Details</span></x-panel><button>Other</button>';globalThis.domChanges = 0;new MutationObserver(records => domChanges += records.length).observe(document, {subtree:true,childList:true,attributes:true,characterData:true});true"#).await;
    h.observe().await;
    count_reads(&h).await;
    for script in [
        "internals.get(document.querySelector('#target')).states.add('gone')",
        "internals.get(document.querySelector('#target')).states.clear()",
        "internals.get(document.querySelector('#target')).states.add('faint')",
        "internals.get(document.querySelector('#target')).states.clear();internals.get(document.querySelector('#target')).states.add('moved')",
        "internals.get(document.querySelector('#target')).states.clear();internals.get(document.querySelector('#target')).states.add('short-name')",
        "internals.get(document.querySelector('#hover')).states.add('armed')",
    ] {
        eval_active(&h.session, "globalThis.__fbReads = {total:0, unchanged:0, delta:0}").await;
        eval_active(&h.session, script).await;
        assert_eq!(eval_active(&h.session, "domChanges").await, json!(0), "{script}");
        let after = h.observe().await;
        assert_eq!(reads(&h).await, (1, 0), "{script}");
        let other = fastbrowse_browser::page::CdpPage::new(h.session.clone(), Config::default());
        let mut full = other.observe().await.expect("full read");
        full.captured_at = after.captured_at;
        assert_eq!(after, full, "{script}");
        h.observe().await;
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_scroll_render_changes_without_dom_mutations_match_full_observations() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(&h.session, r#"document.head.insertAdjacentHTML('beforeend','<style>html{scrollbar-width:none}body::after{content:"";display:block;height:5000px}body:has(x-scroll:state(page-locked)){overflow:hidden}x-scroll{display:block;width:200px;height:100px;overflow:auto;scrollbar-width:none}x-scroll::after{content:"";display:block;width:400px;height:400px;flex-shrink:0}x-scroll:state(locked){overflow:hidden}x-scroll:state(short)::after{width:20px;height:20px}x-scroll:state(reverse){display:flex;flex-direction:column-reverse}x-scroll:state(rtl){direction:rtl}x-scroll:state(vertical){writing-mode:vertical-rl}x-scroll:state(moved){transform:translateY(2000px)}</style>');customElements.define('x-scroll',class extends HTMLElement{constructor(){super();this.internals=this.attachInternals()}});document.body.innerHTML='<x-scroll aria-label="Reviews"></x-scroll>';globalThis.scroller=document.querySelector('x-scroll');globalThis.domChanges=0;new MutationObserver(records=>domChanges+=records.length).observe(document,{subtree:true,attributes:true,childList:true,characterData:true});true"#).await;
    let before = h.observe().await;
    assert!(before.controls.iter().any(|control| control.label == "Vertical scroll area: Reviews"));
    count_reads(&h).await;
    for state in ["moved", "locked", "short", "reverse", "rtl", "vertical", "page-locked"] {
        eval_active(&h.session, "scroller.internals.states.clear()").await;
        let clean = h.observe().await;
        assert!(clean.controls.iter().any(|control| control.label == "Vertical scroll area: Reviews"));
        assert!(clean.controls.iter().any(|control| control.label == "Vertical scroll area: Page"));
        eval_active(&h.session, "globalThis.__fbReads={total:0,unchanged:0,delta:0}").await;
        let script = format!("scroller.internals.states.add('{state}')");
        eval_active(&h.session, &script).await;
        assert_eq!(eval_active(&h.session, "domChanges").await, json!(0), "{script}");
        let after = h.observe().await;
        assert_eq!(reads(&h).await, (1, 0), "{script}");
        if state == "page-locked" {
            assert!(!after.controls.iter().any(|control| control.label == "Vertical scroll area: Page"));
        }
        let other = fastbrowse_browser::page::CdpPage::new(h.session.clone(), Config::default());
        let mut full = other.observe().await.expect("full read");
        full.captured_at = after.captured_at;
        assert_eq!(after, full, "{script}");
        h.observe().await;
        eval_active(&h.session, "globalThis.__fbReads={total:0,unchanged:0,delta:0}").await;
        let mut unchanged = h.observe().await;
        assert_eq!(reads(&h).await, (1, 1), "{script}");
        unchanged.captured_at = after.captured_at;
        assert_eq!(after, unchanged, "{script}");
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_custom_control_properties_without_events_match_full_observations() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(&h.session, "document.body.innerHTML = '<x-field role=textbox id=field style=display:block>Editable</x-field><button>Other</button>';globalThis.field = document.querySelector('#field');field.value = 'First';field.readOnly = false;field.required = false;globalThis.domChanges = 0;new MutationObserver(records => domChanges += records.length).observe(document,{subtree:true,attributes:true,childList:true,characterData:true});true").await;
    h.observe().await;
    count_reads(&h).await;
    for script in ["field.value = 'Second'", "field.required = true", "field.readOnly = true"] {
        eval_active(&h.session, "globalThis.__fbReads = {total:0, unchanged:0, delta:0}").await;
        eval_active(&h.session, script).await;
        assert_eq!(eval_active(&h.session, "domChanges").await, json!(0), "{script}");
        let after = h.observe().await;
        assert_eq!(reads(&h).await, (1, 0), "{script}");
        let other = fastbrowse_browser::page::CdpPage::new(h.session.clone(), Config::default());
        let mut full = other.observe().await.expect("full read");
        full.captured_at = after.captured_at;
        assert_eq!(after, full, "{script}");
        h.observe().await;
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_changed_frame_sends_control_deltas() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(&h.session, "document.body.innerHTML = '<button>Before</button><button>Stable</button>'").await;
    h.observe().await;
    let function = h.session.snapshot().function.clone();
    eval_active(&h.session, "document.querySelector('button').textContent = 'After'").await;
    let delta = eval_active(&h.session, &format!("{function}('snapshot-delta', {{document_key: globalThis.__fastbrowse.kept.snapshot.document_key, read: globalThis.__fastbrowse.kept.snapshot.read}})")).await;
    let rows = delta["control_delta"]["controls"].as_array().expect("a control delta");
    assert_eq!(rows.iter().filter(|row| row.is_object()).count(), 1, "{delta}");
    assert_eq!(rows.iter().filter(|row| row.is_i64()).count(), 1, "{delta}");
    count_reads(&h).await;
    // This handle holds the read before the direct call advanced the producer's baseline.
    eval_active(&h.session, "document.querySelector('button').textContent = 'Newest'").await;
    let observed = h.observe().await;
    assert!(observed.controls.iter().any(|control| control.label == "Newest"));
    assert!(observed.controls.iter().any(|control| control.label == "Stable"));
    let counts = eval_active(&h.session, "globalThis.__fbReads").await;
    assert_eq!(counts["total"], json!(1), "{counts}");
    assert_eq!(counts["delta"], json!(0), "{counts}");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_delta_observations_match_full_reads_after_structural_and_live_changes() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(&h.session, "document.body.innerHTML = '<section><h2>Group</h2><button id=one>One</button><button id=two>Two</button><input id=field><dialog aria-label=Confirm><button>Yes</button></dialog></section>'").await;
    h.observe().await;
    count_reads(&h).await;
    for script in [
        "document.querySelector('#one').textContent = 'Changed'",
        "document.querySelector('#field').value = 'live property'",
        "document.querySelector('section').prepend(document.querySelector('#two'))",
        "document.querySelector('#one').remove()",
        "document.querySelector('dialog').showModal()",
        "document.querySelector('dialog').close()",
        "document.body.appendChild(document.createElement('div')).attachShadow({mode:'open'}).innerHTML = '<button>Shadow</button>'",
        "document.querySelector('div').shadowRoot.querySelector('button').textContent = 'Shadow changed'",
    ] {
        eval_active(&h.session, "globalThis.__fbReads = {total:0, unchanged:0, delta:0}").await;
        eval_active(&h.session, script).await;
        let observed = h.observe().await;
        let counts = eval_active(&h.session, "globalThis.__fbReads").await;
        assert_eq!(counts["total"], json!(1), "{script}: {counts}");
        assert_eq!(counts["delta"], json!(1), "{script}: {counts}");
        let other = fastbrowse_browser::page::CdpPage::new(h.session.clone(), Config::default());
        let mut full = other.observe().await.expect("full read");
        full.captured_at = observed.captured_at;
        assert_eq!(observed, full, "delta differed from a full read after {script}");
        h.observe().await;
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_change_the_dom_observer_sees_is_not_served_from_a_kept_read() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    // A change the page makes itself, of each kind a document observer reports.
    for (kind, script) in [
        ("text", "document.querySelector('button').textContent = 'After';"),
        ("child", "const b = document.createElement('button'); b.textContent = 'Added'; document.body.append(b);"),
        ("attribute", "document.querySelector('button').setAttribute('aria-expanded', 'true');"),
    ] {
        common::eval_active(&h.session, "document.body.innerHTML = '<button>Before</button>';").await;
        let before_obs = h.observe().await;
        count_reads(&h).await;
        common::eval_active(&h.session, script).await;
        let (_, unchanged_before) = reads(&h).await;
        let obs = h.observe().await;
        let (total, unchanged) = reads(&h).await;
        assert!(total >= 1, "{kind}: the page was not read");
        assert_eq!(unchanged, unchanged_before, "{kind}: a changed page was served from the read it kept");
        // The change is in the observation, which a read served from the kept one would have missed.
        assert_ne!(before_obs.page_key, obs.page_key, "{kind}: the observation changed");
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_scroll_or_a_pointer_move_is_not_served_from_a_kept_read() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    common::eval_active(
        &h.session,
        "document.body.innerHTML = '<div style=\"height:4000px\"><button>Top</button></div>';",
    )
    .await;
    let top = h.observe().await;
    count_reads(&h).await;
    // Scrolling moves what is on screen and what an observation calls offscreen, with no node rewritten.
    common::eval_active(&h.session, "window.scrollTo(0, 2500);").await;
    sleep(Duration::from_millis(100)).await;
    let scrolled = h.observe().await;
    let (_, unchanged) = reads(&h).await;
    assert_eq!(unchanged, 0, "a scrolled page was served from the read it kept");
    assert!(
        scrolled.controls.iter().filter(|c| c.role == "button").all(|c| c.offscreen),
        "scrolling past the only control should leave it offscreen: {:?}",
        scrolled.controls.iter().map(|c| (&c.label, c.offscreen)).collect::<Vec<_>>()
    );
    assert!(
        top.controls.iter().any(|c| c.role == "button" && !c.offscreen),
        "the button was on screen before the scroll"
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_frame_whose_kept_read_is_missing_is_read_again() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    common::eval_active(&h.session, "document.body.innerHTML = '<button>Kept</button>';").await;
    h.observe().await;
    // A page keeps its own read; a second page on the same session keeps none, so an "unchanged" answer names a read
    // this side does not have and the frame is read again rather than answered from nothing.
    let other = fastbrowse_browser::page::CdpPage::new(h.session.clone(), Config::default());
    let obs = other.observe().await.expect("observe");
    assert!(
        obs.controls.iter().any(|c| c.label == "Kept"),
        "{:?}",
        obs.controls.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_live_frames_saved_read_survives_tab_switches() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    let before = observe_until(&h.page, "Frame button").await;
    let original = h.session.active_target_id();
    let frame = h.session.frame_sessions()[0].0.clone();
    fastbrowse_browser::testing::open_owned_tab(&h.session, "about:blank").await.expect("open owned tab");
    let other = h.session.active_target_id();
    let checked = async {
        h.page.navigate(&format!("{}/inner.html", sites().main), None).await.expect("navigate other tab");
        h.observe().await;
        let inactive = seams::kept_keys(&h.page);
        h.session.switch_tab(&original).await.expect("switch back");
        let after = observe_until(&h.page, "Frame button").await;
        let restored = seams::kept_keys(&h.page);
        assert!(inactive.iter().any(|key| key.ends_with(&format!("|{frame}"))), "{inactive:?}");
        assert!(restored.iter().any(|key| key.ends_with(&format!("|{frame}"))));
        assert_eq!(
            before.controls.iter().map(|control| &control.label).collect::<Vec<_>>(),
            after.controls.iter().map(|control| &control.label).collect::<Vec<_>>()
        );
        h.session.switch_tab(&other).await.expect("switch away");
        h.session.send("Target.closeTarget", json!({"targetId": original}), None).await.expect("close original tab");
        wait_until(
            || async {
                h.observe().await;
                !seams::kept_keys(&h.page).iter().any(|key| key.ends_with(&format!("|{frame}")))
            },
            5.0,
        )
        .await;
    };
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(checked)).await;
    h.close().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_detached_frame_reads_are_released_and_reinserted_frames_are_read() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    observe_until(&h.page, "Frame button").await;
    let old = h.session.frame_sessions()[0].0.clone();
    assert!(seams::kept_keys(&h.page).iter().any(|key| key.ends_with(&format!("|{old}"))));
    eval_active(&h.session, "globalThis.savedFrame = document.querySelector('#cross-frame'); savedFrame.remove()")
        .await;
    wait_until(|| async { h.session.frame_sessions().is_empty() }, 5.0).await;
    h.observe().await;
    let retained = seams::kept_keys(&h.page);
    eval_active(&h.session, "document.body.append(savedFrame)").await;
    let after = observe_until(&h.page, "Frame button").await;
    let new = h.session.frame_sessions()[0].0.clone();
    let restored = seams::kept_keys(&h.page);
    h.close().await;
    assert!(retained.iter().all(|key| key.ends_with("|main")), "{retained:?}");
    assert_ne!(old, new);
    assert!(restored.iter().any(|key| key.ends_with(&format!("|{new}"))));
    assert!(!restored.iter().any(|key| key.ends_with(&format!("|{old}"))));
    assert!(after.controls.iter().any(|control| control.label == "Frame button"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_selected_value_is_not_served_from_a_kept_read() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(
        &h.session,
        r#"document.body.innerHTML = '<select aria-label="Topic"><option>General</option><option>Damaged item</option></select>';"#,
    ).await;
    let before = h.observe().await;
    let action = Action {
        target_id: Some(find(&before, "Topic").id.clone()),
        text: Some("Damaged item".into()),
        ..Action::new(Operation::Select)
    };
    let result = h.page.act(&action, &before).await.expect("select");
    let after = h.observe().await;
    h.close().await;
    assert_eq!(result.outcome, StepOutcome::Executed);
    assert_eq!(find(&after, "Topic").value.as_deref(), Some("Damaged item"));
    assert_ne!(before.page_key, after.page_key);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_form_state_a_script_sets_without_an_event_is_not_served_from_a_kept_read() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    // A property set from code fires no event and writes no attribute, so neither the listeners nor the document
    // observer see it; the read must still not be served from before it.
    for (kind, script) in [
        ("value", "document.querySelector('input[type=text]').value = 'Filled';"),
        ("checked", "document.querySelector('input[type=checkbox]').checked = true;"),
        ("selection", "document.querySelector('select').selectedIndex = 1;"),
    ] {
        eval_active(
            &h.session,
            r#"document.body.innerHTML = '<input type="text" aria-label="Name"><input type="checkbox" aria-label="Agree"><select aria-label="Topic"><option>General</option><option>Damaged item</option></select>';"#,
        )
        .await;
        let before = h.observe().await;
        let live_before = eval_active(&h.session, "globalThis.__fastbrowse.live()").await;
        eval_active(&h.session, script).await;
        assert_ne!(
            live_before,
            eval_active(&h.session, "globalThis.__fastbrowse.live()").await,
            "{kind}: the retained live check held stale property values"
        );
        let after = h.observe().await;
        assert_ne!(before.page_key, after.page_key, "{kind}: the observation was served from before the change");
        match kind {
            "value" => assert_eq!(find(&after, "Name").value.as_deref(), Some("Filled")),
            "checked" => assert_eq!(find(&after, "Agree").checked, Some(true)),
            _ => assert_eq!(find(&after, "Topic").value.as_deref(), Some("Damaged item")),
        }
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_validation_message_a_script_sets_is_not_served_from_a_kept_read() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(
        &h.session,
        r#"document.body.innerHTML = '<input type="text" aria-label="Handle"><button>Save</button>';"#,
    )
    .await;
    let before = h.observe().await;
    let typed = Action {
        target_id: Some(find(&before, "Handle").id.clone()),
        text: Some("taken".into()),
        ..Action::new(Operation::Fill)
    };
    h.page.act(&typed, &before).await.expect("fill");
    // Leaving the field is what lets the page show a refusal on it (`:user-invalid`).
    let filled = h.observe().await;
    let left = Action { target_id: Some(find(&filled, "Handle").id.clone()), ..Action::new(Operation::Blur) };
    h.page.act(&left, &filled).await.expect("blur");
    let typed_obs = h.observe().await;
    assert_eq!(find(&typed_obs, "Handle").validation_message, None);
    // A server check answering late, as a signup form's "already taken" does: no event, no attribute.
    eval_active(&h.session, "document.querySelector('input').setCustomValidity('That handle is taken');").await;
    let after = h.observe().await;
    h.close().await;
    assert_eq!(find(&after, "Handle").validation_message.as_deref(), Some("That handle is taken"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_retargeted_hover_rule_replaces_its_offered_target() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(&h.session, r#"document.head.insertAdjacentHTML('beforeend', '<style id=hover-rule>.one .child,.two .child{display:none}.one:hover .child{display:block}</style>');document.body.innerHTML = '<div class=one>First<button class=child>Hidden one</button></div><div class=two>Second<button class=child>Hidden two</button></div>'"#).await;
    let before = h.observe().await;
    let hovers = |obs: &fastbrowse_core::page::Observation| {
        obs.controls
            .iter()
            .filter(|c| c.operations.contains(&Operation::Hover))
            .map(|c| c.label.clone())
            .collect::<Vec<_>>()
    };
    assert!(hovers(&before).iter().any(|label| label.contains("First")), "{:?}", hovers(&before));
    eval_active(
        &h.session,
        "document.querySelector('#hover-rule').sheet.cssRules[1].selectorText = '.two:hover .child'",
    )
    .await;
    let after = h.observe().await;
    assert!(hovers(&after).iter().any(|label| label.contains("Second")), "{:?}", hovers(&after));
    assert!(!hovers(&after).iter().any(|label| label.contains("First")), "{:?}", hovers(&after));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_css_rule_edited_in_place_is_not_served_from_a_kept_read() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(
        &h.session,
        "document.head.insertAdjacentHTML('beforeend', '<style>.later { color: black; }</style>'); \
         document.body.innerHTML = '<button>Stays</button><button class=\"later\">Goes</button>';",
    )
    .await;
    let before = h.observe().await;
    assert!(before.controls.iter().any(|c| c.label == "Goes"));
    // The rule count is unchanged and nothing in the document is rewritten.
    eval_active(&h.session, "[...document.styleSheets].at(-1).cssRules[0].style.display = 'none';").await;
    let after = h.observe().await;
    h.close().await;
    assert!(
        !after.controls.iter().any(|c| c.label == "Goes"),
        "a control the edited rule hides is still observed: {:?}",
        after.controls.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_page_handle_is_not_served_another_handles_newer_read() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(&h.session, "document.body.innerHTML = '<button>Before</button>';").await;
    h.observe().await;
    eval_active(&h.session, "document.querySelector('button').textContent = 'After';").await;
    // A second handle on the same session reads the changed page, so the page's kept read is now that one; the first
    // handle still holds its own older read of the same document.
    let other = fastbrowse_browser::page::CdpPage::new(h.session.clone(), Config::default());
    assert!(other.observe().await.expect("observe").controls.iter().any(|c| c.label == "After"));
    let again = h.observe().await;
    h.close().await;
    assert!(
        again.controls.iter().any(|c| c.label == "After"),
        "the first handle was served its own older read: {:?}",
        again.controls.iter().map(|c| &c.label).collect::<Vec<_>>()
    );
}

/// Reading a delay can run page code, so the native call must receive the value already converted for tracking.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_timer_delay_is_converted_once_before_scheduling() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(
        &h.session,
        r#"
      document.body.innerHTML = '<button>Delay</button>';
      window.conversions = window.cancelConversions = 0;
      window.cancelledFired = false;
      document.querySelector('button').onclick = () => {
        const id = setTimeout(() => window.cancelledFired = true, 75);
        clearInterval({valueOf() { return ++window.cancelConversions === 1 ? id : 0; }});
        setTimeout(
          () => document.querySelector('button').textContent = 'Finished',
          {valueOf() { return ++window.conversions === 1 ? 4294967446 : 10000; }}
        );
      };
    "#,
    )
    .await;
    let before = h.observe().await;
    let result = h.page.act(&targeted(Operation::Click, &find(&before, "Delay").id), &before).await.expect("click");
    assert!(result.page_changed, "{result:?}");
    assert_eq!(
        eval_active(&h.session, "[window.conversions, window.cancelConversions, window.cancelledFired]").await,
        json!([1, 1, false])
    );
    assert!(h.observe().await.viewport_text.contains("Finished"));
    h.close().await;
}
