//! Counts, totals and extremes in a composed answer, worked out by code.
//!
//! Jev cannot count, is not a calculator and does poorly with numbers written as text (docs/jev.md, "Where Jev is not
//! used"), and a composer that writes a count in prose miscounts. So the composer only declares what a claim tallies,
//! and writes [`SLOT`] where the value goes. Jev answers one question per note read, whether it is one record of that
//! kind, and for a total or an extreme which number in it is the field; code counts, adds and compares those answers.

use std::collections::HashSet;

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

/// Where a claim's text takes the value code works out.
pub const SLOT: &str = "{tally}";

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

/// A value a claim works out across records: what it does, the kind of record it ranges over, and for a total or an
/// extreme the field it adds or compares.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tally {
    pub op: Op,
    pub of: String,
    pub field: Option<String>,
}

impl Schema for Tally {
    fn ty() -> Ty {
        ModelTy::frozen(
            "Tally",
            vec![
                Field::required("op", Ty::literals(&["count", "sum", "min", "max"])),
                Field::required("of", Ty::Str),
                Field::default("field", Ty::optional(Ty::Str), serde_json::Value::Null),
            ],
        )
        .ty()
    }
}

/// A tally worked out: the claim's text with the value in its slot, and the notes it rests on.
#[derive(Debug, Clone, PartialEq)]
pub struct Worked {
    pub text: String,
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
}

/// Every note read, once each: a superseded read is a past state, and a note whose whole text a note before it
/// already gave is the same record read again, from a later visit or another listing of it.
fn candidates(notes: &Notes) -> Vec<Candidate> {
    let mut seen = HashSet::new();
    notes
        .facts()
        .into_iter()
        .filter_map(|fact| fact.evidence.as_ref().map(|evidence| (fact_id(fact), evidence)))
        .filter(|(id, _)| !notes.superseded(id))
        .filter(|(_, evidence)| seen.insert(text::squash(&evidence.quote)))
        .map(|(id, evidence)| Candidate {
            id,
            quote: evidence.quote.clone(),
            note: json!({"quote": evidence.quote, "page": evidence.url, "headings": evidence.heading_path}),
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

fn value_question(tally: &Tally, note: &serde_json::Value, numbers: &[(usize, usize, String)]) -> Question {
    let field = text::repr(tally.field.as_deref().unwrap_or("value"));
    let mut criteria: IndexMap<String, serde_json::Value> = numbers
        .iter()
        .enumerate()
        .map(|(index, (_, _, number))| (format!("n{index}"), json!(format!("{} is its {field}.", text::repr(number)))))
        .collect();
    criteria.insert(NO_VALUE.into(), json!(format!("None of these numbers is its {field}.")));
    Question::Choice(ChoiceQuestion::new(
        format!("{UNTRUSTED}\nWhich number in this note is the {field} of the record it holds? Note: {}", dumps(note)),
        criteria,
    ))
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
        if !text.contains(SLOT) {
            let reason = format!("The claim sets tally but its text has no {SLOT} where the value goes.");
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
    for (index, tally, text) in claims {
        if worked.contains_key(index) {
            continue;
        }
        let judged: Option<Vec<usize>> = (0..found.len())
            .map(|note| member(*index, note).map(|is| is.then_some(note)))
            .collect::<Option<Vec<_>>>()
            .map(|notes| notes.into_iter().flatten().collect());
        let Some(records) = judged else {
            worked.insert(*index, Err(Unworked::Unjudged));
            continue;
        };
        if tally.op == Op::Count {
            let evidence_ids = records.iter().map(|note| found[*note].id.clone()).collect();
            worked.insert(*index, Ok(Worked { text: text.replace(SLOT, &records.len().to_string()), evidence_ids }));
            continue;
        }
        if records.is_empty() {
            let reason = format!("No note read is one {}.", text::repr(&tally.of));
            worked.insert(*index, Err(Unworked::Draft(reason)));
            continue;
        }
        for note in &records {
            let numbers = spans(&found[*note].quote, SpanKind::Number);
            if numbers.len() > 1 {
                values.insert(format!("t{index}/v{note}"), value_question(tally, &found[*note].note, &numbers));
            }
        }
        pending.push((*index, *tally, *text, records));
    }
    let picked = ask(jev, &state, &values, tokens, ledger).await?;
    for (index, tally, text, records) in pending {
        let mut amounts = Vec::new();
        for note in records {
            let numbers = spans(&found[note].quote, SpanKind::Number);
            let chosen = match numbers.len() {
                0 => None,
                1 => Some(numbers[0].2.clone()),
                _ => match picked.as_ref().and_then(|a| a.get(&format!("t{index}/v{note}"))) {
                    Some(Answer::Choice(answer)) => answer
                        .choice
                        .strip_prefix('n')
                        .and_then(|n| n.parse::<usize>().ok())
                        .map(|n| numbers[n].2.clone()),
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
                Worked { text: text.replace(SLOT, &total(&all)), evidence_ids }
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
                Worked { text: text.replace(SLOT, &winners[0].1.written), evidence_ids }
            }
            Op::Count => unreachable!("a count is worked out above"),
        };
        worked.insert(index, Ok(result));
    }
    Ok(worked)
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
    fn amounts_keep_their_sign_and_places() {
        let found = amount("£10.69").unwrap();
        assert_eq!((found.value, found.places, found.sign), (10.69, 2, Some('£')));
        assert_eq!(amount("1,234").unwrap().value, 1234.0);
        let (a, b) = (amount("£1.5").unwrap(), amount("£2.25").unwrap());
        assert_eq!(total(&[&a, &b]), "£3.75");
        assert_eq!(total(&[&a, &amount("2").unwrap()]), "3.5");
    }
}
