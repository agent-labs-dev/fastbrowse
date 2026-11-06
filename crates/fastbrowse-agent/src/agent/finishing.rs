//! How a run ends: Jev says DONE, and the answer is what Jev read.

use fastbrowse_core::models::{Decider, Evidence, Operation, RunResult, Status, StepOutcome, UntilCheck};
use fastbrowse_core::page::Observation;
use fastbrowse_core::py::text;
use indexmap::IndexMap;
use serde_json::Value;

use super::helpers::unsent_fact;
use super::recovery::{DONE, Idle};
use super::state::{OutputIntent, RunState, Step, Stop};
use super::{Agent, Outcome};
use crate::memory::Notes;
use crate::retrieval::{compose_reviewed, draft_answer, draft_reviewed, marked_partial, verdict};
use crate::verification::{ExtractOptions, Extraction, RecordSchema, extract};

impl Agent {
    async fn observe_if_changed(&self, observation: &Observation) -> Step<Observation> {
        let raw = self.raw().unwrap_or_else(|| observation.clone());
        if self.page.document_changed(&raw).await? {
            return self.observe().await;
        }
        Ok(observation.clone())
    }

    /// The final result of a DONE, or None when the caller's `until` sends the loop back to work.
    pub(super) async fn finish(
        &self,
        state: &mut RunState,
        observation: &Observation,
        output_schema: Option<&RecordSchema>,
        until: Option<&UntilCheck>,
    ) -> Step<Option<RunResult>> {
        // A form filled and left unsent is an ending of its own when that is where the task said to stop and ask.
        if let Some(unsent) = unsent_fact(&state.history, observation)
            && self.stops_to_confirm(state, observation).await?
        {
            return Err(Stop::new(Status::NeedsConfirmation, unsent).into());
        }
        let intent = self.output_intent(state).await?;
        if matches!(intent, OutputIntent::Image | OutputIntent::TextAndImage) && self.page.over_http() {
            return Err(self
                .page
                .needs_browser("The requested output needs a screenshot of the rendered page.".into())
                .into());
        }
        // A task to carry out is not done by reading, or by waiting to read again. Over HTTP nothing can be carried
        // out, and what scripts build on the page was never there to see. A hover only shows what is under the pointer,
        // as a scroll shows what is below: a run that hovered an image whose delete button it never reached had deleted
        // nothing.
        let acted = state.history.iter().any(|entry| {
            entry.outcome == StepOutcome::Executed
                && !matches!(
                    entry.operation,
                    None | Some(
                        Operation::Read | Operation::Scroll | Operation::ScrollUp | Operation::Hover | Operation::Wait
                    )
                )
        });
        if !acted && output_schema.is_none() && matches!(intent, OutputIntent::CarriedOut) {
            // What was read did not do it, so it answers nothing yet.
            state.notes = Notes::default();
            state.to_carry_out = true;
            let reason = "The task asks for something to be carried out, and no step has done it.";
            if self.page.over_http() {
                return Err(self.page.needs_browser(reason.into()).into());
            }
            self.recover(state, observation, reason, Status::Stuck, Some((Operation::Done, None))).await?;
            return Ok(None);
        }
        // Acting is not having done everything asked: a read that answers can come before the steps the task put first.
        if acted && !self.actions_done(state, observation).await? {
            let reason = "A step the task asks to be carried out has not been done, so it is not done.";
            self.recover(state, observation, reason, Status::Stuck, Some((Operation::Done, None))).await?;
            return Ok(None);
        }
        // Jev can say DONE on what it never read, or on a page its last action changed, and an answer comes only from
        // what it read.
        let unread = state.read_documents.get(&super::state::read_key(observation)) != Some(&false);
        if intent != OutputIntent::Image
            && output_schema.is_none()
            && (state.notes.facts().is_empty() || (!observation.document_key.is_empty() && unread))
        {
            let capture = self.capture().await?;
            self.read(state, capture, observation, state.compose).await?;
        }
        // Ending on a failed page with nothing read is that failure, not a finished task.
        if state.notes.facts().is_empty()
            && let Some(failure) = state.site_failure.as_ref().filter(|failure| failure.url == observation.url)
        {
            return Err(failure.stop().into());
        }
        // A drawn page's answer may be only in its pixels, so it is read before the run treats "nothing read" as a
        // step short of the answer. Without this the run recovers on a page whose answer is in the image it shows.
        if intent != OutputIntent::Image && output_schema.is_none() {
            self.read_drawn_page(state, observation).await?;
        }
        // A task over a whole list is not done while a next page the list named is unread: Jev judged the extent from
        // the task, and finishing from the pages reached called part of the list the whole (#750), or ended on one
        // record's own page after leaving the list (#798). Each such page is opened once at most.
        if output_schema.is_none()
            && state.every_page
            && let Some(next) = state.unread_pager_page().cloned()
        {
            return self.open_unread_page(state, observation, next).await;
        }
        // DONE on nothing read, when the task wants to be told something, is a step short of the answer.
        if output_schema.is_none()
            && state.notes.facts().is_empty()
            && matches!(intent, OutputIntent::Text | OutputIntent::TextAndImage)
        {
            let reason = "Nothing read so far answers the task, so it is not done.";
            self.recover(state, observation, reason, Status::Stuck, Some((Operation::Done, None))).await?;
            return Ok(None);
        }
        let Some(result) = Box::pin(self.conclude_attempt(state, observation, output_schema, true)).await? else {
            return Ok(None);
        };
        let reached = match until {
            Some(until) => until(self.raw().map_or_else(|| observation.url.clone(), |raw| raw.url)).await,
            None => true,
        };
        if reached {
            return Ok(Some(result));
        }
        let reason = "The caller's until condition failed on this page.";
        state.history.push(crate::policy::HistoryEntry {
            effect: Some(reason.to_string()),
            ..crate::policy::HistoryEntry::new(Some(Operation::Done), None, StepOutcome::Failed, false)
        });
        self.record_failure(state, observation, Operation::Done, reason, None, None, None, Decider::Code).await?;
        Box::pin(self.count_progress(state, false, observation, Idle::performed(DONE), Status::Stuck)).await?;
        Ok(None)
    }

    /// The caller's schema, filled from a capture taken inside this branch so it overlaps the answer.
    async fn extraction(
        &self,
        task: &str,
        notes: &Notes,
        schema: &RecordSchema,
        ledger: &fastbrowse_core::telemetry::Ledger,
    ) -> Step<Extraction> {
        let capture = self.capture().await?;
        let options = ExtractOptions { notes: Some(notes), tokens: self.config.tokens.clone(), ledger: Some(ledger) };
        Ok(extract(&*self.jev, task, &capture, schema, &options).await?)
    }

    /// Open a next page a whole-list task left unread, as a step the run's code chose.
    async fn open_unread_page(
        &self,
        state: &mut RunState,
        observation: &Observation,
        next: String,
    ) -> Step<Option<RunResult>> {
        state.pager_followed.insert(next.clone());
        self.event("unread_next_page", || serde_json::Map::from_iter([("url".into(), next.clone().into())]));
        let decision = crate::policy::Decision {
            operation: Operation::Navigate,
            url: Some(next),
            target: None,
            tab_id: None,
            operation_confidence: 1.0,
            target_confidence: None,
            login_required: None,
            offered_controls: 0,
            reduction: crate::policy::Reduction::None,
            cost: Vec::new(),
            input_tokens: 0,
            bot_check: None,
            shows_answer: None,
            site_failure: None,
            destination: None,
            replayed: false,
            by_code: true,
            offered: Vec::new(),
            ahead: IndexMap::new(),
        };
        match Box::pin(self.step(state, observation, &decision)).await {
            Err(super::state::Flow::Unsure(unsure)) => {
                self.recover(state, observation, &unsure.reason, unsure.gives_up_as, None).await?;
            }
            other => other?,
        }
        Ok(None)
    }

    /// Read a drawn page's screenshot into the notes when nothing read answers the task.
    ///
    /// A drawn page's answer is only in its pixels: the text layer cannot quote it. The task's wording may allow
    /// saying the value is not visible, and a person looking at the same screen reads it off the image; without this
    /// the run reports nothing while the value is in view. The read runs wherever the run is about to answer or give
    /// up on such a page, so a run that stalls before `conclude` still reads what the screen showed.
    pub(super) async fn read_drawn_page(&self, state: &mut RunState, observation: &Observation) -> Step {
        // What a failed page shows is not what the task asked to be told, as the text read holds too.
        if state.page_failed {
            return Ok(());
        }
        // How a page looks is in its pixels whether or not it shows a picture, and whatever its text answered. Jev is
        // asked that with the task's output, at the first DONE; a run giving up before one reads only drawn pages.
        let drawn_unanswered = observation.visual_content && !state.notes.answered();
        if !drawn_unanswered && !state.appearance {
            return Ok(());
        }
        // Jev's image profile constrains its own questions; this image goes to the LLM reader instead.
        let Some(png) = self.screenshots().await?.into_iter().next() else { return Ok(()) };
        let image = crate::policy::screenshot_reference(&png);
        let read = state.notes.facts().iter().any(|fact| fact.evidence.as_ref().is_some_and(|e| e.source_id == image));
        if read || state.unanswered_images.contains(&image) {
            return Ok(());
        }
        let reading = crate::retrieval::read_image(&*self.llm, &state.task, png.clone(), Some(&state.ledger)).await?;
        state.ledger.record([reading.cost.clone()])?;
        // A look is never in the text layer, so only a reading of text can merely repeat it.
        let reading_text = reading.data.text.filter(|text| {
            !text.trim().is_empty() && (state.appearance || !state.notes.repeats_page_text(text, &observation.url))
        });
        if let Some(text) = reading_text {
            state.notes.add_image_read(&self.mask(&text), &observation.url, observation.captured_at, &png);
            state.compose = true;
        } else {
            state.unanswered_images.insert(image);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) async fn conclude(
        &self,
        state: &mut RunState,
        observation: &Observation,
        output_schema: Option<&RecordSchema>,
    ) -> Step<RunResult> {
        Ok(self.conclude_attempt(state, observation, output_schema, false).await?.expect("terminal conclusion"))
    }

    async fn conclude_attempt(
        &self,
        state: &mut RunState,
        observation: &Observation,
        output_schema: Option<&RecordSchema>,
        recover_rejection: bool,
    ) -> Step<Option<RunResult>> {
        let intent = self.output_intent(state).await?;
        if intent != OutputIntent::Image && output_schema.is_none() {
            self.read_drawn_page(state, observation).await?;
        }
        let mut evidence: Vec<Evidence> = state.notes.facts().iter().filter_map(|fact| fact.evidence.clone()).collect();
        let extraction = match output_schema {
            Some(schema) => {
                let (notes, task, ledger) = (state.notes.clone(), state.task.clone(), state.ledger.clone());
                Some(self.extraction(&task, &notes, schema, &ledger).await?)
            }
            None => None,
        };
        let mut rejection = None;
        let mut answered = if intent == OutputIntent::Image {
            None
        } else if state.compose {
            let (notes, task, ledger) = (state.notes_to_compose(), state.task.clone(), state.ledger.clone());
            let chosen = if state.composition_feedback.is_none() {
                self.trace.phase("verdict", verdict(&*self.jev, &task, &notes, Some(&ledger))).await?
            } else {
                None
            };
            match chosen {
                Some(chosen) => Some(chosen),
                None => {
                    let reviewed = self
                        .trace
                        .phase(
                            "compose",
                            compose_reviewed(
                                &*self.jev,
                                &*self.llm,
                                &task,
                                &notes,
                                Some(&ledger),
                                state.composition_feedback.as_deref(),
                                intent != OutputIntent::CarriedOut,
                            ),
                        )
                        .await?;
                    rejection = reviewed.rejection;
                    Some(reviewed.answer)
                }
            }
        } else if matches!(intent, OutputIntent::Text | OutputIntent::TextAndImage) {
            let (notes, task, ledger) = (state.notes.clone(), state.task.clone(), state.ledger.clone());
            match self.trace.phase("verdict", draft_reviewed(&*self.jev, &task, &notes, Some(&ledger))).await? {
                Some(reviewed) => {
                    // A pick that falls short is written up next time, from everything read by then.
                    state.compose |= reviewed.rejection.is_some() || reviewed.answer.problem.is_some();
                    rejection = reviewed.rejection;
                    Some(reviewed.answer)
                }
                None => None,
            }
        } else {
            draft_answer(&state.notes)
        };
        let rejection = rejection.map(|rejection| self.redact(&rejection));
        // The same draft rejected again: what was gathered since changed nothing that can be written, and reads of a
        // page that reorders on each load count as progress, so only this ends the loop. The run ends with what holds.
        let repeated = state
            .composition_feedback
            .as_deref()
            .zip(rejection.as_deref())
            .is_some_and(|(before, now)| drafted(before).is_some_and(|before| drafted(now) == Some(before)));
        if recover_rejection
            && output_schema.is_none()
            && !repeated
            && let Some(rejection) = rejection
        {
            let reason = rejection_reason(&rejection);
            state.composition_feedback = Some(rejection);
            if let Some(answer) = &answered
                && !answer.claims.is_empty()
            {
                state.composition_partial =
                    Some((answer.clone(), RunState::composition_sources(&state.notes_to_compose())));
            }
            self.recover(state, observation, &reason, Status::Unverified, None).await?;
            return Ok(None);
        }
        if answered.as_ref().is_some_and(|answer| answer.problem.is_none()) {
            state.composition_feedback = None;
            state.composition_partial = None;
        }
        let mut answer_text = None;
        let mut citations = Vec::new();
        let answer_problem = answered.as_ref().and_then(|answer| answer.problem.clone());
        if answer_problem.is_some()
            && let Some(partial) = state.checked_composition(&state.notes_to_compose())
        {
            answered = Some(marked_partial(partial.clone()));
        }
        if let Some(composed) = answered {
            let (text, cited) = self.public_answer(&composed);
            answer_text = (!text.is_empty()).then_some(text);
            citations = cited;
        }
        let mut data = Value::Null;
        let mut status = if answer_problem.is_some() { Status::Unverified } else { Status::Complete };
        let mut error = answer_problem;
        // A whole-list answer from part of the list is not the whole: a limit or a refused page left one unread.
        if state.every_page
            && let Some(unread) = state.unread_list_pages().next()
        {
            status = Status::Unverified;
            error.get_or_insert_with(|| {
                self.redact(&format!(
                    "The list continues at {unread}, which this run did not read, so the answer covers only part of it."
                ))
            });
        }
        if let Some(extraction) = extraction {
            data = extraction.data.unwrap_or(Value::Null);
            evidence.extend(extraction.evidence);
            if let Some(problem) = &extraction.problem {
                status = Status::Unverified;
                // An unverified result says what could not be verified, never `error: null`.
                error.get_or_insert_with(|| self.redact(problem));
            }
        }
        // The answer's notes and each schema field cite independently; the same words on the same page are one
        // citation.
        let mut cited: IndexMap<(String, String), Evidence> = IndexMap::new();
        for item in evidence {
            cited.entry((item.url.clone(), item.quote.clone())).or_insert(item);
        }
        let outcome = Outcome {
            error,
            answer: answer_text,
            data,
            evidence: cited.into_values().collect(),
            citations,
            ..Outcome::default()
        };
        if let Some(observed) = self.observed() {
            self.observe_if_changed(&observed).await?;
        }
        let result = self.result(Some(state), &state.ledger.clone(), status, outcome);
        Ok(Some(self.ending_frame(result, matches!(intent, OutputIntent::Image | OutputIntent::TextAndImage)).await))
    }
}

/// What the navigator is told about a rejected draft, and what the run's error says if recovery runs out: which
/// claim failed which check, so it can look for the evidence that is missing rather than propose done again (#810).
fn rejection_reason(rejection: &str) -> String {
    let feedback: Value = serde_json::from_str(rejection).unwrap_or_default();
    let claim = |index: &Value| {
        let text = index.as_u64().and_then(|index| feedback["draft_claims"].get(index as usize)?["text"].as_str());
        text.map_or_else(|| "A claim".to_string(), text::repr)
    };
    let mut failed: Vec<String> = Vec::new();
    for check in feedback["rejected_checks"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        let (index, part) = check.split_once('/').unwrap_or((check, ""));
        let Some(index) = index.strip_prefix("claim").and_then(|index| index.parse::<u64>().ok()) else {
            if check == "coverage" {
                failed.push("the answer leaves out a fact or value the task asks for".into());
            }
            continue;
        };
        let claim = claim(&index.into());
        match part {
            "count" => {
                let miscount =
                    feedback["miscounted_claims"].as_array().into_iter().flatten().find(|m| m["claim"] == index);
                let held =
                    miscount.and_then(|m| m.get("records_in_cited_quotes").or_else(|| m.get("breakdown_sums_to")));
                match (miscount.map(|m| &m["states"]), held) {
                    (Some(states), Some(held)) => {
                        failed.push(format!("{claim} states {states}, but its cited notes hold {held}"))
                    }
                    _ => failed.push(format!("{claim} states a count its cited notes do not hold")),
                }
            }
            "citations" => failed.push(format!("{claim} cites no note it can be checked against")),
            cell if cell.starts_with("cell") => {
                let column = cell[4..].parse::<usize>().ok().and_then(|cell| feedback["columns"].get(cell)?.as_str());
                match column {
                    Some(column) => failed.push(format!(
                        "{claim}: its {} cell does not follow from the notes it cites",
                        text::repr(column)
                    )),
                    None => failed.push(format!("{claim} does not follow from the notes it cites")),
                }
            }
            _ => failed.push(format!("{claim} does not follow from the notes it cites")),
        }
    }
    failed.dedup();
    if failed.is_empty() {
        return "The composed answer was rejected against its source quotes or requested coverage. Correct the answer or gather the evidence it needs.".into();
    }
    format!(
        "The composed answer was rejected: {}. Correct the answer or gather the evidence it needs.",
        failed.join("; ")
    )
}

/// The claims of a rejected draft, as its feedback records them.
fn drafted(feedback: &str) -> Option<Vec<String>> {
    let feedback: Value = serde_json::from_str(feedback).ok()?;
    let claims = feedback.get("draft_claims")?.as_array()?;
    Some(claims.iter().filter_map(|claim| claim.get("text")?.as_str().map(str::to_string)).collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::rejection_reason;

    #[test]
    fn a_rejected_draft_tells_the_navigator_which_claim_failed_which_check() {
        let feedback = json!({
            "draft_claims": [
                {"text": "There are 9 quotes by Albert Einstein.", "evidence_ids": ["e1"]},
                {"text": "The cheapest is X at £10.69.", "evidence_ids": ["e2"]},
                {"text": "Y costs £12.", "evidence_ids": ["E3"]},
            ],
            "columns": ["Name", "Shape"],
            "rejected_checks": ["claim1", "claim0/count", "claim2/citations", "coverage", "claim2/cell1"],
            "miscounted_claims": [{"claim": 0, "states": 9, "records_in_cited_quotes": 10}],
        });
        let reason = rejection_reason(&feedback.to_string());
        for part in [
            "'The cheapest is X at £10.69.' does not follow from the notes it cites",
            "'There are 9 quotes by Albert Einstein.' states 9, but its cited notes hold 10",
            "'Y costs £12.' cites no note it can be checked against",
            "the answer leaves out a fact or value the task asks for",
            "'Y costs £12.': its 'Shape' cell does not follow from the notes it cites",
        ] {
            assert!(reason.contains(part), "{reason}");
        }
    }
}
