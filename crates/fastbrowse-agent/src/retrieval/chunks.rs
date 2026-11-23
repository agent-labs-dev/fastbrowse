//! The spans code cuts from a capture.

use std::ops::Range;

use fastbrowse_core::effects::ShownPiece;
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

/// The lines of the capture's rendered text that hold text a step added, each quoted once. `shown` is the page's text
/// after the step, piece by piece in the order the page lists them, each marked added or kept.
///
/// An observation lists each DOM text node on its own line while a capture joins the inline content around it, so a
/// captured line reads as a run of consecutive pieces: "Status: <strong>Changes saved successfully</strong>" is two. A
/// line is quoted when the run it reads as holds an added piece. Lines are matched to runs longest first, and among
/// lines alike in the order both list them, so a line repeating the added text elsewhere takes its own kept piece:
/// matched on text alone, a help line reading "Changes saved successfully" took the status's added piece, and the
/// site's reply went uncited. A line found only in a typed observation, a document title or a kept download is not
/// quoted: none of them is text the page showed.
pub(crate) fn line_quotes(capture: &Capture, shown: &[ShownPiece]) -> Vec<Evidence> {
    let page = Page::new(capture);
    let pieces: Vec<(String, bool)> =
        shown.iter().map(|piece| (squeezed(&piece.text), piece.added)).filter(|(text, _)| !text.is_empty()).collect();
    let added: Vec<&str> = pieces.iter().filter(|(_, added)| *added).map(|(text, _)| text.as_str()).collect();
    // Only a line holding some added text can be the reply, and only such lines compete for its pieces.
    let mut lines: Vec<(i64, i64, String, &Block)> = super::text::splitlines(&page.chars)
        .into_iter()
        .filter_map(|(start, end)| {
            let (mut start, mut end) = (start as i64, end as i64);
            while start < end && page.chars[start as usize].is_whitespace() {
                start += 1;
            }
            while end > start && page.chars[end as usize - 1].is_whitespace() {
                end -= 1;
            }
            let line = squeezed(page.slice(start, end));
            if line.is_empty() || !added.iter().any(|piece| line.contains(piece)) {
                return None;
            }
            let block = capture.blocks.iter().find(|block| {
                block.start <= start
                    && end <= block.end
                    && !matches!(block.kind, BlockKind::Observation | BlockKind::DocumentTitle)
                    && block.source_url.is_none()
            })?;
            Some((start, end, line, block))
        })
        .collect();
    lines.sort_by_key(|(_, _, line, _)| std::cmp::Reverse(line.chars().count()));
    let mut used = vec![false; pieces.len()];
    let mut matched = vec![false; lines.len()];
    let mut quotes = Vec::new();
    // A line that reads as a whole run takes it first. Only a line no run fills takes the part of it a run reads as
    // (a capture can hold text beside the pieces the observation listed), so a short piece never claims an unrelated
    // longer line while a line that is its own exists.
    for whole in [true, false] {
        for (at, (start, end, line, block)) in lines.iter().enumerate() {
            if matched[at] {
                continue;
            }
            let Some(run) = (0..pieces.len()).find_map(|from| run_in(&pieces, &used, from, line, whole)) else {
                continue;
            };
            matched[at] = true;
            used[run.clone()].fill(true);
            if pieces[run].iter().any(|(_, added)| *added) {
                quotes.push(evidence(&page, block, *start, *end));
            }
        }
    }
    quotes.sort_by_key(|quote| quote.start);
    quotes
}

fn squeezed(text: &str) -> String {
    text.split_whitespace().collect()
}

/// The run of unused pieces from `from` that reads as `line` once whitespace is left out, or with `whole` unset the
/// longest such run that reads as a part of it and holds an added piece.
fn run_in(pieces: &[(String, bool)], used: &[bool], from: usize, line: &str, whole: bool) -> Option<Range<usize>> {
    let mut joined = String::new();
    let mut found = None;
    for at in from..pieces.len() {
        if used[at] {
            break;
        }
        joined.push_str(&pieces[at].0);
        if whole && joined == line {
            return Some(from..at + 1);
        }
        if !(if whole { line.starts_with(&joined) } else { line.contains(&joined) }) {
            break;
        }
        if !whole && pieces[from..=at].iter().any(|(_, added)| *added) {
            found = Some(from..at + 1);
        }
    }
    found
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
