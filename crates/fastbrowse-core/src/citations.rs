//! Port of `src/fastbrowse/citations.py`: WICG text-fragment deep links derived from captured quotes.

use std::fmt;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use crate::page::{BlockKind, Capture};

/// Everything but ASCII letters, digits and `_.~`. `-`, `,` and `&` are text-directive syntax, so they are encoded
/// too, along with everything else a URL fragment might reinterpret.
const TERM: &AsciiSet = &NON_ALPHANUMERIC.remove(b'_').remove(b'.').remove(b'~');

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyQuote;

impl fmt::Display for EmptyQuote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("A text fragment needs a nonempty quote")
    }
}

impl std::error::Error for EmptyQuote {}

/// `url` with its text directive replaced by one that highlights `text`; any `#anchor` is kept.
///
/// Words split on Unicode White_Space. Long quotes keep five whole words at each end, without overlap.
pub fn text_fragment(url: &str, text: &str) -> Result<String, EmptyQuote> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return Err(EmptyQuote);
    }
    let length = words.iter().map(|w| w.chars().count()).sum::<usize>() + words.len() - 1;
    let terms = if length > 120 && words.len() > 10 {
        vec![words[..5].join(" "), words[words.len() - 5..].join(" ")]
    } else {
        vec![words.join(" ")]
    };
    let directive: Vec<String> = terms.iter().map(|t| utf8_percent_encode(t, TERM).to_string()).collect();
    let (base, fragment) = url.split_once('#').unwrap_or((url, ""));
    let anchor = fragment.split_once(":~:").map_or(fragment, |(a, _)| a);
    Ok(format!("{base}#{anchor}:~:text={}", directive.join(",")))
}

/// Where a capture's table block keeps its own row, as offsets into `Capture.text` in code points.
///
/// A capture writes each table row as a block of its own that repeats what identifies the row: the header rows and
/// their `| --- |` divider, or a headerless table's first row. `row` is where the block's own row starts, so
/// `start..row` is that repeated prefix, and `divider` is the divider's span when the block has one. A table with
/// no data rows is one block of header rows and divider, with `row` at its end. `repeats` is set when the prefix
/// repeats the block before, so on the page it sits above an earlier row, not above this one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableBlock {
    pub start: i64,
    pub end: i64,
    pub row: i64,
    pub divider: Option<(i64, i64)>,
    pub repeats: bool,
}

/// The table blocks of `capture`, in order. A block that is only a divider, as a fetched page writes the one under a
/// lone header row opening the table, is all divider. A downloaded CSV is not one, so its quote stays as cut.
pub fn table_blocks(capture: &Capture) -> Vec<TableBlock> {
    let chars: Vec<char> = capture.text.chars().collect();
    let text = |start: i64, end: i64| -> String {
        let clamp = |at: i64| (at.max(0) as usize).min(chars.len());
        chars[clamp(start)..clamp(end).max(clamp(start))].iter().collect::<String>().trim_end().to_owned()
    };
    let mut tables: Vec<TableBlock> = Vec::new();
    let mut previous: Option<usize> = None;
    // How many blocks of the same table come before this one.
    let mut run = 0;
    for (index, block) in capture.blocks.iter().enumerate() {
        // A downloaded CSV is the file's own text, not a capture's markdown, so its pipes are the file's.
        if block.kind != BlockKind::Table || block.source_id.starts_with("download/") {
            continue;
        }
        run = if previous == Some(index.wrapping_sub(1)) { run + 1 } else { 0 };
        // A cell's own pipes are escaped, so a line opening with a pipe is always the start of a row.
        let rows: Vec<i64> = (block.start..block.end)
            .filter(|&at| {
                chars.get(at as usize) == Some(&'|') && (at == block.start || chars.get(at as usize - 1) == Some(&'\n'))
            })
            .collect();
        let last = rows.last().copied().unwrap_or(block.start);
        // The block before, when it is the same table's: a prefix repeating its prefix, or a headerless table's
        // first row, is not this row's own header.
        let before = tables.last().filter(|_| previous == Some(index.wrapping_sub(1)));
        let repeats = |prefix: &str| {
            !prefix.is_empty()
                && before.is_some_and(|table| {
                    text(table.start, table.row) == prefix
                        || (table.row == table.start && text(table.start, table.end) == prefix)
                })
        };
        let ends_in_divider = divider_line(&text(last, block.end));
        let (row, divider) = if rows.len() > 2 && divider_line(&text(rows[rows.len() - 2], last)) {
            // The divider sits between the header rows and the row, so a prefix of one row is a headerless
            // table's first row, whatever its cells hold.
            (last, Some((rows[rows.len() - 2], last)))
        } else if rows.len() > 1 && ends_in_divider && !repeats(&text(block.start, last)) {
            // A table with no data rows: its header rows and the divider under them.
            (block.end, Some((last, block.end)))
        } else if rows.len() == 1
            && run == 1
            && before.is_some_and(|table| table.row == table.start && table.divider.is_none())
            && ends_in_divider
        {
            (last, Some((block.start, block.end)))
        } else {
            (last, None)
        };
        tables.push(TableBlock {
            start: block.start,
            end: block.end,
            row,
            divider,
            repeats: repeats(&text(block.start, row)),
        });
        previous = Some(index);
    }
    tables
}

/// `| --- | --- |`, the divider a capture writes under a table's header.
fn divider_line(line: &str) -> bool {
    let line = line.trim_end_matches(['\n', '\r']);
    line.strip_prefix("| ")
        .and_then(|inner| inner.strip_suffix(" |"))
        .is_some_and(|inner| inner.split(" | ").all(|cell| cell == "---"))
}

/// The page's own text for `quote`, the span of a capture starting at `start`, given the capture's `tables`.
///
/// A capture writes a table row as `| cell | cell |`, escapes a pipe inside a cell as `\|`, and repeats the header
/// and its `| --- |` divider before every row. None of that is on the page, so a citation showing it, or a text
/// fragment built from it, matches nothing there. Within a table block the cells are kept, separated by a space
/// within a row and a line break between rows, and the divider is dropped. A header or first row repeated before a
/// later row is dropped too, as the page shows it only above the table's first row; the quote stays text that runs
/// on the page. Text outside every table block is the page's own and is kept as it is, pipes and backslashes
/// included.
pub fn page_text(quote: &str, start: i64, tables: &[TableBlock]) -> String {
    let chars: Vec<char> = quote.chars().collect();
    let end = start + chars.len() as i64;
    let text = |from: i64, to: i64| -> String {
        let clamp = |at: i64| (at - start).clamp(0, chars.len() as i64) as usize;
        chars[clamp(from)..clamp(to).max(clamp(from))].iter().collect()
    };
    let mut shown = String::new();
    let mut at = start;
    for table in tables.iter().filter(|table| table.start < end && table.end > start) {
        let whole_divider = table.divider == Some((table.start, table.end));
        // A divider written as a block of its own leaves no blank line where it was.
        if !whole_divider {
            shown.push_str(&text(at, table.start));
        }
        let mut parts = Vec::new();
        if !table.repeats {
            let (cut, resume) = table.divider.unwrap_or((table.row, table.row));
            parts.push(cells(&(text(table.start, cut) + &text(resume, table.row))));
        }
        if !whole_divider {
            parts.push(cells(&text(table.row, table.end)));
        }
        shown.push_str(&parts.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n"));
        at = table.end.max(at);
    }
    if at == start {
        return quote.to_owned();
    }
    shown.push_str(&text(at, end));
    shown
}

/// One fragment of a table's markdown as its cells: every unescaped pipe is a border, `\|` is the pipe the page
/// shows, and a line break between borders ends a row.
fn cells(markdown: &str) -> String {
    let mut cells = Cells::default();
    let mut chars = markdown.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => cells.cell.push(chars.next().unwrap_or('|')),
            '|' => cells.close(),
            _ => cells.cell.push(c),
        }
    }
    cells.close();
    cells.text
}

/// The text read so far, the cell still open, and whether a row ended since the last cell with text.
#[derive(Default)]
struct Cells {
    text: String,
    cell: String,
    new_row: bool,
}

impl Cells {
    fn close(&mut self) {
        let shown = self.cell.trim();
        if shown.is_empty() {
            // Between `|` and `|` of the next row there is only the line break.
            self.new_row |= self.cell.contains('\n');
        } else {
            if !self.text.is_empty() {
                self.text.push(if self.new_row { '\n' } else { ' ' });
            }
            self.text.push_str(shown);
            self.new_row = false;
        }
        self.cell.clear();
    }
}

#[cfg(test)]
mod tests {
    //! Mirrors tests/test_citations.py.
    use super::*;
    use percent_encoding::percent_decode_str;

    /// A capture of `blocks`, each `(is a table, text)`, one per line.
    fn captured(blocks: &[(bool, &str)]) -> Capture {
        let mut text = String::new();
        let mut spans = Vec::new();
        for (index, (table, block)) in blocks.iter().enumerate() {
            if index > 0 {
                text.push('\n');
            }
            let start = text.chars().count();
            text.push_str(block);
            let kind = if *table { "table" } else { "paragraph" };
            spans.push(serde_json::json!({
                "source_id": format!("s{index}"), "kind": kind, "frame_id": null, "start": start,
                "end": text.chars().count(),
            }));
        }
        serde_json::from_value(serde_json::json!({
            "url": "https://e.test", "title": "", "captured_at": "2026-01-01T00:00:00Z", "sha256": "sha",
            "text": text, "blocks": spans,
        }))
        .unwrap()
    }

    /// `page_text` of the capture's text from block `first` to block `last`, inclusive.
    fn shown(capture: &Capture, first: usize, last: usize) -> String {
        let (start, end) = (capture.blocks[first].start, capture.blocks[last].end);
        let quote: String = capture.text.chars().skip(start as usize).take((end - start) as usize).collect();
        page_text(&quote, start, &table_blocks(capture))
    }

    #[test]
    fn a_table_row_is_its_cells_with_the_page_own_pipes() {
        let capture = captured(&[(true, "| Hacker Newsnew \\| past \\| comments | login |")]);
        assert_eq!(shown(&capture, 0, 0), "Hacker Newsnew | past | comments login");
        let capture = captured(&[(true, "| Rank | Story |\n| 1.\nAI is forcing us | (ieee.org) |")]);
        assert_eq!(shown(&capture, 0, 0), "Rank Story\n1.\nAI is forcing us (ieee.org)");
    }

    #[test]
    fn a_span_over_several_rows_shows_the_header_once() {
        let capture = captured(&[
            (true, "| Name | Score |\n| --- | --- |\n| Ada | 10 |"),
            (true, "| Name | Score |\n| --- | --- |\n| Grace | 9 |"),
        ]);
        assert_eq!(shown(&capture, 0, 1), "Name Score\nAda 10\nGrace 9");
        // A span starting at a later row starts at that row, as the header is above the first row only.
        assert_eq!(shown(&capture, 1, 1), "Grace 9");
        let headerless = captured(&[(true, "| Ada | 10 |"), (true, "| Ada | 10 |\n| Grace | 9 |")]);
        assert_eq!(shown(&headerless, 0, 1), "Ada 10\nGrace 9");
        assert_eq!(shown(&headerless, 1, 1), "Grace 9");
    }

    #[test]
    fn a_table_with_no_data_rows_is_its_header() {
        assert_eq!(shown(&captured(&[(true, "| Name |\n| --- |")]), 0, 0), "Name");
        let capture = captured(&[(true, "| Team | Score |\n| Name | Points |\n| --- | --- |")]);
        assert_eq!(shown(&capture, 0, 0), "Team Score\nName Points");
        // A headerless table's row of dashes is still a row.
        let headerless = captured(&[(true, "| Name |"), (true, "| Name |\n| --- |")]);
        assert_eq!(shown(&headerless, 0, 1), "Name\n---");
    }

    #[test]
    fn a_row_of_dashes_is_a_row() {
        let capture = captured(&[
            (true, "| Name | Score |\n| --- | --- |\n| --- | --- |"),
            (true, "| --- | --- |\n| Ada | 10 |"),
        ]);
        assert_eq!(shown(&capture, 0, 0), "Name Score\n--- ---");
        assert_eq!(shown(&capture, 1, 1), "--- ---\nAda 10");
        // A fetched page writes the divider under a lone header row as a block of its own.
        let fetched = captured(&[(true, "| Name |"), (true, "| --- |"), (true, "| --- |")]);
        assert_eq!(shown(&fetched, 0, 2), "Name\n---");
    }

    #[test]
    fn prose_keeps_its_pipes_and_backslashes() {
        let capture =
            captured(&[(false, "| not a row | a \\| b"), (true, "| Dune | Frank Herbert |"), (false, "x | y")]);
        assert_eq!(shown(&capture, 0, 2), "| not a row | a \\| b\nDune Frank Herbert\nx | y");
        assert_eq!(shown(&capture, 0, 0), "| not a row | a \\| b");
        let quote = "| Dune | Frank Herbert |";
        assert_eq!(page_text(quote, 0, &[]), quote, "with no capture the quote stands as cut");
    }

    #[test]
    fn a_table_quote_links_to_text_on_the_page() {
        let capture = captured(&[(true, "| Dune | Frank Herbert |")]);
        assert_eq!(
            text_fragment("https://e.test", &shown(&capture, 0, 0)).unwrap(),
            "https://e.test#:~:text=Dune%20Frank%20Herbert"
        );
    }

    #[test]
    fn text_fragment_encodes_terms() {
        for (text, encoded) in [
            ("a-b & c,d", "a%2Db%20%26%20c%2Cd"),
            ("caf\u{e9} \u{6771}\u{4eac}", "caf%C3%A9%20%E6%9D%B1%E4%BA%AC"),
            ("100% #1 / (yes?)", "100%25%20%231%20%2F%20%28yes%3F%29"),
            ("  a\n\tb\u{a0}c  ", "a%20b%20c"),
        ] {
            assert_eq!(
                text_fragment("https://example.test/a?x=1&y=2", text).unwrap(),
                format!("https://example.test/a?x=1&y=2#:~:text={encoded}")
            );
        }
    }

    #[test]
    fn text_fragment_preserves_anchor_and_replaces_old_directive() {
        for fragment in ["#section", "#section:~:text=old", "#section:~:text=old&text=other"] {
            assert_eq!(
                text_fragment(&format!("https://example.test/{fragment}"), "new").unwrap(),
                "https://example.test/#section:~:text=new"
            );
        }
    }

    #[test]
    fn long_quotes_use_nonoverlapping_whole_words_at_both_ends() {
        let words: Vec<String> = (0..30).map(|n| format!("word-{n},")).collect();
        let link = text_fragment("https://example.test", &words.join(" ")).unwrap();
        let (start, end) = link.split("text=").nth(1).unwrap().split_once(',').unwrap();
        assert_eq!(percent_decode_str(start).decode_utf8().unwrap(), words[..5].join(" "));
        assert_eq!(percent_decode_str(end).decode_utf8().unwrap(), words[25..].join(" "));
        assert!(start.contains("%2D") && end.contains("%2C"));
        let long_word = "a".repeat(121);
        assert!(text_fragment("https://example.test#", &long_word).unwrap().ends_with(&format!("text={long_word}")));
    }

    #[test]
    fn empty_quotes_cannot_make_a_text_fragment() {
        assert_eq!(text_fragment("https://example.test", " \n"), Err(EmptyQuote));
    }

    /// Unicode White_Space, not CPython's `str.isspace`: the information separators U+001C..U+001F are not
    /// whitespace, so they stay in the word and are encoded.
    #[test]
    fn words_split_on_unicode_white_space() {
        assert_eq!(text_fragment("https://e.test", "a\u{1c}b").unwrap(), "https://e.test#:~:text=a%1Cb");
        assert_eq!(text_fragment("https://e.test", "a\u{3000}b\u{2028}c").unwrap(), "https://e.test#:~:text=a%20b%20c");
    }
}
