//! `fetch.py`'s `_Reader`: the rendered text of one served document as the blocks a reader cites, without running it.
//!
//! The tokenizer is CPython 3.13's `html.parser.HTMLParser` with `convert_charrefs=True`, fed the whole document and
//! closed: the same tags, the same raw-text elements, and the same rule for a construct the document never closes.
//! Character references follow the HTML5 tables, as `html.unescape` does.

use fastbrowse_core::page::BlockKind;
use fastbrowse_core::py::text;

/// Scripts, and the elements whose only content is a script or a picture, state nothing a reader can quote.
const SKIPPED: [&str; 11] =
    ["script", "style", "noscript", "template", "svg", "canvas", "iframe", "object", "embed", "audio", "video"];
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
/// Elements `HTMLParser` reads as raw text up to their own end tag, character references left as written.
const RAW_TEXT: [&str; 6] = ["script", "style", "xmp", "iframe", "noembed", "noframes"];
/// Elements read as text up to their own end tag, character references decoded.
const ESCAPABLE_RAW_TEXT: [&str; 2] = ["textarea", "title"];

fn heading_level(tag: &str) -> Option<usize> {
    match tag.as_bytes() {
        [b'h', level @ b'1'..=b'6'] => Some(usize::from(level - b'0')),
        _ => None,
    }
}

/// `display\s*:\s*none|visibility\s*:\s*hidden`, case-insensitively, anywhere in `style`.
pub(crate) fn hides(style: &str) -> bool {
    let lower = style.to_ascii_lowercase();
    let declares = |property: &str, value: &str| {
        lower.match_indices(property).any(|(at, _)| {
            let rest = lower[at + property.len()..].trim_start_matches(text::is_space);
            rest.strip_prefix(':').is_some_and(|rest| rest.trim_start_matches(text::is_space).starts_with(value))
        })
    };
    declares("display", "none") || declares("visibility", "hidden")
}

/// Class names a stylesheet rule hides, such as `.gone { display: none }`.
///
/// Not a renderer: it reads the plain class rules a page uses to keep a template, an alternate layout or a stale value
/// off the screen, and nothing that depends on a cascade it cannot compute.
pub(crate) fn hidden_classes(rules: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut at = 0;
    // `re.findall(r"([^{}]+)\{([^{}]*)\}", rules)`: a selector runs up to a `{`, and its declaration to the next brace,
    // which has to be a `}`.
    while let Some(offset) = rules[at..].find(['{', '}']) {
        let brace = at + offset;
        if rules[brace..].starts_with('{')
            && brace > at
            && let Some(close) = rules[brace + 1..].find(['{', '}']).map(|offset| brace + 1 + offset)
            && rules[close..].starts_with('}')
        {
            if hides(&rules[brace + 1..close]) {
                names.extend(selector_classes(&rules[at..brace]));
            }
            at = close + 1;
        } else {
            at = brace + 1;
        }
    }
    names
}

/// `re.findall(r"\.([A-Za-z_-][\w-]*)", selector)`.
fn selector_classes(selector: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = selector;
    while let Some(dot) = rest.find('.') {
        rest = &rest[dot + 1..];
        if rest.starts_with(|c: char| c.is_ascii_alphabetic() || matches!(c, '_' | '-')) {
            let end = rest.find(|c: char| !(text::is_word(c) || c == '-')).unwrap_or(rest.len());
            names.push(rest[..end].to_string());
            rest = &rest[end..];
        }
    }
    names
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

/// What `HTMLParser` hands its subclass.
#[derive(Debug, PartialEq)]
enum Token {
    Start {
        tag: String,
        attributes: Vec<(String, Option<String>)>,
    },
    /// `<tag/>`, which `_Reader` reads as a start tag with no end.
    StartEnd {
        tag: String,
        attributes: Vec<(String, Option<String>)>,
    },
    End(String),
    Data(String),
}

fn is_tag_space(c: u8) -> bool {
    matches!(c, b'\t' | b'\n' | b'\r' | b'\x0c' | b' ')
}

/// Where `[^\t\n\r\f />]*` (with `extra` also stopping it) stops, from `at`.
fn run_until(bytes: &[u8], mut at: usize, stop: &[u8]) -> usize {
    while at < bytes.len() && !is_tag_space(bytes[at]) && !stop.contains(&bytes[at]) {
        at += 1;
    }
    at
}

/// One attribute from `at`, as `attrfind_tolerant` reads it: (name, value, where its value ended), or None when no
/// attribute starts here. `value` is None for a bare name and keeps its quotes.
fn attribute(source: &str, at: usize) -> Option<(&str, Option<&str>, usize)> {
    let bytes = source.as_bytes();
    // The name must follow a quote, a space or a slash.
    if at == 0 || at >= bytes.len() || !matches!(bytes[at - 1], b'\'' | b'"' | b'/') && !is_tag_space(bytes[at - 1]) {
        return None;
    }
    if is_tag_space(bytes[at]) || matches!(bytes[at], b'/' | b'>') {
        return None;
    }
    let name_end = run_until(bytes, at + 1, b"/=>");
    let name = &source[at..name_end];
    let mut equals = name_end;
    while equals < bytes.len() && is_tag_space(bytes[equals]) {
        equals += 1;
    }
    if bytes.get(equals) != Some(&b'=') {
        return Some((name, None, name_end));
    }
    let mut start = equals + 1;
    while start < bytes.len() && is_tag_space(bytes[start]) {
        start += 1;
    }
    let end = match bytes.get(start) {
        Some(quote @ (b'\'' | b'"')) => match source[start + 1..].find(char::from(*quote)) {
            Some(offset) => start + 1 + offset + 1,
            // An unclosed quote matches no value at all, and the attribute is its bare name.
            None => return Some((name, None, name_end)),
        },
        _ => {
            let mut end = start;
            while end < bytes.len() && bytes[end] != b'>' && !is_tag_space(bytes[end]) {
                end += 1;
            }
            end
        }
    };
    Some((name, Some(&source[start..end]), end))
}

/// `locatetagend` from `at`, the tag name's first letter: where the tag ends, past its `>` if it has one.
fn tag_end(source: &str, at: usize) -> usize {
    let bytes = source.as_bytes();
    let mut k = run_until(bytes, at + 1, b"/>");
    let skip = |mut k: usize| {
        while k < bytes.len() && (is_tag_space(bytes[k]) || bytes[k] == b'/') {
            k += 1;
        }
        k
    };
    k = skip(k);
    while let Some((_, _, end)) = attribute(source, k) {
        k = skip(end);
    }
    if bytes.get(k) == Some(&b'>') { k + 1 } else { k }
}

/// `(?:[\t\n\r\f ]|/(?!>))*` from `at`.
fn tolerant_space(bytes: &[u8], mut at: usize) -> usize {
    while at < bytes.len() && (is_tag_space(bytes[at]) || bytes[at] == b'/' && bytes.get(at + 1) != Some(&b'>')) {
        at += 1;
    }
    at
}

/// The attribute value as `HTMLParser` reports it: quotes dropped, references decoded where an attribute decodes them.
fn attribute_value(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    let unquoted = match raw.as_bytes() {
        [quote @ (b'\'' | b'"'), .., last] if last == quote => &raw[1..raw.len() - 1],
        [b'\'' | b'"'] => "",
        _ => raw,
    };
    Some(htmlize::unescape_attribute(unquoted).into_owned())
}

/// Every token of `source`, as `HTMLParser.feed(source)` then `close()` hands them over.
fn tokens(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let n = bytes.len();
    let mut out = Vec::new();
    let mut raw_text: Option<(String, bool)> = None;
    let data = |out: &mut Vec<Token>, text: &str, escapable: bool| {
        if !text.is_empty() {
            let text = if escapable { htmlize::unescape(text).into_owned() } else { text.to_string() };
            out.push(Token::Data(text));
        }
    };
    let mut i = 0;
    while i < n {
        let j = match &raw_text {
            None => source[i..].find('<').map_or(n, |offset| i + offset),
            Some((element, _)) => match raw_text_end(source, i, element) {
                Some(j) => j,
                None => break,
            },
        };
        data(&mut out, &source[i..j], raw_text.as_ref().is_none_or(|(_, escapable)| *escapable));
        i = j;
        if i == n {
            break;
        }
        let rest = &source[i..];
        let parsed = if bytes.get(i + 1).is_some_and(u8::is_ascii_alphabetic) {
            start_tag(source, i, &mut out, &mut raw_text)
        } else if rest.starts_with("</") {
            end_tag(source, i, &mut out, &mut raw_text)
        } else if rest.starts_with("<!--") {
            comment(source, i)
        } else if rest.starts_with("<?") {
            source[i + 2..].find('>').map(|offset| i + 2 + offset + 1)
        } else if rest.starts_with("<!") {
            declaration(source, i)
        } else {
            out.push(Token::Data("<".into()));
            Some(i + 1)
        };
        i = match parsed {
            Some(k) => k,
            None => {
                // The document ends inside this construct. Only a bare `</` is kept as text; anything else is a tag,
                // a comment or a declaration that never closed, and is dropped with the rest of the document.
                if rest == "</" {
                    out.push(Token::Data("</".into()));
                }
                n
            }
        };
    }
    if i < n {
        let escapable = raw_text.as_ref().is_none_or(|(_, escapable)| *escapable);
        data(&mut out, &source[i..], escapable);
    }
    out
}

/// Where `</element` followed by a tag space, `/` or `>` begins, from `at`, ignoring ASCII case.
fn raw_text_end(source: &str, at: usize, element: &str) -> Option<usize> {
    // Nothing ends `<plaintext>`: the rest of the document is its text.
    if element == "plaintext" {
        return None;
    }
    let bytes = source.as_bytes();
    let mut from = at;
    while let Some(offset) = source[from..].find("</") {
        let open = from + offset;
        let name_end = open + 2 + element.len();
        if bytes.get(open + 2..name_end).is_some_and(|name| name.eq_ignore_ascii_case(element.as_bytes()))
            && bytes.get(name_end).is_some_and(|c| is_tag_space(*c) || matches!(c, b'/' | b'>'))
        {
            return Some(open);
        }
        from = open + 2;
    }
    None
}

fn start_tag(source: &str, i: usize, out: &mut Vec<Token>, raw_text: &mut Option<(String, bool)>) -> Option<usize> {
    let bytes = source.as_bytes();
    let end = tag_end(source, i + 1);
    if bytes[end - 1] != b'>' {
        return None;
    }
    let name_end = run_until(bytes, i + 2, b"/>");
    let tag = source[i + 1..name_end].to_ascii_lowercase();
    let mut k = tolerant_space(bytes, name_end);
    let mut attributes = Vec::new();
    while k < end {
        let Some((name, value, value_end)) = attribute(source, k) else { break };
        attributes.push((name.to_lowercase(), attribute_value(value)));
        k = tolerant_space(bytes, value_end);
    }
    let rest = text::strip(&source[k..end]);
    if rest != ">" && rest != "/>" {
        out.push(Token::Data(source[i..end].to_string()));
        return Some(end);
    }
    if rest == "/>" {
        out.push(Token::StartEnd { tag, attributes });
    } else {
        if RAW_TEXT.contains(&tag.as_str()) || tag == "plaintext" {
            *raw_text = Some((tag.clone(), false));
        } else if ESCAPABLE_RAW_TEXT.contains(&tag.as_str()) {
            *raw_text = Some((tag.clone(), true));
        }
        out.push(Token::Start { tag, attributes });
    }
    Some(end)
}

fn end_tag(source: &str, i: usize, out: &mut Vec<Token>, raw_text: &mut Option<(String, bool)>) -> Option<usize> {
    let bytes = source.as_bytes();
    let close = source[i + 2..].find('>').map(|offset| i + 2 + offset)?;
    if !bytes.get(i + 2).is_some_and(u8::is_ascii_alphabetic) {
        // `</>` is dropped; anything else that is not a tag name is a bogus comment up to the next `>`.
        return Some(close + 1);
    }
    let end = tag_end(source, i + 2);
    if bytes[end - 1] != b'>' {
        return None;
    }
    let name_end = run_until(bytes, i + 3, b"/>");
    out.push(Token::End(source[i + 2..name_end].to_ascii_lowercase()));
    *raw_text = None;
    Some(end)
}

fn comment(source: &str, i: usize) -> Option<usize> {
    let body = &source[i + 4..];
    let mut from = 0;
    while let Some(offset) = body[from..].find("--") {
        let at = from + offset + 2;
        if body[at..].starts_with('>') {
            return Some(i + 4 + at + 1);
        }
        if body[at..].starts_with("!>") {
            return Some(i + 4 + at + 2);
        }
        from = from + offset + 1;
    }
    // `<!-->` and `<!--->` close at once.
    if body.starts_with('>') {
        return Some(i + 5);
    }
    if body.starts_with("->") {
        return Some(i + 6);
    }
    None
}

fn declaration(source: &str, i: usize) -> Option<usize> {
    if source[i..].starts_with("<![CDATA[") {
        return source[i + 9..].find("]]>").map(|offset| i + 9 + offset + 3);
    }
    if source.as_bytes().get(i..i + 9).is_some_and(|head| head.eq_ignore_ascii_case(b"<!doctype")) {
        return source[i + 9..].find('>').map(|offset| i + 9 + offset + 1);
    }
    source[i + 2..].find('>').map(|offset| i + 2 + offset + 1)
}

/// One block of a fetched document, before its offsets are known.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub kind: BlockKind,
    pub text: String,
    pub heading_path: Vec<String>,
    pub href: Option<String>,
}

/// `fetch.py`'s `_Reader`. `capture.js` reads the DOM a browser built, so it sees what CSS hid and what scripts added;
/// this sees the served markup only. It keeps the shapes the reader relies on anyway: headings with the path that names
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
    hidden_classes: Vec<String>,
    style: Option<String>,
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
            hidden_classes: Vec::new(),
            style: None,
        }
    }

    fn start(&mut self, tag: &str, attrs: &[(String, Option<String>)]) {
        // `dict(attrs)`: a repeated attribute keeps its last value.
        let get = |name: &str| {
            attrs.iter().rev().find(|(key, _)| key == name).map(|(_, value)| value.as_deref().unwrap_or_default())
        };
        if tag == "style" {
            // Kept rather than skipped: a rule that hides a class is the page saying a subtree is not shown.
            self.style = Some(String::new());
            self.skip += 1;
            return;
        }
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
        if get("hidden").is_some()
            || hides(get("style").unwrap_or_default())
            || text::split(get("class").unwrap_or_default()).any(|class| self.hidden_classes.iter().any(|h| h == class))
        {
            // A browser never renders this subtree, so a value it holds is not one the user can see or quote.
            self.hidden.push(tag.to_string());
            self.skip += 1;
            return;
        }
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
        if tag == "style"
            && let Some(rules) = self.style.take()
        {
            self.skip = self.skip.saturating_sub(1);
            self.hidden_classes.extend(hidden_classes(&rules));
            return;
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
        if let Some(style) = &mut self.style {
            style.push_str(data);
            return;
        }
        if self.in_title {
            self.title.push_str(data);
            return;
        }
        if self.skip > 0 {
            return;
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

/// `urljoin(base, href)`, as WHATWG resolves it (docs/adr/0002); an address that does not resolve is kept as written.
pub fn join(base: &str, href: &str) -> String {
    url::Url::parse(base).and_then(|base| base.join(href)).map_or_else(|_| href.to_string(), String::from)
}

/// The document's title and its blocks, in reading order.
pub fn blocks(html: &str, url: &str) -> (String, Vec<Found>) {
    let mut reader = Reader::new(url);
    for token in tokens(html) {
        match token {
            Token::Start { tag, attributes } | Token::StartEnd { tag, attributes } => reader.start(&tag, &attributes),
            Token::End(tag) => reader.end(&tag),
            Token::Data(data) => reader.data(&data),
        }
    }
    reader.flush(None, None, false);
    (clean(&reader.title), reader.blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(tag: &str, attributes: &[(&str, Option<&str>)]) -> Token {
        Token::Start {
            tag: tag.into(),
            attributes: attributes.iter().map(|(k, v)| (k.to_string(), v.map(String::from))).collect(),
        }
    }

    #[test]
    fn tokens_follow_html_parser() {
        assert_eq!(
            tokens("<P Class='a b' data-x=1 hidden>x &amp; y</p>"),
            vec![
                start("p", &[("class", Some("a b")), ("data-x", Some("1")), ("hidden", None)]),
                Token::Data("x & y".into()),
                Token::End("p".into()),
            ]
        );
        // A script's body is raw text up to its own end tag, references and all.
        assert_eq!(
            tokens("<script>if (a<b) x = '&amp;</div>'</SCRIPT >z"),
            vec![
                start("script", &[]),
                Token::Data("if (a<b) x = '&amp;</div>'".into()),
                Token::End("script".into()),
                Token::Data("z".into()),
            ]
        );
        // A title decodes its references but reads no tags.
        assert_eq!(
            tokens("<title>A &amp; <b>B</title>"),
            vec![start("title", &[]), Token::Data("A & <b>B".into()), Token::End("title".into())]
        );
        assert_eq!(
            tokens("a <!-- c > d --> b <!doctype html><br/>1 < 2"),
            vec![
                Token::Data("a ".into()),
                Token::Data(" b ".into()),
                Token::StartEnd { tag: "br".into(), attributes: vec![] },
                Token::Data("1 ".into()),
                Token::Data("<".into()),
                Token::Data(" 2".into()),
            ]
        );
        // A quoted value can hold `>`, and an attribute reference decodes only as an exact name.
        assert_eq!(
            tokens("<a href=\"/q?a=1&amp;b=2&ampc=3\" title='x>y'>"),
            vec![start("a", &[("href", Some("/q?a=1&b=2&ampc=3")), ("title", Some("x>y"))])]
        );
        // A tag the document never closes is dropped with the rest of it.
        assert_eq!(tokens("text<a href='x"), vec![Token::Data("text".into())]);
    }

    #[test]
    fn clean_collapses_what_a_browser_would() {
        assert_eq!(clean("  a \t b \n  c\n\n\n\nd  "), "a b\nc\n\nd");
        assert_eq!(clean("a\u{a0}b"), "a\u{a0}b");
    }

    #[test]
    fn a_class_is_hidden_only_by_a_rule_that_hides_it() {
        assert_eq!(hidden_classes(".gone, p.x-y { Display : None } .kept { color: red }"), ["gone", "x-y"]);
        assert_eq!(hidden_classes("@media print { .a { visibility:hidden } }"), ["a"]);
        assert!(hides("color:red; display: none"));
        assert!(!hides("display: block"));
    }
}
