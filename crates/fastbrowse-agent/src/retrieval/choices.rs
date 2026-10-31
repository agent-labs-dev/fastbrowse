//! Jev's short-fact read: a bounded set of quoted spans, one choice per requirement, and the relevance pass that
//! narrows a page too large to offer whole.

use std::collections::BTreeMap;

use fastbrowse_core::config::TokenBudget;
use fastbrowse_core::decision::{Answer, ChoiceQuestion, DecisionClient, MAX_CHOICE_OPTIONS, NoulQuestion, Question};
use fastbrowse_core::models::{CostComponent, CostLine, FactReader, UNTRUSTED};
use fastbrowse_core::page::{Block, BlockKind, Capture};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::py::{round, text};
use fastbrowse_core::telemetry::{Ledger, Trace};
use indexmap::{IndexMap, IndexSet};
use serde_json::{Map, Value, json};

use super::chunks::evidence;
use super::fields::{Candidate, ScalarValue, context, source};
use super::reading::{block_context, heading_facts};
use super::route;
use super::text::{Page, SpanKind, cells, slice, spans, splitlines};
use crate::batches::evaluate_batches;
use crate::difflib::unified_diff;
use crate::error::Result;
use crate::memory::{Fact, Notes, fact_id, unique};
use crate::planner::Requirement;

/// The one bar a Jev read answer must reach to stand: a value chosen, the page holding nothing, or the page adding
/// nothing to the quotes already held. Each of those settles a requirement without the LLM reader, so a mistaken one
/// marks a requirement evidenced, or skips the page that held it; under the bar the LLM reader reads the page instead,
/// which costs a call and loses nothing.
pub(super) const READ_CONFIDENCE: f64 = 0.90;
// Long passages crowd out other relevance questions, so score bounded windows of neighboring blocks.
const FOCUS_WINDOW_CHARS: i64 = 1500;

fn focus_rules() -> String {
    format!(
        "{UNTRUSTED}\nDoes this passage state, qualify or contradict anything the requirements ask for, including headers and\nlabels that give a nearby value its meaning? Site chrome, navigation, footers and unrelated sections do not."
    )
}

fn iter_read_candidates(page: &Page<'_>, blocks: &[&Block], limit: Option<usize>) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for block in blocks {
        let value = page.slice(block.start, block.end);
        let found = if block.kind == BlockKind::Table { cells(value) } else { spans(value, SpanKind::Str) };
        for (start, end, raw) in found {
            if text::strip(&raw).is_empty() {
                continue;
            }
            let length = text::len(&raw);
            let start = start + (length - text::len(raw.trim_start_matches(text::is_space)));
            let end = end - (length - text::len(raw.trim_end_matches(text::is_space)));
            // A cell alone loses its column and row identity at claim checking, and a record's date loses the version
            // it belongs to. Keep the preceding row or record text in the quote, with a distinct end each.
            let quote_start = if matches!(block.kind, BlockKind::Table | BlockKind::Record) { 0 } else { start };
            candidates.push(Candidate {
                id: format!("c{}", candidates.len()),
                value: ScalarValue::Str(slice(value, start, end).to_string()),
                evidence: evidence(page, block, block.start + quote_start as i64, block.start + end as i64),
                context: context(value, start, end, block),
            });
            if limit.is_some_and(|limit| candidates.len() >= limit) {
                return candidates;
            }
        }
    }
    candidates
}

/// Every candidate, or none where more than a choice of `max_options` beside its two escapes could offer.
fn page_read_candidates(page: &Page<'_>, blocks: &[&Block], max_options: usize) -> Vec<Candidate> {
    // Truncation could hide the right answer while leaving a plausible wrong one to choose.
    let candidates = iter_read_candidates(page, blocks, Some(max_options - 1));
    if candidates.len() > max_options - 2 { Vec::new() } else { candidates }
}

/// `read_candidates(capture, blocks=None)`, for a choice Jev could ask.
pub fn read_candidates(capture: &Capture, blocks: Option<&[Block]>) -> Vec<Candidate> {
    let page = Page::new(capture);
    let blocks: Vec<&Block> = blocks.unwrap_or(&capture.blocks).iter().collect();
    page_read_candidates(&page, &blocks, MAX_CHOICE_OPTIONS)
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ChoiceRead {
    pub facts: Vec<Fact>,
    pub absent: Vec<String>,
    pub cost_lines: Vec<CostLine>,
}

fn read_groups(candidates: &[Candidate]) -> IndexMap<String, Vec<Candidate>> {
    let mut groups: IndexMap<(String, String, Option<String>), Vec<Candidate>> = IndexMap::new();
    for candidate in candidates {
        let key = (candidate.value.to_str(), candidate.evidence.url.clone(), candidate.evidence.frame_id.clone());
        groups.entry(key).or_default().push(candidate.clone());
    }
    groups.into_values().map(|group| (group[0].id.clone(), group)).collect()
}

fn read_request(
    page: &Page<'_>,
    requirements: &[Requirement],
    candidates: &[Candidate],
    blocks: Option<&[&Block]>,
) -> (Value, IndexMap<String, Question>) {
    let groups = read_groups(candidates);
    let mut criteria: IndexMap<String, Value> = IndexMap::new();
    for (key, group) in &groups {
        let sources: Vec<Map<String, Value>> = group.iter().map(|c| source(&c.evidence, &c.context)).collect();
        let mut entry = Map::new();
        entry.insert("value".into(), group[0].value.to_str().into());
        if group.len() == 1 {
            entry.extend(sources[0].clone());
        } else {
            entry.insert("sources".into(), Value::Array(sources.into_iter().map(Value::Object).collect()));
        }
        criteria.insert(key.clone(), Value::Object(entry));
    }
    let mut questions = IndexMap::new();
    for requirement in requirements {
        // Plan has no answer-shape field. Jev judges the requirement's meaning in this same call; word lists or
        // passage length cannot reliably tell a scalar lookup from synthesis.
        let mut instructions = format!(
            "{UNTRUSTED}\n\nRequirement: {}\nHow does this page answer the requirement? Select absent when the page holds no evidence for it, not even partial. Observed DOM metadata can evidence absence within its stated scope, such as zero visible h1 headings. An explicit empty-result message also evidences absence within that page's scope; select synthesis to record that finding. Select a candidate when that candidate alone states one short scalar fact that fully answers it, with no inference; a total the page states is a scalar, counting items is not. Otherwise select synthesis: lists, comparisons, summaries, explanations, counts, calculations, several facts, or relevant passages no candidate covers, including one side of a comparison.",
            requirement.text
        );
        if groups.len() < candidates.len() {
            instructions.push_str(" Repeated values share one candidate with all their source contexts. A value answers only when a source states it for the requested record and field.");
        }
        if blocks.is_some() {
            instructions
                .push_str(" The passages that fit are shown, most relevant first; other passages have been omitted.");
        }
        let mut offered = criteria.clone();
        offered.insert(
            "synthesis".into(),
            "Relevant evidence needs the LLM reader, or the answer shape is uncertain.".into(),
        );
        offered.insert("absent".into(), "This page contains no evidence for the requirement; skip reading it.".into());
        questions.insert(requirement.id.clone(), ChoiceQuestion::new(instructions, offered).into());
    }
    let capture = page.capture;
    let mut shown = Map::new();
    shown.insert("url".into(), capture.url.clone().into());
    shown.insert("title".into(), capture.title.clone().into());
    // Unoffered passages can disqualify a plausible candidate, for example an older version.
    let body = match blocks {
        None => capture.text.clone(),
        Some(blocks) => blocks.iter().map(|b| page.slice(b.start, b.end)).collect::<Vec<_>>().join("\n...\n"),
    };
    shown.insert("text".into(), body.into());
    if blocks.is_some() {
        shown.insert("focused".into(), true.into());
    }
    shown.insert("inaccessible_frames".into(), capture.inaccessible_frames.into());
    (json!({"page": Value::Object(shown)}), questions)
}

fn read_fits(state: &Value, questions: &IndexMap<String, Question>, tokens: &TokenBudget) -> bool {
    let written: Vec<String> = questions.values().map(|q| q.dump_json()).collect();
    text::len(&dumps(state)) as i64 <= tokens.input_chars(&written)
}

/// `_focus`: the passages Jev should choose from, best scored first and as many as a choice can hold, or none when a
/// window went unjudged or no candidate fits.
async fn focus<'a>(
    jev: &dyn DecisionClient,
    page: &Page<'a>,
    requirements: &[Requirement],
    tokens: &TokenBudget,
    ledger: Option<&Ledger>,
    trace: &Trace,
) -> Result<(Vec<&'a Block>, Vec<Candidate>, Vec<CostLine>)> {
    let capture = page.capture;
    let mut windows: Vec<Vec<&'a Block>> = Vec::new();
    let mut window_chars = 0;
    for block in &capture.blocks {
        let size = block.end - block.start;
        if windows.is_empty() || window_chars + 1 + size > FOCUS_WINDOW_CHARS {
            windows.push(Vec::new());
            window_chars = -1;
        }
        windows.last_mut().expect("a window was just opened").push(block);
        window_chars += 1 + size;
    }
    let mut questions: IndexMap<String, Question> = IndexMap::new();
    for (i, blocks) in windows.iter().enumerate() {
        let joined = blocks.iter().map(|b| page.slice(b.start, b.end)).collect::<Vec<_>>().join("\n");
        let length = text::len(&joined);
        // A block longer than a window is asked about in pieces: a passage scored on its opening alone could hide the
        // current version below an outdated one.
        let step = FOCUS_WINDOW_CHARS as usize;
        for (j, piece) in (0..length).step_by(step).enumerate() {
            questions.insert(
                format!("w{i}.{j}"),
                NoulQuestion::new(
                    format!("Does this passage bear on the requirements? {}", slice(&joined, piece, piece + step)),
                    None,
                    None,
                )
                .into(),
            );
        }
    }
    let state = json!({
        "rules": focus_rules(),
        "requirements": requirements.iter().map(|r| r.text.clone()).collect::<Vec<_>>(),
        "page": {"url": capture.url, "title": capture.title},
    });
    let answered = evaluate_batches(jev, &state, &questions, tokens, ledger, None, true).await?;
    let (answers, cost) = match answered {
        Some(answered) => (answered.answers, answered.cost),
        None => (IndexMap::new(), Vec::new()),
    };
    // A window is as relevant as its most relevant piece.
    let mut best: BTreeMap<usize, f64> = BTreeMap::new();
    let mut scored = 0;
    for (key, answer) in &answers {
        let window = key[1..].split('.').next().and_then(|i| i.parse().ok());
        if let (Some(window), Some(score)) = (window, answer.probability()) {
            let entry = best.entry(window).or_insert(score);
            *entry = entry.max(score);
            scored += 1;
        }
    }
    let mut ranked: Vec<usize> = best.keys().copied().collect();
    ranked.sort_by(|a, b| best[b].total_cmp(&best[a]).then(a.cmp(b)));
    let passages =
        |shown: usize| -> Vec<&'a Block> { ranked[..shown].iter().flat_map(|i| windows[*i].iter().copied()).collect() };
    let max_options = jev.profile().max_choice_options;
    let fits = |shown: usize| {
        let blocks = passages(shown);
        let candidates = iter_read_candidates(page, &blocks, Some(max_options - 1));
        let (state, questions) = read_request(page, requirements, &candidates, Some(&blocks));
        candidates.len() <= max_options - 2 && read_fits(&state, &questions, tokens)
    };
    let (mut shown, mut high) = (0, ranked.len());
    while shown < high {
        let middle = (shown + high).div_ceil(2);
        if fits(middle) {
            shown = middle;
        } else {
            high = middle - 1;
        }
    }
    let mut kept = passages(shown);
    let mut candidates = page_read_candidates(page, &kept, max_options);
    // A window never judged could hold the answer at any rank, so the LLM reader reads the whole page instead.
    if candidates.is_empty() || scored < questions.len() {
        kept = Vec::new();
        candidates = Vec::new();
    }
    trace.event("read_focus", || {
        let mut fields = Map::new();
        fields.insert("windows".into(), windows.len().into());
        fields.insert("shown".into(), shown.into());
        fields.insert("candidates".into(), candidates.len().into());
        fields.insert("requests".into(), cost.len().into());
        fields
    });
    Ok((kept, candidates, cost))
}

/// `_read_choices`: Jev's scalar choice, its judgement that the page holds nothing, or the reader for the rest.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn read_choices(
    jev: &dyn DecisionClient,
    page: &Page<'_>,
    requirements: &[Requirement],
    tokens: &TokenBudget,
    ledger: Option<&Ledger>,
    notes: Option<&Notes>,
    recovering_outputs: bool,
    trace: &Trace,
) -> Result<ChoiceRead> {
    let capture = page.capture;
    let mut previous: Vec<Value> = Vec::new();
    let mut seen: IndexSet<String> = IndexSet::new();
    let mut blocks: IndexMap<(String, Option<String>), &Block> = IndexMap::new();
    for block in &capture.blocks {
        blocks.insert((block.source_id.clone(), block.frame_id.clone()), block);
    }
    for fact in notes.map(Notes::facts).unwrap_or_default() {
        let Some(quote) = &fact.evidence else { continue };
        if quote.url != capture.url {
            continue;
        }
        let changes = blocks.get(&(quote.source_id.clone(), quote.frame_id.clone())).map(|block| {
            let before = lines_with_ends(&quote.quote);
            let after = lines_with_ends(page.slice(block.start, block.end));
            let before: Vec<&str> = before.iter().map(String::as_str).collect();
            let after: Vec<&str> = after.iter().map(String::as_str).collect();
            unified_diff(&before, &after, "earlier quote", "current block")
        });
        let identity = dumps(&json!([
            fact.text,
            quote.quote,
            quote.frame_id,
            changes,
            quote.heading_path,
            quote.control_context.as_ref().map(Dump::dump),
        ]));
        // Recaptures mint citation ids, but repeating identical context can crowd the novelty check out of its budget.
        if !seen.insert(identity) {
            continue;
        }
        let mut packet = Map::new();
        packet.insert("text".into(), fact.text.clone().into());
        packet.insert("quote".into(), quote.quote.clone().into());
        packet.insert("frame_id".into(), quote.frame_id.clone().into());
        packet.insert("changes".into(), changes.into());
        // Equal field values under different headings can belong to different records.
        if !quote.heading_path.is_empty() {
            packet.insert("heading_path".into(), json!(quote.heading_path));
        }
        // Equal captions under different hovered controls belong to different positional subjects.
        if let Some(control) = &quote.control_context {
            packet.insert("control_context".into(), control.dump());
        }
        previous.push(Value::Object(packet));
    }
    let all: Vec<&Block> = capture.blocks.iter().collect();
    let mut candidates = page_read_candidates(page, &all, jev.profile().max_choice_options);
    if candidates.is_empty() && previous.is_empty() && iter_read_candidates(page, &all, Some(1)).is_empty() {
        route(trace, "llm", "no_bounded_candidate_set", None, Vec::new());
        return Ok(ChoiceRead::default());
    }
    let (mut state, questions) = read_request(page, requirements, &candidates, None);
    let mut asked = questions;
    let mut compare_previous = false;
    // A rejected answer reopens requirements without changing the quotes already held for this page.
    let ids: Vec<&String> = requirements.iter().map(|r| &r.id).collect();
    let reopened = notes.is_some_and(|notes| {
        notes
            .facts()
            .iter()
            .any(|fact| fact.requirement_id.as_ref().is_some_and(|id| ids.contains(&id) && !notes.evidenced(id)))
    });
    if !previous.is_empty() && (!reopened || recovering_outputs) && !asked.contains_key("novelty") {
        let mut compared = state.clone();
        compared["previous"] = Value::Array(previous.clone());
        let listed: Vec<&str> = requirements.iter().map(|r| r.text.as_str()).collect();
        let novelty = NoulQuestion::new(
            format!(
                "{UNTRUSTED} Does the current page add relevant evidence not already preserved in previous for any of these requirements? {}\nChanged prices, dates, availability, record identity or contradictions are new evidence. Cosmetic changes and unrelated countdowns alone are not. The task being unfinished alone does not mean this page adds evidence. Judge the quoted evidence, not its prose summaries. Changes are literal diffs against the current block with the same source id; use them to locate changes, but judge all current page content too.",
                listed.join("\n")
            ),
            Some("The page adds relevant evidence, or this is uncertain."),
            Some("The earlier quotes already preserve all relevant evidence this page adds."),
        );
        let mut with_novelty = asked.clone();
        with_novelty.insert("novelty".into(), novelty.into());
        if read_fits(&compared, &with_novelty, tokens) {
            state = compared;
            asked = with_novelty;
            compare_previous = true;
        }
    }
    let mut focused = false;
    let mut costs: Vec<CostLine> = Vec::new();
    if (candidates.is_empty() && !compare_previous) || !read_fits(&state, &asked, tokens) {
        let (kept, narrowed, spent) = focus(jev, page, requirements, tokens, ledger, trace).await?;
        costs = spent;
        if kept.is_empty() {
            route(trace, "llm", "unfocused", None, Vec::new());
            return Ok(ChoiceRead { cost_lines: costs, ..ChoiceRead::default() });
        }
        candidates = narrowed;
        let (narrowed_state, narrowed_questions) = read_request(page, requirements, &candidates, Some(&kept));
        state = narrowed_state;
        asked = narrowed_questions;
        focused = true;
    }
    // Oversized captures should reach the chunked reader without paying for a doomed choice request.
    if !read_fits(&state, &asked, tokens) {
        route(trace, "llm", "choice_input_too_large", None, Vec::new());
        return Ok(ChoiceRead { cost_lines: costs, ..ChoiceRead::default() });
    }
    if let Some(ledger) = ledger {
        ledger.reserve(CostComponent::Jev, 0.0, 1)?;
    }
    // An optional shortcut's rejected input or malformed answer must still reach the reader.
    let Ok(evaluation) = jev.evaluate(&state, &asked).await else {
        route(trace, "llm", "choice_error", None, Vec::new());
        return Ok(ChoiceRead { cost_lines: costs, ..ChoiceRead::default() });
    };
    if let Some(ledger) = ledger {
        ledger.record([evaluation.cost.clone()])?;
    }
    let mut spent = costs.clone();
    spent.push(evaluation.cost.clone());
    if compare_previous
        && !focused
        && let Some(Answer::Noul(novelty)) = evaluation.answers.get("novelty")
        && 1.0 - novelty.probability >= READ_CONFIDENCE
    {
        route(trace, "none", "preserved_evidence", None, Vec::new());
        return Ok(ChoiceRead {
            facts: Vec::new(),
            absent: requirements.iter().map(|r| r.id.clone()).collect(),
            cost_lines: spent,
        });
    }
    let mut facts: Vec<Fact> = Vec::new();
    let mut absent: Vec<String> = Vec::new();
    let groups = read_groups(&candidates);
    for requirement in requirements {
        let id = Some(requirement.id.as_str());
        let answer = match evaluation.answers.get(&requirement.id) {
            Some(Answer::Choice(answer)) if answer.confidence >= READ_CONFIDENCE => answer,
            found => {
                let choice = match found {
                    Some(Answer::Choice(answer)) => Some(answer),
                    _ => None,
                };
                let fields = vec![
                    ("choice", json!(choice.map(|answer| &answer.choice))),
                    ("confidence", json!(choice.map(|answer| round(answer.confidence, 3)))),
                ];
                route(trace, "llm", "uncertain_choice", id, fields);
                continue;
            }
        };
        if answer.choice == "absent" {
            route(trace, "none", "absent", id, Vec::new());
            absent.push(requirement.id.clone());
            continue;
        }
        let Some(group) = groups.get(&answer.choice) else {
            let reason = if answer.choice == "synthesis" { "synthesis" } else { "invalid_choice" };
            route(trace, "llm", reason, id, vec![("confidence", json!(round(answer.confidence, 3)))]);
            continue;
        };
        let selected = &group[0];
        let context: Vec<Fact> = if group.len() > 1 {
            group
                .iter()
                .map(|c| Fact::read(None, &c.value.to_str(), c.evidence.clone(), FactReader::JevChoice))
                .collect()
        } else {
            Vec::new()
        };
        let mut headings: Vec<Fact> =
            group.iter().flat_map(|c| heading_facts(page, &c.evidence, FactReader::JevChoice)).collect();
        // A candidate is one line of its block, and a quote's line left its author and tag on the next one.
        let mut enclosing: IndexMap<String, Fact> = IndexMap::new();
        for candidate in group {
            if let Some(block) = block_context(page, &candidate.evidence, FactReader::JevChoice) {
                enclosing.insert(fact_id(&block), block);
            }
        }
        headings.extend(enclosing.into_values());
        let basis = unique(context.iter().chain(&headings).map(fact_id));
        facts.extend(context.iter().cloned());
        facts.extend(headings);
        route(trace, "jev_choice", "scalar_candidate", id, vec![("confidence", json!(round(answer.confidence, 3)))]);
        // The candidate's evidence was cut from this capture by code, so it is kept as selected, not re-found. The text
        // is the value alone: the draft answer states a fact's text, and prefixed with the requirement it answered
        // "Find the latest released version of httpx. httpx 0.28.1". The notes name the requirement.
        facts.push(Fact {
            requirement_id: Some(requirement.id.clone()),
            tally: None,
            comparison: None,
            text: selected.value.to_str(),
            evidence: if context.is_empty() { Some(selected.evidence.clone()) } else { None },
            basis,
            reader: FactReader::JevChoice,
            scope: None,
        });
    }
    Ok(ChoiceRead { facts, absent, cost_lines: spent })
}

/// `str.splitlines(keepends=True)`.
fn lines_with_ends(value: &str) -> Vec<String> {
    let chars: Vec<char> = value.chars().collect();
    splitlines(&chars).into_iter().map(|(start, end)| chars[start..end].iter().collect()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::RequirementKind;
    use crate::retrieval::testing::capture;
    use crate::support::testing::block_on;
    use fastbrowse_core::BoxFuture;
    use fastbrowse_core::decision::{DecisionError, DecisionProfile, Evaluation, NoulAnswer, NoulTag};
    use fastbrowse_core::models::{CostBasis, SourceControl};

    /// Scores each passage by the number at its start.
    struct Scorer;

    impl DecisionClient for Scorer {
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
                    let passage = question.instructions.rsplit("requirements? ").next().unwrap();
                    let score: f64 = passage.split(' ').next().unwrap().parse().unwrap();
                    (key.clone(), Answer::Noul(NoulAnswer { kind: NoulTag, probability: score }))
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

    fn passage(score: &str, length: usize) -> String {
        format!("{score} {}", "word ".repeat(length / 5))
    }

    fn focused(captured: &Capture, tokens: &TokenBudget) -> Vec<String> {
        let page = Page::new(captured);
        let requirement = Requirement::new("r", "Find the version", RequirementKind::Information);
        let (kept, _, _) = block_on(focus(&Scorer, &page, &[requirement], tokens, None, &Trace::disabled())).unwrap();
        kept.iter().map(|block| block.source_id.clone()).collect()
    }

    #[test]
    fn passages_reach_the_choice_best_scored_first_whatever_their_score() {
        let parts = [passage("0.2", 1400), passage("0.5", 1400), passage("0.3", 1400)];
        let parts: Vec<_> = parts.iter().map(|part| (BlockKind::Paragraph, part.as_str())).collect();
        assert_eq!(focused(&capture(&parts), &TokenBudget::default()), ["s1", "s2", "s0"]);
    }

    #[test]
    fn the_passages_that_do_not_fit_are_the_lowest_scored() {
        let tokens = TokenBudget::default();
        let count = (tokens.input_chars(&[] as &[&str]) as usize / 1400) + 2;
        let scores: Vec<String> =
            (0..count).map(|i| format!("{:.3}", 0.1 + (i as f64) / (2.0 * count as f64))).collect();
        let parts: Vec<String> = scores.iter().map(|score| passage(score, 1400)).collect();
        let parts: Vec<_> = parts.iter().map(|part| (BlockKind::Paragraph, part.as_str())).collect();
        let kept = focused(&capture(&parts), &tokens);
        assert!(!kept.is_empty() && kept.len() < count);
        let expected: Vec<String> = (0..count).rev().take(kept.len()).map(|i| format!("s{i}")).collect();
        assert_eq!(kept, expected);
    }

    #[test]
    fn short_fact_choice_preserves_equal_captions_control_positions() {
        let captured = capture(&[(BlockKind::Paragraph, "Grace")]);
        let page = Page::new(&captured);
        let base = evidence(&page, &captured.blocks[0], 0, 5);
        let candidates: Vec<Candidate> = ["1 of 2", "2 of 2"]
            .iter()
            .enumerate()
            .map(|(index, position)| Candidate {
                id: format!("c{index}"),
                value: ScalarValue::Str("Grace".into()),
                evidence: fastbrowse_core::models::Evidence {
                    control_context: Some(SourceControl {
                        role: "figure".into(),
                        label: "Avatar".into(),
                        context: Some((*position).into()),
                    }),
                    ..base.clone()
                },
                context: String::new(),
            })
            .collect();
        let requirement = Requirement::new("r", "Find the second avatar name", RequirementKind::Information);
        let (_, questions) = read_request(&page, &[requirement], &candidates, None);
        let Question::Choice(question) = &questions["r"] else { panic!("a choice") };
        let expected: Vec<Value> = ["1 of 2", "2 of 2"]
            .iter()
            .map(|position| {
                json!({"source_id": "s0", "quote": "Grace", "context": "",
                       "control_context": {"role": "figure", "label": "Avatar", "context": position}})
            })
            .collect();
        assert_eq!(question.criteria["c0"]["sources"], Value::Array(expected));
    }
}
