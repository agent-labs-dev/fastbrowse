//! Work that has to be killable runs in a helper process: a PDF's text is read there.
//!
//! A parser running on a thread of this process cannot be stopped. A timeout only stops waiting for it, the runtime's
//! shutdown then waits for it to finish, and with `panic = "abort"` a parser panic ends the whole program. A child
//! process can be killed when its time is up or its run stops, and a panic ends only the child.
//!
//! The helper is this program run again with [`MARKER`] as its first argument, so every host has to say how to run
//! itself: the `fastbrowse` binary registers its own executable, the Python module its interpreter, the Node addon the
//! binary its platform package carries, and a Rust program calls [`run_if_requested`] first thing in `main`. Nothing
//! registered means no PDF text, and the run is told so: a PDF parsed inside the process could outlive the run.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::sync::OnceLock;

/// The first argument that turns a run of this program into a helper.
pub const MARKER: &str = "__fastbrowse-helper";
/// The request the helper serves: a PDF's bytes on stdin, its text in the reply.
pub(crate) const PDF_TEXT: &str = "pdf-text";
pub(crate) const XLSX_TEXT: &str = "xlsx-text";
pub(crate) const DOCX_TEXT: &str = "docx-text";
pub(crate) const ZIP_TEXT: &str = "zip-text";
/// The version of the request and reply. A helper that does not understand it answers [`Reply::Unsupported`]
/// instead of guessing at a request it does not know.
pub(crate) const PROTOCOL: &str = "1";
/// What starts the reply on stdout. A host can print before the helper writes (libtest prints its header), so the
/// reply is found by this rather than read from the first byte.
pub(crate) const MAGIC: &[u8] = b"\0fastbrowse-helper-reply\0";

/// What the helper says it did, the byte after [`MAGIC`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reply {
    /// The reply's body is the PDF's text, empty for a PDF that holds none.
    Text = 0,
    /// The bytes are no PDF this parser can read.
    Failed = 1,
    /// The request is not one this helper knows: it is from another fastbrowse version.
    Unsupported = 2,
}

impl Reply {
    pub(crate) fn from_byte(byte: u8) -> Option<Reply> {
        [Reply::Text, Reply::Failed, Reply::Unsupported].into_iter().find(|reply| *reply as u8 == byte)
    }
}

/// The program and leading arguments that start a helper. [`MARKER`] and the request follow them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl HelperCommand {
    pub fn new(program: impl Into<OsString>) -> HelperCommand {
        HelperCommand { program: program.into(), args: Vec::new() }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> HelperCommand {
        self.args.push(arg.into());
        self
    }
}

static HELPER: OnceLock<HelperCommand> = OnceLock::new();

/// Use `command` to start helpers in this process. The first registration stands, and this returns false for a later
/// one: a host registers once, as it loads, and nothing may swap the helper under a run already reading a PDF.
pub fn register(command: HelperCommand) -> bool {
    HELPER.set(command).is_ok()
}

/// The registered helper, if any host registered one.
pub fn registered() -> Option<&'static HelperCommand> {
    HELPER.get()
}

/// For a Rust program that embeds fastbrowse: the first line of `main`. When this run of the program is a helper, it
/// serves the request and exits; otherwise it registers this executable as the helper and returns.
///
/// Without it, a run still keeps every PDF it meets as a file, but reads none of their text and says so.
pub fn run_if_requested() {
    let args: Vec<String> = std::env::args_os().skip(1).map(|arg| arg.to_string_lossy().into_owned()).collect();
    if let Some(code) = serve(&args) {
        std::process::exit(code);
    }
    if let Ok(exe) = std::env::current_exe() {
        register(HelperCommand::new(exe));
    }
}

/// When `args`, the arguments after the program, ask for a helper: serve the request on stdin and stdout and return
/// the exit code. Called before argument parsing or logging: the helper needs no asynchronous work, and writes
/// nothing to stdout but its reply.
pub fn serve(args: &[String]) -> Option<i32> {
    if args.first().map(String::as_str) != Some(MARKER) {
        return None;
    }
    let (reply, body) = match &args[1..] {
        [request, protocol] if request == PDF_TEXT && protocol == PROTOCOL => pdf_text(),
        [request, protocol] if [XLSX_TEXT, DOCX_TEXT, ZIP_TEXT].contains(&request.as_str()) && protocol == PROTOCOL => {
            let mut raw = Vec::new();
            match std::io::stdin().read_to_end(&mut raw) {
                Ok(_) => match crate::documents::parse(request, &raw) {
                    Ok(text) => (Reply::Text, text.into_bytes()),
                    Err(_) => (Reply::Failed, Vec::new()),
                },
                Err(_) => (Reply::Failed, Vec::new()),
            }
        }
        _ => (Reply::Unsupported, Vec::new()),
    };
    let mut out = std::io::stdout().lock();
    let written = out
        .write_all(MAGIC)
        .and_then(|()| out.write_all(&[reply as u8]))
        .and_then(|()| out.write_all(&(body.len() as u64).to_le_bytes()))
        .and_then(|()| out.write_all(&body))
        .and_then(|()| out.flush());
    // No catch_unwind: a parser panic aborts or unwinds this process alone, and the parent reads no reply.
    Some(if written.is_err() { 1 } else { reply as i32 })
}

fn pdf_text() -> (Reply, Vec<u8>) {
    let mut raw = Vec::new();
    if std::io::stdin().read_to_end(&mut raw).is_err() {
        return (Reply::Failed, Vec::new());
    }
    match pdf_extract::extract_text_from_mem(&raw) {
        Ok(text) => (Reply::Text, text.into_bytes()),
        Err(_) => (Reply::Failed, Vec::new()),
    }
}

/// The reply in a helper's stdout: what it did and the body, or None when it wrote no whole reply because it died
/// first.
pub(crate) fn parse_reply(stdout: &[u8]) -> Option<(Reply, &[u8])> {
    let start = stdout.windows(MAGIC.len()).position(|window| window == MAGIC)? + MAGIC.len();
    let rest = stdout.get(start..)?;
    let (&kind, rest) = rest.split_first()?;
    let length = u64::from_le_bytes(rest.get(..8)?.try_into().ok()?);
    let body = rest.get(8..)?.get(..usize::try_from(length).ok()?)?;
    Some((Reply::from_byte(kind)?, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framed(reply: Reply, body: &[u8]) -> Vec<u8> {
        let mut out = b"\nrunning 1 test\n".to_vec();
        out.extend_from_slice(MAGIC);
        out.push(reply as u8);
        out.extend_from_slice(&(body.len() as u64).to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn a_reply_is_found_after_whatever_the_host_printed() {
        assert_eq!(parse_reply(&framed(Reply::Text, b"Fairbanks Wind")), Some((Reply::Text, &b"Fairbanks Wind"[..])));
        assert_eq!(parse_reply(&framed(Reply::Failed, b"")), Some((Reply::Failed, &b""[..])));
    }

    #[test]
    fn a_reply_cut_short_by_a_dying_helper_is_no_reply() {
        let whole = framed(Reply::Text, b"Fairbanks Wind");
        assert_eq!(parse_reply(&whole[..whole.len() - 1]), None);
        assert_eq!(parse_reply(b"\nrunning 1 test\n"), None);
        let mut unknown = framed(Reply::Text, b"");
        let kind = unknown.len() - 9;
        unknown[kind] = 9;
        assert_eq!(parse_reply(&unknown), None);
    }

    #[test]
    fn only_the_marker_as_first_argument_makes_a_helper() {
        assert_eq!(serve(&["read".into(), MARKER.into()]), None);
        assert_eq!(serve(&[]), None);
    }
}
