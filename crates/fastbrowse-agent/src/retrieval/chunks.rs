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

/// The first span of `capture` that is `quote` exactly and lies inside one block, cut as code cuts any quote. A model
/// that saw the page cites its words; code finds them, so a claim rests on page text whatever the model wrote.
pub(crate) fn quoted(capture: &Capture, quote: &str) -> Option<Evidence> {
    if text::strip(quote).is_empty() {
        return None;
    }
    let page = Page::new(capture);
    let length = quote.chars().count() as i64;
    capture.text.match_indices(quote).find_map(|(at, _)| {
        let start = capture.text[..at].chars().count() as i64;
        let end = start + length;
        let block = capture
            .blocks
            .iter()
            .find(|block| block.kind != BlockKind::DocumentTitle && block.start <= start && end <= block.end)?;
        Some(evidence(&page, block, start, end))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(text: &str, blocks: &[(i64, i64)]) -> Capture {
        let blocks: Vec<_> = blocks
            .iter()
            .enumerate()
            .map(|(i, (start, end))| {
                serde_json::json!({"source_id": format!("b{i}"), "frame_id": null, "kind": "paragraph",
                                   "start": start, "end": end})
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "url": "https://example.test/", "title": "t", "captured_at": "2026-11-18T00:00:00Z",
            "sha256": "s", "text": text, "blocks": blocks
        }))
        .unwrap()
    }

    #[test]
    fn a_quote_is_cut_from_the_block_that_holds_it() {
        let page = capture("Date: 12/04/2026\n\nDay: Friday", &[(0, 16), (18, 29)]);
        let day = quoted(&page, "Day: Friday").expect("on the page");
        assert_eq!((day.start, day.end, day.quote.as_str(), day.source_id.as_str()), (18, 29, "Day: Friday", "b1"));
    }

    #[test]
    fn a_quote_the_page_does_not_hold_in_one_block_is_not_found() {
        let page = capture("Date: 12/04/2026\n\nDay: Friday", &[(0, 16), (18, 29)]);
        assert!(quoted(&page, "Day: Monday").is_none());
        assert!(quoted(&page, "2026\n\nDay").is_none(), "spans two blocks");
        assert!(quoted(&page, "  ").is_none());
    }
}
