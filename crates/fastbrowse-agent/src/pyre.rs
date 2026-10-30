//! The few `re` patterns retrieval uses, matched as CPython's backtracking engine matches them.
//!
//! The `regex` crate has no lookaround and its `\w`, `\s` and case folding are Unicode's, not `str`'s, so a pattern
//! there could pick a different span than Python on the same page, and a quote cut one character apart is another
//! citation id. A pattern here is a tree built in code from Python's own classes, and [`Pat::ends`] lists every end a
//! match from one position can reach, in the order the backtracking engine would try them: the first end that
//! satisfies everything after it is the one Python takes.
//!
//! Offsets are code points into a `&[char]`, as Python indexes a `str`.

use fastbrowse_core::py::text::is_word;

/// `\d` in a `str` pattern: a Unicode decimal digit (category Nd).
pub(crate) fn is_digit(c: char) -> bool {
    crate::comparison::digit(c).is_some()
}

/// One ASCII letter of a pattern compiled with `re.IGNORECASE`. sre folds the dotted and dotless i, the long s and
/// the Kelvin sign onto their ASCII letters as well as the other case.
pub(crate) fn same_letter_ignoring_case(c: char, letter: char) -> bool {
    c.to_ascii_lowercase() == letter
        || matches!((letter, c), ('i', '\u{130}' | '\u{131}') | ('s', '\u{17f}') | ('k', '\u{212a}'))
}

pub(crate) enum Pat {
    /// One character a predicate accepts.
    Class(Box<dyn Fn(char) -> bool + Send + Sync>),
    /// A run of literal characters.
    Lit(Vec<char>),
    /// A run of ASCII letters and other literal characters under `re.IGNORECASE`.
    ILit(Vec<char>),
    Seq(Vec<Pat>),
    /// Alternatives tried in order.
    Alt(Vec<Pat>),
    /// `{min,max}`, greedy or lazy.
    Rep {
        pat: Box<Pat>,
        min: usize,
        max: usize,
        greedy: bool,
    },
    /// `(?=...)` and `(?!...)`.
    Ahead {
        pat: Box<Pat>,
        negate: bool,
    },
    /// `(?<=c)` and `(?<!c)` for a one-character class, the only lookbehind retrieval writes.
    Behind {
        class: fn(char) -> bool,
        negate: bool,
    },
    /// `\b`, by Python's `\w`.
    Boundary,
}

impl Pat {
    pub(crate) fn class(predicate: impl Fn(char) -> bool + Send + Sync + 'static) -> Pat {
        Pat::Class(Box::new(predicate))
    }

    pub(crate) fn lit(text: &str) -> Pat {
        Pat::Lit(text.chars().collect())
    }

    pub(crate) fn ilit(text: &str) -> Pat {
        Pat::ILit(text.chars().collect())
    }

    pub(crate) fn alt(options: Vec<Pat>) -> Pat {
        Pat::Alt(options)
    }

    pub(crate) fn opt(pat: Pat) -> Pat {
        Pat::Rep { pat: Box::new(pat), min: 0, max: 1, greedy: true }
    }

    pub(crate) fn star(pat: Pat) -> Pat {
        Pat::Rep { pat: Box::new(pat), min: 0, max: usize::MAX, greedy: true }
    }

    pub(crate) fn plus(pat: Pat) -> Pat {
        Pat::Rep { pat: Box::new(pat), min: 1, max: usize::MAX, greedy: true }
    }

    pub(crate) fn repeat(pat: Pat, min: usize, max: usize) -> Pat {
        Pat::Rep { pat: Box::new(pat), min, max, greedy: true }
    }

    pub(crate) fn not_ahead(pat: Pat) -> Pat {
        Pat::Ahead { pat: Box::new(pat), negate: true }
    }

    pub(crate) fn not_behind(class: fn(char) -> bool) -> Pat {
        Pat::Behind { class, negate: true }
    }

    /// Every end a match starting at `at` can reach, in the order backtracking tries them.
    pub(crate) fn ends(&self, text: &[char], at: usize, out: &mut Vec<usize>) {
        match self {
            Pat::Class(predicate) => {
                if text.get(at).is_some_and(|c| predicate(*c)) {
                    out.push(at + 1);
                }
            }
            Pat::Lit(chars) => {
                if text.len() >= at + chars.len() && text[at..at + chars.len()] == chars[..] {
                    out.push(at + chars.len());
                }
            }
            Pat::ILit(chars) => {
                let fits = text.len() >= at + chars.len()
                    && chars.iter().zip(&text[at..]).all(|(want, got)| {
                        if want.is_ascii_alphabetic() { same_letter_ignoring_case(*got, *want) } else { want == got }
                    });
                if fits {
                    out.push(at + chars.len());
                }
            }
            Pat::Seq(parts) => seq(parts, text, at, out),
            Pat::Alt(options) => options.iter().for_each(|option| option.ends(text, at, out)),
            Pat::Rep { pat, min, max, greedy } => repeat(pat, *min, *max, *greedy, 0, text, at, out),
            Pat::Ahead { pat, negate } => {
                let mut found = Vec::new();
                pat.ends(text, at, &mut found);
                if found.is_empty() == *negate {
                    out.push(at);
                }
            }
            Pat::Behind { class, negate } => {
                let matched = at > 0 && class(text[at - 1]);
                if matched != *negate {
                    out.push(at);
                }
            }
            Pat::Boundary => {
                let before = at > 0 && is_word(text[at - 1]);
                let after = text.get(at).is_some_and(|c| is_word(*c));
                if before != after {
                    out.push(at);
                }
            }
        }
    }

    /// The end of the match `re.match` finds at `at`.
    pub(crate) fn match_at(&self, text: &[char], at: usize) -> Option<usize> {
        let mut ends = Vec::new();
        self.ends(text, at, &mut ends);
        ends.first().copied()
    }

    /// `re.search` from `from`: the first position a match starts at, and its end.
    pub(crate) fn search(&self, text: &[char], from: usize) -> Option<(usize, usize)> {
        (from..=text.len()).find_map(|start| self.match_at(text, start).map(|end| (start, end)))
    }

    /// `re.finditer`: non-overlapping matches left to right; after an empty match the scan moves one character on.
    pub(crate) fn find_iter(&self, text: &[char]) -> Vec<(usize, usize)> {
        let mut found = Vec::new();
        let mut at = 0;
        while at <= text.len() {
            match self.search(text, at) {
                Some((start, end)) => {
                    found.push((start, end));
                    at = if end == start { end + 1 } else { end };
                }
                None => break,
            }
        }
        found
    }

    /// `re.fullmatch`.
    pub(crate) fn full_match(&self, text: &[char]) -> bool {
        let mut ends = Vec::new();
        self.ends(text, 0, &mut ends);
        ends.contains(&text.len())
    }
}

fn seq(parts: &[Pat], text: &[char], at: usize, out: &mut Vec<usize>) {
    let Some((first, rest)) = parts.split_first() else {
        out.push(at);
        return;
    };
    let mut ends = Vec::new();
    first.ends(text, at, &mut ends);
    for end in ends {
        seq(rest, text, end, out);
    }
}

#[allow(clippy::too_many_arguments)]
fn repeat(
    pat: &Pat,
    min: usize,
    max: usize,
    greedy: bool,
    count: usize,
    text: &[char],
    at: usize,
    out: &mut Vec<usize>,
) {
    if !greedy && count >= min {
        out.push(at);
    }
    if count < max {
        let mut ends = Vec::new();
        pat.ends(text, at, &mut ends);
        for end in ends {
            // An empty iteration past the minimum would repeat forever; sre stops it the same way.
            if end != at || count < min {
                repeat(pat, min, max, greedy, count + 1, text, end, out);
            }
        }
    }
    if greedy && count >= min {
        out.push(at);
    }
}

/// A `str` as the code points a pattern indexes.
#[cfg(test)]
pub(crate) fn chars(text: &str) -> Vec<char> {
    text.chars().collect()
}

/// Code point offsets to byte offsets, for slicing the `&str` a `&[char]` came from.
pub(crate) fn byte_offsets(text: &str) -> Vec<usize> {
    text.char_indices().map(|(at, _)| at).chain([text.len()]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digits() -> Pat {
        Pat::class(is_digit)
    }

    #[test]
    fn a_count_backtracks_off_a_trailing_comma_as_sre_does() {
        // r"\b\d[\d,]*\b"
        let pattern =
            Pat::Seq(vec![Pat::Boundary, digits(), Pat::star(Pat::class(|c| is_digit(c) || c == ',')), Pat::Boundary]);
        let text = chars("x 1,234, 5 a12 \u{661}\u{662}");
        let found: Vec<String> =
            pattern.find_iter(&text).into_iter().map(|(s, e)| text[s..e].iter().collect()).collect();
        assert_eq!(found, ["1,234", "5", "\u{661}\u{662}"]);
    }

    #[test]
    fn a_lazy_repeat_stops_at_the_first_end_that_lets_the_rest_match() {
        // r"a.*?b"
        let pattern = Pat::Seq(vec![
            Pat::lit("a"),
            Pat::Rep { pat: Box::new(Pat::class(|c| c != '\n')), min: 0, max: usize::MAX, greedy: false },
            Pat::lit("b"),
        ]);
        assert_eq!(pattern.search(&chars("xaxbxb"), 0), Some((1, 4)));
    }

    #[test]
    fn ignoring_case_folds_the_letters_sre_folds() {
        assert!(Pat::ilit("yes").full_match(&chars("YE\u{17f}")));
        assert!(Pat::ilit("first").full_match(&chars("f\u{130}rst")));
        assert!(!Pat::ilit("ss").full_match(&chars("\u{df}")));
    }
}
