//! Reading pages into the notes: one read, and a later page of a list read by its records.

use std::sync::Arc;

use fastbrowse_core::decision::{Answer, DecisionClient, NoulQuestion, Question};
use fastbrowse_core::llm::LLMClient;
use fastbrowse_core::models::{CostComponent, CostLine, Operation, UNTRUSTED};
use fastbrowse_core::page::{Capture, Observation, pager_link};
use fastbrowse_core::py::text;
use fastbrowse_core::telemetry::{Ledger, Trace};
use serde_json::{Map, Value, json};

use super::Agent;
use super::helpers::{answer_evidence, lookup, read_question, unread};
use super::state::{RunState, Step};
use crate::batches::evaluate_batches;
use crate::comparison::complete_comparison;
use crate::error::AgentError;
use crate::memory::{Notes, fact_id};
use crate::planner::{Requirement, RequirementKind};
use crate::policy::Decision;
use crate::retrieval::{ReadOptions, ReadOutcome, TallyReader, filter_tallies, read, read_tallies};

fn sorted(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut values: Vec<String> = values.into_iter().collect();
    values.sort();
    values
}

/// Read `capture`'s records for `wanted` alone, as a later page of a list is read: from a counted field when the reader
/// set one up, else by Jev's record filter, else by the reader, retried once on a capture that lost a record.
///
/// On the `last` page a list code counted ends there: the readers exist only for a list whose every page is needed.
#[allow(clippy::too_many_arguments)]
async fn page_records(
    jev: Arc<dyn DecisionClient>,
    llm: Arc<dyn LLMClient>,
    ledger: Arc<Ledger>,
    trace: Trace,
    capture: Capture,
    wanted: Vec<Requirement>,
    question: String,
    tokens: fastbrowse_core::config::TokenBudget,
    tally_readers: Vec<TallyReader>,
    last: bool,
) -> Result<ReadOutcome, AgentError> {
    let ids: Vec<String> = wanted.iter().map(|r| r.id.clone()).collect();
    let mut counted = None;
    if !tally_readers.is_empty() {
        counted = read_tallies(&capture, &tally_readers, &ids)?;
    }
    if counted.is_none() {
        counted = filter_tallies(&*jev, &*llm, &capture, &tally_readers, &ids, &tokens, Some(&ledger), &trace).await?;
    }
    if let Some(counted) = counted {
        return Ok(if last { ReadOutcome { ended: ids, ..counted } } else { counted });
    }
    let mut costs: Vec<CostLine> = Vec::new();
    let mut retried = false;
    loop {
        let options = ReadOptions {
            tokens: tokens.clone(),
            ledger: Some(&ledger),
            requirements: wanted.clone(),
            records_only: true,
            ..ReadOptions::default()
        };
        let mut notes = Notes::default();
        let outcome = read(&*llm, &capture, &question, &ids, &mut notes, &options).await?;
        costs.extend(outcome.cost_lines.iter().cloned());
        if outcome.incomplete.is_empty() || retried {
            return Ok(ReadOutcome { cost_lines: costs, ..outcome });
        }
        // A bad record range used to poison the whole tally; later pages cannot repair its missing quotes. Retry before
        // merging anything.
        retried = true;
    }
}

/// Whether `decision` would leave an unread page while a list is open. Jev's step choice weighs a page's records against
/// everything else on it, so this is when Jev is asked directly whether the page is that list: the notes never get
/// the records of a page left unread.
pub(super) fn leaves_unread_page(state: &RunState, observation: &Observation, decision: &Decision) -> bool {
    matches!(decision.operation, Operation::Click | Operation::Navigate | Operation::Back)
        && !state.continuing.is_empty()
        && !state.read_documents.contains_key(&observation.document_key)
        && unread(state.plan(), &state.notes)
}

/// How a later page of a list went: read as the list, off the list, or on it but left to the ordinary read.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ListRead {
    Read,
    OffList,
    Unread,
}

/// Jev's word on a later page of a list: whether it is that list, and whether the list goes on after it.
pub(super) struct ListPlace {
    pub same_list: bool,
    pub goes_on: bool,
}

/// The page's links as facts for Jev, each with where it leads and whether the page marks it as its next page.
pub(super) fn list_links(observation: &Observation) -> Vec<Value> {
    let links = observation.controls.iter();
    links
        .filter(|c| c.role == "link" && c.href.is_some() && c.operations.contains(&Operation::Click))
        .map(|c| {
            let mut link = json!({"label": c.label, "href": c.href});
            if pager_link(c) {
                link["next_page"] = true.into();
            }
            link
        })
        .collect()
}

fn same_list_question(list: &str) -> Question {
    NoulQuestion::new(
        format!(
            "{UNTRUSTED}\nThe run was reading a list at {list} and that page showed it going on. Is the page in state another page of that same list, rather than a page about something else or one of the list's items?"
        ),
        Some("It is another page of that same list."),
        Some("It is not."),
    )
    .into()
}

/// Jev's yes or no on whether a page shows a value one of the task's actions must carry to another page. Jev can settle
/// every requirement a page is read for and still leave such a value unquoted: a sign-in read on the Phone page found
/// nothing for the orders it was asked about, so the code it showed never reached the notes, and the run went back for
/// it three times. On 16 labelled pages (codes, a discount, a reference, an address and a key, against sign-in forms,
/// listings and unrelated pages) every page holding such a value scored 0.92 or more and every other 0.21 or less.
fn context_question(task: &str, actions: &[&str]) -> Question {
    let listed: Vec<String> = actions.iter().map(|action| format!("- {action}")).collect();
    NoulQuestion::new(
        format!(
            "{UNTRUSTED}\n\n# Task\n{task}\n\n# Actions still to take\n{}\n\nDoes this page show a value one of these actions needs, such as a code, reference, address or key, that the agent must carry to another page?",
            listed.join("\n")
        ),
        Some("Yes: the page shows a value an action needs."),
        Some("No: no value on this page is needed by these actions."),
    )
    .into()
}

impl Agent {
    /// Return whether the read added evidence, since only new evidence makes a read progress.
    pub(super) async fn read(
        &self,
        state: &mut RunState,
        capture: Option<Capture>,
        observation: Option<&Observation>,
    ) -> Step<bool> {
        let capture = match capture {
            Some(capture) => capture,
            None => self.capture().await?,
        };
        let plan = state.await_plan().await?;
        if lookup(&plan) {
            // Reader-selected excerpts cannot decide whether the full sources fit together in one prompt.
            state.read_source_sizes.insert(capture.url.clone(), text::len(&capture.text) as i64);
            let total: i64 = state.read_source_sizes.values().sum();
            state.oversized_answer |= total > self.config.tokens.llm_chars();
        }
        let information = |r: &&Requirement| r.kind == RequirementKind::Information;
        let mut wanted: Vec<Requirement> =
            state.notes.unresolved(&plan).into_iter().filter(|r| information(&r)).collect();
        let transaction_pending = self.unread_receipt(state).await?;
        let changed_since_read =
            observation.is_some_and(|o| state.read_documents.get(&o.document_key).copied().unwrap_or(false));
        let owed = transaction_pending || (wanted.is_empty() && changed_since_read && plan.page_answer_expected());
        if owed || (wanted.is_empty() && !state.open_answer_outputs.is_empty()) {
            // Every requirement was evidenced off the page before an interaction changed it, so they are asked again
            // of what it shows now.
            wanted = plan.requirements.iter().filter(information).cloned().collect();
        }
        if !state.open_answer_outputs.is_empty()
            && state.oversized_answer
            && !state.finish_partitions.is_empty()
            && !transaction_pending
        {
            let mut regrouped = Vec::new();
            for group in &state.finish_partitions {
                let outputs: Vec<&String> = state
                    .open_answer_outputs
                    .iter()
                    .filter(|check| {
                        group.answer_checks.contains(check) || group.requirements.iter().any(|r| &&r.text == check)
                    })
                    .collect();
                if outputs.is_empty() {
                    continue;
                }
                let joined = outputs.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("; ");
                for requirement in group.requirements.iter().filter(information) {
                    regrouped.push(Requirement { text: joined.clone(), ..requirement.clone() });
                }
            }
            wanted = regrouped;
        }
        let mut off_list = false;
        if let Some(observation) = observation
            && !state.continuing.is_empty()
            && state.list_url.as_ref().is_some_and(|list| *list != capture.url)
        {
            if state.through_end && !state.read_documents.contains_key(&observation.document_key) {
                match Box::pin(self.read_list_page(state, &capture, observation, &wanted)).await? {
                    ListRead::Read => return Ok(true),
                    ListRead::OffList => off_list = true,
                    ListRead::Unread => {}
                }
            } else {
                off_list = !self.on_open_list(state, &capture).await?;
            }
        }
        let wanted_ids: Vec<String> = wanted.iter().map(|r| r.id.clone()).collect();
        if let Some(observation) = observation {
            // This content is read now, so the notes hold what the page shows.
            state.read_documents.insert(observation.document_key.clone(), false);
        }
        if text::strip(&capture.text).is_empty() {
            // An empty page may still be rendering; only new content warrants another read.
            if let Some(observation) = observation
                && self.outwait(observation).await?
            {
                let drawn = self.capture().await?;
                if !text::strip(&drawn.text).is_empty() {
                    return Box::pin(self.read(state, Some(drawn), Some(observation))).await;
                }
            }
            // Nothing on the page can evidence anything, so the reader is not asked. A blank outcome was inspected too:
            // it proves nothing, but must not prevent recovery from navigating.
            for candidate in &mut state.transaction_candidates {
                candidate.outcome_read = true;
            }
            return Ok(false);
        }
        let listing = !state.continuing.is_empty() || observation.is_some_and(|o| o.controls.iter().any(pager_link));
        let began = if listing { state.first_url.clone() } else { None };
        let began = began.map(|url| self.redact(&url));
        let mut question = read_question(&state.task, &wanted, began.as_deref());
        if !state.open_answer_outputs.is_empty() {
            question +=
                &format!("\nStill missing or unsupported answer outputs: {}", state.open_answer_outputs.join("; "));
        }
        let actions: Vec<&str> =
            plan.requirements.iter().filter(|r| r.kind == RequirementKind::Action).map(|r| r.text.as_str()).collect();
        if !actions.is_empty() {
            question += "\nAlso keep quotes containing values needed for the task's remaining actions, even when they do not answer an information requirement. Give these context claims a null requirement_id.";
        }
        let before = state.notes.facts().len();
        let known = answer_evidence(&state.notes, false);
        let evidenced: Vec<String> =
            wanted.iter().filter(|r| state.notes.evidenced(&r.id)).map(|r| r.id.clone()).collect();
        let options = ReadOptions {
            tokens: self.config.tokens.clone(),
            ledger: Some(&state.ledger),
            // What Jev cannot quote, and the context the remaining actions need, still go to the LLM reader.
            jev: Some(&*self.jev),
            context_question: (!actions.is_empty()).then(|| context_question(&state.task, &actions)),
            requirements: wanted.clone(),
            targets: Some(plan.requirements.iter().filter(information).cloned().collect()),
            // Rebinding old quotes during output recovery does not make revisiting their page a fresh source.
            revalidate: owed
                && (transaction_pending || state.open_answer_outputs.is_empty() || !state.oversized_answer),
            recovering_outputs: state.oversized_answer && !state.open_answer_outputs.is_empty(),
            preserve_collections: state.oversized_answer
                || !state.finish_partitions.is_empty()
                || !state.open_answer_outputs.is_empty(),
            field_outputs: state.oversized_answer && !state.finish_partitions.is_empty(),
            continuing: sorted(state.continuing.iter().cloned()),
            incomplete: state.incomplete.iter().cloned().collect(),
            require_all_evidence: !off_list
                && state.through_end
                && state.list_url.as_ref().is_some_and(|list| *list != capture.url),
            trace: self.trace.clone(),
            ..ReadOptions::default()
        };
        let ledger = state.ledger.clone();
        let options = ReadOptions { ledger: Some(&ledger), ..options };
        let outcome = read(&*self.llm, &capture, &question, &wanted_ids, &mut state.notes, &options).await?;
        let mut context_added = false;
        if state.oversized_answer && !state.open_answer_outputs.is_empty() {
            // A field read can return context while another field or collection is still open. Its quotes must reach
            // that target's check even when the reader cannot bind the whole requirement.
            let added: Vec<String> = outcome.facts.iter().map(fact_id).collect();
            context_added =
                state.notes.retain_for(wanted.iter().map(|r| r.id.as_str()), added.iter().map(String::as_str));
            if context_added {
                state.rejected_answer_evidence = None;
            }
        }
        if lookup(&plan) {
            let retained = match state.pending_collections.get(&capture.url) {
                Some((sha, facts)) if *sha == capture.sha256 => facts.clone(),
                _ => Vec::new(),
            };
            let mut merged: indexmap::IndexMap<String, crate::memory::Fact> = indexmap::IndexMap::new();
            for fact in retained.into_iter().chain(outcome.collection_facts.iter().cloned()) {
                merged.insert(fact_id(&fact), fact);
            }
            state
                .pending_collections
                .insert(capture.url.clone(), (capture.sha256.clone(), merged.into_values().collect()));
            if state.oversized_answer {
                // Earlier reads keep page-proven coverage outside ordinary notes until the sources need separate
                // prompts. Entering recovery must not require the same collection to be read again.
                let pending = std::mem::take(&mut state.pending_collections);
                for (_, facts) in pending.into_values() {
                    for fact in facts {
                        state.notes.add(fact);
                    }
                }
            }
        }
        for candidate in &mut state.transaction_candidates {
            candidate.outcome_read = true;
        }
        for requirement in &wanted {
            state.notes.supersede(&requirement.id, &capture.url, &capture.sha256, &capture.text);
        }
        state.incomplete.extend(outcome.incomplete.iter().cloned());
        state.tally_readers = outcome.tally_readers.clone();
        state.comparisons = outcome.comparisons.clone();
        // A records-only recovery can prove that a finite list ended. Apply its code-owned result here, where a normal
        // read can finish without `read_list_page` consuming an ended marker.
        for requirement_id in &outcome.ended {
            if state.incomplete.contains(requirement_id) {
                continue;
            }
            if let Some(comparison) = state.comparisons.get(requirement_id).cloned() {
                complete_comparison(&mut state.notes, requirement_id, &comparison);
            }
            if !state.notes.has_untallied_records(requirement_id) {
                state.notes.complete_tallies(requirement_id);
            }
        }
        // A timer changes the capture hash and a reader can paraphrase the same claim, so only a new source quote or a
        // newly evidenced requirement restores the read budget.
        let progressed = context_added
            || answer_evidence(&state.notes, false).iter().any(|key| !known.contains(key))
            || (state.open_answer_outputs.is_empty()
                && wanted.iter().any(|r| !evidenced.contains(&r.id) && state.notes.evidenced(&r.id)));
        let continues: Vec<String> =
            outcome.continues.iter().filter(|key| !state.notes.evidenced(key)).cloned().collect();
        self.event("read", || {
            Map::from_iter([
                ("url".into(), capture.url.clone().into()),
                ("chars".into(), text::len(&capture.text).into()),
                ("wanted".into(), json!(wanted_ids)),
                ("facts_added".into(), (state.notes.facts().len() - before).into()),
                ("rejected_claims".into(), outcome.rejected_claims.into()),
                ("continues".into(), json!(continues)),
                ("incomplete".into(), json!(state.incomplete)),
                ("uncovered".into(), outcome.uncovered.into()),
            ])
        });
        if progressed {
            state.evidence_reads += 1;
        }
        // A page off the list says nothing about it: the list stays open, to be read whole, for when the run comes back.
        if off_list {
            return Ok(progressed);
        }
        state.through_end = !continues.is_empty() && wanted.iter().all(|r| outcome.through_end.contains(&r.id));
        if let Some(observation) = observation {
            if continues.is_empty() {
                if self.on_open_list(state, &capture).await? {
                    state.continuing.clear();
                }
            } else {
                state.continuing = continues.iter().cloned().collect();
                state.list_url = Some(capture.url.clone());
            }
            self.follow_pages(state, &continues, observation, outcome.expands.as_deref());
        }
        Ok(progressed)
    }

    /// Whether the page is on the list left open, by Jev's word. A read that saw no list going on ends the list only
    /// here: off it, on one of its items or anywhere else, the read says nothing about the list, and a total drawn from
    /// it still covers only what was read.
    pub(super) async fn on_open_list(&self, state: &mut RunState, capture: &Capture) -> Step<bool> {
        let Some(list) = state.list_url.clone().filter(|_| !state.continuing.is_empty()) else { return Ok(false) };
        if list == capture.url {
            return Ok(true);
        }
        state.ledger.reserve(CostComponent::Jev, 0.0, 1)?;
        let questions = indexmap::IndexMap::from([("same_list".to_string(), same_list_question(&list))]);
        let page = json!({"page": {"url": capture.url, "title": capture.title, "text": capture.text}});
        let evaluation = self.jev.evaluate(&page, &questions).await?;
        state.ledger.record([evaluation.cost.clone()])?;
        Ok(matches!(evaluation.answers.get("same_list"), Some(Answer::Noul(answer)) if answer.probability >= 0.5))
    }

    /// Read a later page of a list the run needs whole, once Jev chose to read it: by its records alone, because the
    /// first page's reader set up how they are matched. Whether the list ends here is Jev's word, given the page and its
    /// links. Anything but `Read` leaves the page to the ordinary read.
    pub(super) async fn read_list_page(
        &self,
        state: &mut RunState,
        capture: &Capture,
        observation: &Observation,
        wanted: &[Requirement],
    ) -> Step<ListRead> {
        let Some(place) = self.place_in_list(state, capture, observation).await? else { return Ok(ListRead::Unread) };
        if !place.same_list {
            return Ok(ListRead::OffList);
        }
        let mut computable: std::collections::HashSet<String> =
            state.notes.tallies().into_iter().map(|t| t.requirement_id).collect();
        computable.extend(state.comparisons.keys().cloned());
        // A count Jev matches every page of is code's even before a page holds a match.
        computable.extend(state.tally_readers.iter().map(|reader| reader.requirement_id.clone()));
        let last = !place.goes_on;
        if last && !wanted.iter().all(|r| computable.contains(&r.id)) {
            return Ok(ListRead::Unread);
        }
        let question = read_question(&state.task, wanted, state.first_url.as_deref());
        let outcome = page_records(
            self.jev.clone(),
            self.llm.clone(),
            state.ledger.clone(),
            self.trace.clone(),
            capture.clone(),
            wanted.to_vec(),
            question,
            self.config.tokens.clone(),
            state.tally_readers.clone(),
            last,
        )
        .await?;
        let before = state.notes.facts().len();
        state.notes.remember_capture(capture);
        outcome.merge_records(&mut state.notes)?;
        state.read_documents.insert(observation.document_key.clone(), false);
        state.incomplete.extend(outcome.incomplete.iter().cloned());
        state.evidence_reads += usize::from(state.notes.facts().len() > before);
        for candidate in &mut state.transaction_candidates {
            candidate.outcome_read = true;
        }
        self.event("read", || {
            Map::from_iter([
                ("url".into(), self.redact(&capture.url).into()),
                ("chars".into(), text::len(&capture.text).into()),
                ("wanted".into(), json!(wanted.iter().map(|r| &r.id).collect::<Vec<_>>())),
                ("facts_added".into(), (state.notes.facts().len() - before).into()),
                ("goes_on".into(), place.goes_on.into()),
            ])
        });
        if !last {
            state.list_url = Some(capture.url.clone());
            return Ok(ListRead::Read);
        }
        for requirement_id in &outcome.ended {
            if state.incomplete.contains(requirement_id) {
                continue;
            }
            if let Some(comparison) = state.comparisons.get(requirement_id).cloned() {
                complete_comparison(&mut state.notes, requirement_id, &comparison);
            }
            if !state.notes.has_untallied_records(requirement_id) {
                state.notes.complete_tallies(requirement_id);
            }
        }
        if !wanted.iter().all(|r| state.notes.evidenced(&r.id)) {
            return Ok(ListRead::Unread);
        }
        state.continuing.clear();
        state.through_end = false;
        Ok(ListRead::Read)
    }

    /// Jev's word on whether this page belongs to the list being read and whether that list goes on after it, or None
    /// when the page is too large to ask about.
    pub(super) async fn place_in_list(
        &self,
        state: &RunState,
        capture: &Capture,
        observation: &Observation,
    ) -> Step<Option<ListPlace>> {
        let list = state.list_url.clone().unwrap_or_default();
        let goes_on = NoulQuestion::new(
            format!(
                "{UNTRUSTED}\nDoes this list go on to a further page after the one in state? `links` are the page's own links, each with its href, and next_page marks one the page itself names as its next page. A link back to an earlier page, or to another list, is not a further page."
            ),
            Some("Yes: a further page of this list follows this one."),
            Some("No: this is the list's last page."),
        );
        let questions = indexmap::IndexMap::from([
            ("same_list".to_string(), same_list_question(&list)),
            ("goes_on".to_string(), Question::Noul(goes_on)),
        ]);
        let page = json!({"page": {"url": capture.url, "title": capture.title, "text": capture.text}, "links": list_links(observation)});
        let tokens = &self.config.tokens;
        let Some(answered) =
            evaluate_batches(&*self.jev, &page, &questions, tokens, Some(&state.ledger), None, true).await?
        else {
            return Ok(None);
        };
        let yes =
            |key: &str| matches!(answered.answers.get(key), Some(Answer::Noul(answer)) if answer.probability >= 0.5);
        Ok(Some(ListPlace { same_list: yes("same_list"), goes_on: yes("goes_on") }))
    }

    /// Tell Jev that the list the reader needs goes on past what it read, and which control the reader saw show the rest.
    fn follow_pages(
        &self,
        state: &mut RunState,
        continues: &[String],
        observation: &Observation,
        expands: Option<&str>,
    ) {
        if continues.is_empty() {
            return;
        }
        // "Otherwise finish" was a dead end: the reader withholds a list it has not seen the end of, so the done check
        // refused every such finish.
        let mut show = "open the control that loads or pages through more of the list".to_string();
        // Matched against the page's own labels rather than trusted, so a label the reader invented is not repeated.
        if let Some(expands) = expands
            && observation.controls.iter().any(|c| c.label == expands && c.operations.contains(&Operation::Click))
        {
            show = format!("open \"{expands}\"");
        }
        state.hint = Some(format!(
            "The reader saw the list this task needs go on past what the page shows, so what was read cannot settle it. Show the rest and READ it: {show}, or narrow the list with the page's own filter or sort so the answer is in view."
        ));
    }
}
