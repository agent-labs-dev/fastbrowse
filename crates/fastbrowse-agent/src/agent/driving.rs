//! The loop and its step: observe, judge what the last action did, decide, gate and act.

use std::collections::HashSet;
use std::future::Future;
use std::time::{Duration, Instant};

use fastbrowse_core::decision::{EVEN_ODDS, jev_takes_png};
use fastbrowse_core::effects::{SETTING_ROLES, effect, state_key};
use fastbrowse_core::models::{
    Decider, Event, Image, Operation, Status, StepChoice, StepEvent, StepOutcome, StepResult, WallKind,
};
use fastbrowse_core::page::{ActResult, Action, BrowserError, Capture, Control, Dialog, Observation, Tab};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::sha256_hex;
use fastbrowse_core::py::text;
use fastbrowse_core::safety::origin_of;
use serde_json::{Map, Value, json};

use super::helpers::{decision_label, describe, drew_something, netloc_of, path_of, scheme_of};
use super::recovery::{Withheld, withheld};
use super::state::{FailureCause, Flow, RunState, SiteFailure, Step, Stop, Unsure};
use super::{Agent, INTERSTITIAL_SECONDS, REDRAW_WATCH_SECONDS};
use crate::policy::{Decision, HistoryEntry, decide};
use crate::retrieval::page_quotes;
use crate::start::{accept_start, propose_start};
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
                if state.http_seen.insert(document)
                    && matches!(status, 403 | 429 | 503)
                    && self.outwait(&self.raw().unwrap_or_else(|| observation.clone())).await?
                {
                    continue;
                }
            }
            self.settle(state, &observation).await?;
            let raw = self.raw().unwrap_or_else(|| observation.clone());
            if !drew_something(&raw) && self.settle_blank_opening(state, &raw).await? {
                continue;
            }
            let origin = origin_of(&raw.url);
            let page = (raw.url.clone(), raw.document_key.clone());
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
            let decision = match picked {
                Some(picked) => picked,
                None => {
                    let context = self.context(state, &observation, secrets, check_login, fresh)?;
                    let redecided = state.redecided;
                    let deciding = self.trace.phase("decide", async {
                        let (ledger, cache) = (Some(&*state.ledger), Some(&state.relevance_cache));
                        let config = &self.config;
                        Ok(decide(&*self.jev, &observation, &context, config, ledger, cache, &self.trace).await?)
                    });
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
            self.record_page(state, &observation);
            state.choices = self.choices(&decision);
            judge_site(state, &observation, &decision);
            // A page can say it failed and still show the answer, as one with a stale Loading line does: the likelier
            // of Jev's two readings stands.
            state.page_failed = decision
                .site_failure
                .is_some_and(|failed| failed >= EVEN_ODDS && failed > decision.shows_answer.unwrap_or_default());
            // Going back from a commit that failed sends it again.
            let committed = std::mem::take(&mut state.committing);
            if committed
                && state.page_failed
                && let Some(failure) = &state.site_failure
            {
                return Err(failure.stop().into());
            }
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
            // Jev's operation can leave a page, or end the run, before anything it shows has been read: an order total
            // is gone once the order is placed. Jev has said the page shows what the task needs, so the read is not
            // asked again whether it does: the page of a list judged absent was a third of the list lost.
            if decision.operation != Operation::Read
                && decision.shows_answer.is_some_and(|probability| probability > EVEN_ODDS)
            {
                let capture = self.capture().await?;
                self.trace.phase("read", self.read(state, capture, &observation, true)).await?;
            }
            if decision.operation == Operation::Done {
                if let Some(result) =
                    self.trace.phase("finish", Box::pin(self.finish(state, &observation, output_schema, until))).await?
                {
                    return Ok(result);
                }
                continue;
            }
            match Box::pin(self.step(state, &observation, &decision)).await {
                Err(Flow::Unsure(unsure)) => {
                    let left_out = withheld(&decision);
                    let recovering =
                        Box::pin(self.recover(state, &observation, &unsure.reason, unsure.gives_up_as, left_out));
                    self.trace.phase("recover", recovering).await?;
                }
                other => other?,
            }
        }
    }

    /// Jev's offered controls as a step reports them. Their labels come from the masked observation already, and are
    /// masked again so a label that reaches a step can never carry a secret whatever it was read from.
    fn choices(&self, decision: &Decision) -> Vec<StepChoice> {
        let chosen: Vec<&str> = [&decision.target, &decision.destination]
            .into_iter()
            .flatten()
            .map(|control| control.id.as_str())
            .collect();
        decision
            .offered
            .iter()
            .map(|control| StepChoice {
                id: control.id.clone(),
                role: control.role.clone(),
                label: self.mask(&control.label),
                context: control.context.as_deref().map(|context| self.mask(context)),
                operations: control.operations.iter().copied().collect(),
                chosen: chosen.contains(&control.id.as_str()),
            })
            .collect()
    }

    /// What this step showed the run, in the trace, so a run can be checked against the pages it saw.
    pub(super) fn record_page(&self, state: &RunState, observation: &Observation) {
        self.event("page", || {
            let controls: Vec<Value> = observation
                .controls
                .iter()
                .map(|control| json!({"label": control.label, "value": control.value}))
                .collect();
            Map::from_iter([
                ("step".into(), state.steps.len().into()),
                ("url".into(), observation.url.clone().into()),
                ("title".into(), observation.title.clone().into()),
                ("status".into(), observation.response_status.into()),
                ("text".into(), observation.viewport_text.clone().into()),
                ("controls".into(), controls.into()),
            ])
        });
    }

    pub(super) fn redecide(&self, state: &mut RunState, observation: &Observation) {
        self.event("redecide", || Map::from_iter([("url".into(), observation.url.clone().into())]));
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

    /// The page to begin on when the caller named none, proposed from the task.
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

    /// Reload a bot check that offers nothing to act on, once per address.
    ///
    /// Keyed on the address, not the document: the reload mints a new document key, so a document key would let a
    /// challenge that survives its reload be reloaded on every iteration instead of being reported as blocked.
    pub(super) async fn reload_challenge(&self, state: &mut RunState, challenge: &Observation) -> Step<bool> {
        if !state.challenge_reloads.insert(challenge.url.clone()) {
            return Ok(false);
        }
        self.event("challenge_reload", || Map::from_iter([("url".into(), challenge.url.clone().into())]));
        match self.page.navigate(&challenge.url, None).await {
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
        self.page.navigate(&observation.url, None).await?;
        Ok(true)
    }

    /// Take one step and judge whether it moved the run on, which the one stall rule counts.
    pub(super) async fn step(&self, state: &mut RunState, observation: &Observation, decision: &Decision) -> Step {
        let started = Instant::now();
        let facts_before = state.notes.facts().len();
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
            let capture = self.capture().await?;
            let read_progress = self.trace.phase("read", Box::pin(self.read(state, capture, observation, false))).await?;
            progressed = Some(read_progress);
            if read_progress {
                renew(state);
            }
            changed = false;
            let text = if state.notes.answered() {
                "Read this content. The notes hold evidence that answers the task."
            } else {
                "Read this content. The notes hold nothing that answers the task."
            };
            let text = self.redact(text);
            effect_now = Some(text.clone());
            act = ActResult { outcome: StepOutcome::Executed, page_changed: false, detail: Some(text), moved: false };
        } else {
            let redecided = state.redecided;
            let target = decision.target.as_ref().map(|target| target.id.clone());
            let building = self.trace.phase("build_action", self.action(state, observation, decision));
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
                // Typing nothing into an empty field does nothing.
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
            let mut result = self.trace.phase("act", self.page.act(&action, &raw)).await?;
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
            if action.operation == Operation::Navigate
                && result.outcome == StepOutcome::Failed
                && let (Some(url), Some(error)) = (&action.url, &result.detail)
            {
                // The browser retried the address before it gave up, and what it gave is the site failure stated to
                // Jev: it may open the address again, or take another route.
                let since = match &state.site_failure {
                    Some(failure) if failure.url == *url => failure.since,
                    _ => state.steps.len(),
                };
                let cause = FailureCause::Navigation(self.redact(error));
                state.site_failure = Some(SiteFailure { url: url.clone(), cause, since });
            }
            if executed && let Some(text) = &action.text {
                typed = Some(if action.secret { "<secret>".to_string() } else { self.mask(text) });
            }
            changed = result.page_changed;
            // A read takes in the whole page, so scrolling leaves it as read.
            let scrolled = matches!(decision.operation, Operation::Scroll | Operation::ScrollUp);
            if executed
                && changed
                && !scrolled
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
            let judged = match edit {
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
                // A menu that closed without the value changing still changes the page, so what the choice set is
                // stated: Google Flights' "One way" was clicked to the step limit while the history said only "changed".
                let landed = self.observe().await?;
                let done = effect(observation, &landed, Some(target));
                state.acted_from = None;
                setting = done.set_something;
                effect_now = Some(done.summary);
            }
            progressed = judged;
            // What the browser saw the action do (a download, a new tab, why it failed) is stated to Jev beside the
            // page change the next observation finds: a download leaves the page as it was.
            if effect_now.is_none() {
                effect_now = result.detail.as_deref().map(|detail| self.redact(detail));
            }
            act = result;
        }
        state.history.push(HistoryEntry {
            text: typed,
            effect: effect_now,
            setting: setting.then_some(true),
            read_progress: decision.operation == Operation::Read && progressed == Some(true),
            ..HistoryEntry::new(Some(decision.operation), label.as_deref(), act.outcome, changed)
        });
        if let Some(value) = state.history.last().and_then(|entry| entry.text.clone()) {
            self.event("typed", || {
                Map::from_iter([("step".into(), state.steps.len().into()), ("value".into(), value.into())])
            });
        }
        let note = self.redact(act.detail.as_deref().unwrap_or_default());
        let facts: Vec<_> = state.notes.facts()[facts_before.min(state.notes.facts().len())..]
            .iter()
            .map(|fact| self.public_fact(fact))
            .collect();
        let step = StepResult {
            index: state.steps.len() as i64,
            operation: decision.operation,
            decided_by: Decider::Jev,
            outcome: act.outcome,
            url: observation.url.clone(),
            target: label,
            confidence: Some(decision.confidence()),
            note: (!note.is_empty()).then_some(note),
            facts,
            page_changed: (decision.operation != Operation::Read).then_some(changed),
            duration_ms: started.elapsed().as_millis() as i64,
            choices: Vec::new(),
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
        self.event("retarget", || Map::from_iter([("target".into(), target.label.clone().into())]));
        let retargeted = Action { target_id: Some(twins[0].id.clone()), ..action.clone() };
        let raw = self.raw().unwrap_or(fresh);
        Ok(Some(self.page.act(&retargeted, &raw).await?))
    }

    /// Note the page state reached and judge the last page change by it: a state never seen, or one seen before the
    /// last evidence was gathered, is progress. A state never seen renews the recovery budget.
    pub(super) async fn settle(&self, state: &mut RunState, observation: &Observation) -> Step {
        let last = state.reached.insert(state_key(observation), state.evidence_reads);
        if last.is_none() {
            if let Some(failure) = &state.site_failure
                && (observation.url != failure.url
                    || (failure.cause != FailureCause::Reported
                        && observation.response_status.is_some_and(|status| (200..400).contains(&status))))
                && observation.response_status.is_none_or(|status| status < 400)
            {
                state.site_failure = None;
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
        // An error document has the requested address too, but never proves the destination loaded.
        if observation.response_status.is_some_and(|status| status >= 400) {
            state.visited.shift_remove(&observation.url);
        } else {
            state.visited.insert(observation.url.clone(), ());
        }
        if let Some(status) = observation.response_status {
            if status >= 400 {
                let since = match &state.site_failure {
                    Some(failure) if failure.url == observation.url => failure.since,
                    _ => state.steps.len(),
                };
                state.site_failure = Some(SiteFailure::http(&observation.url, status, since));
            } else if status >= 200
                && state.site_failure.as_ref().is_some_and(|failure| {
                    failure.url == observation.url && !matches!(failure.cause, FailureCause::Reported)
                })
            {
                // A page that loads can still report the site failed; only Jev, reading it, clears that.
                state.site_failure = None;
            }
        }
        let Some(before) = state.acted_from.take() else { return };
        let Some(last) = state.history.last_mut() else { return };
        let summary = effect(&before, observation, None).summary;
        last.effect = Some(match last.effect.take() {
            Some(detail) => format!("{detail}; {summary}"),
            None => summary,
        });
    }

    /// Every observation models see has resolved secret values blanked, wherever the page echoes them.
    pub(super) async fn observe(&self) -> Step<Observation> {
        let observation = self.trace.phase("observe", self.page.observe()).await?;
        let revealed = self.reveals(&observation);
        let redactor = self.redactor();
        let mask = |value: &str| redactor.mask(value);
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
        drop(redactor);
        let mut seen = self.seen();
        seen.raw = Some(observation);
        seen.secret_on_screen = revealed;
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
        let capture = self.trace.phase("capture", self.page.capture()).await?;
        let redactor = self.redactor();
        let mask = |value: &str| redactor.mask(value);
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

    /// The viewport for Jev, on a route that takes images and only within the sizes it accepts.
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
        match self.trace.phase("screenshot", self.page.screenshot()).await {
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
        // The step is what a caller replays the run from, as the result's steps and as the step event, so a secret a
        // step's builder did not think to redact is caught on its way out.
        let step = StepResult {
            url: self.redact(&step.url),
            target: step.target.as_deref().map(|target| self.redact(target)),
            note: step.note.as_deref().map(|note| self.redact(note)),
            // A secret resolved between the decision and this record (the step's own fill) is masked here too.
            choices: std::mem::take(&mut state.choices)
                .into_iter()
                .map(|choice| StepChoice {
                    label: self.mask(&choice.label),
                    context: choice.context.as_deref().map(|context| self.mask(context)),
                    ..choice
                })
                .collect(),
            ..step
        };
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
            choices: Vec::new(),
        };
        self.record_step(state, step, true, None).await
    }

    /// A PNG of the page as it is now, or None while a resolved secret is showing on it.
    ///
    /// The check is made against a fresh reading of the page rather than the last observation, which was taken before
    /// the action ran, and the action may be the one that put the secret there.
    pub(super) async fn frame(&self) -> Step<Option<Vec<u8>>> {
        let now = self.trace.phase("frame", self.page.observe()).await?;
        if self.reveals(&now) {
            return Ok(None);
        }
        match self.trace.phase("frame", self.page.screenshot()).await {
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

/// Jev's word on whether the page reports that the site failed, kept as the run's site failure on this address while
/// Jev holds it, and cleared when Jev reads the same address as working again. A status or navigation error the
/// browser gave is the more exact cause, and stays.
fn judge_site(state: &mut RunState, observation: &Observation, decision: &Decision) {
    let Some(probability) = decision.site_failure else { return };
    let here = state.site_failure.as_ref().filter(|failure| failure.url == observation.url);
    let reported = here.is_some_and(|failure| failure.cause == FailureCause::Reported);
    // Jev's answer is the more probable one, as in the done check.
    if probability >= 0.5 {
        if here.is_none() || reported {
            let since = here.map_or(state.steps.len(), |failure| failure.since);
            let url = observation.url.clone();
            state.site_failure = Some(SiteFailure { url, cause: FailureCause::Reported, since });
        }
    } else if reported {
        state.site_failure = None;
    }
}
