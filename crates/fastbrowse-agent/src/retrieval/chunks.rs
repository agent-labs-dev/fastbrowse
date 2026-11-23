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

/// Each of `lines` as a quote of the capture's rendered text, cut at the first line of the page that reads as it once
/// stripped, and each line of the page quoted once. A line found only in a typed observation, a document title or a
/// kept download is not quoted: none of them is text the page showed.
pub(crate) fn line_quotes(capture: &Capture, lines: &[String]) -> Vec<Evidence> {
    let page = Page::new(capture);
    let mut wanted: Vec<&str> = lines.iter().map(|line| text::strip(line)).filter(|line| !line.is_empty()).collect();
    let mut quotes = Vec::new();
    for (start, end) in super::text::splitlines(&page.chars) {
        let (mut start, mut end) = (start as i64, end as i64);
        while start < end && page.chars[start as usize].is_whitespace() {
            start += 1;
        }
        while end > start && page.chars[end as usize - 1].is_whitespace() {
            end -= 1;
        }
        let Some(at) = wanted.iter().position(|line| *line == page.slice(start, end)) else { continue };
        let Some(block) = capture.blocks.iter().find(|block| {
            block.start <= start
                && end <= block.end
                && !matches!(block.kind, BlockKind::Observation | BlockKind::DocumentTitle)
                && block.source_url.is_none()
        }) else {
            continue;
        };
        wanted.remove(at);
        quotes.push(evidence(&page, block, start, end));
    }
    quotes
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
