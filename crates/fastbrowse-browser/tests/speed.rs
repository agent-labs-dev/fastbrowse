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
use fastbrowse_core::page::Action;
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
async fn test_a_settle_of_an_idle_page_pays_no_quiet_window() {
    let _turn = turn().await;
    // A page that was already quiet before the click has nothing to wait for, so the settle answers at once rather
    // than charging every action a fixed quiet window.
    let Some((h, proxy)) = proxied().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(&h.session, "document.body.innerHTML = '<button>Idle</button>'").await;
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
         const installed = scripts[key]; globalThis.__fbReads = { total: 0, unchanged: 0 }; \
         scripts[key] = mode => { const read = installed(mode); globalThis.__fbReads.total++; \
         if (read && read.unchanged) globalThis.__fbReads.unchanged++; return read; }; })()",
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
    // A read that went into a shadow root or a child document is not kept, since a document observer sees neither, so
    // this page has only what the observer covers.
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
        eval_active(&h.session, script).await;
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
