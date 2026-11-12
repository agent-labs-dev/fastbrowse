//! A JSON document read as the blocks its structure gives it.
//!
//! Read as one passage, a JSON response offers no value on its own: `"name":"httpx",` has no space inside it, so a
//! choice among a passage's words cannot copy the name without its neighbours (#1069). A document is read here the way
//! an HTML page is: each object key is a heading and each scalar its own block under the path of keys and array
//! indices that leads to it, so a value is quoted, cited and told apart by where it sits.
//!
//! An object that is an item of an array is one item the document repeats, as a list row is on a page: it is one
//! record block under its path, with a line for each value in it, written as the keys and indices that lead to it from
//! the item, a colon and the value. Read leaf by leaf, PyPI's release files were thousands of notes, and a comparison
//! across them could not be judged.
//!
//! A string reads as its decoded text, a number as it is written, so no digit of a large or exponent number is lost,
//! and `true`, `false` and `null` as those words. An empty object or array reads as `{}` or `[]`. Members keep their
//! order and a repeated key keeps every value. A document that is not entirely valid JSON is no structure, and its
//! caller reads it whole.

use fastbrowse_core::page::BlockKind;
use fastbrowse_core::py::text;

use super::html::Found;

/// The blocks of `body`, when all of it is one JSON value.
pub fn blocks(body: &str) -> Option<Vec<Found>> {
    let mut reader = Reader { bytes: body.as_bytes(), at: 0 };
    let mut found = Vec::new();
    let mut path: Vec<String> = Vec::new();
    // The containers open around the next value, with the index of an array's current item. Kept as a list rather
    // than a call stack, so a deeply nested document cannot overflow one.
    let mut open: Vec<Option<usize>> = Vec::new();
    // The array item being read as a record: the containers open around its members, its path, and its lines.
    let mut record: Option<(usize, Vec<String>, Vec<String>)> = None;
    'value: loop {
        reader.space();
        let leaf = match reader.peek()? {
            b'{' => {
                reader.at += 1;
                reader.space();
                if reader.eat(b'}') {
                    "{}".to_string()
                } else {
                    if record.is_none() && matches!(open.last(), Some(Some(_))) {
                        record = Some((open.len() + 1, path.clone(), Vec::new()));
                    }
                    open.push(None);
                    reader.key(&mut path, &mut found, record.is_none())?;
                    continue 'value;
                }
            }
            b'[' => {
                reader.at += 1;
                reader.space();
                if reader.eat(b']') {
                    "[]".to_string()
                } else {
                    open.push(Some(0));
                    path.push("0".into());
                    continue 'value;
                }
            }
            b'"' => reader.string()?,
            b't' => reader.word("true")?,
            b'f' => reader.word("false")?,
            b'n' => reader.word("null")?,
            _ => reader.number()?,
        };
        match &mut record {
            Some((_, at, lines)) => {
                let within = path[at.len()..].join(".");
                lines.push(if text::strip(&leaf).is_empty() {
                    format!("[{within}: no text]")
                } else {
                    format!("{within}: {leaf}")
                });
            }
            None => push(&mut found, BlockKind::Paragraph, leaf, &path),
        }
        // The value is done: close every container it ends, then move to the next member or item.
        loop {
            reader.space();
            let Some(container) = open.last_mut() else {
                return (reader.at == reader.bytes.len()).then_some(found);
            };
            path.pop();
            match (reader.next()?, container) {
                (b',', None) => {
                    reader.key(&mut path, &mut found, record.is_none())?;
                    continue 'value;
                }
                (b',', Some(index)) => {
                    *index += 1;
                    path.push(index.to_string());
                    continue 'value;
                }
                (b'}', None) | (b']', Some(_)) => {
                    open.pop();
                    if record.as_ref().is_some_and(|(depth, _, _)| *depth == open.len() + 1) {
                        let (_, at, lines) = record.take().expect("a record is open");
                        found.push(Found { whole_record: true, ..block(BlockKind::Record, lines.join("\n"), &at) });
                    }
                }
                _ => return None,
            }
        }
    }
}

/// A block of `text` under `path`. Text with nothing but space in it is no block, as an empty element is none.
fn push(found: &mut Vec<Found>, kind: BlockKind, text: String, path: &[String]) {
    if !text::strip(&text).is_empty() {
        found.push(block(kind, text, path));
    }
}

fn block(kind: BlockKind, text: String, path: &[String]) -> Found {
    Found {
        kind,
        text,
        heading_path: path.to_vec(),
        href: None,
        links: Vec::new(),
        frame_id: None,
        whole_record: false,
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.at += 1;
        Some(byte)
    }

    fn eat(&mut self, byte: u8) -> bool {
        let matched = self.peek() == Some(byte);
        self.at += usize::from(matched);
        matched
    }

    /// JSON's own whitespace: space, tab, line feed and carriage return.
    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    /// An object member's key and its colon: the key is a heading under the object's path, unless a record's line
    /// names it, and the path then leads through it to the member's value.
    fn key(&mut self, path: &mut Vec<String>, found: &mut Vec<Found>, heading: bool) -> Option<()> {
        self.space();
        if self.peek()? != b'"' {
            return None;
        }
        let key = self.string()?;
        self.space();
        if !self.eat(b':') {
            return None;
        }
        if heading {
            push(found, BlockKind::Heading, key.clone(), path);
        }
        path.push(key);
        Some(())
    }

    fn word(&mut self, word: &str) -> Option<String> {
        let end = self.at + word.len();
        (self.bytes.get(self.at..end)? == word.as_bytes()).then(|| {
            self.at = end;
            word.to_string()
        })
    }

    /// A number as it is written: `-`, an integer part with no leading zero, then an optional fraction and exponent.
    fn number(&mut self) -> Option<String> {
        let start = self.at;
        self.eat(b'-');
        match self.next()? {
            b'0' => {}
            b'1'..=b'9' => self.digits(0)?,
            _ => return None,
        }
        if self.eat(b'.') {
            self.digits(1)?;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            self.digits(1)?;
        }
        // The slice is ASCII digits and signs, so it is whole characters.
        Some(String::from_utf8_lossy(&self.bytes[start..self.at]).into_owned())
    }

    /// At least `least` ASCII digits.
    fn digits(&mut self, least: usize) -> Option<()> {
        let start = self.at;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        (self.at - start >= least).then_some(())
    }

    /// A string's decoded text. A control character, an unknown escape or a lone surrogate makes it no JSON string.
    fn string(&mut self) -> Option<String> {
        self.at += 1;
        let mut out = String::new();
        loop {
            let start = self.at;
            while !matches!(self.peek()?, b'"' | b'\\' | 0x00..=0x1f) {
                self.at += 1;
            }
            // The run stops at an ASCII byte, so it ends on a character boundary of the source text.
            out.push_str(std::str::from_utf8(&self.bytes[start..self.at]).ok()?);
            match self.next()? {
                b'"' => return Some(out),
                b'\\' => out.push(self.escape()?),
                _ => return None,
            }
        }
    }

    fn escape(&mut self) -> Option<char> {
        Some(match self.next()? {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let unit = self.unit()?;
                match unit {
                    0xd800..=0xdbff => {
                        if !(self.eat(b'\\') && self.eat(b'u')) {
                            return None;
                        }
                        let low = self.unit()?;
                        if !(0xdc00..=0xdfff).contains(&low) {
                            return None;
                        }
                        char::from_u32(0x10000 + ((unit - 0xd800) << 10) + (low - 0xdc00))?
                    }
                    0xdc00..=0xdfff => return None,
                    _ => char::from_u32(unit)?,
                }
            }
            _ => return None,
        })
    }

    /// The four hex digits of a `\u` escape.
    fn unit(&mut self) -> Option<u32> {
        let hex = std::str::from_utf8(self.bytes.get(self.at..self.at + 4)?).ok()?;
        if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        self.at += 4;
        u32::from_str_radix(hex, 16).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::blocks;
    use fastbrowse_core::page::BlockKind;

    fn read(body: &str) -> Vec<(BlockKind, String, Vec<String>)> {
        blocks(body).expect("valid JSON").into_iter().map(|b| (b.kind, b.text, b.heading_path)).collect()
    }

    fn path(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    #[test]
    fn each_key_heads_its_value_and_each_scalar_is_its_own_block() {
        let found = read(r#"{"info":{"name":"httpx","enabled":true},"releases":[{"name":"beta"},7,null]}"#);
        let (h, p) = (BlockKind::Heading, BlockKind::Paragraph);
        assert_eq!(
            found,
            [
                (h, "info".into(), path(&[])),
                (h, "name".into(), path(&["info"])),
                (p, "httpx".into(), path(&["info", "name"])),
                (h, "enabled".into(), path(&["info"])),
                (p, "true".into(), path(&["info", "enabled"])),
                (h, "releases".into(), path(&[])),
                (BlockKind::Record, "name: beta".into(), path(&["releases", "0"])),
                (p, "7".into(), path(&["releases", "1"])),
                (p, "null".into(), path(&["releases", "2"])),
            ]
        );
    }

    #[test]
    fn an_array_item_object_is_one_whole_record_of_every_value_in_it() {
        let body = r#"{"releases":{"0.1":[{"filename":"a.tar.gz","digests":{"md5":"x"},"tags":["py3",{"abi":"none"}],"note":"","o":{}}]}}"#;
        let found = blocks(body).expect("valid JSON");
        let record: Vec<_> = found.iter().filter(|b| b.kind == BlockKind::Record).collect();
        assert_eq!(record.len(), 1);
        assert!(record[0].whole_record);
        assert_eq!(record[0].heading_path, path(&["releases", "0.1", "0"]));
        assert_eq!(
            record[0].text,
            "filename: a.tar.gz\ndigests.md5: x\ntags.0: py3\ntags.1.abi: none\n[note: no text]\no: {}"
        );
        let headings: Vec<&str> =
            found.iter().filter(|b| b.kind == BlockKind::Heading).map(|b| b.text.as_str()).collect();
        assert_eq!(headings, ["releases", "0.1"]);
    }

    #[test]
    fn strings_are_decoded_and_numbers_keep_their_spelling() {
        let found = read(r#"{"a":"café 😀 \"q\"\n  x","n":[9007199254740993,-0,1.50e+3]}"#);
        let values: Vec<&str> = found.iter().filter(|b| b.0 == BlockKind::Paragraph).map(|b| b.1.as_str()).collect();
        assert_eq!(values, ["café 😀 \"q\"\n  x", "9007199254740993", "-0", "1.50e+3"]);
    }

    #[test]
    fn a_repeated_key_keeps_every_value_and_empty_containers_are_kept() {
        let found = read(r#"{"k":1,"k":2,"o":{},"l":[],"s":"  "}"#);
        let values: Vec<(&str, &[String])> =
            found.iter().filter(|b| b.0 == BlockKind::Paragraph).map(|b| (b.1.as_str(), b.2.as_slice())).collect();
        assert_eq!(
            values,
            [("1", &path(&["k"])[..]), ("2", &path(&["k"])[..]), ("{}", &path(&["o"])[..]), ("[]", &path(&["l"])[..])]
        );
    }

    #[test]
    fn a_root_scalar_has_no_path_and_deep_nesting_is_read() {
        assert_eq!(read(" \"x\" "), [(BlockKind::Paragraph, "x".to_string(), path(&[]))]);
        let deep = format!("{}1{}", "[".repeat(100_000), "]".repeat(100_000));
        assert_eq!(read(&deep).len(), 1);
    }

    #[test]
    fn a_document_that_is_not_all_json_is_no_structure() {
        for body in [
            "",
            "{",
            r#"{"a":1}x"#,
            r#"{"a":1,}"#,
            "[1,]",
            "01",
            "1.",
            "-",
            r#""\ud800""#,
            r#""\x""#,
            "\"a\nb\"",
            "{'a':1}",
            "tru",
            r#"{"a" 1}"#,
            "[1}",
        ] {
            assert!(blocks(body).is_none(), "{body:?}");
        }
    }
}
