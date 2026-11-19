//! Record a run to a video: a tab's screencast frames, or the frames a caller feeds it such as a desktop window's
//! shots, resampled to a steady rate by ffmpeg.
//!
//! The screencast sends a frame only when the page repaints, and a fed recording gets one only when its caller takes
//! one, so a ticker repeats the latest frame at `FPS`; the video's timing is then the run's real timing. The recording
//! follows tab switches, captions each step from when its action began, and ends on a card showing the run's outcome,
//! so one file shows both what the agent did and what it answered. Two more ffmpeg passes over a lossless first one
//! write it twice, one encoder at a time: with the step captions burned in, and plain beside it as `<name>.plain.mp4`.
//!
//! A recording shows whatever the page shows. Typed passwords are masked by the page, but an email address, an order
//! history or an echoed key is not, so check a recording before sharing it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use data_encoding::BASE64;
use fastbrowse_core::models::{Decider, Operation, StepResult};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use crate::adapters::local_chrome::which;
use crate::cdp::secs;
use crate::pointer::PointerMark;
use crate::session::BrowserSession;
use crate::urlsplit::urlsplit;
use crate::warn;

const FPS: u32 = 25;
/// A cast with no frame this long is restarted. A start sent while the tab swaps renderer on a cross-site navigation
/// can go unanswered, and GitHub runs then recorded nothing; restarting a live cast only costs a frame.
const RECAST_SECONDS: f64 = 1.0;
const COMMAND_SECONDS: f64 = 0.5;
/// How long a run that ended without a frame waits for one screenshot of its tab, so a stalled capture cannot hold up
/// the run's result.
const STILL_SECONDS: f64 = 1.0;
/// Long enough to read a one-line answer in a shared clip.
const RESULT_SECONDS: f64 = 4.0;

/// ffmpeg is missing, or the run is scoped. Later failures are logged instead: a video is never worth a run's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingError(pub String);

impl std::fmt::Display for RecordingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RecordingError {}

/// A cited fact the result card lists: `Citation`'s id, url and quote.
#[derive(Debug, Clone)]
pub struct CardCitation {
    pub id: u32,
    pub url: String,
    pub quote: String,
}

/// What the closing card shows of a `RunResult`.
#[derive(Debug, Clone)]
pub struct ResultCard {
    pub status: String,
    pub answer: Option<String>,
    pub error: Option<String>,
    pub citations: Vec<CardCitation>,
    pub steps: usize,
    pub known_dollars: f64,
}

#[derive(Default)]
struct Frames {
    frame: Option<Vec<u8>>,
    frame_at: Option<Instant>,
    /// The video starts at its first frame, which the browser sends some time after entry.
    first_frame_at: Option<Instant>,
    casting: Option<String>,
}

/// The frames a fed recording films, and where its cursor goes, from a caller with no tab to cast.
#[derive(Clone)]
pub struct FrameFeed {
    frames: Arc<Mutex<Frames>>,
    marks: Arc<Mutex<Vec<PointerMark>>>,
}

impl FrameFeed {
    /// Show `frame`, a PNG or JPEG, from now until the next one. The caller holds back a frame that may show a secret.
    pub fn show(&self, frame: Vec<u8>) {
        let mut frames = self.frames.lock().unwrap();
        frames.frame_at = Some(Instant::now());
        frames.frame = Some(frame);
    }

    /// Draw the cursor gliding to `mark` as the action there begins, as a tab recording does.
    pub fn mark(&self, mark: PointerMark) {
        self.marks.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(mark);
    }
}

/// What the closing card of a fed recording shows, drawn over its last frame when it is encoded: there is no tab to
/// open the card in.
struct DrawnCard {
    task: String,
    result: ResultCard,
    seconds: f64,
}

/// Writes the session's active tab, or the frames fed to it, to `path` (an .mp4) from `start` until `finish`.
pub struct Recording {
    /// The session whose active tab is cast, or None for a fed recording.
    session: Option<BrowserSession>,
    path: PathBuf,
    ffmpeg_path: PathBuf,
    scratch: PathBuf,
    ffmpeg: Option<Child>,
    stdin: Arc<tokio::sync::Mutex<Option<ChildStdin>>>,
    frames: Arc<Mutex<Frames>>,
    ticker: Option<JoinHandle<()>>,
    started: Instant,
    captions: Vec<(Instant, String)>,
    pointer_marks: Arc<Mutex<Vec<PointerMark>>>,
    card_at: Option<Instant>,
    drawn_card: Option<DrawnCard>,
    /// The finished videos, set only once both are encoded: a failed run must not report a file it did not write.
    pub outputs: Vec<PathBuf>,
}

impl Recording {
    pub async fn start(session: BrowserSession, path: &Path) -> Result<Self, RecordingError> {
        Self::start_with_marks(session, path, Arc::new(Mutex::new(Vec::new()))).await
    }

    /// start ffmpeg and the cast, and begin writing frames.
    pub async fn start_with_marks(
        session: BrowserSession,
        path: &Path,
        pointer_marks: Arc<Mutex<Vec<PointerMark>>>,
    ) -> Result<Self, RecordingError> {
        if session.grant().is_some() {
            return Err(RecordingError(
                "a recording shows whatever the tab shows, so it cannot be used with allowed_origins".into(),
            ));
        }
        let mut recording = Self::spawn(Some(session.clone()), path)?;
        recording.pointer_marks = pointer_marks;
        let frames = recording.frames.clone();
        let tapped = (session.clone(), frames);
        session.set_frame_tap(Some(Arc::new(move |event| {
            let (session, frames) = &tapped;
            // A background run keeps each tab painting with a 16-pixel cast on a session of its own, which is no
            // picture of the tab.
            if event.session.as_deref() != session.active_session_id().ok().as_deref() {
                return;
            }
            let mut frames = frames.lock().unwrap();
            frames.frame_at = Some(Instant::now());
            // The video holds its last clean frame rather than showing a secret.
            if !session.frames_withheld()
                && let Ok(frame) = BASE64.decode(event.params["data"].as_str().unwrap_or_default().as_bytes())
            {
                frames.frame = Some(frame);
            }
        })));
        Ok(recording)
    }

    /// Start ffmpeg for frames the caller feeds through the returned `FrameFeed`, as a desktop run does with its
    /// window's shots.
    pub fn start_fed(path: &Path) -> Result<(Self, FrameFeed), RecordingError> {
        let recording = Self::spawn(None, path)?;
        let feed = FrameFeed { frames: recording.frames.clone(), marks: recording.pointer_marks.clone() };
        Ok((recording, feed))
    }

    /// Start the lossless ffmpeg pass and the ticker that writes the latest frame to it.
    fn spawn(session: Option<BrowserSession>, path: &Path) -> Result<Self, RecordingError> {
        let ffmpeg_path = which("ffmpeg")
            .map(PathBuf::from)
            .ok_or_else(|| RecordingError("recording needs ffmpeg on PATH".into()))?;
        let scratch = std::env::temp_dir().join(format!(
            "fastbrowse-recording-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&scratch)
            .map_err(|e| RecordingError(format!("could not make a scratch directory: {e}")))?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let uncaptioned = scratch.join("uncaptioned.mkv");
        let mut ffmpeg = Command::new(&ffmpeg_path)
            .args(["-loglevel", "error", "-y", "-f", "image2pipe", "-framerate", &FPS.to_string(), "-i", "-"])
            // Lossless, so the captioning pass is the only lossy encode; 4:4:4 also takes the odd sizes a tab has.
            .args(["-c:v", "libx264", "-qp", "0", "-preset", "ultrafast", "-pix_fmt", "yuv444p"])
            .arg(&uncaptioned)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| RecordingError(format!("could not start ffmpeg: {}", e.kind())))?;
        let stdin = Arc::new(tokio::sync::Mutex::new(ffmpeg.stdin.take()));
        let frames = Arc::new(Mutex::new(Frames::default()));
        let started = Instant::now();
        let ticker = tokio::spawn(tick(session.clone(), frames.clone(), stdin.clone()));
        Ok(Self {
            session,
            path: path.to_owned(),
            ffmpeg_path,
            scratch,
            ffmpeg: Some(ffmpeg),
            stdin,
            frames,
            ticker: Some(ticker),
            started,
            captions: vec![],
            pointer_marks: Arc::default(),
            card_at: None,
            drawn_card: None,
            outputs: vec![],
        })
    }

    fn plain_path(&self) -> PathBuf {
        let stem = self.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let suffix = self.path.extension().map(|s| format!(".{}", s.to_string_lossy())).unwrap_or_default();
        self.path.with_file_name(format!("{stem}.plain{suffix}"))
    }

    /// Caption `step` from when its action began; it arrives once the step is over.
    pub fn caption(&mut self, step: &StepResult) {
        let began = Instant::now()
            .checked_sub(std::time::Duration::from_millis(step.duration_ms.max(0) as u64))
            .unwrap_or(self.started);
        self.captions.push((began, format!("{} \u{b7} {}", step.index + 1, describe(step))));
    }

    /// End the video on the task and its outcome, in the tab being recorded, or drawn over a fed recording's last
    /// frame.
    pub async fn show_result(&mut self, task: &str, result: &ResultCard) -> Result<(), crate::error::BrowserError> {
        let Some(session) = self.session.clone() else {
            let card_at = Instant::now();
            self.card_at = Some(card_at);
            let seconds = (card_at - self.started).as_secs_f64();
            self.drawn_card = Some(DrawnCard { task: task.to_owned(), result: result.clone(), seconds });
            sleep(secs(RESULT_SECONDS)).await;
            return Ok(());
        };
        let text = result.answer.clone().or_else(|| result.error.clone()).unwrap_or_default();
        let parts = split_citation_links(&text);
        let cited: Vec<u32> = parts.iter().skip(1).step_by(2).filter_map(|n| n.parse().ok()).collect();
        let answer: String = parts
            .iter()
            .enumerate()
            .map(|(i, part)| {
                if i % 2 == 1 {
                    format!("<sup style='margin-left:4px;font-size:18px;color:#58a6ff'>{part}</sup>")
                } else {
                    html_escape(part)
                }
            })
            .collect();
        let sources: String = result
            .citations
            .iter()
            .filter(|c| cited.contains(&c.id))
            .map(|c| {
                format!(
                    "<div style='white-space:nowrap;overflow:hidden;text-overflow:ellipsis'>\
                     <span style='color:#58a6ff'>{}</span> {} <span style='color:#e6edf3'>\u{201c}{}\u{201d}</span></div>",
                    c.id,
                    html_escape(&urlsplit(&c.url).1),
                    html_escape(&c.quote)
                )
            })
            .collect();
        let card_at = Instant::now();
        self.card_at = Some(card_at);
        let mut card = format!(
            "<meta charset=utf-8><body style='margin:0;height:100vh;display:grid;place-content:center;gap:28px;\
             grid-template-columns:minmax(0,1fr);padding:0 8vw;background:#0d1117;color:#e6edf3;\
             font:24px system-ui,sans-serif'>\
             <div style='color:#8b949e'>{}</div>\
             <div style='font-size:40px;font-weight:600'>{answer}</div>",
            html_escape(task)
        );
        if !sources.is_empty() {
            card.push_str(&format!("<div style='display:grid;gap:6px;color:#8b949e;font-size:20px'>{sources}</div>"));
        }
        card.push_str(&format!(
            "<div style='color:#3fb950'>{}</div></body>",
            html_escape(&outcome_line(result, (card_at - self.started).as_secs_f64()))
        ));
        let url = format!("data:text/html;base64,{}", BASE64.encode(card.as_bytes()));
        let session_id = session.active_session_id()?;
        session.send("Page.navigate", json!({"url": url}), Some(&session_id)).await?;
        sleep(secs(RESULT_SECONDS)).await;
        Ok(())
    }

    /// stop the ticker, close the lossless pass and encode both videos.
    pub async fn finish(mut self) -> Vec<PathBuf> {
        if let Some(ticker) = self.ticker.take() {
            ticker.abort();
            let _ = ticker.await;
        }
        if let Some(session) = &self.session {
            session.set_frame_tap(None);
        }
        self.close().await;
        let _ = std::fs::remove_dir_all(&self.scratch);
        std::mem::take(&mut self.outputs)
    }

    async fn close(&mut self) {
        let Some(mut ffmpeg) = self.ffmpeg.take() else { return };
        let Some(mut stdin) = self.stdin.lock().await.take() else { return };
        // Chrome can acknowledge a cast and then send nothing, and a short run, an attached one with no result card,
        // ends before the cast is restarted: the video is then one still of the tab rather than nothing at all.
        let (first, held) = {
            let frames = self.frames.lock().unwrap();
            (frames.first_frame_at, frames.frame.clone())
        };
        let mut first = first;
        if first.is_none() {
            let frame = match held {
                Some(frame) => Some(frame),
                None => self.still().await,
            };
            if let Some(frame) = frame
                && stdin.write_all(&frame).await.is_ok()
            {
                first = Some(Instant::now());
            }
        }
        let Some(first) = first else {
            let _ = ffmpeg.start_kill();
            let _ = ffmpeg.wait().await;
            let source = if self.session.is_some() { "the browser sent" } else { "the run took" };
            warn(&format!("{source} no frames, so nothing was recorded to {}", self.path.display()));
            return;
        };
        drop(stdin);
        match ffmpeg.wait_with_output().await {
            Ok(done) if done.status.success() => {}
            Ok(done) => {
                let error = String::from_utf8_lossy(&done.stderr);
                warn(&format!("ffmpeg could not write {}: {}", self.path.display(), cut(error.trim(), 400)));
                return;
            }
            Err(error) => {
                warn(&format!("ffmpeg could not write {}: {}", self.path.display(), error.kind()));
                return;
            }
        }
        self.encode_all(first).await;
    }

    /// One screenshot of the active tab, or None while frames are withheld, when the capture fails, or with no tab.
    async fn still(&self) -> Option<Vec<u8>> {
        // A fed recording's caller has no frame to give that it did not already feed.
        let session = self.session.as_ref()?;
        if session.frames_withheld() {
            return None;
        }
        let shot = timeout(secs(STILL_SECONDS), async {
            // A caller who revoked access after the last step gets no screenshot taken at teardown either.
            session.assert_clear().await?;
            let session_id = session.active_session_id()?;
            session.send("Page.captureScreenshot", json!({"format": "jpeg", "quality": 95}), Some(&session_id)).await
        })
        .await;
        let frame = match shot {
            Ok(Ok(shot)) => BASE64.decode(shot["data"].as_str().unwrap_or_default().as_bytes()).ok()?,
            // Transport errors may contain credentials or page text.
            Ok(Err(error)) => {
                warn(&format!("Page.captureScreenshot for the recording failed ({})", error.kind()));
                return None;
            }
            Err(_) => {
                warn("Page.captureScreenshot for the recording failed (TimeoutError)");
                return None;
            }
        };
        // Checked again for the page as it is once the image exists, as a live frame is when it arrives: the page can
        // come to show a secret while the capture is in flight.
        (!session.frames_withheld()).then_some(frame)
    }

    /// Encode the final video twice from the lossless pass, one pass after the other: with step captions, and plain.
    async fn encode_all(&mut self, origin: Instant) {
        let subtitles = self.scratch.join("steps.srt");
        // A run that opens straight onto its answer takes no steps, and has no captions to end.
        let mut timed = self.captions.clone();
        timed.push((self.card_at.unwrap_or_else(Instant::now), String::new()));
        let since = |at: Instant| at.saturating_duration_since(origin).as_secs_f64();
        let cues: Vec<String> = timed
            .windows(2)
            // A step can end before the browser sends its first frame; its caption starts with the video.
            .filter(|pair| pair[1].0 > pair[0].0.max(origin))
            .map(|pair| {
                // libass reads `{...}` as a style override, so a page's braces are escaped to stay text.
                format!(
                    "{} --> {}\n{}\n",
                    srt_time(since(pair[0].0)),
                    srt_time(since(pair[1].0)),
                    pair[0].1.replace('{', "\\{")
                )
            })
            .collect();
        let srt: Vec<String> = cues.iter().enumerate().map(|(n, cue)| format!("{}\n{cue}", n + 1)).collect();
        let _ = tokio::fs::write(&subtitles, srt.join("\n")).await;
        let marks = self.pointer_marks.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        let card_at = self.card_at.unwrap_or_else(Instant::now);
        let mut overlays: Vec<(&str, String)> = Vec::new();
        if !marks.is_empty() || self.drawn_card.is_some() {
            match self.frame_dimensions().await {
                Some((width, height)) => {
                    if !marks.is_empty() {
                        overlays.push(("cursor.ass", cursor_ass(&marks, origin, card_at, width, height)));
                    }
                    if let Some(card) = &self.drawn_card {
                        overlays.push(("card.ass", card_ass(card, since(card_at), width, height)));
                    }
                }
                None => warn("could not read the recording dimensions, so the cursor and result card are omitted"),
            }
        }
        let mut drawn = Vec::new();
        for (name, ass) in overlays {
            match tokio::fs::write(self.scratch.join(name), ass).await {
                Ok(()) => drawn.push(format!("subtitles={name}")),
                Err(error) => warn(&format!("could not write the recording's {name}: {error}")),
            }
        }
        // BorderStyle 3 draws a box behind the text, coloured by OutlineColour (alpha first, 00 opaque).
        let style = "Fontsize=10,BorderStyle=3,Outline=6,Shadow=0,OutlineColour=&H50000000,MarginV=16,Alignment=2";
        // libass cannot open an empty subtitle file. The filter names it relative to ffmpeg's working directory: an
        // absolute path's drive colon and backslashes are filter syntax, and a Windows run failed on them.
        let captions = (!cues.is_empty()).then(|| format!("subtitles=steps.srt:force_style='{style}'"));
        let mut error = self.encode(captions.as_deref(), &drawn).await;
        if let Some(failure) = &error
            && (captions.is_some() || !drawn.is_empty())
        {
            // An ffmpeg built without libass has no subtitles filter; the plain video is still worth keeping.
            warn(&format!(
                "ffmpeg could not caption {}, so it is written without captions: {failure}",
                self.path.display()
            ));
            error = self.encode(None, &[]).await;
        }
        if let Some(failure) = error {
            warn(&format!("ffmpeg could not write {}: {failure}", self.path.display()));
            return;
        }
        // Encoded in scratch, then copied beside each path and renamed over it: a failed encode or copy leaves whatever
        // was already there untouched, and scratch cleanup takes the partial files with it.
        let mut written = vec![];
        for (name, path) in [("captioned.mp4", self.path.clone()), ("plain.mp4", self.plain_path())] {
            match replace(&self.scratch.join(name), &path) {
                Ok(()) => written.push(path),
                Err(error) => warn(&format!("could not write {}: {error}", path.display())),
            }
        }
        self.outputs = written;
    }

    async fn frame_dimensions(&self) -> Option<(u32, u32)> {
        let sibling = self.ffmpeg_path.with_file_name(format!("ffprobe{}", std::env::consts::EXE_SUFFIX));
        let probe = if sibling.is_file() { sibling } else { PathBuf::from(which("ffprobe")?) };
        let result = Command::new(probe)
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height",
                "-of",
                "csv=s=x:p=0",
            ])
            .arg(self.scratch.join("uncaptioned.mkv"))
            .output()
            .await
            .ok()?;
        if !result.status.success() {
            return None;
        }
        let text = String::from_utf8(result.stdout).ok()?;
        let (width, height) = text.trim().split_once('x')?;
        let (width, height) = (width.parse().ok()?, height.parse().ok()?);
        (width > 0 && height > 0).then_some((width, height))
    }

    /// Write both videos into scratch, the captioned one with `captions` and both with `overlays`; return ffmpeg's
    /// error, if any. Each video is its own ffmpeg pass over the lossless one, so a recording holds one x264 encoder's
    /// memory at a time; the plain pass does not start when the captioned one failed.
    async fn encode(&self, captions: Option<&str>, overlays: &[String]) -> Option<String> {
        for (output, filter) in encode_passes(captions, overlays) {
            let done = Command::new(&self.ffmpeg_path)
                .args(encode_args(&filter, output))
                .current_dir(&self.scratch)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .output()
                .await;
            match done {
                Ok(done) if done.status.success() => {}
                Ok(done) => {
                    let error = String::from_utf8_lossy(&done.stderr).trim().to_owned();
                    return Some(if error.is_empty() {
                        format!("exit status {}", done.status.code().unwrap_or(-1))
                    } else {
                        cut(&error, 400)
                    });
                }
                Err(error) => return Some(error.kind().to_string()),
            }
        }
        None
    }
}

/// The two final videos and the filter each is encoded through: the captioned one burns `captions` in, and both
/// carry `overlays`, the cursor and a drawn result card, in order.
fn encode_passes(captions: Option<&str>, overlays: &[String]) -> [(&'static str, String); 2] {
    // H.264 needs even dimensions, and yuv420p is what phones and social sites play.
    let base = "scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p";
    let chain = |captions: Option<&str>| {
        std::iter::once(base).chain(captions).chain(overlays.iter().map(String::as_str)).collect::<Vec<_>>().join(",")
    };
    [("captioned.mp4", chain(captions)), ("plain.mp4", chain(None))]
}

/// One ffmpeg pass from the lossless video in scratch to `output` through `filter`, with one encoder.
fn encode_args(filter: &str, output: &str) -> Vec<String> {
    // Page text is the subject of a shared clip; x264's default quality blurs small type.
    [
        "-loglevel",
        "error",
        "-y",
        "-i",
        "uncaptioned.mkv",
        "-vf",
        filter,
        "-c:v",
        "libx264",
        "-crf",
        "18",
        "-preset",
        "slow",
        "-movflags",
        "+faststart",
        output,
    ]
    .map(str::to_owned)
    .to_vec()
}

async fn tick(
    session: Option<BrowserSession>,
    frames: Arc<Mutex<Frames>>,
    stdin: Arc<tokio::sync::Mutex<Option<ChildStdin>>>,
) {
    let mut written: u32 = 0;
    loop {
        // A fed recording has no cast to keep alive: its caller sends each frame.
        if let Some(session) = &session {
            let Ok(active) = session.active_session_id() else { return };
            let recast = {
                let frames = frames.lock().unwrap();
                !session.has_frame_handler()
                    && (frames.casting.as_deref() != Some(active.as_str())
                        || frames.frame_at.is_none_or(|at| at.elapsed() > secs(RECAST_SECONDS)))
            };
            if recast {
                cast(session, &frames, &active).await;
            }
        }
        let frame = frames.lock().unwrap().frame.clone();
        if let Some(frame) = frame {
            let mut stdin = stdin.lock().await;
            let Some(pipe) = stdin.as_mut() else { return };
            if pipe.write_all(&frame).await.is_err() || pipe.flush().await.is_err() {
                return;
            }
            let mut frames = frames.lock().unwrap();
            if frames.first_frame_at.is_none() {
                frames.first_frame_at = Some(Instant::now());
            }
            written += 1;
        }
        // The schedule starts at the first frame, like the captions: one started earlier wrote a burst of catch-up
        // frames after a slow first cast, and the video ran ahead of its captions.
        let first = frames.lock().unwrap().first_frame_at;
        let due = match first {
            None => Instant::now() + secs(1.0 / f64::from(FPS)),
            Some(first) => first + secs(f64::from(written) / f64::from(FPS)),
        };
        sleep(due.saturating_duration_since(Instant::now())).await;
    }
}

async fn cast(session: &BrowserSession, frames: &Mutex<Frames>, session_id: &str) {
    // The previous tab may already be closed, taking its screencast with it; an unanswered start is retried.
    let casting = frames.lock().unwrap().casting.clone();
    if let Some(casting) = casting {
        let _ = timeout(secs(COMMAND_SECONDS), session.send("Page.stopScreencast", json!({}), Some(&casting))).await;
    }
    let start = session.send("Page.startScreencast", json!({"format": "jpeg", "quality": 95}), Some(session_id));
    let _ = timeout(secs(COMMAND_SECONDS), start).await;
    let mut frames = frames.lock().unwrap();
    frames.casting = Some(session_id.to_owned());
    frames.frame_at = Some(Instant::now());
}

fn describe(step: &StepResult) -> String {
    let target = step.target.as_ref().map(|t| format!(" {t}")).unwrap_or_default();
    let by = match step.decided_by {
        Decider::Jev => "picked by Jev",
        Decider::Code => "picked by code",
        Decider::Recipe => "recipe action confirmed by Jev",
    };
    match step.operation {
        Operation::Read => format!("read the page \u{b7} {by}"),
        Operation::Done => "check the answer".into(),
        operation => format!("{}{target} \u{b7} {by}", operation.as_str().replace('_', " ")),
    }
}

fn srt_time(seconds: f64) -> String {
    // Python's round() is half to even.
    let millis = (seconds * 1000.0).round_ties_even() as u64;
    format!("{:02}:{:02}:{:02},{:03}", millis / 3_600_000, millis / 60_000 % 60, millis / 1000 % 60, millis % 1000)
}

fn ass_time(seconds: f64) -> String {
    let cs = (seconds.max(0.0) * 100.0).round() as u64;
    format!("{}:{:02}:{:02}.{:02}", cs / 360_000, cs / 6_000 % 60, cs / 100 % 60, cs % 100)
}

/// The card's closing line: how the run ended, how long it took, its steps and its known spend.
fn outcome_line(result: &ResultCard, seconds: f64) -> String {
    format!("{} in {seconds:.1}s, {} steps, ${:.4}", result.status, result.steps, result.known_dollars)
}

/// `text` as ASS dialogue text that libass shows as written.
fn ass_text(text: &str) -> String {
    // libass reads `{...}` as a style override and a backslash before N, n or h as a line break or a hard space, so a
    // word joiner after each backslash and an escaped brace keep an answer's own characters text.
    text.replace('\\', "\\\u{2060}").replace('{', "\\{").replace("\r\n", "\n").replace('\n', "\\N")
}

/// The result card a tab shows, as an ASS overlay that covers the frame from `start` seconds to the end of the video:
/// the task, the answer with its citation numbers, the cited quotes and the outcome line, in the tab card's colours.
fn card_ass(card: &DrawnCard, start: f64, width: u32, height: u32) -> String {
    // The tab card's sizes are CSS pixels on a page about 720 pixels tall.
    let px = |size: f64| (size * f64::from(height) / 720.0).round().max(1.0);
    let margin = (f64::from(width) * 0.08).round();
    let (grey, white, blue, green) = ("&H9E948B&", "&HF3EDE6&", "&HFFA658&", "&H50B93F&");
    let text = card.result.answer.clone().or_else(|| card.result.error.clone()).unwrap_or_default();
    let parts = split_citation_links(&text);
    let cited: Vec<u32> = parts.iter().skip(1).step_by(2).filter_map(|n| n.parse().ok()).collect();
    let answer: String = parts
        .iter()
        .enumerate()
        .map(|(i, part)| if i % 2 == 1 { format!("{{\\c{blue}}} {part}{{\\c{white}}}") } else { ass_text(part) })
        .collect();
    let mut body = format!(
        "{{\\fs{}\\c{grey}}}{}\\N\\N{{\\fs{}\\b1\\c{white}}}{answer}{{\\b0}}",
        px(24.0),
        ass_text(&card.task),
        px(40.0)
    );
    let sources: Vec<String> = card
        .result
        .citations
        .iter()
        .filter(|c| cited.contains(&c.id))
        .map(|c| {
            format!(
                "{{\\c{blue}}}{}{{\\c{grey}}} {} {{\\c{white}}}\u{201c}{}\u{201d}",
                c.id,
                ass_text(&urlsplit(&c.url).1),
                ass_text(&c.quote)
            )
        })
        .collect();
    if !sources.is_empty() {
        body.push_str(&format!("\\N\\N{{\\fs{}}}{}", px(20.0), sources.join("\\N")));
    }
    body.push_str(&format!(
        "\\N\\N{{\\fs{}\\c{green}}}{}",
        px(24.0),
        ass_text(&outcome_line(&card.result, card.seconds))
    ));
    // Drawn to the end of whatever the video holds after the card begins; the ticker stops shortly after it.
    let (from, to) = (ass_time(start), ass_time(start + 3600.0));
    format!(
        "[Script Info]\nScriptType: v4.00+\nPlayResX: {width}\nPlayResY: {height}\nWrapStyle: 0\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Arial,{},&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,0,0,4,{margin},{margin},0,1\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,{from},{to},Default,,0,0,0,,{{\\an7\\pos(0,0)\\p1\\1c&H17110D&}}m 0 0 l {width} 0 {width} {height} 0 {height}{{\\p0}}\nDialogue: 1,{from},{to},Default,,0,0,0,,{body}\n",
        px(24.0)
    )
}

/// The agent cursor of deskpilot's Hyprland plugin (`drawOne` and `glideTo` in src/deskpilot/plugin/deskpilot.cpp), in
/// logical pixels at a 1280 px wide frame, with fastbrowse's own colour and centre dot. A change to the look is made
/// there first and copied here.
mod agent_cursor {
    /// The arm of a bracket, and the width of its rounded stroke.
    pub const BRACKET: f64 = 6.0;
    pub const STROKE: f64 = 1.8;
    /// The brackets' half-width at rest and while gliding, and how far in they move on a press.
    pub const REST: f64 = 15.0;
    pub const PRESS: f64 = 7.0;
    /// A press: the brackets move in to `PRESS` over `SQUEEZE_IN` seconds (cubic ease-out), stay there for
    /// `SQUEEZE_HOLD` and go back to `REST` over `SQUEEZE_OUT` (cubic ease-in-out), computed for each `SQUEEZE_STEP`,
    /// the finest time ASS has.
    pub const SQUEEZE_IN: f64 = 0.05;
    pub const SQUEEZE_HOLD: f64 = 0.07;
    pub const SQUEEZE_OUT: f64 = 0.10;
    pub const SQUEEZE_STEP: f64 = 0.01;
    /// A press also leaves a filled disc at 0.3 opacity fading out, growing from 0.3 to 1 times `PULSE_SIZE` with a
    /// cubic ease-out over `PULSE` seconds, staying where it was pressed, and the centre is squeezed to `PRESSED`.
    pub const PULSE: f64 = 0.38;
    pub const PULSE_SIZE: f64 = 30.0;
    pub const PRESSED: f64 = 0.85;
    /// A glide takes 1.5 times its length over `PEAK_SPEED`, between `MIN_GLIDE` and `MAX_GLIDE` seconds, along an arc
    /// bowed to one side by `ARC` of its length, eased in and out (cubic).
    pub const PEAK_SPEED: f64 = 900.0;
    pub const MIN_GLIDE: f64 = 0.12;
    pub const MAX_GLIDE: f64 = 0.7;
    pub const ARC: f64 = 0.2;
    /// fastbrowse's own centre: a dot of this radius.
    pub const DOT: f64 = 2.4;
    /// The time between a glide's drawn states: ASS cannot ease a position, so a glide is sampled this often and each
    /// piece moves in a line.
    pub const STEP: f64 = 0.02;
}

fn ease_out(t: f64) -> f64 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

fn ease_in_out(t: f64) -> f64 {
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

/// An ASS `\alpha` value for an opacity.
fn ass_alpha(opacity: f64) -> String {
    format!("&H{:02X}&", ((1.0 - opacity.clamp(0.0, 1.0)) * 255.0).round() as u8)
}

/// The brackets and centre dot at `half` from the centre, `unit` frame pixels to a logical pixel. Each arm is a thin
/// rectangle given the stroke's width as a round outline, so its ends are round as deskpilot's are.
fn cursor_shape(half: f64, dot: f64, unit: f64) -> String {
    use agent_cursor::{BRACKET, DOT, STROKE};
    let round = STROKE / 2.0;
    let mut path = String::new();
    let mut line = |x0: f64, y0: f64, x1: f64, y1: f64| {
        let (x0, y0, x1, y1) = (x0 * unit, y0 * unit, x1 * unit, y1 * unit);
        let thin = 0.1;
        path.push_str(&format!(
            "m {x0:.2} {:.2} l {x1:.2} {:.2} {x1:.2} {:.2} {x0:.2} {:.2} ",
            y0 - thin,
            y1 - thin,
            y1 + thin,
            y0 + thin
        ));
    };
    for sx in [-1.0, 1.0] {
        for sy in [-1.0, 1.0] {
            let (cx, cy) = (sx * half, sy * half);
            let hx = if sx < 0.0 { cx - round } else { cx - BRACKET + round };
            let vy = if sy < 0.0 { cy - round } else { cy - BRACKET + round };
            line(hx + round, cy, hx + BRACKET - round, cy);
            line(cx, vy + round, cx, vy + BRACKET - round);
        }
    }
    // The dot's outline is the stroke's, so its drawn radius is the dot's less the outline.
    let radius = ((DOT * dot - round) * unit).max(0.1);
    path.push_str(&disc(radius));
    path
}

/// A filled disc of `radius` frame pixels about the origin.
fn disc(radius: f64) -> String {
    let (r, k) = (radius, radius * 0.5523);
    format!(
        "m -{r:.2} 0 b -{r:.2} -{k:.2} -{k:.2} -{r:.2} 0 -{r:.2} b {k:.2} -{r:.2} {r:.2} -{k:.2} {r:.2} 0 b {r:.2} {k:.2} {k:.2} {r:.2} 0 {r:.2} b -{k:.2} {r:.2} -{r:.2} {k:.2} -{r:.2} 0 "
    )
}

/// The cursor as an ASS overlay, drawn as deskpilot's agent cursor is (see `agent_cursor`): brackets at one size glide
/// to each action point along an eased arc; a press moves them in towards the centre dot and back, and leaves a disc
/// that grows and fades where it was pressed. Points are CSS pixels, scaled to the frame.
fn cursor_ass(marks: &[PointerMark], origin: Instant, until: Instant, width: u32, height: u32) -> String {
    use agent_cursor::*;
    let mut out = format!(
        "[Script Info]\nScriptType: v4.00+\nPlayResX: {width}\nPlayResY: {height}\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H64000000,0,0,0,0,100,100,0,0,1,1,0,7,0,0,0,1\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n"
    );
    // fastbrowse's colour, #C55230, as ASS writes it.
    const COLOUR: &str = "&H3052C5&";
    let unit = f64::from(width) / 1280.0;
    let seconds = |mark: &PointerMark| mark.at.saturating_duration_since(origin).as_secs_f64();
    let point = |mark: &PointerMark| (mark.x * f64::from(width) / mark.width, mark.y * f64::from(height) / mark.height);
    let until = until.saturating_duration_since(origin).as_secs_f64();
    // When each mark's glide begins: as long as deskpilot's for its length, but not before the mark before it landed.
    let starts: Vec<f64> = marks
        .iter()
        .enumerate()
        .map(|(i, mark)| match i.checked_sub(1).map(|previous| &marks[previous]) {
            Some(previous) => {
                let ((px, py), (x, y)) = (point(previous), point(mark));
                let distance = (x - px).hypot(y - py) / unit;
                let glide =
                    if distance < 2.0 { 0.0 } else { (1.5 * distance / PEAK_SPEED).clamp(MIN_GLIDE, MAX_GLIDE) };
                // A press before it keeps its whole squeeze: a glide that would cut it short is shortened instead.
                let squeezing = if previous.pressed { SQUEEZE_IN + SQUEEZE_HOLD + SQUEEZE_OUT } else { 0.0 };
                (seconds(mark) - glide).max(seconds(previous) + squeezing).min(seconds(mark))
            }
            None => seconds(mark),
        })
        .collect();
    // `\an7` puts a drawing's own origin at the position; `\an5` would put the middle of the extent from the origin
    // there, which moves a shape drawn about its origin by half its size.
    let mut dialogue = |layer: u8, start: f64, end: f64, tags: String, shape: String| {
        if end > start {
            out.push_str(&format!(
                "Dialogue: {layer},{},{},Default,,0,0,0,,{{{tags}\\an7\\p1}}{shape}{{\\p0}}\n",
                ass_time(start),
                ass_time(end)
            ));
        }
    };
    for (i, mark) in marks.iter().enumerate() {
        let at = seconds(mark);
        if at >= until {
            continue;
        }
        let (x, y) = point(mark);
        let start = starts[i];
        // The cursor stays where it landed until the next one starts to glide; the result card is no longer the page
        // that supplied these coordinates.
        let limit = starts.get(i + 1).copied().unwrap_or(until).min(until);
        let tags = format!("\\bord{:.2}\\shad0\\1c{COLOUR}\\3c{COLOUR}\\alpha&H00&", STROKE / 2.0 * unit);
        if let Some(previous) = i.checked_sub(1).map(|previous| &marks[previous]) {
            // The glide: a quadratic arc bowed to the side, eased in and out, in short straight pieces, the brackets
            // at rest size throughout.
            let (px, py) = point(previous);
            let (dx, dy) = (x - px, y - py);
            let length = dx.hypot(dy);
            if length >= 2.0 * unit && at > start {
                let bend = ((px + x) / 2.0 - dy / length * length * ARC, (py + y) / 2.0 + dx / length * length * ARC);
                let along = |t: f64| {
                    let e = ease_in_out(t);
                    let (a, b, c) = ((1.0 - e) * (1.0 - e), 2.0 * (1.0 - e) * e, e * e);
                    (px * a + bend.0 * b + x * c, py * a + bend.1 * b + y * c)
                };
                let pieces = ((at - start) / STEP).ceil().max(1.0) as usize;
                for k in 0..pieces {
                    let (t0, t1) = (
                        start + (at - start) * k as f64 / pieces as f64,
                        start + (at - start) * (k + 1) as f64 / pieces as f64,
                    );
                    let ((ax, ay), (bx, by)) = (along(k as f64 / pieces as f64), along((k + 1) as f64 / pieces as f64));
                    let mv = format!("\\move({ax:.1},{ay:.1},{bx:.1},{by:.1},0,{})", ((t1 - t0) * 1000.0).round());
                    dialogue(2, t0, t1, format!("{mv}{tags}"), cursor_shape(REST, 1.0, unit));
                }
            }
        }
        // At rest where it landed, until a press moves the brackets in: the half-width at each step from the press.
        let pos = format!("\\pos({x:.1},{y:.1}){tags}");
        let squeeze = if mark.pressed { SQUEEZE_IN + SQUEEZE_HOLD + SQUEEZE_OUT } else { 0.0 };
        let steps = (squeeze / SQUEEZE_STEP).round() as usize;
        for k in 0..steps {
            let elapsed = SQUEEZE_STEP * k as f64;
            let inward = if elapsed < SQUEEZE_IN {
                ease_out(elapsed / SQUEEZE_IN)
            } else {
                1.0 - ease_in_out((elapsed - SQUEEZE_IN - SQUEEZE_HOLD).max(0.0) / SQUEEZE_OUT)
            };
            let half = REST + (PRESS - REST) * inward;
            let (t0, t1) = (at + elapsed, (at + elapsed + SQUEEZE_STEP).min(limit));
            dialogue(2, t0, t1, pos.clone(), cursor_shape(half, PRESSED, unit));
        }
        dialogue(2, at + squeeze, limit, pos.clone(), cursor_shape(REST, 1.0, unit));
        if mark.pressed {
            let pieces = (PULSE / STEP).round() as usize;
            for k in 0..pieces {
                let t = STEP * k as f64 / PULSE;
                let radius = PULSE_SIZE * (0.3 + 0.7 * ease_out(t)) * unit;
                let tags =
                    format!("\\pos({x:.1},{y:.1})\\bord0\\shad0\\1c{COLOUR}\\alpha{}", ass_alpha(0.3 * (1.0 - t)));
                dialogue(1, at + STEP * k as f64, (at + STEP * (k + 1) as f64).min(until), tags, disc(radius));
            }
        }
    }
    out
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#x27;")
}

fn cut(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

/// `_CITATION_LINK.split`: a numbered answer link, `[3](<url>)`, matched as `\s*\[(\d+)\]\(<[^>]*>\)`, leaves the text
/// and the number in turn; the card shows the number and lists the quote, not the deep link.
fn split_citation_links(text: &str) -> Vec<String> {
    let mut parts = vec![];
    let mut rest = text;
    let mut kept = String::new();
    while !rest.is_empty() {
        let blank = rest.len() - rest.trim_start().len();
        let after = &rest[blank..];
        if let Some(inner) = after.strip_prefix('[') {
            let digits: String = inner.chars().take_while(char::is_ascii_digit).collect();
            if !digits.is_empty()
                && let Some(link) = inner[digits.len()..].strip_prefix("](<")
                && let Some(end) = link.find('>')
                && link[end + 1..].starts_with(')')
            {
                parts.push(std::mem::take(&mut kept));
                parts.push(digits);
                rest = &link[end + 2..];
                continue;
            }
        }
        let mut chars = rest.chars();
        kept.extend(chars.next());
        rest = chars.as_str();
    }
    parts.push(kept);
    parts
}

fn replace(source: &Path, path: &Path) -> std::io::Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let staged = path.with_file_name(format!(".{name}.{}", std::process::id()));
    // With the encoded file's mode, not the private one of a temporary file.
    let copied = std::fs::copy(source, &staged).and_then(|_| std::fs::rename(&staged, path));
    let _ = std::fs::remove_file(&staged);
    copied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn citation_links_split_into_text_and_numbers() {
        assert_eq!(split_citation_links("Paris [1](<https://a/#x>) and [2](<b>)."), ["Paris", "1", " and", "2", "."]);
        assert_eq!(split_citation_links("no links"), ["no links"]);
    }

    #[test]
    fn srt_times_round_half_to_even() {
        assert_eq!(srt_time(3723.012), "01:02:03,012");
        assert_eq!(srt_time(0.0), "00:00:00,000");
    }

    fn mark(origin: Instant, operation: Operation, pressed: bool, secs: u64, x: f64) -> PointerMark {
        PointerMark {
            at: origin + std::time::Duration::from_secs(secs),
            x,
            y: 10.0,
            operation,
            pressed,
            width: 1280.0,
            height: 720.0,
        }
    }

    fn lines(ass: &str, layer: u8) -> Vec<&str> {
        ass.lines().filter(|line| line.starts_with(&format!("Dialogue: {layer},"))).collect()
    }

    /// The half-width of the first bracket corner in a cursor shape: its path starts at the corner's horizontal arm.
    fn half_of(line: &str) -> f64 {
        let shape = line.split("\\p1}").nth(1).unwrap();
        shape.split_whitespace().nth(1).unwrap().parse::<f64>().unwrap().abs()
    }

    #[test]
    fn brackets_stay_at_rest_size_while_gliding_and_move_in_on_a_press_only() {
        let origin = Instant::now();
        let marks = [mark(origin, Operation::Click, true, 1, 100.0), mark(origin, Operation::Hover, false, 3, 600.0)];
        let ass = cursor_ass(&marks, origin, origin + std::time::Duration::from_secs(5), 1280, 720);
        let layer = lines(&ass, 2);
        // A glide is straight pieces of an arc ending at the landing, at the resting size and full opacity.
        let moves = layer.iter().filter(|line| line.contains("\\move(")).collect::<Vec<_>>();
        assert!(moves.len() > 5);
        assert!(moves.last().unwrap().contains(",600.0,"));
        assert!(layer.iter().all(|line| line.contains("\\alpha&H00&") && line.contains("\\3c&H3052C5&")));
        assert!(moves.iter().all(|line| (half_of(line) - 12.0).abs() < 4.0));
        // No bracket is ever wider than at rest.
        let rest = layer.iter().map(|line| half_of(line)).fold(0.0, f64::max);
        assert!(layer.iter().all(|line| half_of(line) <= rest + 1e-9));
        // The press moves them in to 7 and back to 15 in 10 ms steps from the press; the hover does not.
        let press = layer
            .iter()
            .filter(|line| line.starts_with("Dialogue: 2,0:00:01.") && line.contains("\\pos("))
            .collect::<Vec<_>>();
        let halves = press.iter().map(|line| half_of(line)).collect::<Vec<_>>();
        assert_eq!(halves.len(), 23);
        assert!(
            (halves[0] - 15.0).abs() < 0.01
                && halves[5..12].iter().all(|half| (half - 7.0).abs() < 0.01)
                && (halves[22] - 15.0).abs() < 0.5
        );
        assert!(halves[..5].windows(2).all(|pair| pair[1] < pair[0]));
        assert_eq!(
            layer.iter().filter(|line| line.starts_with("Dialogue: 2,0:00:03.") && line.contains("\\pos(")).count(),
            1
        );
    }

    #[test]
    fn a_press_leaves_a_fading_disc_where_it_was_pressed_and_a_hover_none() {
        let origin = Instant::now();
        let marks = [mark(origin, Operation::Fill, true, 1, 100.0), mark(origin, Operation::Hover, false, 3, 600.0)];
        let ass = cursor_ass(&marks, origin, origin + std::time::Duration::from_secs(5), 1280, 720);
        let discs = lines(&ass, 1);
        // 380 ms in 20 ms steps, all at the press, from 0.3 opacity to none.
        assert_eq!(discs.len(), 19);
        assert!(discs.iter().all(|line| line.contains("\\pos(100.0,10.0)") && line.contains("\\bord0")));
        assert!(discs[0].starts_with("Dialogue: 1,0:00:01.00,0:00:01.02") && discs[0].contains("\\alpha&HB3&"));
        assert!(discs[18].contains("\\alpha&HFB&"));
    }

    #[test]
    fn shapes_are_drawn_about_the_press_point() {
        let origin = Instant::now();
        let marks = [mark(origin, Operation::Fill, true, 1, 100.0)];
        let ass = cursor_ass(&marks, origin, origin + std::time::Duration::from_secs(5), 1280, 720);
        // Every event anchors its drawing's origin at the position, and the disc and the brackets are symmetric about it.
        for line in ass.lines().filter(|line| line.starts_with("Dialogue")) {
            assert!(line.contains("\\an7\\p1}"), "{line}");
        }
        for line in lines(&ass, 1).into_iter().chain(lines(&ass, 2)) {
            let shape = line.split("\\p1}").nth(1).unwrap().split("{\\p0}").next().unwrap();
            let numbers = shape.split_whitespace().filter_map(|word| word.parse::<f64>().ok());
            let (mut x, mut y, mut index) = ((f64::MAX, f64::MIN), (f64::MAX, f64::MIN), 0);
            for value in numbers {
                let axis = if index % 2 == 0 { &mut x } else { &mut y };
                *axis = (axis.0.min(value), axis.1.max(value));
                index += 1;
            }
            assert!((x.0 + x.1).abs() < 0.1 && (y.0 + y.1).abs() < 0.1, "{line}");
        }
    }

    #[test]
    fn a_following_glide_leaves_the_previous_press_its_whole_squeeze() {
        let origin = Instant::now();
        let mark = |ms, x, operation, pressed| PointerMark {
            at: origin + std::time::Duration::from_millis(ms),
            x,
            y: 100.0,
            operation,
            pressed,
            width: 1280.0,
            height: 720.0,
        };
        let marks = [mark(1000, 100.0, Operation::Fill, true), mark(1500, 600.0, Operation::Hover, false)];
        let ass = cursor_ass(&marks, origin, origin + std::time::Duration::from_secs(3), 1280, 720);
        let squeezed = ass
            .lines()
            .filter(|line| {
                line.starts_with("Dialogue: 2,") && line.contains("\\pos(100.0,100.0)") && line.contains("m -7.00 ")
            })
            .count();
        assert!(squeezed >= 5, "the squeeze was cut to {squeezed} events");
    }

    #[test]
    fn sizes_follow_the_frame_width() {
        let origin = Instant::now();
        let marks = [mark(origin, Operation::Click, true, 1, 100.0)];
        let wide = cursor_ass(&marks, origin, origin + std::time::Duration::from_secs(2), 2560, 1440);
        let narrow = cursor_ass(&marks, origin, origin + std::time::Duration::from_secs(2), 1280, 720);
        // The resting brackets' corner is 15 logical pixels out: 15 frame pixels at 1280 wide, 30 at 2560.
        assert!(narrow.contains("m -15.00 "));
        assert!(wide.contains("-30.00"));
    }

    #[test]
    fn encode_passes_run_one_encoder_each() {
        let base = "scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p";
        let steps = "subtitles=steps.srt:force_style='Fontsize=10'";
        let [captioned, plain] = encode_passes(Some(steps), &[]);
        assert_eq!(captioned, ("captioned.mp4", format!("{base},{steps}")));
        assert_eq!(plain, ("plain.mp4", base.to_owned()));

        // The cursor and a drawn result card are on both videos, over the captions; only one has the captions.
        let overlays = ["subtitles=cursor.ass".to_owned(), "subtitles=card.ass".to_owned()];
        let [captioned, plain] = encode_passes(Some(steps), &overlays);
        assert_eq!(captioned.1, format!("{base},{steps},subtitles=cursor.ass,subtitles=card.ass"));
        assert_eq!(plain.1, format!("{base},subtitles=cursor.ass,subtitles=card.ass"));

        let passes = encode_passes(None, &[]);
        assert!(passes.iter().all(|(_, filter)| filter == base));

        for (output, filter) in passes {
            let args = encode_args(&filter, output);
            assert_eq!(args.iter().filter(|arg| *arg == "-c:v").count(), 1);
            assert_eq!(args.iter().filter(|arg| arg.ends_with(".mp4")).collect::<Vec<_>>(), [output]);
            assert_eq!(args.last().map(String::as_str), Some(output));
            assert!(!args.iter().any(|arg| arg == "-filter_complex" || arg == "-map" || arg.contains("split")));
        }
    }

    #[test]
    fn a_drawn_card_covers_the_frame_with_the_answer_its_sources_and_outcome() {
        let card = DrawnCard {
            task: "write {x} and read it back".into(),
            result: ResultCard {
                status: "complete".into(),
                answer: Some("hello\\N [1](<desktop://org.gnome.TextEditor#e3>)".into()),
                error: None,
                citations: vec![CardCitation {
                    id: 1,
                    url: "desktop://org.gnome.TextEditor".into(),
                    quote: "hello".into(),
                }],
                steps: 3,
                known_dollars: 0.0123,
            },
            seconds: 12.34,
        };
        let ass = card_ass(&card, 10.0, 1280, 720);
        assert!(ass.contains("PlayResX: 1280\nPlayResY: 720"));
        // One opaque box over the whole frame from the card on, and the text above it.
        assert!(ass.contains("Dialogue: 0,0:00:10.00,1:00:10.00,Default,,0,0,0,,{\\an7\\pos(0,0)\\p1\\1c&H17110D&}m 0 0 l 1280 0 1280 720 0 720"));
        let text = ass.lines().find(|line| line.starts_with("Dialogue: 1,")).unwrap();
        // The page's braces and a typed backslash stay text, and the deep link becomes its number.
        assert!(text.contains("write \\{x} and read it back"), "{text}");
        assert!(text.contains("hello\\\u{2060}N"), "{text}");
        assert!(!text.contains("<desktop://"), "{text}");
        assert!(text.contains("\u{201c}hello\u{201d}"), "{text}");
        assert!(text.ends_with("complete in 12.3s, 3 steps, $0.0123"), "{text}");
    }

    #[test]
    fn steps_are_described_as_python_does() {
        use fastbrowse_core::models::StepOutcome;
        let step = StepResult {
            index: 0,
            operation: Operation::SwitchTab,
            decided_by: Decider::Jev,
            outcome: StepOutcome::Executed,
            failure: None,
            url: "https://example.test/".into(),
            target: Some("Docs".into()),
            confidence: None,
            note: None,
            facts: vec![],
            page_changed: None,
            duration_ms: 10,
            choices: Vec::new(),
        };
        assert_eq!(describe(&step), "switch tab Docs \u{b7} picked by Jev");
    }
}
