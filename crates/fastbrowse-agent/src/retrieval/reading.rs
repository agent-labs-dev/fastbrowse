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
use fastbrowse_core::schema::{Field, ModelTy, Schema, Ty};
use fastbrowse_core::telemetry::{Ledger, Trace};
use fastbrowse_core::validation::ModelSchema;
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::choices::read_choices;
use super::chunks::{Chunk, Cite, chunk_page, cited, evidence, offered, read_message, source_marker};
use super::text::{Page, alnum_runs, counted_heading, lines_of, loose_matches, loose_search, numbers, states_empty};
use super::{notes_room, route};
use crate::batches::evaluate_batches;
use crate::comparison::{NumericComparison, QuotedField, digit};
use crate::error::{AgentError, Result};
use crate::memory::{Comparison, Fact, Notes, RenderOptions, Tally, evidence_id, fact_id, unique};
use crate::planner::{FIELD_CATEGORIES, Requirement, RequirementKind};
use crate::support::generate;

// Twelve thousand characters leave room for source blocks, accumulated evidence and instructions per read.
pub(crate) const READ_CHUNK_CHARS: i64 = 12_000;
// Navigation before the details can fill a chunk and buy a read with no claims.
const LLM_READ_CHUNK_CHARS: i64 = 24_000;
// Ours. A quote card on quotes.toscrape.com runs to about 1,100 characters with its author and tags; a block much
// longer than this is prose, where the sentence around a value is not what names its subject.
const CONTEXT_BLOCK_CHARS: i64 = 2_000;
// A page of a list costs two short block labels a record. The cap is what one capture can plausibly show, and bounds
// what one page adds to the notes. It is applied in code, not the schema: a reply over it, or a pager-only chunk with
// no records, would otherwise fail validation and end the run over a page that read fine.
const MAX_CONTINUING_RECORDS: usize = 60;

fn counted_kind(kind: BlockKind) -> bool {
    matches!(kind, BlockKind::Record | BlockKind::ListItem)
}

// --- response models ------------------------------------------------------------------------------------------------

const COMPARISON: &str = "For a lowest/highest/top-N ranking by one numeric field whose requested output is only record labels and values: the order, number of results, and literal delimiters around each record's label and displayed value (including its currency). Empty delimiter means the quote boundary. Use only when these fields occur exactly once in every matching record, and all remaining pages are needed. Apply the task's filters in records as usual. Null for counts, calculations, extra output fields, tie rules or values requiring unit conversion. Related requirements asking for the same winners' labels or values use the same comparison and records.";
const TALLIES: &str = "For every count of matching records, including filtered counts, or ranking by count. Group matching records by their stated label, or use key=null for an ungrouped count. Use explicit record ranges when matches are not contiguous. Leave records empty.";
const THROUGH_END: &str = "True only when the task needs every remaining page of this list. False when the task bounds the pages (such as this page and the next), or the rest is behind an expander rather than a pager.";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListOrder {
    pub confirmed: bool,
}

impl Schema for ListOrder {
    fn ty() -> Ty {
        ModelTy::frozen("_ListOrder", vec![Field::required("confirmed", Ty::Bool)]).ty()
    }
}

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
                Field::default("excerpt", Ty::optional(Ty::Str.constrained("minLength", 1).constrained("maxLength", 2000)), Value::Null).doc("An optional literal contiguous passage within cite, including record identity, field labels and qualifications. Use for a long block holding unrelated content; code must find it uniquely and copies the original source spelling. Null when the complete cited blocks are needed."),
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
    #[serde(default)]
    pub through_end: bool,
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
                Field::default("through_end", Ty::Bool, false).doc(THROUGH_END),
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
    pub through_end: bool,
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
                Field::default("through_end", Ty::Bool, false).doc(THROUGH_END),
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
    pub requested_scope_complete: Vec<String>,
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
                Field::default("requested_scope_complete", Ty::list(Ty::Str), json!([])).doc("Requirement ids whose requested bounded page scope is complete, even if another pager remains. Do not use for physical list exhaustion."),
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
    /// Records the first page's reader matched, shown to Jev so it applies the filter the same way.
    #[serde(default)]
    pub examples: Vec<String>,
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
    pub coverage: Vec<i64>,
    pub rejected_claims: i64,
    pub cost_lines: Vec<CostLine>,
    /// Requirements whose list goes on past this capture, so no claim from it closes them.
    pub continues: Vec<String>,
    /// Requirements whose continuation lost a record, so later comparisons cannot settle them.
    pub incomplete: Vec<String>,
    /// Records a continuation named that no run of offered blocks resolved, or that fell past the cap, so this page's
    /// comparison is incomplete in the notes even though the reader believed it had listed them.
    pub uncovered: i64,
    /// The label the reader gave for the control that shows the rest of the list, so the run can open it instead of
    /// being told only that the list goes on.
    pub expands: Option<String>,
    pub through_end: Vec<String>,
    pub continuation_records: IndexMap<String, Vec<String>>,
    pub ended: Vec<String>,
    pub requested_scope_complete: Vec<String>,
    pub tally_readers: Vec<TallyReader>,
    pub comparisons: IndexMap<String, NumericComparison>,
}

/// pydantic writes a `tuple[Fact, ...]` field by its declared type, so a collection fact there loses its scope.
fn as_plain_facts<S: serde::Serializer>(facts: &[Fact], serializer: S) -> std::result::Result<S::Ok, S::Error> {
    let plain: Vec<Fact> = facts.iter().map(|fact| Fact { scope: None, ..fact.clone() }).collect();
    plain.serialize(serializer)
}

impl ReadOutcome {
    fn new(facts: Vec<Fact>, coverage: Vec<i64>, rejected_claims: i64, cost_lines: Vec<CostLine>) -> ReadOutcome {
        ReadOutcome {
            facts,
            collection_facts: Vec::new(),
            coverage,
            rejected_claims,
            cost_lines,
            continues: Vec::new(),
            incomplete: Vec::new(),
            uncovered: 0,
            expands: None,
            through_end: Vec::new(),
            continuation_records: IndexMap::new(),
            ended: Vec::new(),
            requested_scope_complete: Vec::new(),
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
) -> Result<Option<Fact>> {
    let mut basis: Vec<String> = Vec::new();
    for reference in &claim.draws_on {
        // Python logs the reference it dropped.
        let Some(key) = references.get(reference) else { continue };
        let sources = if preserve_collections { source_basis(notes, key) } else { vec![key.clone()] };
        for source in sources {
            push_unique(&mut basis, source);
        }
    }
    let mut quote = claim.cite.as_ref().and_then(|cite| cited(page, part, cite));
    if let Some(raw) = &claim.excerpt {
        let Some(span) = quote.clone() else { return Ok(None) };
        if text::strip(raw).is_empty() {
            return Ok(None);
        }
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
        let Some((offset, stop)) = excerpt_match(page, &span, &matches) else {
            return Ok(None);
        };
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
        || block.end - block.start > CONTEXT_BLOCK_CHARS
        || !page.slice(block.start, block.end).contains('\n')
        || (text::strip(page.slice(block.start, quote.start)).is_empty()
            && text::strip(page.slice(quote.end, block.end)).is_empty())
    {
        return None;
    }
    let span = evidence(page, block, block.start, block.end);
    Some(Fact::read(None, &span.quote.clone(), span, reader))
}

/// `_excerpt_match`: the one place in the cited run an excerpt names, or None when it is missing or could be either of
/// two.
///
/// Equal text in two places can be two records' values, the same price under two products, so a repeated excerpt is
/// refused. One case is not ambiguous: the excerpt is the whole of exactly one block, and its other matches sit inside
/// longer blocks that say more around it. A wizard's result panel read "Multi-step form submitted successfully." and
/// its toast "Success toast: Multi-step form submitted successfully."; refused as ambiguous, the confirmation was never
/// noted, and two runs that had submitted the form ended stuck.
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

fn quoted_count(fact: &Fact, notes: &Notes, records: &IndexMap<String, Fact>, page: &Page<'_>) -> bool {
    let Some(quote) = &fact.evidence else { return false };
    let capture = page.capture;
    if !fact.basis.is_empty() {
        let facts = notes.facts_by_id();
        let mut counted: HashSet<String> = records.keys().cloned().collect();
        counted.extend(notes.comparison_records(None));
        counted.extend(notes.tallies().into_iter().flat_map(|t| t.records));
        for key in &fact.basis {
            // Scope quotes may accompany a stated total; counted records cannot turn a page number into that total.
            let Some(context) = facts.get(key) else { return false };
            let Some(span) = &context.evidence else { return false };
            if counted.contains(key) || !context.basis.is_empty() || context.tally.is_some() {
                return false;
            }
            if span.capture_sha256 != capture.sha256 {
                return false;
            }
            let blocks: Vec<&Block> =
                capture.blocks.iter().filter(|b| b.start < span.end && b.end > span.start).collect();
            if blocks.is_empty()
                || blocks.iter().any(|b| !matches!(b.kind, BlockKind::Heading | BlockKind::Paragraph | BlockKind::Link))
            {
                return false;
            }
            if blocks
                .iter()
                .any(|b| b.frame_id != span.frame_id || b.source_url.as_deref().unwrap_or(&capture.url) != span.url)
            {
                return false;
            }
        }
    }
    let stated = numbers(&fact.text);
    let shown: HashSet<String> = numbers(&quote.quote).into_iter().map(|n| n.replace(',', "")).collect();
    stated.len() == 1 && shown.contains(&stated[0].replace(',', ""))
}

fn field_groups(page: &Page<'_>, part: &Chunk, field: &TallyField) -> Option<Vec<TallyGroup>> {
    let span = cited(page, part, &field.span)?;
    let blocks: Vec<&Block> =
        offered(page, part).into_iter().filter(|b| b.start < span.end && b.end > span.start).collect();
    if blocks.is_empty() || blocks.len() > MAX_CONTINUING_RECORDS {
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
        if text::len(&key) > 200 {
            return None;
        }
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

/// Fully shown counted lists as complete collections.
///
/// `targets` is every information requirement of the plan. When there is more than one, a list counts for a target
/// only if the page names it: with one requirement left to read, an unrelated "Reviews (3)" on another target's page
/// would otherwise close the last target's collection.
pub(crate) fn counted_collections(
    page: &Page<'_>,
    requirement_ids: &[String],
    read_end: i64,
    requirements: &[Requirement],
    targets: Option<&[Requirement]>,
) -> Vec<Fact> {
    let capture = page.capture;
    let targets = targets.unwrap_or(requirements);
    let blocks = &capture.blocks;
    let mut collections = Vec::new();
    for (index, heading) in blocks.iter().enumerate() {
        if heading.kind != BlockKind::Heading || heading.frame_id.is_some() {
            continue;
        }
        let title = page.slice(heading.start, heading.end);
        let count = counted_heading(title);
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
        if count.is_none() {
            let following = &blocks[index + 1..(index + 3).min(blocks.len())];
            if following.first().is_some_and(|b| b.kind == BlockKind::Heading) {
                continue;
            }
            let empty =
                following.iter().find(|b| b.kind == BlockKind::Paragraph && states_empty(page.slice(b.start, b.end)));
            match empty {
                Some(empty) if members.is_empty() => members = vec![empty],
                _ => continue,
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
        if let Some(count) = &count {
            let total = match members[0].list_count {
                Some(total) => total,
                None => {
                    if members.iter().any(|b| !counted_kind(b.kind)) {
                        continue;
                    }
                    members.len() as i64
                }
            };
            if Some(total) != digits_value(count) {
                continue;
            }
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
        let keys: Vec<String> = sources.iter().map(fact_id).collect();
        collections.extend(sources);
        let named = |subject: &str| -> HashSet<String> {
            targets
                .iter()
                .filter(|r| text::len(subject) > 3 && names_subject(&r.text, subject))
                .map(|r| r.id.clone())
                .collect()
        };
        let site = capture.title.split(" - ").next().unwrap_or_default();
        let mut matching: HashSet<String> = HashSet::new();
        for subject in heading.heading_path.iter().map(String::as_str).chain([site]) {
            matching.extend(named(subject));
        }
        if count.is_some() {
            // The counted heading can name the target itself. A label every requirement mentions names none.
            let label = title.rfind('(').map_or(title, |at| &title[..at]);
            let own = named(text::strip(label));
            if own.len() < targets.len() {
                matching.extend(own);
            }
        }
        for requirement_id in requirement_ids {
            if targets.len() > 1 && !matching.contains(requirement_id) {
                continue;
            }
            let scope = format!("{}: {title}", capture.title);
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
    collections
}

/// `_counted_collections(capture, ...)`, for callers holding only the capture.
pub fn counted_collections_for(
    capture: &Capture,
    requirement_ids: &[String],
    read_end: i64,
    requirements: &[Requirement],
    targets: Option<&[Requirement]>,
) -> Vec<Fact> {
    counted_collections(&Page::new(capture), requirement_ids, read_end, requirements, targets)
}

/// `read_tallies`: a later page of a list counted by the field the reader set up, without asking the reader again.
pub fn read_tallies(
    capture: &Capture,
    readers: &[TallyReader],
    requirement_ids: &[String],
) -> Result<Option<ReadOutcome>> {
    let page = Page::new(capture);
    if capture.inaccessible_frames > 0 || page.len() > READ_CHUNK_CHARS {
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
    let parts = chunk_page(&page, READ_CHUNK_CHARS, 1);
    if parts.len() != 1 {
        return Ok(None);
    }
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
        let Some(groups) = field_groups(&page, &parts[0], &field) else { return Ok(None) };
        for group in groups {
            let mut keys = Vec::new();
            for cite in &group.records {
                let span = cited(&page, &parts[0], cite).expect("a field group's records resolve");
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
    Ok(Some(ReadOutcome::new(notes.facts().into_iter().cloned().collect(), vec![0], 0, Vec::new())))
}

/// A record Jev matches at or above the first, or rejects at or below the second, is settled; any record between sends
/// the whole page back to the LLM reader. Jev's yes for a plain author or rating match sits at 0.78 to 0.84, and a
/// wrongly dropped record leaves no trace, so a rejection needs more certainty than a match.
const RECORD_MATCH_FROM: f64 = 0.70;
const RECORD_REJECT_TO: f64 = 0.20;

/// `filter_tallies`: a later page of a filtered count, counted by asking Jev whether each record matches, in one
/// request.
///
/// None when the page does not have the first page's shape or Jev is unsure of any record: the LLM reader then reads
/// the page as before. The records are code-cut blocks, so every counted one is still a quote.
pub async fn filter_tallies(
    jev: &dyn DecisionClient,
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
    if shapes.len() != 1 || readers.iter().any(|reader| capture.title != reader.title) {
        return Ok(None);
    }
    let shaped = |b: &Block| counted_kind(b.kind) && shapes.contains(&(&b.heading_path, &b.frame_id, Some(b.kind)));
    // The records are the blocks shaped like the ones the first page's reader cited; a sidebar or a footer list sits
    // under another heading and is not asked about. They must also run unbroken: a featured strip of the same shape
    // elsewhere on the page would be counted again on every page that repeats it.
    let positions: Vec<usize> =
        capture.blocks.iter().enumerate().filter(|(_, b)| shaped(b)).map(|(index, _)| index).collect();
    if positions.is_empty() || positions.len() > MAX_CONTINUING_RECORDS {
        return Ok(None);
    }
    let between = &capture.blocks[positions[0]..=positions[positions.len() - 1]];
    if between.iter().any(|b| counted_kind(b.kind) && !shaped(b))
        || between.iter().filter(|b| counted_kind(b.kind)).count() != positions.len()
    {
        return Ok(None);
    }
    let page = Page::new(capture);
    let records: Vec<&Block> = positions.iter().map(|index| &capture.blocks[*index]).collect();
    let mut questions: IndexMap<String, Question> = IndexMap::new();
    for reader in readers {
        let filter = reader.filter.as_deref().unwrap_or_default();
        for (index, block) in records.iter().enumerate() {
            let question = NoulQuestion::new(
                format!(
                    "{UNTRUSTED}\n\n# Requirement\n{filter}\n\n# Record\n{}\n\nDoes this record pass every filter the requirement states, so that it is one of the records the requirement counts or compares? Judge this record alone; do not judge whether it wins.",
                    page.slice(block.start, block.end)
                ),
                Some("Yes, this record matches the requirement."),
                Some("No, this record does not match."),
            );
            questions.insert(format!("{}::{index}", reader.requirement_id), question.into());
        }
    }
    let reader = &readers[0];
    // Two requirements can filter the same records differently, such as a count of Ada's records and a count of
    // Ben's. Each requirement keeps the examples its own first-page reader matched, keyed by the filter the reader
    // states, so its records are never judged by another requirement's reading.
    let mut examples: IndexMap<&str, Vec<&str>> = IndexMap::new();
    for one in readers {
        let kept = examples.entry(one.filter.as_deref().unwrap_or_default()).or_default();
        for quote in &one.examples {
            if !kept.contains(&quote.as_str()) {
                kept.push(quote);
            }
        }
    }
    let state = json!({
        "page": {"url": capture.url, "title": capture.title, "heading": reader.heading_path},
        // How each requirement's reader applied its filter on the first page, so a later record is judged by the
        // same reading the requirement's own records were kept under.
        "matching_examples_from_the_first_page": examples,
    });
    let Some(answered) = evaluate_batches(jev, &state, &questions, tokens, ledger, None, true).await? else {
        return Ok(None);
    };
    let mut notes = Notes::default();
    let mut continuation_records: IndexMap<String, Vec<String>> = IndexMap::new();
    for reader in readers {
        let mut keys: Vec<String> = Vec::new();
        for (index, block) in records.iter().enumerate() {
            let Some(Answer::Noul(answer)) = answered.answers.get(&format!("{}::{index}", reader.requirement_id))
            else {
                return Ok(None);
            };
            let probability = answer.probability;
            if RECORD_REJECT_TO < probability && probability < RECORD_MATCH_FROM {
                trace.event("record_filter", || {
                    let mut fields = Map::new();
                    fields.insert("reader".into(), "llm".into());
                    fields.insert("reason".into(), "uncertain_record".into());
                    fields.insert("probability".into(), probability.into());
                    fields
                });
                return Ok(None);
            }
            if probability >= RECORD_MATCH_FROM {
                let text = page.slice(block.start, block.end);
                // Code picks the winner from each kept record's label and value; one it cannot read them from sends
                // the page to the LLM reader rather than leaving a record the comparison cannot rank.
                if let Some(comparison) = &reader.comparison
                    && (comparison.label.extract(text).is_none() || comparison.value.extract(text).is_none())
                {
                    trace.event("record_filter", || {
                        let mut fields = Map::new();
                        fields.insert("reader".into(), "llm".into());
                        fields.insert("reason".into(), "unreadable_value".into());
                        fields
                    });
                    return Ok(None);
                }
                let fact = quoted(evidence(&page, block, block.start, block.end));
                keys.push(fact_id(&fact));
                notes.add(fact);
            }
        }
        let matched = keys.len();
        if reader.compares {
            continuation_records.insert(reader.requirement_id.clone(), keys);
        } else if !keys.is_empty() {
            notes.add_tally(Tally {
                requirement_id: reader.requirement_id.clone(),
                key: reader.count_label.clone().filter(|label| !label.is_empty()).unwrap_or_else(|| "records".into()),
                records: keys,
            })?;
        }
        trace.event("record_filter", || {
            let mut fields = Map::new();
            fields.insert("reader".into(), "jev".into());
            fields.insert("requirement".into(), reader.requirement_id.clone().into());
            fields.insert("records".into(), records.len().into());
            fields.insert("matched".into(), matched.into());
            fields
        });
    }
    let mut outcome = ReadOutcome::new(notes.facts().into_iter().cloned().collect(), vec![0], 0, Vec::new());
    outcome.continues = readers.iter().map(|r| r.requirement_id.clone()).collect();
    outcome.continuation_records = continuation_records;
    Ok(Some(outcome))
}

// --- read ---------------------------------------------------------------------------------------------------------

/// `read`'s keyword arguments.
#[derive(Default, Clone)]
pub struct ReadOptions<'a> {
    pub max_chars: Option<i64>,
    pub tokens: TokenBudget,
    pub ledger: Option<&'a Ledger>,
    pub jev: Option<&'a dyn DecisionClient>,
    pub requirements: Vec<Requirement>,
    /// What the caller knows about the page that its text does not say, such as its next-page control; it goes with
    /// every question the reader is asked, however the question is narrowed.
    pub notice: String,
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
    pub skip_order_shortcuts: Vec<String>,
    pub trace: Trace,
}

const READER_BASE: &str = "# Reader\nAnswer the question from this capture's source blocks and the collected evidence. Each claim states only what its cited blocks, and the claims it draws on, show.\n\n# Sources\nThe [source id], (block kind) and embedded-frame labels are code's annotations, not page text. Repeated table headers provide context, not literal text inside every row. Citations use the source id; literal field delimiters use only the record text after those annotations. Do not copy annotation prefixes into field delimiters.\n\n# Claims\n- Write the requested value itself in each claim's text (the price, count, name or date): code checks the text against its cited blocks, and a claim that only says where the value is answers nothing.\n- Keep each claim to one fact or one derived conclusion. Do not summarize the whole task in one claim. A prior claim's prose is a hypothesis, not additional evidence: check its original quoted basis before carrying a value or inference forward. Preserve conditions such as eligibility, location and configuration in every claim that uses the value. A component maximum is not an aggregate maximum, and independent maxima are not additive unless their quoted sources explicitly allow simultaneous addition. A product name or nominal rating alone cannot settle an explicitly requested configuration. Leave an unsupported requested value open instead of inferring it from a related value.\n- For a long block containing unrelated content, set excerpt to the smallest literal contiguous passage supporting the claim, including record identity, field labels and qualifications. A pronoun or relative reference does not identify an entity on its own: keep the quoted antecedent alongside its value, extending the excerpt or citing a context claim with that literal identifying passage. Split claims supported by separate passages. Code matches the excerpt uniquely inside the cited blocks and copies its original spelling; never paraphrase it. Leave excerpt null when the complete blocks are needed.\n- A claim cites one run of blocks. For a comparison, put every compared record from this chunk in the conclusion's records as block ranges only. Code copies their quotes into its basis; never write a prose claim for each compared record. Use draws_on for earlier evidence and context claims.\n- The answer is checked later without the page, so a comparison also needs claims for the query, filters, date and sort that make it valid, with a null requirement id.\n- A count, total or winner rests on every record it counts or compares and on those context claims. It cites blocks only when the page itself states it.\n- Give a claim a requirement id only when it answers that whole requirement with its constraints; otherwise null. Set answered only when the collected evidence and this capture fully answer the question.\n- An explicit empty-result message is a finding: quote it and state that no matching records are reported within that page's scope. Missing text, a loading view or an unread section cannot establish absence.\n- Values typed into fields, suggestions and previews are inputs, not results.\n\n";

const READER_TALLIES: &str = "# Tallies\nFor a count of records or a ranking by record count, return tally groups: each key is the label stated in its records, and each record cites its own source blocks. Choose the entity level requested by the task. When counting items inside groups, group headings and subtotals are context: cite each matching item separately. When counting the groups themselves, cite each group instead of its children. Verify each counted entity's filters and required status. When the page explicitly states the requested total for the entire matching list, quote that statement as a claim instead of deriving it from partial tallies. A displayed subtotal, group count or number loaded so far does not state the requested whole-list total. Use continues.tallies while more pages remain and tallies with complete=true only on the last requested page. List only records from this chunk; never repeat earlier records, quote their text, calculate totals or write claims for counted records. Code deduplicates, counts and ranks them, preserving their quotes behind the tally references. Reuse a tally reference as a basis instead of listing every earlier record. Keep other claims concise, at most 60.\n\nA filtered count also uses tallies: select only matching records, grouped by their stated label (key=null for an ungrouped count). If matches are separated by excluded records, list their block ranges explicitly in the group's records. Never put counted records in continues.records. A page with no matches contributes an empty group.\n\nFor repeated record blocks with the grouping label between the same literal delimiters, prefer one tally group with key=null, records=[] and field: span covers the records, prefix and suffix are the exact text surrounding the label, each occurring once per record (include a newline in the prefix when it starts a line). Code extracts each record's label and counts it. Every block in the range must be a complete record matching the task; do not include headings, navigation or records excluded by a filter. When using tallies, leave the continuation's records empty.\n\nSet reuse_field=true for an unfiltered count of every record across this whole list, when all record blocks use this same grouping field. Code can then read later pages with the same structure. Any condition selecting records forbids reuse_field.\n\n";

const READER_RANKINGS: &str = "# Numeric rankings\nFor a minimum, maximum or top-N by one numeric field, set continues.comparison when all requested outputs are the winners' labels and values. Give order=lowest/highest, limit=N, and exact prefix/suffix delimiters for label and value inside each matching record's quote. The value includes its currency symbol. An empty prefix or suffix means the beginning or end of the quote. Each field must match once per record. Code will extract, sort and draft the answer once every page is read. Still list every matching record, applying the task's filters; never select only page winners. Related requirements for the same winners use the same comparison and records. Leave comparison null for counts, calculations, extra output fields or ambiguous formats.\n\n";

const READER_PAGES: &str = "# Lists over several pages\nA count, total or superlative over a list needs the whole list. Earlier pages are in the collected evidence under their own URLs. When the list goes on past this capture and the collected evidence does not cover the rest, add an entry to continues naming the requirement. For counts, put matching records in its tallies as above. For comparisons of record values, give in its records every record this capture adds to that comparison, each as the blocks holding it and the value compared. Apply the requirement's filters on every page: include every matching record with its compared value and the attributes that show it matches; omit records those filters exclude. Name in expands the control on this page that shows the rest, when one is offered. If instead the page states that the list is ordered or filtered by the very quantity being compared, the leading record answers: give the claim citing it its requirement id and cite that statement in orders_list rather than listing the requirement in continues. Once the collected evidence and this capture cover every page, the tally uses complete=true; a comparison's conclusion takes the requirement id, includes this chunk's records and draws on earlier records once each. A task that bounds the pages it covers ends at the last page it names.\n\n";

// A read resent its whole instruction set on every chunk of every page. On short tasks that fixed text was 80 to 88
// percent of the read's input, and most of it could not apply: tally and comparison guidance on a page holding no
// records, paging guidance where the page offers no pager. Which sections ship is decided by the page and the plan,
// never by the task's wording.
/// The reader's instructions for this capture, carrying only the sections its records and controls can use.
fn reader_system(
    capture: &Capture,
    notice: &str,
    continuing: &[String],
    counting: &IndexMap<String, String>,
) -> String {
    let records = capture.blocks.iter().any(|block| counted_kind(block.kind));
    let mut text = READER_BASE.to_string();
    if records || !counting.is_empty() {
        text.push_str(READER_TALLIES);
        text.push_str(READER_RANKINGS);
    }
    if records || !notice.is_empty() || !continuing.is_empty() {
        text.push_str(READER_PAGES);
    }
    text.push_str(&format!("\n\n# Trust\n{UNTRUSTED} Never infer facts the capture and evidence do not show."));
    text
}

const COLLECTION_EVIDENCE: &str = "\n\n# Collection evidence\nFor a collection summary, retain every inspected member and the collection's identifying boundary or total in records, including identities, roles and reply text. Account for every requested category. Report a bounded absence explicitly only when all members have been read and none matches. Selected excerpts, missing text and unread sections cannot establish absence. Use continues.records while more members or pages remain unread. When separate facts together answer one requirement, add a derived conclusion with that requirement id and draws_on for every supporting claim or prior note. A claim covering only some requested fields must leave requirement_id null. A requested link needs its quoted destination; navigation labels without destinations do not answer it. A written hostname or hostname/path is a destination without a URL scheme; preserve it separately from labels whose destinations are missing. Leave that field open when its destination has not been read.";

const FIELD_COLLECTIONS: &str = " Also fill collections independently of claims: quote the boundary and every member of each inspected collection, even when another section or field is still missing. Code records completeness from the page's stated total and captured list members. A navigation link to a section is not its contents. If no member matches a requested field, include the entire collection and its boundary in collections, including excluded members. Prior summaries cannot establish that absence. ";

const PAGE_RECORDS: &str = "# Page records\nCollect the records on this page for the requested requirements. Return each requirement in continues even when no records match. For counts of matching records, including filtered counts, or rankings by count, use continues.tallies, grouped by the label stated in each record (key=null for an ungrouped count). Select only matching records. If excluded records separate matches, list each matching block range explicitly in the group's records. A page with no matches contributes an empty group. For comparisons of record values, use continues.records, including the compared value and the attributes that show each record matches. Prefer a tally field range for complete record blocks with the grouping label between the same exact prefix and suffix: key=null, records=[], field={span, prefix, suffix}. Delimiters must occur once per record; include the newline for a field starting a line. Every block in that range must be a matching record. When using tallies, leave the continuation's records empty. Omit records excluded by the task's filters, but never select only the page's winners. Earlier pages are being read separately. Use context for source block ranges needed to interpret these records, such as filters or table headers. If the task asks for a bounded page scope and this capture completes that scope, list its requirement id in requested_scope_complete even when another pager remains. Do not conclude comparisons.\n\n# Trust\n";

const FIELD_REPAIR: &str = "Your tally field range could not be resolved into complete matching records. Read the same capture again. Use explicit record block ranges when the field's delimiters or range cannot be verified. The [source id] and (block kind) annotations are not literal record text. A field range must contain only complete record blocks, with key=null and records=[]. Retain every matching record and keep the requirement open unless the evidence covers the entire requested list.";

async fn ask_reader<T: serde::de::DeserializeOwned>(
    llm: &dyn LLMClient,
    messages: &[Message],
    ty: Ty,
    tokens: &TokenBudget,
    ledger: Option<&Ledger>,
) -> Result<fastbrowse_core::llm::Generation<T>> {
    let schema = ModelSchema { ty };
    let generated = llm.generate_value(LLMPurpose::Read, messages, &schema, tokens.read_output_tokens, ledger).await?;
    let data = serde_json::from_value(generated.data).map_err(|error| AgentError::Invalid(error.to_string()))?;
    Ok(fastbrowse_core::llm::Generation { data, cost: generated.cost })
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
    let mut coverage: Vec<i64> = Vec::new();
    let mut costs: Vec<CostLine> = Vec::new();
    let mut continues: IndexSet<String> = IndexSet::new();
    let mut lost: IndexSet<String> = IndexSet::new();
    let mut requested_scope_complete: Vec<String> = Vec::new();
    let mut found: Vec<Fact> = Vec::new();
    let mut collections: Vec<Fact> = Vec::new();
    let mut rejected = 0;
    let mut uncovered = 0;
    let mut ordered: IndexSet<String> = IndexSet::new();
    let mut unconfirmed_orders: IndexSet<String> = IndexSet::new();
    let mut stated_counts: HashSet<(String, String)> = HashSet::new();
    let mut expands: Option<String> = None;
    let mut tally_complete: IndexSet<String> = IndexSet::new();
    let mut through_end: Vec<String> = Vec::new();
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
    // Record extraction emits dense structured output; larger prose captures must not overflow that response.
    let max_chars = options.max_chars.unwrap_or(if options.records_only || !counting.is_empty() {
        READ_CHUNK_CHARS
    } else {
        LLM_READ_CHUNK_CHARS
    });
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
    // A pager notice is a caveat on what this page can answer, and the choice model picks quotes without weighing one.
    if let Some(jev) = options.jev
        && !wanted.is_empty()
        && options.notice.is_empty()
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
            return Ok(ReadOutcome::new(facts.into_values().collect(), Vec::new(), rejected, costs));
        }
        // Narrow the obligations without dropping the task's constraints or separating ids from their meaning.
        let remaining: Vec<String> = options
            .requirements
            .iter()
            .filter(|r| requirement_ids.contains(&r.id))
            .map(|r| format!("- {}: {}", r.id, r.text))
            .collect();
        question.push_str(&format!("\n\nRead only these remaining requirements:\n{}", remaining.join("\n")));
    }
    if !options.notice.is_empty() {
        question.push_str(&format!("\n\n{}", options.notice));
    }
    // A list an earlier page said runs on is the requirement a reader most often drops on the next page, so each one is
    // named with the only two things that may happen to it.
    let open: Vec<String> = options
        .requirements
        .iter()
        .filter(|r| options.continuing.contains(&r.id) && requirement_ids.contains(&r.id))
        .map(|r| match notes.comparison_records(Some(&r.id)).len() {
            0 => format!("- {}: {}", r.id, r.text),
            1 => format!("- {}: {} (1 record collected so far)", r.id, r.text),
            n => format!("- {}: {} ({n} records collected so far)", r.id, r.text),
        })
        .collect();
    if !open.is_empty() {
        question.push_str(&format!(
            "\n\nOpen from earlier pages:\n{}\nFor each of these, either add it to continues with this capture's records when the list goes on past this capture, or conclude it with a claim that takes its requirement id, includes this chunk's records and draws on the earlier ones. Never leave one out.",
            open.join("\n")
        ));
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
    let parts = chunk_page(&page, max_chars, 1);
    let mut last: Option<(Chunk, bool)> = None;
    for part in parts {
        let mut system = reader_system(capture, &options.notice, &options.continuing, &counting);
        if options.preserve_collections {
            system.push_str(COLLECTION_EVIDENCE);
        }
        if options.field_outputs {
            system.push_str(FIELD_COLLECTIONS);
            system.push_str(FIELD_CATEGORIES);
        }
        if options.records_only {
            system = format!("{PAGE_RECORDS}{UNTRUSTED}");
        }
        let mut messages = vec![Message::system(system), read_message(&page, &part, &question, &requirement_ids, "")];
        let schema = if options.records_only { RecordsResponse::ty() } else { read_response_ty(collection_schema) };
        let room = notes_room(tokens, &messages, &schema);
        let labels: IndexMap<String, String> =
            so_far.facts().iter().enumerate().map(|(i, fact)| (fact_id(fact), format!("e{i}"))).collect();
        // Reading a new source does not require every earlier project's evidence in the same prompt. Comparisons that
        // need all records and final verdicts still refuse incomplete evidence.
        let render = RenderOptions {
            preserve_requirements: options.require_all_evidence,
            preserve_collections: options.preserve_collections,
            source_only: false,
            json_encoded: false,
            labels: Some(labels.clone()),
        };
        let offered_notes = match so_far.render_with_ids(room, &render) {
            Ok(rendered) => rendered,
            Err(AgentError::Invalid(_)) => {
                return Err(AgentError::NotesTooLarge(format!(
                    "The {room} character reader notes budget cannot report omitted evidence"
                )));
            }
            Err(other) => return Err(other),
        };
        // A refused fact is withheld from every prompt by design, so it is not a record this budget lost.
        if options.require_all_evidence {
            let offered_ids: HashSet<&String> = offered_notes.evidence_ids.iter().collect();
            let lost_any = so_far
                .facts()
                .iter()
                .map(|fact| fact_id(fact))
                .any(|key| !so_far.refused(&key) && !offered_ids.contains(&key));
            if lost_any {
                return Err(AgentError::NotesTooLarge(
                    "The final page cannot fit every earlier record in its collected evidence".into(),
                ));
            }
        }
        messages[1] = read_message(&page, &part, &question, &requirement_ids, &offered_notes.text);
        let mut result: fastbrowse_core::llm::Generation<ReadResponse>;
        if options.records_only {
            let collected = ask_reader::<RecordsResponse>(llm, &messages, schema, tokens, ledger).await?;
            ended = collected.data.ended.iter().filter(|key| requirement_ids.contains(key)).cloned().collect();
            requested_scope_complete = collected
                .data
                .requested_scope_complete
                .iter()
                .filter(|key| requirement_ids.contains(key))
                .cloned()
                .collect();
            let mut context = Vec::new();
            for cite in &collected.data.context {
                match cited(&page, &part, cite) {
                    None => {
                        lost.extend(requirement_ids.iter().cloned());
                        uncovered += 1;
                    }
                    Some(span) => context.push(ReadClaim {
                        cite: Some(cite.clone()),
                        excerpt: None,
                        draws_on: Vec::new(),
                        records: Vec::new(),
                        orders_list: None,
                        text: span.quote,
                        requirement_id: None,
                    }),
                }
            }
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
                            through_end: c.through_end,
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
            result = ask_reader::<ReadResponse>(llm, &messages, schema.clone(), tokens, ledger).await?;
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
                result = ask_reader::<ReadResponse>(llm, &repaired, schema, tokens, ledger).await?;
            }
        }
        if let Some(ledger) = ledger {
            ledger.record([result.cost.clone()])?;
        }
        costs.push(result.cost.clone());
        coverage.push(part.index);
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
        through_end = carried.iter().filter(|c| c.through_end).map(|c| c.requirement_id.clone()).collect();
        comparisons = carried
            .iter()
            .filter(|c| c.through_end && !counting.contains_key(&c.requirement_id))
            .filter_map(|c| c.comparison.clone().map(|comparison| (c.requirement_id.clone(), comparison)))
            .collect();
        expands = carried.iter().find_map(|c| c.expands.clone().filter(|label| !label.is_empty()));
        for continuation in &carried {
            if !continuation.reuse_field || !continuation.through_end || continuation.tallies.len() != 1 {
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
                    examples: Vec::new(),
                    comparison: None,
                };
                if read_tallies(capture, std::slice::from_ref(&reader), std::slice::from_ref(&reader.requirement_id))?
                    .is_some()
                {
                    tally_readers.push(reader);
                }
            }
        }
        let texts: IndexMap<&str, &str> =
            options.requirements.iter().map(|r| (r.id.as_str(), r.text.as_str())).collect();
        for continuation in &carried {
            // A filtered count or a comparison names its matches record by record, so a later page of the same list
            // is matched by Jev record by record rather than read again by the LLM. Code counts the matches, or picks
            // the winner from their values with the comparison's delimiters.
            let compares = continuation.comparison.is_some() && !counting.contains_key(&continuation.requirement_id);
            let Some(requirement_text) = texts.get(continuation.requirement_id.as_str()).copied() else { continue };
            if continuation.reuse_field
                || !continuation.through_end
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
                examples: quotes.iter().take(3).map(|evidence| evidence.quote.clone()).collect(),
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
                let mut missing = cites.len().saturating_sub(MAX_CONTINUING_RECORDS) as i64;
                for cite in cites.iter().take(MAX_CONTINUING_RECORDS) {
                    let span = cited(&page, &part, cite);
                    let matches_key = |span: &Evidence| match &group.key {
                        None => true,
                        Some(key) => !text::strip(key).is_empty() && loose_search(key, &span.quote),
                    };
                    let Some(mut span) = span.filter(|span| matches_key(span)) else {
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
            let mut missing = claim.records.len().saturating_sub(MAX_CONTINUING_RECORDS) as i64;
            for cite in claim.records.iter().take(MAX_CONTINUING_RECORDS) {
                match cited(&page, &part, cite) {
                    None => missing += 1,
                    Some(span) => {
                        let record = quoted(span);
                        records.insert(fact_id(&record), record);
                    }
                }
            }
            if missing > 0 {
                // A comparison missing an operand cannot close here or on a later page using these notes.
                match &claim.requirement_id {
                    Some(requirement_id) if requirement_ids.contains(requirement_id) => {
                        lost.insert(requirement_id.clone());
                    }
                    _ => lost.extend(requirement_ids.iter().cloned()),
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
            let Some(mut fact) =
                remember(&page, &part, &unassigned, &mut so_far, &references, options.preserve_collections)?
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
            let mut requirement_id = claim.requirement_id.clone().filter(|id| requirement_ids.contains(id));
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
            let stated_count = requirement_id.as_ref().is_some_and(|id| counting.contains_key(id))
                && quoted_count(&fact, &so_far, &records, &page);
            if stated_count && let Some(id) = &requirement_id {
                stated_counts.insert((id.clone(), fact_id(&fact)));
            }
            if let Some(id) = &requirement_id
                && notes.tallies().iter().any(|t| &t.requirement_id == id)
                && !stated_count
            {
                // A derived total needs complete tallies. A total the page states has its own quote to verify.
                requirement_id = None;
            }
            // A site that sorts or filters its own list by the quantity compared settles the superlative on its leading
            // record: the rest of the list cannot beat it. The page has to say so, in its own text, and that statement
            // is kept as a fact so the claim rests on it and the claim check can judge it. Only a claim resting on that
            // one record qualifies: a count or total draws on every record it counts, and the part of a list one page
            // shows does not settle it however the site sorts it.
            if let Some(id) = requirement_id.clone()
                && !options.skip_order_shortcuts.contains(&id)
                && let Some(orders_list) = &claim.orders_list
                && claim.draws_on.is_empty()
            {
                let ordering = cited(&page, &part, orders_list);
                let mut confirmed = false;
                if let Some(ordering) = &ordering {
                    // Offering a sort link does not establish which order is active on the current list.
                    let messages = [
                        Message::system(format!(
                            "{UNTRUSTED} Confirm only when the quoted source explicitly states the current list is ordered or filtered by the quantity and direction needed for the requested superlative. Available sort options, navigation links, a URL and the reader's assertion do not establish active ordering. Do not infer sorting from the order of the shown records. Return false when uncertain."
                        )),
                        Message::user(dumps(&json!({"question": question, "quote": ordering.quote}))),
                    ];
                    let assessed =
                        generate::<ListOrder>(llm, LLMPurpose::Read, &messages, tokens.read_output_tokens, ledger)
                            .await?;
                    if let Some(ledger) = ledger {
                        ledger.record([assessed.cost.clone()])?;
                    }
                    costs.push(assessed.cost.clone());
                    confirmed = assessed.data.confirmed;
                }
                match ordering.filter(|_| confirmed) {
                    None => {
                        unconfirmed_orders.insert(id);
                        requirement_id = None;
                    }
                    Some(ordering) => {
                        let stated = quoted(ordering);
                        fact.basis.push(fact_id(&stated));
                        so_far.add(stated.clone());
                        found.push(stated);
                        ordered.insert(id);
                    }
                }
            }
            if let Some(id) = requirement_id.clone()
                && (options.continuing.contains(&id) || !so_far.comparison_records(Some(&id)).is_empty())
                && !options.notice.is_empty()
                && !ordered.contains(&id)
                && !options.skip_order_shortcuts.contains(&id)
            {
                // A reader can omit orders_list while still claiming a winner on a continuing list.
                unconfirmed_orders.insert(id);
                requirement_id = None;
            }
            found.push(Fact { requirement_id, ..fact });
            accepted += 1;
        }
        // The records a continuing page compared, kept as facts so a later page's winner can show what it beat. Code
        // copies each quote from the blocks named, exactly as it does for a claim, so a record is the page's own text and
        // never the reader's retyping of it.
        for continuation in &carried {
            // Records past the cap are not kept, but one an accepted claim already holds is not lost.
            let held: HashSet<String> = found.iter().map(fact_id).chain(so_far.evidence().into_keys()).collect();
            let mut missing = 0;
            for cite in continuation.records.iter().skip(MAX_CONTINUING_RECORDS) {
                if cited(&page, &part, cite).is_none_or(|span| !held.contains(&evidence_id(&span))) {
                    missing += 1;
                }
            }
            for cite in continuation.records.iter().take(MAX_CONTINUING_RECORDS) {
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
            && options.notice.is_empty()
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
    // A lost record leaves later comparisons incomplete too; the page's own stated order can still settle a winner.
    for key in &ordered {
        continues.shift_remove(key);
    }
    for requirement_id in &tally_complete {
        // Earlier continuation quotes have no tally key, so counting only later pages would omit them.
        if notes.has_untallied_records(requirement_id) {
            lost.insert(requirement_id.clone());
        }
    }
    let mut blocked: HashSet<String> =
        options.incomplete.iter().cloned().chain(lost.iter().cloned()).filter(|key| !ordered.contains(key)).collect();
    if options.records_only {
        blocked.extend(requirement_ids.iter().cloned());
    }
    for mut fact in found {
        let key = fact_id(&fact);
        // Missing records invalidate a derived tally, but a separately quoted total does not depend on them.
        let stated_count = fact.requirement_id.as_ref().is_some_and(|id| {
            stated_counts.contains(&(id.clone(), key.clone())) && !lost.contains(id) && !options.records_only
        });
        if fact
            .requirement_id
            .as_ref()
            .is_some_and(|id| continues.contains(id) || (blocked.contains(id) && !stated_count))
        {
            fact.requirement_id = None;
        }
        if let Some(comparison) = &mut fact.comparison {
            // A pager the caller knows of outranks the reader's belief that the list ended, and another requirement
            // left open says nothing about this one's collection.
            comparison.complete = fact.requirement_id.as_ref().is_some_and(|id| {
                (!options.field_outputs || options.require_all_evidence || ordered.contains(id))
                    && (answered || requirement_ids.len() > 1)
                    && part.index == part.total - 1
                    && (options.notice.is_empty() || ordered.contains(id))
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
            && (!options.notice.is_empty()
                || continues.contains(&comparison.requirement_id)
                || blocked.contains(&comparison.requirement_id))
        {
            comparison.complete = false;
        }
        notes.add(collection.clone());
        facts.insert((fact_id(&collection), None), collection);
    }
    let collection_facts = if options.notice.is_empty() {
        let open: Vec<String> = requirement_ids.iter().filter(|key| !blocked.contains(*key)).cloned().collect();
        counted_collections(&page, &open, part.end, &options.requirements, options.targets.as_deref())
    } else {
        Vec::new()
    };
    if options.field_outputs {
        for fact in &collection_facts {
            notes.add(fact.clone());
            facts.insert((fact_id(fact), None), fact.clone());
        }
    }
    let mut outcome = ReadOutcome {
        facts: facts.into_values().collect(),
        collection_facts,
        coverage,
        rejected_claims: rejected,
        cost_lines: costs,
        continues: continues.iter().cloned().collect(),
        incomplete: lost.iter().cloned().collect(),
        uncovered,
        expands: if continues.is_empty() { None } else { expands },
        through_end: through_end.into_iter().filter(|key| continues.contains(key)).collect(),
        continuation_records: continuation_records.into_iter().map(|(key, records)| (key, unique(records))).collect(),
        ended: ended.into_iter().filter(|key| !lost.contains(key)).collect(),
        requested_scope_complete: requested_scope_complete.into_iter().filter(|key| !lost.contains(key)).collect(),
        tally_readers: tally_readers.into_iter().filter(|reader| !lost.contains(&reader.requirement_id)).collect(),
        comparisons: comparisons.into_iter().filter(|(key, _)| !lost.contains(key)).collect(),
    };
    unconfirmed_orders.retain(|key| !ordered.contains(key));
    if !unconfirmed_orders.is_empty() {
        // A rejected sorting shortcut has uncollected records, not permanently missing records.
        let mut pending: Vec<String> = unconfirmed_orders.iter().cloned().collect();
        pending.sort();
        let targets = Some(options.targets.clone().unwrap_or_else(|| options.requirements.clone()));
        let collecting = ReadOptions {
            max_chars: Some(max_chars.min(READ_CHUNK_CHARS)),
            tokens: tokens.clone(),
            ledger,
            jev: None,
            requirements: options.requirements.iter().filter(|r| unconfirmed_orders.contains(&r.id)).cloned().collect(),
            targets: targets.clone(),
            records_only: true,
            preserve_collections: options.preserve_collections,
            trace: options.trace.clone(),
            ..ReadOptions::default()
        };
        let collected = read(llm, capture, &question, &pending, notes, &collecting).await?;
        let scope_ids: Vec<String> = collected
            .requested_scope_complete
            .iter()
            .filter(|key| unconfirmed_orders.contains(*key) && !collected.incomplete.contains(key))
            .cloned()
            .collect();
        let scoped = if scope_ids.is_empty() {
            None
        } else {
            let scoping = ReadOptions {
                max_chars: Some(max_chars.min(READ_CHUNK_CHARS)),
                tokens: tokens.clone(),
                ledger,
                jev: None,
                requirements: options.requirements.iter().filter(|r| scope_ids.contains(&r.id)).cloned().collect(),
                targets,
                require_all_evidence: true,
                skip_order_shortcuts: scope_ids.clone(),
                preserve_collections: options.preserve_collections,
                trace: options.trace.clone(),
                ..ReadOptions::default()
            };
            Some(read(llm, capture, &question, &scope_ids, notes, &scoping).await?)
        };
        outcome = merge_outcomes(outcome, collected, scoped, &unconfirmed_orders, &scope_ids);
    }
    Ok(outcome)
}

fn merge_outcomes(
    outcome: ReadOutcome,
    collected: ReadOutcome,
    scoped: Option<ReadOutcome>,
    unconfirmed: &IndexSet<String>,
    scope_ids: &[String],
) -> ReadOutcome {
    let scoped_ref = scoped.as_ref();
    let all = |pick: fn(&ReadOutcome) -> &Vec<String>| -> Vec<String> {
        unique(pick(&outcome).iter().chain(pick(&collected)).chain(scoped_ref.map(pick).into_iter().flatten()).cloned())
    };
    let mut facts = outcome.facts.clone();
    facts.extend(collected.facts.iter().cloned());
    let mut cost_lines = outcome.cost_lines.clone();
    cost_lines.extend(collected.cost_lines.iter().cloned());
    let mut tally_readers = outcome.tally_readers.clone();
    tally_readers.extend(collected.tally_readers.iter().cloned());
    let mut comparisons = outcome.comparisons.clone();
    comparisons.extend(collected.comparisons.clone());
    let mut continuation_records = outcome.continuation_records.clone();
    continuation_records.extend(collected.continuation_records.clone());
    let mut rejected = outcome.rejected_claims + collected.rejected_claims;
    let mut uncovered = outcome.uncovered + collected.uncovered;
    if let Some(scoped) = scoped_ref {
        facts.extend(scoped.facts.iter().cloned());
        cost_lines.extend(scoped.cost_lines.iter().cloned());
        tally_readers.extend(scoped.tally_readers.iter().cloned());
        comparisons.extend(scoped.comparisons.clone());
        continuation_records.extend(scoped.continuation_records.clone());
        rejected += scoped.rejected_claims;
        uncovered += scoped.uncovered;
    }
    let continues: Vec<String> = all(|o| &o.continues)
        .into_iter()
        .filter(|key| !(scoped_ref.is_some_and(|s| scope_ids.contains(key) && !s.continues.contains(key))))
        .collect();
    let incomplete = unique(
        outcome
            .incomplete
            .iter()
            .filter(|key| !unconfirmed.contains(*key))
            .chain(&collected.incomplete)
            .chain(scoped_ref.map(|s| &s.incomplete).into_iter().flatten())
            .cloned(),
    );
    let expands = scoped_ref.and_then(|s| s.expands.clone()).or(collected.expands.clone()).or(outcome.expands.clone());
    ReadOutcome {
        facts,
        collection_facts: outcome.collection_facts.clone(),
        coverage: unique_ints(outcome.coverage.iter().chain(&collected.coverage).copied()),
        rejected_claims: rejected,
        cost_lines,
        continues,
        incomplete,
        uncovered,
        expands,
        through_end: all(|o| &o.through_end),
        continuation_records,
        ended: all(|o| &o.ended),
        requested_scope_complete: all(|o| &o.requested_scope_complete),
        tally_readers,
        comparisons,
    }
}

fn unique_ints(values: impl Iterator<Item = i64>) -> Vec<i64> {
    let mut seen = IndexSet::new();
    for value in values {
        seen.insert(value);
    }
    seen.into_iter().collect()
}
