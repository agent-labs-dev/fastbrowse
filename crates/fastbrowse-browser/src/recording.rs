//! Record a run's active tab to a video: Chrome's screencast frames, resampled to a steady rate by ffmpeg.
//!
//! The screencast sends a frame only when the page repaints, so a ticker repeats the latest frame at `FPS`; the
//! video's timing is then the run's real timing. The recording follows tab switches, captions each step from when
//! its action began, and ends on a card showing the run's outcome, so one file shows both what the agent did and
//! what it answered. Two more ffmpeg passes over a lossless first one write it twice, one encoder at a time: with the
//! step captions burned in, and plain beside it as `<name>.plain.mp4`.
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

/// Writes the session's active tab to `path` (an .mp4) from `start` until `finish`.
pub struct Recording {
    session: BrowserSession,
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
        let tapped = (session.clone(), frames.clone());
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
            pointer_marks,
            card_at: None,
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

    /// End the video on the task and its outcome, in the tab being recorded.
    pub async fn show_result(&mut self, task: &str, result: &ResultCard) -> Result<(), crate::error::BrowserError> {
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
            "<div style='color:#3fb950'>{} in {:.1}s, {} steps, ${:.4}</div></body>",
            html_escape(&result.status),
            (card_at - self.started).as_secs_f64(),
            result.steps,
            result.known_dollars
        ));
        let url = format!("data:text/html;base64,{}", BASE64.encode(card.as_bytes()));
        let session_id = self.session.active_session_id()?;
        self.session.send("Page.navigate", json!({"url": url}), Some(&session_id)).await?;
        sleep(secs(RESULT_SECONDS)).await;
        Ok(())
    }

    /// stop the ticker, close the lossless pass and encode both videos.
    pub async fn finish(mut self) -> Vec<PathBuf> {
        if let Some(ticker) = self.ticker.take() {
            ticker.abort();
            let _ = ticker.await;
        }
        self.session.set_frame_tap(None);
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
            warn(&format!("the browser sent no frames, so nothing was recorded to {}", self.path.display()));
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

    /// One screenshot of the active tab, or None while frames are withheld or when the capture fails.
    async fn still(&self) -> Option<Vec<u8>> {
        if self.session.frames_withheld() {
            return None;
        }
        let shot = timeout(secs(STILL_SECONDS), async {
            // A caller who revoked access after the last step gets no screenshot taken at teardown either.
            self.session.assert_clear().await?;
            let session_id = self.session.active_session_id()?;
            self.session
                .send("Page.captureScreenshot", json!({"format": "jpeg", "quality": 95}), Some(&session_id))
                .await
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
        (!self.session.frames_withheld()).then_some(frame)
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
        let cursor = self.scratch.join("cursor.ass");
        let cursor_present = if marks.is_empty() {
            false
        } else if let Some((width, height)) = self.frame_dimensions().await {
            let ass = cursor_ass(&marks, origin, self.card_at.unwrap_or_else(Instant::now), width, height);
            match tokio::fs::write(&cursor, ass).await {
                Ok(()) => true,
                Err(error) => {
                    warn(&format!("could not write the recording cursor: {error}"));
                    false
                }
            }
        } else {
            warn("could not read the recording dimensions, so the cursor is omitted");
            false
        };
        // BorderStyle 3 draws a box behind the text, coloured by OutlineColour (alpha first, 00 opaque).
        let style = "Fontsize=10,BorderStyle=3,Outline=6,Shadow=0,OutlineColour=&H50000000,MarginV=16,Alignment=2";
        // libass cannot open an empty subtitle file. The filter names it relative to ffmpeg's working directory: an
        // absolute path's drive colon and backslashes are filter syntax, and a Windows run failed on them.
        let mut error = self.encode(&filter_chain(!cues.is_empty(), cursor_present, style)).await;
        if let Some(failure) = &error
            && (!cues.is_empty() || cursor_present)
        {
            // An ffmpeg built without libass has no subtitles filter; the plain video is still worth keeping.
            warn(&format!(
                "ffmpeg could not caption {}, so it is written without captions: {failure}",
                self.path.display()
            ));
            error = self.encode("null").await;
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

    /// Write both videos into scratch through `burn` on the captioned one; return ffmpeg's error, if any. Each video is
    /// its own ffmpeg pass over the lossless one, so a recording holds one x264 encoder's memory at a time; the plain
    /// pass does not start when the captioned one failed.
    async fn encode(&self, burn: &str) -> Option<String> {
        for (output, filter) in encode_passes(burn) {
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

/// The two final videos and the filter each is encoded through: the captioned one burns `burn` in, and the plain one
/// carries only the cursor, when `burn` draws one.
fn encode_passes(burn: &str) -> [(&'static str, String); 2] {
    // H.264 needs even dimensions, and yuv420p is what phones and social sites play.
    let base = "scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p";
    let plain = if burn.contains("cursor.ass") { format!("{base},subtitles=cursor.ass") } else { base.to_owned() };
    [("captioned.mp4", format!("{base},{burn}")), ("plain.mp4", plain)]
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

async fn tick(session: BrowserSession, frames: Arc<Mutex<Frames>>, stdin: Arc<tokio::sync::Mutex<Option<ChildStdin>>>) {
    let mut written: u32 = 0;
    loop {
        let Ok(active) = session.active_session_id() else { return };
        let recast = {
            let frames = frames.lock().unwrap();
            !session.has_frame_handler()
                && (frames.casting.as_deref() != Some(active.as_str())
                    || frames.frame_at.is_none_or(|at| at.elapsed() > secs(RECAST_SECONDS)))
        };
        if recast {
            cast(&session, &frames, &active).await;
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

fn filter_chain(captions: bool, cursor: bool, style: &str) -> String {
    let mut filters = Vec::new();
    if captions {
        filters.push(format!("subtitles=steps.srt:force_style='{style}'"));
    }
    if cursor {
        filters.push("subtitles=cursor.ass".to_owned());
    }
    if filters.is_empty() { "null".to_owned() } else { filters.join(",") }
}

/// The cursor as an ASS overlay: an arrow that glides into each action point in the 400 ms before the action, stays
/// there until the next one, and a ring that swells and fades on each click. Points are CSS pixels, scaled to the frame.
fn cursor_ass(marks: &[PointerMark], origin: Instant, until: Instant, width: u32, height: u32) -> String {
    let mut out = format!(
        "[Script Info]\nScriptType: v4.00+\nPlayResX: {width}\nPlayResY: {height}\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H64000000,0,0,0,0,100,100,0,0,1,1,0,7,0,0,0,1\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n"
    );
    // The marker is centered on the action point and stays upright as it travels.
    let size = (f64::from(width) / 1280.0 * 140.0).round().max(100.0);
    let seconds = |mark: &PointerMark| mark.at.saturating_duration_since(origin).as_secs_f64();
    let point = |mark: &PointerMark| {
        ((mark.x * f64::from(width) / mark.width).round(), (mark.y * f64::from(height) / mark.height).round())
    };
    let until = until.saturating_duration_since(origin).as_secs_f64();
    for (i, mark) in marks.iter().enumerate() {
        if seconds(mark) >= until {
            continue;
        }
        let at = seconds(mark);
        let (x, y) = point(mark);
        let (start, placed) = match i.checked_sub(1).map(|previous| &marks[previous]) {
            Some(previous) => {
                let (px, py) = point(previous);
                let start = (at - 0.4).max(seconds(previous));
                let glide = ((at - start) * 1000.0).round();
                (start, format!("\\move({px},{py},{x},{y},0,{glide})"))
            }
            None => (at, format!("\\pos({x},{y})")),
        };
        // The result card is no longer the page that supplied these coordinates.
        let end = marks.get(i + 1).map_or(until, |next| (seconds(next) - 0.4).max(at)).min(until);
        out.push_str(&format!(
            "Dialogue: 2,{},{},Default,,0,0,0,,{{{placed}\\an5\\fscx{size}\\fscy{size}\\p1\\bord1\\shad0\\1c&H3052C5&\\3c&HFFFFFF&}}m -11 -5 l -11 -11 -5 -11 -5 -9 -9 -9 -9 -5 m 5 -11 l 11 -11 11 -5 9 -5 9 -9 5 -9 m 11 5 l 11 11 5 11 5 9 9 9 9 5 m -5 11 l -11 11 -11 5 -9 5 -9 9 -5 9 m -1 -1 l 1 -1 1 1 -1 1{{\\p0}}\n",
            ass_time(start),
            ass_time(end)
        ));
        if mark.operation == Operation::Click {
            out.push_str(&format!(
                "Dialogue: 1,{},{},Default,,0,0,0,,{{\\pos({x},{y})\\an5\\p1\\bord3\\1a&HFF&\\3c&H3052C5&\\fscx100\\fscy100\\t(0,400,\\fscx400\\fscy400\\3a&HFF&)}}m -6 0 b -6 -3 -3 -6 0 -6 b 3 -6 6 -3 6 0 b 6 3 3 6 0 6 b -3 6 -6 3 -6 0{{\\p0}}\n",
                ass_time(at),
                ass_time((at + 0.4).min(until))
            ));
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

    #[test]
    fn cursor_ass_scales_and_times_marks() {
        let origin = Instant::now();
        let marks = [
            PointerMark {
                at: origin + std::time::Duration::from_secs(1),
                x: 50.0,
                y: 25.0,
                operation: Operation::Click,
                width: 100.0,
                height: 50.0,
            },
            PointerMark {
                at: origin + std::time::Duration::from_secs(2),
                x: 25.0,
                y: 25.0,
                operation: Operation::Hover,
                width: 50.0,
                height: 50.0,
            },
        ];
        let ass = cursor_ass(&marks, origin, origin + std::time::Duration::from_secs(3), 200, 100);
        assert!(ass.contains("PlayResX: 200\nPlayResY: 100"));
        assert!(ass.contains("\\pos(100,50)"));
        assert!(ass.contains("\\move(100,50,100,50,0,400)"));
        // The arrow stays where it clicked until the glide to the next point begins, and the last one stays put.
        assert!(ass.contains("Dialogue: 2,0:00:01.00,0:00:01.60"));
        assert!(ass.contains("Dialogue: 2,0:00:01.60,0:00:03.00"));
        // Only the click pulses.
        assert!(ass.contains("Dialogue: 1,0:00:01.00,0:00:01.40"));
        assert_eq!(ass.matches("Dialogue: 1,").count(), 1);
    }

    #[test]
    fn empty_cursor_leaves_existing_filter_graph_choice_unchanged() {
        assert_eq!(filter_chain(false, false, ""), "null");
        assert_eq!(filter_chain(true, false, "Fontsize=10"), "subtitles=steps.srt:force_style='Fontsize=10'");
        assert_eq!(filter_chain(false, true, ""), "subtitles=cursor.ass");
    }

    #[test]
    fn encode_passes_run_one_encoder_each() {
        let base = "scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p";
        let steps = "subtitles=steps.srt:force_style='Fontsize=10'";
        let [captioned, plain] = encode_passes(&filter_chain(true, false, "Fontsize=10"));
        assert_eq!(captioned, ("captioned.mp4", format!("{base},{steps}")));
        assert_eq!(plain, ("plain.mp4", base.to_owned()));

        let [captioned, plain] = encode_passes(&filter_chain(true, true, "Fontsize=10"));
        assert_eq!(captioned.1, format!("{base},{steps},subtitles=cursor.ass"));
        assert_eq!(plain.1, format!("{base},subtitles=cursor.ass"));

        let passes = encode_passes("null");
        assert!(passes.iter().all(|(_, filter)| !filter.contains("subtitles")));

        for (output, filter) in passes {
            let args = encode_args(&filter, output);
            assert_eq!(args.iter().filter(|arg| *arg == "-c:v").count(), 1);
            assert_eq!(args.iter().filter(|arg| arg.ends_with(".mp4")).collect::<Vec<_>>(), [output]);
            assert_eq!(args.last().map(String::as_str), Some(output));
            assert!(!args.iter().any(|arg| arg == "-filter_complex" || arg == "-map" || arg.contains("split")));
        }
    }

    #[test]
    fn steps_are_described_as_python_does() {
        use fastbrowse_core::models::StepOutcome;
        let step = StepResult {
            index: 0,
            operation: Operation::SwitchTab,
            decided_by: Decider::Jev,
            outcome: StepOutcome::Executed,
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
