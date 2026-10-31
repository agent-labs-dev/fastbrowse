//! Jev's read: a bounded set of quoted spans, one choice over them, and the relevance pass that narrows a page too
//! large to offer whole. Whatever Jev picks stands, an absent page included.

use std::collections::BTreeMap;

use fastbrowse_core::config::TokenBudget;
use fastbrowse_core::decision::{Answer, ChoiceQuestion, DecisionClient, MAX_CHOICE_OPTIONS, NoulQuestion, Question};
use fastbrowse_core::models::{CostComponent, CostLine, Evidence, FactReader, UNTRUSTED};
use fastbrowse_core::page::{Block, BlockKind, Capture};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::py::{round, text};
use fastbrowse_core::telemetry::{Ledger, Trace};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::chunks::evidence;
use super::fields::{Candidate, ScalarValue, context, source};
use super::text::{Page, SpanKind, cells, slice, spans};
use crate::batches::evaluate_batches;
use crate::error::Result;
use crate::memory::{Fact, fact_id, unique};

// Long passages crowd out other relevance questions, so score bounded windows of neighboring blocks.
const FOCUS_WINDOW_CHARS: i64 = 1500;
/// The key the task's one choice goes under.
const CHOICE: &str = "task";
/// What Jev answers when no one candidate states the answer.
const SYNTHESIS: &str = "synthesis";

fn focus_rules() -> String {
    format!(
        "{UNTRUSTED}\nDoes this passage state, qualify or contradict anything the task asks for, including headers and\nlabels that give a nearby value its meaning? Site chrome, navigation, footers and unrelated sections do not."
    )
}

fn iter_read_candidates(page: &Page<'_>, blocks: &[&Block], limit: Option<usize>) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for block in blocks {
        let value = page.slice(block.start, block.end);
        // A downloaded CSV's row is a table block with no cells to split.
        let found = match cells(value) {
            cells if block.kind == BlockKind::Table && !cells.is_empty() => cells,
            _ => spans(value, SpanKind::Str),
        };
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
    task: &str,
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
    // Jev judges the task's meaning in this same call; word lists or passage length cannot reliably tell a scalar lookup
    // from synthesis.
    let mut instructions = format!(
        "{UNTRUSTED}\n\nTask: {task}\nHow does this page answer the task? Select absent when the page holds no evidence for it, not even part of it. Observed DOM metadata can evidence absence within its stated scope, such as zero visible h1 headings. An explicit empty-result message is a finding: select the candidate stating it. Select a candidate when that candidate alone states what answers it; a total the page states is one candidate, counting items is not. Select synthesis when the answer must be composed from several facts, on this page or across pages: lists, comparisons, summaries, explanations, counts or calculations. This page holding one of those facts is reason to select it."
    );
    if groups.len() < candidates.len() {
        instructions.push_str(" Repeated values share one candidate with all their source contexts. A value answers only when a source states it for the requested record and field.");
    }
    if blocks.is_some() {
        instructions
            .push_str(" The passages that fit are shown, most relevant first; other passages have been omitted.");
    }
    let mut offered = criteria;
    offered.insert("absent".into(), "This page contains no evidence for the task; skip reading it.".into());
    offered.insert(
        SYNTHESIS.into(),
        "The answer must be composed from several facts, and this page holds at least one.".into(),
    );
    let questions = IndexMap::from([(CHOICE.to_string(), ChoiceQuestion::new(instructions, offered).into())]);
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
    task: &str,
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
                    format!("Does this passage bear on the task? {}", slice(&joined, piece, piece + step)),
                    None,
                    None,
                )
                .into(),
            );
        }
    }
    let state = json!({
        "rules": focus_rules(),
        "task": task,
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
        let (state, questions) = read_request(page, task, &candidates, Some(&blocks));
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
    let kept = passages(shown);
    let candidates = page_read_candidates(page, &kept, max_options);
    trace.event("read_focus", || {
        let mut fields = Map::new();
        fields.insert("windows".into(), windows.len().into());
        fields.insert("shown".into(), shown.into());
        fields.insert("candidates".into(), candidates.len().into());
        fields.insert("requests".into(), cost.len().into());
        fields.insert("scored".into(), scored.into());
        fields
    });
    Ok((kept, candidates, cost))
}

/// What a read took from a page.
#[derive(Default)]
pub(crate) struct PageRead {
    pub facts: Vec<Fact>,
    /// No one fact answers the task: the answer is written from the notes when the run ends.
    pub compose: bool,
}

/// `read_page`: the facts Jev picks from this page, none when it says the page holds nothing for `task`. An answer it
/// says must be composed keeps every candidate as a fact.
pub(crate) async fn read_page(
    jev: &dyn DecisionClient,
    capture: &Capture,
    task: &str,
    tokens: &TokenBudget,
    ledger: Option<&Ledger>,
    trace: &Trace,
) -> Result<PageRead> {
    let page = Page::new(capture);
    let all: Vec<&Block> = capture.blocks.iter().collect();
    let mut candidates = page_read_candidates(&page, &all, jev.profile().max_choice_options);
    let (mut state, mut asked) = read_request(&page, task, &candidates, None);
    if candidates.is_empty() || !read_fits(&state, &asked, tokens) {
        let (kept, narrowed, _) = focus(jev, &page, task, tokens, ledger, trace).await?;
        if narrowed.is_empty() {
            return Ok(PageRead::default());
        }
        candidates = narrowed;
        (state, asked) = read_request(&page, task, &candidates, Some(&kept));
    }
    if let Some(ledger) = ledger {
        ledger.reserve(CostComponent::Jev, 0.0, 1)?;
    }
    let evaluation = jev.evaluate(&state, &asked).await?;
    if let Some(ledger) = ledger {
        ledger.record([evaluation.cost.clone()])?;
    }
    let Some(Answer::Choice(answer)) = evaluation.answers.get(CHOICE) else { return Ok(PageRead::default()) };
    let groups = read_groups(&candidates);
    if answer.choice == SYNTHESIS {
        let facts = candidates
            .iter()
            .map(|candidate| Fact::read(&candidate.value.to_str(), candidate.evidence.clone(), FactReader::JevChoice))
            .collect();
        return Ok(PageRead { facts, compose: true });
    }
    let Some(group) = groups.get(&answer.choice) else { return Ok(PageRead::default()) };
    let selected = &group[0];
    let context: Vec<Fact> = if group.len() > 1 {
        group.iter().map(|c| Fact::read(&c.value.to_str(), c.evidence.clone(), FactReader::JevChoice)).collect()
    } else {
        Vec::new()
    };
    let mut headings: Vec<Fact> =
        group.iter().flat_map(|c| heading_facts(&page, &c.evidence, FactReader::JevChoice)).collect();
    // A candidate is one line of its block, and a quote's line left its author and tag on the next one.
    let mut enclosing: IndexMap<String, Fact> = IndexMap::new();
    for candidate in group {
        if let Some(block) = block_context(&page, &candidate.evidence, FactReader::JevChoice) {
            enclosing.insert(fact_id(&block), block);
        }
    }
    headings.extend(enclosing.into_values());
    let basis = unique(context.iter().chain(&headings).map(fact_id));
    let mut facts: Vec<Fact> = context.clone();
    facts.extend(headings);
    trace.event("read_choice", || Map::from_iter([("confidence".to_string(), json!(round(answer.confidence, 3)))]));
    // The candidate's evidence was cut from this capture by code, so it is kept as selected, not re-found. The text is
    // the value alone: the answer states a fact's text.
    facts.push(Fact {
        answers: true,
        text: selected.value.to_str(),
        evidence: if context.is_empty() { Some(selected.evidence.clone()) } else { None },
        basis,
        reader: FactReader::JevChoice,
    });
    Ok(PageRead { facts, compose: false })
}

/// `_block_context`: the whole block a narrowed quote was cut from, kept beside it as context.
///
/// The claim check judges a requested subject only by the literal quotes it is offered. Cut to "by Marilyn Monroe", or
/// to a quote's text without the "C.S. Lewis (god)" line under it, a correct answer had no quote naming what it
/// answered for, and the run ended unverified with the record that named it in its notes.
fn block_context(page: &Page<'_>, quote: &Evidence, reader: FactReader) -> Option<Fact> {
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
    Some(Fact::read(&span.quote.clone(), span, reader))
}

/// `_heading_facts`: a scalar quote can omit its subject, so both readers retain the headings that actually scoped it.
fn heading_facts(page: &Page<'_>, quote: &Evidence, reader: FactReader) -> Vec<Fact> {
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
            facts.push(Fact::read(heading, evidence(page, block, block.start, block.start + length), reader));
        }
    }
    facts
}

#[cfg(test)]
mod tests {
    use super::*;
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
                    let passage = question.instructions.rsplit("task? ").next().unwrap();
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
        let (kept, _, _) =
            block_on(focus(&Scorer, &page, "Find the version", tokens, None, &Trace::disabled())).unwrap();
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
        let (_, questions) = read_request(&page, "Find the second avatar name", &candidates, None);
        let Question::Choice(question) = &questions[CHOICE] else { panic!("a choice") };
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
