//! Port of `src/fastbrowse/comparison.py`: numeric rankings over complete, cited records, with ambiguous fields left to
//! the reader.

use std::cmp::Ordering;
use std::collections::HashSet;

use fastbrowse_core::models::FactReader;
use fastbrowse_core::py::text;
use serde::{Deserialize, Deserializer, Serialize};

use crate::memory::{Comparison, Fact, Notes, unique};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotedField {
    pub prefix: String,
    pub suffix: String,
}

impl QuotedField {
    pub fn new(prefix: &str, suffix: &str) -> QuotedField {
        QuotedField { prefix: prefix.into(), suffix: suffix.into() }
    }

    /// The one value between the delimiters, stripped, or None when the quote holds none, several, or one spanning a
    /// line break.
    ///
    /// `re.finditer(r"(?=" + start + r"(.*?)" + end + r")", quote, re.DOTALL)`: every position the lookahead matches,
    /// overlapping, since adjacent fields can share a delimiter and consuming a match would hide the next one. An empty
    /// prefix is `\A` and an empty suffix `\Z`: the boundary of the quote, never every position inside it.
    pub fn extract(&self, quote: &str) -> Option<String> {
        let starts: Vec<usize> = if self.prefix.is_empty() {
            vec![0]
        } else {
            quote.char_indices().map(|(at, _)| at).filter(|at| quote[*at..].starts_with(&self.prefix)).collect()
        };
        let mut found = None;
        let mut matches = 0;
        for start in starts {
            let from = start + self.prefix.len();
            let value = if self.suffix.is_empty() {
                Some(&quote[from..])
            } else {
                quote[from..].find(&self.suffix).map(|end| &quote[from..from + end])
            };
            if let Some(value) = value {
                matches += 1;
                found = Some(value);
            }
        }
        let value = text::strip(found.filter(|_| matches == 1)?);
        (!value.is_empty() && !value.contains('\n')).then(|| value.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    Lowest,
    Highest,
}

impl Order {
    fn as_str(self) -> &'static str {
        match self {
            Order::Lowest => "lowest",
            Order::Highest => "highest",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NumericComparison {
    pub order: Order,
    #[serde(deserialize_with = "limit")]
    pub limit: usize,
    pub label: QuotedField,
    pub value: QuotedField,
}

/// `int = Field(ge=1, le=60)`.
fn limit<'de, D: Deserializer<'de>>(d: D) -> Result<usize, D::Error> {
    let value = fastbrowse_core::py::lax::int(d)?;
    match value {
        ..1 => Err(serde::de::Error::custom("Input should be greater than or equal to 1")),
        61.. => Err(serde::de::Error::custom("Input should be less than or equal to 60")),
        _ => Ok(value as usize),
    }
}

/// The first code point of each run of ten Unicode decimal digits (category Nd) in Unicode 15.1, CPython 3.13's
/// tables, so `\d` and `Decimal` read the digits Python reads, Arabic-Indic and Devanagari as well as ASCII.
const DIGIT_ZEROS: [u32; 68] = [
    0x30, 0x660, 0x6f0, 0x7c0, 0x966, 0x9e6, 0xa66, 0xae6, 0xb66, 0xbe6, 0xc66, 0xce6, 0xd66, 0xde6, 0xe50, 0xed0,
    0xf20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80, 0x1a90, 0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620,
    0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0, 0xff10, 0x104a0, 0x10d30, 0x11066, 0x110f0, 0x11136, 0x111d0,
    0x112f0, 0x11450, 0x114d0, 0x11650, 0x116c0, 0x11730, 0x118e0, 0x11950, 0x11c50, 0x11d50, 0x11da0, 0x11f50,
    0x16a60, 0x16ac0, 0x16b50, 0x1d7ce, 0x1d7d8, 0x1d7e2, 0x1d7ec, 0x1d7f6, 0x1e140, 0x1e2f0, 0x1e4f0, 0x1e950,
    0x1fbf0,
];

/// The value of `c` as a decimal digit, as `\d` in a `str` pattern matches it.
fn digit(c: char) -> Option<u8> {
    let code = c as u32;
    DIGIT_ZEROS.iter().find(|zero| (**zero..*zero + 10).contains(&code)).map(|zero| (code - zero) as u8)
}

/// A `Decimal` read from `-?\d+(?:\.\d+)?`: sign, whole digits without leading zeros, fraction without trailing ones,
/// so equal values compare equal however they were written.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Number {
    negative: bool,
    whole: Vec<u8>,
    fraction: Vec<u8>,
}

impl Number {
    fn parse(text: &str) -> Option<Number> {
        let (negative, rest) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text),
        };
        let (whole, fraction) = match rest.split_once('.') {
            Some((whole, fraction)) => (whole, Some(fraction)),
            None => (rest, None),
        };
        let digits = |part: &str| -> Option<Vec<u8>> {
            let digits: Option<Vec<u8>> = part.chars().map(digit).collect();
            digits.filter(|digits| !digits.is_empty())
        };
        let mut whole = digits(whole)?;
        let mut fraction = match fraction {
            Some(fraction) => digits(fraction)?,
            None => Vec::new(),
        };
        let leading = whole.iter().take_while(|d| **d == 0).count();
        whole.drain(..leading);
        while fraction.last() == Some(&0) {
            fraction.pop();
        }
        // Decimal("-0") equals Decimal("0").
        let negative = negative && !(whole.is_empty() && fraction.is_empty());
        Some(Number { negative, whole, fraction })
    }
}

impl Ord for Number {
    fn cmp(&self, other: &Self) -> Ordering {
        let magnitude = self
            .whole
            .len()
            .cmp(&other.whole.len())
            .then_with(|| self.whole.cmp(&other.whole))
            .then_with(|| self.fraction.cmp(&other.fraction));
        match (self.negative, other.negative) {
            (false, false) => magnitude,
            (true, true) => magnitude.reverse(),
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
        }
    }
}

impl PartialOrd for Number {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// `re.fullmatch(r"([$£€]?)(-?\d+(?:\.\d+)?)", value)`: the currency sign and the number.
fn amount(value: &str) -> Option<(&str, Number)> {
    let unit = value.chars().next().filter(|c| matches!(c, '$' | '\u{a3}' | '\u{20ac}')).map_or(0, char::len_utf8);
    Some((&value[..unit], Number::parse(&value[unit..])?))
}

pub fn complete_comparison(notes: &mut Notes, requirement_id: &str, comparison: &NumericComparison) -> bool {
    // Only quotes the requirement may still rest on: a held guessed row, or the whole notes taken as context with it,
    // left a rebuilt ranking refused for good.
    let known = notes.usable_evidence();
    let records: Vec<String> =
        notes.comparison_records(Some(requirement_id)).into_iter().filter(|key| !notes.refused(key)).collect();
    if records.is_empty() {
        return false;
    }
    let mut rows: Vec<(Number, String, String)> = Vec::new();
    let mut units: HashSet<String> = HashSet::new();
    for key in &records {
        let Some(evidence) = known.get(key) else { return false };
        let (Some(label), Some(value)) =
            (comparison.label.extract(&evidence.quote), comparison.value.extract(&evidence.quote))
        else {
            return false;
        };
        // Commas, ranges, conversions and multiple units need interpretation, not a guessed number format.
        let Some((unit, number)) = amount(&value) else { return false };
        units.insert(unit.to_string());
        rows.push((number, label, value.clone()));
    }
    let labels: HashSet<&String> = rows.iter().map(|row| &row.1).collect();
    if units.len() != 1 || rows.len() < comparison.limit || labels.len() != rows.len() {
        return false;
    }
    match comparison.order {
        Order::Lowest => rows.sort_by(|a, b| a.0.cmp(&b.0)),
        Order::Highest => rows.sort_by(|a, b| b.0.cmp(&a.0)),
    }
    // A tied cutoff needs the task's tie rule; choosing the first captured row would silently invent one.
    if rows.len() > comparison.limit && rows[comparison.limit - 1].0 == rows[comparison.limit].0 {
        return false;
    }
    let order = comparison.order.as_str();
    let mut text = format!("{}{} {}:", order[..1].to_uppercase(), &order[1..], comparison.limit);
    for (_, label, value) in &rows[..comparison.limit] {
        text.push_str(&format!("\n{label}: {value}"));
    }
    // Context such as the active category or filter is part of the proof, even when it is outside a record, but only
    // the context on the pages the records were read from: the whole notes as the basis tied a finished ranking to
    // every unrelated quote, and a refusal of one of those reopened it.
    let read: HashSet<(&String, &String)> =
        records.iter().map(|key| (&known[key].url, &known[key].capture_sha256)).collect();
    let context = known.iter().filter(|(_, e)| read.contains(&(&e.url, &e.capture_sha256))).map(|(key, _)| key.clone());
    let basis = unique(records.iter().cloned().chain(context));
    notes.add(Fact {
        requirement_id: Some(requirement_id.into()),
        tally: None,
        comparison: Some(Comparison { requirement_id: requirement_id.into(), records, complete: true }),
        text,
        evidence: None,
        basis,
        reader: FactReader::Llm,
        scope: None,
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_read_as_decimal_reads_them() {
        let (unit, three_and_a_half) = amount("$3.50").unwrap();
        assert_eq!(unit, "$");
        assert_eq!(three_and_a_half, amount("$3.5").unwrap().1);
        assert_eq!(amount("-0").unwrap().1, amount("0.00").unwrap().1);
        assert!(amount("\u{661}\u{662}").unwrap().1 > amount("11.99").unwrap().1);
        assert!(amount("-2").unwrap().1 < amount("-1.5").unwrap().1);
        for unread in ["1,000", "3.", ".5", "+1", "$", "1-2", "GBP 3"] {
            assert!(amount(unread).is_none(), "{unread}");
        }
    }

    #[test]
    fn a_limit_outside_one_to_sixty_is_refused() {
        let ranking = |limit: i64| {
            serde_json::from_value::<NumericComparison>(serde_json::json!({
                "order": "lowest", "limit": limit,
                "label": {"prefix": "", "suffix": ": "}, "value": {"prefix": ": ", "suffix": ""},
            }))
        };
        assert!(ranking(0).is_err() && ranking(61).is_err() && ranking(60).is_ok());
    }
}
