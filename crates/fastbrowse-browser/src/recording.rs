//! Record a run's active tab to a video: Chrome's screencast frames, resampled to a steady rate by ffmpeg.
//!
//! The screencast sends a frame only when the page repaints, so a ticker repeats the latest frame at `FPS`; the
//! video's timing is then the run's real timing. The recording follows tab switches, captions each step from when
//! its action began, and ends on a card showing the run's outcome, so one file shows both what the agent did and
//! what it answered. A second ffmpeg pass over a lossless first one writes it twice: with the step captions burned
//! in, and plain beside it as `<name>.plain.mp4`.
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
    card_at: Option<Instant>,
    /// The finished videos, set only once both are encoded: a failed run must not report a file it did not write.
    pub outputs: Vec<PathBuf>,
}

impl Recording {
    /// Start ffmpeg and the cast, and begin writing frames.
    pub async fn start(session: BrowserSession, path: &Path) -> Result<Self, RecordingError> {
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

    /// Stop the ticker, close the lossless pass and encode both videos.
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

    /// Encode the final video twice from the lossless pass: with step captions, and plain.
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
        // BorderStyle 3 draws a box behind the text, coloured by OutlineColour (alpha first, 00 opaque).
        let style = "Fontsize=10,BorderStyle=3,Outline=6,Shadow=0,OutlineColour=&H50000000,MarginV=16,Alignment=2";
        // libass cannot open an empty subtitle file. The filter names it relative to ffmpeg's working directory: an
        // absolute path's drive colon and backslashes are filter syntax, and a Windows run failed on them.
        let burn =
            if cues.is_empty() { "null".to_owned() } else { format!("subtitles=steps.srt:force_style='{style}'") };
        let mut error = self.encode(&burn).await;
        if let Some(failure) = &error
            && !cues.is_empty()
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

    /// Write both videos into scratch through `burn` on the captioned one; return ffmpeg's error, if any.
    async fn encode(&self, burn: &str) -> Option<String> {
        // H.264 needs even dimensions, and yuv420p is what phones and social sites play.
        let graph = format!(
            "[0:v]scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p,split=2[plain][steps];[steps]{burn}[captioned]"
        );
        // Page text is the subject of a shared clip; x264's default quality blurs small type.
        let encode = ["-c:v", "libx264", "-crf", "18", "-preset", "slow", "-movflags", "+faststart"];
        let output = Command::new(&self.ffmpeg_path)
            .args(["-loglevel", "error", "-y", "-i"])
            .arg(self.scratch.join("uncaptioned.mkv"))
            .args(["-filter_complex", &graph, "-map", "[captioned]"])
            .args(encode)
            .args(["captioned.mp4", "-map", "[plain]"])
            .args(encode)
            .arg("plain.mp4")
            .current_dir(&self.scratch)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await;
        match output {
            Ok(done) if done.status.success() => None,
            Ok(done) => {
                let error = String::from_utf8_lossy(&done.stderr).trim().to_owned();
                Some(if error.is_empty() {
                    format!("exit status {}", done.status.code().unwrap_or(-1))
                } else {
                    cut(&error, 400)
                })
            }
            Err(error) => Some(error.kind().to_string()),
        }
    }
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
    };
    match step.operation {
        Operation::Read => format!("read the page \u{b7} {by}"),
        Operation::Done => "check the answer".into(),
        operation => format!("{}{target} \u{b7} {by}", operation.as_str().replace('_', " ")),
    }
}

fn srt_time(seconds: f64) -> String {
    let millis = (seconds * 1000.0).round_ties_even() as u64;
    format!("{:02}:{:02}:{:02},{:03}", millis / 3_600_000, millis / 60_000 % 60, millis / 1000 % 60, millis % 1000)
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
        let paged =
            StepResult { operation: Operation::Click, decided_by: Decider::Code, target: Some("Next".into()), ..step };
        assert_eq!(describe(&paged), "click Next \u{b7} picked by code");
    }
}
