//! Counts, totals and extremes in a composed answer, worked out by code.
//!
//! Jev cannot count, is not a calculator and does poorly with numbers written as text (docs/jev.md, "Where Jev is not
//! used"), and a composer that writes a count in prose miscounts. So the composer declares what a claim tallies and the
//! value as its text writes it, and code writes the value it works out in that value's place. Jev answers one question
//! per note read, whether it is one record of that kind, and for a total or an extreme which number in it is the
//! field; code counts, adds and compares those answers. For a count per group the text model copies each record's
//! group from its note, since Jev can choose only among names it is given, and code groups and counts the records.

use std::collections::{BTreeSet, HashMap, HashSet};

use fastbrowse_core::config::TokenBudget;
use fastbrowse_core::decision::{Answer, ChoiceQuestion, DecisionClient, NoulQuestion, Question};
use fastbrowse_core::llm::{LLMClient, Message, Typed};
use fastbrowse_core::models::{JevPurpose, LLMPurpose, UNTRUSTED};
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::py::text;
use fastbrowse_core::schema::{Field, ModelTy, Schema, Ty};
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::answers::{context_limit, request_bytes};
use super::text::{SpanKind, currency, spans};
use crate::batches::evaluate_batches;
use crate::error::{AgentError, Result};
use crate::memory::{Notes, address, fact_id};
use crate::pyre::{Pat, digit};
use crate::support::generate;
use fastbrowse_core::py::text::is_word;

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
/// extreme the field it adds or compares, for an extreme its place counted from the end, and the value as the claim's
/// text writes it, which code replaces.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tally {
    pub op: Op,
    pub of: String,
    pub field: Option<String>,
    /// The second cheapest is the minimum at rank 2: with no rank a ranking had no tally, so its claim cited one record
    /// for a place among all of them and was rejected. None is the extreme itself.
    #[serde(default)]
    pub rank: Option<u64>,
    /// For a count per group, such as quotes per author: the field that groups the records and the group this claim
    /// counts. Code reads every record's group off its note, so a claim names only its own: asked to list every group,
    /// the composer named the three authors it ranked and never the other seventeen, and each draft was sent back until
    /// the run timed out (#985). A count per group with a rank is a place among the groups by their counts, such as the
    /// author with the second most quotes: written as a count of "quote by Albert Einstein" with "the most" beside it,
    /// the place was not checked, and the claim was rejected for stating it (#911).
    #[serde(default)]
    pub by: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    pub written: String,
}

impl Tally {
    /// Whether the claim writes the record at a minimum or maximum, such as the shop that sells it cheapest, rather than
    /// the value there: asked which shop is cheaper, the composer set `written` to the shop's name, and code wrote the
    /// price over it, so the answer read "84.99 sells the Burr Grinder cheaper".
    pub fn names_record(&self) -> bool {
        matches!(self.op, Op::Min | Op::Max) && amount(&self.written).is_none()
    }

    /// The extreme's place counted from its end, 1 for the extreme itself.
    pub fn place(&self) -> u64 {
        self.rank.unwrap_or(1)
    }

    /// The group a count per group counts, with the field that groups the records.
    pub fn grouped(&self) -> Option<(&str, &str)> {
        self.by.as_deref().zip(self.group.as_deref())
    }
}

impl Schema for Tally {
    fn ty() -> Ty {
        ModelTy::frozen(
            "Tally",
            vec![
                Field::required("op", Ty::literals(&["count", "sum", "min", "max"])),
                Field::required("of", Ty::Str),
                Field::default("field", Ty::optional(Ty::Str), serde_json::Value::Null),
                Field::default("rank", Ty::optional(Ty::Int), serde_json::Value::Null),
                Field::default("by", Ty::optional(Ty::Str), serde_json::Value::Null),
                Field::default("group", Ty::optional(Ty::Str), serde_json::Value::Null),
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
    /// The draft is at fault over the notes named by fact id: the reason speaks of "these notes", and the composer is
    /// shown them by the labels it cites notes by. Written into the reason, the ids named no note it was shown, and a
    /// ranked tally left 100 records unnamed was rejected the same way until the run gave up.
    Notes { reason: String, notes: Vec<String> },
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
    /// Whether the quote is the page's own text, rather than a reading of its screenshot or an element's markup.
    literal: bool,
    /// The page the note was read from.
    page: String,
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

/// For each record that is not page text, the questions asking which other record on its page it is. A record read
/// from a screenshot or an element's markup is in the reader's words, so no text match finds it among the notes cut
/// from its page's text: a book read off a listing's screenshot was counted beside its own text note, and a category of
/// 11 one-star books was given as 12 (#984). Jev says which record it repeats, and code counts the rest. Offered are
/// the records cut from its page's text, and those not cut from text that come before it, so of two such readings the
/// first stays; a page shows only its own records, so no other page's are offered. The offers are split to fit one
/// choice, with an option kept for none.
fn repeat_questions(index: usize, records: &[usize], found: &[Candidate], most: usize) -> IndexMap<String, Question> {
    let mut questions = IndexMap::new();
    for (at, &note) in records.iter().enumerate() {
        let reading = &found[note];
        if reading.literal {
            continue;
        }
        let offered: Vec<usize> = records
            .iter()
            .enumerate()
            .filter(|&(other, &record)| {
                other != at && found[record].page == reading.page && (found[record].literal || other < at)
            })
            .map(|(_, &record)| record)
            .collect();
        for (chunk, offer) in offered.chunks(most - 1).enumerate() {
            let mut criteria: IndexMap<String, serde_json::Value> = offer
                .iter()
                .map(|record| {
                    let note = dumps(&found[*record].note);
                    (format!("m{record}"), json!(format!("It is the record in this note: {note}")))
                })
                .collect();
            criteria.insert(NO_VALUE.into(), json!("It is a record none of these notes holds."));
            let question = format!(
                "{UNTRUSTED}\nThis note was not cut from its page's text: it was read from a screenshot of the page or from an element's markup, so a note cut from the page's text may hold the same record in other words. Which of these notes holds the record this one gives? Note: {}",
                dumps(&reading.note)
            );
            questions
                .insert(format!("t{index}/s{note}c{chunk}"), Question::Choice(ChoiceQuestion::new(question, criteria)));
        }
    }
    questions
}

/// `records` less each one Jev said repeats another, or None when Jev left one of those questions unanswered.
fn distinct(
    index: usize,
    records: Vec<usize>,
    found: &[Candidate],
    questions: &IndexMap<String, Question>,
    answered: Option<&IndexMap<String, Answer>>,
) -> Option<Vec<usize>> {
    let mut kept = Vec::new();
    for note in records {
        let prefix = format!("t{index}/s{note}c");
        let mut repeats = false;
        for key in questions.keys().filter(|key| key.starts_with(&prefix)) {
            let Some(Answer::Choice(answer)) = answered.and_then(|answered| answered.get(key)) else {
                return None;
            };
            let picked = answer.choice.strip_prefix('m').and_then(|n| n.parse::<usize>().ok());
            match picked.filter(|n| *n < found.len()) {
                Some(_) => repeats = true,
                None if answer.choice == NO_VALUE => {}
                None => return None,
            }
        }
        if !repeats {
            kept.push(note);
        }
    }
    Some(kept)
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
            let span = (evidence.capture_sha256.clone(), evidence.start, evidence.end);
            let (literal, page) = (evidence.rendered_text, address(&evidence.url));
            Candidate { id, quote: evidence.quote.clone(), note: shown(notes, evidence), span, literal, page }
        })
        .collect()
}

/// A note as a membership question shows it.
fn shown(notes: &Notes, evidence: &fastbrowse_core::models::Evidence) -> serde_json::Value {
    let mut note = json!({"quote": evidence.quote, "page": evidence.url, "headings": evidence.heading_path});
    let blocks = notes.table_blocks(evidence);
    if let Some(block) = blocks.iter().find(|block| block.start <= evidence.start && evidence.start < block.end) {
        let count = blocks.last().map_or(0, |last| last.table + 1);
        note["table"] = format!("table {} of the {count} on its page", block.table + 1).into();
    }
    note
}

fn membership_state(task: &str, notes: &Notes) -> serde_json::Value {
    // A date note alone does not show the separate label that makes it the current record. Keep source context
    // before candidate deduplication, since identical labels on different pages still describe different records.
    let context: Vec<_> = notes
        .facts()
        .into_iter()
        .filter(|fact| !notes.superseded(&fact_id(fact)))
        .filter_map(|fact| fact.evidence.as_ref())
        .map(|evidence| shown(notes, evidence))
        .collect();
    json!({"task": task, "notes": context})
}

/// The first note a claim cites, as a membership question shows it: the composer's own example of the record it
/// counts.
fn example(notes: &Notes, cited: &[String]) -> Option<serde_json::Value> {
    let facts = notes.facts_by_id();
    notes
        .expand_evidence_ids(cited.iter().map(String::as_str))
        .iter()
        .find_map(|id| facts.get(id).and_then(|fact| fact.evidence.as_ref()))
        .map(|evidence| shown(notes, evidence))
}

/// Whether a count includes this whole record, rather than a mention or part of it.
fn member_question(tally: &Tally, note: &serde_json::Value, example: Option<&serde_json::Value>) -> Question {
    let of = text::repr(&tally.of);
    // Asked about a biography that quotes its author, with only `of` ("quote by J.K. Rowling across the whole site")
    // to go on, Jev said it was one such quote in 3 replays of 10, and the count was one too many. Shown a record the
    // claim cites, it said no 10 times of 10, and still said yes to every quote record.
    let example = example.map_or(String::new(), |example| {
        format!(
            " The claim cites records such as this one: {}. A note is a record only when it is an entry of that same kind, not a passage that mentions or quotes one.",
            dumps(example)
        )
    });
    let question = format!(
        "Is this note exactly one whole record among {of}? A note holding several records is not one, and nor is a record's tags, label or heading alone, or a value a page states across several records, such as a total.{example}"
    );
    let yes = format!("The note is exactly one whole record among {of}.");
    let no = format!("The note is no record among {of}, several, or only part of one.");
    Question::Noul(NoulQuestion::new(format!("{UNTRUSTED}\n{question} Note: {}", dumps(note)), Some(&yes), Some(&no)))
}

const NO_VALUE: &str = "none";
const NO_RECORD: &str = "no_record";

/// Qualification and the field are one decision: independent questions can admit an unrelated date, then reject it.
fn field_question(tally: &Tally, note: &serde_json::Value, offers: &[String], grouped: bool) -> Question {
    let of = text::repr(&tally.of);
    let field = text::repr(tally.field.as_deref().unwrap_or("value"));
    let mut criteria: IndexMap<String, serde_json::Value> = offers
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let key = format!("{}{index}", if grouped { "g" } else { "n" });
            let value = if grouped { format!("one of {value}") } else { text::repr(value) };
            (
                key,
                json!(format!(
                    "{value} is its {field}: the note gives one {of} with its {field}, or the {field} of one."
                )),
            )
        })
        .collect();
    criteria.insert(NO_RECORD.into(), json!(format!(
        "The note is not one whole {of} and does not give the {field} of one. A name, label, heading, link or unrelated field alone is not a whole record. Several records, and a value stated across records such as a total or a starting price, are not one record."
    )));
    criteria.insert(NO_VALUE.into(), json!(format!("The note gives one whole {of}, but does not state its {field}. A name, label, heading, link or part alone is not a whole record.")));
    Question::Choice(ChoiceQuestion::new(
        format!(
            "{UNTRUSTED}\nDoes this note give exactly one whole record among {of}, or its {field}? If it does, which quoted value is that record's {field}? Note: {}",
            dumps(note)
        ),
        criteria,
    ))
}

#[derive(Debug)]
enum FieldAnswer {
    Value(String),
    NoRecord,
    NoValue,
    Unjudged,
}

fn field_answer(answer: Option<&Answer>, offers: &[String]) -> FieldAnswer {
    let Some(Answer::Choice(answer)) = answer else { return FieldAnswer::Unjudged };
    match answer.choice.as_str() {
        NO_RECORD => FieldAnswer::NoRecord,
        NO_VALUE => FieldAnswer::NoValue,
        choice => choice
            .strip_prefix('n')
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|n| offers.get(n))
            .map_or(FieldAnswer::Unjudged, |value| FieldAnswer::Value(value.clone())),
    }
}

const RECORD: &str = "The record's id, such as r0.";
const GROUP_TEXT: &str = "The record's value for the field named, copied character for character from its quote. Null when its quote does not state one.";

/// One record's group as its note writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordGroup {
    pub record: String,
    pub text: Option<String>,
}

impl Schema for RecordGroup {
    fn ty() -> Ty {
        let fields = vec![
            Field::required("record", Ty::Str).doc(RECORD),
            Field::required("text", Ty::optional(Ty::Str)).doc(GROUP_TEXT),
        ];
        ModelTy::frozen("RecordGroup", fields).ty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordGroups {
    pub records: Vec<RecordGroup>,
}

impl Schema for RecordGroups {
    fn ty() -> Ty {
        let field = Field::required("records", Ty::list(RecordGroup::ty())).doc("Every record listed, once each.");
        ModelTy::frozen("RecordGroups", vec![field]).ty()
    }
}

const GROUPS_INSTRUCTIONS: &str = "# Groups\nEach record below is one note a browser agent read, listed by its id. For every record, copy the value of the field named from the record's quote, exactly as the quote writes it, or give null when its quote does not state one. Code groups and counts the records by these values, so give each record's own value as written, never one taken from another record, corrected or shortened.\n\n# Trust\n";

/// The groups of a count per group: each group's text as first read, and its records in reading order, keyed by the
/// text with its whitespace squashed.
type Groups = IndexMap<String, (String, Vec<usize>)>;

fn groups_messages(by: &str, of: &str, lines: &[String]) -> [Message; 2] {
    let field =
        format!("# Field\nThe {} of each {}\n\n# Records\n{}", text::repr(by), text::repr(of), lines.join("\n"));
    [Message::system(format!("{GROUPS_INSTRUCTIONS}{UNTRUSTED}")), Message::user(field)]
}

/// Each record's `by` read by the text model, which can copy a name Jev could only choose from a list. A read is kept
/// only when it is text of the record's own quote, as a claim's value must be, so a group is never one the model
/// supplied. None when a record was left unread, read twice, read as text its quote does not hold or read as stating
/// no group, so no count rests on a record whose group is unknown.
async fn read_groups(
    llm: &dyn LLMClient,
    by: &str,
    of: &str,
    records: &[usize],
    found: &[Candidate],
    ledger: Option<&Ledger>,
) -> Result<Option<Groups>> {
    let lines: Vec<String> = records.iter().map(|note| format!("r{note}: {}", dumps(&found[*note].note))).collect();
    // The records go in as few requests as fit the model's context, which is the provider's limit.
    let schema = Typed::<RecordGroups>::default();
    let limit = match llm.context_tokens(LLMPurpose::Groups).await {
        Some(context) => Some(context_limit(context)?),
        None => None,
    };
    let fits =
        |lines: &[String]| limit.is_none_or(|limit| request_bytes(&groups_messages(by, of, lines), &schema) <= limit);
    let mut parts: Vec<Vec<String>> = Vec::new();
    for line in lines {
        match parts.last_mut() {
            Some(part) if fits(&[part.as_slice(), std::slice::from_ref(&line)].concat()) => part.push(line),
            _ if fits(std::slice::from_ref(&line)) => parts.push(vec![line]),
            // A record too long for any request cannot be read, so its group is not known.
            _ => return Ok(None),
        }
    }
    let mut read: HashMap<usize, Option<String>> = HashMap::new();
    for part in parts {
        let messages = groups_messages(by, of, &part);
        let generated = generate::<RecordGroups>(llm, LLMPurpose::Groups, &messages, ledger).await?;
        if let Some(ledger) = ledger {
            ledger.record([generated.cost])?;
        }
        for answer in generated.data.records {
            let note = answer.record.strip_prefix('r').and_then(|n| n.parse::<usize>().ok());
            let Some(note) = note.filter(|note| records.contains(note)) else { return Ok(None) };
            let quoted = answer.text.as_deref().map(text::squash);
            if quoted.as_deref().is_some_and(|text| text.is_empty() || !text::squash(&found[note].quote).contains(text))
            {
                return Ok(None);
            }
            if read.insert(note, answer.text).is_some() {
                return Ok(None);
            }
        }
    }
    let mut groups = Groups::new();
    for note in records {
        // A record whose note does not state its group is of unknown group, so it is not left out of the counts.
        let Some(Some(text)) = read.remove(note) else { return Ok(None) };
        groups.entry(text::squash(&text)).or_insert_with(|| (text.trim().to_string(), Vec::new())).1.push(*note);
    }
    Ok(Some(groups))
}

/// A count per group from each record's group: the group's count, and with a rank, its place among every group by
/// count, ties sharing a place as at an extreme.
fn per_group(
    tally: &Tally,
    by: &str,
    group: &str,
    groups: &Groups,
    found: &[Candidate],
) -> std::result::Result<Worked, Unworked> {
    let (of, by_repr) = (text::repr(&tally.of), text::repr(by));
    let mut order: Vec<(usize, &String)> = groups.values().map(|(name, members)| (members.len(), name)).collect();
    order.sort_by_key(|(count, _)| std::cmp::Reverse(*count));
    let listed = order.iter().map(|(n, name)| format!("{} {n}", text::repr(name))).collect::<Vec<_>>().join(", ");
    let Some(at) = groups.get_index_of(&text::squash(group)) else {
        let read = if listed.is_empty() {
            format!("No {of} read states its {by_repr}.")
        } else {
            format!("The {by_repr} of each {of} read, as the notes write it, with its count, most first: {listed}.")
        };
        return Err(Unworked::Draft(format!(
            "No {of} read has the {by_repr} {}. {read} Set tally.group to one of these as written.",
            text::repr(group)
        )));
    };
    let count = groups[at].1.len();
    // A place among the groups rests on every group's count, so a ranked claim cites every grouped record, its own
    // group's first; a plain count rests on its group's records alone.
    let cited: Vec<usize> = match tally.rank {
        Some(_) => std::iter::once(at).chain((0..groups.len()).filter(|other| *other != at)).collect(),
        None => vec![at],
    };
    let evidence_ids = cited.iter().flat_map(|group| &groups[*group].1).map(|note| found[*note].id.clone()).collect();
    if let Some(rank) = tally.rank {
        let mut counts: Vec<usize> = groups.values().map(|(_, members)| members.len()).collect();
        counts.sort_unstable_by(|a, b| b.cmp(a));
        counts.dedup();
        let place = counts.iter().position(|&each| each == count).map_or(0, |at| at as u64 + 1);
        if place != rank {
            return Err(Unworked::Draft(format!(
                "The claim's tally places {} at {rank}, but its {count} {of} put it at place {place} by count. The \
                 count of each {by_repr}, most first: {listed}.",
                text::repr(group),
            )));
        }
    }
    Ok(Worked { value: count.to_string(), evidence_ids })
}

/// What narrowing a note's numbers to a choice Jev can hold found.
enum Narrowed {
    /// The numbers that still may be the field, few enough to offer at once.
    Offer(Vec<String>),
    /// Jev said no group holds the field.
    NoValue,
    /// The note does not hold the requested record or its field.
    NoRecord,
    /// Jev left a group question unanswered.
    Unjudged,
}

/// The numbers of a note with more than a choice can offer, narrowed group by group to the one holding the field, as
/// a field read from many candidates is: a results block holding 259 numbers was sent as one choice, Jev refused it
/// for having more than 255 options, and the claim's support was left unanswered.
async fn narrowed(
    jev: &dyn DecisionClient,
    state: &serde_json::Value,
    tally: &Tally,
    note: &serde_json::Value,
    mut numbers: Vec<String>,
    tokens: &TokenBudget,
    ledger: Option<&Ledger>,
) -> Result<Narrowed> {
    let max = jev.profile().max_choice_options;
    // Two options distinguish an irrelevant note from a whole record whose field is missing.
    while numbers.len() + 2 > max {
        let size = numbers.len().div_ceil(max - 2);
        let groups: Vec<Vec<String>> = numbers.chunks(size).map(<[String]>::to_vec).collect();
        let offers = groups
            .iter()
            .map(|group| group.iter().map(|number| text::repr(number)).collect::<Vec<_>>().join(", "))
            .collect::<Vec<_>>();
        let question = field_question(tally, note, &offers, true);
        let key = "group".to_string();
        let answered = ask(jev, state, &IndexMap::from([(key.clone(), question)]), tokens, ledger).await?;
        let Some(Answer::Choice(answer)) = answered.as_ref().and_then(|answered| answered.get(&key)) else {
            return Ok(Narrowed::Unjudged);
        };
        if answer.choice == NO_VALUE {
            return Ok(Narrowed::NoValue);
        }
        if answer.choice == NO_RECORD {
            return Ok(Narrowed::NoRecord);
        }
        let Some(group) =
            answer.choice.strip_prefix('g').and_then(|n| n.parse::<usize>().ok()).and_then(|n| groups.get(n))
        else {
            return Ok(Narrowed::Unjudged);
        };
        numbers = group.clone();
    }
    Ok(Narrowed::Offer(numbers))
}

/// The values a note could hold its field as: each date it writes, whole and then its year, then its numbers. A comma
/// both groups thousands and separates the cells of a line, as in a CSV row, and the text alone cannot say which:
/// "INV-01,120.00" reads as 1,120.00 or as 01 and 120.00. Both readings are offered, and Jev picks the one that is the
/// field. Asked which number in "Released: May 14, 2026 (2026-05-14T19:25:26+0000)" is the latest release date, Jev
/// was offered only '2026', '05', '14' and the like, said none, and a correct answer was rejected for a record with no
/// date (#977); had it picked a day, code would have compared days. So a date's day and month are not offered alone,
/// since neither orders anything on its own. Its year is: a year orders on its own, and a field such as a founding
/// year is one, so a record dated "3 March 1998" is compared with another's "2001".
fn numbers(quote: &str) -> Vec<String> {
    let chars: Vec<char> = quote.chars().collect();
    let dated = dates(quote);
    let mut found: Vec<String> = Vec::new();
    for (start, end, value) in &dated {
        let date: String = chars[*start..*end].iter().collect();
        // `dates` values a date as its year times 10,000 plus its month and day, and reads a year of four digits.
        let year = format!("{:04}", (*value as u32) / 10_000);
        for each in [date, year] {
            if !found.contains(&each) {
                found.push(each);
            }
        }
    }
    let rest: String = chars
        .iter()
        .enumerate()
        .map(|(at, c)| if dated.iter().any(|(start, end, _)| (*start..*end).contains(&at)) { ' ' } else { *c })
        .collect();
    found.extend(spans(&rest, SpanKind::Number).into_iter().map(|span| span.2));
    let cells = rest.lines().flat_map(|line| line.split(','));
    for number in cells.flat_map(|cell| spans(cell, SpanKind::Number)).map(|span| span.2) {
        if !found.contains(&number) {
            found.push(number);
        }
    }
    found
}

/// The months as a date writes them, in calendar order: the full name, its first three letters, or "Sept". These are
/// the calendar's own names, the set a date parser reads in English, not a list fitted to pages; a date written in
/// another language is read as its numbers.
const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// Every date `text` writes, as code point spans with a value that orders them: an ISO 8601 date such as "2026-05-14",
/// or a day, month name and year in either order, such as "May 14, 2026" or "6 Dec 2024". A date is its day; a time
/// beside it is left as numbers, so two records on one day tie.
fn dates(text: &str) -> Vec<(usize, usize, f64)> {
    // A day the month does not have, such as 2025-02-31, is no date, so it orders nothing.
    let ordinal = |year: u32, month: u32, day: u32| {
        let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
        let days = match month {
            2 if leap => 29,
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            1..=12 => 31,
            _ => 0,
        };
        (1..=days).contains(&day).then(|| f64::from(year * 10_000 + month * 100 + day))
    };
    let number = |digits: &[char]| digits.iter().try_fold(0u32, |value, c| Some(value * 10 + u32::from(digit(*c)?)));
    let chars: Vec<char> = text.chars().collect();
    let mut found: Vec<(usize, usize, f64)> = spans(text, SpanKind::Date)
        .into_iter()
        .filter_map(|(start, end, _)| {
            let written = &chars[start..end];
            let value = ordinal(number(&written[..4])?, number(&written[5..7])?, number(&written[8..10])?)?;
            Some((start, end, value))
        })
        .collect();
    for (start, end) in prose_date().find_iter(&chars) {
        let written = &chars[start..end];
        let name = written.iter().filter(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_lowercase();
        let month =
            MONTHS.iter().position(|full| name == *full || name == full[..3] || name == "sept" && *full == "september");
        let runs: Vec<&[char]> = written.split(|c| !c.is_ascii_digit()).filter(|run| !run.is_empty()).collect();
        let (Some(month), [first, second]) = (month, runs.as_slice()) else { continue };
        let (day, year) = if first.len() == 4 { (second, first) } else { (first, second) };
        if let Some(value) = number(year).zip(number(day)).and_then(|(year, day)| ordinal(year, month as u32 + 1, day))
        {
            found.push((start, end, value));
        }
    }
    found.sort_by_key(|(start, _, _)| *start);
    found
}

/// A day, month name and year, in either order: "May 14, 2026", "Dec. 6 2024" or "6 December, 2024".
fn prose_date() -> Pat {
    let gap = || Pat::plus(Pat::class(|c| c == ' ' || c == '\t' || c == '\u{a0}'));
    let day = || Pat::repeat(Pat::class(|c| c.is_ascii_digit()), 1, 2);
    let year = || Pat::repeat(Pat::class(|c| c.is_ascii_digit()), 4, 4);
    let month = || {
        let names = MONTHS.iter().map(|full| Pat::ilit(full)).chain(std::iter::once(Pat::ilit("sept")));
        let short = MONTHS.iter().map(|full| Pat::ilit(&full[..3]));
        Pat::Seq(vec![
            Pat::alt(names.chain(short).collect()),
            Pat::opt(Pat::lit(".")),
            Pat::not_ahead(Pat::class(is_word)),
        ])
    };
    Pat::Seq(vec![
        Pat::not_behind(is_word),
        Pat::alt(vec![
            Pat::Seq(vec![month(), gap(), day(), Pat::opt(Pat::lit(",")), gap(), year()]),
            Pat::Seq(vec![day(), gap(), month(), Pat::opt(Pat::lit(",")), gap(), year()]),
        ]),
        Pat::not_ahead(Pat::class(is_word)),
    ])
}

/// A number as written, as code reads it: its value, its decimal places, and the currency sign it carries. A date is
/// one as well, valued so that a later date is a greater value, and is compared but never added.
#[derive(Debug, Clone, PartialEq)]
struct Amount {
    value: f64,
    places: usize,
    sign: Option<char>,
    written: String,
    date: bool,
}

/// `written` read as a number: one word of digits, beside at most currency signs and ISO 4217 codes such as "GBP".
/// Any other word makes it something else, such as a record's name: "Galaxy S20 Ultra" read as 20 had the worked
/// price written over its model number.
fn amount(written: &str) -> Option<Amount> {
    let trimmed = written.trim();
    if let [(0, end, value)] = dates(trimmed)[..]
        && end == trimmed.chars().count()
    {
        return Some(Amount { value, places: 0, sign: None, written: trimmed.to_string(), date: true });
    }
    let mut number = None;
    for word in written.split_whitespace() {
        if word.chars().any(|c| c.is_ascii_digit()) {
            let numeric = word.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | '-' | '+') || currency(c));
            if number.is_some() || !numeric {
                return None;
            }
            number = Some(word);
        } else if !(word.chars().all(currency) || word.len() == 3 && word.chars().all(|c| c.is_ascii_uppercase())) {
            return None;
        }
    }
    let sign = written.chars().find(|c| currency(*c));
    let digits: String = number?.chars().filter(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+')).collect();
    let value = digits.parse::<f64>().ok()?;
    let places = digits.split_once('.').map_or(0, |(_, places)| places.len());
    Some(Amount { value, places, sign, written: trimmed.to_string(), date: false })
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
    llm: &dyn LLMClient,
    task: &str,
    claims: &[(usize, &Tally, &str, &[String])],
    notes: &Notes,
    tokens: &TokenBudget,
    ledger: Option<&Ledger>,
) -> Result<IndexMap<usize, std::result::Result<Worked, Unworked>>> {
    let mut worked = IndexMap::new();
    let found = candidates(notes);
    let state = json!({"task": task});
    let mut members = IndexMap::new();
    let source_state = membership_state(task, notes);
    let mut offered = HashMap::new();
    let mut fields = HashMap::new();
    // Each count per group's first claim of its kind and grouping field, and the claims it leads.
    let mut leaders: HashMap<(String, String), usize> = HashMap::new();
    let mut led: HashMap<usize, usize> = HashMap::new();
    // The groups of each count per group, by the claim leading it, or None when a record's group was left unread.
    let mut grouping: IndexMap<usize, Option<Groups>> = IndexMap::new();
    for (index, tally, text, cited) in claims {
        let times = if tally.written.trim().is_empty() { 0 } else { text.matches(tally.written.as_str()).count() };
        if times != 1 {
            // Code writes its value over `written`, so the reason names the claim and the rewrite that places it.
            // The two drafts need different rewrites, and code knows which it has: a claim missing the value needs
            // it written with its record (told to split instead, it moved the record to a claim no check could
            // support), and one repeating it needs the other records' values moved out (told only that it was
            // "not written once", it resent the claim 10 times of 10).
            let written = text::repr(&tally.written);
            let reason = if times == 0 {
                format!(
                    "Claim {index} does not write its tally.written {written}: written is the value as claim {index}'s own \
                     text writes it, which code replaces with the worked value. Set tally.written to the value exactly \
                     as claim {index} writes it, or rewrite claim {index} to write {written} once, with the record that \
                     has it when the task asks which record it is."
                )
            } else {
                format!(
                    "Claim {index} writes its tally.written {written} {times} times, and code writes the worked value \
                     in one place. Rewrite claim {index} to write {written} once, keeping in it the record that has \
                     that value, and state each other record's value in a claim of its own with tally null."
                )
            };
            worked.insert(*index, Err(Unworked::Draft(reason)));
            continue;
        }
        let grouped = tally.by.is_some() || tally.group.is_some();
        if grouped && (tally.op != Op::Count || tally.grouped().is_none()) {
            let reason = "The claim's tally groups records but is not a count with both by and group: a count per \
                          group sets op count, by to the field that groups the records, and group to the one this \
                          claim counts."
                .to_string();
            worked.insert(*index, Err(Unworked::Draft(reason)));
            continue;
        }
        let ranked = |op| op == Op::Sum || op == Op::Count && !grouped;
        if tally.place() == 0 || (tally.rank.is_some() && ranked(tally.op)) {
            let reason =
                "The claim's tally sets a rank, which counts places from a minimum or maximum, from 2 for the \
                          second, or a group's place by its count, from 1 for the most: a total or a count of one \
                          kind has no rank, and the extreme itself has none."
                    .to_string();
            worked.insert(*index, Err(Unworked::Draft(reason)));
            continue;
        }
        // Code writes a count or total over `written`, so words there would be lost.
        if matches!(tally.op, Op::Count | Op::Sum) && amount(&tally.written).is_none_or(|amount| amount.date) {
            let reason = format!(
                "Claim {index}'s tally.written {} is not a number, and code writes the worked number in its place. Set \
                 written to the {} as claim {index} writes it, and keep the record's name in the claim's text.",
                text::repr(&tally.written),
                tally.field.as_deref().map_or("count".to_string(), text::repr),
            );
            worked.insert(*index, Err(Unworked::Draft(reason)));
            continue;
        }
        if tally.op != Op::Count && tally.field.is_none() {
            let reason = "The claim's tally adds or compares records but names no field.".to_string();
            worked.insert(*index, Err(Unworked::Draft(reason)));
            continue;
        }
        // The counts of one kind of record per group are worked out together: each record is judged and its group
        // read once.
        if let Some((by, _)) = tally.grouped() {
            let leader = *leaders.entry((tally.of.clone(), by.to_string())).or_insert(*index);
            led.insert(*index, leader);
            if leader != *index {
                continue;
            }
        }
        let example = example(notes, cited);
        for (note, candidate) in found.iter().enumerate() {
            if tally.op == Op::Count {
                members.insert(format!("t{index}/n{note}"), member_question(tally, &candidate.note, example.as_ref()));
            } else {
                match narrowed(jev, &source_state, tally, &candidate.note, numbers(&candidate.quote), tokens, ledger)
                    .await?
                {
                    Narrowed::Offer(numbers) => {
                        members.insert(
                            format!("t{index}/n{note}"),
                            field_question(tally, &candidate.note, &numbers, false),
                        );
                        offered.insert((*index, note), numbers);
                    }
                    Narrowed::NoRecord => {
                        fields.insert((*index, note), FieldAnswer::NoRecord);
                    }
                    Narrowed::NoValue => {
                        fields.insert((*index, note), FieldAnswer::NoValue);
                    }
                    Narrowed::Unjudged => {
                        fields.insert((*index, note), FieldAnswer::Unjudged);
                    }
                }
            }
        }
    }
    let answered = ask(jev, &source_state, &members, tokens, ledger).await?;
    for ((index, note), numbers) in offered {
        let answer = answered.as_ref().and_then(|a| a.get(&format!("t{index}/n{note}")));
        fields.insert((index, note), field_answer(answer, &numbers));
    }
    let member = |index: usize, note: usize| {
        let index = led.get(&index).copied().unwrap_or(index);
        match fields.get(&(index, note)) {
            Some(FieldAnswer::Value(_) | FieldAnswer::NoValue) => Some(true),
            Some(FieldAnswer::NoRecord) => Some(false),
            Some(FieldAnswer::Unjudged) => None,
            None => match answered.as_ref().and_then(|a| a.get(&format!("t{index}/n{note}"))) {
                Some(Answer::Noul(answer)) if (0.0..=1.0).contains(&answer.probability) => {
                    Some(answer.probability > 0.5)
                }
                _ => None,
            },
        }
    };
    // Each claim's records Jev judged, whole, before any repeat among them is dropped.
    let mut judged: HashMap<usize, Option<Vec<usize>>> = HashMap::new();
    let mut repeats = IndexMap::new();
    for (index, _, _, _) in claims {
        if worked.contains_key(index) {
            continue;
        }
        let records = (0..found.len())
            .map(|note| member(*index, note).map(|is| is.then_some(note)))
            .collect::<Option<Vec<_>>>()
            .map(|notes| whole(notes.into_iter().flatten().collect(), &found));
        if let Some(records) = &records {
            let asking = led.get(index).copied().unwrap_or(*index);
            repeats.extend(repeat_questions(asking, records, &found, jev.profile().max_choice_options));
        }
        judged.insert(*index, records);
    }
    let repeated = ask(jev, &state, &repeats, tokens, ledger).await?;
    let mut pending = Vec::new();
    for (index, tally, _, _) in claims {
        if worked.contains_key(index) {
            continue;
        }
        let asking = led.get(index).copied().unwrap_or(*index);
        let records = judged.remove(index).flatten();
        let Some(records) = records.and_then(|records| distinct(asking, records, &found, &repeats, repeated.as_ref()))
        else {
            worked.insert(*index, Err(Unworked::Unjudged));
            continue;
        };
        if let Some((by, _)) = tally.grouped() {
            let leader = led[index];
            if leader == *index {
                grouping.insert(*index, read_groups(llm, by, &tally.of, &records, &found, ledger).await?);
            }
            continue;
        }
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
        pending.push((*index, *tally, records));
    }
    for (index, tally, _, _) in claims {
        let Some((by, group)) = tally.grouped() else { continue };
        if worked.contains_key(index) {
            continue;
        }
        let leader = led[index];
        let result = match grouping.get(&leader) {
            Some(Some(groups)) => per_group(tally, by, group, groups, &found),
            Some(None) => Err(Unworked::Unjudged),
            // The claim leading its kind was sent back, so no record of it was grouped.
            None => Err(Unworked::Draft(format!("Claim {leader} counts the same records and was rejected."))),
        };
        worked.insert(*index, result);
    }
    for (index, tally, records) in pending {
        let mut amounts = Vec::new();
        for note in records {
            let chosen = match fields.get(&(index, note)) {
                Some(FieldAnswer::Value(value)) => Some(value.as_str()),
                Some(FieldAnswer::NoValue) => None,
                _ => {
                    worked.insert(index, Err(Unworked::Unjudged));
                    break;
                }
            };
            // A record whose field is not in its note is counted as having none, and so leaves no value to state.
            let Some(amount) = chosen.and_then(amount) else {
                let field = text::repr(tally.field.as_deref().unwrap_or("value"));
                let reason = format!("This note is one {} but states no {field}.", text::repr(&tally.of));
                worked.insert(index, Err(Unworked::Notes { reason, notes: vec![found[note].id.clone()] }));
                break;
            };
            amounts.push((note, amount));
        }
        if worked.contains_key(&index) {
            continue;
        }
        // A date is compared, never added, and a date and a number have no order between them.
        let dated: Vec<&str> =
            amounts.iter().filter(|(_, amount)| amount.date).map(|(note, _)| found[*note].id.as_str()).collect();
        if !dated.is_empty() && (tally.op == Op::Sum || dated.len() < amounts.len()) {
            let field = text::repr(tally.field.as_deref().unwrap_or("value"));
            let notes = dated.join(", ");
            let reason = if tally.op == Op::Sum {
                format!("Notes {notes} give the {field} as a date, which a total cannot add.")
            } else {
                format!(
                    "Notes {notes} give the {field} as a date and other records' notes as a number, which code cannot compare."
                )
            };
            worked.insert(index, Err(Unworked::Draft(reason)));
            continue;
        }
        let result = match tally.op {
            Op::Sum => {
                let all: Vec<&Amount> = amounts.iter().map(|(_, amount)| amount).collect();
                let evidence_ids = amounts.iter().map(|(note, _)| found[*note].id.clone()).collect();
                Worked { value: total(&all), evidence_ids }
            }
            Op::Min | Op::Max => {
                // Places count distinct values, so two records tied at the lowest are both first, and the next value
                // is second.
                let mut values: Vec<f64> = amounts.iter().map(|(_, amount)| amount.value).collect();
                values.sort_by(|a, b| if tally.op == Op::Min { a.total_cmp(b) } else { b.total_cmp(a) });
                values.dedup();
                let Some(&at) = values.get(tally.place() as usize - 1) else {
                    let reason = format!(
                        "The claim's tally asks for place {} among the {} of every {}, but the notes read give only {} \
                         different values.",
                        tally.place(),
                        text::repr(tally.field.as_deref().unwrap_or("value")),
                        text::repr(&tally.of),
                        values.len()
                    );
                    worked.insert(index, Err(Unworked::Draft(reason)));
                    continue;
                };
                // Every record at that place is cited, so a tie is not settled by reading order.
                let winners: Vec<&(usize, Amount)> = amounts.iter().filter(|(_, amount)| amount.value == at).collect();
                let evidence_ids = winners.iter().map(|(note, _)| found[*note].id.clone()).collect();
                if tally.names_record() {
                    // The claim's answer is the record, which the record check judges, so its words stay.
                    worked.insert(index, Ok(Worked { value: tally.written.clone(), evidence_ids }));
                    continue;
                }
                let winner = &winners[0].1;
                if winner.date {
                    // A date is written whole, so the claim keeps its own form of the date when it is the one found.
                    let same =
                        amount(&tally.written).is_some_and(|claimed| claimed.date && claimed.value == winner.value);
                    let value = if same { tally.written.clone() } else { winner.written.clone() };
                    worked.insert(index, Ok(Worked { value, evidence_ids }));
                    continue;
                }
                Worked { value: winner.written.clone(), evidence_ids }
            }
            Op::Count => unreachable!("a count is worked out above"),
        };
        worked.insert(index, Ok(Worked { value: as_written(&result.value, &tally.written), ..result }));
    }
    Ok(worked)
}

/// A worked amount in the form the claim writes its value: code puts its number where the claim's number is and keeps
/// the words around it, so "GBP 84.99" keeps its code where the note gave the number alone, and a claim whose
/// `written` leaves the currency sign in the text beside it ("$" then "251.00") is not given a second sign.
fn as_written(value: &str, written: &str) -> String {
    let number: String = value.chars().filter(|c| !currency(*c)).collect();
    let digit = |c: char| c.is_ascii_digit();
    match (written.find(digit), written.rfind(digit)) {
        (Some(start), Some(end)) => {
            // A sign written just before the number belongs to it, so the worked value's own sign takes its place.
            let start =
                written[..start].char_indices().rev().find(|(_, c)| !currency(*c)).map_or(0, |(i, c)| i + c.len_utf8());
            let sign: String = value.chars().filter(|c| currency(*c)).collect();
            let sign = if written[start..].chars().next().is_some_and(currency) { sign } else { String::new() };
            format!("{}{sign}{number}{}", &written[..start], &written[end + 1..])
        }
        _ => value.to_string(),
    }
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
    use fastbrowse_core::BoxFuture;
    use fastbrowse_core::llm::{Generation, LLMError, ResponseSchema};
    use fastbrowse_core::models::{CostBasis, CostComponent, CostLine};

    use super::*;

    #[test]
    fn membership_context_keeps_separate_page_labels_and_uncited_fields() {
        use crate::memory::Fact;
        use fastbrowse_core::models::{Evidence, FactReader};

        let fact = |page: &str, quote: &str, start: usize| {
            let evidence: Evidence = serde_json::from_value(json!({
                "source_id": "s1", "url": page, "frame_id": null,
                "captured_at": "2026-01-01T00:00:00Z", "capture_sha256": page,
                "start": start, "end": start + quote.len(), "quote": quote,
                "heading_path": ["Product", "Offer"],
            }))
            .unwrap();
            Fact::read(quote, evidence, FactReader::JevChoice)
        };
        let mut old = fact("https://b.test", "Expired offer", 40);
        old.evidence.as_mut().unwrap().capture_sha256 = "before".into();
        let mut notes = Notes::new([
            fact("https://a.test", "Current offer", 0),
            fact("https://b.test", "Current offer", 0),
            fact("https://b.test", "25", 20),
            old,
        ]);
        notes.supersede("https://b.test", "https://b.test", "Current offer 25");
        let state = membership_state("Compare current offers", &notes);
        assert_eq!(state["task"], "Compare current offers");
        let context = state["notes"].as_array().unwrap();
        assert_eq!(context.len(), 3);
        assert_eq!(context[0]["page"], "https://a.test");
        assert_eq!(context[1]["page"], "https://b.test");
        assert_eq!(context[0]["quote"], context[1]["quote"]);
        assert_eq!(context[2]["quote"], "25");
        assert_eq!(context[2]["headings"], json!(["Product", "Offer"]));
    }

    #[test]
    fn a_comma_separated_line_offers_each_cell_as_well_as_the_grouped_number() {
        assert_eq!(super::numbers("reference,item,total\nA-1001,Blue Kettle,34.50"), ["1001", "34.50"]);
        assert_eq!(super::numbers("invoice,amount\nINV-01,120.00"), ["01,120.00", "01", "120.00"]);
        assert_eq!(super::numbers("Total: $1,234.50"), ["$1,234.50", "$1", "234.50"]);
    }

    #[test]
    fn a_date_is_offered_whole_and_ordered_by_its_day() {
        let quote = "Released: May 14, 2026 (2026-05-14T19:25:26+0000)";
        assert_eq!(super::numbers(quote), ["May 14, 2026", "2026", "2026-05-14", "25", "26", "0000"]);
        assert_eq!(super::numbers("6 Dec. 2024, 3 files"), ["6 Dec. 2024", "2024", "3"]);
        let [later, earlier] = ["2026-05-14", "December 16, 2024"].map(|date| amount(date).unwrap());
        assert!(later.date && earlier.date && later.value > earlier.value);
        assert_eq!(amount("May 14, 2026").unwrap().value, later.value);
        assert_eq!(amount("2026-13-01"), None, "no thirteenth month");
        assert!(super::dates("2025-02-31 and February 29, 2025").is_empty(), "no day the month lacks");
        assert_eq!(super::dates("2024-02-29 and 29 Feb 2000").len(), 2, "a leap year's 29 February");
        assert!(super::dates("1900-02-29").is_empty(), "a century not divisible by 400 is no leap year");
        assert_eq!(super::numbers("Mayday 14, 2026"), ["2026", "14"], "a month is a whole word");
    }

    #[test]
    fn amounts_keep_their_sign_and_places() {
        let found = amount("£10.69").unwrap();
        assert_eq!((found.value, found.places, found.sign), (10.69, 2, Some('£')));
        assert_eq!(amount("1,234").unwrap().value, 1234.0);
        assert_eq!(amount("GBP 84.99").unwrap().value, 84.99);
        assert_eq!(amount("Galaxy S20 Ultra"), None, "a name with a model number is not a number");
        assert_eq!(amount("Pixel 2"), None);
        let (a, b) = (amount("£1.5").unwrap(), amount("£2.25").unwrap());
        assert_eq!(total(&[&a, &b]), "£3.75");
        assert_eq!(total(&[&a, &amount("2").unwrap()]), "3.5");
    }

    #[test]
    fn a_worked_amount_carries_a_sign_only_when_the_claim_writes_one_in_its_value() {
        assert_eq!(as_written("$251.00", "251.00"), "251.00", "the claim writes the sign before its value");
        assert_eq!(as_written("$251.00", "$251"), "$251.00");
        assert_eq!(as_written("3.5", "3"), "3.5");
        assert_eq!(as_written("84.99", "GBP 84.99"), "GBP 84.99", "the claim's currency code stays");
        assert_eq!(as_written("£12.00", "£12"), "£12.00");
    }

    #[test]
    fn a_line_inside_a_counted_card_is_not_counted_again() {
        let note = |capture: &str, start, end| Candidate {
            id: String::new(),
            quote: String::new(),
            note: json!({}),
            span: (capture.into(), start, end),
            literal: true,
            page: String::new(),
        };
        let found = [note("a", 0, 50), note("a", 0, 20), note("b", 0, 20), note("a", 60, 90)];
        assert_eq!(whole(vec![0, 1, 2, 3], &found), [0, 2, 3], "only the line inside card 0 on its capture goes");
        assert_eq!(whole(vec![1, 3], &found), [1, 3], "a line whose card is not a member still counts");
    }

    /// Reads every listed record's group as `reads` gives it by the record's id.
    struct ReadsGroups<'a>(&'a [(&'a str, Option<&'a str>)]);

    impl LLMClient for ReadsGroups<'_> {
        fn generate_value<'a>(
            &'a self,
            purpose: LLMPurpose,
            _: &'a [Message],
            schema: &'a dyn ResponseSchema,
            _: Option<&'a Ledger>,
        ) -> BoxFuture<'a, std::result::Result<Generation<serde_json::Value>, LLMError>> {
            assert_eq!(purpose, LLMPurpose::Groups);
            let records: Vec<serde_json::Value> =
                self.0.iter().map(|(record, text)| json!({"record": record, "text": text})).collect();
            Box::pin(async move {
                let data = schema.validate_json(&json!({"records": records}).to_string()).map_err(LLMError::Failed)?;
                let mut cost = CostLine::new(CostComponent::Llm, CostBasis::Estimated, Some(0.001));
                cost.purpose = Some(purpose);
                cost.physical_requests = Some(1);
                Ok(Generation { data, cost })
            })
        }
    }

    #[tokio::test]
    async fn grouping_records_cost_even_when_the_groups_are_unusable() {
        let found = [Candidate {
            id: String::new(),
            quote: "One by Ada".into(),
            note: json!({"quote": "One by Ada"}),
            span: (String::new(), 0, 0),
            literal: true,
            page: String::new(),
        }];
        for (record, text, usable) in [("r0", Some("Ada"), true), ("r0", None, false), ("r1", Some("Ada"), false)] {
            let ledger = Ledger::new(Default::default());
            let responses = [(record, text)];
            let llm = ReadsGroups(&responses);
            let groups = read_groups(&llm, "author", "quote", &[0], &found, Some(&ledger)).await.unwrap();
            assert_eq!(groups.is_some(), usable);
            let lines = ledger.state().lines.clone();
            assert_eq!(lines.len(), 1, "a received answer is charged before its groups are checked");
            assert_eq!(lines[0].component, CostComponent::Llm);
            assert_eq!(lines[0].dollars, Some(0.001));
            assert_eq!(lines[0].purpose, Some(LLMPurpose::Groups));
            assert_eq!(lines[0].physical_requests, Some(1));
            let capped =
                Ledger::new(fastbrowse_core::models::Limits { max_dollars: Some(0.0005), ..Default::default() });
            let result = read_groups(&llm, "author", "quote", &[0], &found, Some(&capped)).await;
            assert!(matches!(result, Err(AgentError::Budget(_))));
            assert_eq!(capped.state().lines.len(), 1, "the over-budget receipt remains reportable");
        }
    }

    /// A member whose note does not state its group is a record of unknown group, so no count per group rests on the
    /// others alone: with Bo's second quote unread for its author, Ada's two would lead a count that Bo ties.
    #[tokio::test]
    async fn a_member_whose_group_is_unstated_leaves_the_tally_unworked() {
        let note = |quote: &str| Candidate {
            id: String::new(),
            quote: quote.into(),
            note: json!({"quote": quote}),
            span: (String::new(), 0, 0),
            literal: true,
            page: String::new(),
        };
        let found = [note("One by Ada"), note("Two by Ada"), note("Three by Bo"), note("Four by Bo")];
        let records = [0, 1, 2, 3];
        let stated = ReadsGroups(&[("r0", Some("Ada")), ("r1", Some("Ada")), ("r2", Some("Bo")), ("r3", Some("Bo"))]);
        let groups = read_groups(&stated, "author", "quote", &records, &found, None).await.unwrap();
        assert!(groups.is_some(), "every member's group is read");
        let unstated = ReadsGroups(&[("r0", Some("Ada")), ("r1", Some("Ada")), ("r2", Some("Bo")), ("r3", None)]);
        let groups = read_groups(&unstated, "author", "quote", &records, &found, None).await.unwrap();
        assert!(groups.is_none(), "a member of unknown group leaves the tally unworked");
    }
}
