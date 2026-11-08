//! Counts, totals and extremes in a composed answer, worked out by code.
//!
//! Jev cannot count, is not a calculator and does poorly with numbers written as text (docs/jev.md, "Where Jev is not
//! used"), and a composer that writes a count in prose miscounts. So the composer declares what a claim tallies and the
//! value as its text writes it, and code writes the value it works out in that value's place. Jev answers one question
//! per note read, whether it is one record of that kind, and for a total or an extreme which number in it is the
//! field; code counts, adds and compares those answers.

use std::collections::{BTreeSet, HashMap, HashSet};

use fastbrowse_core::config::TokenBudget;
use fastbrowse_core::decision::{Answer, ChoiceQuestion, DecisionClient, NoulQuestion, Question};
use fastbrowse_core::models::{JevPurpose, UNTRUSTED};
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::py::text;
use fastbrowse_core::schema::{Field, ModelTy, Schema, Ty};
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::text::{SpanKind, currency, spans};
use crate::batches::evaluate_batches;
use crate::error::{AgentError, Result};
use crate::memory::{Notes, fact_id};

/// What the support check is shown in place of the value, which it is not asked to judge.
pub const COMPUTED: &str = "[computed]";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Count,
    Sum,
    Min,
    Max,
}

/// A value a claim works out across records: what it does, the kind of record it ranges over, for a total or an
/// extreme the field it adds or compares, and the value as the claim's text writes it, which code replaces.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tally {
    pub op: Op,
    pub of: String,
    pub field: Option<String>,
    pub written: String,
}

impl Schema for Tally {
    fn ty() -> Ty {
        ModelTy::frozen(
            "Tally",
            vec![
                Field::required("op", Ty::literals(&["count", "sum", "min", "max"])),
                Field::required("of", Ty::Str),
                Field::default("field", Ty::optional(Ty::Str), serde_json::Value::Null),
                Field::required("written", Ty::Str),
            ],
        )
        .ty()
    }
}

/// A tally worked out: the value for the claim's slot, as written, and the notes it rests on.
#[derive(Debug, Clone, PartialEq)]
pub struct Worked {
    pub value: String,
    pub evidence_ids: Vec<String>,
}

/// Why a tally has no value.
#[derive(Debug, Clone, PartialEq)]
pub enum Unworked {
    /// The draft is at fault and a corrected one can fix it: the reason is shown to the composer.
    Draft(String),
    /// Jev left a question unanswered, so no value can be stated.
    Unjudged,
}

/// One note a tally may count: the first note read with its text, so a record read twice is one record.
struct Candidate {
    id: String,
    quote: String,
    note: serde_json::Value,
    /// The capture the quote was cut from, and its span there.
    span: (String, i64, i64),
}

impl Candidate {
    /// Whether this note's span lies inside `other`'s, on the same capture: a line of a card, not the card.
    fn within(&self, other: &Candidate) -> bool {
        let ((capture, start, end), (outer, from, to)) = (&self.span, &other.span);
        capture == outer && from <= start && end <= to && (from, to) != (start, end)
    }
}

/// The records Jev judged members, less any inside another member: part of a record is not one of its own.
fn whole(records: Vec<usize>, found: &[Candidate]) -> Vec<usize> {
    records.iter().copied().filter(|&note| !records.iter().any(|&other| found[note].within(&found[other]))).collect()
}

/// Every note read, once each: a superseded read is a past state, and a note whose whole text a note before it
/// already gave is the same record read again, from a later visit or another listing of it. Within one capture the
/// same text twice is two records, such as a row two tables both hold, so a note is keyed by its text and which
/// occurrence of that text it is on its capture. A note from a table says
/// which of its page's tables it is in, counted by code, since a task names a table by its place on the page and a
/// note alone does not show it.
fn candidates(notes: &Notes) -> Vec<Candidate> {
    let live: Vec<(String, &fastbrowse_core::models::Evidence)> = notes
        .facts()
        .into_iter()
        .filter_map(|fact| fact.evidence.as_ref().map(|evidence| (fact_id(fact), evidence)))
        .filter(|(id, _)| !notes.superseded(id))
        .collect();
    // Where each text was noted on each capture, in page order.
    let mut starts: HashMap<(&str, String), BTreeSet<i64>> = HashMap::new();
    for (_, evidence) in &live {
        let key = (evidence.capture_sha256.as_str(), text::squash(&evidence.quote));
        starts.entry(key).or_default().insert(evidence.start);
    }
    let mut seen = HashSet::new();
    live.into_iter()
        .filter(|(_, evidence)| {
            let quote = text::squash(&evidence.quote);
            let place = starts[&(evidence.capture_sha256.as_str(), quote.clone())].range(..evidence.start).count();
            seen.insert((quote, place))
        })
        .map(|(id, evidence)| {
            let mut note = json!({"quote": evidence.quote, "page": evidence.url, "headings": evidence.heading_path});
            let blocks = notes.table_blocks(evidence);
            if let Some(block) = blocks.iter().find(|block| block.start <= evidence.start && evidence.start < block.end)
            {
                let count = blocks.last().map_or(0, |last| last.table + 1);
                note["table"] = format!("table {} of the {count} on its page", block.table + 1).into();
            }
            let span = (evidence.capture_sha256.clone(), evidence.start, evidence.end);
            Candidate { id, quote: evidence.quote.clone(), note, span }
        })
        .collect()
}

fn member_question(tally: &Tally, note: &serde_json::Value) -> Question {
    let of = text::repr(&tally.of);
    Question::Noul(NoulQuestion::new(
        format!(
            "{UNTRUSTED}\nIs this note one whole {of}? Part of a record, such as its tags, a label or a heading, is not one, and nor is a total a page states. Note: {}",
            dumps(note)
        ),
        Some(&format!("The note is one {of}.")),
        Some(&format!("The note is not one {of}, or is only part of one.")),
    ))
}

const NO_VALUE: &str = "none";

fn value_question(tally: &Tally, note: &serde_json::Value, numbers: &[String]) -> Question {
    let field = text::repr(tally.field.as_deref().unwrap_or("value"));
    let mut criteria: IndexMap<String, serde_json::Value> = numbers
        .iter()
        .enumerate()
        .map(|(index, number)| (format!("n{index}"), json!(format!("{} is its {field}.", text::repr(number)))))
        .collect();
    criteria.insert(NO_VALUE.into(), json!(format!("None of these numbers is its {field}.")));
    Question::Choice(ChoiceQuestion::new(
        format!("{UNTRUSTED}\nWhich number in this note is the {field} of the record it holds? Note: {}", dumps(note)),
        criteria,
    ))
}

/// The numbers a note could hold its field as. A comma both groups thousands and separates the cells of a line, as in
/// a CSV row, and the text alone cannot say which: "INV-01,120.00" reads as 1,120.00 or as 01 and 120.00. Both readings
/// are offered, and Jev picks the one that is the field.
fn numbers(quote: &str) -> Vec<String> {
    let cells = quote.lines().flat_map(|line| line.split(','));
    let mut found: Vec<String> = spans(quote, SpanKind::Number).into_iter().map(|span| span.2).collect();
    for number in cells.flat_map(|cell| spans(cell, SpanKind::Number)).map(|span| span.2) {
        if !found.contains(&number) {
            found.push(number);
        }
    }
    found
}

/// A number as written, as code reads it: its value, its decimal places, and the currency sign it carries.
#[derive(Debug, Clone, PartialEq)]
struct Amount {
    value: f64,
    places: usize,
    sign: Option<char>,
    written: String,
}

fn amount(written: &str) -> Option<Amount> {
    let sign = written.chars().find(|c| currency(*c));
    let digits: String = written.chars().filter(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+')).collect();
    let value = digits.parse::<f64>().ok()?;
    let places = digits.split_once('.').map_or(0, |(_, places)| places.len());
    Some(Amount { value, places, sign, written: written.trim().to_string() })
}

/// The total of `amounts` written as they are: their currency sign when they share one, and their most places.
fn total(amounts: &[&Amount]) -> String {
    let places = amounts.iter().map(|amount| amount.places).max().unwrap_or(0);
    let sum: f64 = amounts.iter().map(|amount| amount.value).sum();
    let sign =
        amounts.first().and_then(|first| first.sign).filter(|sign| amounts.iter().all(|a| a.sign == Some(*sign)));
    format!("{}{sum:.places$}", sign.map(String::from).unwrap_or_default())
}

/// Each claim's tally worked out, by the claim's index; a claim without a tally has no entry. Every note read is asked
/// whether it is one record of the kind, rather than the notes the claim cites, so a record the composer left out is
/// still counted.
pub async fn work(
    jev: &dyn DecisionClient,
    task: &str,
    claims: &[(usize, &Tally, &str)],
    notes: &Notes,
    tokens: &TokenBudget,
    ledger: Option<&Ledger>,
) -> Result<IndexMap<usize, std::result::Result<Worked, Unworked>>> {
    let mut worked = IndexMap::new();
    let found = candidates(notes);
    let state = json!({"task": task});
    let mut members = IndexMap::new();
    for (index, tally, text) in claims {
        if tally.written.trim().is_empty() || text.matches(tally.written.as_str()).count() != 1 {
            let reason = format!(
                "The claim's tally.written {} is not written once, word for word, in its text.",
                text::repr(&tally.written)
            );
            worked.insert(*index, Err(Unworked::Draft(reason)));
            continue;
        }
        if tally.op != Op::Count && tally.field.is_none() {
            let reason = "The claim's tally adds or compares records but names no field.".to_string();
            worked.insert(*index, Err(Unworked::Draft(reason)));
            continue;
        }
        for (note, candidate) in found.iter().enumerate() {
            members.insert(format!("t{index}/n{note}"), member_question(tally, &candidate.note));
        }
    }
    let answered = ask(jev, &state, &members, tokens, ledger).await?;
    let member = |index: usize, note: usize| match answered.as_ref().and_then(|a| a.get(&format!("t{index}/n{note}"))) {
        Some(Answer::Noul(answer)) if (0.0..=1.0).contains(&answer.probability) => Some(answer.probability > 0.5),
        _ => None,
    };
    let mut pending = Vec::new();
    let mut values = IndexMap::new();
    for (index, tally, _) in claims {
        if worked.contains_key(index) {
            continue;
        }
        let judged: Option<Vec<usize>> = (0..found.len())
            .map(|note| member(*index, note).map(|is| is.then_some(note)))
            .collect::<Option<Vec<_>>>()
            .map(|notes| notes.into_iter().flatten().collect());
        let Some(records) = judged.map(|records| whole(records, &found)) else {
            worked.insert(*index, Err(Unworked::Unjudged));
            continue;
        };
        if tally.op == Op::Count {
            let evidence_ids = records.iter().map(|note| found[*note].id.clone()).collect();
            worked.insert(*index, Ok(Worked { value: records.len().to_string(), evidence_ids }));
            continue;
        }
        if records.is_empty() {
            let reason = format!("No note read is one {}.", text::repr(&tally.of));
            worked.insert(*index, Err(Unworked::Draft(reason)));
            continue;
        }
        for note in &records {
            let numbers = numbers(&found[*note].quote);
            if numbers.len() > 1 {
                values.insert(format!("t{index}/v{note}"), value_question(tally, &found[*note].note, &numbers));
            }
        }
        pending.push((*index, *tally, records));
    }
    let picked = ask(jev, &state, &values, tokens, ledger).await?;
    for (index, tally, records) in pending {
        let mut amounts = Vec::new();
        for note in records {
            let numbers = numbers(&found[note].quote);
            let chosen = match numbers.len() {
                0 => None,
                1 => Some(numbers[0].clone()),
                _ => match picked.as_ref().and_then(|a| a.get(&format!("t{index}/v{note}"))) {
                    Some(Answer::Choice(answer)) => answer
                        .choice
                        .strip_prefix('n')
                        .and_then(|n| n.parse::<usize>().ok())
                        .and_then(|n| numbers.get(n).cloned()),
                    _ => {
                        worked.insert(index, Err(Unworked::Unjudged));
                        break;
                    }
                },
            };
            // A record whose field is not in its note is counted as having none, and so leaves no value to state.
            let Some(amount) = chosen.as_deref().and_then(amount) else {
                let field = text::repr(tally.field.as_deref().unwrap_or("value"));
                let reason = format!("Note {} is one {} but states no {field}.", found[note].id, text::repr(&tally.of));
                worked.insert(index, Err(Unworked::Draft(reason)));
                break;
            };
            amounts.push((note, amount));
        }
        if worked.contains_key(&index) {
            continue;
        }
        let result = match tally.op {
            Op::Sum => {
                let all: Vec<&Amount> = amounts.iter().map(|(_, amount)| amount).collect();
                let evidence_ids = amounts.iter().map(|(note, _)| found[*note].id.clone()).collect();
                Worked { value: total(&all), evidence_ids }
            }
            Op::Min | Op::Max => {
                let better = |a: f64, b: f64| if tally.op == Op::Min { a < b } else { a > b };
                let best =
                    amounts.iter().map(|(_, amount)| amount.value).reduce(|a, b| if better(b, a) { b } else { a });
                let best = best.expect("a tally over no records stopped above");
                // Every record at the extreme is cited, so a tie is not settled by reading order.
                let winners: Vec<&(usize, Amount)> =
                    amounts.iter().filter(|(_, amount)| amount.value == best).collect();
                let evidence_ids = winners.iter().map(|(note, _)| found[*note].id.clone()).collect();
                Worked { value: winners[0].1.written.clone(), evidence_ids }
            }
            Op::Count => unreachable!("a count is worked out above"),
        };
        worked.insert(index, Ok(Worked { value: as_written(&result.value, &tally.written), ..result }));
    }
    Ok(worked)
}

/// A worked amount in the form the claim writes its value: a claim whose `written` leaves the currency sign in the
/// text beside it ("$" then "251.00") takes the number alone, or the sign would be written twice.
fn as_written(value: &str, written: &str) -> String {
    if written.chars().any(currency) { value.to_string() } else { value.chars().filter(|c| !currency(*c)).collect() }
}

/// Jev's answers to `questions`, or None when there are none to ask or Jev could not answer them.
async fn ask(
    jev: &dyn DecisionClient,
    state: &serde_json::Value,
    questions: &IndexMap<String, Question>,
    tokens: &TokenBudget,
    ledger: Option<&Ledger>,
) -> Result<Option<IndexMap<String, Answer>>> {
    if questions.is_empty() {
        return Ok(None);
    }
    match evaluate_batches(JevPurpose::Verify, jev, state, questions, tokens, ledger, None, false).await {
        Ok(answered) => Ok(answered.map(|answered| answered.answers)),
        Err(AgentError::Jev(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_comma_separated_line_offers_each_cell_as_well_as_the_grouped_number() {
        assert_eq!(super::numbers("reference,item,total\nA-1001,Blue Kettle,34.50"), ["1001", "34.50"]);
        assert_eq!(super::numbers("invoice,amount\nINV-01,120.00"), ["01,120.00", "01", "120.00"]);
        assert_eq!(super::numbers("Total: $1,234.50"), ["$1,234.50", "$1", "234.50"]);
    }

    #[test]
    fn amounts_keep_their_sign_and_places() {
        let found = amount("£10.69").unwrap();
        assert_eq!((found.value, found.places, found.sign), (10.69, 2, Some('£')));
        assert_eq!(amount("1,234").unwrap().value, 1234.0);
        let (a, b) = (amount("£1.5").unwrap(), amount("£2.25").unwrap());
        assert_eq!(total(&[&a, &b]), "£3.75");
        assert_eq!(total(&[&a, &amount("2").unwrap()]), "3.5");
    }

    #[test]
    fn a_worked_amount_carries_a_sign_only_when_the_claim_writes_one_in_its_value() {
        assert_eq!(as_written("$251.00", "251.00"), "251.00", "the claim writes the sign before its value");
        assert_eq!(as_written("$251.00", "$251"), "$251.00");
        assert_eq!(as_written("3.5", "3"), "3.5");
    }

    #[test]
    fn a_line_inside_a_counted_card_is_not_counted_again() {
        let note = |capture: &str, start, end| Candidate {
            id: String::new(),
            quote: String::new(),
            note: json!({}),
            span: (capture.into(), start, end),
        };
        let found = [note("a", 0, 50), note("a", 0, 20), note("b", 0, 20), note("a", 60, 90)];
        assert_eq!(whole(vec![0, 1, 2, 3], &found), [0, 2, 3], "only the line inside card 0 on its capture goes");
        assert_eq!(whole(vec![1, 3], &found), [1, 3], "a line whose card is not a member still counts");
    }
}
