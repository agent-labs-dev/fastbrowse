//! The loop and its step: observe, judge what the last action did, decide, gate and act.

use std::collections::HashSet;
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
    caller_link, decision_label, describe, dialog_question, drew_something, follow_recovery, join, navigation_question,
    netloc_of, not_acting, paging, path_of, read_exhausted, scheme_of, unread,
};
use super::recovery::{Withheld, withheld};
use super::state::{Flow, HttpFailure, RunState, Step, Stop, TransactionCandidate, Unsure};
use super::{Agent, INTERSTITIAL_SECONDS, REDRAW_WATCH_SECONDS, SHORTCUT_WAIT_SECONDS};
use crate::policy::{Decision, HistoryEntry, StepContext, decide};
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
            // An error page is Jev's to act on, told its status in the step's page facts. Code only waits out a
            // status that can clear by itself, once per document, and stops a list walk that reached one.
            if let Some(status) = observation.response_status
                && status >= 400
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
                if state.http_seen.insert(document) {
                    if matches!(status, 403 | 429 | 503)
                        && self.outwait(&self.raw().unwrap_or_else(|| observation.clone())).await?
                    {
                        continue;
                    }
                    state.paging_failed |= state.next_page || state.paged_from.is_some();
                    state.next_page = false;
                }
            }
            self.settle(state, &observation).await?;
            // Walking a list is dispatched before Jev is asked, because not asking is the point: the reader asked for
            // the rest of the list and the link is already found, so a decision would buy nothing. It therefore passes
            // none of the gates below, which is safe only because of what it can be: a pager link to another address.
            if state.next_page && state.through_end && !state.paging_failed {
                Box::pin(self.pipeline_pages(state, &observation)).await?;
                continue;
            }
            if let Some(decision) = paging(state, &observation) {
                let options = StepOptions { gate: false, ..StepOptions::default() };
                Box::pin(self.step(state, &observation, &decision, Decider::Code, options)).await?;
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
                    let context = self.context(state, &observation, secrets, check_login, fresh)?;
                    let redecided = state.redecided;
                    let deciding = self.decide_looking(state, &observation, &context);
                    let decided = self.unless_redrawn(redecided, deciding, &observation, None).await?;
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
                return Err(self.meet_wall(&observation, &raw));
            }
            let mut uncertain = decision.confidence() < self.config.thresholds.recover_below;
            if let Some(directed) = follow_recovery(state, &observation, &decision, uncertain) {
                (decision, uncertain, decided_by) = (directed, false, Decider::Llm);
            }
            if uncertain && state.ready_plan.is_none() {
                // Unsure without the requirements: the plan is already in flight and costs less than recovery.
                state.await_plan().await?;
                continue;
            }
            if decision.operation == Operation::Read {
                state.await_plan().await?;
                Box::pin(self.step(state, &observation, &decision, decided_by, StepOptions::default())).await?;
                continue;
            }
            // The confidence gate exists to stop the agent acting on a page it does not understand. READ and DONE do
            // not act: a read changes nothing, and DONE is judged again by `finish`.
            if decision.operation == Operation::Escalate || (uncertain && !not_acting(decision.operation)) {
                let reason = format!("uncertain next step ({:.2})", decision.confidence());
                Box::pin(self.consult(state, &observation, &reason)).await?;
                continue;
            }
            if decision.operation == Operation::Done {
                if let Some(result) = Box::pin(self.finish(state, output_schema, until)).await? {
                    return Ok(result);
                }
                continue;
            }
            match Box::pin(self.step(state, &observation, &decision, decided_by, StepOptions::default())).await {
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

    /// Take one step and judge whether it moved the run on, which the one stall rule counts.
    pub(super) async fn step(
        &self,
        state: &mut RunState,
        observation: &Observation,
        decision: &Decision,
        decided_by: Decider,
        options: StepOptions,
    ) -> Step {
        let started = Instant::now();
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
        // None for a page change, which the next observation judges by the page state it lands on.
        let progressed: Option<bool>;
        let frame: Option<Vec<u8>>;
        if decision.operation == Operation::Read {
            frame = if frames { self.frame().await? } else { None };
            let capture = match options.capture {
                Some(capture) => capture,
                None => self.capture().await?,
            };
            let (read_progress, skipped) =
                Box::pin(self.read(state, Some(capture), Some(observation), options.require_all_evidence)).await?;
            progressed = Some(read_progress);
            if read_progress {
                renew(state);
            }
            changed = false;
            let text = if skipped {
                format!("This content was already read; duplicate read skipped. {}", read_exhausted(state))
            } else if unread(state.plan(), &state.notes) {
                format!("Read this content. {}", read_exhausted(state))
            } else {
                "Read this content.".to_string()
            };
            let text = self.redact(&text);
            effect_now = Some(text.clone());
            act = ActResult {
                outcome: if skipped { StepOutcome::Failed } else { StepOutcome::Executed },
                page_changed: false,
                detail: Some(text),
                moved: false,
            };
        } else {
            let redecided = state.redecided;
            let target = decision.target.as_ref().map(|target| target.id.clone());
            let building = self.action(state, observation, decision, decided_by, options.gate);
            let action = match self.unless_redrawn(redecided, building, observation, target.as_deref()).await? {
                Watched::Done(action) => action,
                Watched::Redrawn => {
                    self.redecide(state, observation);
                    return Ok(());
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
            if result.outcome == StepOutcome::Stale
                && let Some(target) = &decision.target
                && let Some(retried) = self.act_on_twin(&action, observation, target).await?
            {
                result = retried;
            }
            if result.outcome == StepOutcome::Covered
                && action.operation == Operation::Click
                && let Some(target) = &decision.target
                && let Some(clicked) = self.click_where_seen(state, &raw, target).await?
            {
                result = clicked;
            }
            let executed = result.outcome == StepOutcome::Executed;
            if executed && state.authorization.irreversible_actions {
                let question = match &decision.target {
                    Some(target) if fastbrowse_core::safety::may_be_irreversible(decision.operation) => {
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
            if executed && let Some(text) = &action.text {
                typed = Some(if action.secret { "<secret>".to_string() } else { self.mask(text) });
            }
            changed = result.page_changed;
            if executed
                && changed
                && let Some(read) = state.read_documents.get_mut(&observation.document_key)
            {
                *read = true;
            }
            // A value edit answers "was this progress" itself, and its answer beats `changed`: the popup a fill draws
            // IS a page change, so `changed` alone kept crediting the identical re-fill. `changed` decides every other
            // operation, by the page state it lands on.
            let written = state.written.entry(observation.document_key.clone()).or_default();
            let edit = edit_progress(decision, &action, written);
            if executed
                && edit.is_some()
                && let Some(target) = &decision.target
            {
                written.insert(target.id.clone());
            }
            let mut judged = match edit {
                _ if !executed => Some(false),
                Some(edited) => Some(edited),
                None => (!changed).then_some(false),
            };
            if edit == Some(false) && executed {
                // The field already held the value, so this step moved nothing and cannot count: three of them trip
                // the stall budget. A run that went back to a credentials form re-entered the password it already had
                // exactly this way, and nothing told it the form was ready to submit instead.
                // An edit always has a target, so the label is there; "None" is what Python's f-string would write.
                let named = label.as_deref().unwrap_or("None");
                state.hint = Some(self.redact(&format!(
                    "{named} already holds that value: submit the form, or read the page, instead of typing it again."
                )));
            }
            state.acted_from = Some(observation.clone());
            if executed
                && let Some(target) = &decision.target
                && SETTING_ROLES.contains(&target.role.as_str())
            {
                // Choosing an option has an intended effect to check: a menu that closed without the value changing
                // still changes the page, and Google Flights' "One way" was clicked to the step limit.
                let landed = self.observe().await?;
                let done = effect(observation, &landed, Some(target));
                state.acted_from = None;
                setting = done.set_something;
                judged = Some(done.set_something);
                if !done.set_something {
                    result.detail = Some(format!("no effect: {}", done.summary));
                }
                effect_now = Some(done.summary);
            }
            progressed = judged;
            act = result;
        }
        state.history.push(HistoryEntry {
            text: typed,
            effect: effect_now,
            setting: setting.then_some(true),
            read_progress: decision.operation == Operation::Read && progressed == Some(true),
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
        match progressed {
            Some(progressed) => {
                self.count_progress(state, progressed, observation, withheld(decision), Status::Stuck).await
            }
            None => {
                state.landing = Some(withheld(decision));
                Ok(())
            }
        }
    }

    /// The one stall rule: `stall.unchanged_actions` steps in a row that moved nothing ask Jev again with the stalled
    /// action left out. A step moves the run on when it gathers evidence, writes a new value, or reaches a page state
    /// not seen since the last evidence.
    pub(super) async fn count_progress(
        &self,
        state: &mut RunState,
        progressed: bool,
        observation: &Observation,
        stalled: Withheld,
        gives_up_as: Status,
    ) -> Step {
        if progressed {
            state.unchanged = 0;
            state.hint = None;
            return Ok(());
        }
        state.unchanged += 1;
        if state.unchanged >= self.config.stall.unchanged_actions {
            let reason = format!("no_progress ({})", state.unchanged);
            Box::pin(self.recover(state, observation, &reason, gives_up_as, stalled)).await?;
        }
        Ok(())
    }

    pub(super) async fn follow_pager(&self, action: &Action, observation: &Observation) -> Step<ActResult> {
        let target = super::helpers::next_page_control(observation);
        let stale = ActResult { outcome: StepOutcome::Stale, page_changed: false, detail: None, moved: false };
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

    /// Note the page state reached and judge the last page change by it: a state never seen, or one seen before the
    /// last evidence was gathered, is progress. A state never seen renews the recovery budget, which is spent on being
    /// stuck, not on the whole run.
    pub(super) async fn settle(&self, state: &mut RunState, observation: &Observation) -> Step {
        let last = state.reached.insert(state_key(observation), state.evidence_reads);
        if last.is_none() {
            if let Some(failure) = &state.http_failure
                && ((observation.response_status.is_none() && observation.url != failure.url)
                    || observation.response_status.is_some_and(|status| (200..400).contains(&status)))
            {
                state.http_failure = None;
            }
            renew(state);
        }
        if let Some(stalled) = state.landing.take() {
            let progressed = last.is_none_or(|seen_at| seen_at < state.evidence_reads);
            Box::pin(self.count_progress(state, progressed, observation, stalled, Status::Stuck)).await?;
        }
        Ok(())
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
        if let Some(status) = observation.response_status {
            if status >= 400 {
                state.http_failure = Some(HttpFailure { url: observation.url.clone(), status });
            } else if status >= 200 && state.http_failure.as_ref().is_some_and(|failure| failure.url == observation.url)
            {
                state.http_failure = None;
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
    /// Jev reads the page as text first, and sees it only when that leaves its pick under the bar an action is held
    /// to: a second look costs less than the LLM recovery the unsure pick would go to.
    pub(super) async fn decide_looking(
        &self,
        state: &RunState,
        observation: &Observation,
        context: &StepContext,
    ) -> Step<Decision> {
        let (ledger, cache, trace) = (Some(&*state.ledger), Some(&state.relevance_cache), &self.trace);
        let read = decide(&*self.jev, observation, context, &self.config, ledger, cache, trace).await?;
        if read.confidence() >= self.config.thresholds.recover_below {
            return Ok(read);
        }
        let Some(png) = self.viewport().await? else { return Ok(read) };
        let mut seen = context.clone();
        seen.screenshot = Some(png);
        let mut looked = decide(&*self.jev, observation, &seen, &self.config, ledger, cache, trace).await?;
        looked.cost = read.cost.into_iter().chain(looked.cost).collect();
        looked.input_tokens += read.input_tokens;
        Ok(looked)
    }

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

/// A new page state or new evidence restores the recovery budget.
fn renew(state: &mut RunState) {
    state.recoveries = 0;
    state.recovery_log.clear();
    state.gave_up = None;
}

/// `_step`'s keyword arguments.
pub(super) struct StepOptions {
    pub capture: Option<Capture>,
    pub gate: bool,
    pub require_all_evidence: bool,
    pub follow_pager: bool,
}

impl Default for StepOptions {
    fn default() -> Self {
        StepOptions { capture: None, gate: true, require_all_evidence: false, follow_pager: false }
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
