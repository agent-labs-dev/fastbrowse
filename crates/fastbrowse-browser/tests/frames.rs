//! The live view, frame callback, command deadline, active target, attached recording and cursor suites of
//! tests/browser, one section per Python file.

mod common;
mod lifecycle_support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fastbrowse_browser::session::{BrowserSession, SessionOptions};
use fastbrowse_browser::shared::{BoxError, BrowserError, FrameHandler};
use fastbrowse_browser::testing;
use lifecycle_support::{FakeChrome, Reply};
use serde_json::json;
use tokio::sync::watch;

#[cfg(any(feature = "cursor", feature = "recording"))]
fn temp_dir(name: &str) -> std::path::PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("fastbrowse-frames-{name}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// -- test_frames.py: live view delivery must not hold up the browser's control connection --------------------------

fn emit_frame(session: &BrowserSession, number: i64, data: &[u8], tab_session: &str) {
    let params = json!({"sessionId": number, "data": data_encoding::BASE64.encode(data), "metadata": {}});
    testing::dispatch(session, "Page.screencastFrame", params, Some(tab_session));
}

/// `await asyncio.gather(*session._background)`, which in Rust holds the frame delivery apart from the rest.
async fn settle(session: &BrowserSession) {
    testing::settle_delivery(session).await;
    testing::settle_background(session).await;
}

/// The test's `receive`: keeps each frame, says it is delivering, and holds the delivery until released.
struct Receiver {
    frames: Mutex<Vec<Vec<u8>>>,
    delivering: watch::Sender<bool>,
    release: watch::Sender<bool>,
}

impl Receiver {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            frames: Mutex::default(),
            delivering: watch::channel(false).0,
            release: watch::channel(true).0,
        })
    }

    fn handler(self: &Arc<Self>) -> FrameHandler {
        let receiver = self.clone();
        Arc::new(move |frame| {
            let receiver = receiver.clone();
            Box::pin(async move {
                receiver.frames.lock().unwrap().push(frame);
                receiver.delivering.send_replace(true);
                let _ = receiver.release.subscribe().wait_for(|released| *released).await;
                Ok(())
            })
        })
    }

    fn frames(&self) -> Vec<Vec<u8>> {
        self.frames.lock().unwrap().clone()
    }

    async fn delivering(&self) {
        let _ = self.delivering.subscribe().wait_for(|delivering| *delivering).await;
    }
}

fn bytes(frames: &[&[u8]]) -> Vec<Vec<u8>> {
    frames.iter().map(|f| f.to_vec()).collect()
}

/// A current-thread runtime, as Python's event loop is: a frame that supersedes another must arrive before the
/// delivery task can run, which a second worker thread would let it do in between.
#[tokio::test]
async fn test_live_frames_follow_tabs_ack_after_delivery_and_keep_only_the_latest() {
    let fake = FakeChrome::start().await;
    let receiver = Receiver::new();
    let options = SessionOptions { on_frame: Some(receiver.handler()), ..Default::default() };
    let session = fake.session_with(fake.connection(), options).await;
    settle(&session).await;
    let live = json!({"format": "jpeg", "quality": 60, "maxWidth": 1280, "maxHeight": 800});
    assert_eq!(fake.script().requested("Page.startScreencast", &live, Some("session")), 1);

    emit_frame(&session, 1, b"first", "session");
    settle(&session).await;
    emit_frame(&session, 2, b"superseded", "session");
    emit_frame(&session, 3, b"latest", "session");
    settle(&session).await;
    assert_eq!(receiver.frames(), bytes(&[b"first", b"latest"]));

    receiver.release.send_replace(false);
    receiver.delivering.send_replace(false);
    emit_frame(&session, 4, b"slow", "session");
    receiver.delivering().await;
    emit_frame(&session, 5, b"arrived while busy", "session");
    testing::dispatch(
        &session,
        "Page.javascriptDialogOpening",
        json!({"type": "alert", "message": "ready"}),
        Some("session"),
    );
    assert!(session.pending_dialog().is_some());
    session.handle_dialog(true, None).await.unwrap();
    tokio::task::yield_now().await;
    // Back-pressure: Chrome is not told to send more until the frame in hand has been delivered.
    assert_eq!(fake.script().requested("Page.screencastFrameAck", &json!({"sessionId": 4}), Some("session")), 0);
    receiver.release.send_replace(true);
    settle(&session).await;
    assert_eq!(receiver.frames(), bytes(&[b"first", b"latest", b"slow", b"arrived while busy"]));

    fake.queue("Target.createTarget", [json!({"targetId": "second"})]);
    fake.queue("Target.attachToTarget", [json!({"sessionId": "second-session"})]);
    testing::open_owned_tab(&session, "about:blank").await.unwrap();
    settle(&session).await;
    session.switch_tab("owned").await.unwrap();
    settle(&session).await;
    session.switch_tab("second").await.unwrap();
    settle(&session).await;
    emit_frame(&session, 6, b"old tab", "session");
    emit_frame(&session, 7, b"new tab", "second-session");
    settle(&session).await;
    assert_eq!(receiver.frames().last().unwrap(), b"new tab");
    assert!(!receiver.frames().contains(&b"old tab".to_vec()));

    testing::dispatch(&session, "Target.targetDestroyed", json!({"targetId": "second"}), None);
    settle(&session).await;
    assert_eq!(session.active_target_id(), "owned");

    // Recording owns a tap on every frame while it runs, without replacing the live consumer. Python installs a
    // `Recording`'s handler without starting it; this tap is the Rust recording's, which starts ffmpeg when started.
    let recorded: Arc<Mutex<Option<Vec<u8>>>> = Arc::default();
    session.set_frame_tap(Some(Arc::new({
        let (session, recorded) = (session.clone(), recorded.clone());
        move |event| {
            if event.session.as_deref() != session.active_session_id().ok().as_deref() {
                return;
            }
            let data = event.params["data"].as_str().unwrap_or_default();
            *recorded.lock().unwrap() = data_encoding::BASE64.decode(data.as_bytes()).ok();
        }
    })));
    emit_frame(&session, 8, b"recorded too", "session");
    settle(&session).await;
    assert_eq!(receiver.frames().last().unwrap(), b"recorded too");
    assert_eq!(recorded.lock().unwrap().as_deref(), Some(&b"recorded too"[..]));
    // A frame from another session, as the cast that keeps a background tab painting sends, is acked unseen.
    emit_frame(&session, 80, b"no picture of the tab", "keeper");
    settle(&session).await;
    assert_eq!(receiver.frames().last().unwrap(), b"recorded too");
    assert_eq!(recorded.lock().unwrap().as_deref(), Some(&b"recorded too"[..]));
    {
        let mut script = fake.script();
        let keeper = ("Page.screencastFrameAck".to_owned(), json!({"sessionId": 80}), Some("keeper".to_owned()));
        let at = script.requests.iter().position(|r| *r == keeper).expect("the keeper's frame was acked");
        script.requests.remove(at);
    }

    receiver.release.send_replace(false);
    receiver.delivering.send_replace(false);
    emit_frame(&session, 9, b"cancel on close", "session");
    receiver.delivering().await;
    session.close().await.unwrap();
    fake.stopped().await;

    assert_eq!(testing::background_len(&session), 0);
    // Asserted last, so the rest of the test still checks what it can while the library bug below stands.
    let scheduled = testing::frame_scheduled(&session);
    {
        let script = fake.script();
        let acks: Vec<_> = script.requests.iter().filter(|r| r.0 == "Page.screencastFrameAck").cloned().collect();
        let expected: Vec<_> = (1..9)
            // 9 was cancelled mid-delivery by close, which stops the cast; an ack would be for a cast already gone.
            .map(|n| {
                let tab = if n == 7 { "second-session" } else { "session" };
                ("Page.screencastFrameAck".to_owned(), json!({"sessionId": n}), Some(tab.to_owned()))
            })
            .collect();
        assert_eq!(acks, expected);
        let casts: Vec<(String, String)> = script
            .requests
            .iter()
            .filter(|r| r.0.ends_with("Screencast"))
            .map(|r| (r.0.clone(), r.2.clone().unwrap_or_default()))
            .collect();
        let expected: Vec<(String, String)> = [
            ("Page.startScreencast", "session"),
            ("Page.stopScreencast", "session"),
            ("Page.startScreencast", "second-session"),
            ("Page.stopScreencast", "second-session"),
            ("Page.startScreencast", "session"),
            ("Page.stopScreencast", "session"),
            ("Page.startScreencast", "second-session"),
            ("Page.stopScreencast", "second-session"),
            ("Page.startScreencast", "session"),
            ("Page.stopScreencast", "session"),
        ]
        .iter()
        .map(|(m, s)| ((*m).to_owned(), (*s).to_owned()))
        .collect();
        assert_eq!(casts, expected);
        assert!(script.index("Page.stopScreencast") < script.index("stop"));
    }

    fake.script().requests.clear();
    let session = fake.session(fake.connection()).await;
    testing::dispatch(&session, "Page.screencastFrame", json!({}), None);
    settle(&session).await;
    session.close().await.unwrap();
    assert!(!fake.script().requests.iter().any(|(m, _, _)| m.contains("Screencast") || m.contains("screencast")));
    assert!(!scheduled, "the first session left a frame delivery scheduled after close");
}

/// Python also checks every failure is logged as a warning that quotes nothing private; the Rust warnings go to
/// stderr only when `FASTBROWSE_LOG` is set, and a test cannot read its own stderr.
#[tokio::test]
async fn test_live_view_failures_are_warnings_and_leave_the_session_usable() {
    for failure in ["Page.startScreencast", "Page.stopScreencast", "Page.screencastFrameAck", "handler"] {
        let fake = FakeChrome::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let on_frame: FrameHandler = Arc::new({
            let calls = calls.clone();
            move |_frame| {
                calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    if failure == "handler" {
                        return Err::<(), BoxError>("private page text".into());
                    }
                    Ok(())
                })
            }
        });
        if failure != "handler" {
            fake.fail(failure, Reply::Error(-32603));
        }
        let options = SessionOptions { on_frame: Some(on_frame), ..Default::default() };
        let session = fake.session_with(fake.connection(), options).await;
        settle(&session).await;
        emit_frame(&session, 1, b"jpeg", "session");
        settle(&session).await;
        emit_frame(&session, 2, b"next jpeg", "session");
        settle(&session).await;
        session.switch_tab("owned").await.unwrap();
        session.close().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "{failure}");
        let calls = fake.calls();
        assert!(calls.iter().any(|m| m == "Page.screencastFrameAck"), "{failure}");
        assert!(calls.iter().any(|m| m == "Target.activateTarget"), "{failure}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_stalled_screencast_does_not_delay_switching_tabs() {
    let fake = FakeChrome::start().await;
    fake.block("Page.startScreencast");
    let on_frame: FrameHandler = Arc::new(|_frame| Box::pin(async { Ok(()) }));
    let options = SessionOptions { on_frame: Some(on_frame), ..Default::default() };
    let session = fake.session_with(fake.connection(), options).await;
    fake.sent("Page.startScreencast").await;
    let client = session.client().unwrap();
    session.switch_tab("owned").await.unwrap();
    // `transport.finished` does not hold the start yet: it is still waiting on Chrome.
    assert!(client.pending() >= 1);
    session.close().await.unwrap();
    assert_eq!(client.pending(), 0);
}

/// Python also checks the warning that nothing was recorded; see the note on the live view failures test.
#[cfg(feature = "recording")]
#[tokio::test(flavor = "multi_thread")]
async fn test_a_recording_whose_cast_sent_no_frame_ends_on_a_screenshot_unless_frames_are_withheld() {
    // Chrome can acknowledge `Page.startScreencast` and then send nothing, and a short run ends before the restart.
    use fastbrowse_browser::recording::Recording;
    use fastbrowse_browser::session::CheckAccess;

    if fastbrowse_browser::adapters::local_chrome::which("ffmpeg").is_none() {
        eprintln!("recording needs ffmpeg; skipping");
        return;
    }
    for page in ["clear", "showing a secret", "revealing a secret during the capture", "after access is revoked"] {
        let dir = temp_dir("recording");
        let still = dir.join("still.jpg");
        let made = tokio::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-f", "lavfi", "-i", "color=c=teal:s=64x48", "-frames:v", "1"])
            .arg(&still)
            .status()
            .await
            .unwrap();
        assert!(made.success());
        let fake = FakeChrome::start().await;
        let shot = data_encoding::BASE64.encode(&std::fs::read(&still).unwrap());
        fake.queue("Page.captureScreenshot", [json!({"data": shot})]);
        let path = dir.join("clip.mp4");
        let revoked = Arc::new(AtomicBool::new(false));
        let check_access: CheckAccess = Arc::new({
            let revoked = revoked.clone();
            move || {
                let revoked = revoked.load(Ordering::SeqCst);
                Box::pin(async move { if revoked { Err::<(), BoxError>("revoked".into()) } else { Ok(()) } })
            }
        });
        let options = SessionOptions { check_access: Some(check_access), ..Default::default() };
        let session = fake.session_with(fake.connection(), options).await;
        session.set_frames_withheld(page == "showing a secret");
        if page == "revealing a secret during the capture" {
            let revealing = session.clone();
            fake.on_call("Page.captureScreenshot", Arc::new(move |_| revealing.set_frames_withheld(true)));
        }
        let recording = Recording::start(session.clone(), &path).await.unwrap();
        tokio::time::sleep(Duration::from_secs_f64(0.2)).await;
        revoked.store(page == "after access is revoked", Ordering::SeqCst);
        let outputs = recording.finish().await;
        session.close().await.unwrap();
        let script = fake.script();
        let cast = json!({"format": "jpeg", "quality": 95});
        assert!(script.requested("Page.startScreencast", &cast, Some("session")) >= 1, "{page}");
        if page == "clear" {
            assert_eq!(outputs, [path.clone(), dir.join("clip.plain.mp4")]);
            assert!(outputs.iter().all(|output| std::fs::metadata(output).unwrap().len() > 0));
            let _ = std::fs::remove_dir_all(&dir);
            continue;
        }
        // A recording keeps its last clean frame, and with none it records nothing rather than the secret.
        assert_eq!(
            script.calls.iter().any(|m| m == "Page.captureScreenshot"),
            page == "revealing a secret during the capture",
            "{page}"
        );
        assert!(outputs.is_empty(), "{page}");
        assert!(!path.exists(), "{page}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// -- test_frame_callback.py ------------------------------------------------------------------------------------------
//
// Python runs these on a real Chrome and sets the frame handler on the open session. The Rust session takes its
// handler when it is made, so on a real Chrome the cast would hand its own frames to the handler before the test's;
// the fake sends none.

#[tokio::test(flavor = "multi_thread")]
async fn test_close_cancels_a_blocked_frame_callback() {
    let fake = FakeChrome::start().await;
    let started = watch::channel(false).0;
    let on_frame: FrameHandler = Arc::new({
        let started = started.clone();
        move |_frame| {
            started.send_replace(true);
            Box::pin(std::future::pending())
        }
    });
    let options = SessionOptions { on_frame: Some(on_frame), ..Default::default() };
    let session = fake.session_with(fake.connection(), options).await;
    let active = session.active_session_id().unwrap();
    testing::deliver_frame(&session, "aGVsbG8=", 1, Some(&active));
    let mut waiting = started.subscribe();
    tokio::time::timeout(Duration::from_secs(1), waiting.wait_for(|s| *s)).await.unwrap().unwrap();

    tokio::time::timeout(Duration::from_secs(2), session.close()).await.unwrap().unwrap();

    assert!(testing::lifecycle::closing(&session));
    assert_eq!(testing::background_len(&session), 0);
    assert!(!testing::has_pending_frame(&session));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_close_does_not_wait_on_a_frame_callback_that_ignores_cancellation() {
    // Python's handler swallows its cancellation; a Rust future cannot, so this one blocks its thread instead, which
    // no cancellation reaches either. Python shortens `_FRAME_RELEASE_SECONDS` to 0.1s; the Rust one is 2s.
    let fake = FakeChrome::start().await;
    let started = watch::channel(false).0;
    let released = Arc::new(AtomicBool::new(false));
    let on_frame: FrameHandler = Arc::new({
        let (started, released) = (started.clone(), released.clone());
        move |_frame| {
            let (started, released) = (started.clone(), released.clone());
            Box::pin(async move {
                started.send_replace(true);
                // A caller's handler can ignore its cancellation and keep waiting, as a retry loop that catches
                // everything does; the run must still return and stop its browser.
                while !released.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(())
            })
        }
    });
    let options = SessionOptions { on_frame: Some(on_frame), ..Default::default() };
    let session = fake.session_with(fake.connection(), options).await;
    let frame = json!({
        "data": "aGVsbG8=",
        "sessionId": 1,
        "metadata": {
            "offsetTop": 0, "pageScaleFactor": 1, "deviceWidth": 1, "deviceHeight": 1, "scrollOffsetX": 0,
            "scrollOffsetY": 0,
        },
    });
    let active = session.active_session_id().unwrap();
    testing::dispatch(&session, "Page.screencastFrame", frame, Some(&active));
    let mut waiting = started.subscribe();
    tokio::time::timeout(Duration::from_secs(1), waiting.wait_for(|s| *s)).await.unwrap().unwrap();

    let closing = tokio::spawn({
        let session = session.clone();
        async move { session.close().await }
    });
    let finished = tokio::time::timeout(Duration::from_secs(4), async {
        while !closing.is_finished() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    released.store(true, Ordering::SeqCst);
    closing.await.unwrap().unwrap();
    testing::settle_background(&session).await;
    assert!(finished.is_ok(), "close waited on the frame callback");
    assert!(session.client().is_err());
}

// -- test_command_deadline.py: a live browser can leave one page command unanswered indefinitely --------------------
//
// These run on the real `CDP_REPLY_SECONDS` (60s), `CDP_ALIVE_SECONDS` (10s) and `CDP_COMMAND_SECONDS` (120s), which
// are constants a test cannot shorten as Python's monkeypatch does, so each takes minutes and is ignored by default.
// Each Python scenario is kept, its times scaled to these.

#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: waits out the real CDP_COMMAND_SECONDS, about 120s"]
async fn test_unanswered_command_from_live_browser_has_a_deadline() {
    let fake = FakeChrome::start().await;
    let session = fake.session(fake.connection()).await;
    fake.block("Runtime.evaluate");
    let client = session.client().unwrap();
    let raised = tokio::time::timeout(
        Duration::from_secs(150),
        client.send("Runtime.evaluate", json!({"expression": "aPromiseThatNeverSettles()"}), None),
    )
    .await
    .expect("the deadline ends the wait")
    .unwrap_err();
    assert!(matches!(raised, BrowserError::PageUnresponsive(_)), "{raised:?}");
    assert!(raised.to_string().contains("command deadline"), "{raised}");
    assert_eq!(client.pending(), 0);
    session.close().await.unwrap();
    // The browser answered throughout, so the page stopping is the run's failure, never an outage.
    assert!(!raised.is_unavailable());
    assert!(fake.calls().iter().any(|m| m == "Browser.getVersion"));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: waits out the real CDP_COMMAND_SECONDS, about 120s"]
async fn test_repeated_command_deadlines_stop_the_tab_instead_of_recurring() {
    let fake = FakeChrome::start().await;
    let session = fake.session(fake.connection()).await;
    fake.block("Runtime.evaluate");
    let client = session.client().unwrap();
    // Python waits out the deadlines one after another; they are counted the same side by side, in half the time.
    let deadlines = (0..fastbrowse_browser::cdp::CDP_DEADLINES_BEFORE_STOP)
        .map(|_| client.send("Runtime.evaluate", json!({"expression": "1"}), Some("session")));
    for raised in futures_util::future::join_all(deadlines).await {
        let raised = raised.unwrap_err();
        assert!(matches!(raised, BrowserError::PageUnresponsive(_)) && raised.to_string().contains("command deadline"));
    }
    let sent = fake.calls().len();
    let raised = tokio::time::timeout(
        Duration::from_secs_f64(0.02),
        client.send("Runtime.evaluate", json!({"expression": "1"}), Some("session")),
    )
    .await
    .expect("refused at once")
    .unwrap_err();
    assert!(matches!(raised, BrowserError::PageUnresponsive(_)) && raised.to_string().contains("already exceeded"));
    assert_eq!(fake.calls().len(), sent, "a tab command after repeated deadlines is not sent to wait again");
    // Closing the run's tabs is still possible.
    client.send("Target.getTargets", json!({}), None).await.unwrap();
    session.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: waits out the real CDP_COMMAND_SECONDS and CDP_ALIVE_SECONDS, about 130s"]
async fn test_a_browser_gone_by_the_command_deadline_is_an_outage() {
    let fake = FakeChrome::start().await;
    let session = fake.session(fake.connection()).await;
    fake.block("Runtime.evaluate");
    let client = session.client().unwrap();
    let sending = client.send("Runtime.evaluate", json!({"expression": "1"}), Some("session"));
    let gone = async {
        // The probe after the first wait answers; the browser goes before the deadline ends the second.
        tokio::time::sleep(Duration::from_secs(65)).await;
        fake.block("Browser.getVersion");
    };
    let (raised, ()) = tokio::join!(sending, gone);
    let raised = raised.unwrap_err();
    assert!(matches!(raised, BrowserError::BrowserUnresponsive(_)), "{raised:?}");
    assert!(raised.to_string().contains("stopped answering"), "{raised}");
    session.close().await.unwrap();
    assert!(raised.is_unavailable());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: waits out the 120s command deadline"]
async fn test_a_reply_that_lands_during_the_deadline_probe_is_kept() {
    let fake = FakeChrome::start().await;
    let session = fake.session(fake.connection()).await;
    // The probe at the deadline waits on a browser slow to answer, and the page's reply lands meanwhile. Python's
    // probe at the first wait is cut short by the deadline; the Rust probe is shorter than the wait, so it answers.
    fake.queue("Browser.getVersion", [json!({})]);
    fake.block("Browser.getVersion");
    fake.delay("Runtime.evaluate", 125.0);
    let reply = session.client().unwrap().send("Runtime.evaluate", json!({"expression": "1"}), Some("session")).await;
    session.close().await.unwrap();
    assert_eq!(reply.unwrap(), json!({"result": {"value": "loading"}}));
}

// -- test_active_target.py: closing the last owned tab must produce a classified browser failure ----------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_capture_session_after_last_target_closes_is_unavailable() {
    let fake = FakeChrome::start().await;
    let session = fake.session(fake.connection()).await;
    testing::dispatch(&session, "Target.targetDestroyed", json!({"targetId": session.active_target_id()}), None);
    let error = session.active_session_id().unwrap_err();
    assert!(error.is_unavailable() && error.to_string().contains("active browser tab closed"), "{error}");
    session.close().await.unwrap();
}

// -- test_attached_recording.py --------------------------------------------------------------------------------------
//
// test_recording_preserves_attached_page_geometry_and_reattachment is not ported: it runs `run_task` and `Agent`,
// the run layer, which is not ported here.

// -- test_cursor.py: cursor geometry must belong to the page that supplied the action point --------------------------

#[cfg(feature = "cursor")]
mod cursor {
    use super::*;
    use crate::common::{control, harness};
    use fastbrowse_browser::page::CdpPage;
    use fastbrowse_browser::shared::{Action, Config, Operation, StepOutcome};
    use fastbrowse_browser::testing::lifecycle::{cursor_geometry, open_cursor};

    #[tokio::test(flavor = "multi_thread")]
    async fn test_geometry_rejects_a_point_from_another_tab() {
        let Some(h) = harness().await else { return };
        h.goto("/").await;
        let observation = h.observe().await;
        let active = h.session.active_session_id().unwrap();
        assert!(cursor_geometry(&h.page, &active, &observation.document_key).await.is_some());
        assert!(cursor_geometry(&h.page, "another-session", &observation.document_key).await.is_none());
        h.close().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_geometry_rejects_a_point_from_the_previous_document() {
        let Some(h) = harness().await else { return };
        h.goto("/").await;
        let before = h.observe().await;
        let session_id = h.session.active_session_id().unwrap();
        h.goto("/dispatch.html").await;
        let after = h.observe().await;
        assert_ne!(before.document_key, after.document_key);
        assert!(cursor_geometry(&h.page, &session_id, &before.document_key).await.is_none());
        assert!(cursor_geometry(&h.page, &session_id, &after.document_key).await.is_some());
        h.close().await;
    }

    /// A `cua-driver` that completes the handshake and then fails every tool call, as Python's mock driver does.
    const FAILING_DRIVER: &str = r#"#!/usr/bin/env python3
import json, sys
tools = ["start_session", "end_session", "list_windows", "move_cursor", "set_agent_cursor_enabled"]
started = False
for line in sys.stdin:
    message = json.loads(line)
    if "id" not in message:
        continue
    method = message.get("method")
    if method == "initialize":
        reply = {"result": {"capabilities": {}}}
    elif method == "tools/list":
        schema = {"properties": {"scope": {"enum": ["window", "desktop"]}}}
        reply = {"result": {"tools": [{"name": name, "inputSchema": schema} for name in tools]}}
    elif message["params"]["name"] == "start_session" and not started:
        started = True
        reply = {"result": {"content": [], "structuredContent": {}}}
    else:
        reply = {"error": {"code": -32000, "message": "driver unavailable"}}
    print(json.dumps({"jsonrpc": "2.0", "id": message["id"], **reply}), flush=True)
"#;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_driver_failure_does_not_repeat_or_change_a_browser_action() {
        use std::os::unix::fs::PermissionsExt;

        let Some(h) = harness().await else { return };
        let dir = temp_dir("cursor");
        let binary = dir.join("cua-driver");
        std::fs::write(&binary, FAILING_DRIVER).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let feedback = Arc::new(open_cursor(&binary).await.expect("the driver's handshake passes"));
        let page = CdpPage::new(h.session.clone(), Config::default()).with_cursor(Some(feedback.clone()));
        page.navigate(
            "data:text/html,<button onclick=\"this.textContent='Clicked '+(++window.clicks)\">Click</button>\
             <script>window.clicks=0</script>",
            None,
        )
        .await
        .unwrap();
        let observation = page.observe().await.unwrap();
        let mut action = Action::new(Operation::Click);
        action.target_id = Some(control(&observation, "Click").id.clone());
        let result = page.act(&action, &observation).await.unwrap();
        feedback.settled().await;
        assert_eq!(result.outcome, StepOutcome::Executed);
        let session_id = h.session.active_session_id().unwrap();
        assert_eq!(page.evaluate(&session_id, "window.clicks").await.unwrap(), json!(1));
        assert!(page.capture().await.unwrap().text.contains("Clicked 1"));
        // `driver.close.assert_awaited_once()`: the failure closed the driver.
        assert!(!feedback.running());
        feedback.close().await;
        h.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Python makes the first `_evaluate` on a real page raise; the fake answers the geometry read with CDP's -32000
    /// once, as a navigation that destroyed the context does.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_navigation_context_loss_skips_only_the_cursor_read() {
        let fake = FakeChrome::start().await;
        let session = fake.session(fake.connection()).await;
        let page = CdpPage::new(session.clone(), Config::default());
        let metrics = json!({"cssLayoutViewport": {"clientWidth": 800}, "layoutViewport": {"clientWidth": 800}});
        let reading = json!({"result": {"value": [0, 0, 800, 600, 800, 600, 1, true, "document"]}});
        fake.queue("SystemInfo.getProcessInfo", [json!({"processInfo": [{"type": "browser", "id": 5}]})]);
        fake.queue("Page.getLayoutMetrics", [metrics.clone(), metrics]);
        fake.script()
            .results
            .entry("Runtime.evaluate".into())
            .or_default()
            .extend([Reply::Error(-32000), Reply::Result(reading)]);
        assert!(cursor_geometry(&page, "session", "document").await.is_none());
        assert!(cursor_geometry(&page, "session", "document").await.is_some());
        session.close().await.unwrap();
    }
}
