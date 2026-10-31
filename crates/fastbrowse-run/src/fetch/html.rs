//! The rendered text of one served document as the blocks a reader cites, without running it.
//!
//! html5ever's tokenizer reads the tags, the raw-text elements and the character references as a browser's does; the
//! reader turns them into the blocks `capture.js` makes of the same page.

use std::cell::RefCell;

use encoding_rs::{Encoding, UTF_8};
use fastbrowse_core::models::Operation;
use fastbrowse_core::page::{BlockKind, Control};
use fastbrowse_core::py::text;
use html5ever::Attribute;
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{BufferQueue, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts};

/// Scripts, and the elements whose only content is a script or a picture, state nothing a reader can quote.
/// `embed` is void, so it has no content to skip and no end tag that would end the skip.
const SKIPPED: [&str; 10] =
    ["script", "style", "noscript", "template", "svg", "canvas", "iframe", "object", "audio", "video"];
/// Whether two runs of text belong to one block: a block-level tag ends one, an inline tag does not.
const BLOCKS: [&str; 27] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "dd",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "tbody",
    "tfoot",
    "thead",
    "ul",
];
fn heading_level(tag: &str) -> Option<usize> {
    match tag.as_bytes() {
        [b'h', level @ b'1'..=b'6'] => Some(usize::from(level - b'0')),
        _ => None,
    }
}

/// Whether an element's own `style` declares `display: none` or `visibility: hidden`, which a browser never renders.
fn hides(style: &str) -> bool {
    let lower = style.to_ascii_lowercase();
    let declares = |property: &str, value: &str| {
        lower.match_indices(property).any(|(at, _)| {
            let rest = lower[at + property.len()..].trim_start_matches(text::is_space);
            rest.strip_prefix(':').is_some_and(|rest| rest.trim_start_matches(text::is_space).starts_with(value))
        })
    };
    declares("display", "none") || declares("visibility", "hidden")
}

/// Text with the whitespace a browser would collapse, as `capture.js` writes it.
///
/// Quotes are validated against what a reader sees, so the same page has to read the same way whether it was captured
/// from the DOM or fetched: a stray newline from markup would put a value on its own line and break a quote taken from
/// the other reader.
pub fn clean(raw: &str) -> String {
    // `[ \t\r\f\v]+` becomes one space, then ` *\n *` one newline.
    let mut spaced = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            ' ' | '\t' | '\r' | '\u{c}' | '\u{b}' => {
                if !spaced.ends_with(' ') {
                    spaced.push(' ');
                }
            }
            '\n' => {
                while spaced.ends_with(' ') {
                    spaced.pop();
                }
                spaced.push('\n');
            }
            c if c != ' ' && spaced.ends_with("\n ") => {
                spaced.pop();
                spaced.push(c);
            }
            c => spaced.push(c),
        }
    }
    if spaced.ends_with("\n ") {
        spaced.pop();
    }
    // `\n{3,}` becomes two.
    let mut collapsed = String::with_capacity(spaced.len());
    let mut newlines = 0;
    for c in spaced.chars() {
        newlines = if c == '\n' { newlines + 1 } else { 0 };
        if newlines <= 2 {
            collapsed.push(c);
        }
    }
    text::strip(&collapsed).to_string()
}

/// The tokenizer's tokens, handed to a [`Reader`]. A start tag also sets the tokenizer's state, as the tree builder
/// would, so a script's or a title's text is read as text up to its own end tag.
struct Sink<'a>(RefCell<Reader<'a>>);

impl TokenSink for Sink<'_> {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let mut reader = self.0.borrow_mut();
        match token {
            // `<svg/>` is foreign content, where a self-closing tag ends the element: it opens nothing to skip.
            Token::TagToken(tag) if tag.kind == TagKind::StartTag && tag.self_closing && &*tag.name == "svg" => {}
            Token::TagToken(tag) if tag.kind == TagKind::StartTag => {
                reader.start(&tag.name, &tag.attrs);
                return match &*tag.name {
                    "title" | "textarea" => TokenSinkResult::RawData(RawKind::Rcdata),
                    "style" | "xmp" | "iframe" | "noembed" | "noframes" | "noscript" => {
                        TokenSinkResult::RawData(RawKind::Rawtext)
                    }
                    "script" => TokenSinkResult::RawData(RawKind::ScriptData),
                    "plaintext" => TokenSinkResult::Plaintext,
                    _ => TokenSinkResult::Continue,
                };
            }
            Token::TagToken(tag) => reader.end(&tag.name),
            Token::CharacterTokens(data) => reader.data(&data),
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

/// Every token of `html`, handed to `sink` in order.
fn tokenize<S: TokenSink>(html: &str, sink: S) -> S {
    let tokenizer = Tokenizer::new(sink, TokenizerOpts::default());
    let input = BufferQueue::default();
    input.push_back(StrTendril::from(html));
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    tokenizer.sink
}

/// One block of a fetched document, before its offsets are known.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub kind: BlockKind,
    pub text: String,
    pub heading_path: Vec<String>,
    pub href: Option<String>,
}

/// The served markup as blocks. `capture.js` reads the DOM a browser built, so it sees what CSS hid and what scripts
/// added; this sees the served markup only. It keeps the shapes the reader relies on anyway: headings with the path that names
/// what a value under them belongs to, one block per table row under its header, and an inline value left in the block
/// of its label.
struct Reader<'a> {
    url: &'a str,
    blocks: Vec<Found>,
    title: String,
    skip: usize,
    buffer: String,
    kind: BlockKind,
    href: Option<String>,
    headings: Vec<(usize, String)>,
    heading: Option<usize>,
    pre: usize,
    in_title: bool,
    tables: usize,
    rows: Vec<Vec<String>>,
    row: Option<Vec<String>>,
    cell: Option<String>,
    cell_header: bool,
    header_row: Option<usize>,
    hidden: Vec<String>,
    controls: Vec<Control>,
    /// The control whose element is open, which its text labels.
    labelling: Option<(usize, String, String)>,
}

impl<'a> Reader<'a> {
    fn new(url: &'a str) -> Reader<'a> {
        Reader {
            url,
            blocks: Vec::new(),
            title: String::new(),
            skip: 0,
            buffer: String::new(),
            kind: BlockKind::Paragraph,
            href: None,
            headings: Vec::new(),
            heading: None,
            pre: 0,
            in_title: false,
            tables: 0,
            rows: Vec::new(),
            row: None,
            cell: None,
            cell_header: false,
            header_row: None,
            hidden: Vec::new(),
            controls: Vec::new(),
            labelling: None,
        }
    }

    /// The control a start tag makes, as the served markup describes it.
    fn control(&mut self, tag: &str, attrs: &[Attribute]) {
        let get = |name: &str| attrs.iter().find(|a| &*a.name.local == name).map(|a| a.value.to_string());
        let kind = get("type").unwrap_or_default().to_ascii_lowercase();
        let (role, operations): (&str, &[Operation]) = match tag {
            "a" if get("href").is_some_and(|href| !href.is_empty()) => ("link", &[Operation::Click]),
            "button" => ("button", &[Operation::Click]),
            "select" => ("combobox", &[Operation::Select]),
            "textarea" => ("textbox", &[Operation::Fill]),
            "input" => match kind.as_str() {
                "hidden" => return,
                "submit" | "button" | "reset" | "image" => ("button", &[Operation::Click]),
                "checkbox" | "radio" => (kind.as_str(), &[Operation::Click]),
                "file" => ("button", &[Operation::Upload]),
                _ => ("textbox", &[Operation::Fill, Operation::Enter]),
            },
            _ => return,
        };
        let named = ["aria-label", "placeholder", "title", "value", "name"];
        let label = named.iter().find_map(|name| get(name).filter(|value| !clean(value).is_empty()));
        let id = format!("fetched-{}", self.controls.len());
        let mut control = Control::new(&id, role, &clean(&label.unwrap_or_default()), operations.iter().copied());
        control.context = self.headings.last().map(|(_, heading)| heading.clone());
        control.href = get("href").filter(|_| tag == "a").map(|href| join(self.url, &href));
        control.input_type = (tag == "input").then_some(kind.clone()).filter(|kind| !kind.is_empty());
        control.input_name = get("name");
        control.next_page = get("rel").filter(|rel| rel.split_whitespace().any(|rel| rel == "next")).map(|_| true);
        control.download = get("download").map(|_| true);
        if matches!(tag, "a" | "button") {
            self.labelling = Some((self.controls.len(), tag.to_string(), String::new()));
        }
        self.controls.push(control);
    }

    fn start(&mut self, tag: &str, attrs: &[Attribute]) {
        let get = |name: &str| attrs.iter().find(|a| &*a.name.local == name).map(|a| &*a.value);
        if SKIPPED.contains(&tag) {
            self.skip += 1;
            return;
        }
        if self.skip > 0 {
            return;
        }
        if tag == "title" {
            self.in_title = true;
            return;
        }
        if get("hidden").is_some() || hides(get("style").unwrap_or_default()) {
            // A browser never renders this subtree, so a value it holds is not one the user can see or quote.
            self.hidden.push(tag.to_string());
            self.skip += 1;
            return;
        }
        self.control(tag, attrs);
        if tag == "br" {
            self.add("\n");
            return;
        }
        if let Some(level) = heading_level(tag) {
            self.flush(None, None, false);
            self.heading = Some(level);
            self.kind = BlockKind::Heading;
            return;
        }
        if tag == "table" {
            self.flush(None, None, false);
            self.tables += 1;
            if self.tables == 1 {
                self.rows.clear();
                self.header_row = None;
            }
            return;
        }
        if self.tables > 0 {
            self.table_start(tag, get("alt").unwrap_or_default());
            return;
        }
        if tag == "li" {
            self.flush(None, None, false);
            self.kind = BlockKind::ListItem;
            return;
        }
        if tag == "pre" {
            self.flush(None, None, false);
            self.pre += 1;
            return;
        }
        if tag == "img"
            && self.cell.is_none()
            && let Some(alt) = get("alt").filter(|alt| !alt.is_empty())
        {
            // An icon or a flag states its value only in its alt text, which no other reader would quote.
            self.add(&format!(" [{}]", clean(alt)));
            return;
        }
        if tag == "a"
            && let Some(href) = get("href").filter(|href| !href.is_empty())
            && clean(&self.buffer).is_empty()
        {
            // A link whose own text is the whole block keeps its destination, which is the one thing an answer
            // about addresses has to cite.
            self.href = Some(join(self.url, href));
            return;
        }
        if BLOCKS.contains(&tag) {
            self.flush(None, None, false);
        }
    }

    fn end(&mut self, tag: &str) {
        if let Some((index, _, text)) = self.labelling.take_if(|(_, open, _)| open == tag) {
            // What the element says for itself names it before any attribute does.
            let text = clean(&text);
            if !text.is_empty() {
                self.controls[index].label = text;
            }
        }
        if self.hidden.last().is_some_and(|hidden| hidden == tag) {
            self.hidden.pop();
            self.skip = self.skip.saturating_sub(1);
            return;
        }
        if SKIPPED.contains(&tag) {
            self.skip = self.skip.saturating_sub(1);
            return;
        }
        if self.skip > 0 {
            return;
        }
        if tag == "title" {
            self.in_title = false;
            return;
        }
        if heading_level(tag).is_some() {
            self.flush(None, None, true);
            self.heading = None;
            self.kind = BlockKind::Paragraph;
            return;
        }
        if tag == "table" && self.tables > 0 {
            self.close_cell();
            self.tables -= 1;
            if self.tables == 0 {
                self.close_row();
                self.emit_table();
            }
            return;
        }
        if self.tables > 0 {
            if tag == "td" || tag == "th" {
                self.close_cell();
            } else if tag == "tr" {
                self.close_cell();
                self.close_row();
            }
            return;
        }
        if tag == "pre" {
            self.pre = self.pre.saturating_sub(1);
            self.flush(None, None, false);
            return;
        }
        if tag == "li" {
            self.flush(None, None, false);
            self.kind = BlockKind::Paragraph;
            return;
        }
        if tag == "a"
            && let Some(href) = self.href.take()
        {
            self.flush(Some(BlockKind::Link), Some(href), false);
            return;
        }
        if BLOCKS.contains(&tag) {
            self.flush(None, None, false);
        }
    }

    fn data(&mut self, data: &str) {
        if self.in_title {
            self.title.push_str(data);
            return;
        }
        if self.skip > 0 {
            return;
        }
        if let Some((_, _, text)) = &mut self.labelling {
            text.push_str(data);
        }
        self.add(data);
    }

    fn add(&mut self, data: &str) {
        match &mut self.cell {
            Some(cell) => cell.push_str(data),
            None => self.buffer.push_str(data),
        }
    }

    fn table_start(&mut self, tag: &str, alt: &str) {
        if self.tables > 1 {
            return;
        }
        if tag == "tr" {
            self.close_cell();
            self.close_row();
            self.row = Some(Vec::new());
            return;
        }
        if tag == "td" || tag == "th" {
            self.close_cell();
            self.cell = Some(String::new());
            self.cell_header = tag == "th";
            return;
        }
        if tag == "img"
            && !alt.is_empty()
            && let Some(cell) = &mut self.cell
        {
            // A cell can state its value with a picture alone, the way a winner's flag does.
            cell.push_str(&format!(" [{}]", clean(alt)));
        }
    }

    fn close_cell(&mut self) {
        let Some(cell) = self.cell.take() else { return };
        self.row.get_or_insert_with(Vec::new).push(clean(&cell));
        if self.cell_header && self.header_row.is_none() {
            self.header_row = Some(self.rows.len());
        }
    }

    fn close_row(&mut self) {
        if let Some(row) = self.row.take().filter(|row| !row.is_empty()) {
            self.rows.push(row);
        }
    }

    fn emit_table(&mut self) {
        let path = self.path();
        for (index, cells) in std::mem::take(&mut self.rows).into_iter().enumerate() {
            let line = cells.iter().map(|cell| cell.replace('|', "\\|")).collect::<Vec<_>>().join(" | ");
            self.blocks.push(Found {
                kind: BlockKind::Table,
                text: format!("| {line} |"),
                heading_path: path.clone(),
                href: None,
            });
            if index == 0 && self.header_row == Some(0) {
                // Without the separator the row above reads as prose, and a reader cannot tell a column name from a
                // value.
                let separator = vec!["---"; cells.len()].join(" | ");
                self.blocks.push(Found {
                    kind: BlockKind::Table,
                    text: format!("| {separator} |"),
                    heading_path: path.clone(),
                    href: None,
                });
            }
        }
        self.row = None;
        self.header_row = None;
    }

    fn path(&self) -> Vec<String> {
        self.headings.iter().map(|(_, name)| name.clone()).collect()
    }

    fn flush(&mut self, kind: Option<BlockKind>, href: Option<String>, closing_heading: bool) {
        let raw = std::mem::take(&mut self.buffer);
        if let Some(cell) = &mut self.cell {
            // Text of a block inside a cell belongs to that cell's row, not to a block of its own.
            cell.push_str(&raw);
            return;
        }
        // A pre block keeps its line breaks, which are its content; everything else is read as prose.
        let text = if self.pre > 0 {
            if text::strip(&raw).is_empty() {
                String::new()
            } else {
                raw.trim_matches('\n').trim_end_matches(text::is_space).to_string()
            }
        } else {
            clean(&raw)
        };
        if text.is_empty() {
            return;
        }
        let kind = kind.unwrap_or(self.kind);
        self.blocks.push(Found { kind, text: text.clone(), heading_path: self.path(), href });
        if closing_heading && let Some(level) = self.heading {
            // A heading names what follows it, never itself, and a lower heading ends every higher one.
            self.headings.retain(|(higher, _)| *higher < level);
            self.headings.push((level, text));
        }
    }
}

/// `href` resolved against `base` as WHATWG resolves it (docs/adr/0002); an address that does not resolve is kept as
/// written.
pub fn join(base: &str, href: &str) -> String {
    url::Url::parse(base).and_then(|base| base.join(href)).map_or_else(|_| href.to_string(), String::from)
}

/// The document's title and its blocks, in reading order.
pub fn blocks(html: &str, url: &str) -> (String, Vec<Found>) {
    let (title, found, _) = read(html, url);
    (title, found)
}

/// The document's title, its blocks in reading order, and the controls its markup offers.
pub fn read(html: &str, url: &str) -> (String, Vec<Found>, Vec<Control>) {
    let mut reader = tokenize(html, Sink(RefCell::new(Reader::new(url)))).0.into_inner();
    reader.flush(None, None, false);
    (clean(&reader.title), reader.blocks, reader.controls)
}

/// The first `<meta>` charset in a document's first 1024 bytes, which is where the HTML standard's prescan looks for
/// one when the response names none. UTF-16 named here reads as UTF-8, since a document that says so in ASCII is not.
pub fn meta_charset(body: &[u8]) -> Option<&'static Encoding> {
    struct Meta(RefCell<Option<&'static Encoding>>);
    impl TokenSink for Meta {
        type Handle = ();
        fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
            if let Token::TagToken(tag) = token
                && tag.kind == TagKind::StartTag
                && &*tag.name == "meta"
                && self.0.borrow().is_none()
            {
                let get = |name: &str| tag.attrs.iter().find(|a| &*a.name.local == name).map(|a| a.value.to_string());
                let label = get("charset").or_else(|| {
                    let content_type = get("http-equiv").is_some_and(|v| v.eq_ignore_ascii_case("content-type"));
                    content_type.then(|| get("content").and_then(|content| charset_parameter(&content))).flatten()
                });
                *self.0.borrow_mut() = label.and_then(|label| Encoding::for_label(label.as_bytes()));
            }
            TokenSinkResult::Continue
        }
    }
    let head = &body[..body.len().min(1024)];
    let (ascii, _, _) = encoding_rs::WINDOWS_1252.decode(head);
    let found = tokenize(&ascii, Meta(RefCell::new(None))).0.into_inner()?;
    Some(if found == encoding_rs::UTF_16BE || found == encoding_rs::UTF_16LE { UTF_8 } else { found })
}

/// The `charset` parameter of a `Content-Type` value, unquoted.
pub fn charset_parameter(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        name.trim().eq_ignore_ascii_case("charset").then(|| value.trim().trim_matches(['"', '\'']).to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_collapses_what_a_browser_would() {
        assert_eq!(clean("  a \t b \n  c\n\n\n\nd  "), "a b\nc\n\nd");
        assert_eq!(clean("a\u{a0}b"), "a\u{a0}b");
    }

    #[test]
    fn a_row_with_no_closing_tag_still_reaches_the_table() {
        let (_, found) = blocks("<table><tr><td>a<td>1<tr><td>b<td>2</table>", "https://a.example/");
        let texts: Vec<&str> = found.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["| a | 1 |", "| b | 2 |"]);
    }

    #[test]
    fn an_element_hides_itself_only_by_its_own_declaration() {
        assert!(hides("color:red; Display : None"));
        assert!(hides("visibility:hidden"));
        assert!(!hides("display: block"));
    }

    #[test]
    fn raw_text_and_references_read_as_a_browser_reads_them() {
        let (title, found) = blocks(
            "<title>A &amp; <b>B</title><script>if (a<b) x = '&amp;</div>'</SCRIPT >\
             <p data-x=1>x &amp; y &copy</p><!-- c > d --><p>1 < 2</p>",
            "https://a.example/",
        );
        assert_eq!(title, "A & <b>B");
        let texts: Vec<&str> = found.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["x & y \u{a9}", "1 < 2"]);
    }

    #[test]
    fn the_markup_names_its_links_and_fields() {
        let (_, _, controls) = read(
            "<h2>Pages</h2><a href='/b' rel='next'>More <b>items</b></a><a href='/c'><img src=x></a>\
             <div hidden><a href='/secret'>x</a></div><table><tr><td><a href=/d>In a row</a></table>\
             <form><input type=hidden name=t><input name=q placeholder='Search'><button>Go</button></form>",
            "https://a.example/list",
        );
        let shown: Vec<(&str, &str, Option<&str>)> =
            controls.iter().map(|c| (c.role.as_str(), c.label.as_str(), c.href.as_deref())).collect();
        assert_eq!(
            shown,
            [
                ("link", "More items", Some("https://a.example/b")),
                ("link", "", Some("https://a.example/c")),
                ("link", "In a row", Some("https://a.example/d")),
                ("textbox", "Search", None),
                ("button", "Go", None),
            ]
        );
        assert_eq!(controls[0].next_page, Some(true));
        assert_eq!(controls[0].context.as_deref(), Some("Pages"));
    }

    #[test]
    fn a_meta_charset_names_the_encoding() {
        assert_eq!(meta_charset(b"<meta charset=\"shift_jis\"><p>x"), Some(encoding_rs::SHIFT_JIS));
        let equiv = b"<meta http-equiv=Content-Type content='text/html; charset=windows-1251'>";
        assert_eq!(meta_charset(equiv), Some(encoding_rs::WINDOWS_1251));
        assert_eq!(meta_charset(b"<meta charset=utf-16le>"), Some(UTF_8));
        assert_eq!(meta_charset(b"<p>no meta</p>"), None);
    }
}
