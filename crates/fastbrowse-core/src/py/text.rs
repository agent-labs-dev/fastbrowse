//! The `str` methods and `re` classes the ported modules lean on, where Rust's nearest one answers differently.
//!
//! Offsets elsewhere in the crate are byte offsets into UTF-8; a Python length is a count of code points, and
//! [`len`] is that count wherever a budget or a cut is measured.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

/// `len(text)`: code points, not bytes.
pub fn len(text: &str) -> usize {
    text.chars().count()
}

/// `text[:count]` by code points.
pub fn prefix(text: &str, count: usize) -> &str {
    match text.char_indices().nth(count) {
        Some((at, _)) => &text[..at],
        None => text,
    }
}

/// `str.isspace` for one character, which is also what `\s` and `str.split()` treat as space. It is Unicode's
/// White_Space plus the four information separators U+001C to U+001F, which Rust's `is_whitespace` leaves out.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `str.split()` with no arguments.
pub fn split(text: &str) -> impl Iterator<Item = &str> {
    text.split(is_space).filter(|part| !part.is_empty())
}

/// `" ".join(text.split())`: runs of space collapsed, ends trimmed.
pub fn squash(text: &str) -> String {
    split(text).collect::<Vec<_>>().join(" ")
}

/// `str.strip()`.
pub fn strip(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// `\w` in a `str` pattern: a letter, digit or underscore in any script.
pub fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `str.casefold`. Lowercasing differs from folding where one character folds to several: `ß` is `ss`, and the
/// final sigma and the Latin ligatures fold to their plain forms.
pub fn casefold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\u{df}' | '\u{1e9e}' => out.push_str("ss"),
            '\u{3c2}' => out.push('\u{3c3}'),
            '\u{fb00}' => out.push_str("ff"),
            '\u{fb01}' => out.push_str("fi"),
            '\u{fb02}' => out.push_str("fl"),
            '\u{fb03}' => out.push_str("ffi"),
            '\u{fb04}' => out.push_str("ffl"),
            '\u{fb05}' | '\u{fb06}' => out.push_str("st"),
            '\u{130}' => out.push_str("i\u{307}"),
            c => out.extend(c.to_lowercase()),
        }
    }
    out
}

/// `str.title()`: each run of cased letters starts upper and continues lower.
pub fn title(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut previous_cased = false;
    for c in text.chars() {
        if previous_cased {
            out.extend(c.to_lowercase());
        } else {
            out.extend(c.to_uppercase());
        }
        previous_cased = c.is_lowercase() || c.is_uppercase();
    }
    out
}

/// Characters a case-insensitive `re` match treats as one letter.
pub fn same_letter(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// Whether `text` starts with `word`, ignoring case as `re.IGNORECASE` does; the length matched, in bytes.
pub fn starts_with_ignore_case(text: &str, word: &str) -> Option<usize> {
    let mut chars = text.char_indices();
    for w in word.chars() {
        let (_, c) = chars.next()?;
        if !same_letter(c, w) {
            return None;
        }
    }
    Some(chars.next().map_or(text.len(), |(at, _)| at))
}

/// A list as pydantic words it: `a, b or c`.
pub fn or_list(items: &[String]) -> String {
    match items.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// `repr(text)`: single quotes unless the text holds one and no double quote.
pub fn repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if unprintable(c) => match c as u32 {
                n @ ..0x100 => out.push_str(&format!("\\x{n:02x}")),
                n @ ..0x10000 => out.push_str(&format!("\\u{n:04x}")),
                n => out.push_str(&format!("\\U{n:08x}")),
            },
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Whether `str.isprintable` is false for `c`: controls, format characters, private use and separators other than
/// the space. Unassigned code points, which Python also escapes, are left as they are: telling them apart needs the
/// Unicode tables.
fn unprintable(c: char) -> bool {
    (c.is_control() || is_space(c) && c != ' ')
        || matches!(
            c as u32,
            0xad | 0x600..=0x605
                | 0x61c
                | 0x6dd
                | 0x70f
                | 0x890..=0x891
                | 0x8e2
                | 0x180e
                | 0x200b..=0x200f
                | 0x202a..=0x202e
                | 0x2060..=0x2064
                | 0x2066..=0x206f
                | 0xe000..=0xf8ff
                | 0xfeff
                | 0xfff9..=0xfffb
                | 0x110bd
                | 0x110cd
                | 0x13430..=0x1343f
                | 0x1bca0..=0x1bca3
                | 0x1d173..=0x1d17a
                | 0xe0001
                | 0xe0020..=0xe007f
                | 0xf0000..=0xffffd
                | 0x100000..=0x10fffd
        )
}

/// What `urllib.parse.quote` never encodes: letters, digits and `_.-~`.
const ALWAYS_SAFE: &AsciiSet = &NON_ALPHANUMERIC.remove(b'_').remove(b'.').remove(b'-').remove(b'~');
const SAFE_SLASH: &AsciiSet = &ALWAYS_SAFE.remove(b'/');
const SAFE_SPACE: &AsciiSet = &ALWAYS_SAFE.remove(b' ');

/// `urllib.parse.quote(text, safe="")`.
pub fn quote(text: &str) -> String {
    utf8_percent_encode(text, ALWAYS_SAFE).to_string()
}

/// `urllib.parse.quote(text)`, whose default keeps `/`.
pub fn quote_path(text: &str) -> String {
    utf8_percent_encode(text, SAFE_SLASH).to_string()
}

/// `urllib.parse.quote_plus(text)`.
pub fn quote_plus(text: &str) -> String {
    utf8_percent_encode(text, SAFE_SPACE).to_string().replace(' ', "+")
}

const KEYWORDS: [&str; 35] = [
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
    "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "nonlocal",
    "not", "or", "pass", "raise", "return", "try", "while", "with", "yield",
];

/// `keyword.iskeyword`.
pub fn is_keyword(name: &str) -> bool {
    KEYWORDS.contains(&name)
}

/// `str.isidentifier`, approximated by Unicode's XID classes through `char` properties: a letter or `_`, then
/// letters, digits, marks and `_`.
pub fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' => chars.all(|c| c.is_alphanumeric() || c == '_'),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_string_methods() {
        assert_eq!(squash("  next\n\u{1c}page "), "next page");
        assert_eq!(casefold("Stra\u{df}e"), "strasse");
        assert_eq!(title("capture_sha256"), "Capture_Sha256");
        assert_eq!(title("2fa"), "2Fa");
        assert_eq!(repr("it's"), "\"it's\"");
        assert_eq!(repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(quote("p@ss w\"ord"), "p%40ss%20w%22ord");
        assert_eq!(quote_plus("p@ss w\"ord"), "p%40ss+w%22ord");
        assert_eq!(quote_path("/a b/c~"), "/a%20b/c~");
        assert_eq!(prefix("\u{6771}\u{4eac}x", 2), "\u{6771}\u{4eac}");
    }
}
