//! The spans code cuts from a capture.

use fastbrowse_core::models::Evidence;
use fastbrowse_core::page::{Block, BlockKind, Capture};
use fastbrowse_core::py::text;

use super::text::Page;

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
        rendered_text: !capture
            .blocks
            .iter()
            .any(|item| item.kind == BlockKind::Observation && item.start < end && item.end > start),
        heading_path: block.heading_path.clone(),
    }
}

/// The page's rendered text, quoted block by block, for a page the run stops on without reading.
pub fn page_quotes(capture: &Capture) -> Vec<Evidence> {
    let page = Page::new(capture);
    capture
        .blocks
        .iter()
        // A typed observation is a description code wrote, not words the page showed, and a download the session
        // kept is an earlier file appended after the page, not the page itself.
        .filter(|block| {
            block.kind != BlockKind::Observation
                && block.source_url.is_none()
                && !text::strip(page.slice(block.start, block.end)).is_empty()
        })
        .map(|block| evidence(&page, block, block.start, block.end))
        .collect()
}
