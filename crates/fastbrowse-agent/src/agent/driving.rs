//! The loop and its step: observe, judge what the last action did, decide, gate and act.

use std::collections::{BTreeSet, HashSet};
use std::future::Future;
use std::time::{Duration, Instant};

use fastbrowse_core::decision::jev_takes_png;
use fastbrowse_core::effects::{SETTING_ROLES, effect, state_key};
use fastbrowse_core::models::{Decider, Event, Image, Operation, Status, StepEvent, StepOutcome, StepResult, WallKind};
use fastbrowse_core::page::{ActResult, Action, BrowserError, Capture, Control, Dialog, Observation, Tab};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::sha256_hex;
use fastbrowse_core::py::text;
use fastbrowse_core::safety::{origin_of, starts_download};
use serde_json::Map;

use super::helpers::{
    STALE_LINK_FAILURES, access_inspection, answer_evidence, answered, caller_link, code_decision, decision_label,
    describe, dialog_question, drew_something, follow_recovery, join, link_key, lookup, navigation_question, netloc_of,
    not_acting, paging, path_of, read_exhausted, redrawn_read, scheme_of, seen, signature, stale_link_key, unread,
};
use super::reading::covered;
use super::recovery::{READ, withheld};
use super::state::{Flow, HttpFailure, RunState, Step, Stop, TransactionCandidate, Unsure};
use super::{Agent, INTERSTITIAL_SECONDS, REDRAW_WATCH_SECONDS, REPEATS_BEFORE_CYCLE, SHORTCUT_WAIT_SECONDS};
use crate::planner::RequirementKind;
use crate::policy::{Decision, HistoryEntry, decide};
use crate::retrieval::page_quotes;
use crate::shortcut::{accept, accept_start, propose_start};
use crate::verification::RecordSchema;

/// What `unless_redrawn` came back with.
pub(super) enum Watched<T> {
    Done(T),
    /// The page redrew under the work, which was dropped.
    Redrawn,
}

impl Agent {
    pub(super) async fn run_loop(
        &self,
        state: &mut RunState,
        output_schema: Option<&RecordSchema>,
        until: Option<&fastbrowse_core::models::UntilCheck>,
    ) -> Step<fastbrowse_core::models::RunResult> {
        loop {
            state.ledger.check(0.0)?;
            let observation = self.observe().await?;
            if state.first_url.is_none() {
                state.first_url = Some(observation.url.clone());
            }
            self.note_effect(state, &observation);
            let inspect_wall =
                matches!(observation.response_status, Some(401 | 403)) && access_inspection(&state.await_plan().await?);
            if inspect_wall {
                state.http_failure = None;
            }
            if let Some(status) = observation.response_status
                && status >= 400
                && !inspect_wall
            {
                let document = (
                    observation.url.clone(),
                    if observation.document_key.is_empty() {
                        observation.page_key.clone()
                    } else {
                        observation.document_key.clone()
                    },
                    status,
                );
                if !state.http_seen.contains(&document) {
                    state.http_seen.insert(document);
                    if matches!(status, 403 | 429 | 503)
                        && self.outwait(&self.raw().unwrap_or_else(|| observation.clone())).await?
                    {
                        continue;
                    }
                    state.paging_failed |= state.next_page || state.paged_from.is_some();
                    state.next_page = false;
                    state.await_plan().await?;
                    let reason = HttpFailure { url: observation.url.clone(), status }.message();
                    Box::pin(self.recover(state, &observation, &reason, Status::Stuck, None)).await?;
                    continue;
                }
            }
            self.settle(state, &observation);
            // Walking a list is dispatched before Jev is asked, because not asking is the point: the reader asked for
            // the rest of the list and the link is already found, so a decision would buy nothing. It therefore passes
            // none of the gates below, which is safe only because of what it can be: a pager link to another address.
            if state.next_page && state.through_end && !state.paging_failed {
                Box::pin(self.pipeline_pages(state, &observation)).await?;
                if !state.paging_failed
                    && !state.owes_read
                    && lookup(state.plan())
                    && state.open_answer_outputs.is_empty()
                    && answered(state.plan(), &state.notes)
                    && self.observed().is_some()
                    && let Some(result) = Box::pin(self.finish(state, output_schema, until)).await?
                {
                    return Ok(result);
                }
                continue;
            }
            if Box::pin(self.reread_if_changed(state, &observation)).await? {
                continue;
            }
            if !state.open_answer_outputs.is_empty()
                && !state.owes_read
                && state.rejected_answer_evidence.as_ref() != Some(&answer_evidence(&state.notes, true))
                && state.history.last().is_some_and(|entry| entry.read_progress)
                && state.ready_plan.as_ref().is_some_and(|plan| lookup(plan) && answered(plan, &state.notes))
            {
                // Recovery's missing-output list describes the rejected draft. New quoted support needs another
                // finish check before that stale list sends the browser back to content it has already read.
                if let Some(result) = Box::pin(self.finish(state, output_schema, until)).await? {
                    return Ok(result);
                }
                continue;
            }
            if let Some(decision) = paging(state, &observation) {
                let options = StepOptions { gate: false, ..StepOptions::default() };
                if Box::pin(self.step(state, &observation, &decision, Decider::Code, options)).await? {
                    let reason = read_exhausted(state);
                    Box::pin(self.recover(state, &observation, &reason, Status::Stuck, READ)).await?;
                }
                continue;
            }
            let raw = self.raw().unwrap_or_else(|| observation.clone());
            if !drew_something(&raw) && self.settle_blank_opening(state, &raw).await? {
                continue;
            }
            let origin = origin_of(&raw.url);
            let page = (raw.url.clone(), raw.document_key.clone());
            if state.planning.done() {
                state.await_plan().await?;
            }
            let secrets = self.secret_names(&origin);
            // A held secret makes a sign-in wall work to do rather than a stop, but passes no CAPTCHA, so the bot check
            // is asked on every page a secret covers too.
            let fresh = state.last_page.as_ref() != Some(&page);
            let check_login = fresh && secrets.is_empty();
            // Jev's pick when it was asked again on this page is the decision here: asking once more would only ask
            // the same question without what went wrong.
            let picked = state
                .picked
                .take()
                .filter(|(key, _)| *key == state_key(&observation))
                .map(|(_, picked)| picked)
                .filter(|picked| {
                    picked.target.as_ref().is_none_or(|target| observation.controls.iter().any(|c| c.id == target.id))
                });
            let mut decision = match picked {
                Some(picked) => picked,
                None => {
                    let mut context = self.context(state, secrets, check_login, fresh)?;
                    context.screenshot = self.viewport().await?;
                    let redecided = state.redecided;
                    let deciding = decide(
                        &*self.jev,
                        &observation,
                        &context,
                        &self.config,
                        Some(&state.ledger),
                        Some(&state.relevance_cache),
                        &self.trace,
                    );
                    let decided = self
                        .unless_redrawn(redecided, async { deciding.await.map_err(Flow::from) }, &observation, None)
                        .await?;
                    let Watched::Done(decision) = decided else {
                        self.redecide(state, &observation);
                        continue;
                    };
                    // Only an answered decision has asked the page's sign-in and bot questions; a dropped one asked
                    // nothing.
                    state.last_page = Some(page);
                    decision
                }
            };
            let mut decided_by = Decider::Jev;
            let thresholds = &self.config.thresholds;
            let bot_check = decision.bot_check.unwrap_or(0.0) > thresholds.bot_check_above;
            if bot_check || decision.login_required.unwrap_or(0.0) > thresholds.login_required_above {
                // A wall offering nothing to act on cannot be signed into. It is a bot check such as PyPI's "Client
                // Challenge", which clears itself once its script runs, and stopping on it failed five runs in six of
                // a task hosted agents finish by waiting. A bot check that draws a CAPTCHA has controls, and may still
                // clear itself before it does.
                if (bot_check || raw.controls.is_empty()) && self.outwait(&raw).await? {
                    continue;
                }
                if bot_check && raw.controls.is_empty() && self.reload_challenge(state, &raw).await? {
                    continue;
                }
                if bot_check {
                    // The address is the raw observation's, and an error is written out of the process, so a secret
                    // typed earlier and echoed in another host is redacted here as in any other report.
                    let error = format!(
                        "bot check at {}; it is not a sign-in and no credential passes it. Retry using an attached local browser or choose another source for this research.",
                        self.redact(&origin)
                    );
                    let evidence = self.wall_quotes(&raw).await;
                    return Err(Stop {
                        status: Status::Blocked,
                        error: Some(error),
                        evidence,
                        wall: Some(WallKind::BotCheck),
                    }
                    .into());
                }
                let plan = state.await_plan().await?;
                state.passed_wall = Some(raw.url.clone());
                if !access_inspection(&plan) {
                    Box::pin(self.meet_wall(state, &observation, &raw, &plan)).await?;
                    continue;
                }
                // A task inspecting an access boundary needs its observed wall read and verified, not passed.
                decision = code_decision(Operation::Read, None);
                decided_by = Decider::Code;
            }
            let mut uncertain = decision.confidence() < self.config.thresholds.recover_below;
            if (!state.open_answer_outputs.is_empty() || uncertain || !not_acting(decision.operation))
                && let Some(directed) = follow_recovery(state, &observation, &decision, uncertain)
            {
                (decision, uncertain, decided_by) = (directed, false, Decider::Llm);
            }
            if Box::pin(self.read_before_interaction(state, &observation, &decision)).await? {
                let done = state.ready_plan.as_ref().is_some_and(|plan| {
                    lookup(plan) && state.open_answer_outputs.is_empty() && answered(plan, &state.notes)
                });
                if !done {
                    continue;
                }
                // The read answered a lookup, and deciding again on the same page only arrived at DONE: a shortlist and
                // a decision, 1 to 2s, in 11 of 138 0.5.7 runs. `finish` still judges whether DONE holds.
                (decision, uncertain, decided_by) = (code_decision(Operation::Done, None), false, Decider::Code);
            }
            let pager = decision.operation == Operation::Click
                && decision.target.as_ref().is_some_and(fastbrowse_core::page::pager_link);
            // Only a pager waits for the plan: the clicks that set a search up run while it is still being written.
            let plan = if pager { Some(state.await_plan().await?) } else { state.ready_plan.clone() };
            if decision.operation == Operation::Click
                && let Some(plan) = &plan
            {
                if pager && state.open_answer_outputs.is_empty() && answered(plan, &state.notes) {
                    // Everything asked to be found is evidenced, so another page is wandering. DONE is judged again by
                    // `finish`, which carries on if it does not hold.
                    (decision, uncertain, decided_by) = (code_decision(Operation::Done, None), false, Decider::Code);
                } else if pager && !state.read_here && unread(plan, &state.notes) {
                    // Turning the page of a list nobody has read loses that page. Read here first, which also tells
                    // code whether the list goes on.
                    decision = Decision { operation: Operation::Read, target: None, ..decision };
                    (uncertain, decided_by) = (false, Decider::Code);
                }
            }
            if uncertain && state.ready_plan.is_none() {
                // Unsure without the requirements: the plan is already in flight and costs less than recovery.
                state.await_plan().await?;
                continue;
            }
            if not_acting(decision.operation) {
                let plan = state.await_plan().await?;
                // Notes that evidence every requirement can still describe the page before the last interaction
                // redrew it, so an answer owed after one is read off what it drew. A plan that only acts has nothing
                // to read, and finishes without waiting.
                let unread_plan = unread(&plan, &state.notes);
                let mut reading_owed = unread_plan || (state.owes_read && plan.page_answer_expected());
                // Only the interaction's own claim is in question here: an unread requirement needs a read whatever the
                // page shows, and a direction the reader named behind a control is carried out rather than traded for
                // a read of the page that named it.
                let capture = if reading_owed && !unread_plan && state.directed.is_none() {
                    Some(self.capture().await?)
                } else {
                    None
                };
                if capture.as_ref().is_some_and(|capture| covered(state, &observation, capture)) {
                    // The interaction redrew the page and the notes still describe it: everything they quote is still
                    // shown and nothing on it is unread. A read would re-buy what they hold.
                    state.owes_read = false;
                    state.read_here = true;
                    self.event("read_not_owed", || {
                        Map::from_iter([("url".into(), self.redact(&observation.url).into())])
                    });
                    reading_owed = false;
                }
                if reading_owed {
                    let reading = Decision { operation: Operation::Read, target: None, ..decision.clone() };
                    // A read that ran spends the direction it was sent on, but may leave one of its own. A skipped
                    // read spends nothing.
                    let held = state.directed.take();
                    let recoveries = state.recoveries;
                    let options = StepOptions { capture, ..StepOptions::default() };
                    // A DONE that still owes a read is read by rule; a READ is its decider's.
                    let by = if decision.operation == Operation::Read { decided_by } else { Decider::Code };
                    if !Box::pin(self.step(state, &observation, &reading, by, options)).await? {
                        if !(lookup(&plan)
                            && state.open_answer_outputs.is_empty()
                            && answered(&plan, &state.notes)
                            && state.recoveries == recoveries)
                        {
                            continue;
                        }
                        // A read does not change the page, so observing it again and deciding only arrived at DONE,
                        // 1.5 to 2s a lookup. A tripwire that sent the read to recovery left it work to do.
                        decision = Decision { operation: Operation::Done, target: None, ..decision };
                    } else if !unread(&plan, &state.notes) {
                        state.directed = held;
                        // Only an owed read gets here: a scroll changes the page but not its text, so the read found
                        // content already read, and the notes already describe what the interaction drew.
                        decision = Decision { operation: Operation::Done, target: None, ..decision };
                    } else {
                        state.directed = held;
                        let directed = if decision.directed {
                            Some(decision.clone())
                        } else {
                            follow_recovery(state, &observation, &decision, true)
                        };
                        let Some(directed) = directed else {
                            let reason = read_exhausted(state);
                            Box::pin(self.recover(state, &observation, &reason, Status::Stuck, READ)).await?;
                            continue;
                        };
                        (decision, uncertain, decided_by) = (directed, false, Decider::Llm);
                    }
                } else {
                    state.directed = None;
                    decision = Decision { operation: Operation::Done, target: None, ..decision };
                }
            }
            // The confidence gate exists to stop the agent acting on a page it does not understand. READ and DONE do
            // not act: a read changes nothing, and DONE is judged again by `finish`.
            if decision.operation == Operation::Escalate || (uncertain && !not_acting(decision.operation)) {
                let reason = format!("uncertain next step ({:.2})", decision.confidence());
                Box::pin(self.consult(state, &observation, &reason)).await?;
                continue;
            }
            // An action that changed nothing from this page goes to recovery before it is taken again.
            if !not_acting(decision.operation)
                && !decision.directed
                && state.idle.contains(&signature(&decision, &observation))
            {
                let what = decision_label(&decision).map_or_else(
                    || decision.operation.to_string(),
                    |label| format!("{} {}", decision.operation, text::repr(&label)),
                );
                let reason = self.redact(&format!("{what} changed nothing the last time it was taken from this page"));
                Box::pin(self.recover(state, &observation, &reason, Status::Stuck, withheld(&decision))).await?;
                continue;
            }
            if decision.operation == Operation::Done {
                if let Some(result) = Box::pin(self.finish(state, output_schema, until)).await? {
                    return Ok(result);
                }
                continue;
            }
            let acted = async {
                if !uncertain && !decision.directed && Box::pin(self.fill_form(state, &observation, &decision)).await? {
                    return Ok(());
                }
                if Box::pin(self.step(state, &observation, &decision, decided_by, StepOptions::default())).await? {
                    let reason = read_exhausted(state);
                    Box::pin(self.recover(state, &observation, &reason, Status::Stuck, READ)).await?;
                }
                Ok(())
            };
            match acted.await {
                Err(Flow::Unsure(unsure)) => {
                    let left_out = withheld(&decision);
                    Box::pin(self.recover(state, &observation, &unsure.reason, unsure.gives_up_as, left_out)).await?;
                }
                other => other?,
            }
        }
    }

    pub(super) fn redecide(&self, state: &mut RunState, observation: &Observation) {
        self.event("redecide", || Map::from_iter([("url".into(), self.redact(&observation.url).into())]));
        state.redecided = true;
    }

    /// `work`'s result, or `Redrawn` when the page redrew under it: `target`, or with none any control offered.
    ///
    /// A page can draw after it settles, and nothing says it will: picking a day in Google Flights' date picker blanks
    /// the prices, the page waits about 400ms with no request, spinner or mutation, and then fetches and draws them.
    /// Watching while Jev decides and gates drops the work as soon as it is doomed, and the next decision is made on
    /// the settled page. Only one drop per action: a control that never stops changing is still acted on.
    pub(super) async fn unless_redrawn<T, F>(
        &self,
        redecided: bool,
        work: F,
        observation: &Observation,
        target: Option<&str>,
    ) -> Step<Watched<T>>
    where
        F: Future<Output = Step<T>>,
    {
        if redecided {
            return work.await.map(Watched::Done);
        }
        // Python runs the work and the watch as tasks and waits on both, so whatever was already ready, such as a head
        // start's plan, runs first. Without a shortcut to wait for, this is a run's first wait, and a plan left
        // unpolled here went unasked and unbilled when the first action stopped the run.
        tokio::task::yield_now().await;
        let raw = self.raw().unwrap_or_else(|| observation.clone());
        let watching = self.page.redrawn(&raw, REDRAW_WATCH_SECONDS, target);
        tokio::pin!(work);
        tokio::pin!(watching);
        tokio::select! {
            biased;
            done = &mut work => done.map(Watched::Done),
            redrawn = &mut watching => match redrawn {
                Ok(true) => Ok(Watched::Redrawn),
                Ok(false) => work.await.map(Watched::Done),
                Err(error) => Err(error.into()),
            },
        }
    }

    /// The page to begin on when the caller named none, proposed from the task exactly as the shortcut is.
    pub(super) async fn first_page(&self, task: &str, ledger: &fastbrowse_core::telemetry::Ledger) -> Step<String> {
        let proposal = propose_start(&*self.llm, task, Some(ledger)).await?;
        ledger.record([proposal.cost])?;
        let Some(opening) = accept_start(proposal.data.url.as_deref()) else {
            return Err(
                Stop::new(Status::NeedsInput, "the task names no page to start from, and none was given").into()
            );
        };
        self.event("start_page", || Map::from_iter([("url".into(), opening.clone().into())]));
        Ok(opening)
    }

    /// Go to the site's front page when the address this run chose for itself opened nothing.
    ///
    /// A start page worked out from the task is a guess at an address, and a guess can name a path the site does not
    /// serve. Only the path was ever in doubt, so the front page of the site already chosen is where to be. A start
    /// page the caller gave is left alone. Returns whether it went there.
    pub(super) async fn front_page_if_blank(&self, opened: &str) -> Step<bool> {
        if matches!(path_of(opened).as_str(), "" | "/") {
            return Ok(false);
        }
        let observation = self.observe().await?;
        if drew_something(&observation) {
            return Ok(false);
        }
        // A page built by script is observable before it draws: navigation returns at `readyState`, and hydration
        // follows, so a deep route that was right is not abandoned for being slow.
        if self.outwait(&observation).await? && drew_something(&self.observe().await?) {
            return Ok(false);
        }
        let front = format!("{}://{}", scheme_of(opened), netloc_of(opened));
        self.event("start_page_blank", || {
            Map::from_iter([("proposed".into(), opened.into()), ("front".into(), front.clone().into())])
        });
        self.page.navigate(&front, None).await?;
        Ok(true)
    }

    /// Open a direct address for the task on `start`'s site when one is proposed in time, else `start`.
    ///
    /// The shortcut is opened in place of the start page, not after it: loading github.com's front page only to leave
    /// it cost 1 to 4 seconds of every shortcut run. The start page stays one BACK away all the same.
    pub(super) async fn open(
        &self,
        task: &str,
        start: &str,
        ledger: &std::sync::Arc<fastbrowse_core::telemetry::Ledger>,
        proposing: Option<std::sync::Arc<super::state::Spawned<crate::shortcut::Shortcut>>>,
    ) -> Step<(Vec<HistoryEntry>, HashSet<String>)> {
        let proposing = match proposing {
            Some(proposing) => Some(proposing),
            None if super::shortcut_worth_proposing(start) => {
                Some(std::sync::Arc::new(super::propose(self.llm.clone(), task, start, ledger.clone())))
            }
            // The caller named the page; going elsewhere on its origin is not the run's to propose.
            None => None,
        };
        let proposal = match proposing {
            None => None,
            Some(proposing) => {
                // Cancelling a dispatched shortcut loses its bill and stops capped runs; the head start owns cleanup.
                let waited =
                    tokio::time::timeout(Duration::from_secs_f64(SHORTCUT_WAIT_SECONDS), proposing.wait()).await;
                match waited {
                    Ok(Some(Ok(proposal))) => Some(proposal),
                    Ok(Some(Err(error))) => match Flow::from(error) {
                        flow if flow.is_llm() => None,
                        flow => return Err(flow),
                    },
                    Ok(None) | Err(_) => None,
                }
            }
        };
        let shortcut = proposal.and_then(|proposal| accept(proposal.url.as_deref(), start));
        // A shortcut is a page to start on. One naming a file would save it before any step's download gate could ask.
        let Some(shortcut) = shortcut.filter(|shortcut| !starts_download(&caller_link(shortcut))) else {
            self.page.navigate(start, None).await?;
            return Ok((Vec::new(), HashSet::new()));
        };
        let landed = async {
            self.page.navigate(&shortcut, Some(start)).await?;
            // `accept` saw only the proposed address; a redirect can still land on another site.
            let landed = self.page.address().await?;
            let status = self.page.response_status().await?;
            Ok::<_, BrowserError>((landed, status))
        }
        .await;
        let (landed, status) = match landed {
            Ok((landed, status)) => (Some(landed), status),
            Err(error) if error.needs_browser() => return Err(error.into()),
            Err(_) => (None, None),
        };
        let Some(landed) = landed.filter(|landed| origin_of(landed) == origin_of(start)) else {
            self.page.navigate(start, None).await?;
            return Ok((Vec::new(), HashSet::new()));
        };
        if status.is_some_and(|status| status >= 400) {
            // A proposed address is a guess, and a guess can name a path the site does not serve: books-mystery opened
            // `mysteryfile_3/`, read a 404 and spent a BACK leaving it, every run. The start page is known good.
            self.page.navigate(start, None).await?;
            return Ok((Vec::new(), HashSet::new()));
        }
        let note = format!("opened {shortcut} directly instead of the start page {start}, which is one BACK away");
        let opened = HistoryEntry { note: Some(note), ..HistoryEntry::new(None, None, StepOutcome::Executed, true) };
        // The facts cite where the page landed, so a redirect's address is the one they can be matched against.
        Ok((vec![opened], HashSet::from([shortcut, landed])))
    }

    /// Reload a bot check that offers nothing to act on, once per address.
    ///
    /// Keyed on the address, not the document: the reload mints a new document key, so a document key would let a
    /// challenge that survives its reload be reloaded on every iteration instead of being reported as blocked.
    pub(super) async fn reload_challenge(&self, state: &mut RunState, challenge: &Observation) -> Step<bool> {
        if !state.challenge_reloads.insert(challenge.url.clone()) {
            return Ok(false);
        }
        self.event("challenge_reload", || Map::from_iter([("url".into(), challenge.url.clone().into())]));
        let back_to = if state.invented.contains(&challenge.url) { state.started_url.clone() } else { None };
        match self.page.navigate(&challenge.url, back_to.as_deref()).await {
            Err(error) if error.needs_browser() => return Err(error.into()),
            Err(_) => return Ok(false),
            Ok(()) => {}
        }
        if self.outwait(challenge).await? {
            return Ok(true);
        }
        Ok(self.observe().await?.page_key != challenge.page_key)
    }

    /// Re-observe until the page is no longer `stuck`, returning whether it moved in time.
    ///
    /// The wait is on the page's own lifecycle events, not a fixed interval: a document that is navigating or loading
    /// reports each transition, so a stuck page is re-read the moment it moves. A wait that reports nothing new still
    /// re-reads once before the deadline, so a client-side move is never missed.
    pub(super) async fn outwait(&self, stuck: &Observation) -> Step<bool> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(INTERSTITIAL_SECONDS);
        let mut mark = self.page.lifecycle_mark();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            let moved = self.page.wait_for_lifecycle(mark, remaining.as_secs_f64()).await;
            mark = self.page.lifecycle_mark();
            if self.observe().await?.page_key != stuck.page_key {
                return Ok(true);
            }
            if !moved {
                return Ok(false);
            }
        }
    }

    pub(super) async fn settle_blank_opening(&self, state: &mut RunState, observation: &Observation) -> Step<bool> {
        if state.blank_opening_checked || state.history.iter().any(|entry| entry.operation.is_some()) {
            return Ok(false);
        }
        state.blank_opening_checked = true;
        if self.outwait(observation).await? {
            return Ok(true);
        }
        // A cold app can leave the first document blank after the loading wait. Reloading once gives its scripts
        // another chance, but doing so after an interaction could discard a form or repeat a submission.
        if state.started_url.is_none()
            || !matches!(scheme_of(&observation.url).as_str(), "http" | "https")
            || observation.response_status.is_some_and(|status| status >= 400)
        {
            return Ok(false);
        }
        self.event("blank_reload", || Map::from_iter([("url".into(), observation.url.clone().into())]));
        let back_to = if state.invented.contains(&observation.url) { state.started_url.clone() } else { None };
        self.page.navigate(&observation.url, back_to.as_deref()).await?;
        Ok(true)
    }

    /// Take one step; whether a duplicate read was skipped, so callers can recover or continue the interaction.
    pub(super) async fn step(
        &self,
        state: &mut RunState,
        observation: &Observation,
        decision: &Decision,
        decided_by: Decider,
        options: StepOptions,
    ) -> Step<bool> {
        if decision.operation == Operation::Click
            && let Some(target) = &decision.target
            && let Some(link) = link_key(observation, target)
            && let Some(failure) = state.failed_links.get(&link)
        {
            return Err(
                Unsure::new(format!("{} failed before: {}", text::repr(&target.label), failure.message())).into()
            );
        }
        let stale_link = decision.target.as_ref().and_then(|target| stale_link_key(observation, target));
        if decision.operation == Operation::Click
            && let Some(stale) = &stale_link
            && state.stale_links.get(stale).copied().unwrap_or(0) >= STALE_LINK_FAILURES
        {
            return Err(Unsure::new(
                "This link repeatedly became stale before input. Use another route to the requested page.",
            )
            .into());
        }
        let started = Instant::now();
        state.form_continues = false;
        if !matches!(decision.operation, Operation::Fill | Operation::Read) && !decision.operation.is_scrolling() {
            state.form_values.clear();
        }
        let facts_before = state.notes.facts().len();
        let reason = if decision.directed { state.hint.clone() } else { None };
        let label = decision_label(decision);
        let mut typed: Option<String> = None;
        let mut effect_now: Option<String> = None;
        let mut setting = false;
        let frames = self.on_event.is_some() && self.config.step_frames;
        let act: ActResult;
        let changed: bool;
        let mut progressed: bool;
        let frame: Option<Vec<u8>>;
        if decision.operation == Operation::Read {
            frame = if frames { self.frame().await? } else { None };
            let capture = match options.capture {
                Some(capture) => capture,
                None => self.capture().await?,
            };
            let (read_progress, skipped) =
                Box::pin(self.read(state, Some(capture), Some(observation), options.require_all_evidence)).await?;
            progressed = read_progress;
            state.read_here = true;
            state.typed_on_passed = false;
            if skipped {
                // Silent cache hits made recovery repeatedly ask to read a page whose content was already read.
                let effect_text = self.redact(&format!(
                    "This content was already read; duplicate read skipped. {}",
                    read_exhausted(state)
                ));
                state.history.push(HistoryEntry {
                    effect: Some(effect_text),
                    ..HistoryEntry::new(Some(Operation::Read), None, StepOutcome::Failed, false)
                });
                return Ok(true);
            }
            changed = false;
            let mut text = String::from("Read this content.");
            if unread(state.plan(), &state.notes) {
                text += &format!(" {}", read_exhausted(state));
            }
            effect_now = Some(text.clone());
            act = ActResult {
                outcome: StepOutcome::Executed,
                page_changed: false,
                detail: Some(text),
                form_unchanged: false,
                moved: false,
            };
        } else {
            let action = match options.prepared_action {
                Some(action) => action,
                None => {
                    let redecided = state.redecided;
                    let target = decision.target.as_ref().map(|target| target.id.clone());
                    let building = self.action(state, observation, decision, decided_by, options.gate);
                    match self.unless_redrawn(redecided, building, observation, target.as_deref()).await? {
                        Watched::Done(action) => action,
                        Watched::Redrawn => {
                            self.redecide(state, observation);
                            return Ok(false);
                        }
                    }
                }
            };
            state.redecided = false;
            if action.operation == Operation::Fill
                && action.text.as_deref().is_none_or(str::is_empty)
                && require(decision.target.as_ref())?.value.as_deref().is_none_or(str::is_empty)
            {
                // Typing nothing into an empty field does nothing. Jev, unsure between wizard steps, picked a page's
                // search box, the writer found no text for it, and the empty fill was taken three times over.
                let named = text::repr(&describe(require(decision.target.as_ref())?));
                return Err(Unsure::new(format!("the task gives no text for {named}: leave it empty")).into());
            }
            // A click or Enter can replace this page before the step is recorded.
            frame = if frames { self.frame().await? } else { None };
            if action.secret {
                // Held from the keystrokes on: a page may mirror the value, and only the next reading shows it.
                self.page.withhold_frames(true);
            }
            let raw = self.raw().unwrap_or_else(|| observation.clone());
            let mut result = if options.follow_pager {
                self.follow_pager(&action, &raw).await?
            } else {
                self.page.act(&action, &raw).await?
            };
            state.form_continues = action.form_fill
                && result.form_unchanged
                && !result.page_changed
                && result.outcome == StepOutcome::Executed;
            if result.outcome == StepOutcome::Stale
                && let Some(target) = &decision.target
                && !action.form_fill
                && let Some(retried) = self.act_on_twin(&action, observation, target).await?
            {
                result = retried;
            }
            if result.outcome == StepOutcome::Covered
                && action.operation == Operation::Click
                && let Some(target) = &decision.target
                && let Some(clicked) = self.click_where_seen(state, target).await?
            {
                result = clicked;
            }
            if action.operation == Operation::Click
                && let Some(stale) = &stale_link
            {
                match result.outcome {
                    StepOutcome::Stale => *state.stale_links.entry(stale.clone()).or_insert(0) += 1,
                    StepOutcome::Executed => {
                        state.stale_links.remove(stale);
                    }
                    _ => {}
                }
            }
            if result.outcome == StepOutcome::Executed && state.authorization.irreversible_actions {
                let question = match &decision.target {
                    Some(target) if fastbrowse_core::safety::may_be_irreversible(decision.operation, Some(target)) => {
                        Some(fastbrowse_core::safety::irreversible_question(
                            &state.task,
                            decision.operation,
                            target,
                            decision.destination.as_ref(),
                        ))
                    }
                    _ if decision.operation == Operation::Dialog && action.accept_dialog == Some(true) => {
                        dialog_question(&state.task, observation.dialog.as_ref())
                    }
                    _ if decision.operation == Operation::Navigate => {
                        decision.url.as_deref().map(|url| navigation_question(&state.task, url))
                    }
                    _ => None,
                };
                if let Some(question) = question {
                    state.transaction_candidates.push(TransactionCandidate {
                        question,
                        from_url: observation.url.clone(),
                        landed_url: None,
                        landed_status: None,
                        committing: None,
                        outcome_read: false,
                    });
                }
            }
            if result.outcome == StepOutcome::Executed
                && let Some(text) = &action.text
            {
                typed = Some(if action.secret { "<secret>".to_string() } else { self.mask(text) });
            }
            changed = result.page_changed;
            if result.outcome == StepOutcome::Executed {
                let typing = matches!(decision.operation, Operation::Fill | Operation::Select) && !changed;
                let here = seen(observation);
                state.typed_on_passed = typing && (state.passed.contains(&here) || state.typed_on_passed);
                if !state.read_here {
                    state.passed.insert(here.clone());
                }
                state.acted_on = Some(here);
            }
            state.owes_read = state.owes_read || (result.outcome == StepOutcome::Executed && changed);
            // A value edit answers "was this progress" itself, and its answer beats `changed`: the popup a fill draws
            // IS a page change, so `changed` alone kept crediting the identical re-fill. `changed` decides every other
            // operation.
            let written = state.written.entry(observation.document_key.clone()).or_default();
            let edit = edit_progress(decision, &action, written);
            if result.outcome == StepOutcome::Executed
                && edit.is_some()
                && let Some(target) = &decision.target
            {
                written.insert(target.id.clone());
            }
            progressed = result.outcome == StepOutcome::Executed && edit.unwrap_or(changed);
            if edit == Some(false) && result.outcome == StepOutcome::Executed {
                // The field already held the value, so this step moved nothing and cannot count: three of them trip
                // the stall budget. A run that went back to a credentials form re-entered the password it already had
                // exactly this way, and nothing told it the form was ready to submit instead.
                // An edit always has a target, so the label is there; "None" is what Python's f-string would write.
                let named = label.as_deref().unwrap_or("None");
                state.hint = Some(self.redact(&format!(
                    "{named} already holds that value: submit the form, or read the page, instead of typing it again."
                )));
            }
            if progressed
                && decision.operation.is_scrolling()
                && !state.open_answer_outputs.is_empty()
                && state.ready_plan.as_ref().is_some_and(super::helpers::lookup)
                && state.reads.iter().any(|key| key.0 == observation.document_key)
            {
                // A full-page read includes offscreen text, so moving its viewport adds no evidence unless the scroll
                // loads content that the reader has not seen.
                let scrolled = self.capture().await?;
                let read_key = scrolled.read_key();
                if state.reads.iter().any(|key| key.0 == observation.document_key && key.1 == read_key) {
                    progressed = false;
                    effect_now = Some(
                        "Scroll moved the viewport over content already read in full; no new source content.".into(),
                    );
                }
            }
            // Moving between two pages changes the page every time, and a run went round "open the author, back to
            // the list" to its step limit with its stall budget reset at every hop. The same action from the same page
            // a third time is going round, not forward.
            let key = signature(decision, observation);
            let count = {
                let attempt = state.attempts.entry(key.clone()).or_default();
                attempt.count += 1;
                attempt.count
            };
            if !decision.operation.is_scrolling() && count > REPEATS_BEFORE_CYCLE {
                progressed = false;
            }
            state.acted_from = Some(observation.clone());
            state.pending_link = match &decision.target {
                Some(target)
                    if changed && result.outcome == StepOutcome::Executed && decision.operation == Operation::Click =>
                {
                    link_key(observation, target)
                }
                _ => None,
            };
            if result.outcome == StepOutcome::Executed
                && let Some(target) = &decision.target
                && SETTING_ROLES.contains(&target.role.as_str())
            {
                // Choosing an option has an intended effect to check: a menu that closed without the value changing
                // still changes the page, and Google Flights' "One way" was clicked to the step limit.
                let landed = self.observe().await?;
                let done = effect(observation, &landed, Some(target));
                state.acted_from = None;
                setting = done.set_something;
                if !done.set_something {
                    progressed = false;
                    result.detail = Some(format!("no effect: {}", done.summary));
                }
                effect_now = Some(done.summary);
            }
            act = result;
        }
        if changed {
            state.read_here = false;
        }
        state.history.push(HistoryEntry {
            text: typed,
            effect: effect_now,
            setting: setting.then_some(true),
            read_progress: decision.operation == Operation::Read && progressed,
            ..HistoryEntry::new(Some(decision.operation), label.as_deref(), act.outcome, changed)
        });
        let note: Vec<&str> =
            [reason.as_deref(), act.detail.as_deref()].into_iter().flatten().filter(|part| !part.is_empty()).collect();
        let note = self.redact(&note.join("\n"));
        let facts: Vec<_> = state.notes.facts()[facts_before.min(state.notes.facts().len())..]
            .iter()
            .map(|fact| self.public_fact(fact))
            .collect();
        let step = StepResult {
            index: state.steps.len() as i64,
            operation: decision.operation,
            decided_by,
            outcome: act.outcome,
            url: observation.url.clone(),
            target: label,
            confidence: (decided_by == Decider::Jev).then(|| decision.confidence()),
            note: (!note.is_empty()).then_some(note),
            facts,
            page_changed: (decision.operation != Operation::Read).then_some(changed),
            duration_ms: started.elapsed().as_millis() as i64,
        };
        self.record_step(state, step, false, frame).await?;
        // The one stall rule: a run is stuck when its page has not changed for `unchanged_actions` steps.
        if progressed {
            state.unchanged = 0;
            state.hint = None;
            state.idle.clear();
        } else {
            state.unchanged += 1;
            if decision.operation != Operation::Read && act.outcome != StepOutcome::Stale {
                state.idle.insert(signature(decision, observation));
            }
        }
        if state.unchanged >= self.config.stall.unchanged_actions {
            let reason = format!("no_progress ({})", state.unchanged);
            Box::pin(self.recover(state, observation, &reason, Status::Stuck, withheld(decision))).await?;
            state.form_continues = false;
        }
        Ok(false)
    }

    pub(super) async fn follow_pager(&self, action: &Action, observation: &Observation) -> Step<ActResult> {
        let target = super::helpers::next_page_control(observation);
        let stale = ActResult {
            outcome: StepOutcome::Stale,
            page_changed: false,
            detail: None,
            form_unchanged: false,
            moved: false,
        };
        let Some(target) = target else { return Ok(stale) };
        if action.operation != Operation::Click
            || action.target_id.as_deref() != Some(target.id.as_str())
            || self.page.address().await? != observation.url
        {
            return Ok(stale);
        }
        // A known pager address needs no pointer events, whose guards and settling cost a cloud round trip each.
        let href = target.href.as_deref().unwrap_or_default();
        let url = join(&observation.url, href).unwrap_or_else(|| href.to_string());
        self.page.navigate(&url, None).await?;
        Ok(ActResult {
            outcome: StepOutcome::Executed,
            page_changed: self.page.address().await? != observation.url,
            detail: Some("Followed next-page link by URL.".into()),
            form_unchanged: false,
            moved: false,
        })
    }

    /// Act on the one control now standing where `target` stood, if the page redrew it since it was observed.
    ///
    /// A control that is the only one retaining its guard and semantics in the same document can inherit that decision.
    /// A new document at the same address, or a changed form, has not passed the original authorization gate.
    pub(super) async fn act_on_twin(
        &self,
        action: &Action,
        observation: &Observation,
        target: &Control,
    ) -> Step<Option<ActResult>> {
        // A drag's authorization covers both ends; a source twin cannot authorize a changed destination.
        if action.operation == Operation::Drag {
            return Ok(None);
        }
        let fresh = self.observe().await?;
        if observation.document_key.is_empty()
            || target.retarget_key.as_deref().is_none_or(str::is_empty)
            || fresh.document_key != observation.document_key
            || fresh.url != observation.url
            || fresh.title != observation.title
        {
            return Ok(None);
        }
        let twins: Vec<&Control> = fresh
            .controls
            .iter()
            .filter(|control| {
                Control { id: target.id.clone(), offscreen: target.offscreen, ..(*control).clone() } == *target
            })
            .collect();
        if twins.len() != 1 {
            return Ok(None);
        }
        self.event("retarget", || Map::from_iter([("target".into(), self.redact(&target.label).into())]));
        let retargeted = Action { target_id: Some(twins[0].id.clone()), ..action.clone() };
        let raw = self.raw().unwrap_or(fresh);
        Ok(Some(self.page.act(&retargeted, &raw).await?))
    }

    /// Note the page state reached, renewing the recovery budget when it is new.
    ///
    /// Recoveries are spent on being stuck, not on the whole run, so a page state never seen before restores the
    /// budget.
    pub(super) fn settle(&self, state: &mut RunState, observation: &Observation) {
        let renews = !redrawn_read(state, observation);
        let key = state_key(observation);
        let first = state.reached.get(&key).copied();
        let evidence = if state
            .ready_plan
            .as_ref()
            .is_some_and(|plan| plan.requirements.iter().any(|r| r.kind == RequirementKind::Action))
        {
            answer_evidence(&state.notes, false)
        } else {
            BTreeSet::new()
        };
        let prior = state.reached_with_evidence.get(&key).cloned().unwrap_or_else(|| evidence.clone());
        let new_context = evidence.iter().any(|item| !prior.contains(item));
        state.reached_with_evidence.insert(key.clone(), prior.union(&evidence).cloned().collect());
        if first.is_none() {
            if let Some(failure) = &state.http_failure
                && ((observation.response_status.is_none() && observation.url != failure.url)
                    || observation.response_status.is_some_and(|status| (200..400).contains(&status)))
            {
                state.http_failure = None;
            }
            state.reached.insert(key, state.history.len());
        }
        // Returning to a form with a newly read prerequisite is progress once for that state and those sources.
        if (first.is_none() || new_context)
            && renews
            && (state.open_answer_outputs.is_empty()
                || Some(answer_evidence(&state.notes, true)) != state.rejected_answer_evidence)
        {
            state.recoveries = 0;
            state.recovery_log.clear();
        }
    }

    /// Record on the last action what it did, which the next choice and recovery both read.
    pub(super) fn note_effect(&self, state: &mut RunState, observation: &Observation) {
        if let Some(candidate) = state.transaction_candidates.last_mut()
            && candidate.landed_url.is_none()
        {
            candidate.landed_url = Some(observation.url.clone());
            candidate.landed_status = observation.response_status;
        }
        // An error document has the requested address too, but never proves the destination loaded.
        if observation.response_status.is_some_and(|status| status >= 400) {
            state.visited.shift_remove(&observation.url);
        } else {
            state.visited.insert(observation.url.clone(), ());
        }
        let link = state.pending_link.take();
        if let Some(status) = observation.response_status {
            if status >= 400 {
                let failure = HttpFailure { url: observation.url.clone(), status };
                state.http_failure = Some(failure.clone());
                if let Some(link) = link {
                    state.failed_links.insert(link, failure);
                }
            } else if status >= 200 {
                state.failed_links.retain(|_, failure| failure.url != observation.url);
                if state.http_failure.as_ref().is_some_and(|failure| failure.url == observation.url) {
                    state.http_failure = None;
                }
            }
        }
        let Some(before) = state.acted_from.take() else { return };
        let Some(last) = state.history.last_mut() else { return };
        if last.effect.is_some() {
            return;
        }
        last.effect = Some(effect(&before, observation, None).summary);
    }

    /// Every observation models see has resolved secret values blanked, wherever the page echoes them.
    pub(super) async fn observe(&self) -> Step<Observation> {
        let observation = self.page.observe().await?;
        let revealed = self.reveals(&observation);
        let mut seen = self.seen();
        seen.raw = Some(observation.clone());
        seen.secret_on_screen = revealed;
        let mask = |value: &str| seen.redactor.mask(value);
        let mask_opt = |value: &Option<String>| value.as_deref().map(mask);
        let controls = observation
            .controls
            .iter()
            .map(|control| Control {
                label: mask(&control.label),
                context: mask_opt(&control.context),
                value: mask_opt(&control.value),
                input_name: mask_opt(&control.input_name),
                autocomplete: mask_opt(&control.autocomplete),
                href: mask_opt(&control.href),
                frame_origin: mask_opt(&control.frame_origin),
                submit_semantics: mask_opt(&control.submit_semantics),
                options: control.options.iter().map(|option| mask(option)).collect(),
                ..control.clone()
            })
            .collect();
        let masked = Observation {
            url: mask(&observation.url),
            title: mask(&observation.title),
            viewport_text: mask(&observation.viewport_text),
            controls,
            tabs: observation
                .tabs
                .iter()
                .map(|tab| Tab { url: mask(&tab.url), title: mask(&tab.title), ..tab.clone() })
                .collect(),
            dialog: observation.dialog.as_ref().map(|dialog| Dialog {
                message: mask(&dialog.message),
                default_prompt: mask_opt(&dialog.default_prompt),
                ..dialog.clone()
            }),
            ..observation.clone()
        };
        seen.observed = Some(masked.clone());
        Ok(masked)
    }

    /// The challenge's own words, for a blocked result: a caller left only the final address could not tell a CAPTCHA
    /// from a rate limit or an outage. A failed capture loses only the quote.
    pub(super) async fn wall_quotes(&self, wall: &Observation) -> Vec<fastbrowse_core::models::Evidence> {
        let quoted = async {
            let capture = self.capture().await?;
            // A scripted check can clear just after it was outwaited, and the capture then reads its destination; the
            // page is observed again so only the wall's own words are cited.
            if self.observe().await?.page_key != wall.page_key {
                return Ok::<_, Flow>(Vec::new());
            }
            Ok(page_quotes(&capture))
        };
        quoted.await.unwrap_or_default()
    }

    pub(super) async fn capture(&self) -> Step<Capture> {
        let capture = self.page.capture().await?;
        let seen = self.seen();
        let mask = |value: &str| seen.redactor.mask(value);
        let text = mask(&capture.text);
        let digest = sha256_hex(text.as_bytes());
        let blocks = capture
            .blocks
            .iter()
            .map(|block| fastbrowse_core::page::Block {
                heading_path: block.heading_path.iter().map(|heading| mask(heading)).collect(),
                control_context: block.control_context.as_ref().map(|context| fastbrowse_core::models::SourceControl {
                    role: mask(&context.role),
                    label: mask(&context.label),
                    context: context.context.as_deref().map(mask),
                }),
                href: block.href.as_deref().map(mask),
                source_url: block.source_url.as_deref().map(mask),
                ..block.clone()
            })
            .collect();
        Ok(Capture {
            text,
            url: mask(&capture.url),
            title: mask(&capture.title),
            sha256: digest,
            blocks,
            ..capture.clone()
        })
    }

    /// The viewport for Jev's action decision, on a route that takes images and only within the sizes it accepts.
    pub(super) async fn viewport(&self) -> Step<Option<Vec<u8>>> {
        if !self.jev.profile().images {
            return Ok(None);
        }
        Ok(self.screenshots().await?.into_iter().find(|png| jev_takes_png(png)))
    }

    /// No image while a secret shows as page text: pixels cannot be masked like text. Typed fields are masked by the
    /// page itself.
    pub(super) async fn screenshots(&self) -> Step<Vec<Vec<u8>>> {
        if self.seen().secret_on_screen {
            return Ok(Vec::new());
        }
        match self.page.screenshot().await {
            Ok(image) => Ok(vec![image]),
            Err(BrowserError::ScreenshotsUnavailable(_)) => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) async fn record_step(
        &self,
        state: &mut RunState,
        step: StepResult,
        capture_frame: bool,
        frame: Option<Vec<u8>>,
    ) -> Step {
        state.steps.push(step.clone());
        // A stale step dispatched nothing; the stall budget bounds redraw loops.
        if step.outcome != StepOutcome::Stale {
            state.ledger.state().steps += 1;
        }
        let Some(on_event) = &self.on_event else { return Ok(()) };
        let frame = if capture_frame && self.config.step_frames { self.frame().await? } else { frame };
        on_event(Event::Step(StepEvent { kind: Default::default(), step, frame: frame.map(Image) })).await;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn record_failure(
        &self,
        state: &mut RunState,
        observation: &Observation,
        operation: Operation,
        reason: &str,
        target: Option<String>,
        confidence: Option<f64>,
        page_changed: Option<bool>,
        decided_by: Decider,
    ) -> Step {
        let step = StepResult {
            index: state.steps.len() as i64,
            operation,
            decided_by,
            outcome: StepOutcome::Failed,
            url: observation.url.clone(),
            target,
            confidence,
            note: Some(reason.to_string()),
            facts: Vec::new(),
            page_changed,
            duration_ms: 0,
        };
        self.record_step(state, step, true, None).await
    }

    /// A PNG of the page as it is now, or None while a resolved secret is showing on it.
    ///
    /// The check is made against a fresh reading of the page rather than the last observation, which was taken before
    /// the action ran, and the action may be the one that put the secret there.
    pub(super) async fn frame(&self) -> Step<Option<Vec<u8>>> {
        let now = self.page.observe().await?;
        if self.reveals(&now) {
            return Ok(None);
        }
        match self.page.screenshot().await {
            Ok(image) => Ok(Some(image)),
            Err(BrowserError::ScreenshotsUnavailable(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Whether a resolved secret shows on the page; live and recorded frames are held back while one does.
    pub(super) fn reveals(&self, observation: &Observation) -> bool {
        // A dialog hides the page text, so its observation cannot prove the pixels contain no secret.
        let revealed = observation.dialog.is_some() || self.reveals_text(&observation.dump_json());
        self.page.withhold_frames(revealed);
        revealed
    }
}

/// `_step`'s keyword arguments.
pub(super) struct StepOptions {
    pub capture: Option<Capture>,
    pub gate: bool,
    pub require_all_evidence: bool,
    pub follow_pager: bool,
    pub prepared_action: Option<Action>,
}

impl Default for StepOptions {
    fn default() -> Self {
        StepOptions {
            capture: None,
            gate: true,
            require_all_evidence: false,
            follow_pager: false,
            prepared_action: None,
        }
    }
}

/// Rewriting the observed field value vetoes progress even when the page changes, and the first new value a field
/// receives is progress even when the page does not: a page's fingerprint ignores field values, so a three-field
/// checkout form counted three unchanged steps and tripped the stall recovery every run.
pub(super) fn edit_progress(decision: &Decision, action: &Action, written: &HashSet<String>) -> Option<bool> {
    if !matches!(decision.operation, Operation::Fill | Operation::Select) {
        return None;
    }
    let target = decision.target.as_ref()?;
    let text = action.text.as_ref()?;
    // A secret's value never leaves the page; only its length is observed, so that is what to compare.
    let held = if action.secret { "\u{2022}".repeat(text.chars().count()) } else { text.clone() };
    if target.value.as_deref() == Some(held.as_str()) {
        return Some(false);
    }
    if written.contains(&target.id) { None } else { Some(true) }
}

pub(super) fn require(target: Option<&Control>) -> Step<&Control> {
    target.ok_or_else(|| {
        Flow::from(fastbrowse_core::decision::DecisionError::Failed(
            "operation needs a target and Jev offered none".into(),
        ))
    })
}
