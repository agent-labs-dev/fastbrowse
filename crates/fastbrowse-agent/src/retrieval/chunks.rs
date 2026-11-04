//! Chunks of a capture sized for one reader prompt, the spans code cuts from them, and the prompt that shows them.

use fastbrowse_core::llm::Message;
use fastbrowse_core::models::Evidence;
use fastbrowse_core::page::{Block, BlockKind, Capture};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::text;
use fastbrowse_core::schema::{Field, ModelTy, Schema, Ty};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::text::{Page, splitlines, table_header_from_text};
use crate::error::{AgentError, Result};

/// One reader prompt's share of a capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chunk {
    pub index: i64,
    pub total: i64,
    pub start: i64,
    /// Bounds of the payload; a repeated header may precede start in the capture.
    pub end: i64,
    pub text: String,
    pub block_ids: Vec<String>,
    /// A table's header repeated before a continuation of its rows, which lies before `start`.
    #[serde(default)]
    pub header: String,
}

#[derive(Debug, Clone)]
struct Piece<'a> {
    block: &'a Block,
    start: i64,
    end: i64,
    header: Option<Block>,
}

fn table_header(page: &Page<'_>, block: &Block) -> Option<Block> {
    let header = table_header_from_text(page.slice(block.start, block.end))?;
    Some(Block { end: block.start + text::len(&header) as i64, ..block.clone() })
}

/// `_lines`: line spans of `capture.text[start:end]`, a line longer than `max_chars` cut into spans that fit.
fn lines(page: &Page<'_>, start: i64, end: i64, max_chars: i64) -> Vec<(i64, i64)> {
    let from = start.clamp(0, page.len()) as usize;
    let to = end.clamp(0, page.len()) as usize;
    let mut spans = Vec::new();
    if from >= to {
        return spans;
    }
    let mut offset = start;
    for (line_start, line_end) in splitlines(&page.chars[from..to]) {
        let length = (line_end - line_start) as i64;
        let mut cut = 0;
        while cut < length {
            spans.push((offset + cut, offset + (cut + max_chars).min(length)));
            cut += max_chars;
        }
        offset += length;
    }
    spans
}

fn pieces<'a>(page: &Page<'a>, max_chars: i64) -> Vec<Piece<'a>> {
    let piece = |block: &'a Block, start: i64, end: i64, header: Option<Block>| Piece { block, start, end, header };
    let mut result = Vec::new();
    let mut header: Option<Block> = None;
    for block in &page.capture.blocks {
        if block.kind != BlockKind::Table {
            header = None;
            // Split oversized blocks so the reader's prompt still has room for notes.
            if block.end - block.start <= max_chars {
                result.push(piece(block, block.start, block.end, None));
            } else {
                result.extend(
                    lines(page, block.start, block.end, max_chars).into_iter().map(|(s, e)| piece(block, s, e, None)),
                );
            }
            continue;
        }
        if header.as_ref().is_some_and(|h| (&h.frame_id, &h.heading_path) != (&block.frame_id, &block.heading_path)) {
            header = None;
        }
        let own_header = table_header(page, block);
        if own_header.is_none() && block.end - block.start > max_chars {
            // Headerless comparison tables are one source; bounded reads must still retain their first row.
            let first_end = page.find('\n', block.start, block.end);
            match first_end {
                Some(first_end) if first_end - block.start + 1 < max_chars => {
                    let prefix = Block { end: first_end + 1, ..block.clone() };
                    result.push(piece(block, block.start, prefix.end, None));
                    let room = max_chars - (prefix.end - prefix.start);
                    for (s, e) in lines(page, prefix.end, block.end, room) {
                        result.push(piece(block, s, e, Some(prefix.clone())));
                    }
                }
                _ => result.extend(
                    lines(page, block.start, block.end, max_chars).into_iter().map(|(s, e)| piece(block, s, e, None)),
                ),
            }
            header = None;
            continue;
        }
        header = Some(own_header.clone().or_else(|| header.clone()).unwrap_or_else(|| block.clone()));
        match own_header {
            Some(own) if block.end - block.start > max_chars => {
                result.push(piece(block, block.start, own.end, header.clone()));
                for (s, e) in lines(page, own.end, block.end, max_chars) {
                    result.push(piece(block, s, e, header.clone()));
                }
            }
            _ => result.push(piece(block, block.start, block.end, header.clone())),
        }
    }
    result
}

/// The text a header shown before a piece adds: `text[start:end].rstrip("\n")`.
fn shown<'a>(page: &Page<'a>, start: i64, end: i64) -> &'a str {
    page.slice(start, end).trim_end_matches('\n')
}

fn chunk_text(page: &Page<'_>, pieces: &[Piece<'_>]) -> (String, Vec<String>, String) {
    let mut spans: Vec<(i64, i64)> = pieces.iter().map(|p| (p.start, p.end)).collect();
    let mut ids: Vec<String> = pieces.iter().map(|p| p.block.source_id.clone()).collect();
    let first = &pieces[0];
    let mut header = String::new();
    if let Some(h) = first.header.as_ref().filter(|h| h.end <= first.start) {
        header = shown(page, h.start, h.end).to_string();
        spans.insert(0, (h.start, h.end));
        ids.insert(0, h.source_id.clone());
    }
    let text: Vec<&str> = spans.iter().map(|(start, end)| shown(page, *start, *end)).collect();
    (text.join("\n"), crate::memory::unique(ids), header)
}

/// `chunk(capture, max_chars, overlap_blocks=1)`. Chunk budgets are soft only for an indivisible block or row plus its
/// table header; large Markdown tables split on row boundaries, keeping the original source id.
pub fn chunk(capture: &Capture, max_chars: i64, overlap_blocks: i64) -> Result<Vec<Chunk>> {
    if max_chars <= 0 || overlap_blocks < 0 {
        return Err(AgentError::Invalid("max_chars must be positive and overlap_blocks nonnegative".into()));
    }
    let page = Page::new(capture);
    Ok(chunk_page(&page, max_chars, overlap_blocks as usize))
}

pub(crate) fn chunk_page(page: &Page<'_>, max_chars: i64, overlap_blocks: usize) -> Vec<Chunk> {
    let pieces = pieces(page, max_chars);
    // Size candidate spans without repeatedly joining every prefix of a long chunk.
    let mut lengths = vec![0i64];
    let mut header_lengths: IndexMap<(i64, i64), i64> = IndexMap::new();
    for piece in &pieces {
        lengths.push(lengths[lengths.len() - 1] + text::len(shown(page, piece.start, piece.end)) as i64 + 1);
        if let Some(h) = &piece.header {
            header_lengths.entry((h.start, h.end)).or_insert_with(|| text::len(shown(page, h.start, h.end)) as i64 + 1);
        }
    }
    let size = |start: usize, end: usize| {
        let mut length = lengths[end] - lengths[start] - 1;
        let first = &pieces[start];
        if let Some(h) = first.header.as_ref().filter(|h| h.end <= first.start) {
            length += header_lengths[&(h.start, h.end)];
        }
        length
    };
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut cursor = 0;
    while cursor < pieces.len() {
        let mut start = cursor.saturating_sub(overlap_blocks);
        if pieces[cursor].block.kind == BlockKind::Heading {
            start = cursor;
        }
        // Overlap must never prevent forward progress, even for one oversized block.
        while start < cursor && size(start, cursor + 1) > max_chars {
            start += 1;
        }
        let mut end = cursor + 1;
        while end < pieces.len() && size(start, end + 1) <= max_chars {
            end += 1;
        }
        if end < pieces.len()
            && let Some(heading) = (cursor + 1..end).rev().find(|i| pieces[*i].block.kind == BlockKind::Heading)
        {
            end = heading;
        }
        let (text, block_ids, header) = chunk_text(page, &pieces[start..end]);
        chunks.push(Chunk {
            index: chunks.len() as i64,
            total: 0,
            start: pieces[start].start,
            end: pieces[end - 1].end,
            text,
            block_ids,
            header,
        });
        cursor = end;
    }
    let total = chunks.len() as i64;
    chunks.into_iter().map(|chunk| Chunk { total, ..chunk }).collect()
}

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

/// The page's first rendered text, quoted block by block, for a page the run stops on without reading.
pub fn leading_quotes(capture: &Capture, max_chars: i64) -> Vec<Evidence> {
    let page = Page::new(capture);
    let mut quotes = Vec::new();
    let mut left = max_chars;
    for block in &capture.blocks {
        // A typed observation is a description code wrote, not words the page showed, and a download the session
        // kept is an earlier file appended after the page, not the page itself.
        if left <= 0
            || block.kind == BlockKind::Observation
            || block.source_url.is_some()
            || text::strip(page.slice(block.start, block.end)).is_empty()
        {
            continue;
        }
        let end = block.end.min(block.start + left);
        quotes.push(evidence(&page, block, block.start, end));
        left -= end - block.start;
    }
    quotes
}

/// A run of source blocks a model names by its ends.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Cite {
    pub first: String,
    pub last: String,
}

impl Cite {
    pub fn one(source_id: &str) -> Cite {
        Cite { first: source_id.into(), last: source_id.into() }
    }
}

impl Schema for Cite {
    fn ty() -> Ty {
        ModelTy::frozen(
            "_Cite",
            vec![
                Field::required("first", Ty::Str).doc(
                    "The first source block the claim reads, by label without brackets (main/:12 for a line shown as\n[main/:12]); never an evidence id.",
                ),
                Field::required("last", Ty::Str).doc(
                    "The last source block it reads: the same label for one block, a later one for a run of blocks.",
                ),
            ],
        )
        .ty()
    }
}

/// `_offered`: the blocks the reader is shown for this chunk, in page order: the only ones a cite may name.
pub(crate) fn offered<'a>(page: &Page<'a>, part: &Chunk) -> Vec<&'a Block> {
    page.capture
        .blocks
        .iter()
        .filter(|b| part.block_ids.contains(&b.source_id) && b.start < part.end && b.end > part.start)
        .collect()
}

/// `_cited`: the text a run of blocks showed the reader, when both ends were offered in this chunk, in order, in one
/// frame.
///
/// The reader names blocks and code copies their text: a model asked to reproduce a quote writes the page as it reads
/// it, and a table's escaped pipe, a record split over two quotes or a placeholder for a count followed. A run is given
/// by its ends because a model listing a run's blocks wrote only its first and last.
pub(crate) fn cited(page: &Page<'_>, part: &Chunk, cite: &Cite) -> Option<Evidence> {
    let offered = offered(page, part);
    // A later block with the same id wins, as a dict built from the list keeps the last.
    let position = |id: &str| offered.iter().rposition(|block| block.source_id == id);
    let (first, last) = (position(&cite.first)?, position(&cite.last)?);
    if first > last {
        return None;
    }
    let run = &offered[first..=last];
    let address =
        |block: &Block| (block.frame_id.clone(), block.source_url.clone().unwrap_or(page.capture.url.clone()));
    if run.iter().any(|block| address(block) != address(run[0])) {
        return None;
    }
    Some(evidence(page, run[0], run[0].start.max(part.start), run[run.len() - 1].end.min(part.end)))
}

/// `_marks`: what the text alone does not say about a block. A frame's text reads like the page around it, and a
/// heading like any line: asked for the heading an embedded form shows, the reader gave the page's heading above the
/// frame, and once shown which text was the frame's, called the form's own <h1> a placeholder.
pub(crate) fn marks(block: &Block) -> String {
    let mut marks = Vec::new();
    if block.kind == BlockKind::Heading {
        marks.push("heading");
    }
    if block.frame_id.is_some() {
        marks.push("inside an embedded frame");
    }
    if marks.is_empty() { String::new() } else { format!("({}) ", marks.join(", ")) }
}

pub(crate) fn source_marker(block: &Block) -> String {
    let context = block
        .control_context
        .as_ref()
        .map(|context| format!(" control_context={}", context.dump_json()))
        .unwrap_or_default();
    format!(
        "[{}] ({}{}){context} ",
        block.source_id,
        block.kind.as_str(),
        if block.frame_id.is_some() { ", inside an embedded frame" } else { "" }
    )
}

/// `_read_message`.
pub(crate) fn read_message(
    page: &Page<'_>,
    part: &Chunk,
    question: &str,
    requirement_ids: &[String],
    evidence: &str,
) -> Message {
    let offered = offered(page, part);
    // A table cut mid-rows is shown under its header, which lies before the chunk, so its columns keep their names.
    let sources: Vec<String> = offered
        .iter()
        .map(|block| {
            let header = if !part.header.is_empty() && block.start < part.start {
                format!("{}\n", part.header)
            } else {
                String::new()
            };
            format!(
                "{}{header}{}",
                source_marker(block),
                page.slice(block.start.max(part.start), block.end.min(part.end))
            )
        })
        .collect();
    // Context first and the question last, as long-context guidance for Gemini and Claude recommends.
    let content = format!(
        "# Collected evidence\n{evidence}\n\n# Capture\nURL: {}\nInaccessible frames: {}\n\n# Source blocks (chunk {} of {})\n{}\n\n# Question\n{question}\n\n# Requirement ids\n{}",
        page.capture.url,
        page.capture.inaccessible_frames,
        part.index + 1,
        part.total,
        sources.join("\n"),
        requirement_ids.join(", "),
    );
    Message::user(content)
}

/// `_read_message(capture, part, question, requirement_ids, evidence)`, for callers holding only the capture.
pub fn read_message_for(
    capture: &Capture,
    part: &Chunk,
    question: &str,
    requirement_ids: &[String],
    evidence: &str,
) -> Message {
    read_message(&Page::new(capture), part, question, requirement_ids, evidence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retrieval::testing::capture;

    #[test]
    fn a_quote_crossing_a_dom_observation_is_not_literal_page_text() {
        let captured = capture(&[(BlockKind::Paragraph, "Create agent"), (BlockKind::Observation, "Name: [empty]")]);
        let page = Page::new(&captured);
        let block = &captured.blocks[0];
        assert!(!evidence(&page, block, 0, page.len()).rendered_text);
        assert!(evidence(&page, block, 0, block.end).rendered_text);
    }
}
