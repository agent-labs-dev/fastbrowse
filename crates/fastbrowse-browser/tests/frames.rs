//! The live view, frame callback, command deadline, active target, attached recording and cursor suites of
//! tests/browser, one section per Python file.

mod common;
mod lifecycle_support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fastbrowse_browser::session::{BrowserSession, SessionOptions};
use fastbrowse_browser::testing;
use fastbrowse_core::models::{BrowserWarningEvent, BrowserWarningFailure, BrowserWarningOperation, FrameHandler};
use lifecycle_support::{FakeChrome, Reply};
use serde_json::json;
use tokio::sync::watch;

#[cfg(feature = "recording")]
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

#[tokio::test]
async fn warning_callback_replaces_the_stderr_fallback() {
    const MODE: &str = "FASTBROWSE_TEST_WARNING_STDERR";
    if let Ok(mode) = std::env::var(MODE) {
        let fake = FakeChrome::start().await;
        fake.fail("Page.startScreencast", Reply::Error(-32603));
        let options = SessionOptions {
            on_frame: Some(Arc::new(|_| Box::pin(async {}))),
            on_warning: (mode == "callback")
                .then(|| Arc::new(|_: BrowserWarningEvent| {}) as Arc<dyn Fn(BrowserWarningEvent) + Send + Sync>),
            ..Default::default()
        };
        let session = fake.session_with(fake.connection(), options).await;
        settle(&session).await;
        session.close().await.unwrap();
        return;
    }
    for mode in ["fallback", "callback"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["warning_callback_replaces_the_stderr_fallback", "--exact", "--nocapture"])
            .env(MODE, mode)
            .output()
            .unwrap();
        assert!(output.status.success(), "child failed: {}", String::from_utf8_lossy(&output.stderr));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(stderr.contains("Page.startScreencast failed"), mode == "fallback", "{mode}: {stderr}");
        assert!(!stderr.contains("secret"), "warning leaked transport text: {stderr}");
    }
}

/// Frame delivery remains usable after a transport failure, and warning payloads contain only closed codes.
#[tokio::test]
async fn test_live_view_failures_are_warnings_and_leave_the_session_usable() {
    for failure in ["Page.startScreencast", "Page.stopScreencast", "Page.screencastFrameAck"] {
        let fake = FakeChrome::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let on_frame: FrameHandler = Arc::new({
            let calls = calls.clone();
            move |_frame| {
                calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {})
            }
        });
        fake.fail(failure, Reply::Error(-32603));
        let warnings = Arc::new(Mutex::new(Vec::<BrowserWarningEvent>::new()));
        let options = SessionOptions {
            on_frame: Some(on_frame),
            on_warning: Some({
                let warnings = warnings.clone();
                Arc::new(move |warning| warnings.lock().unwrap().push(warning))
            }),
            ..Default::default()
        };
        let session = fake.session_with(fake.connection(), options).await;
        settle(&session).await;
        emit_frame(&session, 1, b"jpeg", "session");
        settle(&session).await;
        emit_frame(&session, 2, b"next jpeg", "session");
        settle(&session).await;
        session.switch_tab("owned").await.unwrap();
        // The retry and the stop run in the background; close() would race them.
        settle(&session).await;
        session.close().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "{failure}");
        let calls = fake.calls();
        assert!(calls.iter().any(|m| m == "Page.screencastFrameAck"), "{failure}");
        assert!(calls.iter().any(|m| m == "Target.activateTarget"), "{failure}");
        let warnings = warnings.lock().unwrap();
        let operation = match failure {
            "Page.startScreencast" => BrowserWarningOperation::ScreencastStart,
            "Page.stopScreencast" => BrowserWarningOperation::ScreencastStop,
            _ => BrowserWarningOperation::ScreencastFrameAck,
        };
        // A refused start is tried again on the switch; a refused stop or ack is not repeated.
        let expected = if failure == "Page.stopScreencast" { 1 } else { 2 };
        assert_eq!(warnings.iter().filter(|warning| warning.operation == operation).count(), expected, "{failure}");
        assert!(warnings.iter().all(|warning| warning.failure == BrowserWarningFailure::BrowserError));
        let serialized = serde_json::to_string(&*warnings).unwrap();
        assert!(!serialized.contains("secret"), "typed warning leaked transport text: {serialized}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_stalled_frame_ack_does_not_block_delivery_or_tab_input() {
    let fake = FakeChrome::start().await;
    fake.block("Page.screencastFrameAck");
    let calls = Arc::new(AtomicUsize::new(0));
    let on_frame: FrameHandler = Arc::new({
        let calls = calls.clone();
        move |_frame| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
    });
    let options = SessionOptions { on_frame: Some(on_frame), ..Default::default() };
    let session = fake.session_with(fake.connection(), options).await;
    settle(&session).await;
    emit_frame(&session, 1, b"jpeg", "session");
    fake.sent("Page.screencastFrameAck").await;
    emit_frame(&session, 2, b"next jpeg", "session");
    testing::settle_delivery(&session).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    session.switch_tab("owned").await.unwrap();
    settle(&session).await;
    assert_eq!(testing::background_len(&session), 0);
    let client = session.client().unwrap();
    session.close().await.unwrap();
    assert_eq!(client.pending(), 0);
}

/// Chrome can refuse the first start, as while a tab is still attaching. The cast was marked running anyway, so no
/// later switch to the tab started it again and no frame ever came.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_refused_screencast_start_is_tried_again_on_the_next_switch() {
    let fake = FakeChrome::start().await;
    fake.script().results.entry("Page.startScreencast".into()).or_default().push_back(Reply::Error(-32000));
    let calls = Arc::new(AtomicUsize::new(0));
    let on_frame: FrameHandler = Arc::new({
        let calls = calls.clone();
        move |_frame| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
    });
    let options = SessionOptions { on_frame: Some(on_frame), ..Default::default() };
    let session = fake.session_with(fake.connection(), options).await;
    settle(&session).await;
    assert_eq!(fake.script().count("Page.startScreencast"), 1);
    session.switch_tab("owned").await.unwrap();
    settle(&session).await;
    assert_eq!(fake.script().count("Page.startScreencast"), 2);
    emit_frame(&session, 1, b"jpeg", "session");
    settle(&session).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    session.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_stalled_screencast_does_not_delay_switching_tabs() {
    let fake = FakeChrome::start().await;
    fake.block("Page.startScreencast");
    let on_frame: FrameHandler = Arc::new(|_frame| Box::pin(async {}));
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

/// A start Chrome runs but never answers in time, or that a close cancels mid-await, was recorded only after the
/// answer, so close never stopped a cast Chrome might still be sending.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_screencast_start_cancelled_by_close_is_still_stopped() {
    let fake = FakeChrome::start().await;
    fake.block("Page.startScreencast");
    let on_frame: FrameHandler = Arc::new(|_frame| Box::pin(async {}));
    let options = SessionOptions { on_frame: Some(on_frame), ..Default::default() };
    let session = fake.session_with(fake.connection(), options).await;
    fake.sent("Page.startScreencast").await;
    session.close().await.unwrap();
    assert!(fake.calls().iter().any(|m| m == "Page.stopScreencast"), "{:?}", fake.calls());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_screencast_start_that_times_out_is_stopped_on_close() {
    let fake = FakeChrome::start().await;
    fake.block("Page.startScreencast");
    let on_frame: FrameHandler = Arc::new(|_frame| Box::pin(async {}));
    let (timed_out, mut timeouts) = tokio::sync::mpsc::unbounded_channel();
    let on_warning = Arc::new(move |warning: BrowserWarningEvent| {
        if warning.operation == BrowserWarningOperation::ScreencastStart
            && warning.failure == BrowserWarningFailure::Timeout
        {
            let _ = timed_out.send(());
        }
    });
    let options = SessionOptions { on_frame: Some(on_frame), on_warning: Some(on_warning), ..Default::default() };
    let session = fake.session_with(fake.connection(), options).await;
    fake.sent("Page.startScreencast").await;
    // The start must end unanswered, not cancelled by close.
    tokio::time::timeout(Duration::from_secs(10), timeouts.recv()).await.unwrap().expect("start timeout warning");
    session.close().await.unwrap();
    assert!(fake.calls().iter().any(|m| m == "Page.stopScreencast"), "{:?}", fake.calls());
}

/// Python also checks the warning that nothing was recorded; see the note on the live view failures test.
#[cfg(feature = "recording")]
#[tokio::test(flavor = "multi_thread")]
async fn test_a_recording_whose_cast_sent_no_frame_ends_on_a_screenshot_unless_frames_are_withheld() {
    // Chrome can acknowledge `Page.startScreencast` and then send nothing, and a short run ends before the restart.
    use fastbrowse_browser::recording::Recording;
    use fastbrowse_browser::session::{BoxError, CheckAccess};

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
