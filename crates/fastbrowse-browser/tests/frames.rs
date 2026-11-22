//! The live view, frame callback, command deadline, active target, attached recording and cursor suites of
//! tests/browser, one section per Python file.

mod common;
mod lifecycle_support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fastbrowse_browser::error::BrowserError;
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

#[cfg(feature = "recording")]
#[tokio::test(flavor = "multi_thread")]
async fn test_a_fed_recording_writes_both_videos_with_the_cursor_and_the_drawn_card() {
    // A desktop run has no tab: it feeds window shots, and the card is drawn over the last one.
    use fastbrowse_browser::pointer::PointerMark;
    use fastbrowse_browser::recording::{Recording, ResultCard};
    use fastbrowse_core::models::Operation;

    if fastbrowse_browser::adapters::local_chrome::which("ffmpeg").is_none() {
        eprintln!("recording needs ffmpeg; skipping");
        return;
    }
    let dir = temp_dir("fed-recording");
    let still = dir.join("window.png");
    let made = tokio::process::Command::new("ffmpeg")
        .args(["-loglevel", "error", "-f", "lavfi", "-i", "color=c=white:s=320x240", "-frames:v", "1"])
        .arg(&still)
        .status()
        .await
        .unwrap();
    assert!(made.success());
    let path = dir.join("clip.mp4");
    let (mut recording, feed) = Recording::start_fed(&path).unwrap();
    feed.show(std::fs::read(&still).unwrap());
    feed.mark(PointerMark {
        at: std::time::Instant::now(),
        x: 160.0,
        y: 120.0,
        operation: Operation::Click,
        pressed: true,
        width: 320.0,
        height: 240.0,
    });
    tokio::time::sleep(Duration::from_secs_f64(0.5)).await;
    let card = ResultCard {
        status: "complete".into(),
        answer: Some("hello".into()),
        error: None,
        citations: vec![],
        steps: 1,
        known_dollars: 0.0,
    };
    recording.show_result("write hello", &card).await.unwrap();
    let outputs = recording.finish().await;
    assert_eq!(outputs, [path.clone(), dir.join("clip.plain.mp4")]);
    // The last frame is the card's dark background, not the white window it covers.
    let last = tokio::process::Command::new("ffmpeg")
        .args(["-loglevel", "error", "-sseof", "-0.5", "-i"])
        .arg(&path)
        .args(["-frames:v", "1", "-vf", "scale=1:1", "-f", "rawvideo", "-pix_fmt", "gray", "-"])
        .output()
        .await
        .unwrap();
    assert!(last.status.success());
    assert!(last.stdout.first().is_some_and(|&luma| luma < 64), "{:?}", last.stdout);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(feature = "recording")]
#[tokio::test(flavor = "multi_thread")]
async fn test_a_fed_recording_fits_a_window_of_another_size_inside_its_first_frame() {
    // A desktop run's windows differ in size: a tall dialog after a wide editor is shrunk and centred, not stretched.
    use fastbrowse_browser::recording::Recording;

    if fastbrowse_browser::adapters::local_chrome::which("ffmpeg").is_none() {
        eprintln!("recording needs ffmpeg; skipping");
        return;
    }
    let dir = temp_dir("fed-sizes");
    let shot = |name: &str, colour: &str, size: &str| {
        let path = dir.join(name);
        let source = format!("color=c={colour}:s={size}");
        async move {
            let made = tokio::process::Command::new("ffmpeg")
                .args(["-loglevel", "error", "-f", "lavfi", "-i", &source, "-frames:v", "1"])
                .arg(&path)
                .status()
                .await
                .unwrap();
            assert!(made.success());
            std::fs::read(&path).unwrap()
        }
    };
    let (wide, tall) = (shot("wide.png", "white", "320x240").await, shot("tall.png", "lime", "160x480").await);
    let path = dir.join("clip.mp4");
    let (recording, feed) = Recording::start_fed(&path).unwrap();
    feed.show(wide);
    tokio::time::sleep(Duration::from_secs_f64(0.5)).await;
    feed.show(tall);
    tokio::time::sleep(Duration::from_secs_f64(0.5)).await;
    let outputs = recording.finish().await;
    let plain = dir.join("clip.plain.mp4");
    assert_eq!(outputs, [path.clone(), plain.clone()]);
    let luma = |x: u32, y: u32| {
        let plain = plain.clone();
        async move {
            let last = tokio::process::Command::new("ffmpeg")
                .args(["-loglevel", "error", "-sseof", "-0.2", "-i"])
                .arg(&plain)
                .args([
                    "-frames:v",
                    "1",
                    "-vf",
                    &format!("crop=2:2:{x}:{y}"),
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "gray",
                    "-",
                ])
                .output()
                .await
                .unwrap();
            assert!(last.status.success(), "{}", String::from_utf8_lossy(&last.stderr));
            last.stdout[0]
        }
    };
    // The video keeps the first window's size, with the tall window centred on black.
    assert!(luma(4, 4).await < 32);
    assert!(luma(160, 120).await > 100);
    let _ = std::fs::remove_dir_all(&dir);
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
// These run on the real `CDP_REPLY_SECONDS` (60s), `CDP_PAGE_SECONDS` (10s), `CDP_ALIVE_SECONDS` (10s) and
// `CDP_COMMAND_SECONDS` (120s), which are constants a test cannot shorten as Python's monkeypatch does, so each takes
// half a minute or more and is ignored by default. Each Python scenario is kept, its times scaled to these. A command
// to a page also asks the page whether it answers, and the fake holds that `Runtime.evaluate` with the command.

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
#[ignore = "slow: waits out the real CDP_PAGE_SECONDS and CDP_ALIVE_SECONDS, about 20s"]
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
#[ignore = "slow: waits out the real CDP_PAGE_SECONDS and two CDP_ALIVE_SECONDS, about 30s"]
async fn test_a_browser_gone_by_the_command_deadline_is_an_outage() {
    let fake = FakeChrome::start().await;
    let session = fake.session(fake.connection()).await;
    fake.block("Runtime.evaluate");
    let client = session.client().unwrap();
    let sending = client.send("Runtime.evaluate", json!({"expression": "1"}), Some("session"));
    let gone = async {
        // The browser answers the probe after the first wait, and goes while the page is asked whether it answers.
        tokio::time::sleep(Duration::from_secs(15)).await;
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
#[ignore = "slow: waits out the real CDP_PAGE_SECONDS and two CDP_ALIVE_SECONDS, about 30s"]
async fn test_a_reply_that_lands_during_the_deadline_probe_is_kept() {
    let fake = FakeChrome::start().await;
    let session = fake.session(fake.connection()).await;
    // The page leaves the probe unanswered, so the command is past its deadline, and the browser probe that follows
    // waits on a browser slow to answer; the page's reply lands meanwhile. The first browser probe answers.
    fake.queue("Browser.getVersion", [json!({})]);
    fake.block("Browser.getVersion");
    fake.delay("Runtime.evaluate", 22.0);
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

#[tokio::test(flavor = "multi_thread")]
async fn test_a_closed_active_tab_hands_over_to_its_opener_else_the_last_active_tab() {
    // An OAuth popup closes itself once it has signed the run in: the run goes back to the tab that opened it, not to
    // the first tab, and a tab with no opener left hands over to the one the run was on before it.
    let fake = FakeChrome::start().await;
    let session = fake.session(fake.connection()).await;
    for (target, session_id) in [("second", "second-session"), ("third", "third-session")] {
        fake.queue("Target.createTarget", [json!({"targetId": target})]);
        fake.queue("Target.attachToTarget", [json!({"sessionId": session_id})]);
        testing::open_owned_tab(&session, "about:blank").await.unwrap();
    }
    session.switch_tab("second").await.unwrap();
    session.switch_tab("third").await.unwrap();
    fake.queue("Target.attachToTarget", [json!({"sessionId": "popup-session"})]);
    testing::adopt(&session, "popup", "second").await;
    session.switch_tab("popup").await.unwrap();
    for (closed, next) in [("popup", "second"), ("second", "third"), ("third", "owned")] {
        testing::dispatch(&session, "Target.targetDestroyed", json!({"targetId": closed}), None);
        assert_eq!(session.active_target_id(), next, "after {closed} closed");
    }
    session.close().await.unwrap();
}

/// The decision loop holds the page as `Arc<dyn Page>`, and reads an outage there as core's `Unavailable`.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_closed_tab_reaches_the_loop_as_unavailable() {
    use fastbrowse_browser::page::CdpPage;
    use fastbrowse_core::page::{BrowserError as PageError, Page};

    let fake = FakeChrome::start().await;
    let session = fake.session(fake.connection()).await;
    let page: Arc<dyn Page> = Arc::new(CdpPage::new(session.clone(), Default::default()));
    testing::dispatch(&session, "Target.targetDestroyed", json!({"targetId": session.active_target_id()}), None);
    let error = page.capture().await.unwrap_err();
    assert!(matches!(&error, PageError::Unavailable(m) if m.contains("active browser tab closed")), "{error:?}");
    session.close().await.unwrap();
}

// -- test_attached_recording.py --------------------------------------------------------------------------------------
//
// test_recording_preserves_attached_page_geometry_and_reattachment is not ported: it runs `run_task` and `Agent`,
// the run layer, which is not ported here.

/// A server that never answers leaves Chrome loading with no further event, so a load is waited on for as long as a
/// navigation is given one, from its own start, and no longer.
#[tokio::test]
async fn a_load_that_never_ends_stops_being_waited_on_once_it_has_had_a_navigations_time() {
    let fake = FakeChrome::start().await;
    let session = fake.session_with(fake.connection(), SessionOptions::default()).await;
    settle(&session).await;
    assert_eq!(session.loading_left(), None);
    tokio::time::pause();
    testing::dispatch(&session, "Page.frameStartedLoading", json!({"frameId": "top"}), Some("session"));
    assert_eq!(session.loading_left(), Some(225.0));
    tokio::time::advance(Duration::from_secs(200)).await;
    // The same load reported again is no new time to wait.
    testing::dispatch(&session, "Page.frameStartedLoading", json!({"frameId": "top"}), Some("session"));
    assert_eq!(session.loading_left(), Some(25.0));
    tokio::time::advance(Duration::from_secs(25)).await;
    assert_eq!(session.loading_left(), None);
    assert!(session.navigating(), "the load is still in flight; only the wait on it ends");
    // A load that lands ends; the next one, as a challenge page that keeps navigating starts, has its own time.
    testing::dispatch(&session, "Page.frameStoppedLoading", json!({"frameId": "top"}), Some("session"));
    testing::dispatch(&session, "Page.frameStartedLoading", json!({"frameId": "top"}), Some("session"));
    assert_eq!(session.loading_left(), Some(225.0));
}
