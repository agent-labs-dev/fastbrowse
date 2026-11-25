//! The spans code cuts from a capture.

use fastbrowse_core::citations::table_blocks;
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

/// The first span of `capture` that is `quote` exactly and lies in blocks of one document, cut as code cuts any quote. A model
/// that saw the page cites its words; code finds them, so a claim rests on page text whatever the model wrote.
///
/// A quote of a table row's cells starts at its block, which the capture opens with the table's header: cut alone,
/// `| Boston Bruins | 1990 | 44 | 24 |` said nothing of which number is wins, and the answer gave the losses.
pub(crate) fn quoted(capture: &Capture, quote: &str) -> Option<Evidence> {
    if text::strip(quote).is_empty() {
        return None;
    }
    let page = Page::new(capture);
    let tables = table_blocks(capture);
    spans_of(&capture.text, quote).find_map(|(at, to)| {
        let start = capture.text[..at].chars().count() as i64;
        let end = start + capture.text[at..to].chars().count() as i64;
        // A quote may run across blocks of one document, its frame and address: saucedemo draws "Total: $" and "43.18"
        // as two, so the total as the driver read it lay in no single block and done was refused with it on screen.
        // A block's source id names that block alone, so it cannot say which blocks are one document.
        let covered: Vec<&Block> =
            capture.blocks.iter().filter(|block| block.start < end && start < block.end).collect();
        let block = *covered.first()?;
        let one_source = covered.iter().all(|other| {
            !matches!(other.kind, BlockKind::DocumentTitle | BlockKind::Observation)
                && other.frame_id == block.frame_id
                && other.source_url == block.source_url
        });
        if !one_source || block.start > start || covered.last()?.end < end {
            return None;
        }
        let start = tables
            .iter()
            .find(|table| table.start == block.start && table.start < table.row && table.row <= start)
            .map_or(start, |table| table.start);
        Some(evidence(&page, block, start, end))
    })
}

/// The byte spans of `text` that hold `quote`'s words in order, each gap between them any run of whitespace. The
/// driver is shown a page one block per line and writes the words back as it read them: it quoted "Total: $\n43.18"
/// as "Total: $ 43.18", and done was refused twice with the total on screen. Whitespace is layout, not the words a
/// quote cites, so this is the comparison a quote needs; the evidence keeps the page's own text.
///
/// The span holds whole words, as `memory::shows` judges them: cut from inside one, "verified" was the evidence of a
/// page reading "unverified", and the claim it cited was judged supported.
fn spans_of<'a>(text: &'a str, quote: &'a str) -> impl Iterator<Item = (usize, usize)> + 'a {
    let words: Vec<&str> = quote.split_whitespace().collect();
    let first = words.first().copied().unwrap_or_default();
    text.match_indices(first).filter(|_| !first.is_empty()).filter_map(move |(at, _)| {
        if text[..at].chars().next_back().is_some_and(text::is_word) && first.chars().next().is_some_and(text::is_word)
        {
            return None;
        }
        let mut to = at + first.len();
        for word in &words[1..] {
            let rest = &text[to..];
            let gap = rest.len() - rest.trim_start().len();
            if gap == 0 || !rest[gap..].starts_with(word) {
                return None;
            }
            to += gap + word.len();
        }
        let last = words.last().copied().unwrap_or_default();
        (!(text[to..].chars().next().is_some_and(text::is_word) && last.chars().next_back().is_some_and(text::is_word)))
            .then_some((at, to))
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
    fn a_quote_the_page_does_not_hold_is_not_found() {
        let page = capture("Date: 12/04/2026\n\nDay: Friday", &[(0, 16), (18, 29)]);
        assert!(quoted(&page, "Day: Monday").is_none());
        assert!(quoted(&page, "  ").is_none());
    }

    #[test]
    fn a_quote_is_whole_words_of_the_page() {
        let page = capture("Status: unverified", &[(0, 18)]);
        assert!(quoted(&page, "verified").is_none(), "a word cut from inside another");
        assert!(quoted(&page, "Status: unver").is_none(), "a word cut short");
        assert!(quoted(&page, "unverified").is_some());
    }

    #[test]
    fn a_quote_of_a_table_row_carries_the_header_its_block_opens_with() {
        let text = "| Team | Year | Wins | Losses |\n| --- | --- | --- | --- |\n| Boston Bruins | 1990 | 44 | 24 |";
        let page: Capture = serde_json::from_value(serde_json::json!({
            "url": "https://example.test/", "title": "t", "captured_at": "2026-11-18T00:00:00Z", "sha256": "s",
            "text": text, "blocks": [{"source_id": "r", "frame_id": null, "kind": "table", "start": 0,
                                      "end": text.chars().count()}]
        }))
        .unwrap();
        let row = quoted(&page, "| Boston Bruins | 1990 | 44 | 24 |").expect("on the page");
        assert_eq!((row.start, row.quote.as_str()), (0, text));
        let header = quoted(&page, "| Team | Year |").expect("on the page");
        assert_eq!(header.quote, "| Team | Year |", "a quote of the header itself is cut as written");
    }

    #[test]
    fn a_quote_may_run_across_blocks_of_one_document_but_not_into_another() {
        let text = "Total: $\n43.18\nAd";
        let page: Capture = serde_json::from_value(serde_json::json!({
            "url": "https://example.test/", "title": "t", "captured_at": "2026-11-18T00:00:00Z", "sha256": "s",
            "text": text, "blocks": [
                {"source_id": "main/div:0", "frame_id": null, "kind": "paragraph", "start": 0, "end": 8},
                {"source_id": "main/div:1", "frame_id": null, "kind": "paragraph", "start": 9, "end": 14},
                {"source_id": "f/div:2", "frame_id": "f", "kind": "paragraph", "start": 15, "end": 17}]
        }))
        .unwrap();
        let total = quoted(&page, "Total: $\n43.18").expect("one document's blocks");
        assert_eq!((total.start, total.end), (0, 14));
        let spaced = quoted(&page, "Total: $ 43.18").expect("the words, whatever whitespace joins them");
        assert_eq!((spaced.start, spaced.end, spaced.quote.as_str()), (0, 14, "Total: $\n43.18"));
        assert!(quoted(&page, "Total: $43.18").is_none(), "words the page keeps apart are not one word");
        assert!(quoted(&page, "43.18\nAd").is_none(), "a quote into another frame's block is not one passage");
    }
}
