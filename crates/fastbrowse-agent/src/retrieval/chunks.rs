//! The spans code cuts from a capture.

use fastbrowse_core::models::{Evidence, Link};
use fastbrowse_core::page::{Block, BlockKind, Capture};
use fastbrowse_core::py::text;

use super::text::Page;
use crate::support::compute;

/// `_evidence`: the span `start..end` of `block`, as code cut it from the capture.
pub(crate) fn evidence(page: &Page<'_>, block: &Block, start: i64, end: i64) -> Evidence {
    let capture = page.capture;
    Evidence {
        source_id: block.source_id.clone(),
        url: block.source_url.clone().unwrap_or_else(|| capture.url.clone()),
        frame_id: block.frame_id.clone(),
        captured_at: capture.captured_at,
        capture_sha256: capture.sha256.clone(),
        start,
        end,
        quote: page.slice(start, end).to_string(),
        complete_source: block.complete_source
            && block.start <= start
            && start < end
            && end <= block.end
            && text::strip(page.slice(block.start, start)).is_empty()
            && text::strip(page.slice(end, block.end)).is_empty(),
        control_context: block.control_context.clone(),
        rendered_text: !capture.blocks.iter().any(|item| {
            matches!(item.kind, BlockKind::Observation | BlockKind::DocumentTitle)
                && item.start < end
                && item.end > start
        }),
        heading_path: block.heading_path.clone(),
        // Set when the run ends and keeps the captures it cites.
        capture_artifact: None,
        links: Link::within(&block.links, start, end),
    }
}

/// Each of `lines` as a quote of the capture's rendered text, cut at the last line of the page that reads as it once
/// stripped, and each line of the page quoted once. The last, because `added_text` names the last listed of a piece
/// the page already showed: the first would quote an element that was there before the step. A line found only in a
/// typed observation, a document title or a kept download is not quoted: none of them is text the page showed.
/// `unchanged` is the text the page showed before the step and after it: a line of only that is never the reply.
pub(crate) fn line_quotes(capture: &Capture, lines: &[String], unchanged: &[String]) -> Vec<Evidence> {
    let page = Page::new(capture);
    let mut wanted: Vec<&str> = lines.iter().map(|line| text::strip(line)).filter(|line| !line.is_empty()).collect();
    let known: Vec<&str> = unchanged.iter().map(|line| text::strip(line)).filter(|line| !line.is_empty()).collect();
    let mut quotes = Vec::new();
    let spans: Vec<_> = super::text::splitlines(&page.chars).into_iter().collect();
    // A line the added text fills is that text's quote. Only when none does, the line holding it beside text the page
    // already showed ("Status: " around a changed value) is, so a short piece never claims an unrelated longer line.
    for whole in [true, false] {
        for &(start, end) in spans.iter().rev() {
            let (mut start, mut end) = (start as i64, end as i64);
            while start < end && page.chars[start as usize].is_whitespace() {
                start += 1;
            }
            while end > start && page.chars[end as usize - 1].is_whitespace() {
                end -= 1;
            }
            let line = page.slice(start, end);
            // A line made of text the page showed before the step is not the page's reply, whatever it contains.
            if !whole && added_run(&known, line, true).is_some() {
                continue;
            }
            let Some(run) = added_run(&wanted, line, whole) else { continue };
            let Some(block) = capture.blocks.iter().find(|block| {
                block.start <= start
                    && end <= block.end
                    && !matches!(block.kind, BlockKind::Observation | BlockKind::DocumentTitle)
                    && block.source_url.is_none()
            }) else {
                continue;
            };
            wanted.drain(run);
            quotes.push(evidence(&page, block, start, end));
        }
    }
    quotes.sort_by_key(|quote| quote.start);
    quotes
}

/// The consecutive added pieces that read as `line` once whitespace is left out. An observation lists each DOM text
/// node on its own line, while a capture joins the inline content around it, so "Changes <strong>saved</strong>
/// successfully" is three added pieces and one captured line. The last such run, as the capture is read from its end:
/// a later line that reads like one of the notice's pieces must take its own piece, not the notice's. With `whole`
/// unset, the run may be a part of the line.
fn added_run(wanted: &[&str], line: &str, whole: bool) -> Option<std::ops::Range<usize>> {
    let squeezed = |text: &str| text.split_whitespace().collect::<String>();
    let line = squeezed(line);
    if line.is_empty() {
        return None;
    }
    (0..wanted.len()).rev().find_map(|from| {
        let mut joined = String::new();
        for (at, piece) in wanted.iter().enumerate().skip(from) {
            joined.push_str(&squeezed(piece));
            if joined == line || (!whole && line.contains(&joined)) {
                return Some(from..at + 1);
            }
            if !(if whole { line.starts_with(&joined) } else { line.contains(&joined) }) {
                return None;
            }
        }
        None
    })
}

/// The page's rendered text, quoted block by block, for a page the run stops on without reading.
pub fn page_quotes(capture: &Capture) -> Vec<Evidence> {
    // Each block's evidence looks at every other block, so a page of many blocks is long work (#1284).
    compute(|| {
        let page = Page::new(capture);
        capture
            .blocks
            .iter()
            // A typed observation is a description code wrote, not words the page showed, and a download the session
            // kept is an earlier file appended after the page, not the page itself.
            .filter(|block| {
                !matches!(block.kind, BlockKind::Observation | BlockKind::DocumentTitle)
                    && block.source_url.is_none()
                    && !text::strip(page.slice(block.start, block.end)).is_empty()
            })
            .map(|block| evidence(&page, block, block.start, block.end))
            .collect()
    })
}
