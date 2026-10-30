//! Answers built from notes: the composer, the drafts code writes without one, and the claim checks Jev answers.

use std::collections::HashSet;

use fastbrowse_core::citations::text_fragment;
use fastbrowse_core::config::TokenBudget;
use fastbrowse_core::llm::{Generation, LLMClient, Message};
use fastbrowse_core::models::{Citation, LLMPurpose, UNTRUSTED, background_section};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::py::text;
use fastbrowse_core::schema::{Field, ModelTy, Schema, Ty};
use fastbrowse_core::telemetry::{Ledger, Trace};
use fastbrowse_core::validation::ModelSchema;
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::notes_room;
use super::reading::source_basis;
use crate::error::{AgentError, Result};
use crate::memory::{Notes, RenderOptions, fact_id, unique};
use crate::planner::{FIELD_CATEGORIES, Plan, Requirement, RequirementKind};
use crate::pyre::{Pat, is_digit, is_word};

const RANKS: &str = "True if it states a rank or order among counted groups.";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub text: String,
    pub evidence_ids: Vec<String>,
    /// Whether the checks are shown each cited tally's standing among the other groups. A count alone needs only its
    /// own tally, and a standing for every tally put hundreds of group labels into each claim question.
    #[serde(default)]
    pub ranks: bool,
}

impl Claim {
    pub fn new(text: &str, evidence_ids: Vec<String>) -> Claim {
        Claim { text: text.into(), evidence_ids, ranks: false }
    }
}

impl Schema for Claim {
    fn ty() -> Ty {
        ModelTy::frozen(
            "Claim",
            vec![
                Field::required("text", Ty::Str),
                Field::required("evidence_ids", Ty::list(Ty::Str)),
                Field::default("ranks", Ty::Bool, false).doc(RANKS),
            ],
        )
        .ty()
    }
}

/// `Literal["source", "assertion"]`: which check rejected a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Source,
    #[default]
    Assertion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerCorrection {
    #[serde(default)]
    pub stage: Stage,
    pub criterion: String,
    pub claims: Vec<Claim>,
    pub reason: String,
}

/// `ComposedAnswer`, and its subclass `FieldAnswer` when `output_claims` is set: each requested field keeps its own
/// claims through selection and fallback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposedAnswer {
    /// The claims as plain text: what Jev judges. A link's percent-encoded quote read to it as more evidence than the
    /// claim cited, so a draft with links was sent for rewriting and its one-quote claims were doubted.
    pub answer: String,
    /// The same claims, each followed by numbered Markdown links to its quotes: what the caller receives.
    pub linked_answer: String,
    pub claims: Vec<Claim>,
    #[serde(default)]
    pub citations: Vec<Citation>,
    #[serde(default)]
    pub dropped_claims: i64,
    /// Original obligations retained for the omission check, including unevidenced ones.
    #[serde(default)]
    pub requirements: Vec<Requirement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_claims: Option<IndexMap<String, Vec<usize>>>,
}

impl ComposedAnswer {
    /// `isinstance(answer, FieldAnswer)`.
    pub fn is_field_answer(&self) -> bool {
        self.output_claims.is_some()
    }

    /// `FieldAnswer(**self.model_dump(), output_claims=outputs)`.
    pub fn with_outputs(self, outputs: IndexMap<String, Vec<usize>>) -> ComposedAnswer {
        ComposedAnswer { output_claims: Some(outputs), ..self }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AnswerDraft {
    pub claims: Vec<Claim>,
}

impl Schema for AnswerDraft {
    fn ty() -> Ty {
        ModelTy::frozen("_AnswerDraft", vec![Field::required("claims", Ty::list(Claim::ty()))]).ty()
    }
}

/// `create_model("_FieldAnswerDraft", ...)`: one list of claims per requested field, described by its check.
pub(crate) fn field_answer_draft(fields: &[(usize, String)]) -> Ty {
    ModelTy::frozen(
        "_FieldAnswerDraft",
        fields
            .iter()
            .map(|(index, check)| Field::required(&format!("output_{index}"), Ty::list(Claim::ty())).doc(check))
            .collect(),
    )
    .ty()
}

/// `retain_fields`: fields the previous answer had verified keep their claims when the new answer left them empty.
pub fn retain_fields(answer: ComposedAnswer, previous: &ComposedAnswer, notes: &Notes) -> ComposedAnswer {
    let Some(previous_outputs) = &previous.output_claims else { return answer };
    let mut claims = answer.claims.clone();
    let mut outputs = answer.output_claims.clone().unwrap_or_default();
    for (check, indices) in previous_outputs {
        if outputs.get(check).is_some_and(|kept| !kept.is_empty()) {
            continue;
        }
        let mut kept = Vec::new();
        for index in indices {
            let claim = &previous.claims[*index];
            if !claims.contains(claim) {
                claims.push(claim.clone());
            }
            kept.push(claims.iter().position(|c| c == claim).unwrap_or_default());
        }
        outputs.insert(check.clone(), kept);
    }
    assemble_answer(&claims, notes, &answer.requirements, answer.dropped_claims).with_outputs(outputs)
}

/// `assemble_answer`: the claims with their numbered links and the citations they use.
pub fn assemble_answer(
    claims: &[Claim],
    notes: &Notes,
    requirements: &[Requirement],
    dropped_claims: i64,
) -> ComposedAnswer {
    // A claim keeps what it cited, which is what it states; the records its facts were derived from are shown with
    // it, so the caller and the claim check see what a total or winner was compared against.
    let supports: Vec<Vec<String>> =
        claims.iter().map(|claim| notes.expand_evidence_ids(claim.evidence_ids.iter().map(String::as_str))).collect();
    // A derived fact has no page to link; its basis records carry the citations.
    let known: IndexMap<String, Citation> = notes
        .facts()
        .into_iter()
        .filter_map(|fact| fact.evidence.as_ref().map(|evidence| (fact, evidence)))
        .enumerate()
        .map(|(index, (fact, evidence))| {
            // Python raises on a quote of only whitespace; a capture never cuts one, so the address stands in.
            let deep_link = if evidence.rendered_text {
                text_fragment(&evidence.url, &evidence.quote).unwrap_or_else(|_| evidence.url.clone())
            } else {
                evidence.url.clone()
            };
            let citation = Citation {
                id: index as i64 + 1,
                text: fact.text.clone(),
                requirement_id: fact.requirement_id.clone(),
                url: evidence.url.clone(),
                quote: evidence.quote.clone(),
                deep_link,
            };
            (fact_id(fact), citation)
        })
        .collect();
    let cited: HashSet<&String> = supports.iter().flatten().collect();
    let linked: Vec<String> = claims
        .iter()
        .zip(&supports)
        .map(|(claim, support)| {
            let links: Vec<String> = support
                .iter()
                .filter_map(|key| known.get(key))
                .map(|citation| format!("[{}](<{}>)", citation.id, citation.deep_link))
                .collect();
            format!("{} {}", claim.text, links.join(" "))
        })
        .collect();
    ComposedAnswer {
        answer: claims.iter().map(|claim| claim.text.as_str()).collect::<Vec<_>>().join("\n\n"),
        linked_answer: linked.join("\n\n"),
        claims: claims.to_vec(),
        citations: known.iter().filter(|(key, _)| cited.contains(key)).map(|(_, citation)| citation.clone()).collect(),
        dropped_claims,
        requirements: requirements.to_vec(),
        output_claims: None,
    }
}

fn citation_label(label: &[char]) -> bool {
    if !label.is_empty() && label.iter().all(|c| is_digit(*c)) {
        return true;
    }
    let hex = || Pat::class(|c: char| c.is_ascii_digit() || ('a'..='f').contains(&c));
    let digits = || Pat::plus(Pat::class(is_digit));
    // r"[\w-]+:\d+:\d+(?::[0-9a-f]{16})?|(?:derived|tally):[0-9a-f]+|e\d+"
    Pat::alt(vec![
        Pat::Seq(vec![
            Pat::plus(Pat::class(|c| is_word(c) || c == '-')),
            Pat::lit(":"),
            digits(),
            Pat::lit(":"),
            digits(),
            Pat::opt(Pat::Seq(vec![Pat::lit(":"), Pat::repeat(hex(), 16, 16)])),
        ]),
        Pat::Seq(vec![Pat::alt(vec![Pat::lit("derived"), Pat::lit("tally")]), Pat::lit(":"), Pat::plus(hex())]),
        Pat::Seq(vec![Pat::lit("e"), digits()]),
    ])
    .full_match(label)
}

/// `_without_citation_markup`: the composer can echo bracketed references in prose; only its checked evidence_ids
/// create links. `re.sub(r"\[([^\]\n]+)\](\([^\n)]*\)|\[[^\]\n]*\])?", ...)`.
pub(crate) fn without_citation_markup(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let close = |from: usize, stop: &[char], end: char| {
        let mut at = from;
        while at < chars.len() && !stop.contains(&chars[at]) {
            at += 1;
        }
        (chars.get(at) == Some(&end)).then_some(at)
    };
    let mut out = String::new();
    let mut at = 0;
    while at < chars.len() {
        if chars[at] == '['
            && let Some(label_end) = close(at + 1, &[']', '\n'], ']').filter(|end| *end > at + 1)
        {
            let label = &chars[at + 1..label_end];
            let after = label_end + 1;
            let group_end = match chars.get(after) {
                Some('(') => close(after + 1, &['\n', ')'], ')').map(|end| end + 1),
                Some('[') => close(after + 1, &[']', '\n'], ']').map(|end| end + 1),
                _ => None,
            };
            let end = group_end.unwrap_or(after);
            if citation_label(label) {
                // Python logs the dropped reference as a warning; the label is gone either way.
            } else if group_end.is_some() {
                out.extend(label);
            } else {
                out.extend(&chars[at..end]);
            }
            at = end;
            continue;
        }
        out.push(chars[at]);
        at += 1;
    }
    text::strip(&out).to_string()
}

/// `compose`'s keyword arguments.
#[derive(Default)]
pub struct ComposeOptions<'a> {
    pub tokens: TokenBudget,
    pub ledger: Option<&'a Ledger>,
    pub transaction_evidence_ids: Vec<String>,
    pub corrections: Vec<AnswerCorrection>,
    pub preserve_collections: bool,
    pub field_outputs: bool,
    pub verified: Option<ComposedAnswer>,
    /// Defaults to true, as Python's keyword does; see [`ComposeOptions::new`].
    pub independent_fields: bool,
    /// The caller's background, shown after the task as context and never as an output to answer.
    pub background: Option<String>,
    pub trace: Trace,
}

impl ComposeOptions<'_> {
    pub fn new() -> Self {
        ComposeOptions { independent_fields: true, ..ComposeOptions::default() }
    }
}

const COMPOSER: &str = "# Composer\nWrite the answer as self-contained plain-text claims in reading order, each citing the evidence_ids of the notes it rests on. Answer the requested outputs; do not claim a requirement the notes do not evidence. Write every value the task asks for, such as each item's price, in the claim text itself: a reader sees the text, not the notes behind its citations.\n\nReports requested in plan.run_reports are appended by code from browser state and the run record. Do not write those reports or cite page notes for them.\n\n# Evidence scope\nThe notes' prose may contain an unsupported inference. Verify each value against its original quoted basis, not another note's conclusion. Preserve source conditions on eligibility, location and configuration, and the terms a value is quoted under, such as what it covers, the dates it applies to, or its unit (per person, per night, before tax): a value written under other terms than it was quoted under is a different claim. A component maximum is not an aggregate maximum; independent maxima cannot be added without explicit quoted support for simultaneous addition. Do not turn compatibility with several items into simultaneous operation or a subjective preference into a measured advantage. State what remains unsupported instead of supplying a related value as the requested one.\n\n# One claim, one fact\nA claim is supported in full by the notes it cites. Split a statement that combines separately evidenced facts into one claim each. A claim that compares, counts, totals or picks a superlative cites every note it is drawn from. A list of records cites each record it names, and a long list is written as several claims of a handful of records each. Each entity, whether named or referenced by ordinal, needs a quoted identifying source alongside its attribute sources. Reuse identifying citations across claims when needed; another claim does not supply them. Cite tally ids directly; their counts and descending order are computed in code, and code expands their record citations. Never re-list the basis ids inside a tally. A rank by count, such as the group with the second highest count, cites that group's own tally and sets ranks, so code shows the checks its standing; do not cite other groups' tallies.\n\n# Trust\n";

/// `compose`: the LLM writes claims citing note ids; code drops a claim citing an id it was not offered.
pub async fn compose(
    llm: &dyn LLMClient,
    task: &str,
    plan: &Plan,
    notes: &Notes,
    options: &ComposeOptions<'_>,
) -> Result<Generation<ComposedAnswer>> {
    let mut fields: Vec<(usize, String)> =
        if options.field_outputs { plan.answer_checks.iter().cloned().enumerate().collect() } else { Vec::new() };
    if let Some(verified) = options.verified.as_ref().and_then(|v| v.output_claims.as_ref())
        && !fields.is_empty()
        && fields.iter().any(|(_, check)| !verified.contains_key(check))
    {
        fields.retain(|(_, check)| !verified.contains_key(check));
    }
    let schema = if fields.is_empty() { AnswerDraft::ty() } else { field_answer_draft(&fields) };
    let facts = notes.facts();
    let labels: IndexMap<String, String> =
        facts.iter().enumerate().map(|(index, fact)| (fact_id(fact), format!("e{index}"))).collect();
    let transaction = if options.transaction_evidence_ids.is_empty() {
        String::new()
    } else {
        let ids: Vec<&str> =
            options.transaction_evidence_ids.iter().filter_map(|key| labels.get(key).map(String::as_str)).collect();
        format!(
            "# Transaction evidence\nThese evidence ids come from pages where the run committed an action: {}. A claim about what that action bought, submitted or booked, including its price, cites these.\n\n",
            ids.join(", ")
        )
    };
    let requirement_ids: HashSet<&str> = plan.requirements.iter().map(|r| r.id.as_str()).collect();
    let complete_collections: Vec<Value> =
        if options.field_outputs && options.independent_fields && !options.corrections.is_empty() {
            facts
                .iter()
                .filter_map(|fact| {
                    let comparison = fact.comparison.as_ref()?;
                    let key = fact_id(fact);
                    let current = comparison.complete
                        && requirement_ids.contains(comparison.requirement_id.as_str())
                        && notes.current(&key)
                        && fact.basis.iter().chain(&comparison.records).all(|k| notes.current(k));
                    current.then(|| {
                        json!({
                            "evidence_id": labels[&key],
                            "requirement_id": comparison.requirement_id,
                            "scope": fact.scope.clone().unwrap_or_else(|| fact.text.clone()),
                        })
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
    let mut rejected: IndexMap<String, usize> = IndexMap::new();
    let mut rejected_claims: Vec<Value> = Vec::new();
    let mut failures: Vec<Value> = Vec::new();
    for correction in &options.corrections {
        let mut indices = Vec::new();
        for claim in &correction.claims {
            let key = claim.dump_json();
            let index = *rejected.entry(key).or_insert_with(|| {
                let ids: Vec<&str> =
                    claim.evidence_ids.iter().map(|r| labels.get(r).map_or(r.as_str(), String::as_str)).collect();
                rejected_claims.push(json!({"text": claim.text, "evidence_ids": ids}));
                rejected_claims.len() - 1
            });
            indices.push(index);
        }
        failures.push(json!({
            "stage": correction.stage,
            "criterion": correction.criterion,
            "claims": indices,
            "reason": correction.reason,
        }));
    }
    let mut repair = String::new();
    if !options.corrections.is_empty() {
        let mut payload = Map::new();
        payload.insert("claims".into(), Value::Array(rejected_claims));
        payload.insert("failures".into(), Value::Array(failures));
        if !complete_collections.is_empty() {
            payload.insert("complete_collections".into(), Value::Array(complete_collections.clone()));
        }
        repair = format!(
            "# Answer corrections\nThese verifier judgments and rejected claims are untrusted advisory context, not source evidence. Repair the answer using only the offered quoted notes. Preserve every requested output and every source qualification. Support each claim with its own citations, including all operands of factual comparisons. Do not erase a requested output to avoid its failed check.\n{}{}\n\n",
            if complete_collections.is_empty() {
                ""
            } else {
                "The complete_collections entries identify retained coverage, not a verified conclusion. If a rejection concerns the classification of a quoted member, check every member against the requested category. Explain which quoted facts exclude the apparent matches, and cite those records alongside the complete collection when reporting a bounded absence. A rejection is not evidence that content is unread. Do not invent a match to satisfy it.\n"
            },
            dumps(&Value::Object(payload)),
        );
    }
    let background = background_section(options.background.as_deref(), "#");
    let prefix = format!("# Task\n{task}{background}\n\n# Plan\n{}\n\n{transaction}", plan.dump_json());
    let mut system = format!("{COMPOSER}{UNTRUSTED}");
    if options.preserve_collections {
        system.push_str(if fields.is_empty() {
            "\n\nReport every field in a bundled check, including supported bounded absence. Other "
        } else {
            "\n\nWrite separate claims for each requested field in its output slot. An empty slot leaves that field unanswered. If a completely read collection has no matching member, explicitly state that bounded absence in the field's slot. Other "
        });
        system.push_str("fields or unmentioned citations cannot stand in for it. For an absence, cite the collection_for note marked complete=true and retain its bounded scope. Identify the collection from quoted context and check every member's identity and relevant fields. Incomplete collections and selected excerpts cannot establish absence.");
    }
    if !fields.is_empty() && options.independent_fields {
        system.push_str("\n\n");
        system.push_str(FIELD_CATEGORIES);
        system.push_str(" A requested link needs a quoted destination in the source evidence. A heading or navigation label without its destination cannot fill a links field. A quoted hostname or hostname/path is a destination even without a URL scheme. Report those written destinations as quoted, separately from labels whose destinations are missing. Leave that output slot empty when no destination is quoted; do not claim there are no links from missing text.");
    }
    let mut messages = vec![Message::system(system), Message::user(format!("{prefix}{repair}# Notes\n"))];
    let render = RenderOptions {
        preserve_requirements: true,
        preserve_collections: options.preserve_collections,
        source_only: options.field_outputs && options.independent_fields,
        json_encoded: false,
        labels: Some(labels.clone()),
    };
    let room = notes_room(&options.tokens, &messages, &schema);
    let offered = match notes.render_with_ids(room, &render) {
        Ok(offered) => offered,
        Err(AgentError::NotesTooLarge(message)) => {
            if repair.is_empty() || !complete_collections.is_empty() {
                return Err(AgentError::NotesTooLarge(message));
            }
            // Advisory corrections must not crowd out the source quotes needed to write a grounded answer.
            messages[1].content = format!("{prefix}# Notes\n");
            let room = notes_room(&options.tokens, &messages, &schema);
            notes.render_with_ids(room, &render)?
        }
        Err(other) => return Err(other),
    };
    messages[1].content.push_str(&offered.text);
    let response = ModelSchema { ty: schema };
    let result = llm.generate_value(LLMPurpose::Compose, &messages, &response, options.ledger).await?;
    if let Some(ledger) = options.ledger {
        ledger.record([result.cost.clone()])?;
    }
    let known: IndexSet<&String> = offered.evidence_ids.iter().collect();
    let mut references: IndexMap<String, String> =
        known.iter().map(|key| (labels.get(*key).cloned().unwrap_or_default(), (*key).clone())).collect();
    references.extend(known.iter().map(|key| ((*key).clone(), (*key).clone())));
    let parse = |value: &Value| -> Result<Vec<Claim>> {
        serde_json::from_value(value.clone()).map_err(|error| AgentError::Invalid(error.to_string()))
    };
    let drafts: Vec<(Option<String>, Claim)> = if fields.is_empty() {
        parse(&result.data["claims"])?.into_iter().map(|claim| (None, claim)).collect()
    } else {
        let mut drafts = Vec::new();
        for (index, check) in &fields {
            for claim in parse(&result.data[format!("output_{index}")])? {
                drafts.push((Some(check.clone()), claim));
            }
        }
        drafts
    };
    let mut outputs: IndexMap<String, Vec<usize>> =
        fields.iter().map(|(_, check)| (check.clone(), Vec::new())).collect();
    let mut claims: Vec<Claim> = Vec::new();
    let all_facts = notes.facts();
    for (check, claim) in &drafts {
        let unknown: Vec<&String> = claim.evidence_ids.iter().filter(|key| !references.contains_key(*key)).collect();
        if claim.evidence_ids.is_empty() || !unknown.is_empty() {
            options.trace.event("dropped_claim", || {
                let mut fields = Map::new();
                fields.insert("claim".into(), claim.text.as_str().into());
                fields.insert("unknown".into(), json!(unknown));
                fields
            });
            continue;
        }
        let mut keys: Vec<String> = claim.evidence_ids.iter().map(|key| references[key].clone()).collect();
        if options.preserve_collections {
            keys = unique(keys.iter().flat_map(|key| source_basis(notes, key)));
            let selected: HashSet<String> =
                notes.expand_evidence_ids(keys.iter().map(String::as_str)).into_iter().collect();
            // A composer can cite every record directly and omit the collection's completeness metadata.
            let collections: Vec<String> = all_facts
                .iter()
                .filter(|fact| {
                    let key = fact_id(fact);
                    fact.comparison.as_ref().is_some_and(|comparison| {
                        !comparison.records.is_empty() && comparison.records.iter().all(|r| selected.contains(r))
                    }) && known.contains(&key)
                        && notes.expand_evidence_ids([key.as_str()]).iter().all(|k| notes.current(k))
                })
                .map(|fact| fact_id(fact))
                .collect();
            keys = unique(keys.into_iter().chain(collections));
        }
        claims.push(Claim { text: without_citation_markup(&claim.text), evidence_ids: keys, ranks: claim.ranks });
        if let Some(check) = check {
            let index = claims.len() - 1;
            outputs.entry(check.clone()).or_default().push(index);
        }
    }
    let dropped = (drafts.len() - claims.len()) as i64;
    let mut answer = assemble_answer(&claims, notes, &plan.requirements, dropped);
    if !fields.is_empty() {
        answer = answer.with_outputs(outputs);
        if let Some(verified) = &options.verified
            && verified.is_field_answer()
        {
            answer = retain_fields(answer, verified, notes);
        }
    }
    Ok(Generation { data: answer, cost: result.cost })
}

const PARTIAL_NOTICE: &str = "Partial evidence only. Remaining coverage is omitted; totals may be incomplete.";

/// `partial_answer`: everything a stopped run collected, without another model call.
pub fn partial_answer(notes: &Notes) -> ComposedAnswer {
    let facts = notes.facts();
    let mut claims: Vec<Claim> = Vec::new();
    let mut counted: HashSet<String> = HashSet::new();
    // Every count comes before any quote, highest first. Tallies have no source address, so sharing one turn listed
    // only the highest count; and a stopped run's counts are incomplete, so ordering by what evidences a requirement
    // put a quote answering something else first and left no room for any count.
    let mut tallies: Vec<&&crate::memory::Fact> = facts.iter().filter(|fact| fact.tally.is_some()).collect();
    tallies.sort_by(|a, b| {
        let key = |fact: &crate::memory::Fact| {
            let tally = fact.tally.as_ref().expect("filtered to tallies");
            (std::cmp::Reverse(tally.count()), tally.requirement_id.clone(), tally.key.clone())
        };
        key(a).cmp(&key(b))
    });
    let mut quoted: Vec<&&crate::memory::Fact> = facts.iter().filter(|fact| fact.evidence.is_some()).collect();
    quoted.sort_by_key(|fact| notes.fact_requirements(&fact_id(fact)).is_empty());
    let mut ranks: IndexMap<Option<String>, usize> = IndexMap::new();
    let mut ranked: Vec<(usize, &&crate::memory::Fact)> = Vec::new();
    // Early sources can fill the partial answer before a later target's finding gets a turn.
    for fact in quoted {
        let source = fact.evidence.as_ref().map(|evidence| evidence.url.clone());
        let rank = ranks.get(&source).copied().unwrap_or(0);
        ranked.push((rank, fact));
        ranks.insert(source, rank + 1);
    }
    ranked.sort_by_key(|(rank, _)| *rank);
    for fact in tallies.into_iter().chain(ranked.into_iter().map(|(_, fact)| fact)) {
        let key = fact_id(fact);
        if counted.contains(&key) {
            continue;
        }
        let claim_text = if fact.tally.is_some() {
            fact.text.clone()
        } else {
            fact.evidence.as_ref().map(|evidence| evidence.quote.clone()).unwrap_or_default()
        };
        let claim = Claim::new(&claim_text, vec![key]);
        counted.extend(notes.expand_evidence_ids(claim.evidence_ids.iter().map(String::as_str)));
        claims.push(claim);
    }
    let result = assemble_answer(&claims, notes, &[], 0);
    ComposedAnswer {
        answer: text::strip(&format!("{PARTIAL_NOTICE}\n\n{}", result.answer)).to_string(),
        linked_answer: text::strip(&format!("{PARTIAL_NOTICE}\n\n{}", result.linked_answer)).to_string(),
        ..result
    }
}

/// The facts the reader already wrote, in requirement order, offered as the answer without a composer.
///
/// Each fact cites its quote and the records it draws on, so this draft passes the same claim checks a composed
/// answer does. Whether it reads as an answer to the task is Jev's call, made in the done check.
pub fn draft_answer(plan: &Plan, notes: &Notes) -> Option<ComposedAnswer> {
    let mut claims: IndexMap<String, Claim> = IndexMap::new();
    for requirement in &plan.requirements {
        if requirement.kind == RequirementKind::Information {
            for (key, fact) in notes.supporting(&requirement.id) {
                claims.entry(key.clone()).or_insert_with(|| Claim::new(&fact.text, vec![key]));
            }
        }
    }
    if claims.is_empty() {
        return None;
    }
    let claims: Vec<Claim> = claims.into_values().collect();
    Some(assemble_answer(&claims, notes, &plan.requirements, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_citation_stripping_accepts_address_qualified_evidence_ids() {
        let key = "a1759f11b7f98944:0:9:f1e2c7da767e6e0f";
        assert_eq!(without_citation_markup(&format!("Price \u{a3}12 [{key}]")), "Price \u{a3}12");
        assert_eq!(without_citation_markup(&format!("Price \u{a3}12 [{key}](https://example.test)")), "Price \u{a3}12");
        assert_eq!(without_citation_markup("Value [sha:1:2], [derived:abc], [tally:abc]"), "Value , ,");
        assert_eq!(without_citation_markup("Use [optional] fields"), "Use [optional] fields");
        assert_eq!(without_citation_markup("See [docs](https://x.test) and [3]"), "See docs and");
    }
}
