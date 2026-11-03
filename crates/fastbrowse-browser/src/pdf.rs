//! A PDF's text, for a run to read. Chrome draws a PDF in its own viewer, whose text never reaches the page's DOM, so
//! a PDF tab captures as an empty page and a downloaded PDF as a file with no text. Its bytes are read here instead.

use std::time::Duration;

/// A parse that outlasts this is given up on: the run waits for no file longer than it waits for a page to load.
const PARSE_SECONDS: u64 = 10;

/// The text a PDF's pages hold, in page order, or None when it holds none or cannot be parsed. A scanned PDF is an
/// image of text and holds none.
pub async fn text(raw: Vec<u8>) -> Option<String> {
    let parse = tokio::task::spawn_blocking(move || {
        // A malformed file can panic the parser; with unwinding that is one unreadable file, not a failed run.
        std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(&raw)).ok()?.ok()
    });
    let text = tokio::time::timeout(Duration::from_secs(PARSE_SECONDS), parse).await.ok()?.ok()??;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// The passages of a PDF's text, one per paragraph, so a claim cites the passage it rests on rather than the file.
pub fn paragraphs(text: &str) -> impl Iterator<Item = &str> {
    text.split("\n\n").map(str::trim).filter(|paragraph| !paragraph.is_empty())
}
