//! Jev's record reader: the page's run of records matched one by one against a requirement, so code counts them or
//! keeps them as a comparison's records, with only the records Jev is unsure of read by the LLM.
//!
//! Every counted record is a block code cut from the capture, so it is a quote.

use std::collections::HashSet;

use fastbrowse_core::config::TokenBudget;
use fastbrowse_core::decision::{Answer, DecisionClient, NoulQuestion, Question};
use fastbrowse_core::llm::{LLMClient, Message};
use fastbrowse_core::models::{CostLine, UNTRUSTED};
use fastbrowse_core::page::{Block, BlockKind, Capture};
use fastbrowse_core::schema::{Field, ModelTy, Schema, Ty};
use fastbrowse_core::telemetry::{Ledger, Trace};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, json};

use super::chunks::evidence;
use super::reading::{ReadOutcome, TallyReader, ask_reader, counted_kind, quoted};
use super::text::Page;
use crate::batches::evaluate_batches;
use crate::error::Result;
use crate::memory::{Fact, Notes, Tally, fact_id};
use crate::planner::Requirement;

/// A record Jev matches at or above the first, or rejects at or below the second, is settled; one between goes to the
/// LLM reader on its own. Jev's yes for a plain author or rating match sits at 0.78 to 0.84, and a wrongly dropped
/// record leaves no trace, so a rejection needs more certainty than a match.
const RECORD_MATCH_FROM: f64 = 0.70;
const RECORD_REJECT_TO: f64 = 0.20;

type Shape = (Vec<String>, Option<String>, BlockKind);

fn shape(block: &Block) -> Shape {
    (block.heading_path.clone(), block.frame_id.clone(), block.kind)
}

/// The page's list: the largest unbroken run of record blocks sharing a heading, a frame and a kind, or the one run of
/// `wanted`'s shape. A sidebar or a footer list sits under another heading and is not the list; a second run of the
/// list's own shape, a featured strip repeating its records, would be counted again on every page that repeats it, so
/// a page with one is not read here.
fn record_run<'a>(capture: &'a Capture, wanted: Option<&Shape>) -> Option<Vec<&'a Block>> {
    let mut runs: Vec<Vec<&Block>> = Vec::new();
    for block in capture.blocks.iter().filter(|block| counted_kind(block.kind)) {
        match runs.last_mut() {
            Some(run) if shape(run[0]) == shape(block) => run.push(block),
            _ => runs.push(vec![block]),
        }
    }
    let list = match wanted {
        Some(wanted) => wanted.clone(),
        None => {
            let longest = runs.iter().map(Vec::len).max()?;
            shape(runs.iter().find(|run| run.len() == longest)?[0])
        }
    };
    let mut of_shape = runs.into_iter().filter(|run| shape(run[0]) == list);
    match (of_shape.next(), of_shape.next()) {
        (Some(run), None) => Some(run),
        _ => None,
    }
}

fn record_question(requirement: &str, record: &str) -> Question {
    NoulQuestion::new(
        format!(
            "{UNTRUSTED}\n\n# Requirement\n{requirement}\n\n# Record\n{record}\n\nDoes this record pass every filter the requirement states, so that it is one of the records the requirement counts or compares? Judge this record alone; do not judge whether it wins."
        ),
        Some("Yes, this record matches the requirement."),
        Some("No, this record does not match."),
    )
    .into()
}

/// Whether the requirement's count is settled by this list as code reads it: every page of it when it goes on, or this
/// page when it does not. A count bounded to pages the task names, or a list that goes on behind another control, stays
/// with the LLM reader.
fn scope_question(requirement: &str, next_page: bool) -> Question {
    let (fact, asked, yes, no) = if next_page {
        (
            "This page shows one page of a list that goes on: code found a control to its next page, and pages through the list to its last page.",
            "Does the requirement count the matching records on every page of this list, rather than only on pages the task names?",
            "Yes, it counts every page of the list.",
            "No, it counts only some pages, or not this list.",
        )
    } else {
        (
            "Code found no control to a next page of this list.",
            "Does this page show every record the requirement counts, so that its count is complete on this page?",
            "Yes, this page shows the whole list the requirement counts.",
            "No, the list goes on elsewhere, such as behind a control that loads more, or it is not this list.",
        )
    };
    NoulQuestion::new(
        format!("{UNTRUSTED}\n\n# Requirement\n{requirement}\n\n# List\n{fact}\n\n{asked}"),
        Some(yes),
        Some(no),
    )
    .into()
}

fn probability(answers: &IndexMap<String, Answer>, key: &str) -> Option<f64> {
    match answers.get(key) {
        Some(Answer::Noul(answer)) => Some(answer.probability),
        _ => None,
    }
}

/// The records Jev kept for one requirement and those it left between its two bars or did not answer.
struct Matched<'a> {
    kept: Vec<&'a Block>,
    unsure: Vec<&'a Block>,
}

fn matched<'a>(answers: &IndexMap<String, Answer>, requirement_id: &str, records: &[&'a Block]) -> Matched<'a> {
    let mut found = Matched { kept: Vec::new(), unsure: Vec::new() };
    for (index, block) in records.iter().enumerate() {
        match probability(answers, &format!("{requirement_id}::{index}")) {
            Some(p) if p >= RECORD_MATCH_FROM => found.kept.push(block),
            Some(p) if p <= RECORD_REJECT_TO => {}
            _ => found.unsure.push(block),
        }
    }
    found
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordMatches {
    matching: Vec<String>,
}

impl Schema for RecordMatches {
    fn ty() -> Ty {
        ModelTy::frozen(
            "_RecordMatches",
            vec![
                Field::required("matching", Ty::list(Ty::Str))
                    .doc("The source ids of the records that pass every filter the requirement states."),
            ],
        )
        .ty()
    }
}

const RECORD_READER: &str = "# Records\nEach record below is one record of a list, after its [source id]. Return the source ids of the records that pass every filter the requirement states, judging each record alone. Code counts or compares the records you return, and ignores an id not shown here.";

/// The LLM's reading of the records Jev was unsure of, shown alone rather than with the page around them.
async fn read_unsure<'a>(
    llm: &dyn LLMClient,
    page: &Page<'_>,
    requirement: &str,
    unsure: &[&'a Block],
    tokens: &TokenBudget,
    ledger: Option<&Ledger>,
) -> Result<(Vec<&'a Block>, CostLine)> {
    let records: Vec<String> =
        unsure.iter().map(|block| format!("[{}] {}", block.source_id, page.slice(block.start, block.end))).collect();
    let messages = [
        Message::system(format!("{RECORD_READER}\n\n# Trust\n{UNTRUSTED}")),
        Message::user(format!("# Requirement\n{requirement}\n\n# Records\n{}", records.join("\n\n"))),
    ];
    let read = ask_reader::<RecordMatches>(llm, &messages, RecordMatches::ty(), tokens, ledger).await?;
    if let Some(ledger) = ledger {
        ledger.record([read.cost.clone()])?;
    }
    let matching: HashSet<&String> = read.data.matching.iter().collect();
    Ok((unsure.iter().filter(|block| matching.contains(&block.source_id)).copied().collect(), read.cost))
}

fn filter_event(trace: &Trace, reader: &str, requirement_id: &str, records: usize, matched: usize) {
    trace.event("record_filter", || {
        let mut fields = Map::new();
        fields.insert("reader".into(), reader.into());
        fields.insert("requirement".into(), requirement_id.into());
        fields.insert("records".into(), records.into());
        fields.insert("matched".into(), matched.into());
        fields
    });
}

/// The records kept for each requirement: Jev's matches, and those the LLM matched among the ones Jev was unsure of.
#[allow(clippy::too_many_arguments)]
async fn keep<'a>(
    llm: &dyn LLMClient,
    page: &Page<'_>,
    answers: &IndexMap<String, Answer>,
    requirement_id: &str,
    requirement: &str,
    records: &[&'a Block],
    tokens: &TokenBudget,
    ledger: Option<&Ledger>,
    trace: &Trace,
) -> Result<(Vec<&'a Block>, Vec<CostLine>)> {
    let Matched { mut kept, unsure } = matched(answers, requirement_id, records);
    filter_event(trace, "jev", requirement_id, records.len(), kept.len());
    let mut costs = Vec::new();
    if !unsure.is_empty() {
        let (read, cost) = read_unsure(llm, page, requirement, &unsure, tokens, ledger).await?;
        filter_event(trace, "llm", requirement_id, unsure.len(), read.len());
        kept.extend(read);
        costs.push(cost);
    }
    kept.sort_by_key(|block| block.start);
    Ok((kept, costs))
}

/// Each kept record as a fact in the notes, by key.
fn remember(page: &Page<'_>, kept: &[&Block], notes: &mut Notes) -> Vec<String> {
    kept.iter()
        .map(|block| {
            let fact = quoted(evidence(page, block, block.start, block.end));
            let key = fact_id(&fact);
            notes.add(fact);
            key
        })
        .collect()
}

/// What Jev settled of a page's counts: the facts it added to the notes, the requirements it settled, those whose list
/// goes on past the page with the reader each later page is matched by, and what the LLM cost.
#[derive(Default)]
pub(crate) struct Counted {
    pub facts: Vec<Fact>,
    pub settled: Vec<String>,
    pub continues: Vec<String>,
    pub readers: Vec<TallyReader>,
    pub cost_lines: Vec<CostLine>,
}

impl Counted {
    /// `outcome` with the lists Jev counted on this page going on to the next, every page of them needed.
    pub fn into_outcome(self, mut outcome: ReadOutcome) -> ReadOutcome {
        outcome.through_end.extend(self.continues.iter().cloned());
        outcome.continues.extend(self.continues);
        outcome.tally_readers.extend(self.readers);
        outcome
    }
}

/// `requirements`' counts read from the page's record run: Jev matches each record against the requirement's own text
/// and says whether this list is the whole of what the requirement counts. A requirement Jev cannot settle so is left
/// to the LLM reader with the whole page.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn count_records(
    jev: &dyn DecisionClient,
    llm: &dyn LLMClient,
    page: &Page<'_>,
    requirements: &[Requirement],
    next_page: bool,
    notes: &mut Notes,
    tokens: &TokenBudget,
    ledger: Option<&Ledger>,
    trace: &Trace,
) -> Result<Counted> {
    let capture = page.capture;
    let mut counted = Counted::default();
    // An unopened frame could hold records of the list.
    if requirements.is_empty() || capture.inaccessible_frames > 0 {
        return Ok(counted);
    }
    let Some(records) = record_run(capture, None) else { return Ok(counted) };
    let mut questions: IndexMap<String, Question> = IndexMap::new();
    for requirement in requirements {
        questions.insert(format!("{}::scope", requirement.id), scope_question(&requirement.text, next_page));
        for (index, block) in records.iter().enumerate() {
            questions.insert(
                format!("{}::{index}", requirement.id),
                record_question(&requirement.text, page.slice(block.start, block.end)),
            );
        }
    }
    let state = json!({"page": {"url": capture.url, "title": capture.title, "heading": records[0].heading_path}});
    let Some(answered) = evaluate_batches(jev, &state, &questions, tokens, ledger, None, true).await? else {
        return Ok(counted);
    };
    let answers = &answered.answers;
    for requirement in requirements {
        if probability(answers, &format!("{}::scope", requirement.id)).is_none_or(|p| p < RECORD_MATCH_FROM) {
            continue;
        }
        let (kept, costs) =
            keep(llm, page, answers, &requirement.id, &requirement.text, &records, tokens, ledger, trace).await?;
        counted.cost_lines.extend(costs);
        // A count of none has no record to cite, so on a list that ends here the LLM reader states it from the page.
        if kept.is_empty() && !next_page {
            continue;
        }
        let mut keys = remember(page, &kept, notes);
        if !kept.is_empty() {
            let tally =
                Tally { requirement_id: requirement.id.clone(), key: requirement.text.clone(), records: keys.clone() };
            keys.push(fact_id(&notes.add_tally(tally)?));
        }
        if next_page {
            counted.continues.push(requirement.id.clone());
            counted.readers.push(TallyReader {
                requirement_id: requirement.id.clone(),
                count_label: Some(requirement.text.clone()),
                title: capture.title.clone(),
                heading_path: records[0].heading_path.clone(),
                frame_id: records[0].frame_id.clone(),
                prefix: String::new(),
                suffix: String::new(),
                filter: Some(requirement.text.clone()),
                compares: false,
                record_kind: Some(records[0].kind),
                comparison: None,
            });
        } else {
            notes.complete_tallies(&requirement.id);
        }
        let held = notes.facts_by_id();
        counted.facts.extend(keys.iter().filter_map(|key| held.get(key)).map(|fact| (*fact).clone()));
        counted.settled.push(requirement.id.clone());
    }
    Ok(counted)
}

/// `filter_tallies`: a later page of a filtered count or comparison, its records matched by Jev in one request.
///
/// None when the page does not have the first page's shape, or a comparison's kept record has no value code can read:
/// the LLM reader then reads the page as before.
#[allow(clippy::too_many_arguments)]
pub async fn filter_tallies(
    jev: &dyn DecisionClient,
    llm: &dyn LLMClient,
    capture: &Capture,
    readers: &[TallyReader],
    requirement_ids: &[String],
    tokens: &TokenBudget,
    ledger: Option<&Ledger>,
    trace: &Trace,
) -> Result<Option<ReadOutcome>> {
    let wanted: HashSet<&String> = requirement_ids.iter().collect();
    let have: HashSet<&String> = readers.iter().map(|r| &r.requirement_id).collect();
    if readers.is_empty()
        || readers.iter().any(|reader| reader.filter.is_none())
        || wanted != have
        || capture.inaccessible_frames > 0
    {
        return Ok(None);
    }
    let shapes: HashSet<(&Vec<String>, &Option<String>, Option<BlockKind>)> =
        readers.iter().map(|reader| (&reader.heading_path, &reader.frame_id, reader.record_kind)).collect();
    let reader = &readers[0];
    let Some(kind) = reader.record_kind else { return Ok(None) };
    if shapes.len() != 1 || readers.iter().any(|reader| capture.title != reader.title) {
        return Ok(None);
    }
    let list = (reader.heading_path.clone(), reader.frame_id.clone(), kind);
    let Some(records) = record_run(capture, Some(&list)) else { return Ok(None) };
    let page = Page::new(capture);
    let mut questions: IndexMap<String, Question> = IndexMap::new();
    for reader in readers {
        let filter = reader.filter.as_deref().unwrap_or_default();
        for (index, block) in records.iter().enumerate() {
            questions.insert(
                format!("{}::{index}", reader.requirement_id),
                record_question(filter, page.slice(block.start, block.end)),
            );
        }
    }
    let state = json!({"page": {"url": capture.url, "title": capture.title, "heading": reader.heading_path}});
    let Some(answered) = evaluate_batches(jev, &state, &questions, tokens, ledger, None, true).await? else {
        return Ok(None);
    };
    // Code picks the winner from each kept record's label and value; a record it cannot read them from sends the page
    // to the LLM reader rather than leaving a record the comparison cannot rank.
    for reader in readers {
        let Some(comparison) = &reader.comparison else { continue };
        let Matched { kept, unsure } = matched(&answered.answers, &reader.requirement_id, &records);
        let unreadable = kept.iter().chain(&unsure).any(|block| {
            let text = page.slice(block.start, block.end);
            comparison.label.extract(text).is_none() || comparison.value.extract(text).is_none()
        });
        if unreadable {
            trace.event("record_filter", || {
                let mut fields = Map::new();
                fields.insert("reader".into(), "llm".into());
                fields.insert("reason".into(), "unreadable_value".into());
                fields
            });
            return Ok(None);
        }
    }
    let mut notes = Notes::default();
    let mut cost_lines = Vec::new();
    let mut continuation_records: IndexMap<String, Vec<String>> = IndexMap::new();
    for reader in readers {
        let filter = reader.filter.as_deref().unwrap_or_default();
        let (kept, costs) =
            keep(llm, &page, &answered.answers, &reader.requirement_id, filter, &records, tokens, ledger, trace)
                .await?;
        cost_lines.extend(costs);
        let keys = remember(&page, &kept, &mut notes);
        if reader.compares {
            continuation_records.insert(reader.requirement_id.clone(), keys);
        } else if !keys.is_empty() {
            notes.add_tally(Tally {
                requirement_id: reader.requirement_id.clone(),
                key: reader.count_label.clone().filter(|label| !label.is_empty()).unwrap_or_else(|| "records".into()),
                records: keys,
            })?;
        }
    }
    let mut outcome = ReadOutcome::new(notes.facts().into_iter().cloned().collect(), vec![0], 0, cost_lines);
    outcome.continues = readers.iter().map(|r| r.requirement_id.clone()).collect();
    outcome.continuation_records = continuation_records;
    Ok(Some(outcome))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use fastbrowse_core::BoxFuture;
    use fastbrowse_core::decision::{DecisionError, Evaluation, NoulAnswer, NoulTag};
    use fastbrowse_core::llm::{Generation, LLMError, ResponseSchema};
    use fastbrowse_core::models::{CostBasis, CostComponent, LLMPurpose};
    use serde_json::Value;

    use super::*;
    use crate::comparison::{NumericComparison, Order, QuotedField, complete_comparison};
    use crate::planner::RequirementKind;
    use crate::retrieval::reading::{ReadOptions, read};
    use crate::retrieval::testing::capture;
    use crate::support::testing::block_on;

    /// Answers each record question by whether its record holds `matches`, and each scope question yes.
    struct Jev {
        matches: &'static str,
        unsure: Option<&'static str>,
    }

    impl DecisionClient for Jev {
        fn evaluate<'a>(
            &'a self,
            _: &'a Value,
            questions: &'a IndexMap<String, Question>,
        ) -> BoxFuture<'a, std::result::Result<Evaluation, DecisionError>> {
            let answers = questions
                .iter()
                .map(|(key, question)| {
                    let Question::Noul(question) = question else { panic!("a yes/no question") };
                    let record = question.instructions.split("# Record\n").nth(1).unwrap_or_default();
                    let probability = if key.ends_with("::scope") {
                        0.9
                    } else if self.unsure.is_some_and(|unsure| record.contains(unsure)) {
                        0.5
                    } else if record.contains(self.matches) {
                        0.9
                    } else {
                        0.05
                    };
                    (key.clone(), Answer::Noul(NoulAnswer { kind: NoulTag, probability }))
                })
                .collect();
            Box::pin(async move {
                Ok(Evaluation {
                    model: "test".into(),
                    answers,
                    input_tokens: 1,
                    cost: CostLine::new(CostComponent::Jev, CostBasis::Estimated, Some(0.0)),
                    requests: 1,
                })
            })
        }
    }

    /// Replies `reply` to a record read and keeps what it was shown; None fails any call.
    struct Llm {
        reply: Option<Value>,
        shown: Mutex<Vec<String>>,
    }

    impl LLMClient for Llm {
        fn generate_value<'a>(
            &'a self,
            _: LLMPurpose,
            messages: &'a [Message],
            schema: &'a dyn ResponseSchema,
            _: i64,
            _: Option<&'a Ledger>,
        ) -> BoxFuture<'a, std::result::Result<Generation<Value>, LLMError>> {
            self.shown.lock().unwrap().push(messages[1].content.clone());
            Box::pin(async move {
                let reply = self.reply.as_ref().ok_or_else(|| LLMError::Failed("no LLM call expected".into()))?;
                let data = schema.validate_json(&reply.to_string()).map_err(LLMError::Failed)?;
                Ok(Generation { data, cost: CostLine::new(CostComponent::Llm, CostBasis::Estimated, Some(0.0)) })
            })
        }
    }

    fn packages() -> Capture {
        capture(&[
            (BlockKind::ListItem, "Oak - available"),
            (BlockKind::ListItem, "Elm - available"),
            (BlockKind::ListItem, "Pine - archived"),
        ])
    }

    fn reader(capture: &Capture, filter: &str, comparison: Option<NumericComparison>) -> TallyReader {
        TallyReader {
            requirement_id: "r".into(),
            count_label: Some(filter.into()),
            title: capture.title.clone(),
            heading_path: Vec::new(),
            frame_id: None,
            prefix: String::new(),
            suffix: String::new(),
            filter: Some(filter.into()),
            compares: comparison.is_some(),
            record_kind: Some(BlockKind::ListItem),
            comparison,
        }
    }

    #[test]
    fn a_first_page_is_counted_by_jev_with_no_llm_call() {
        let captured = packages();
        let jev = Jev { matches: "available", unsure: None };
        let llm = Llm { reply: None, shown: Mutex::new(Vec::new()) };
        let text = "Count every available package";
        let requirement =
            Requirement { count_records: true, ..Requirement::new("r", text, RequirementKind::Information) };
        let options = ReadOptions {
            jev: Some(&jev),
            requirements: vec![requirement],
            notice: "Next page: a control to the next page of this list.".into(),
            ..ReadOptions::default()
        };
        let mut notes = Notes::default();
        let ids = vec!["r".to_string()];
        let outcome = block_on(read(&llm, &captured, text, &ids, &mut notes, &options)).unwrap();
        assert!(llm.shown.lock().unwrap().is_empty());
        assert_eq!((outcome.continues.clone(), outcome.through_end.clone()), (ids.clone(), ids));
        assert_eq!(outcome.tally_readers, vec![reader(&captured, text, None)]);
        let tallies = notes.tallies();
        assert_eq!(tallies.len(), 1);
        let quotes: Vec<&str> = tallies[0]
            .records
            .iter()
            .map(|key| notes.facts_by_id()[key].evidence.as_ref().unwrap().quote.as_str())
            .collect();
        assert_eq!(quotes, ["Oak - available", "Elm - available"]);
    }

    #[test]
    fn a_record_jev_is_unsure_of_goes_to_the_llm_alone() {
        let captured = packages();
        let jev = Jev { matches: "Oak", unsure: Some("Elm") };
        let llm = Llm { reply: Some(json!({"matching": ["s1"]})), shown: Mutex::new(Vec::new()) };
        let filter = "Count every available package";
        let readers = [reader(&captured, filter, None)];
        let ids = vec!["r".to_string()];
        let outcome = block_on(filter_tallies(
            &jev,
            &llm,
            &captured,
            &readers,
            &ids,
            &TokenBudget::default(),
            None,
            &Trace::disabled(),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(*llm.shown.lock().unwrap(), [format!("# Requirement\n{filter}\n\n# Records\n[s1] Elm - available")]);
        let mut notes = Notes::default();
        outcome.merge_records(&mut notes).unwrap();
        let tallies = notes.tallies();
        let quotes: Vec<&str> = tallies[0]
            .records
            .iter()
            .map(|key| notes.facts_by_id()[key].evidence.as_ref().unwrap().quote.as_str())
            .collect();
        assert_eq!(quotes, ["Oak - available", "Elm - available"]);
    }

    #[test]
    fn code_picks_the_cheapest_of_the_records_jev_kept() {
        let captured = capture(&[
            (BlockKind::ListItem, "Oak - $5.00 in stock"),
            (BlockKind::ListItem, "Elm - $3.00 in stock"),
            (BlockKind::ListItem, "Pine - $1.00 sold out"),
        ]);
        let jev = Jev { matches: "in stock", unsure: None };
        let llm = Llm { reply: None, shown: Mutex::new(Vec::new()) };
        let comparison = NumericComparison {
            order: Order::Lowest,
            limit: 1,
            label: QuotedField::new("", " - "),
            value: QuotedField::new(" - ", " "),
        };
        let readers = [reader(&captured, "Find the cheapest package in stock", Some(comparison.clone()))];
        let ids = vec!["r".to_string()];
        let outcome = block_on(filter_tallies(
            &jev,
            &llm,
            &captured,
            &readers,
            &ids,
            &TokenBudget::default(),
            None,
            &Trace::disabled(),
        ))
        .unwrap()
        .unwrap();
        assert!(llm.shown.lock().unwrap().is_empty());
        let mut notes = Notes::default();
        outcome.merge_records(&mut notes).unwrap();
        assert!(complete_comparison(&mut notes, "r", &comparison));
        let ranked = notes.facts().into_iter().find(|fact| fact.comparison.is_some()).unwrap();
        assert_eq!(ranked.text, "Lowest 1:\nElm: $3.00");
    }
}
