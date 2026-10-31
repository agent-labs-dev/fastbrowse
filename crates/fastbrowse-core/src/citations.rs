//! WICG text-fragment deep links derived from captured quotes.

use std::fmt;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

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

#[cfg(test)]
mod tests {
    use super::*;
    use percent_encoding::percent_decode_str;

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
