//! A capture's text indexed as Python indexes a `str`, and the patterns retrieval matches against it.
//!
//! Block offsets are code points, so every slice here takes code point offsets and clamps them the way a Python slice
//! does: a quote cut one character apart from Python's is another citation id.

use fastbrowse_core::page::Capture;
use fastbrowse_core::py::text::{is_space, is_word};

use crate::pyre::{Pat, byte_offsets, is_digit};

/// A capture with its text indexed by code point.
pub(crate) struct Page<'a> {
    pub capture: &'a Capture,
    pub chars: Vec<char>,
    bytes: Vec<usize>,
}

impl<'a> Page<'a> {
    pub fn new(capture: &'a Capture) -> Page<'a> {
        Page { capture, chars: capture.text.chars().collect(), bytes: byte_offsets(&capture.text) }
    }

    pub fn len(&self) -> i64 {
        self.chars.len() as i64
    }

    fn clamp(&self, at: i64) -> usize {
        at.clamp(0, self.len()) as usize
    }

    /// `capture.text[start:end]`.
    pub fn slice(&self, start: i64, end: i64) -> &'a str {
        let (start, end) = (self.clamp(start), self.clamp(end));
        if start >= end {
            return "";
        }
        &self.capture.text[self.bytes[start]..self.bytes[end]]
    }
}

/// `text[start:end]` by code points, clamped as a slice is.
pub(crate) fn slice(text: &str, start: usize, end: usize) -> &str {
    let bytes = byte_offsets(text);
    let count = bytes.len() - 1;
    let (start, end) = (start.min(count), end.min(count));
    if start >= end { "" } else { &text[bytes[start]..bytes[end]] }
}

/// Whether a character ends a line for `str.splitlines`.
fn line_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// `str.splitlines(keepends=True)` as code point spans: `\r\n` is one break, and so are the other separators Python
/// splits on, the vertical tab and the Unicode line and paragraph separators among them.
pub(crate) fn splitlines(chars: &[char]) -> Vec<(usize, usize)> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut at = 0;
    while at < chars.len() {
        if line_break(chars[at]) {
            let end = if chars[at] == '\r' && chars.get(at + 1) == Some(&'\n') { at + 2 } else { at + 1 };
            lines.push((start, end));
            start = end;
            at = end;
        } else {
            at += 1;
        }
    }
    if start < chars.len() {
        lines.push((start, chars.len()));
    }
    lines
}

/// `str.splitlines()` without the breaks.
pub(crate) fn lines_of(value: &str) -> Vec<String> {
    let chars: Vec<char> = value.chars().collect();
    splitlines(&chars)
        .into_iter()
        .map(|(start, end)| {
            let mut stop = end;
            while stop > start && line_break(chars[stop - 1]) {
                stop -= 1;
            }
            chars[start..stop].iter().collect()
        })
        .collect()
}

/// `re.fullmatch(r"\s*\|?[\s:|\-]+\|?\s*", line) and "---" in line`: a markdown table's separator row.
fn separator_row(line: &str) -> bool {
    !line.is_empty() && line.chars().all(|c| is_space(c) || matches!(c, ':' | '|' | '-')) && line.contains("---")
}

/// `_table_header_from_text`: the header and separator rows a markdown table opens with.
pub(crate) fn table_header_from_text(value: &str) -> Option<String> {
    let chars: Vec<char> = value.chars().collect();
    let lines = splitlines(&chars);
    if lines.len() < 2 {
        return None;
    }
    let second: String = chars[lines[1].0..lines[1].1].iter().collect();
    separator_row(&second).then(|| chars[lines[0].0..lines[1].1].iter().collect())
}

/// `re.fullmatch(r"\|(?:\s*-+\s*\|)+\s*", line)`: a separator row of dashes alone.
fn dash_row(line: &[char]) -> bool {
    let column = Pat::Seq(vec![
        Pat::star(Pat::class(is_space)),
        Pat::plus(Pat::lit("-")),
        Pat::star(Pat::class(is_space)),
        Pat::lit("|"),
    ]);
    Pat::Seq(vec![Pat::lit("|"), Pat::plus(column), Pat::star(Pat::class(is_space))]).full_match(line)
}

/// One cell's span by `(?<=\|)\s*((?:[^|\\\n]|\\.)+?)\s*(?=\|)` from `at`, just after a pipe, with the match's end.
fn cell_at(line: &[char], at: usize) -> Option<(usize, usize, usize)> {
    let lead = line[at..].iter().take_while(|c| is_space(**c)).count();
    // The leading spaces are greedy, so a cell that is all space gives back its last one to the lazy group.
    for taken in (0..=lead).rev() {
        let start = at + taken;
        let mut end = start;
        loop {
            let unit = match line.get(end) {
                Some('|' | '\n') | None => None,
                Some('\\') => line.get(end + 1).filter(|c| **c != '\n').map(|_| end + 2),
                Some(_) => Some(end + 1),
            };
            let Some(next) = unit else { break };
            end = next;
            let close = end + line[end..].iter().take_while(|c| is_space(**c)).count();
            if line.get(close) == Some(&'|') {
                return Some((start, end, close));
            }
        }
    }
    None
}

/// `_cells(table)`: cell spans of a pipe-rendered table, so a string field can pick one cell rather than the table.
pub(crate) fn cells(table: &str) -> Vec<(usize, usize, String)> {
    let chars: Vec<char> = table.chars().collect();
    let mut spans = Vec::new();
    for (offset, end) in splitlines(&chars) {
        let line = &chars[offset..end];
        if dash_row(line) {
            continue;
        }
        let mut at = 1;
        while at <= line.len() {
            if line[at - 1] == '|'
                && let Some((start, stop, close)) = cell_at(line, at)
            {
                spans.push((offset + start, offset + stop, line[start..stop].iter().collect()));
                at = close.max(at + 1);
                continue;
            }
            at += 1;
        }
    }
    spans
}

/// The shapes of value `_spans` finds, by the field's annotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpanKind {
    Str,
    Bool,
    Date,
    Number,
}

/// `$` without MULTILINE at `at`.
fn at_end(chars: &[char], at: usize) -> bool {
    at == chars.len() || (at + 1 == chars.len() && chars[at] == '\n')
}

/// `r"\S[^\n]*?(?:[.!?](?=[ \t]|$)|(?=\n|$))"`: a sentence or the rest of a line. Written out because its lazy run
/// can cover a whole line of a long block, which a recursive matcher would walk one frame per character.
fn sentences(chars: &[char]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(start) = (at..chars.len()).find(|i| !is_space(chars[*i])) {
        let mut end = start + 1;
        loop {
            if chars.get(end).is_some_and(|c| matches!(c, '.' | '!' | '?'))
                && (matches!(chars.get(end + 1), Some(' ' | '\t')) || at_end(chars, end + 1))
            {
                end += 1;
                break;
            }
            if chars.get(end) == Some(&'\n') || at_end(chars, end) {
                break;
            }
            end += 1;
        }
        found.push((start, end));
        at = end;
    }
    found
}

pub(crate) fn currency(c: char) -> bool {
    matches!(c, '$' | '\u{a3}' | '\u{20ac}' | '\u{a5}')
}

fn sign(c: char) -> bool {
    matches!(c, '+' | '-')
}

/// The pattern `_spans` uses for one annotation.
fn span_pattern(kind: SpanKind) -> Pat {
    let digit = || Pat::class(is_digit);
    match kind {
        SpanKind::Str => unreachable!("sentences are matched by `sentences`"),
        SpanKind::Bool => Pat::Seq(vec![
            Pat::Boundary,
            Pat::alt(vec![Pat::ilit("true"), Pat::ilit("false"), Pat::ilit("yes"), Pat::ilit("no")]),
            Pat::Boundary,
        ]),
        SpanKind::Date => Pat::Seq(vec![
            Pat::not_behind(is_digit),
            Pat::repeat(digit(), 4, 4),
            Pat::lit("-"),
            Pat::repeat(digit(), 2, 2),
            Pat::lit("-"),
            Pat::repeat(digit(), 2, 2),
            Pat::not_ahead(digit()),
        ]),
        SpanKind::Number => Pat::Seq(vec![
            Pat::not_behind(|c| is_word(c) || c == '.' || c == ','),
            Pat::alt(vec![
                Pat::Seq(vec![
                    Pat::alt(vec![
                        Pat::Seq(vec![Pat::opt(Pat::class(sign)), Pat::class(currency)]),
                        Pat::Seq(vec![Pat::class(currency), Pat::opt(Pat::class(sign))]),
                    ]),
                    Pat::star(Pat::class(|c| c == ' ' || c == '\t')),
                ]),
                Pat::opt(Pat::class(sign)),
            ]),
            Pat::alt(vec![
                Pat::Seq(vec![
                    Pat::repeat(digit(), 1, 3),
                    Pat::plus(Pat::Seq(vec![Pat::lit(","), Pat::repeat(digit(), 3, 3)])),
                ]),
                Pat::plus(digit()),
            ]),
            Pat::opt(Pat::Seq(vec![Pat::lit("."), Pat::plus(digit())])),
            Pat::not_ahead(Pat::alt(vec![
                Pat::class(|c| is_word(c) || c == ','),
                Pat::Seq(vec![Pat::lit("."), digit()]),
            ])),
        ]),
    }
}

/// `_spans(text, annotation)`: every value of the annotation's shape, as code point spans with their text.
pub(crate) fn spans(value: &str, kind: SpanKind) -> Vec<(usize, usize, String)> {
    let chars: Vec<char> = value.chars().collect();
    let found = match kind {
        SpanKind::Str => sentences(&chars),
        other => span_pattern(other).find_iter(&chars),
    };
    found.into_iter().map(|(start, end)| (start, end, chars[start..end].iter().collect())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_keep_escaped_pipes_and_skip_the_separator() {
        let found: Vec<String> = cells("| a | b\\|c |\n|---|---|\n|  | d |").into_iter().map(|c| c.2).collect();
        assert_eq!(found, ["a", "b\\|c", " ", "d"]);
    }

    #[test]
    fn sentences_end_at_a_stop_or_a_line() {
        let found: Vec<String> = spans("One. Two!\nthree", SpanKind::Str).into_iter().map(|s| s.2).collect();
        assert_eq!(found, ["One.", "Two!", "three"]);
    }

    #[test]
    fn numbers_take_currency_and_thousands() {
        let found: Vec<String> =
            spans("$1,234.50 and -\u{20ac} 3 but 1234,567 x9", SpanKind::Number).into_iter().map(|s| s.2).collect();
        assert_eq!(found, ["$1,234.50", "-\u{20ac} 3"]);
    }
}

/// One word of a text, by character offset, with where the words around it reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Word {
    pub index: usize,
    pub start: usize,
    pub end: usize,
    /// The start of each word, this one's included, so context around it is found by counting words.
    starts: std::sync::Arc<Vec<usize>>,
    ends: std::sync::Arc<Vec<usize>>,
}

impl Word {
    /// Where the word `count` before this one starts.
    pub fn before(&self, count: usize) -> usize {
        self.starts[self.index.saturating_sub(count)]
    }

    /// Where the word `count` after this one ends.
    pub fn after(&self, count: usize) -> usize {
        self.ends[(self.index + count).min(self.ends.len() - 1)]
    }
}

/// `value`'s whitespace-separated words with the punctuation around each trimmed off.
pub(crate) fn words(value: &str) -> Vec<Word> {
    let chars: Vec<char> = value.chars().collect();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut from = None;
    for (at, ch) in chars.iter().chain(std::iter::once(&' ')).enumerate() {
        match (ch.is_whitespace(), from) {
            (false, None) => from = Some(at),
            (true, Some(start)) => {
                let word = &chars[start..at];
                let edge = |ch: &char| ch.is_alphanumeric();
                if let (Some(first), Some(last)) = (word.iter().position(edge), word.iter().rposition(edge)) {
                    spans.push((start + first, start + last + 1));
                }
                from = None;
            }
            _ => {}
        }
    }
    let starts: std::sync::Arc<Vec<usize>> = std::sync::Arc::new(spans.iter().map(|span| span.0).collect());
    let ends: std::sync::Arc<Vec<usize>> = std::sync::Arc::new(spans.iter().map(|span| span.1).collect());
    spans
        .iter()
        .enumerate()
        .map(|(index, &(start, end))| Word { index, start, end, starts: starts.clone(), ends: ends.clone() })
        .collect()
}
