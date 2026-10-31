//! Answers built from notes: Jev's facts, each with the quotes it rests on.

use std::collections::HashSet;

use fastbrowse_core::citations::text_fragment;
use fastbrowse_core::llm::{LLMClient, Message};
use fastbrowse_core::models::{Citation, LLMPurpose, UNTRUSTED};
use fastbrowse_core::py::text;
use fastbrowse_core::schema::{Field, ModelTy, Schema, Ty};
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::memory::{Notes, fact_id};
use crate::support::generate;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub text: String,
    pub evidence_ids: Vec<String>,
}

impl Claim {
    pub fn new(text: &str, evidence_ids: Vec<String>) -> Claim {
        Claim { text: text.into(), evidence_ids }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposedAnswer {
    pub answer: String,
    /// The same claims, each followed by numbered Markdown links to its quotes: what the caller receives.
    pub linked_answer: String,
    pub claims: Vec<Claim>,
    #[serde(default)]
    pub citations: Vec<Citation>,
}

impl Schema for Claim {
    fn ty() -> Ty {
        ModelTy::frozen(
            "Claim",
            vec![Field::required("text", Ty::Str), Field::required("evidence_ids", Ty::list(Ty::Str))],
        )
        .ty()
    }
}

/// `assemble_answer`: the claims with their numbered links and the citations they use.
pub fn assemble_answer(claims: &[Claim], notes: &Notes) -> ComposedAnswer {
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
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerDraft {
    claims: Vec<Claim>,
}

impl Schema for AnswerDraft {
    fn ty() -> Ty {
        ModelTy::frozen("AnswerDraft", vec![Field::required("claims", Ty::list(Claim::ty()))]).ty()
    }
}

const COMPOSER: &str = "# Composer\nWrite the answer to the task as self-contained plain-text claims in reading order, each citing the evidence_ids of the notes it rests on. Answer only what the task asks to be told, as things stand at the end of the notes, not how the work was done. Write every value the task asks for in the claim text itself. A claim that compares, counts, totals or picks a superlative cites every note it is drawn from. Do not claim what the notes do not evidence: state what remains unsupported instead of supplying a related value.\n\n# Trust\n";

/// The answer Jev said must be composed, written by the LLM from the notes. A claim citing nothing the notes hold is
/// dropped: every claim stands on its quotes.
pub async fn compose(
    llm: &dyn LLMClient,
    task: &str,
    notes: &Notes,
    ledger: Option<&Ledger>,
) -> Result<ComposedAnswer> {
    let facts = notes.facts();
    let labels: IndexMap<String, String> =
        facts.iter().enumerate().map(|(index, fact)| (format!("e{index}"), fact_id(fact))).collect();
    let written: Vec<String> =
        labels.keys().zip(&facts).map(|(label, fact)| format!("{label}: {}", fact.text)).collect();
    let messages = [
        Message::system(format!("{COMPOSER}{UNTRUSTED}")),
        Message::user(format!("# Task\n{task}\n\n# Notes\n{}", written.join("\n"))),
    ];
    let draft = generate::<AnswerDraft>(llm, LLMPurpose::Compose, &messages, ledger).await?;
    if let Some(ledger) = ledger {
        ledger.record([draft.cost.clone()])?;
    }
    let claims: Vec<Claim> = draft
        .data
        .claims
        .into_iter()
        .filter_map(|claim| {
            let cited: Vec<String> = claim.evidence_ids.iter().filter_map(|id| labels.get(id).cloned()).collect();
            (!cited.is_empty()).then(|| Claim::new(&claim.text, cited))
        })
        .collect();
    Ok(assemble_answer(&claims, notes))
}

const PARTIAL_NOTICE: &str = "Partial evidence only. Remaining coverage is omitted; totals may be incomplete.";

/// `partial_answer`: everything a stopped run collected, without another model call.
pub fn partial_answer(notes: &Notes) -> ComposedAnswer {
    let facts = notes.facts();
    let mut quoted: Vec<&&crate::memory::Fact> = facts.iter().filter(|fact| fact.evidence.is_some()).collect();
    quoted.sort_by_key(|fact| !notes.answers(&fact_id(fact)));
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
    let mut counted: HashSet<String> = HashSet::new();
    let mut claims: Vec<Claim> = Vec::new();
    for (_, fact) in ranked {
        let key = fact_id(fact);
        if counted.contains(&key) {
            continue;
        }
        let claim_text = fact.evidence.as_ref().map(|evidence| evidence.quote.clone()).unwrap_or_default();
        let claim = Claim::new(&claim_text, vec![key]);
        counted.extend(notes.expand_evidence_ids(claim.evidence_ids.iter().map(String::as_str)));
        claims.push(claim);
    }
    let result = assemble_answer(&claims, notes);
    ComposedAnswer {
        answer: text::strip(&format!("{PARTIAL_NOTICE}\n\n{}", result.answer)).to_string(),
        linked_answer: text::strip(&format!("{PARTIAL_NOTICE}\n\n{}", result.linked_answer)).to_string(),
        ..result
    }
}

/// The facts standing as the answer, each citing its quote and the records it draws on.
pub fn draft_answer(notes: &Notes) -> Option<ComposedAnswer> {
    let claims: Vec<Claim> =
        notes.supporting().into_iter().map(|(key, fact)| Claim::new(&fact.text, vec![key])).collect();
    (!claims.is_empty()).then(|| assemble_answer(&claims, notes))
}
