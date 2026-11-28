//! How a run ends: the driver calls done, and the answer is what it quoted, reviewed against what was read.

use fastbrowse_core::models::{Decider, Evidence, Operation, RunResult, Status, StepOutcome, UntilCheck};
use fastbrowse_core::page::Observation;
use fastbrowse_core::py::text;
use indexmap::IndexMap;
use serde_json::{Map, Value};

use super::acting::Carried;
use super::helpers::unsent_fact;
use super::recovery::{DONE, Idle};
use super::state::{Extent, FailureCause, OutputIntent, RunState, SiteFailure, Step, Stop};
use super::{Agent, Outcome};
use std::collections::HashSet;

use crate::memory::{Fact, Notes};
use crate::retrieval::{
    ComposedAnswer, Draft, Screenshot, compose_reviewed, draft_answer, draft_reviewed, noticed, review_written,
};
use crate::verification::{ExtractOptions, Extraction, RecordSchema, extract};

/// The rejected drafts a run sends back to gather evidence, however much progress it makes between them. One run sent
/// three different ones back over 53 steps and 455 Jev calls ($0.39) before its time limit.
pub(super) const MAX_REWRITES: i64 = 3;

/// The stop of a run that finished on a page asking to sign in before it shows what the task asks for (#1582).
fn sign_in_wall() -> Stop {
    Stop::new(
        Status::NeedsLogin,
        "Finish/sign-in-wall",
        "The page asks to sign in before it shows what the task asks for.",
    )
}

impl Agent {
    /// Refuse a done for `reason`: spend a recovery on it and tell the driver why, in those words.
    async fn refuse(&self, state: &mut RunState, observation: &Observation, reason: &str, gives_up_as: Status) -> Step {
        self.recover(state, observation, reason, gives_up_as).await?;
        state.chat.notices.push(format!("done was refused: {}", self.redact(reason)));
        Ok(())
    }

    /// Whether the answer goes out with a screenshot of the page the run ends on, which the checks that judge it are
    /// told: the task asks for one and there is a browser page to picture. A run over HTTP has no rendered page, and
    /// one stopped there on a screenshot task was told a screenshot came with its answer when none could. A run that
    /// holds an address credential takes no image (`images_withheld`), and the checks are told why.
    pub(super) fn screenshot(&self, intent: OutputIntent) -> Screenshot {
        if !intent.attaches_screenshot() || self.page.over_http() {
            Screenshot::Absent
        } else if self.images_withheld() {
            Screenshot::Withheld
        } else {
            Screenshot::Attached
        }
    }

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
            return Err(Stop::new(Status::NeedsConfirmation, "Confirm/unsent-form", unsent).into());
        }
        let intent = self.output_intent(state).await?;
        if intent.attaches_screenshot() && self.page.over_http() {
            return Err(self
                .page
                .needs_browser("The requested output needs a screenshot of the rendered page.".into())
                .into());
        }
        // A task to carry out is not done by reading, or by waiting to read again. Over HTTP nothing can be carried
        // out, and what scripts build on the page was never there to see. A hover only shows what is under the pointer,
        // as a scroll shows what is below: a run that hovered an image whose delete button it never reached had deleted
        // nothing.
        let acted = state.history.iter().any(crate::policy::HistoryEntry::acted);
        let reason = "The task asks for something to be carried out, and no step has done it.";
        // Served markup is not the page a browser shows: it holds what a stylesheet hides and lacks what scripts build,
        // such as a success message kept hidden until a form is sent (#894). A task with a step to take, whatever it
        // wants reported, is finished in a browser, as it is in a run that never fetched.
        if !acted && self.page.over_http() && (state.asks_steps || intent == OutputIntent::CarriedOut) {
            return Err(self.page.needs_browser(reason.into()).into());
        }
        if !acted && output_schema.is_none() && matches!(intent, OutputIntent::CarriedOut) {
            // What was read did not do it, so it answers nothing yet.
            state.notes = Notes::default();
            state.to_carry_out = true;
            self.refuse(state, observation, reason, Status::Stuck).await?;
            return Ok(None);
        }
        // Acting is not having done everything asked: a read that answers can come before the steps the task put first.
        // A step the site refuses is no step left to take: the run reads and answers, and ends unverified (#779).
        // A step tried and not carried out, such as a click on a covered button, is asked about too: a task that wants
        // text after a step is not one whose only step is reading. So is a task that asks for a step when none has been
        // taken: a page can already show what it asks to be made (#1186).
        // A page served as a 5xx reply to what the run sent is not its outcome, whatever it says: code knows the status,
        // and Jev, asked whether a "Thank you, your message has been sent" page served with 503 reports a failure,
        // answered 0.50, so the run completed on it. A page found elsewhere showing the result is still judged.
        let failed_reply = state.site_failure.as_ref().is_some_and(|failure| failure.sent)
            && observation.response_status.is_some_and(|status| status >= 500);
        let carried = if failed_reply {
            Some(Carried::SiteFailed)
        } else if state.history.iter().any(crate::policy::HistoryEntry::acts) || state.asks_steps {
            Some(self.actions_done(state, observation).await?)
        } else {
            self.site_failed(state, observation).await?.then_some(Carried::SiteFailed)
        };
        // A page reporting that the site failed is not where the task ends: the failure can clear, so the run is sent
        // back to retry it, and past the stall budget it ends as that failure (unavailable) rather than complete on an
        // error panel.
        let here = state.site_failure.as_ref().filter(|failure| failure.url == observation.url);
        if carried == Some(Carried::SiteFailed) {
            // A failed send stands wherever the run is now: looking for its result elsewhere and finding another failure
            // does not make sending it again safe.
            let sent = state.site_failure.as_ref().is_some_and(|failure| failure.sent);
            if !sent && here.is_none_or(|failure| failure.cause == FailureCause::Reported) {
                let since = here.map_or(state.steps.len(), |failure| failure.since);
                state.site_failure = Some(SiteFailure {
                    url: observation.url.clone(),
                    cause: FailureCause::Reported,
                    since,
                    sent: false,
                });
            }
            // Told to repeat the action that led to a failed send, the driver sent a message again, which can make it
            // twice when the first went through.
            let reason = if sent {
                "The site's reply to what the run sent reports a failure, so the task is not done and whether it went \
                through is unknown: sending it again can make it twice. Look for its result where the site shows it, or \
                give up as unavailable."
            } else {
                "The page reports that the site failed to deliver what was asked, so the task is not done: open the page \
                again with goto(its address), which loads it afresh, or repeat the action that led to it."
            };
            self.refuse(state, observation, reason, Status::Unavailable).await?;
            return Ok(None);
        }
        if here.is_some_and(|failure| failure.cause == FailureCause::Reported) {
            state.site_failure = None;
        }
        if let Some(carried) = carried {
            state.refused = carried == Carried::Refused;
            state.undone = match carried {
                Carried::Undone(step) => Some(step),
                Carried::Done | Carried::Refused | Carried::SiteFailed => None,
            };
            if let Some(step) = state.undone.as_deref() {
                let reason = format!("The task step {step:?} has not been carried out, so it is not done.");
                self.refuse(state, observation, &reason, Status::Stuck).await?;
                return Ok(None);
            }
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
        // A task over a whole list, or over pages of it the task names, is not done while a next page it covers is
        // unread: Jev judged the extent from the task, and finishing from the pages reached called part of the list the
        // whole (#750), or ended on one record's own page after leaving the list (#798). A page past the ones the task
        // names is not opened (#824). Each such page is opened once at most.
        if state.extent.walks()
            && let Some(next) = state.unread_pager_page().cloned()
        {
            return self.open_unread_page(state, observation, next).await;
        }
        // DONE on nothing that answers, when the task wants to be told something, is a step short of the answer. Notes
        // can hold facts that answer nothing, such as the link a page offered the steps, and no answer is drafted from
        // them: DONE there ended a run complete with no answer (#1174).
        if output_schema.is_none()
            && !state.notes.answered()
            && !state.compose
            && matches!(intent, OutputIntent::Text | OutputIntent::TextAndImage)
        {
            if self.signin_required(state, observation).await? {
                return Err(sign_in_wall().into());
            }
            self.refuse(state, observation, UNANSWERED, Status::Unverified).await?;
            return Ok(None);
        }
        let Some(result) = Box::pin(self.conclude_attempt(state, observation, output_schema, true)).await? else {
            return Ok(None);
        };
        let reached = match until {
            // The embedder is shown the address as events show it: its credentials and secrets masked (#1381).
            Some(until) => until(observation.url.clone()).await,
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

    /// The caller's schema, filled from a capture taken inside this branch so it overlaps the answer, each field chosen
    /// to agree with `answer`'s claims.
    pub(super) async fn schema_fields(
        &self,
        state: &RunState,
        output_schema: Option<&RecordSchema>,
        answer: Option<&ComposedAnswer>,
    ) -> Step<Option<Extraction>> {
        let Some(schema) = output_schema else { return Ok(None) };
        let capture = self.capture().await?;
        let claims: Option<Vec<String>> =
            answer.map(|answer| answer.claims.iter().map(|claim| claim.text.clone()).collect());
        let options = ExtractOptions {
            notes: Some(&state.notes),
            tokens: self.config.tokens.clone(),
            ledger: Some(&state.ledger),
            answer: claims.as_deref().filter(|claims| !claims.is_empty()),
        };
        Ok(Some(extract(&*self.jev, &self.model_task(state), &capture, schema, &options).await?))
    }

    /// Open a next page of the list that a task over the whole list, or over pages of it it names, left unread, as a
    /// step the run's code chose.
    async fn open_unread_page(
        &self,
        state: &mut RunState,
        observation: &Observation,
        next: String,
    ) -> Step<Option<RunResult>> {
        state.pager_followed.insert(next.clone());
        self.event("unread_next_page", || serde_json::Map::from_iter([("url".into(), next.clone().into())]));
        let _ = self.open_by_code(state, observation, next).await?;
        Ok(None)
    }

    /// Open `url` as a step the run's code chose. False when the step did not open it: refused by a gate, or the page
    /// changed under it, which the history shows as no executed navigation.
    pub(super) async fn open_by_code(
        &self,
        state: &mut RunState,
        observation: &Observation,
        next: String,
    ) -> Step<bool> {
        let before = state.history.len();
        let decision = crate::policy::Decision {
            operation: Operation::Navigate,
            url: Some(next),
            target: None,
            tab_id: None,
            operation_confidence: 1.0,
            destination: None,
            replayed: false,
            by_code: true,
            given: None,
        };
        match Box::pin(self.step(state, observation, &decision)).await {
            Err(super::state::Flow::Unsure(unsure)) => {
                self.refuse(state, observation, &unsure.reason, unsure.gives_up_as).await?;
            }
            other => other?,
        }
        Ok(state.history[before..]
            .iter()
            .any(|entry| entry.operation == Some(Operation::Navigate) && entry.outcome == StepOutcome::Executed))
    }

    /// Read a drawn page's screenshot into the notes when nothing read answers the task.
    ///
    /// A drawn page's answer is only in its pixels: the text layer cannot quote it. The task's wording may allow
    /// saying the value is not visible, and a person looking at the same screen reads it off the image; without this
    /// the run reports nothing while the value is in view. The read runs wherever the run is about to answer or give
    /// up on such a page, so a run that stalls before `conclude` still reads what the screen showed.
    pub(super) async fn read_drawn_page(&self, state: &mut RunState, observation: &Observation) -> Step {
        // How a page looks is in its pixels whether or not it shows a picture, and whatever its text answered. Jev is
        // asked that with the task's output, at the first DONE; a run giving up before one reads only drawn pages.
        // A rejected draft says what was read does not answer the task: a chart's title, read as the answer to "how
        // many units were sold in April", stood as answered, the bars were never read, and the rewrite said the value
        // is not shown.
        // An answer the chat driver named in quotes is the answer, so only a task about how the page looks, which the
        // driver cannot see, reads the pixels beside it.
        let drawn_unanswered = observation.visual_content
            && state.chat.answer.is_none()
            && (!state.notes.answered() || state.composition_feedback.is_some());
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
        let name = format!("{image}.png");
        let first_attempt = self.seen().screenshot_attempts.insert(name.clone());
        if first_attempt {
            // The run deadline covers this write too; a failed sink must not stop the image reader.
            match self.page.keep_screenshot(&name, &png).await {
                Ok(Some(artifact)) => self.seen().screenshot_artifacts.push(artifact),
                Ok(None) => {}
                Err(_) => self
                    .event("screenshot_not_kept", || Map::from_iter([("error".into(), "artifact sink failed".into())])),
            }
        }
        let reading =
            crate::retrieval::read_image(&*self.llm, &self.model_task(state), png.clone(), Some(&state.ledger)).await?;
        state.ledger.record([reading.cost.clone()])?;
        // A look is never in the text layer, so only a reading of text can merely repeat it.
        let reading_text = reading.data.text.filter(|text| {
            !text.trim().is_empty() && (state.appearance || !state.notes.repeats_page_text(text, &observation.url))
        });
        if let Some(text) = reading_text {
            let fact = Fact::read_from_image(&self.mask(&text), &observation.url, observation.captured_at, &png);
            // An image read has no capture, so its trace event keeps the fact with its evidence: `Notes::add` rebuilds
            // it, as `Notes::record` rebuilds a page read. Without it a trace lost every answer read off a screenshot.
            self.event("image_read", || {
                Map::from_iter([
                    ("step".into(), state.steps.len().into()),
                    ("url".into(), observation.url.clone().into()),
                    ("facts".into(), serde_json::to_value([&fact]).unwrap_or_default()),
                ])
            });
            state.notes.add(fact);
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
        let mut rejection = None;
        let mut answered = if intent == OutputIntent::Image {
            None
        } else if intent == OutputIntent::CarriedOut && !state.refused {
            // A task that asks to be told nothing is answered by what was read: writing it up cost a task to open a
            // page 1 to 4.5 seconds of composing (#910). A step the site refused is reported, as REFUSED says.
            draft_answer(&outcome_notes(state))
        } else if let Some(written) = state.chat.answer.clone() {
            // The driver wrote the answer from the pages it walked, so it is reviewed as written, with no composer. It is
            // reviewed against the notes a composer is given: against every note, a count over pages 2 to 3 took in
            // the records of page 1, read whole on the way.
            let (notes, task, ledger) = (state.notes_to_compose(), self.model_task(state), state.ledger.clone());
            let (steps, kept) = (state.executed_steps(), state.committed_steps());
            let reviewed = self
                .trace
                .phase(
                    "review",
                    review_written(
                        &*self.jev,
                        &*self.llm,
                        &task,
                        &written,
                        &notes,
                        Some(&ledger),
                        intent != OutputIntent::CarriedOut,
                        self.screenshot(intent),
                        &steps,
                        &kept,
                    ),
                )
                .await?;
            rejection = reviewed.rejection;
            Some(reviewed.answer)
        } else if state.compose {
            let (notes, task, ledger) = (state.notes_to_compose(), self.model_task(state), state.ledger.clone());
            let steps = state.executed_steps();
            let kept = state.committed_steps();
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
                        self.screenshot(intent),
                        &steps,
                        &kept,
                    ),
                )
                .await?;
            rejection = reviewed.rejection;
            Some(reviewed.answer)
        } else if matches!(intent, OutputIntent::Text | OutputIntent::TextAndImage) {
            let (notes, task, ledger) = (state.notes.clone(), self.model_task(state), state.ledger.clone());
            let steps = state.executed_steps();
            let kept = state.committed_steps();
            let reviewed = match self
                .trace
                .phase(
                    "coverage",
                    draft_reviewed(&*self.jev, &task, self.screenshot(intent), &notes, Some(&ledger), &steps, &kept),
                )
                .await?
            {
                Some(Draft::Reviewed(reviewed)) => Some(reviewed),
                // The picks hold the answer among other page text, which only a writer can narrow: it writes the
                // answer from them now, and their claim check holds it to their quotes.
                Some(Draft::Write(feedback)) => {
                    let written = compose_reviewed(
                        &*self.jev,
                        &*self.llm,
                        &task,
                        &notes,
                        Some(&ledger),
                        Some(&feedback),
                        true,
                        self.screenshot(intent),
                        &steps,
                        &kept,
                    );
                    Some(self.trace.phase("compose", written).await?)
                }
                None => None,
            };
            reviewed.map(|reviewed| {
                // A pick that falls short is written up next time, from everything read by then.
                state.compose |= reviewed.rejection.is_some() || reviewed.answer.problem.is_some();
                rejection = reviewed.rejection;
                reviewed.answer
            })
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
            .is_some_and(|(before, now)| same_draft(before, now));
        // A run with an output schema composes its answer too, and a rejected one ends it unverified, so it gets the
        // same rewrite: without one, a single misspelt name ended a cheapest-product run with its extraction correct.
        if recover_rejection
            && !repeated
            && let Some(rejection) = rejection
        {
            // A few rewrites are free of the stall budget; past them the next rejection is the recovery that runs out.
            // Spent as a recovery, the first rejection used the run's one recovery, and the driver's corrected draft,
            // rejected again with no page read between, ended the run and handed the answer to a composer.
            // A wall is no answer to rewrite: the page the run stands on is what the caller has to act on (#1582).
            let walled = self.signin_required(state, observation).await?;
            let exhausted = !walled && state.rewrites >= MAX_REWRITES;
            if exhausted {
                state.recoveries = state.recoveries.max(self.config.stall.max_recoveries);
            }
            state.rewrites += 1;
            let reason = rejection_reason(&rejection, false);
            if let Some(answer) = &answered
                && !answer.claims.is_empty()
            {
                state.composition_partial = Some((
                    withheld(&rejection, answer.clone()),
                    RunState::composition_sources(&state.notes_to_compose()),
                    state.executed_steps().len(),
                ));
            }
            state.composition_feedback = Some(rejection);
            // Filled from the claims that passed, so a stop before the rewrite keeps no value the answer did not give.
            let passed = answered.as_ref().filter(|answer| !answer.claims.is_empty());
            state.schema_partial = self.schema_fields(state, output_schema, passed).await?;
            // The stop carries the validated partial saved above, so a caller gets the wall and what was supported.
            if walled {
                return Err(sign_in_wall().into());
            }
            if exhausted {
                self.refuse(state, observation, &reason, Status::Unverified).await?;
            } else {
                state.chat.queue.clear();
                state.chat.notices.push(format!("done was refused: {reason}"));
            }
            return Ok(None);
        }
        // A task that wants to be told something is not done on facts none of which answers it: such a run ended
        // `complete` with no answer (#1343).
        let unanswered = output_schema.is_none()
            && answered.is_none()
            && matches!(intent, OutputIntent::Text | OutputIntent::TextAndImage);
        if unanswered && recover_rejection {
            if self.signin_required(state, observation).await? {
                return Err(sign_in_wall().into());
            }
            self.refuse(state, observation, UNANSWERED, Status::Unverified).await?;
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
            answered = Some(partial.clone());
        }
        // Each field is chosen from what the answer gives, so data cannot name what the answer does not.
        let extraction = self.schema_fields(state, output_schema, answered.as_ref()).await?;
        if let Some(composed) = answered {
            let (text, cited) = self.public_answer(&composed);
            answer_text = (!text.is_empty()).then_some(text);
            citations = cited;
        }
        let mut data = Value::Null;
        let mut status = if answer_problem.is_some() { Status::Unverified } else { Status::Complete };
        // Each check that leaves the answer unverified names itself, and the first, as with `error`, is the kind.
        let mut kind = answer_problem.is_some().then_some("Done/answer-problem");
        // A problem can quote a withheld claim, which can hold what the run typed.
        let mut error = answer_problem.map(|problem| self.redact(&problem));
        // A whole-list answer from part of the list is not the whole: a limit or a refused page left one unread. The same
        // holds for the pages a task names.
        if state.extent.walks()
            && let Some(unread) = state.unread_list_pages().next()
        {
            status = Status::Unverified;
            let message = match state.extent {
                Extent::Pages(Some((first, last))) => format!(
                    "The task covers pages {first} to {last} of the list; {unread} was not read, so the answer covers only part of them."
                ),
                _ => format!(
                    "The list continues at {unread}, which this run did not read, so the answer covers only part of it."
                ),
            };
            error.get_or_insert_with(|| self.redact(&message));
            kind.get_or_insert("Done/list-unread");
        }
        if unanswered {
            status = Status::Unverified;
            error.get_or_insert_with(|| UNANSWERED.to_string());
            kind.get_or_insert("Done/unanswered");
        }
        if state.refused {
            status = Status::Unverified;
            error.get_or_insert_with(|| self.redact(REFUSED));
            kind.get_or_insert("Done/refused");
        }
        if let Some(extraction) = extraction {
            data = extraction.data.unwrap_or(Value::Null);
            evidence.extend(extraction.evidence);
            if let Some(problem) = &extraction.problem {
                status = Status::Unverified;
                // An unverified result says what could not be verified, never `error: null`.
                error.get_or_insert_with(|| self.redact(problem));
                kind.get_or_insert("Done/extraction-problem");
            }
        }
        let outcome = Outcome {
            error,
            answer: answer_text,
            data,
            evidence: one_citation_per_quote(evidence, &state.notes),
            citations,
            ..Outcome::default()
        };
        if let Some(observed) = self.observed() {
            self.observe_if_changed(&observed).await?;
        }
        let mut result = self.result(Some(state), &state.ledger.clone(), status, outcome);
        result.failure_kind = kind.map(Into::into);
        Ok(Some(self.ending_frame(result, intent.attaches_screenshot()).await))
    }
}

pub(super) const UNANSWERED: &str = "Nothing read so far answers the task, so it is not done.";

/// Said before the fields a run that ended without finishing changed, each with what it left there.
pub(super) const CHANGED: &str =
    "This run ended without finishing after changing fields on the page, which may still hold what it entered:";

/// Said before the commits a run that ended without finishing sent, each with the fields of its own form it entered. A
/// run that posted two replies and then stopped on a later click said both replies may still be in their fields (#1317).
pub(super) const COMMITTED: &str =
    "This run sent these commits before it ended, each with what it entered in that commit's form:";

/// Said of a commit the run sent and then ended before the next page showed whether it was taken.
pub(super) const UNSEEN: &str = "the run ended before the page showed whether it was taken";

/// What a clicked option, box or switch is said to have left when the page changed without any field reporting a
/// new value, checked or selected state, as a select that shows its choice only in its label does.
pub(super) const UNREPORTED: &str = "clicked, but no field on the page reports what it now holds";

/// What a run's executed steps did to the page: each commit it sent, as [`COMMITTED`] says it, with what it entered in
/// the commit's own form, then the fields left changed, the latest change to each, as [`CHANGED`] says it: text typed
/// or an option chosen, quoted as recorded (a secret by its marker), and what a click left in each field it changed, as
/// the page read it after the click. A commit sends only its own form, as the page reports it, so a field of another
/// form stays among the changed fields. Without a form, only the committing control's own change is sent.
pub(super) fn changed_fields(history: &[crate::policy::HistoryEntry]) -> Option<String> {
    // Each field's latest value, with its form and the step that directly targeted it.
    let mut changed = IndexMap::new();
    let mut committed = Vec::new();
    for (index, entry) in history.iter().enumerate().filter(|(_, entry)| entry.outcome == StepOutcome::Executed) {
        let form = entry.form.as_ref();
        let mut put = |form, field, left, was_target: bool| {
            let key = (form, field);
            changed.shift_remove(&key);
            changed.insert(key, (was_target.then_some(index), left));
        };
        match (entry.operation, &entry.text, entry.target.as_deref()) {
            (Some(Operation::Fill | Operation::Select), Some(text), Some(target)) => {
                put(form, target, text::repr(text), true)
            }
            _ => entry.set.iter().for_each(|field| {
                put(field.form.as_ref(), field.label.as_str(), field.value.clone(), field.was_target);
            }),
        }
        let Some(commit) = entry.commit else { continue };
        let step = match (entry.operation, entry.target.as_deref()) {
            (Some(operation), Some(target)) => format!("{operation} {}", text::repr(target)),
            (Some(operation), None) => operation.to_string(),
            (None, _) => "a step".to_string(),
        };
        let mut entered = Vec::new();
        changed.retain(|(from, field), (by, left)| {
            let sent = *from == form && (form.is_some() || *by == Some(index));
            if sent {
                entered.push(format!("{field}: {left}"));
            }
            !sent
        });
        let mut line = format!("- {step}");
        if commit == crate::policy::Commit::Sent {
            line += &format!(" ({UNSEEN})");
        }
        if !entered.is_empty() {
            line += &format!(", with {}", entered.join("; "));
        }
        committed.push(line);
    }
    let committed = (!committed.is_empty()).then(|| format!("{COMMITTED}\n{}", committed.join("\n")));
    let changed = (!changed.is_empty()).then(|| {
        let lines: Vec<String> = changed.iter().map(|((_, field), (_, left))| format!("- {field}: {left}")).collect();
        format!("{CHANGED}\n{}", lines.join("\n"))
    });
    match (committed, changed) {
        (Some(committed), Some(changed)) => Some(format!("{committed}\n\n{changed}")),
        (committed, changed) => committed.or(changed),
    }
}

/// What the answer of a run that stopped with a step of its task undone says first: what it read can show what was
/// asked for, and without this a run that never pressed the keys its task named answered that the saved description
/// matched, as if the task were done (#937).
pub(super) fn undone_notice(step: &str) -> String {
    format!(
        "The task step {step:?} was not carried out, so this answer reports what was read, not that the task was done."
    )
}

/// The error of a run whose finish check found a step the site does not allow; the answer names the step.
pub(super) const REFUSED: &str = "A step the task asks for was not carried out because the site does not allow it, as the page or the recent actions show. The answer reports what was and was not done.";

/// What the navigator is told about a rejected draft, and what the run's error says if recovery runs out: which
/// claim failed which check, so it can look for the evidence that is missing rather than propose done again (#810).
pub(super) fn rejection_reason(rejection: &str, published: bool) -> String {
    let (failed, unsupported) = rejected_parts(rejection, published);
    if failed.is_empty() {
        return "The composed answer was rejected against its source quotes or requested coverage. Correct the answer or gather the evidence it needs.".into();
    }
    let mut reason = format!(
        "The composed answer was rejected: {}. Correct the answer or gather the evidence it needs.",
        failed.join("; ")
    );
    // Told only that a "cheapest" claim did not follow from its one quoted fare, the driver cited other passages of
    // that fare until one was accepted, while a cheaper nonstop sat in lines it had not read (google-flights). Told
    // that an extreme is worked out from every record, it read the rest of the list or set a tally.
    if unsupported && !published {
        reason += " A count, total, minimum or maximum across records, such as the cheapest, does not follow from \
                   quoting one record: set the claim's tally and pages, and code works it out from every record on \
                   those pages.";
    }
    // A coverage check rejects the answer whole, so only the draft says what it lacks. Told only that a value was left
    // out, the navigator chose done again over a checkout whose draft gave one shipping method's total and "not shown"
    // for four others; shown that draft, it chose the next method (#897).
    let feedback: Value = serde_json::from_str(rejection).unwrap_or_default();
    let coverage = feedback["rejected_checks"].as_array().is_some_and(|checks| checks.iter().any(|c| c == "coverage"));
    if !published
        && coverage
        && let Some(claims) = drafted(rejection).filter(|claims| !claims.is_empty())
    {
        reason += "\nThe rejected answer:";
        if let Some(columns) = feedback["columns"].as_array().filter(|columns| !columns.is_empty()) {
            let names: Vec<&str> = columns.iter().filter_map(Value::as_str).collect();
            reason += &format!("\n{}", names.join(" | "));
        }
        for claim in claims {
            reason += &format!("\n- {claim}");
        }
    }
    reason
}

/// The claims of a rejected draft that held, under a notice naming what was withheld and why. A run that finished its
/// task and withheld one claim was told "Partial evidence only. Remaining coverage is omitted", a notice for a run that
/// stopped before reading everything, and nothing said which claim was missing (#892).
fn withheld(rejection: &str, held: ComposedAnswer) -> ComposedAnswer {
    let (failed, _) = rejected_parts(rejection, true);
    let notice = if failed.is_empty() {
        "This answer leaves out what did not pass its checks. Each claim below passed them.".to_string()
    } else {
        format!(
            "This answer leaves out what did not pass its checks: {}. Each claim below passed them.",
            failed.join("; ")
        )
    };
    noticed(&notice, held)
}

/// Which claim of a rejected draft failed which check, one sentence part each, and whether a claim failed for not
/// following from the notes it cites.
fn rejected_parts(rejection: &str, published: bool) -> (Vec<String>, bool) {
    let feedback: Value = serde_json::from_str(rejection).unwrap_or_default();
    let claim = |index: &Value| {
        let text = index.as_u64().and_then(|index| feedback["draft_claims"].get(index as usize)?["text"].as_str());
        text.map_or_else(|| "A claim".to_string(), text::repr)
    };
    let mut failed: Vec<String> = Vec::new();
    let mut unsupported = false;
    for check in feedback["rejected_checks"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        let (index, part) = check.split_once('/').unwrap_or((check, ""));
        let Some(index) = index.strip_prefix("claim").and_then(|index| index.parse::<u64>().ok()) else {
            if check == "coverage" {
                failed.push("the answer leaves out a fact or value the task asks for".into());
            } else if check == "actions" {
                failed.push(
                    "the answer says something was not done or already existed that a step this run took may have done"
                        .into(),
                );
            }
            continue;
        };
        let claim = if published {
            let composition = &feedback["drafted_composition"][index as usize];
            let fields: Vec<_> = composition["text"]
                .as_array()
                .into_iter()
                .flatten()
                .chain(composition["cells"].as_array().into_iter().flatten().filter_map(Value::as_array).flatten())
                .filter_map(|part| part["Computed"]["field"].as_str())
                .map(text::repr)
                .collect();
            // Failed field slots are draft notation, not source text. Publishing the draft in the notice left
            // "[title]" beside a price even though that claim had been withheld from the answer (#1247).
            if !fields.is_empty() {
                format!("the claim using the selected record's {}", fields.join(", "))
            } else {
                claim(&index.into())
            }
        } else {
            claim(&index.into())
        };
        match part {
            "tally" => {
                let untallied =
                    feedback["untallied_claims"].as_array().into_iter().flatten().find(|u| u["claim"] == index);
                match untallied.and_then(|u| u["reason"].as_str()) {
                    Some(reason) => failed.push(format!("{claim}: {reason}")),
                    None => failed.push(format!("{claim}: its count, total or extreme could not be worked out")),
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
                unsupported = true;
            }
            _ => match feedback["tally_reasons"][check].as_str() {
                Some(reason) => failed.push(format!("{claim}: {reason}")),
                None => {
                    failed.push(format!("{claim} does not follow from the notes it cites"));
                    unsupported = true;
                }
            },
        }
    }
    failed.dedup();
    (failed, unsupported)
}

/// The claims of a rejected draft, as its feedback records them.
/// Whether two rejections judged the same draft: its claims with what each cites and works out, not their sentences
/// alone. Compared by text, a driver that kept its sentence and cited every record of a list, with the maximum to work
/// out, was taken to repeat its one-quote draft and the run ended unverified (books-travel-priciest).
fn same_draft(before: &str, now: &str) -> bool {
    let draft = |feedback: &str| {
        let feedback: Value = serde_json::from_str(feedback).ok()?;
        let claims = feedback.get("drafted_claims")?.clone();
        Some([claims, feedback["drafted_within"].clone(), feedback["drafted_composition"].clone()])
    };
    draft(before).is_some_and(|before| draft(now) == Some(before))
}

fn drafted(feedback: &str) -> Option<Vec<String>> {
    let feedback: Value = serde_json::from_str(feedback).ok()?;
    let claims = feedback.get("draft_claims")?.as_array()?;
    Some(claims.iter().filter_map(|claim| claim.get("text")?.as_str().map(str::to_string)).collect())
}

/// What a task carried out read once its last step that changes the site was taken: the outcome it reports. What was
/// read before it was kept for those steps, such as a value read on one page and entered on another, and answering with
/// it reported a write task's start page, "Save: disabled", the disabled Save button beside the field it was about to
/// change (#939). A fact drawn from other facts holds what they quote: it is kept when every quote it rests on was
/// read after that step. A read that joined two spans of "hello" into one answering fact had no quote of its own, was
/// dropped, and a run asked to write "hello" and read it back ended `complete` with no answer (#1343).
pub(super) fn outcome_notes(state: &RunState) -> Notes {
    let taken = state.history.iter().filter(|entry| entry.changed_site()).count();
    let notes = &state.notes;
    let after = |evidence: &Evidence| notes.steps_taken(evidence) == Some(taken);
    let sources = notes.evidence();
    let keep: HashSet<String> = notes
        .facts_by_id()
        .iter()
        .filter(|(key, fact)| match &fact.evidence {
            Some(evidence) => after(evidence),
            None => {
                let quotes: Vec<&Evidence> =
                    notes.expand_evidence_ids([key.as_str()]).iter().filter_map(|id| sources.get(id)).collect();
                !quotes.is_empty() && quotes.into_iter().all(after)
            }
        })
        .map(|(key, _)| key.clone())
        .collect();
    let mut outcome = notes.clone();
    outcome.retain(|key, _| keep.contains(key));
    outcome
}

/// The answer's notes and each schema field cite independently; the same read is one citation, its latest.
/// `notes` are the reads the run holds beyond those cited: a read the answer did not select can still be the later
/// capture of one it did.
pub(super) fn one_citation_per_quote(evidence: Vec<Evidence>, notes: &Notes) -> Vec<Evidence> {
    let known: Vec<Evidence> = evidence.iter().cloned().chain(crate::retrieval::noted_evidence(notes)).collect();
    crate::retrieval::latest_reads(evidence, |e| Some(e.clone()), &known, |e| notes.record_of(e).map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{one_citation_per_quote, rejection_reason, same_draft};
    use crate::memory::Notes;

    /// A rewrite that keeps its sentence and cites every record of the list, with the maximum to work out, is a new
    /// draft; only the same claims citing the same notes and working out the same thing are the draft rejected again.
    #[test]
    fn test_a_draft_that_cites_more_is_not_the_same_draft() {
        let claim = "A Year in Provence is the most expensive Travel book, at £56.88.";
        let feedback = |ids: &[&str], tally: Option<serde_json::Value>| {
            json!({"drafted_claims": [{"text": claim, "evidence_ids": ids, "tally": tally}], "rejected_checks": ["claim0"]})
                .to_string()
        };
        let first = feedback(&["e0"], None);
        let max = json!({"op": "max", "of": "book", "field": "price", "written": "A Year in Provence"});
        assert!(same_draft(&first, &feedback(&["e0"], None)));
        assert!(!same_draft(&first, &feedback(&["e0", "e1", "e2"], None)));
        assert!(!same_draft(&first, &feedback(&["e0"], Some(max))));
    }

    #[test]
    fn the_same_words_read_again_are_cited_as_of_their_latest_read() {
        let read = |sha: &str, start: u32, at: &str| {
            serde_json::from_value(json!({
                "source_id": "main/:76", "url": "https://example.test/#/edit", "frame_id": null, "captured_at": at,
                "capture_sha256": sha, "start": start, "end": start + 14, "quote": "Save: disabled",
            }))
            .unwrap()
        };
        let cited = one_citation_per_quote(
            vec![
                read("before-edit", 17198, "2026-11-21T20:29:37Z"),
                read("after-reload", 17234, "2026-11-21T20:30:12Z"),
            ],
            &Notes::default(),
        );
        assert_eq!(cited.len(), 1);
        assert_eq!((cited[0].capture_sha256.as_str(), cited[0].start), ("after-reload", 17234));
    }

    #[test]
    fn an_older_capture_appended_after_a_newer_read_does_not_replace_it() {
        let read = |sha: &str, start: u32, at: &str| {
            serde_json::from_value(json!({
                "source_id": "main/:76", "url": "https://example.test/#/edit", "frame_id": null, "captured_at": at,
                "capture_sha256": sha, "start": start, "end": start + 14, "quote": "Save: disabled",
            }))
            .unwrap()
        };
        let cited = one_citation_per_quote(
            vec![
                read("after-reload", 17234, "2026-11-21T20:30:12Z"),
                read("before-edit", 17198, "2026-11-21T20:29:37Z"),
            ],
            &Notes::default(),
        );
        assert_eq!(cited.len(), 1);
        assert_eq!((cited[0].capture_sha256.as_str(), cited[0].start), ("after-reload", 17234));
    }

    #[test]
    fn identical_quotes_in_one_capture_keep_distinct_source_spans() {
        let read = |source_id: &str, start: u32| {
            serde_json::from_value(json!({
                "source_id": source_id, "url": "https://example.test/records", "frame_id": null,
                "captured_at": "2026-11-21T20:30:12Z", "capture_sha256": "same-capture",
                "start": start, "end": start + 3, "quote": "GBP 5",
            }))
            .unwrap()
        };
        let cited = one_citation_per_quote(vec![read("record-a", 10), read("record-b", 30)], &Notes::default());
        assert_eq!(cited.len(), 2);
        assert_eq!(
            cited.iter().map(|item| (item.source_id.as_str(), item.start)).collect::<Vec<_>>(),
            [("record-a", 10), ("record-b", 30)]
        );
    }

    #[test]
    fn withheld_winner_fields_do_not_publish_unfilled_drafts() {
        let feedback = json!({
            "draft_claims": [{"text": "[title] is the most expensive at GBP 54.81."}],
            "drafted_composition": [{"text": [
                {"Computed": {"field": "title"}},
                {"Prose": " is the most expensive at "},
                {"Computed": {"field": null}},
            ], "cells": null}],
            "rejected_checks": ["claim0/tally"],
            "untallied_claims": [{"claim": 0, "reason": "No title could be copied from the selected record."}],
        });
        let held = crate::retrieval::ComposedAnswer {
            answer: "There are 13 matching records.".into(),
            linked_answer: "There are 13 matching records.".into(),
            claims: Vec::new(),
            citations: Vec::new(),
            problem: None,
        };
        let answer = super::withheld(&feedback.to_string(), held);
        assert!(!answer.answer.contains("[title]"), "{}", answer.answer);
        assert!(!answer.linked_answer.contains("[title]"), "{}", answer.linked_answer);
        assert!(answer.answer.contains("No title could be copied from the selected record."));
        assert!(answer.answer.contains("There are 13 matching records."));
        assert!(rejection_reason(&feedback.to_string(), false).contains("[title]"));
        assert!(!rejection_reason(&feedback.to_string(), true).contains("[title]"));
        let mut incomplete = feedback;
        incomplete["untallied_claims"][0]["reason"] = json!("Only 1 of 50 pages was read.");
        incomplete["rejected_checks"] = json!(["claim0/tally", "claim0/cell0", "claim0/support"]);
        let notice = super::rejected_parts(&incomplete.to_string(), true).0.join("; ");
        assert!(notice.contains("Only 1 of 50 pages was read."), "{notice}");
        assert!(!notice.contains("[title]"), "{notice}");
    }

    #[test]
    fn coverage_feedback_shows_the_navigator_the_rejected_answer() {
        let feedback = json!({
            "draft_claims": [{"text": "Two matching records were found.", "evidence_ids": ["e0"]}],
            "rejected_checks": ["coverage"],
        });
        let reason = rejection_reason(&feedback.to_string(), false);
        assert!(reason.contains("Two matching records were found."), "{reason}");
    }

    #[test]
    fn a_rejected_draft_tells_the_navigator_which_claim_failed_which_check() {
        let feedback = json!({
            "draft_claims": [
                {"text": "There are 9 quotes by Albert Einstein.", "evidence_ids": ["e1"]},
                {"text": "The cheapest is X at £10.69.", "evidence_ids": ["e2"]},
                {"text": "Y costs £12.", "evidence_ids": ["E3"]},
                {"text": "The total is 175.00 (A 120.00, B 55.00).", "evidence_ids": ["e4"]},
            ],
            "columns": ["Name", "Shape"],
            "rejected_checks": ["claim1", "claim0/tally", "claim2/citations", "coverage", "claim2/cell1", "actions", "claim3/states"],
            "tally_reasons": {"claim3/states": "it states something besides its total"},
            "untallied_claims": [{"claim": 0, "reason": "No note read is one 'quote by Albert Einstein'."}],
        });
        let reason = rejection_reason(&feedback.to_string(), false);
        for part in [
            "'The cheapest is X at £10.69.' does not follow from the notes it cites",
            "'There are 9 quotes by Albert Einstein.': No note read is one 'quote by Albert Einstein'.",
            "'Y costs £12.' cites no note it can be checked against",
            "the answer leaves out a fact or value the task asks for",
            "'Y costs £12.': its 'Shape' cell does not follow from the notes it cites",
            "the answer says something was not done or already existed that a step this run took may have done",
            "'The total is 175.00 (A 120.00, B 55.00).': it states something besides its total",
            "set the claim's tally and pages",
        ] {
            assert!(reason.contains(part), "{reason}");
        }
        let tally_only = json!({
            "draft_claims": [{"text": "There are 9 quotes.", "evidence_ids": ["e1"]}],
            "rejected_checks": ["claim0/tally"],
        });
        let reason = rejection_reason(&tally_only.to_string(), false);
        assert!(!reason.contains("set the claim's tally"), "{reason}");
    }

    #[test]
    fn a_draft_rejected_for_coverage_shows_the_navigator_what_it_lacks() {
        // #897: one shipping method's total was read, and the draft said "not shown" for the others.
        let draft = |checks: serde_json::Value| {
            json!({
                "draft_claims": [
                    {"text": "UPS Saver | $32.61 | $174.69", "evidence_ids": ["e1"]},
                    {"text": "DHL Express | $28.40 | not shown for this method", "evidence_ids": ["e2"]},
                ],
                "columns": ["Shipping method", "Price", "Grand Total"],
                "rejected_checks": checks,
            })
            .to_string()
        };
        let reason = rejection_reason(&draft(json!(["coverage"])), false);
        assert!(
            reason.ends_with(
                "\nThe rejected answer:\nShipping method | Price | Grand Total\n- UPS Saver | $32.61 | $174.69\n- DHL Express | $28.40 | not shown for this method"
            ),
            "{reason}"
        );
        // A claim its own check rejected is named already; the draft is shown only for what no one claim is to blame.
        assert!(!rejection_reason(&draft(json!(["claim1"])), false).contains("The rejected answer"));
    }
}
