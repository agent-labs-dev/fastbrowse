//! A PDF's text, for a run to read. Chrome draws a PDF in its own viewer, whose text never reaches the page's DOM, so
//! its bytes are read here instead, in a helper process ([`crate::helper`]) that is killed when its time is up.

use std::process::{Child, Command, Stdio};
use std::sync::Once;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::helper::{self, HelperCommand, Reply};

/// A parse that outlasts this is given up on: the run waits for no file longer than it waits for a page to load.
pub(crate) const PARSE_SECONDS: u64 = 10;

/// Why a PDF that was kept has no text in the capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfUnread {
    /// No helper is registered in this program, so nothing can read a PDF where it can be stopped.
    NoReader,
    /// Reading it took longer than [`PARSE_SECONDS`], and the reader was stopped.
    TimedOut,
    /// The reader answered that the bytes are no PDF it can parse.
    Unparsable,
    /// The decompressed document or complete output exceeds max_download_bytes.
    TooLarge,
    /// The reader did not start, ended without an answer, or is from another fastbrowse version. This says nothing
    /// about the file: an older `fastbrowse` binary, or one a package upgrade deleted under a long-running server,
    /// fails here on any PDF.
    ReaderFailed,
}

/// The text a PDF's pages hold, in page order, or None when it holds none: a scanned PDF is an image of text.
pub(crate) async fn text(raw: Vec<u8>) -> Result<Option<String>, PdfUnread> {
    document_text(helper::PDF_TEXT, raw).await
}

pub(crate) async fn document_text(request: &str, raw: Vec<u8>) -> Result<Option<String>, PdfUnread> {
    let Some(command) = helper::registered() else {
        static SAID: Once = Once::new();
        SAID.call_once(|| {
            crate::warn(
                "Document text is not read: no document helper process is registered. The fastbrowse binary registers itself, \
                 and the Python package its interpreter when Python's own command line started it. Under the Node SDK, \
                 set FASTBROWSE_BINARY to a fastbrowse binary of this version; a Rust program calls \
                 fastbrowse::helper::run_if_requested() first in main",
            )
        });
        return Err(PdfUnread::NoReader);
    };
    text_with_request(command, request, raw, Duration::from_secs(PARSE_SECONDS)).await
}

#[cfg(test)]
pub(crate) async fn text_with(
    command: &HelperCommand,
    raw: Vec<u8>,
    limit: Duration,
) -> Result<Option<String>, PdfUnread> {
    text_with_request(command, helper::PDF_TEXT, raw, limit).await
}

async fn text_with_request(
    command: &HelperCommand,
    request: &str,
    raw: Vec<u8>,
    limit: Duration,
) -> Result<Option<String>, PdfUnread> {
    let mut child = Owned::spawn(command, request)?;
    let stdin = child.0.stdin.take().map(tokio::process::ChildStdin::from_std);
    let stdout = child.0.stdout.take().map(tokio::process::ChildStdout::from_std);
    let (Some(Ok(mut stdin)), Some(Ok(mut stdout))) = (stdin, stdout) else { return Err(PdfUnread::ReaderFailed) };
    // Written and read at once: a helper that writes before it has read all of a large PDF would otherwise fill one
    // pipe while this side waits on the other.
    let write = async move {
        // A helper that died early closes its stdin; what it replied, or did not, says what happened.
        let _ = stdin.write_all(&raw).await;
        // Dropping stdin here is the end of the PDF the helper reads to.
    };
    let mut out = Vec::new();
    let read = stdout.read_to_end(&mut out);
    let exchanged = tokio::time::timeout(limit, async { tokio::join!(write, read).1 }).await;
    // Killed and reaped whatever the reply was, before this returns.
    drop(child);
    match exchanged {
        Err(_) => Err(PdfUnread::TimedOut),
        Ok(Err(_)) => Err(PdfUnread::ReaderFailed),
        Ok(Ok(_)) => match helper::parse_reply(&out) {
            Some((Reply::Text, body)) => {
                let text = String::from_utf8_lossy(body);
                let text = text.trim();
                Ok((!text.is_empty()).then(|| text.to_owned()))
            }
            Some((Reply::Failed, _)) => Err(PdfUnread::Unparsable),
            Some((Reply::TooLarge, _)) => Err(PdfUnread::TooLarge),
            Some((Reply::Unsupported, _)) => {
                crate::warn(
                    "Document text is not read: the document helper process is a fastbrowse of another version. Point it at a \
                     fastbrowse binary of this version",
                );
                Err(PdfUnread::ReaderFailed)
            }
            // A parser that crashed on this file and a helper that is no fastbrowse at all look the same from here.
            None => Err(PdfUnread::ReaderFailed),
        },
    }
}

/// A helper this process started, killed and reaped when dropped: on a reply, a timeout, or the run's future being
/// dropped mid-parse, as a cancelled run or a runtime shut down drops it. Tokio's own `kill_on_drop` leaves the reaping
/// to a later signal on a runtime that may be gone, so the wait is here. It blocks only on a child already sent a
/// kill, which ends it at once.
///
/// Only the child is killed, not a process tree, so every host registers a program that is the helper itself and
/// never a launcher that starts it: the Python package registers its interpreter's own executable, since a Windows
/// venv's `python.exe` starts the real interpreter as a child of its own.
struct Owned(Child);

impl Owned {
    fn spawn(command: &HelperCommand, request: &str) -> Result<Owned, PdfUnread> {
        let mut spawn = Command::new(&command.program);
        spawn
            .args(&command.args)
            .args([helper::MARKER, request, helper::PROTOCOL])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW: a console flashing up for every PDF read by a windowed host.
            spawn.creation_flags(0x0800_0000);
        }
        spawn.spawn().map(Owned).map_err(|_| {
            crate::warn(
                "Document text is not read: the document helper process could not start. Check that the program it was \
                 registered with still exists",
            );
            PdfUnread::ReaderFailed
        })
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Instant;

    use super::*;

    const WIND: &[u8] = include_bytes!("../tests/fixtures/wind.pdf");

    #[tokio::test]
    async fn a_document_uses_the_same_killable_helper_as_a_pdf() {
        use std::io::{Cursor, Write};
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("report.csv", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(b"Item,Amount\nKettle,34.50").unwrap();
        let raw = zip.finish().unwrap().into_inner();
        let text = text_with_request(&helper("read"), helper::ZIP_TEXT, raw.clone(), Duration::from_secs(30))
            .await
            .unwrap()
            .unwrap();
        assert!(text.contains("[Archive member: report.csv]\nItem,Amount\nKettle,34.50"));
        assert_eq!(
            text_with_request(&helper("read"), "zip-text:1", raw, Duration::from_secs(30)).await,
            Err(PdfUnread::TooLarge)
        );
    }

    /// This test binary run as a helper, by [`helper_child`]. A mode just before the marker makes it misbehave.
    fn helper(mode: &str) -> HelperCommand {
        let mut command = crate::testing::test_binary_helper("pdf::tests::helper_child");
        command.args.push(mode.into());
        command
    }

    /// Not a test: the helper, when the binary is run as one. libtest takes the arguments after the test's name as more
    /// names to match exactly, which none does.
    #[test]
    fn helper_child() {
        let args: Vec<String> = std::env::args().collect();
        let Some(at) = args.iter().position(|arg| arg == helper::MARKER) else { return };
        let mode = args[..at].last().map(String::as_str).unwrap_or_default();
        if let Some(pid_file) = mode.strip_prefix("hang=") {
            // Written whole under another name first, so a reader never sees half a number.
            let partial = format!("{pid_file}.partial");
            std::fs::write(&partial, std::process::id().to_string()).unwrap();
            std::fs::rename(&partial, pid_file).unwrap();
        }
        match mode.split('=').next().unwrap_or_default() {
            "hang" => std::thread::sleep(Duration::from_secs(600)),
            // What `panic = "abort"` does to a parser panic in the shipped binary.
            "abort" => {
                // A core file of the whole test binary for each run of this test is litter, not evidence.
                #[cfg(target_os = "linux")]
                unsafe {
                    libc::prctl(libc::PR_SET_DUMPABLE, 0);
                }
                std::process::abort()
            }
            "panic" => panic!("a parser panic"),
            // What an older fastbrowse does with arguments it does not know: complain and exit, with no reply.
            "silent" => std::process::exit(2),
            // A helper of another version is asked a request it does not know.
            "skewed" => {
                let request = [helper::MARKER.to_owned(), helper::PDF_TEXT.to_owned(), "0".to_owned()];
                std::process::exit(helper::serve(&request).unwrap_or(1))
            }
            _ => {}
        }
        std::process::exit(helper::serve(&args[at..]).unwrap_or(1));
    }

    #[cfg(unix)]
    fn alive(pid: i32) -> bool {
        // kill(pid, 0) succeeds for any process still in the table, a zombie included: only a reaped child is gone.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[tokio::test]
    async fn a_pdf_is_read_in_the_helper() {
        let text = text_with(&helper("read"), WIND.to_vec(), Duration::from_secs(30)).await;
        assert_eq!(text, Ok(Some("Fairbanks Wind".to_owned())));
    }

    #[tokio::test]
    async fn bytes_that_are_no_pdf_fail_and_a_pdf_without_text_has_none() {
        let limit = Duration::from_secs(30);
        assert_eq!(text_with(&helper("read"), b"not a pdf".to_vec(), limit).await, Err(PdfUnread::Unparsable));
        let truncated = WIND[..WIND.len() / 2].to_vec();
        assert_eq!(text_with(&helper("read"), truncated, limit).await, Err(PdfUnread::Unparsable));
        let blank = include_bytes!("../tests/fixtures/blank.pdf").to_vec();
        assert_eq!(text_with(&helper("read"), blank, limit).await, Ok(None));
    }

    #[tokio::test]
    async fn a_helper_that_aborts_or_panics_is_a_failed_reader_not_a_bad_pdf() {
        let limit = Duration::from_secs(30);
        assert_eq!(text_with(&helper("abort"), WIND.to_vec(), limit).await, Err(PdfUnread::ReaderFailed));
        assert_eq!(text_with(&helper("panic"), WIND.to_vec(), limit).await, Err(PdfUnread::ReaderFailed));
    }

    #[tokio::test]
    async fn a_hung_helper_is_killed_at_the_limit() {
        let started = Instant::now();
        let text = text_with(&helper("hang"), WIND.to_vec(), Duration::from_millis(300)).await;
        assert_eq!(text, Err(PdfUnread::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn a_large_input_unread_by_a_hung_helper_does_not_block_past_the_limit() {
        // Far more than a pipe holds: the write blocks until the kill closes the pipe.
        let started = Instant::now();
        let big = vec![b'%'; 64 << 20];
        assert_eq!(text_with(&helper("hang"), big, Duration::from_millis(300)).await, Err(PdfUnread::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn a_helper_that_is_not_there_is_a_failed_reader_not_a_bad_pdf() {
        let missing = HelperCommand::new("/nonexistent/fastbrowse-helper");
        assert_eq!(text_with(&missing, WIND.to_vec(), Duration::from_secs(5)).await, Err(PdfUnread::ReaderFailed));
    }

    /// A `fastbrowse` from before the helper existed refuses the marker as an unknown argument and replies nothing.
    #[tokio::test]
    async fn a_program_that_is_no_helper_is_a_failed_reader_not_a_bad_pdf() {
        let limit = Duration::from_secs(30);
        assert_eq!(text_with(&helper("silent"), WIND.to_vec(), limit).await, Err(PdfUnread::ReaderFailed));
    }

    #[tokio::test]
    async fn a_helper_from_another_version_is_a_failed_reader_not_a_bad_pdf() {
        let limit = Duration::from_secs(30);
        assert_eq!(text_with(&helper("skewed"), WIND.to_vec(), limit).await, Err(PdfUnread::ReaderFailed));
    }

    /// The issue's probe: a parse that outlives its timeout held the runtime's drop until it finished.
    #[test]
    fn dropping_the_runtime_after_a_timeout_does_not_wait_for_the_parse() {
        let started = Instant::now();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let text = runtime.block_on(text_with(&helper("hang"), WIND.to_vec(), Duration::from_millis(20)));
        assert_eq!(text, Err(PdfUnread::TimedOut));
        drop(runtime);
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

    /// A run cancelled mid-parse, and a runtime dropped with the parse still pending, drop the future: the helper is
    /// killed and reaped then, not left to a signal handler on a runtime that is gone.
    #[cfg(unix)]
    #[test]
    fn a_parse_dropped_mid_flight_kills_and_reaps_its_helper() {
        for drop_runtime in [false, true] {
            let pid_file =
                std::env::temp_dir().join(format!("fastbrowse-helper-{}-{drop_runtime}", std::process::id()));
            let _ = std::fs::remove_file(&pid_file);
            let mode = format!("hang={}", pid_file.display());
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let task =
                runtime.spawn(async move { text_with(&helper(&mode), WIND.to_vec(), Duration::from_secs(600)).await });
            let pid: i32 = runtime.block_on(async {
                loop {
                    if let Some(pid) = std::fs::read_to_string(&pid_file).ok().and_then(|pid| pid.parse().ok()) {
                        return pid;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            });
            std::fs::remove_file(&pid_file).unwrap();
            assert!(alive(pid));
            let started = Instant::now();
            if drop_runtime {
                drop(runtime);
            } else {
                task.abort();
                runtime.block_on(async { assert!(task.await.unwrap_err().is_cancelled()) });
            }
            assert!(!alive(pid), "helper {pid} is still in the process table");
            assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        }
    }
}
