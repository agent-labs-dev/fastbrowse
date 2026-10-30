//! Jev's record reader: a later page's run of records matched one by one against a requirement, so code counts them or
//! keeps them as a comparison's records, with only the records Jev is unsure of read by the LLM.
//!
//! Every counted record is a block code cut from the capture, so it is a quote.

use std::collections::HashSet;

use fastbrowse_core::config::TokenBudget;
use fastbrowse_core::decision::{Answer, DecisionClient, NoulQuestion, Question};
use fastbrowse_core::llm::{LLMClient, Message};
use fastbrowse_core::models::{CostLine, UNTRUSTED};
use fastbrowse_core::page::{Block, BlockKind, Capture};
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::py::text;
use fastbrowse_core::schema::{Field, ModelTy, Schema, Ty, json_schema};
use fastbrowse_core::telemetry::{Ledger, Trace};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, json};

use super::chunks::evidence;
use super::reading::{ReadOutcome, TallyReader, ask_reader, counted_kind, quoted};
use super::text::Page;
use crate::batches::evaluate_batches;
use crate::error::Result;
use crate::memory::{Notes, Tally, fact_id};

/// A record Jev matches at or above the first, or rejects at or below the second, is settled; one between goes to the
/// LLM reader on its own. Jev's yes for a plain author or rating match sits at 0.78 to 0.84, and a wrongly dropped
/// record leaves no trace, so a rejection needs more certainty than a match.
const RECORD_MATCH_FROM: f64 = 0.70;
const RECORD_REJECT_TO: f64 = 0.20;
/// A yes from here pages a list to its end; TypeSafe's documented starting bar for a yes/no question.
const EVERY_PAGE_FROM: f64 = 0.70;

type Shape = (Vec<String>, Option<String>, BlockKind);

fn shape(block: &Block) -> Shape {
    (block.heading_path.clone(), block.frame_id.clone(), block.kind)
}

/// The page's list: the one unbroken run of record blocks of `list`'s shape, the shape of the records the first page's
/// reader cited. A sidebar or a footer list sits under another heading and is not the list; a second run of the list's
/// own shape, a featured strip repeating its records, would be counted again on every page that repeats it, so a page
/// with one is not read here.
fn record_run<'a>(capture: &'a Capture, list: &Shape) -> Option<Vec<&'a Block>> {
    let mut runs: Vec<Vec<&Block>> = Vec::new();
    for block in capture.blocks.iter().filter(|block| counted_kind(block.kind)) {
        match runs.last_mut() {
            Some(run) if shape(run[0]) == shape(block) => run.push(block),
            _ => runs.push(vec![block]),
        }
    }
    let mut of_shape = runs.into_iter().filter(|run| shape(run[0]) == *list);
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

fn scope_question(requirement: &str) -> Question {
    NoulQuestion::new(
        format!(
            "{UNTRUSTED}\n\n# Requirement\n{requirement}\n\n# List\nThis page shows one page of a list that goes on to further pages.\n\nDoes the task need the matching records from every page of this list?"
        ),
        Some("Yes: the task covers the whole list, to its last page."),
        Some("No: the task names only some pages, or is about another list."),
    )
    .into()
}

/// The requirements, of those whose list the LLM reader found going on past this page, that Jev places on every page of
/// it, and what asking cost. Jev is asked with the task, because a task can bound the pages it covers ("this page and
/// the next"). The reader used to say this itself, and one LLM said no for a whole-site list another said yes for, so
/// which model was configured decided whether Jev or the LLM read every later page.
pub(crate) async fn through_end(
    jev: &dyn DecisionClient,
    task: &str,
    capture: &Capture,
    requirements: &[(String, String)],
    tokens: &TokenBudget,
    ledger: Option<&Ledger>,
) -> Result<(Vec<String>, Vec<CostLine>)> {
    let questions: IndexMap<String, Question> =
        requirements.iter().map(|(id, text)| (id.clone(), scope_question(text))).collect();
    let state = json!({"task": task, "page": {"url": capture.url, "title": capture.title}});
    let Some(answered) = evaluate_batches(jev, &state, &questions, tokens, ledger, None, true).await? else {
        return Ok((Vec::new(), Vec::new()));
    };
    let whole = requirements
        .iter()
        .filter(|(id, _)| probability(&answered.answers, id).is_some_and(|p| p >= EVERY_PAGE_FROM))
        .map(|(id, _)| id.clone())
        .collect();
    Ok((whole, answered.cost))
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

/// The LLM's reading of the records Jev was unsure of, shown alone rather than with the page around them, in as few
/// prompts as fit the LLM input budget. Each record is judged alone, so splitting them changes no answer; a record too
/// long for any prompt goes in one of its own, which the provider reports as too large.
async fn read_unsure<'a>(
    llm: &dyn LLMClient,
    page: &Page<'_>,
    requirement: &str,
    unsure: &[&'a Block],
    tokens: &TokenBudget,
    ledger: Option<&Ledger>,
) -> Result<(Vec<&'a Block>, Vec<CostLine>)> {
    let system = format!("{RECORD_READER}\n\n# Trust\n{UNTRUSTED}");
    let heading = format!("# Requirement\n{requirement}\n\n# Records\n");
    let fixed = text::len(&format!("{system}{heading}{}", dumps(&json_schema(&RecordMatches::ty())))) as i64;
    let room = tokens.llm_chars() - fixed;
    let mut groups: Vec<Vec<String>> = Vec::new();
    let mut used = 0;
    for block in unsure {
        let record = format!("[{}] {}", block.source_id, page.slice(block.start, block.end));
        let size = text::len(&record) as i64 + 2;
        match groups.last_mut() {
            Some(group) if used + size <= room => group.push(record),
            _ => {
                groups.push(vec![record]);
                used = 0;
            }
        }
        used += size;
    }
    let mut matching = HashSet::new();
    let mut costs = Vec::new();
    for records in groups {
        let messages = [Message::system(system.clone()), Message::user(format!("{heading}{}", records.join("\n\n")))];
        let read = ask_reader::<RecordMatches>(llm, &messages, RecordMatches::ty(), ledger).await?;
        if let Some(ledger) = ledger {
            ledger.record([read.cost.clone()])?;
        }
        matching.extend(read.data.matching);
        costs.push(read.cost);
    }
    Ok((unsure.iter().filter(|block| matching.contains(&block.source_id)).copied().collect(), costs))
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
        costs.extend(cost);
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
    let Some(records) = record_run(capture, &list) else { return Ok(None) };
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
    use fastbrowse_core::decision::{
        ChoiceAnswer, ChoiceTag, DecisionError, DecisionProfile, Evaluation, NoulAnswer, NoulTag,
    };
    use fastbrowse_core::llm::{Generation, LLMError, ResponseSchema};
    use fastbrowse_core::models::{CostBasis, CostComponent, LLMPurpose};
    use serde_json::Value;

    use super::*;
    use crate::comparison::{NumericComparison, Order, QuotedField, complete_comparison};
    use crate::planner::{Requirement, RequirementKind};
    use crate::retrieval::reading::{ReadOptions, read};
    use crate::retrieval::testing::capture;
    use crate::support::testing::block_on;

    /// Answers each record question by whether its record holds `matches`.
    struct Jev {
        matches: &'static str,
        unsure: Option<&'static str>,
    }

    impl DecisionClient for Jev {
        fn profile(&self) -> DecisionProfile {
            DecisionProfile::jev()
        }

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
                    let probability = if self.unsure.is_some_and(|unsure| record.contains(unsure)) {
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

    /// Answers every yes/no question with its one probability and sends every choice to the LLM reader.
    struct Scope(f64);

    impl DecisionClient for Scope {
        fn profile(&self) -> DecisionProfile {
            DecisionProfile::jev()
        }

        fn evaluate<'a>(
            &'a self,
            _: &'a Value,
            questions: &'a IndexMap<String, Question>,
        ) -> BoxFuture<'a, std::result::Result<Evaluation, DecisionError>> {
            let answers = questions
                .iter()
                .map(|(key, question)| {
                    let answer = match question {
                        Question::Noul(_) => Answer::Noul(NoulAnswer { kind: NoulTag, probability: self.0 }),
                        _ => Answer::Choice(ChoiceAnswer {
                            kind: ChoiceTag,
                            choice: "synthesis".into(),
                            probabilities: IndexMap::from([("synthesis".to_string(), 1.0)]),
                            confidence: 1.0,
                        }),
                    };
                    (key.clone(), answer)
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

    #[test]
    fn jev_not_the_reader_says_whether_a_count_needs_every_page_of_its_list() {
        let captured = packages();
        let text = "Count every available package";
        let reply = json!({"claims": [], "answered": false, "continues": [{"requirement_id": "r", "tallies": [
            {"key": null, "records": [{"first": "s0", "last": "s0"}, {"first": "s1", "last": "s1"}]},
        ]}]});
        let ids = vec!["r".to_string()];
        for (probability, every_page) in [(0.9, true), (0.1, false)] {
            let llm = Llm { reply: Some(reply.clone()), shown: Mutex::new(Vec::new()) };
            let requirement =
                Requirement { count_records: true, ..Requirement::new("r", text, RequirementKind::Information) };
            let jev = Scope(probability);
            let options = ReadOptions { jev: Some(&jev), requirements: vec![requirement], ..ReadOptions::default() };
            let outcome = block_on(read(&llm, &captured, text, &ids, &mut Notes::default(), &options)).unwrap();
            assert_eq!(outcome.continues, ids);
            let (through_end, readers) =
                if every_page { (ids.clone(), vec![reader(&captured, text, None)]) } else { (Vec::new(), Vec::new()) };
            assert_eq!((outcome.through_end, outcome.tally_readers), (through_end, readers), "{probability}");
        }
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
    fn records_jev_is_unsure_of_are_read_in_prompts_that_fit_the_llm_budget() {
        let captured = packages();
        let page = Page::new(&captured);
        let blocks: Vec<&Block> = captured.blocks.iter().take(2).collect();
        let requirement = "Count every available package";
        let llm = Llm { reply: Some(json!({"matching": ["s0", "s1"]})), shown: Mutex::new(Vec::new()) };
        let prompt = format!("{RECORD_READER}\n\n# Trust\n{UNTRUSTED}# Requirement\n{requirement}\n\n# Records\n");
        let fixed = text::len(&format!("{prompt}{}", dumps(&json_schema(&RecordMatches::ty())))) as i64;
        // Room for one record a prompt, not two.
        let tokens =
            TokenBudget { llm_context_tokens: Some(fixed + 25), chars_per_token: 1.0, ..TokenBudget::default() };
        let (kept, costs) = block_on(read_unsure(&llm, &page, requirement, &blocks, &tokens, None)).unwrap();
        let heading = format!("# Requirement\n{requirement}\n\n# Records\n");
        assert_eq!(
            *llm.shown.lock().unwrap(),
            [format!("{heading}[s0] Oak - available"), format!("{heading}[s1] Elm - available")]
        );
        assert_eq!((kept, costs.len()), (blocks, 2));
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
