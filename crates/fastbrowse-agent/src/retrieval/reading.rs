//! The LLM reader: claims cite source blocks and code copies their quotes; tallies, continuations and collections are
//! recorded as code-checked facts.

use std::collections::HashSet;

use fastbrowse_core::BoxFuture;
use fastbrowse_core::config::TokenBudget;
use fastbrowse_core::decision::{Answer, DecisionClient, NoulQuestion, Question};
use fastbrowse_core::llm::{LLMClient, Message};
use fastbrowse_core::models::{CostLine, Evidence, FactReader, LLMPurpose, UNTRUSTED};
use fastbrowse_core::page::{Block, BlockKind, Capture};
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::py::text;
use fastbrowse_core::schema::{Field, ModelTy, Schema, Ty, json_schema};
use fastbrowse_core::telemetry::{Ledger, Trace};
use fastbrowse_core::validation::ModelSchema;
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::choices::{READ_CONFIDENCE, read_choices};
use super::chunks::{Chunk, Cite, chunk_page, cited, evidence, offered, read_message, source_marker};
use super::records::through_end as scope;
use super::text::{Page, alnum_runs, counted_heading, lines_of, loose_matches};
use super::{notes_room, route};
use crate::batches::evaluate_batches;
use crate::comparison::{NumericComparison, QuotedField, digit};
use crate::error::{AgentError, Result};
use crate::memory::{Comparison, Fact, Notes, RenderOptions, Tally, fact_id, unique};
use crate::planner::{FIELD_CATEGORIES, Requirement, RequirementKind};

pub(super) fn counted_kind(kind: BlockKind) -> bool {
    matches!(kind, BlockKind::Record | BlockKind::ListItem)
}

// --- response models ------------------------------------------------------------------------------------------------

const COMPARISON: &str = "For a lowest/highest/top-N ranking by one numeric field whose requested output is only record labels and values: the order, number of results, and literal delimiters around each record's label and displayed value (including its currency). Empty delimiter means the quote boundary. Use only when these fields occur exactly once in every matching record, and all remaining pages are needed. Apply the task's filters in records as usual. Null for counts, calculations, extra output fields, tie rules or values requiring unit conversion. Related requirements asking for the same winners' labels or values use the same comparison and records.";
const TALLIES: &str = "For every count of matching records, including filtered counts, or ranking by count. Group matching records by their stated label, or use key=null for an ungrouped count. Use explicit record ranges when matches are not contiguous. Leave records empty.";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadClaim {
    pub cite: Option<Cite>,
    #[serde(default)]
    pub excerpt: Option<String>,
    #[serde(default)]
    pub draws_on: Vec<String>,
    #[serde(default)]
    pub records: Vec<Cite>,
    #[serde(default)]
    pub orders_list: Option<Cite>,
    pub text: String,
    #[serde(default)]
    pub requirement_id: Option<String>,
}

impl Schema for ReadClaim {
    fn ty() -> Ty {
        ModelTy::frozen(
            "_ReadClaim",
            vec![
                Field::required("cite", Ty::optional(Cite::ty())).doc("The run of source blocks the claim reads, first to last. Null for a count, total or winner the page does not state, which rests on draws_on alone."),
                Field::default("excerpt", Ty::optional(Ty::Str.constrained("minLength", 1)), Value::Null).doc("An optional literal contiguous passage within cite, including record identity, field labels and qualifications. Use for a long block holding unrelated content; code must find it uniquely and copies the original source spelling. Null when the complete cited blocks are needed."),
                Field::default("draws_on", Ty::list(Ty::Str), json!([])).doc("Every record counted or compared: evidence ids from collected notes (without brackets), or claim:N for an earlier claim in this response's claims array, indexed from zero."),
                Field::default("records", Ty::list(Cite::ty()), json!([])).doc("Every record from this chunk compared by this claim, as block ranges only. Code copies their quotes into the claim's basis; do not also write a prose claim for each record."),
                Field::default("orders_list", Ty::optional(Cite::ty()), Value::Null).doc("For a superlative the page itself settles: the source blocks where the page states how the list is ordered or filtered, by the very quantity being compared (a sort control reading 'price, low to high', a heading naming the filter in force). Only when the page says it; never inferred from the order the records happen to appear in. Offered sorting links are not an active sorting state. With this, the leading record answers even though the list goes on, so it settles only a claim that cites that record and draws on nothing."),
                Field::required("text", Ty::Str).doc("The fact or conclusion in one sentence, stating the requested value itself: the price, count, name or date, not only that the cited blocks hold it."),
                Field::default("requirement_id", Ty::optional(Ty::Str), Value::Null).doc("The requirement id this claim answers in full with its constraints; null for a partial or context claim."),
            ],
        )
        .ty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TallyField {
    pub span: Cite,
    pub prefix: String,
    pub suffix: String,
}

impl Schema for TallyField {
    fn ty() -> Ty {
        ModelTy::frozen(
            "_TallyField",
            vec![
                Field::required("span", Cite::ty()),
                Field::required("prefix", Ty::Str)
                    .doc("Exact literal record text before the grouped value, excluding source metadata."),
                Field::required("suffix", Ty::Str)
                    .doc("Exact literal record text after the grouped value, excluding source metadata."),
            ],
        )
        .ty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TallyGroup {
    pub key: Option<String>,
    #[serde(default)]
    pub records: Vec<Cite>,
    #[serde(default)]
    pub field: Option<TallyField>,
}

impl Schema for TallyGroup {
    fn ty() -> Ty {
        ModelTy::frozen(
            "_TallyGroup",
            vec![
                Field::required("key", Ty::optional(Ty::Str))
                    .doc("The label stated in each record, such as its author; null for an ungrouped count."),
                Field::default("records", Ty::list(Cite::ty()), json!([])).doc("The source block ranges of matching records only. Use explicit ranges when a filter excludes records between matches; a field range cannot filter them. Empty when none match this page."),
                Field::default("field", Ty::optional(TallyField::ty()), Value::Null).doc("Instead of enumerating records: a range containing only complete record blocks, each counted once, grouped by the text between the exact prefix and suffix in each record. Use only when every record in the range matches the task. Set key=null and records=[] when using field."),
            ],
        )
        .ty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TallyRead {
    pub requirement_id: String,
    pub groups: Vec<TallyGroup>,
    /// True only when earlier evidence and this capture cover the entire requested list.
    #[serde(default)]
    pub complete: bool,
}

impl Schema for TallyRead {
    fn ty() -> Ty {
        ModelTy::frozen(
            "_TallyRead",
            vec![
                Field::required("requirement_id", Ty::Str),
                Field::required("groups", Ty::list(TallyGroup::ty())),
                Field::default("complete", Ty::Bool, false)
                    .doc("True only when earlier evidence and this capture cover the entire requested list."),
            ],
        )
        .ty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Continuation {
    #[serde(default)]
    pub comparison: Option<NumericComparison>,
    #[serde(default)]
    pub tallies: Vec<TallyGroup>,
    #[serde(default)]
    pub reuse_field: bool,
    pub requirement_id: String,
    #[serde(default)]
    pub expands: Option<String>,
    #[serde(default)]
    pub records: Vec<Cite>,
}

impl Schema for Continuation {
    fn ty() -> Ty {
        ModelTy::frozen(
            "_Continuation",
            vec![
                Field::default("comparison", Ty::optional(NumericComparison::ty()), Value::Null).doc(COMPARISON),
                Field::default("tallies", Ty::list(TallyGroup::ty()), json!([])).doc(TALLIES),
                Field::default("reuse_field", Ty::Bool, false).doc("True only for an unfiltered count of EVERY record in this entire paginated list, grouped by the same field. Never for a count conditioned on any record attribute. Code may reuse the field on later pages only while every record has the same structure."),
                Field::required("requirement_id", Ty::Str).doc("A requirement whose answer ranges over a list this capture shows only part of, because it continues on further pages or behind a load-more control, and the collected evidence does not cover the rest."),
                Field::default("expands", Ty::optional(Ty::Str), Value::Null).doc("The exact label of the control on this page that shows the rest of the list, when one is offered. Null when nothing on the page expands it."),
                Field::default("records", Ty::list(Cite::ty()), json!([])).doc("For comparisons of record values, not counts: every record this capture adds to that comparison, each as the run of source blocks holding it and the value being compared. A later page cannot show what its winner beat unless this page names the records it was compared against, so list every one this capture shows; a capture holding only the pager lists none."),
            ],
        )
        .ty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadCollection {
    pub requirement_id: String,
    pub scope: String,
    pub records: Vec<Cite>,
}

impl Schema for ReadCollection {
    fn ty() -> Ty {
        ModelTy::frozen(
            "_ReadCollection",
            vec![
                Field::required("requirement_id", Ty::Str),
                Field::required("scope", Ty::Str)
                    .doc("The bounded collection identified by its quoted heading and boundary."),
                Field::required("records", Ty::list(Cite::ty()))
                    .doc("The identifying boundary and EVERY member, including roles and replies."),
            ],
        )
        .ty()
    }
}

/// `_ReadResponse`, and `_CollectionResponse` when `collections` is asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadResponse {
    #[serde(default)]
    pub tallies: Vec<TallyRead>,
    pub claims: Vec<ReadClaim>,
    pub answered: bool,
    #[serde(default)]
    pub continues: Vec<Continuation>,
    #[serde(default)]
    pub collections: Vec<ReadCollection>,
}

pub(crate) fn read_response_ty(collections: bool) -> Ty {
    let mut fields = vec![
        Field::default("tallies", Ty::list(TallyRead::ty()), json!([])),
        Field::required("claims", Ty::list(ReadClaim::ty()))
            .doc("Every fact this capture shows toward the question, each with the value stated in text."),
        Field::required("answered", Ty::Bool)
            .doc("True only when the collected evidence and these claims answer every requirement."),
        Field::default("continues", Ty::list(Continuation::ty()), json!([])),
    ];
    if collections {
        fields.push(Field::default("collections", Ty::list(ReadCollection::ty()), json!([])));
    }
    ModelTy::frozen(if collections { "_CollectionResponse" } else { "_ReadResponse" }, fields).ty()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordSet {
    pub requirement_id: String,
    #[serde(default)]
    pub comparison: Option<NumericComparison>,
    #[serde(default)]
    pub tallies: Vec<TallyGroup>,
    #[serde(default)]
    pub records: Vec<Cite>,
}

impl Schema for RecordSet {
    fn ty() -> Ty {
        ModelTy::frozen(
            "_RecordSet",
            vec![
                Field::required("requirement_id", Ty::Str),
                Field::default("comparison", Ty::optional(NumericComparison::ty()), Value::Null).doc(COMPARISON),
                Field::default("tallies", Ty::list(TallyGroup::ty()), json!([])).doc(TALLIES),
                Field::default("records", Ty::list(Cite::ty()), json!([])).doc("For comparisons of record values, not counts: every matching record as source block ranges including the compared value and attributes that show it matches. Counts belong in tallies."),
            ],
        )
        .ty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordsResponse {
    pub continues: Vec<RecordSet>,
    #[serde(default)]
    pub context: Vec<Cite>,
    #[serde(default)]
    pub ended: Vec<String>,
}

impl Schema for RecordsResponse {
    fn ty() -> Ty {
        ModelTy::frozen(
            "_RecordsResponse",
            vec![
                Field::required("continues", Ty::list(RecordSet::ty())),
                Field::default("context", Ty::list(Cite::ty()), json!([])),
                Field::default("ended", Ty::list(Ty::Str), json!([])).doc("Requirement ids whose requested list this capture shows reaching its end. Empty for a partial list, a loading page, an unrelated page, or a list continuing behind any control."),
            ],
        )
        .ty()
    }
}

// --- outcome ----------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TallyReader {
    pub requirement_id: String,
    #[serde(default)]
    pub count_label: Option<String>,
    pub title: String,
    pub heading_path: Vec<String>,
    pub frame_id: Option<String>,
    pub prefix: String,
    pub suffix: String,
    /// For a filtered count or comparison, the requirement each later record is matched against by Jev.
    #[serde(default)]
    pub filter: Option<String>,
    /// The filter feeds a comparison's records rather than a tally: code picks the winner from their values.
    #[serde(default)]
    pub compares: bool,
    /// The block kind of the records the first page's reader cited, which a later page's records must share.
    #[serde(default)]
    pub record_kind: Option<BlockKind>,
    /// For a comparison, the delimiters every kept record must yield a label and a value from.
    #[serde(default)]
    pub comparison: Option<NumericComparison>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadOutcome {
    #[serde(serialize_with = "as_plain_facts")]
    pub facts: Vec<Fact>,
    #[serde(skip)]
    pub collection_facts: Vec<Fact>,
    pub rejected_claims: i64,
    pub cost_lines: Vec<CostLine>,
    /// Requirements whose list goes on past this capture, so no claim from it closes them.
    pub continues: Vec<String>,
    /// Requirements whose continuation lost a record, so later comparisons cannot settle them.
    pub incomplete: Vec<String>,
    /// Records a continuation named that no run of offered blocks resolved, so this page's comparison is incomplete in
    /// the notes even though the reader believed it had listed them.
    pub uncovered: i64,
    /// The label the reader gave for the control that shows the rest of the list, so the run can open it instead of
    /// being told only that the list goes on.
    pub expands: Option<String>,
    pub through_end: Vec<String>,
    pub continuation_records: IndexMap<String, Vec<String>>,
    pub ended: Vec<String>,
    pub tally_readers: Vec<TallyReader>,
    pub comparisons: IndexMap<String, NumericComparison>,
}

/// pydantic writes a `tuple[Fact, ...]` field by its declared type, so a collection fact there loses its scope.
fn as_plain_facts<S: serde::Serializer>(facts: &[Fact], serializer: S) -> std::result::Result<S::Ok, S::Error> {
    let plain: Vec<Fact> = facts.iter().map(|fact| Fact { scope: None, ..fact.clone() }).collect();
    plain.serialize(serializer)
}

impl ReadOutcome {
    pub(super) fn new(facts: Vec<Fact>, rejected_claims: i64, cost_lines: Vec<CostLine>) -> ReadOutcome {
        ReadOutcome {
            facts,
            collection_facts: Vec::new(),
            rejected_claims,
            cost_lines,
            continues: Vec::new(),
            incomplete: Vec::new(),
            uncovered: 0,
            expands: None,
            through_end: Vec::new(),
            continuation_records: IndexMap::new(),
            ended: Vec::new(),
            tally_readers: Vec::new(),
            comparisons: IndexMap::new(),
        }
    }

    /// Merge an isolated records read in page order, using the same span and tally deduplication.
    pub fn merge_records(&self, notes: &mut Notes) -> Result<()> {
        for fact in &self.facts {
            if fact.tally.is_none() {
                notes.add(Fact { requirement_id: None, ..fact.clone() });
            }
        }
        for fact in &self.facts {
            if let Some(tally) = &fact.tally {
                notes.add_tally(tally.clone())?;
            }
        }
        for (requirement_id, records) in &self.continuation_records {
            for record in records {
                notes.add_continuation(requirement_id, record);
            }
        }
        Ok(())
    }
}

/// `_quoted`: a fact that is only the page's own text, kept as context a claim can rest on.
pub(crate) fn quoted(evidence: Evidence) -> Fact {
    Fact::read(None, &evidence.quote.clone(), evidence, FactReader::Llm)
}

fn block_containing<'a>(page: &Page<'a>, at: i64) -> Option<&'a Block> {
    page.capture.blocks.iter().find(|block| block.start <= at && at < block.end)
}

fn first_block<'a>(page: &Page<'a>, source_id: &str) -> Option<&'a Block> {
    page.capture.blocks.iter().find(|block| block.source_id == source_id)
}

// --- claims -------------------------------------------------------------------------------------------------------

fn record_cites(page: &Page<'_>, part: &Chunk, cites: &[Cite], kinds: fn(BlockKind) -> bool) -> Vec<Cite> {
    let mut result = Vec::new();
    for cite in cites {
        let blocks: Vec<&Block> = match cited(page, part, cite) {
            None => Vec::new(),
            Some(evidence) => {
                offered(page, part).into_iter().filter(|b| b.start < evidence.end && b.end > evidence.start).collect()
            }
        };
        if !blocks.is_empty() && blocks.iter().all(|b| kinds(b.kind) && b.start >= part.start && b.end <= part.end) {
            result.extend(blocks.iter().map(|b| Cite::one(&b.source_id)));
        } else {
            result.push(cite.clone());
        }
    }
    result
}

fn record_kind(kind: BlockKind) -> bool {
    kind == BlockKind::Record
}

/// `_source_basis`: plain summaries add no source evidence; collection and tally nodes still carry required coverage
/// checks.
pub(crate) fn source_basis(notes: &Notes, key: &str) -> Vec<String> {
    let held = notes.facts_by_id();
    let substantive = |fact: &Fact| fact.evidence.is_some() || fact.tally.is_some() || fact.comparison.is_some();
    match held.get(key) {
        Some(fact) if !substantive(fact) => notes
            .expand_evidence_ids([key])
            .into_iter()
            .filter(|source| held.get(source).is_none_or(substantive))
            .collect(),
        _ => vec![key.to_string()],
    }
}

fn push_unique(basis: &mut Vec<String>, key: String) {
    if !basis.contains(&key) {
        basis.push(key);
    }
}

/// `_remember`: the claim as a fact whose quote code copied from the blocks it cites, or None when it cites them
/// wrongly.
fn remember(
    page: &Page<'_>,
    part: &Chunk,
    claim: &ReadClaim,
    notes: &mut Notes,
    references: &IndexMap<String, String>,
    preserve_collections: bool,
    trace: &Trace,
) -> Result<Option<Fact>> {
    let mut basis: Vec<String> = Vec::new();
    for reference in &claim.draws_on {
        let Some(key) = references.get(reference) else {
            trace.event("unknown_reference", || {
                let mut fields = serde_json::Map::new();
                fields.insert("reference".into(), reference.as_str().into());
                fields.insert("claim".into(), claim.text.as_str().into());
                fields
            });
            continue;
        };
        let sources = if preserve_collections { source_basis(notes, key) } else { vec![key.clone()] };
        for source in sources {
            push_unique(&mut basis, source);
        }
    }
    let mut quote = claim.cite.as_ref().and_then(|cite| cited(page, part, cite));
    // A blank excerpt asks for the complete blocks, as a null one does.
    if let Some(raw) = claim.excerpt.as_ref().filter(|raw| !text::strip(raw).is_empty()) {
        let Some(span) = quote.clone() else { return Ok(None) };
        let mut excerpt = text::strip(raw).to_string();
        let mut matches = loose_matches(&excerpt, &span.quote);
        if matches.is_empty() {
            // Readers copy our block labels between paragraphs; those annotations are not captured page text.
            let markers: Vec<String> = offered(page, part)
                .into_iter()
                .filter(|block| block.start < span.end && block.end > span.start)
                .map(source_marker)
                .collect();
            excerpt = lines_of(&excerpt)
                .into_iter()
                .map(|line| match markers.iter().find(|marker| line.starts_with(marker.as_str())) {
                    Some(marker) => line[marker.len()..].to_string(),
                    None => line,
                })
                .collect::<Vec<_>>()
                .join("\n");
            matches = loose_matches(&excerpt, &span.quote);
        }
        if matches.is_empty() {
            return Ok(None);
        }
        // An excerpt found more than once in its cited run still holds there: the claim keeps the whole run, which
        // states it, rather than being refused or narrowed to one of the places it could be.
        if let Some((offset, stop)) = excerpt_match(page, &span, &matches) {
            let (mut start, end) = (span.start + offset as i64, span.start + stop as i64);
            let Some(block) = block_containing(page, start) else { return Ok(None) };
            // A table excerpt can select an attribute row without the preceding cells identifying its columns.
            if block.kind == BlockKind::Table {
                start = span.start;
            }
            let cite = claim.cite.as_ref().expect("an excerpt needs a cite");
            if cite.first != cite.last && (start, end) != (span.start, span.end) {
                // Narrowing a total to its value can discard the subjects in the reader's selected blocks.
                let context = quoted(span.clone());
                basis.push(fact_id(&context));
                notes.add(context);
            }
            let narrowed = evidence(page, block, start, end);
            // Narrowing one record to its value can discard the subject named beside it in the same block.
            if cite.first == cite.last
                && let Some(context) = block_context(page, &narrowed, FactReader::Llm)
            {
                let key = fact_id(&context);
                notes.add(context);
                push_unique(&mut basis, key);
            }
            quote = Some(narrowed);
        }
    }
    // A derived claim cites nothing and rests on its basis; one that cites blocks must cite them correctly.
    if quote.is_none() && (claim.cite.is_some() || basis.is_empty()) {
        return Ok(None);
    }
    if let Some(span) = &quote {
        for context in heading_facts(page, span, FactReader::Llm) {
            let key = fact_id(&context);
            notes.add(context);
            push_unique(&mut basis, key);
        }
        if !part.header.is_empty()
            && let Some(block) = block_containing(page, span.start)
            && block.kind == BlockKind::Table
            && block.start < part.start
        {
            let prefix_end = block.start + text::len(&part.header) as i64;
            if page.slice(block.start, prefix_end) == part.header && prefix_end <= span.start {
                // Continuations show real leading cells separately; retain that quote as identity evidence too.
                let context =
                    Fact::read(None, &part.header, evidence(page, block, block.start, prefix_end), FactReader::Llm);
                basis.push(fact_id(&context));
                notes.add(context);
            }
        }
    }
    let fact = Fact {
        requirement_id: claim.requirement_id.clone(),
        tally: None,
        comparison: None,
        text: claim.text.clone(),
        evidence: quote,
        basis,
        reader: FactReader::Llm,
        scope: None,
    };
    notes.add(fact.clone());
    Ok(Some(fact))
}

/// `_block_context`: the whole block a narrowed quote was cut from, kept beside it as context.
///
/// The claim check judges a requested subject only by the literal quotes it is offered. Cut to "by Marilyn Monroe", or
/// to a quote's text without the "C.S. Lewis (god)" line under it, a correct answer had no quote naming what it
/// answered for, and the run ended unverified with the record that named it in its notes.
pub(crate) fn block_context(page: &Page<'_>, quote: &Evidence, reader: FactReader) -> Option<Fact> {
    let capture = page.capture;
    let block = capture.blocks.iter().find(|block| {
        block.frame_id == quote.frame_id
            && block.source_url.as_deref().unwrap_or(&capture.url) == quote.url
            && block.start <= quote.start
            && quote.end <= block.end
    })?;
    // A table row already keeps the cells before its value, and the rows after it are other records. A downloaded
    // file's block is the whole file, which an excerpt must not stand in for. One line of prose names its subject in
    // the sentence the reader quoted; it is a record's other lines that carry the name.
    if matches!(block.kind, BlockKind::Observation | BlockKind::Table)
        || block.complete_source
        || block.source_url.is_some()
        || !page.slice(block.start, block.end).contains('\n')
        || (text::strip(page.slice(block.start, quote.start)).is_empty()
            && text::strip(page.slice(quote.end, block.end)).is_empty())
    {
        return None;
    }
    let span = evidence(page, block, block.start, block.end);
    Some(Fact::read(None, &span.quote.clone(), span, reader))
}

/// `_excerpt_match`: the one place in the cited run an excerpt names, or None when it could be either of two.
///
/// Equal text in two places can be two records' values, the same price under two products, so a repeated excerpt is
/// not narrowed to either. One case is not ambiguous: the excerpt is the whole of exactly one block, and its other matches sit inside
/// longer blocks that say more around it. A wizard's result panel read "Multi-step form submitted successfully." and
/// its toast "Success toast: Multi-step form submitted successfully.", and the confirmation is the panel's.
fn excerpt_match(page: &Page<'_>, span: &Evidence, matches: &[(usize, usize)]) -> Option<(usize, usize)> {
    if matches.len() == 1 {
        return Some(matches[0]);
    }
    let whole = |(offset, stop): &(usize, usize)| {
        let (start, end) = (span.start + *offset as i64, span.start + *stop as i64);
        page.capture.blocks.iter().any(|block| {
            block.start <= start
                && end <= block.end
                && block.frame_id == span.frame_id
                && text::strip(page.slice(block.start, start)).is_empty()
                && text::strip(page.slice(end, block.end)).is_empty()
        })
    };
    let blocks: Vec<&(usize, usize)> = matches.iter().filter(|m| whole(m)).collect();
    match blocks.as_slice() {
        [only] => Some(**only),
        _ => None,
    }
}

/// `_heading_facts`: a scalar quote can omit its subject, so both readers retain the headings that actually scoped it.
pub(crate) fn heading_facts(page: &Page<'_>, quote: &Evidence, reader: FactReader) -> Vec<Fact> {
    let mut facts = Vec::new();
    for heading in &quote.heading_path {
        let length = text::len(heading) as i64;
        let block = page.capture.blocks.iter().rev().find(|block| {
            block.kind == BlockKind::Heading
                && block.frame_id == quote.frame_id
                && block.start + length <= quote.start
                && block.start + length <= block.end
                && page.slice(block.start, block.start + length) == heading
        });
        if let Some(block) = block {
            facts.push(Fact::read(None, heading, evidence(page, block, block.start, block.start + length), reader));
        }
    }
    facts
}

fn field_groups(page: &Page<'_>, part: &Chunk, field: &TallyField) -> Option<Vec<TallyGroup>> {
    let span = cited(page, part, &field.span)?;
    let blocks: Vec<&Block> =
        offered(page, part).into_iter().filter(|b| b.start < span.end && b.end > span.start).collect();
    if blocks.is_empty() {
        return None;
    }
    let mut groups: IndexMap<String, Vec<Cite>> = IndexMap::new();
    let delimiters = QuotedField::new(&field.prefix, &field.suffix);
    for block in blocks {
        // A paragraph may hold more than one record, and a split record may hide a second matching field.
        if !counted_kind(block.kind) || block.start < part.start || block.end > part.end {
            return None;
        }
        let key = delimiters.extract(page.slice(block.start, block.end))?;
        groups.entry(key).or_default().push(Cite::one(&block.source_id));
    }
    Some(groups.into_iter().map(|(key, records)| TallyGroup { key: Some(key), records, field: None }).collect())
}

/// `subject_words`.
pub fn subject_words(value: &str) -> Vec<String> {
    alnum_runs(&text::casefold(value))
}

/// Whether `text` holds `subject` as whole words, so "Birch" does not name "Birchwood".
pub fn names_subject(value: &str, subject: &str) -> bool {
    let (tokens, target) = (subject_words(value), subject_words(subject));
    !target.is_empty() && tokens.windows(target.len()).any(|window| window == target.as_slice())
}

/// `int()` of a run of Unicode decimal digits.
fn digits_value(count: &str) -> Option<i64> {
    count.chars().try_fold(0i64, |total, c| total.checked_mul(10)?.checked_add(i64::from(digit(c)?)))
}

/// A counted list shown in full: its heading states how many members it has and the page shows that many.
struct CountedList {
    title: String,
    heading_path: Vec<String>,
    sources: Vec<Fact>,
}

/// The counted lists `page` shows in full up to `read_end`.
fn counted_lists(page: &Page<'_>, read_end: i64) -> Vec<CountedList> {
    let capture = page.capture;
    let blocks = &capture.blocks;
    let mut lists = Vec::new();
    for (index, heading) in blocks.iter().enumerate() {
        if heading.kind != BlockKind::Heading || heading.frame_id.is_some() {
            continue;
        }
        let title = page.slice(heading.start, heading.end);
        let Some(count) = counted_heading(title) else { continue };
        let mut members: Vec<&Block> = Vec::new();
        for block in &blocks[index + 1..] {
            if block.frame_id != heading.frame_id || block.kind == BlockKind::Heading {
                break;
            }
            match members.first() {
                None => {
                    if !counted_kind(block.kind) && block.list_id.is_none() {
                        break;
                    }
                }
                Some(first) if first.list_id.is_some() => {
                    if block.list_id != first.list_id {
                        break;
                    }
                }
                Some(_) => {
                    if !counted_kind(block.kind) {
                        break;
                    }
                }
            }
            members.push(block);
        }
        if let Some(list_id) = members.first().and_then(|first| first.list_id.clone()) {
            // A member can contain headings or form fields between its text blocks. The list identity, not adjacency,
            // keeps its entire captured text in the collection.
            members = blocks.iter().filter(|b| b.list_id.as_deref() == Some(list_id.as_str())).collect();
            if members[0].start < heading.end {
                continue;
            }
        }
        if members.is_empty() || members[members.len() - 1].end > read_end {
            continue;
        }
        if capture.inaccessible_frames > 0 && (members[0].list_id.is_none() || !members[0].list_frameless) {
            // An unopened frame could hold members of a list known only by adjacency; an advertisement frame elsewhere
            // on the page used to withhold coverage from a list read in full, so a correct finding of none was
            // rejected on whichever run its frame did not load.
            continue;
        }
        let total = match members[0].list_count {
            Some(total) => total,
            None => {
                if members.iter().any(|b| !counted_kind(b.kind)) {
                    continue;
                }
                members.len() as i64
            }
        };
        if Some(total) != digits_value(&count) {
            continue;
        }
        let context = blocks[..index]
            .iter()
            .rev()
            .find(|b| {
                b.kind == BlockKind::Heading
                    && b.frame_id == heading.frame_id
                    && heading.heading_path.iter().any(|h| h == page.slice(b.start, b.end))
            })
            .unwrap_or(heading);
        let boundary = quoted(evidence(page, context, context.start, heading.end));
        let first = members[0];
        let last = members[members.len() - 1];
        let sources: Vec<Fact> = if first.list_id.is_some() {
            vec![boundary, quoted(evidence(page, first, first.start, last.end))]
        } else {
            std::iter::once(boundary).chain(members.iter().map(|b| quoted(evidence(page, b, b.start, b.end)))).collect()
        };
        lists.push(CountedList { title: title.to_string(), heading_path: heading.heading_path.clone(), sources });
    }
    lists
}

/// Fully shown counted lists as complete collections of the requirements they cover, and what asking Jev cost.
///
/// `targets` is every information requirement of the plan. With one, a list read in full covers it. With more, Jev
/// says which of the open requirements each list is the whole of: an unrelated "Reviews (3)" on another target's page
/// would otherwise close the last target's collection. Without Jev no list covers any of several targets.
pub(crate) async fn counted_collections(
    page: &Page<'_>,
    requirement_ids: &[String],
    read_end: i64,
    targets: &[Requirement],
    jev: Option<&dyn DecisionClient>,
    tokens: &TokenBudget,
    ledger: Option<&Ledger>,
) -> Result<(Vec<Fact>, Vec<CostLine>)> {
    let capture = page.capture;
    let lists = counted_lists(page, read_end);
    let mut covered: HashSet<(usize, &String)> = HashSet::new();
    let mut cost = Vec::new();
    if targets.len() <= 1 {
        covered.extend((0..lists.len()).flat_map(|index| requirement_ids.iter().map(move |id| (index, id))));
    } else if let Some(jev) = jev {
        let mut questions: IndexMap<String, Question> = IndexMap::new();
        let mut asked: IndexMap<String, (usize, &String)> = IndexMap::new();
        for index in 0..lists.len() {
            for id in requirement_ids {
                let Some(requirement) = targets.iter().find(|r| &r.id == id) else { continue };
                let key = format!("l{index}.{id}");
                let instructions = format!(
                    "{UNTRUSTED} Requirement: {}\nIs counted list l{index} in lists, read in full on this page, the whole list of what this requirement asks about? A list of something else on the same page, such as another product's reviews, is not.",
                    requirement.text,
                );
                questions.insert(key.clone(), NoulQuestion::new(instructions, None, None).into());
                asked.insert(key, (index, id));
            }
        }
        let shown: Vec<Value> = lists
            .iter()
            .enumerate()
            .map(|(index, list)| json!({"id": format!("l{index}"), "heading": list.title, "under": list.heading_path}))
            .collect();
        let state = json!({"page": {"url": capture.url, "title": capture.title}, "lists": shown});
        if !questions.is_empty()
            && let Some(answered) = evaluate_batches(jev, &state, &questions, tokens, ledger, None, true).await?
        {
            cost = answered.cost;
            for (key, pair) in asked {
                if answered.answers.get(&key).and_then(Answer::probability).is_some_and(|p| p >= READ_CONFIDENCE) {
                    covered.insert(pair);
                }
            }
        }
    }
    let mut collections = Vec::new();
    for (index, list) in lists.into_iter().enumerate() {
        let keys: Vec<String> = list.sources.iter().map(fact_id).collect();
        collections.extend(list.sources);
        for requirement_id in requirement_ids.iter().filter(|id| covered.contains(&(index, *id))) {
            let scope = format!("{}: {}", capture.title, list.title);
            collections.push(Fact {
                requirement_id: None,
                tally: None,
                comparison: Some(Comparison {
                    requirement_id: requirement_id.clone(),
                    records: keys.clone(),
                    complete: true,
                }),
                text: scope.clone(),
                evidence: None,
                basis: keys.clone(),
                reader: FactReader::Llm,
                scope: Some(scope),
            });
        }
    }
    Ok((collections, cost))
}

/// [`counted_collections`] for callers holding only the capture.
#[allow(clippy::too_many_arguments)]
pub async fn counted_collections_for(
    capture: &Capture,
    requirement_ids: &[String],
    read_end: i64,
    targets: &[Requirement],
    jev: Option<&dyn DecisionClient>,
    tokens: &TokenBudget,
    ledger: Option<&Ledger>,
) -> Result<(Vec<Fact>, Vec<CostLine>)> {
    counted_collections(&Page::new(capture), requirement_ids, read_end, targets, jev, tokens, ledger).await
}

/// `read_tallies`: a later page of a list counted by the field the reader set up, without asking the reader again.
pub fn read_tallies(
    capture: &Capture,
    readers: &[TallyReader],
    requirement_ids: &[String],
) -> Result<Option<ReadOutcome>> {
    let page = Page::new(capture);
    if capture.inaccessible_frames > 0 {
        return Ok(None);
    }
    let records: Vec<&Block> = capture.blocks.iter().filter(|b| counted_kind(b.kind)).collect();
    let wanted: HashSet<&String> = requirement_ids.iter().collect();
    let have: HashSet<&String> = readers.iter().map(|r| &r.requirement_id).collect();
    if records.is_empty() || wanted != have {
        return Ok(None);
    }
    if readers.iter().any(|reader| reader.filter.is_some()) {
        return Ok(None);
    }
    let whole = whole_page(&page);
    let mut notes = Notes::default();
    for reader in readers {
        if capture.title != reader.title
            || records.iter().any(|b| (&b.heading_path, &b.frame_id) != (&reader.heading_path, &reader.frame_id))
        {
            return Ok(None);
        }
        let field = TallyField {
            span: Cite { first: records[0].source_id.clone(), last: records[records.len() - 1].source_id.clone() },
            prefix: reader.prefix.clone(),
            suffix: reader.suffix.clone(),
        };
        let Some(groups) = field_groups(&page, &whole, &field) else { return Ok(None) };
        for group in groups {
            let mut keys = Vec::new();
            for cite in &group.records {
                let span = cited(&page, &whole, cite).expect("a field group's records resolve");
                let fact = quoted(span);
                keys.push(fact_id(&fact));
                notes.add(fact);
            }
            let label = [reader.count_label.as_deref(), group.key.as_deref()]
                .into_iter()
                .flatten()
                .find(|label| !label.is_empty())
                .unwrap_or("records");
            notes.add_tally(Tally {
                requirement_id: reader.requirement_id.clone(),
                key: label.into(),
                records: keys,
            })?;
        }
    }
    Ok(Some(ReadOutcome::new(notes.facts().into_iter().cloned().collect(), 0, Vec::new())))
}

// --- read ---------------------------------------------------------------------------------------------------------

/// A yes from here reads the page for the actions' context. The labelled pages `context_question` was measured on split
/// at 0.92 and 0.21, so even odds is the bar.
const CONTEXT_FROM: f64 = 0.5;

/// Whether Jev says the page shows a value the task's actions need, and what asking cost. A page too large to ask about,
/// or a question that went unanswered, counts as a yes: a value the reader never quoted cannot be typed later.
async fn carries_context(
    jev: &dyn DecisionClient,
    capture: &Capture,
    asked: &Question,
    tokens: &TokenBudget,
    ledger: Option<&Ledger>,
) -> Result<(bool, Vec<CostLine>)> {
    let state = json!({"page": {"url": capture.url, "title": capture.title, "text": capture.text}});
    let questions = IndexMap::from([("context".to_string(), asked.clone())]);
    let Some(answered) = evaluate_batches(jev, &state, &questions, tokens, ledger, None, true).await? else {
        return Ok((true, Vec::new()));
    };
    let carried = match answered.answers.get("context") {
        Some(Answer::Noul(answer)) => answer.probability >= CONTEXT_FROM,
        _ => true,
    };
    Ok((carried, answered.cost))
}

/// `read`'s keyword arguments.
#[derive(Default, Clone)]
pub struct ReadOptions<'a> {
    pub max_chars: Option<i64>,
    pub tokens: TokenBudget,
    pub ledger: Option<&'a Ledger>,
    pub jev: Option<&'a dyn DecisionClient>,
    /// Asked of Jev about the page once it has settled every requirement: a yes still sends the page to the LLM reader,
    /// for the context quotes the task's actions need.
    pub context_question: Option<Question>,
    pub requirements: Vec<Requirement>,
    /// The requirements an earlier page already said run past it.
    pub continuing: Vec<String>,
    pub incomplete: Vec<String>,
    pub records_only: bool,
    pub require_all_evidence: bool,
    pub revalidate: bool,
    pub recovering_outputs: bool,
    pub preserve_collections: bool,
    pub field_outputs: bool,
    /// The plan's information requirements, read or not, which decide whether a counted list must name the target it
    /// covers.
    pub targets: Option<Vec<Requirement>>,
    pub trace: Trace,
}

const READER_BASE: &str = "# Reader\nAnswer the question from this capture's source blocks and the collected evidence. Each claim states only what its cited blocks, and the claims it draws on, show.\n\n# Sources\nThe [source id], (block kind) and embedded-frame labels are code's annotations, not page text. Repeated table headers provide context, not literal text inside every row. Citations use the source id; literal field delimiters use only the record text after those annotations. Do not copy annotation prefixes into field delimiters.\n\n# Claims\n- Write the requested value itself in each claim's text (the price, count, name or date): code checks the text against its cited blocks, and a claim that only says where the value is answers nothing.\n- Keep each claim to one fact or one derived conclusion. Do not summarize the whole task in one claim. A prior claim's prose is a hypothesis, not additional evidence: check its original quoted basis before carrying a value or inference forward. Preserve conditions such as eligibility, location and configuration in every claim that uses the value. A component maximum is not an aggregate maximum, and independent maxima are not additive unless their quoted sources explicitly allow simultaneous addition. A product name or nominal rating alone cannot settle an explicitly requested configuration. Leave an unsupported requested value open instead of inferring it from a related value.\n- For a long block containing unrelated content, set excerpt to the smallest literal contiguous passage supporting the claim, including record identity, field labels and qualifications. A pronoun or relative reference does not identify an entity on its own: keep the quoted antecedent alongside its value, extending the excerpt or citing a context claim with that literal identifying passage. Split claims supported by separate passages. Code matches the excerpt uniquely inside the cited blocks and copies its original spelling; never paraphrase it. Leave excerpt null when the complete blocks are needed.\n- A claim cites one run of blocks. For a comparison, put every compared record from this chunk in the conclusion's records as block ranges only. Code copies their quotes into its basis; never write a prose claim for each compared record. Use draws_on for earlier evidence and context claims.\n- The answer is checked later without the page, so a comparison also needs claims for the query, filters, date and sort that make it valid, with a null requirement id.\n- A count, total or winner rests on every record it counts or compares and on those context claims. It cites blocks only when the page itself states it.\n- Give a claim a requirement id only when it answers that whole requirement with its constraints; otherwise null. Set answered only when the collected evidence and this capture fully answer the question.\n- An explicit empty-result message is a finding: quote it and state that no matching records are reported within that page's scope. Missing text, a loading view or an unread section cannot establish absence.\n- Values typed into fields, suggestions and previews are inputs, not results.\n\n";

const READER_TALLIES: &str = "# Tallies\nFor a count of records or a ranking by record count, return tally groups: each key is the label stated in its records, and each record cites its own source blocks. Choose the entity level requested by the task. When counting items inside groups, group headings and subtotals are context: cite each matching item separately. When counting the groups themselves, cite each group instead of its children. Verify each counted entity's filters and required status. When the page explicitly states the requested total for the entire matching list, quote that statement as a claim instead of deriving it from partial tallies. A displayed subtotal, group count or number loaded so far does not state the requested whole-list total. Use continues.tallies while more pages remain and tallies with complete=true only on the last requested page. List only records from this chunk; never repeat earlier records, quote their text, calculate totals or write claims for counted records. Code deduplicates, counts and ranks them, preserving their quotes behind the tally references. Reuse a tally reference as a basis instead of listing every earlier record.\n\nA filtered count also uses tallies: select only matching records, grouped by their stated label (key=null for an ungrouped count). If matches are separated by excluded records, list their block ranges explicitly in the group's records. Never put counted records in continues.records. A page with no matches contributes an empty group.\n\nFor repeated record blocks with the grouping label between the same literal delimiters, prefer one tally group with key=null, records=[] and field: span covers the records, prefix and suffix are the exact text surrounding the label, each occurring once per record (include a newline in the prefix when it starts a line). Code extracts each record's label and counts it. Every block in the range must be a complete record matching the task; do not include headings, navigation or records excluded by a filter. When using tallies, leave the continuation's records empty.\n\nSet reuse_field=true for an unfiltered count of every record across this whole list, when all record blocks use this same grouping field. Code can then read later pages with the same structure. Any condition selecting records forbids reuse_field.\n\n";

const READER_RANKINGS: &str = "# Numeric rankings\nFor a minimum, maximum or top-N by one numeric field, set continues.comparison when all requested outputs are the winners' labels and values. Give order=lowest/highest, limit=N, and exact prefix/suffix delimiters for label and value inside each matching record's quote. The value includes its currency symbol. An empty prefix or suffix means the beginning or end of the quote. Each field must match once per record. Code will extract, sort and draft the answer once every page is read. Still list every matching record, applying the task's filters; never select only page winners. Related requirements for the same winners use the same comparison and records. Leave comparison null for counts, calculations, extra output fields or ambiguous formats.\n\n";

const READER_PAGES: &str = "# Lists over several pages\nA count, total or superlative over a list needs the whole list. Earlier pages are in the collected evidence under their own URLs. When the list goes on past this capture and the collected evidence does not cover the rest, add an entry to continues naming the requirement. For counts, put matching records in its tallies as above. For comparisons of record values, give in its records every record this capture adds to that comparison, each as the blocks holding it and the value compared. Apply the requirement's filters on every page: include every matching record with its compared value and the attributes that show it matches; omit records those filters exclude. Name in expands the control on this page that shows the rest, when one is offered. If instead the page states that the list is ordered or filtered by the very quantity being compared, the leading record answers: give the claim citing it its requirement id and cite that statement in orders_list rather than listing the requirement in continues. Once the collected evidence and this capture cover every page, the tally uses complete=true; a comparison's conclusion takes the requirement id, includes this chunk's records and draws on earlier records once each. A task that bounds the pages it covers ends at the last page it names.\n\n";

// A read resent its whole instruction set on every chunk of every page. On short tasks that fixed text was 80 to 88
// percent of the read's input, and most of it could not apply: tally and comparison guidance on a page holding no
// records, paging guidance where the page offers no pager. Which sections ship is decided by the page and the plan,
// never by the task's wording.
/// The reader's instructions for this capture, carrying only the sections its records and controls can use.
fn reader_system(capture: &Capture, continuing: &[String], counting: &IndexMap<String, String>) -> String {
    let records = capture.blocks.iter().any(|block| counted_kind(block.kind));
    let mut text = READER_BASE.to_string();
    if records || !counting.is_empty() {
        text.push_str(READER_TALLIES);
        text.push_str(READER_RANKINGS);
    }
    if records || !continuing.is_empty() {
        text.push_str(READER_PAGES);
    }
    text.push_str(&format!("\n\n# Trust\n{UNTRUSTED} Never infer facts the capture and evidence do not show."));
    text
}

const COLLECTION_EVIDENCE: &str = "\n\n# Collection evidence\nFor a collection summary, retain every inspected member and the collection's identifying boundary or total in records, including identities, roles and reply text. Account for every requested category. Report a bounded absence explicitly only when all members have been read and none matches. Selected excerpts, missing text and unread sections cannot establish absence. Use continues.records while more members or pages remain unread. When separate facts together answer one requirement, add a derived conclusion with that requirement id and draws_on for every supporting claim or prior note. A claim covering only some requested fields must leave requirement_id null. A requested link needs its quoted destination; navigation labels without destinations do not answer it. A written hostname or hostname/path is a destination without a URL scheme; preserve it separately from labels whose destinations are missing. Leave that field open when its destination has not been read.";

const FIELD_COLLECTIONS: &str = " Also fill collections independently of claims: quote the boundary and every member of each inspected collection, even when another section or field is still missing. Code records completeness from the page's stated total and captured list members. A navigation link to a section is not its contents. If no member matches a requested field, include the entire collection and its boundary in collections, including excluded members. Prior summaries cannot establish that absence. ";

const PAGE_RECORDS: &str = "# Page records\nCollect the records on this page for the requested requirements. Return each requirement in continues even when no records match. For counts of matching records, including filtered counts, or rankings by count, use continues.tallies, grouped by the label stated in each record (key=null for an ungrouped count). Select only matching records. If excluded records separate matches, list each matching block range explicitly in the group's records. A page with no matches contributes an empty group. For comparisons of record values, use continues.records, including the compared value and the attributes that show each record matches. Prefer a tally field range for complete record blocks with the grouping label between the same exact prefix and suffix: key=null, records=[], field={span, prefix, suffix}. Delimiters must occur once per record; include the newline for a field starting a line. Every block in that range must be a matching record. When using tallies, leave the continuation's records empty. Omit records excluded by the task's filters, but never select only the page's winners. Earlier pages are being read separately. Use context for source block ranges needed to interpret these records, such as filters or table headers. Do not conclude comparisons.\n\n# Trust\n";

const FIELD_REPAIR: &str = "Your tally field range could not be resolved into complete matching records. Read the same capture again. Use explicit record block ranges when the field's delimiters or range cannot be verified. The [source id] and (block kind) annotations are not literal record text. A field range must contain only complete record blocks, with key=null and records=[]. Retain every matching record and keep the requirement open unless the evidence covers the entire requested list.";

pub(super) async fn ask_reader<T: serde::de::DeserializeOwned>(
    llm: &dyn LLMClient,
    messages: &[Message],
    ty: Ty,
    ledger: Option<&Ledger>,
) -> Result<fastbrowse_core::llm::Generation<T>> {
    let schema = ModelSchema { ty };
    let generated = llm.generate_value(LLMPurpose::Read, messages, &schema, ledger).await?;
    let data = serde_json::from_value(generated.data).map_err(|error| AgentError::Invalid(error.to_string()))?;
    Ok(fastbrowse_core::llm::Generation { data, cost: generated.cost })
}

/// The whole capture as one chunk.
fn whole_page(page: &Page<'_>) -> Chunk {
    chunk_page(page, i64::MAX, 1).remove(0)
}

/// Characters of page each reader prompt shows: the whole page when it fits the LLM input budget beside the
/// instructions, schema and the evidence the read must show, or else the fewest even chunks that do. Other collected
/// evidence fills what each prompt leaves.
pub(super) fn reader_chunk_chars(
    page: &Page<'_>,
    tokens: &TokenBudget,
    system: &str,
    question: &str,
    requirement_ids: &[String],
    required: &str,
    schema: &Ty,
) -> i64 {
    let length = page.len();
    if length == 0 {
        return 1;
    }
    let message = read_message(page, &whole_page(page), question, requirement_ids, required);
    let prompt = format!("{system}{}{}", message.content, dumps(&json_schema(schema)));
    let overflow = text::len(&prompt) as i64 - tokens.llm_chars();
    let room = length - overflow;
    // The instructions or the evidence the read must show fill the budget without any page text: the page goes in
    // one prompt, which the read or the provider reports as too large, rather than in one-character chunks.
    if overflow <= 0 || room <= 0 {
        return i64::MAX;
    }
    let parts = (length + room - 1) / room;
    if parts <= 1 { i64::MAX } else { (length + parts - 1) / parts }
}

/// `read`: the remaining requirements read from `capture`, Jev's short-fact choice first where it can answer.
///
/// LLM claims reach the run's notes only once the whole page is read: the pager that says a list goes on sits at its
/// foot, in the last chunk, so which requirements a claim may close is settled after every chunk.
pub fn read<'a>(
    llm: &'a dyn LLMClient,
    capture: &'a Capture,
    question: &'a str,
    requirement_ids: &'a [String],
    notes: &'a mut Notes,
    options: &'a ReadOptions<'a>,
) -> BoxFuture<'a, Result<ReadOutcome>> {
    Box::pin(read_inner(llm, capture, question.to_string(), requirement_ids.to_vec(), notes, options))
}

async fn read_inner(
    llm: &dyn LLMClient,
    capture: &Capture,
    mut question: String,
    mut requirement_ids: Vec<String>,
    notes: &mut Notes,
    options: &ReadOptions<'_>,
) -> Result<ReadOutcome> {
    let page = Page::new(capture);
    let tokens = &options.tokens;
    let ledger = options.ledger;
    notes.remember_capture(capture);
    let mut facts: IndexMap<(String, Option<String>), Fact> = IndexMap::new();
    let mut costs: Vec<CostLine> = Vec::new();
    let mut continues: IndexSet<String> = IndexSet::new();
    let mut lost: IndexSet<String> = IndexSet::new();
    let mut found: Vec<Fact> = Vec::new();
    let mut collections: Vec<Fact> = Vec::new();
    let mut rejected = 0;
    let mut uncovered = 0;
    let mut ordered: IndexSet<String> = IndexSet::new();
    let mut expands: Option<String> = None;
    let mut tally_complete: IndexSet<String> = IndexSet::new();
    let mut through_end: Vec<String> = Vec::new();
    // Jev's word on whether a continuing requirement needs every page of its list, asked once per page read.
    let mut asked: HashSet<String> = HashSet::new();
    let mut every_page: HashSet<String> = HashSet::new();
    let task = question.clone();
    let mut continuation_records: IndexMap<String, Vec<String>> = IndexMap::new();
    let mut context_records: IndexMap<String, Vec<String>> = IndexMap::new();
    let mut ended: Vec<String> = Vec::new();
    let mut tally_readers: Vec<TallyReader> = Vec::new();
    let mut comparisons: IndexMap<String, NumericComparison> = IndexMap::new();
    let counting: IndexMap<String, String> = options
        .requirements
        .iter()
        .filter(|r| r.kind == RequirementKind::Information && r.count_records)
        .map(|r| (r.id.clone(), r.text.clone()))
        .collect();
    let wanted: Vec<Requirement> = options
        .requirements
        .iter()
        .filter(|r| {
            requirement_ids.contains(&r.id)
                && r.kind == RequirementKind::Information
                && !options.continuing.contains(&r.id)
                && !options.incomplete.contains(&r.id)
        })
        .cloned()
        .collect();
    if let Some(jev) = options.jev
        && !wanted.is_empty()
        && !options.records_only
    {
        let earlier = if options.revalidate { None } else { Some(&*notes) };
        let chosen =
            read_choices(jev, &page, &wanted, tokens, ledger, earlier, options.recovering_outputs, &options.trace)
                .await?;
        costs.extend(chosen.cost_lines.iter().cloned());
        for fact in chosen.facts {
            notes.add(fact.clone());
            facts.insert((fact_id(&fact), fact.requirement_id.clone()), fact);
        }
        let answered: HashSet<Option<String>> = facts.values().map(|fact| fact.requirement_id.clone()).collect();
        requirement_ids.retain(|key| !answered.contains(&Some(key.clone())) && !chosen.absent.contains(key));
        if requirement_ids.is_empty() {
            let carried = match &options.context_question {
                Some(asked) => {
                    let (carried, spent) = carries_context(jev, capture, asked, tokens, ledger).await?;
                    costs.extend(spent);
                    carried
                }
                None => false,
            };
            if !carried {
                return Ok(ReadOutcome::new(facts.into_values().collect(), rejected, costs));
            }
            question
                .push_str("\n\nEvery requirement is answered. Read only the context claims the task's actions need.");
        } else {
            // Narrow the obligations without dropping the task's constraints or separating ids from their meaning.
            let remaining: Vec<String> = options
                .requirements
                .iter()
                .filter(|r| requirement_ids.contains(&r.id))
                .map(|r| format!("- {}: {}", r.id, r.text))
                .collect();
            question.push_str(&format!("\n\nRead only these remaining requirements:\n{}", remaining.join("\n")));
        }
    }
    if options.require_all_evidence {
        question.push_str("\n\nCode attaches every earlier comparison record to each requirement's conclusion. You need not repeat those references in draws_on; use it for any extra context the claim needs. Still include every compared record from this capture in records, and state the requested values in the answer.");
    }
    if options.records_only {
        question.push_str("\n\nThis page is being read while earlier pages are still being read. Collect this page's records for each requirement in continues.records or continues.tallies, with any quoted context needed to interpret them. Apply the requirement's filters: include every matching record with its compared value and the attributes that show it matches; omit records those filters exclude. Do not select only this page's winners. Do not conclude a comparison or mark a tally complete. A later read will receive all pages' records and answer the question.");
    }
    // LLM claims reach the run's notes only once the whole page is read. Carry earlier chunks and Jev's facts into each
    // chunk so a count or comparison is not asked in ignorance of what was already collected.
    let mut so_far = notes.clone();
    route(&options.trace, "llm", "remaining_requirements", None, vec![("requirements", json!(requirement_ids))]);
    let collection_schema = options.field_outputs && !options.records_only;
    let system = if options.records_only {
        format!("{PAGE_RECORDS}{UNTRUSTED}")
    } else {
        let mut system = reader_system(capture, &options.continuing, &counting);
        if options.preserve_collections {
            system.push_str(COLLECTION_EVIDENCE);
        }
        if options.field_outputs {
            system.push_str(FIELD_COLLECTIONS);
            system.push_str(FIELD_CATEGORIES);
        }
        system
    };
    let schema = if options.records_only { RecordsResponse::ty() } else { read_response_ty(collection_schema) };
    // Reading a new source does not require every earlier project's evidence in the same prompt. Comparisons that
    // need all records and final verdicts still refuse incomplete evidence.
    let render = |notes: &Notes| RenderOptions {
        preserve_requirements: options.require_all_evidence,
        preserve_collections: options.preserve_collections,
        source_only: false,
        json_encoded: false,
        labels: Some(notes.facts().iter().enumerate().map(|(i, fact)| (fact_id(fact), format!("e{i}"))).collect()),
    };
    let chunk_chars = match options.max_chars {
        Some(chars) => chars,
        None => {
            let required = if options.require_all_evidence {
                so_far.render_with_ids(i64::MAX, &render(&so_far))?.text
            } else {
                String::new()
            };
            reader_chunk_chars(&page, tokens, &system, &question, &requirement_ids, &required, &schema)
        }
    };
    let mut last: Option<(Chunk, bool)> = None;
    for part in chunk_page(&page, chunk_chars, 1) {
        let mut messages =
            vec![Message::system(system.clone()), read_message(&page, &part, &question, &requirement_ids, "")];
        let room = notes_room(tokens, &messages, &schema);
        let render = render(&so_far);
        let labels = render.labels.clone().unwrap_or_default();
        let offered_notes = match so_far.render_with_ids(room, &render) {
            Ok(rendered) => rendered,
            Err(AgentError::Invalid(_)) => {
                return Err(AgentError::NotesTooLarge(format!(
                    "The {room} character reader notes budget cannot report omitted evidence"
                )));
            }
            Err(other) => return Err(other),
        };
        if options.require_all_evidence {
            let offered_ids: HashSet<&String> = offered_notes.evidence_ids.iter().collect();
            let lost_any = so_far.facts().iter().map(|fact| fact_id(fact)).any(|key| !offered_ids.contains(&key));
            if lost_any {
                return Err(AgentError::NotesTooLarge(
                    "The final page cannot fit every earlier record in its collected evidence".into(),
                ));
            }
        }
        messages[1] = read_message(&page, &part, &question, &requirement_ids, &offered_notes.text);
        let mut result: fastbrowse_core::llm::Generation<ReadResponse>;
        if options.records_only {
            let collected = ask_reader::<RecordsResponse>(llm, &messages, schema.clone(), ledger).await?;
            ended = collected.data.ended.iter().filter(|key| requirement_ids.contains(key)).cloned().collect();
            // Context is no requirement's record, so a cite that does not resolve loses none of them.
            let context: Vec<ReadClaim> = collected
                .data
                .context
                .iter()
                .filter_map(|cite| {
                    let span = cited(&page, &part, cite)?;
                    Some(ReadClaim {
                        cite: Some(cite.clone()),
                        excerpt: None,
                        draws_on: Vec::new(),
                        records: Vec::new(),
                        orders_list: None,
                        text: span.quote,
                        requirement_id: None,
                    })
                })
                .collect();
            // An omitted requirement could have lost a whole page. An explicit empty record set means none matched.
            let returned: HashSet<&String> = collected.data.continues.iter().map(|c| &c.requirement_id).collect();
            lost.extend(requirement_ids.iter().filter(|key| !returned.contains(key)).cloned());
            result = fastbrowse_core::llm::Generation {
                data: ReadResponse {
                    tallies: Vec::new(),
                    claims: context,
                    answered: false,
                    continues: collected
                        .data
                        .continues
                        .into_iter()
                        .map(|c| Continuation {
                            comparison: c.comparison,
                            tallies: c.tallies,
                            reuse_field: false,
                            requirement_id: c.requirement_id,
                            expands: None,
                            records: c.records,
                        })
                        .collect(),
                    collections: Vec::new(),
                },
                cost: collected.cost,
            };
        } else {
            result = ask_reader::<ReadResponse>(llm, &messages, schema.clone(), ledger).await?;
            let groups: Vec<&TallyGroup> = result
                .data
                .tallies
                .iter()
                .filter(|t| requirement_ids.contains(&t.requirement_id))
                .flat_map(|t| &t.groups)
                .chain(
                    result
                        .data
                        .continues
                        .iter()
                        .filter(|c| requirement_ids.contains(&c.requirement_id))
                        .flat_map(|c| &c.tallies),
                )
                .collect();
            let malformed = groups.iter().any(|g| {
                g.field.as_ref().is_some_and(|field| {
                    g.key.is_some() || !g.records.is_empty() || field_groups(&page, &part, field).is_none()
                })
            });
            if malformed {
                // A malformed field range must be repaired on this capture before it poisons later pages' counts.
                if let Some(ledger) = ledger {
                    ledger.record([result.cost.clone()])?;
                }
                costs.push(result.cost.clone());
                let mut repaired = messages.clone();
                repaired.push(Message::user(FIELD_REPAIR));
                result = ask_reader::<ReadResponse>(llm, &repaired, schema.clone(), ledger).await?;
            }
        }
        if let Some(ledger) = ledger {
            ledger.record([result.cost.clone()])?;
        }
        costs.push(result.cost.clone());
        let mut accepted = 0;
        let mut rejected_here = 0;
        // The latest chunk decides: it holds the page's foot, where a pager sits, reads every earlier chunk's records in
        // its collected evidence, and is given the caller's next-page notice. An earlier chunk's "continues" meant the
        // list went on into this chunk; a union let it block the last chunk's conclusion.
        let carried: Vec<Continuation> = result
            .data
            .continues
            .iter()
            .filter(|c| requirement_ids.contains(&c.requirement_id))
            .map(|c| Continuation { records: record_cites(&page, &part, &c.records, record_kind), ..c.clone() })
            // A count returned as plain continuation records otherwise reaches the last page with no tally to close.
            .map(|c| {
                if counting.contains_key(&c.requirement_id) && !c.records.is_empty() {
                    let mut tallies = c.tallies.clone();
                    tallies.push(TallyGroup { key: None, records: c.records.clone(), field: None });
                    Continuation { tallies, records: Vec::new(), ..c }
                } else {
                    c
                }
            })
            .collect();
        continues = carried.iter().map(|c| c.requirement_id.clone()).collect();
        let texts: IndexMap<&str, &str> =
            options.requirements.iter().map(|r| (r.id.as_str(), r.text.as_str())).collect();
        // A records-only read is a page of a list already being paged, and its caller takes no scope from it.
        if let Some(jev) = options.jev
            && !options.records_only
        {
            let unasked: Vec<(String, String)> = carried
                .iter()
                .filter(|c| asked.insert(c.requirement_id.clone()))
                .filter_map(|c| Some((c.requirement_id.clone(), texts.get(c.requirement_id.as_str())?.to_string())))
                .collect();
            if !unasked.is_empty() {
                let (whole, spent) = scope(jev, &task, capture, &unasked, tokens, ledger).await?;
                costs.extend(spent);
                every_page.extend(whole);
            }
        }
        through_end = carried
            .iter()
            .filter(|c| every_page.contains(&c.requirement_id))
            .map(|c| c.requirement_id.clone())
            .collect();
        comparisons = carried
            .iter()
            .filter(|c| through_end.contains(&c.requirement_id) && !counting.contains_key(&c.requirement_id))
            .filter_map(|c| c.comparison.clone().map(|comparison| (c.requirement_id.clone(), comparison)))
            .collect();
        expands = carried.iter().find_map(|c| c.expands.clone().filter(|label| !label.is_empty()));
        for continuation in &carried {
            if !continuation.reuse_field
                || !through_end.contains(&continuation.requirement_id)
                || continuation.tallies.len() != 1
            {
                continue;
            }
            let group = &continuation.tallies[0];
            let records: Vec<&Block> = capture.blocks.iter().filter(|b| counted_kind(b.kind)).collect();
            if let Some(field) = &group.field
                && group.key.is_none()
                && group.records.is_empty()
                && part.total == 1
                && !records.is_empty()
                && field.span
                    == (Cite {
                        first: records[0].source_id.clone(),
                        last: records[records.len() - 1].source_id.clone(),
                    })
            {
                let reader = TallyReader {
                    requirement_id: continuation.requirement_id.clone(),
                    count_label: counting.get(&continuation.requirement_id).cloned(),
                    title: capture.title.clone(),
                    heading_path: records[0].heading_path.clone(),
                    frame_id: records[0].frame_id.clone(),
                    prefix: field.prefix.clone(),
                    suffix: field.suffix.clone(),
                    filter: None,
                    compares: false,
                    record_kind: None,
                    comparison: None,
                };
                if read_tallies(capture, std::slice::from_ref(&reader), std::slice::from_ref(&reader.requirement_id))?
                    .is_some()
                {
                    tally_readers.push(reader);
                }
            }
        }
        for continuation in &carried {
            // A filtered count or a comparison names its matches record by record, so a later page of the same list
            // is matched by Jev record by record rather than read again by the LLM. Code counts the matches, or picks
            // the winner from their values with the comparison's delimiters.
            let compares = continuation.comparison.is_some() && !counting.contains_key(&continuation.requirement_id);
            let Some(requirement_text) = texts.get(continuation.requirement_id.as_str()).copied() else { continue };
            if continuation.reuse_field
                || !through_end.contains(&continuation.requirement_id)
                || tally_readers.iter().any(|r| r.requirement_id == continuation.requirement_id)
                || !(compares
                    || (counting.contains_key(&continuation.requirement_id)
                        && continuation.tallies.len() == 1
                        && continuation.tallies[0].field.is_none()))
            {
                continue;
            }
            let cites = if compares { &continuation.records } else { &continuation.tallies[0].records };
            let quotes: Vec<Evidence> = cites.iter().filter_map(|cite| cited(&page, &part, cite)).collect();
            let blocks_by_id: IndexMap<(&String, &Option<String>), &Block> =
                capture.blocks.iter().map(|b| ((&b.source_id, &b.frame_id), b)).collect();
            let shapes: IndexSet<(Vec<String>, Option<String>, BlockKind)> = quotes
                .iter()
                .filter_map(|evidence| blocks_by_id.get(&(&evidence.source_id, &evidence.frame_id)))
                .filter(|block| counted_kind(block.kind))
                .map(|block| (block.heading_path.clone(), block.frame_id.clone(), block.kind))
                .collect();
            // The filter needs one record shape to find the next page's records by; matches cited across more than one
            // shape, or none, stay with the LLM reader. A comparison record that spans blocks would be asked about a
            // block at a time and lose its value, so only one-block records are filtered.
            if shapes.len() != 1 || quotes.is_empty() || (compares && cites.iter().any(|cite| cite.first != cite.last))
            {
                continue;
            }
            let Some((heading_path, frame_id, record_kind)) = shapes.into_iter().next() else { continue };
            tally_readers.push(TallyReader {
                requirement_id: continuation.requirement_id.clone(),
                // Keyed as the LLM reader keys a filtered count, so every page adds to one tally.
                count_label: if compares { None } else { Some(requirement_text.to_string()) },
                title: capture.title.clone(),
                heading_path,
                frame_id,
                prefix: String::new(),
                suffix: String::new(),
                filter: Some(requirement_text.to_string()),
                compares,
                record_kind: Some(record_kind),
                comparison: if compares { continuation.comparison.clone() } else { None },
            });
        }
        let mut references: IndexMap<String, String> = offered_notes
            .evidence_ids
            .iter()
            .map(|key| (labels.get(key).cloned().unwrap_or_default(), key.clone()))
            .collect();
        references.extend(offered_notes.evidence_ids.iter().map(|key| (key.clone(), key.clone())));
        let mut tally_reads: Vec<TallyRead> =
            result.data.tallies.iter().filter(|t| requirement_ids.contains(&t.requirement_id)).cloned().collect();
        tally_reads.extend(carried.iter().filter(|c| !c.tallies.is_empty()).map(|c| TallyRead {
            requirement_id: c.requirement_id.clone(),
            groups: c.tallies.clone(),
            complete: false,
        }));
        for tally_read in &tally_reads {
            let requirement_id = &tally_read.requirement_id;
            notes.unevidence([requirement_id.as_str()]);
            so_far.unevidence([requirement_id.as_str()]);
            if tally_read.complete && result.data.answered && part.index == part.total - 1 {
                tally_complete.insert(requirement_id.clone());
            }
            let mut groups: Vec<TallyGroup> = Vec::new();
            for group in &tally_read.groups {
                match &group.field {
                    None => groups.push(group.clone()),
                    Some(field) => {
                        let expanded = (group.key.is_none() && group.records.is_empty())
                            .then(|| field_groups(&page, &part, field))
                            .flatten()
                            .filter(|expanded| !expanded.is_empty());
                        match expanded {
                            Some(expanded) => groups.extend(expanded),
                            None => {
                                lost.insert(requirement_id.clone());
                                uncovered += 1;
                            }
                        }
                    }
                }
            }
            for group in groups {
                let mut records = Vec::new();
                // Capture makes each leaf list item one unit, so a counted range of them is that many records; a
                // compared record may still span more than one item, so only counting splits them.
                let cites = record_cites(&page, &part, &group.records, counted_kind);
                let mut missing = 0;
                for cite in &cites {
                    let Some(mut span) = cited(&page, &part, cite) else {
                        missing += 1;
                        continue;
                    };
                    if cite.first == cite.last
                        && let Some(block) = first_block(&page, &cite.first)
                        && block.kind == BlockKind::Record
                    {
                        // Chunking a long record must not count its quoted pieces as separate records.
                        span = evidence(&page, block, block.start, block.end);
                    }
                    let record = quoted(span);
                    let key = fact_id(&record);
                    so_far.add(record.clone());
                    notes.add(record.clone());
                    records.push(key.clone());
                    facts.insert((key, None), record);
                }
                if !records.is_empty() {
                    // Pages may label the same count differently; the requirement keeps its total and filter together.
                    let label = counting.get(requirement_id).cloned().unwrap_or_else(|| {
                        group.key.clone().filter(|k| !k.is_empty()).unwrap_or_else(|| "records".into())
                    });
                    let tally = Tally { requirement_id: requirement_id.clone(), key: label, records };
                    so_far.add_tally(tally.clone())?;
                    let fact = notes.add_tally(tally)?;
                    facts.insert((fact_id(&fact), None), fact);
                    accepted += 1;
                }
                uncovered += missing;
                if missing > 0 {
                    lost.insert(requirement_id.clone());
                }
            }
        }
        if collection_schema {
            for collection in &result.data.collections {
                if !requirement_ids.contains(&collection.requirement_id) || collection.records.is_empty() {
                    continue;
                }
                let records: Option<Vec<Evidence>> =
                    collection.records.iter().map(|cite| cited(&page, &part, cite)).collect();
                let Some(records) = records else { continue };
                let sources: Vec<Fact> = records.into_iter().map(quoted).collect();
                for source in &sources {
                    so_far.add(source.clone());
                    found.push(source.clone());
                }
                let keys = unique(sources.iter().map(fact_id));
                collections.push(Fact {
                    requirement_id: None,
                    tally: None,
                    comparison: Some(Comparison {
                        requirement_id: collection.requirement_id.clone(),
                        records: keys.clone(),
                        complete: false,
                    }),
                    text: collection.scope.clone(),
                    evidence: None,
                    basis: keys,
                    reader: FactReader::Llm,
                    scope: Some(collection.scope.clone()),
                });
            }
        }
        for (index, original) in result.data.claims.iter().enumerate() {
            let mut claim = original.clone();
            if options.require_all_evidence
                && let Some(requirement_id) = &claim.requirement_id
                && requirement_ids.contains(requirement_id)
            {
                // A winner must retain the records it beat even when the reader cites only its chosen rows.
                claim.draws_on.extend(so_far.comparison_records(Some(requirement_id)));
            }
            let mut records: IndexMap<String, Fact> = IndexMap::new();
            let mut missing = 0;
            for cite in &claim.records {
                match cited(&page, &part, cite) {
                    None => missing += 1,
                    Some(span) => {
                        let record = quoted(span);
                        records.insert(fact_id(&record), record);
                    }
                }
            }
            if missing > 0 {
                // A comparison missing an operand cannot close here or on a later page using these notes. A claim that
                // names no requirement of this read is no requirement's operand: a context claim citing a record the
                // changed page no longer resolves used to mark every open requirement lost for the rest of the run.
                if let Some(requirement_id) = claim.requirement_id.as_ref().filter(|id| requirement_ids.contains(id)) {
                    lost.insert(requirement_id.clone());
                }
                uncovered += missing;
                rejected_here += 1;
                continue;
            }
            if options.preserve_collections {
                // A partial collection claim can supply a later conclusion without closing the requirement itself.
                let cited_refs: Vec<String> =
                    claim.draws_on.iter().filter_map(|reference| references.get(reference).cloned()).collect();
                let inherited: IndexSet<String> = so_far
                    .expand_evidence_ids(cited_refs.iter().map(String::as_str))
                    .iter()
                    .flat_map(|key| context_records.get(key).cloned().unwrap_or_default())
                    .collect();
                let held = so_far.facts_by_id();
                for key in inherited {
                    if let Some(fact) = held.get(&key) {
                        records.insert(key, fact.clone());
                    }
                }
            }
            for (key, record) in &records {
                so_far.add(record.clone());
                found.push(record.clone());
                references.insert(key.clone(), key.clone());
            }
            claim.draws_on.extend(records.keys().cloned());
            // Carried to the next chunk without its requirement id, which only the whole page can settle.
            let unassigned = ReadClaim { requirement_id: None, ..claim.clone() };
            let Some(mut fact) = remember(
                &page,
                &part,
                &unassigned,
                &mut so_far,
                &references,
                options.preserve_collections,
                &options.trace,
            )?
            else {
                rejected_here += 1;
                continue;
            };
            // Context retained while copying a table quote must reach the caller's notes with the claim.
            let held: HashSet<String> =
                found.iter().map(fact_id).chain(notes.facts().iter().map(|fact| fact_id(fact))).collect();
            let extra: Vec<Fact> = so_far
                .facts()
                .into_iter()
                .filter(|kept| {
                    let key = fact_id(kept);
                    fact.basis.contains(&key) && !held.contains(&key)
                })
                .cloned()
                .collect();
            found.extend(extra);
            references.insert(format!("claim:{index}"), fact_id(&fact));
            if options.preserve_collections && !records.is_empty() {
                context_records.insert(fact_id(&fact), records.keys().cloned().collect());
            }
            let requirement_id = claim.requirement_id.clone().filter(|id| requirement_ids.contains(id));
            if let Some(id) = &requirement_id {
                let earlier = so_far.comparison_records(Some(id));
                if !records.is_empty() || !earlier.is_empty() {
                    fact.comparison = Some(Comparison {
                        requirement_id: id.clone(),
                        records: unique(earlier.into_iter().chain(records.keys().cloned())),
                        complete: false,
                    });
                }
            }
            // A page that states its own order backs the leading record it cites: the statement is kept beside it.
            if requirement_id.is_some()
                && let Some(orders_list) = &claim.orders_list
                && let Some(ordering) = cited(&page, &part, orders_list)
            {
                let stated = quoted(ordering);
                fact.basis.push(fact_id(&stated));
                so_far.add(stated.clone());
                found.push(stated);
                if let Some(id) = &requirement_id {
                    ordered.insert(id.clone());
                }
            }
            found.push(Fact { requirement_id, ..fact });
            accepted += 1;
        }
        // The records a continuing page compared, kept as facts so a later page's winner can show what it beat. Code
        // copies each quote from the blocks named, exactly as it does for a claim, so a record is the page's own text and
        // never the reader's retyping of it.
        for continuation in &carried {
            let mut missing = 0;
            for cite in &continuation.records {
                let Some(span) = cited(&page, &part, cite) else {
                    missing += 1;
                    continue;
                };
                let record = quoted(span);
                let key = fact_id(&record);
                so_far.add(record.clone());
                so_far.add_continuation(&continuation.requirement_id, &key);
                notes.add_continuation(&continuation.requirement_id, &key);
                continuation_records.entry(continuation.requirement_id.clone()).or_default().push(key);
                found.push(record);
            }
            uncovered += missing;
            if missing > 0 {
                lost.insert(continuation.requirement_id.clone());
            }
        }
        rejected += rejected_here;
        let answered = result.data.answered;
        last = Some((part.clone(), answered));
        // An unsupported assertion of completion cannot suppress reading the remaining chunks. Nor can it end a read of
        // a page whose list goes on, whether this chunk said so or the caller's notice did: the rest of this page is
        // part of the set being counted or compared, and a pager sits at the foot of a listing, in the last chunk,
        // after the chunk that believes it has the answer.
        if answered
            && accepted > 0
            && rejected_here == 0
            && continues.is_empty()
            && !options.records_only
            && (tally_reads.is_empty() || part.index == part.total - 1)
        {
            break;
        }
    }
    // Python reads `part` and `result` after the loop; a capture with no blocks has neither and raises.
    let Some((part, answered)) = last else {
        return Err(AgentError::Invalid(
            "cannot access local variable 'part' where it is not associated with a value".into(),
        ));
    };
    // A claim the reader gave a requirement id concludes that requirement: the list it ranged over is not open, and a
    // record the reader cited badly elsewhere does not undo it.
    let concluded: IndexSet<String> = found.iter().filter_map(|fact| fact.requirement_id.clone()).collect();
    for key in ordered.iter().chain(&concluded) {
        continues.shift_remove(key);
        lost.shift_remove(key);
    }
    for requirement_id in &tally_complete {
        // Earlier continuation quotes have no tally key, so counting only later pages would omit them.
        if notes.has_untallied_records(requirement_id) {
            lost.insert(requirement_id.clone());
        }
    }
    let mut blocked: HashSet<String> =
        options.incomplete.iter().cloned().chain(lost.iter().cloned()).filter(|key| !concluded.contains(key)).collect();
    if options.records_only {
        blocked.extend(requirement_ids.iter().cloned());
    }
    for mut fact in found {
        if let Some(comparison) = &mut fact.comparison {
            // Another requirement left open says nothing about this one's collection.
            comparison.complete = fact.requirement_id.as_ref().is_some_and(|id| {
                (!options.field_outputs || options.require_all_evidence || ordered.contains(id))
                    && (answered || requirement_ids.len() > 1)
                    && part.index == part.total - 1
            });
        }
        // Its quote was verified against this capture when the chunk was read.
        notes.add(fact.clone());
        facts.insert((fact_id(&fact), fact.requirement_id.clone()), fact);
    }
    for requirement_id in &tally_complete {
        if !continues.contains(requirement_id) && !blocked.contains(requirement_id) {
            notes.complete_tallies(requirement_id);
        }
    }
    for mut collection in collections {
        if let Some(comparison) = &mut collection.comparison
            && (continues.contains(&comparison.requirement_id) || blocked.contains(&comparison.requirement_id))
        {
            comparison.complete = false;
        }
        notes.add(collection.clone());
        facts.insert((fact_id(&collection), None), collection);
    }
    let open: Vec<String> = requirement_ids.iter().filter(|key| !blocked.contains(*key)).cloned().collect();
    let targets = options.targets.as_deref().unwrap_or(&options.requirements);
    let (collection_facts, spent) =
        counted_collections(&page, &open, part.end, targets, options.jev, tokens, ledger).await?;
    costs.extend(spent);
    if options.field_outputs {
        for fact in &collection_facts {
            notes.add(fact.clone());
            facts.insert((fact_id(fact), None), fact.clone());
        }
    }
    let outcome = ReadOutcome {
        facts: facts.into_values().collect(),
        collection_facts,
        rejected_claims: rejected,
        cost_lines: costs,
        continues: continues.iter().cloned().collect(),
        incomplete: lost.iter().cloned().collect(),
        uncovered,
        expands: if continues.is_empty() { None } else { expands },
        through_end: through_end.into_iter().filter(|key| continues.contains(key)).collect(),
        continuation_records: continuation_records.into_iter().map(|(key, records)| (key, unique(records))).collect(),
        ended: ended.into_iter().filter(|key| !lost.contains(key)).collect(),
        tally_readers: tally_readers.into_iter().filter(|reader| !lost.contains(&reader.requirement_id)).collect(),
        comparisons: comparisons.into_iter().filter(|(key, _)| !lost.contains(key)).collect(),
    };
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retrieval::testing::capture;

    /// The reader's chunks of a 200-row page under a budget of the whole prompt's length less `short` characters.
    fn chunks(short: i64) -> Vec<Chunk> {
        let rows: Vec<String> = (0..200).map(|i| format!("Row {i} {}", "x".repeat(90))).collect();
        let captured = capture(&rows.iter().map(|row| (BlockKind::Paragraph, row.as_str())).collect::<Vec<_>>());
        let page = Page::new(&captured);
        let (system, question, ids, schema) =
            ("# Reader", "Which rows?", vec!["r".to_string()], read_response_ty(false));
        let message = read_message(&page, &whole_page(&page), question, &ids, "");
        let whole = text::len(&format!("{system}{}{}", message.content, dumps(&json_schema(&schema)))) as i64;
        let tokens =
            TokenBudget { llm_context_tokens: Some(whole - short), chars_per_token: 1.0, ..TokenBudget::default() };
        chunk_page(&page, reader_chunk_chars(&page, &tokens, system, question, &ids, "", &schema), 1)
    }

    #[test]
    fn a_page_the_budget_holds_is_read_in_one_prompt() {
        assert_eq!(chunks(0).len(), 1);
    }

    #[test]
    fn a_page_past_the_budget_is_split_into_the_fewest_even_chunks() {
        let parts = chunks(5_000);
        assert_eq!(parts.len(), 2);
        let sizes: Vec<usize> = parts.iter().map(|part| part.block_ids.len()).collect();
        assert!(sizes[0].abs_diff(sizes[1]) <= 2, "{sizes:?}");
    }

    #[test]
    fn instructions_past_the_budget_go_in_one_prompt_rather_than_a_character_at_a_time() {
        assert_eq!(chunks(100_000).len(), 1);
    }
}
