//! The loop and its step: observe, judge what the last action did, decide, gate and act.

use std::future::Future;
use std::time::Instant;

use fastbrowse_core::decision::{EVEN_ODDS, jev_takes_png};
use fastbrowse_core::effects::{Effect, SETTING_ROLES, Setting, effect, state_key};
use fastbrowse_core::models::{
    Decider, Event, Image, Operation, Status, StepChoice, StepEvent, StepOutcome, StepResult, WallKind,
};
use fastbrowse_core::page::{ActResult, Action, BrowserError, Capture, Control, Dialog, Observation, Tab};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::sha256_hex;
use fastbrowse_core::py::text;
use fastbrowse_core::safety::{may_be_irreversible, origin_of};
use serde_json::{Map, Value, json};

use super::finishing::UNREPORTED;
use super::helpers::{decision_label, describe, doing, drew_something, netloc_of, path_of, scheme_of};
use super::recovery::{Idle, leave_out_idle, uncover, withheld};
use super::state::{FailureCause, Flow, OutputIntent, RunState, SiteFailure, Step, Stop, Unsure, Written, read_key};

use super::{Agent, REDRAW_WATCH_SECONDS, browsing_deadline};
use crate::memory::address;
use crate::policy::{Decision, HistoryEntry, Planned, decide};
use crate::retrieval::page_quotes;
use crate::start::{accept_start, propose_start};
use crate::verification::RecordSchema;

fn recipe_step(
    decision: &Decision,
    raw: &Observation,
    action: &Action,
    source: Option<fastbrowse_core::recipe::RecipeText>,
    reveals: impl Fn(&str) -> bool,
) -> Option<fastbrowse_core::recipe::RecipeStep> {
    let mut recorded = decision.clone();
    if let Some(target) = &decision.target
        && let Some(raw_target) = raw.controls.iter().find(|control| control.id == target.id)
    {
        recorded.target = Some(raw_target.clone());
    }
    super::replay::step_of(&raw.url, &recorded, action, source, reveals)
}

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
        state.extraction_pending = output_schema.is_some();
        loop {
            state.ledger.check(0.0)?;
            if let Some(stop) = super::out_of_browsing_time(state) {
                return Err(Flow::Stop(stop));
            }
            let observation = self.observe().await?;
            if state.first_url.is_none() {
                state.first_url = Some(observation.url.clone());
            }
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
                    && self
                        .outwait(
                            &self.raw().unwrap_or_else(|| observation.clone()),
                            browsing_deadline(&state.ledger, state.compose),
                        )
                        .await?
                {
                    continue;
                }
            }
            let raw = self.raw().unwrap_or_else(|| observation.clone());
            if self.settle_blank(state, &raw).await? {
                continue;
            }
            self.note_effect(state, &observation);
            self.settle(state, &observation).await?;
            let origin = origin_of(&raw.url);
            let page = (raw.url.clone(), raw.document_key.clone());
            let secrets = self.secret_names(&origin);
            // A held secret makes a sign-in wall work to do rather than a stop, but passes no CAPTCHA, so the bot check
            // is asked on every page a secret covers too.
            // A page reported failed is judged afresh, since a "Loading..." page can fill in under the same document.
            let fresh = state.last_page.as_ref() != Some(&page) || state.page_failed;
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
            if picked.is_some()
                && let Some(replay) = state.replay.as_mut()
            {
                replay.replaced();
            }
            // A known page the run is on has been reached, and is not offered again once it is left.
            let here = |url: &&String| fastbrowse_core::navigation::same_address(url, &observation.url);
            if let Some(url) =
                state.known_pages.iter().find(here).filter(|url| !state.reached_known_pages.contains(url))
            {
                state.reached_known_pages.push(url.clone());
            }
            let decision = match picked {
                Some(picked) => picked,
                None => {
                    let mut context = self.context(state, &observation, secrets, check_login, fresh)?;
                    let mut offered = observation.clone();
                    leave_out_idle(state, &mut offered, &mut context);
                    state.mark_read_links(&mut offered);
                    state.mark_shown(&mut offered);
                    // The next saved step, when its page and control are certain, is the one control Jev is offered:
                    // Jev still judges the page, and may read or stop instead, but has no target to choose and no
                    // screenshot to look at, so no shortlist or picture is paid for.
                    let missing = &state.missing;
                    let saved = state.replay.as_mut().and_then(|replay| {
                        replay.ahead(&offered.url, &offered.controls, |target| {
                            missing.contains(&(offered.url.clone(), target.label.clone(), target.context.clone()))
                        })
                    });
                    match &saved {
                        Some(saved) => {
                            let id = saved.target.as_ref().map(|target| target.id.clone());
                            offered.controls.retain(|control| Some(&control.id) == id.as_ref());
                            self.event("recipe_step", || {
                                serde_json::Map::from_iter([
                                    ("operation".into(), saved.operation.to_string().into()),
                                    (
                                        "target".into(),
                                        saved.target.as_ref().map(|target| self.redact(&target.label)).into(),
                                    ),
                                ])
                            });
                        }
                        None => context.screenshot = self.drawn_viewport(&observation).await?,
                    }
                    let redecided = state.redecided;
                    let deciding = self.trace.phase("decide", async {
                        let (ledger, cache) = (Some(&*state.ledger), Some(&state.relevance_cache));
                        let config = &self.config;
                        Ok(decide(&*self.jev, &offered, &context, config, ledger, cache, &self.trace).await?)
                    });
                    let decided = self.unless_redrawn(redecided, deciding, &observation, None).await?;
                    let Watched::Done(mut decision) = decided else {
                        if let Some(replay) = state.replay.as_mut() {
                            replay.replaced();
                        }
                        self.redecide(state, &observation);
                        continue;
                    };
                    if let Some(saved) = saved {
                        let agreed = decision.operation == saved.operation
                            && decision.target.as_ref().map(|target| &target.id)
                                == saved.target.as_ref().map(|target| &target.id);
                        let replay = state.replay.as_mut().expect("a saved step comes from the replay");
                        if agreed {
                            decision.replayed = true;
                        } else {
                            replay.declined();
                            // Another action on the one control Jev was shown is decided again with the whole page
                            // in view; a read, a wait or done needs no control Jev was not shown.
                            if decision.target.is_some() {
                                continue;
                            }
                        }
                    } else if let Some(replay) = state.replay.as_mut() {
                        let missing = decision.target.as_ref().is_some_and(|target| {
                            state.missing.contains(&(offered.url.clone(), target.label.clone(), target.context.clone()))
                        });
                        replay.confirm(&offered.url, &offered.controls, &mut decision, missing);
                    }
                    state.last_page = Some(page);
                    decision
                }
            };
            self.record_page(state, &observation);
            state.note_pager_pages(&observation);
            state.choices = self.choices(&decision);
            judge_site(state, &observation, &decision);
            if let Some(probability) = decision.login_required {
                state.login_readings.insert(read_key(&observation), probability);
            }
            state.page_failed = decision.site_failure.is_some_and(|failed| failed >= EVEN_ODDS);
            // A fetched page has not run its scripts: one stuck on "Loading..." has failed only if a browser says so.
            if state.page_failed
                && self.page.over_http()
                && let Some(failure) = &state.site_failure
            {
                return Err(self.page.needs_browser(failure.message()).into());
            }
            // Going back from a commit that failed sends it again.
            let committed = std::mem::take(&mut state.committing);
            if committed
                && state.page_failed
                && let Some(failure) = &state.site_failure
            {
                return Err(failure.stop().into());
            }
            // Only a commit that was sent counts: one the page redrew, covered or replaced before it went is not one
            // the run has made. One that opened a dialog is not made yet: accepting the dialog makes it, through its
            // own gate.
            if committed
                && !state.page_failed
                && observation.dialog.is_none()
                && state.history.last().is_some_and(|entry| entry.outcome == StepOutcome::Executed)
            {
                state.commits += 1;
            }
            let thresholds = &self.config.thresholds;
            let bot_check = decision.bot_check.unwrap_or(0.0) > thresholds.bot_check_above;
            if bot_check || decision.login_required.unwrap_or(0.0) > thresholds.login_required_above {
                // A wall offering nothing to act on cannot be signed into, so it is waited on as a bot check is: for as
                // long as the browser's CAPTCHA solver works on it or its document is still loading, and no longer.
                if (bot_check || raw.controls.is_empty())
                    && self.outwait(&raw, browsing_deadline(&state.ledger, state.compose)).await?
                {
                    continue;
                }
                if bot_check && raw.controls.is_empty() && self.reload_challenge(state, &raw).await? {
                    continue;
                }
                // A bot check that does not clear ends the route, not the run.
                if bot_check && self.route_around(state, &observation, &raw.url).await? {
                    continue;
                }
                if bot_check {
                    // The address is the raw observation's, and an error is written out of the process, so a secret
                    // typed earlier and echoed in another host is redacted here as in any other report.
                    let error = format!(
                        "bot check at {}; it is not a sign-in and no credential passes it, and no other route to the task was found. Retry using an attached local browser or choose another source for this research.",
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
                // What the wall means is the task's to say, asked of the task alone and combined here: credentials it
                // gives are a sign-in to carry out, and a question about the restriction is answered by the wall.
                match self.sign_in_needs(state).await? {
                    Some(needs) if needs.credentials || needs.inspects => {}
                    // A route the task does not need an account for is walled, not the task: the run goes another way,
                    // and ends needing a sign-in only once its recoveries are spent.
                    Some(needs) if !needs.account => {
                        let reason =
                            "this page is a sign-in wall, and the task does not need an account: take another way";
                        let recovering = Box::pin(self.recover(
                            state,
                            &observation,
                            reason,
                            Status::NeedsLogin,
                            withheld(&decision),
                        ));
                        self.trace.phase("recover", recovering).await?;
                        continue;
                    }
                    _ => return Err(self.meet_wall(&observation, &raw)),
                }
            }
            // A run that read "Sold out" on its first step spent 74 more steps because nothing asked whether the task
            // could still be done (#960). Jev judges it from the page and the task; this ends the run on that answer.
            if decision.unobtainable.is_some_and(|probability| probability >= EVEN_ODDS) {
                return Err(self.unobtainable(state, &observation).await?.into());
            }
            if let Some(shows) = Self::reads_before_leaving(state, &observation, &decision)
                // The page the run began on is read for an answer only for a task that asks to be told something: a
                // task that only carries something out would otherwise answer with whatever its start page showed.
                && (shows || self.output_intent(state).await? != OutputIntent::CarriedOut)
            {
                let capture = self.capture().await?;
                // A click's action is only its gates, which see the task, the origin and the history but never the
                // notes, so it is built while the page is read.
                let beside = matches!(decision.operation, Operation::Click | Operation::Enter);
                let built = if beside {
                    let building = async |state: &mut RunState| self.build(state, &observation, &decision).await;
                    self.read_beside(state, capture.clone(), &observation, shows, building).await?
                } else {
                    None
                };
                match built {
                    Some(built) => state.built = Some(built),
                    None => {
                        self.trace.phase("read", self.read(state, capture, &observation, shows)).await?;
                    }
                }
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
                Ok(()) if !decision.plan.is_empty() => {
                    self.trace.phase("plan", Box::pin(self.run_plan(state, &observation, &decision.plan))).await?;
                }
                other => other?,
            }
        }
    }

    /// Fills the rest of a step's planned fields, each as its own step, while the page stays the document it was made
    /// on and the field is still there. Anything else, or a value the step builder is unsure of, hands the page back to
    /// a full decision.
    pub(super) async fn run_plan(&self, state: &mut RunState, decided_on: &Observation, plan: &[Planned]) -> Step {
        for planned in plan {
            if state.redecided || state.picked.is_some() {
                break;
            }
            // Each planned fill is a step, and the run's step, time and spend limits bound it as they bound a decided one.
            state.ledger.check(0.0)?;
            if let Some(stop) = super::out_of_browsing_time(state) {
                return Err(Flow::Stop(stop));
            }
            let observation = self.observe().await?;
            let same_document = observation.url == decided_on.url
                && observation.document_key == decided_on.document_key
                && observation.dialog.is_none()
                && observation.modal == decided_on.modal;
            // The field Jev judged, still fillable: an id kept by a field the page rewrote is another field.
            let same_field = |control: &&Control| {
                let (was, now) = (&planned.target, *control);
                now.id == was.id
                    && now.operations.contains(&Operation::Fill)
                    && (&now.role, &now.label, &now.context, &now.input_name, &now.autocomplete)
                        == (&was.role, &was.label, &was.context, &was.input_name, &was.autocomplete)
            };
            let found = observation.controls.iter().find(same_field).cloned();
            let (true, Some(target)) = (same_document, found) else {
                break;
            };
            self.note_effect(state, &observation);
            self.settle(state, &observation).await?;
            let decision = Decision {
                operation: Operation::Fill,
                url: None,
                target: Some(target),
                tab_id: None,
                operation_confidence: planned.probability,
                target_confidence: Some(planned.probability),
                login_required: None,
                offered_controls: 0,
                reduction: crate::policy::Reduction::None,
                cost: Vec::new(),
                input_tokens: 0,
                bot_check: None,
                shows_answer: None,
                site_failure: None,
                unobtainable: None,
                destination: None,
                replayed: false,
                by_code: false,
                offered: Vec::new(),
                ahead: planned.ahead.clone(),
                plan: Vec::new(),
            };
            self.event("plan_step", || Map::from_iter([("target".into(), self.redact(&planned.target.label).into())]));
            match Box::pin(self.step(state, &observation, &decision)).await {
                Err(Flow::Unsure(_)) => break,
                other => other?,
            }
        }
        Ok(())
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
        // Nothing was sent, so the commit the gate cleared is not one the run has made.
        state.committing = false;
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

    /// The pages the site is known to serve for the items the task names, asked while the first page opens (#983).
    pub(super) async fn known_pages(
        &self,
        task: &str,
        start: Option<&str>,
        ledger: &fastbrowse_core::telemetry::Ledger,
    ) -> Step<Vec<String>> {
        // As with the task's dates, a run whose known pages could not be asked for runs on without them: a provider
        // error on this hint ended a run before its first page was read. Only the budget stops the run here.
        let proposal = match crate::start::known_pages(&*self.llm, task, start, Some(ledger)).await {
            Ok(proposal) => proposal,
            Err(crate::error::AgentError::Budget(budget)) => return Err(budget.into()),
            Err(error) => {
                self.event("known_pages_failed", || Map::from_iter([("error".into(), error.to_string().into())]));
                return Ok(Vec::new());
            }
        };
        ledger.record([proposal.cost])?;
        let mut known: Vec<String> = Vec::new();
        for url in proposal.data.urls.iter().filter_map(|url| crate::start::accept_start(Some(url))) {
            if !known.contains(&url) {
                known.push(url);
            }
        }
        if !known.is_empty() {
            self.event("known_pages", || Map::from_iter([("urls".into(), known.clone().into())]));
        }
        Ok(known)
    }

    /// The dates the task names, as the parts it states. Jev first judges from the task alone whether it names any, so
    /// a task without one costs no extraction; the run asks this while its first page opens.
    pub(super) async fn task_dates(
        &self,
        task: &str,
        ledger: &fastbrowse_core::telemetry::Ledger,
    ) -> Step<Vec<crate::dates::DateReference>> {
        use fastbrowse_core::decision::{Answer, DecisionError, Question};
        ledger.reserve(fastbrowse_core::models::CostComponent::Jev, 0.0, 1)?;
        let questions =
            indexmap::IndexMap::from([("dates".to_string(), Question::Noul(crate::dates::dates_question(task)))]);
        let state = json!({"task": task});
        let asked = self.jev.evaluate(&state, &questions);
        // As with the consent question, a task whose dates could not be asked about runs on with today's date alone:
        // the step that needs Jev next meets the same failure and handles it. Only the budget stops the run here.
        let failed = |error: String| {
            self.event("task_dates_failed", || Map::from_iter([("error".into(), error.into())]));
            Ok(Vec::new())
        };
        let evaluation =
            match fastbrowse_clients::validation::jev_purpose(fastbrowse_core::models::JevPurpose::Action, asked).await
            {
                Ok(evaluation) => evaluation,
                Err(DecisionError::Budget(budget)) => return Err(budget.into()),
                Err(error) => return failed(error.to_string()),
            };
        ledger.record([evaluation.cost.clone()])?;
        if !matches!(evaluation.answers.get("dates"), Some(Answer::Noul(answer)) if answer.probability > EVEN_ODDS) {
            return Ok(Vec::new());
        }
        let extracted = match crate::dates::task_dates(&*self.llm, task, Some(ledger)).await {
            Ok(extracted) => extracted,
            Err(crate::error::AgentError::Budget(budget)) => return Err(budget.into()),
            Err(error) => return failed(error.to_string()),
        };
        ledger.record([extracted.cost])?;
        let references = extracted.data.dates;
        self.event("task_dates", || Map::from_iter([("dates".into(), json!(references))]));
        Ok(references)
    }

    /// Go to the site's front page when the address this run chose for itself opened nothing.
    ///
    /// A start page worked out from the task is a guess at an address, and a guess can name a path the site does not
    /// serve. Only the path was ever in doubt, so the front page of the site already chosen is where to be. A start
    /// page the caller gave is left alone. Returns whether it went there.
    pub(super) async fn front_page_if_blank(&self, opened: &str, until: Option<tokio::time::Instant>) -> Step<bool> {
        if matches!(path_of(opened).as_str(), "" | "/") {
            return Ok(false);
        }
        let observation = self.observe().await?;
        if drew_something(&observation) {
            return Ok(false);
        }
        // A page built by script is observable before it draws: navigation returns at `readyState`, and hydration
        // follows, so a deep route that was right is not abandoned for being slow.
        if self.outwait(&observation, until).await? && drew_something(&self.observe().await?) {
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
        if self.outwait(challenge, browsing_deadline(&state.ledger, state.compose)).await? {
            return Ok(true);
        }
        Ok(self.observe().await?.page_key != challenge.page_key)
    }

    /// Re-observe until the page is no longer `stuck`, returning whether it moved.
    ///
    /// The page is waited on only while something says it is still on its way: the browser's CAPTCHA solver is working
    /// on it, or its document or a frame in it is still loading. Each wait is for the page's next lifecycle event, and
    /// the run's browsing deadline `until` bounds them all; a run with no time limit waits for the event. With neither
    /// signal the page is re-read once and the wait ends, and a CAPTCHA the solver gave up on ends it too: nothing
    /// else will clear it.
    pub(super) async fn outwait(&self, stuck: &Observation, until: Option<tokio::time::Instant>) -> Step<bool> {
        let mut mark = self.page.lifecycle_mark();
        let mut solving = false;
        loop {
            if self.observe().await?.page_key != stuck.page_key {
                return Ok(true);
            }
            match self.page.captcha_solve().filter(|solve| solve.url == stuck.url) {
                Some(solve) if solve.working => {
                    if !solving {
                        self.event("captcha_solving", || {
                            Map::from_iter([("url".into(), solve.url.into()), ("vendor".into(), solve.vendor.into())])
                        });
                    }
                    solving = true;
                }
                Some(solve) => {
                    self.event("captcha_unsolved", || {
                        Map::from_iter([("url".into(), solve.url.into()), ("vendor".into(), solve.vendor.into())])
                    });
                    return Ok(false);
                }
                None if self.page.loading() => {}
                None => return Ok(false),
            }
            let seconds = match until {
                Some(until) => until.saturating_duration_since(tokio::time::Instant::now()).as_secs_f64(),
                None => f64::INFINITY,
            };
            if seconds <= 0.0 {
                return Ok(false);
            }
            self.page.wait_for_lifecycle(mark, seconds).await;
            mark = self.page.lifecycle_mark();
        }
    }

    pub(super) async fn settle_blank(&self, state: &mut RunState, observation: &Observation) -> Step<bool> {
        if drew_something(observation) {
            // A later navigation can have the same empty frame set before its destination starts answering.
            state.blank_waited.clear();
            return Ok(false);
        }
        let document =
            if observation.document_key.is_empty() { &observation.page_key } else { &observation.document_key };
        if !state.blank_waited.insert((observation.url.clone(), document.clone())) {
            return Ok(false);
        }
        if self.outwait(observation, browsing_deadline(&state.ledger, state.compose)).await? {
            return Ok(true);
        }
        if state.blank_opening_checked || state.history.iter().any(|entry| entry.operation.is_some()) {
            return Ok(false);
        }
        state.blank_opening_checked = true;
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

    /// The decision's action, dropped when the page redraws its target while it is built.
    async fn build(
        &self,
        state: &mut RunState,
        observation: &Observation,
        decision: &Decision,
    ) -> Step<Watched<Action>> {
        let redecided = state.redecided;
        let target = decision.target.as_ref().map(|target| target.id.as_str());
        let building = self.trace.phase("build_action", self.action(state, observation, decision));
        self.unless_redrawn(redecided, building, observation, target).await
    }

    /// Take one step and judge whether it moved the run on, which the one stall rule counts.
    pub(super) async fn step(&self, state: &mut RunState, observation: &Observation, decision: &Decision) -> Step {
        let started = Instant::now();
        let facts_before = state.notes.facts().len();
        let label = decision_label(decision);
        let mut typed: Option<String> = None;
        let mut effect_now: Option<String> = None;
        let mut setting = false;
        let mut set: Vec<(String, String)> = Vec::new();
        let frames = self.on_event.is_some() && self.config.step_frames;
        let act: ActResult;
        let changed: bool;
        // None for a page change, which the next observation judges by the page state it lands on.
        let progressed: Option<bool>;
        let frame: Option<Vec<u8>>;
        if decision.operation == Operation::Read {
            frame = if frames { self.frame().await? } else { None };
            let shows = Self::page_shows(state, observation, decision);
            let capture = self.capture().await?;
            let read_progress =
                self.trace.phase("read", Box::pin(self.read(state, capture, observation, shows))).await?;
            progressed = Some(read_progress);
            changed = false;
            let text = state.read_note();
            let text = self.redact(text);
            effect_now = Some(text.clone());
            act = ActResult { outcome: StepOutcome::Executed, page_changed: false, detail: Some(text), moved: false };
        } else {
            let built = match state.built.take() {
                Some(built) => built?,
                None => self.build(state, observation, decision).await?,
            };
            let action = match built {
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
            let mut result = match self.trace.phase("act", self.page.act(&action, &raw)).await {
                Ok(result) => result,
                Err(BrowserError::PageUnresponsive(_)) if Self::may_reopen(state, decision, &action) => {
                    return self.reopen_hung_page(state, observation, decision, started, frame).await;
                }
                Err(error) => return Err(error.into()),
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
            // A known page can redirect to another address, so the landing page does not identify it: the
            // navigation's own address does. Marking it reached here keeps it from being offered again, which would
            // spend the no-progress budget on the same redirect.
            if executed
                && action.operation == Operation::Navigate
                && let Some(url) = action.url.as_ref().filter(|url| state.known_pages.contains(url))
                && !state.reached_known_pages.contains(url)
            {
                state.reached_known_pages.push(url.clone());
            }
            if let Some(replay) = state.replay.as_mut() {
                if decision.replayed {
                    replay.settled(executed);
                } else if executed {
                    replay.followed(&observation.url, decision);
                }
            }
            let source = state.text_source.take();
            if executed
                && super::replay::recorded(decision.operation)
                && let Some(step) = recipe_step(decision, &raw, &action, source, |text| self.reveals_text(text))
            {
                state.recipe_log.push(step);
            }
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
            // Showing another tab leaves the one it left as it was.
            let stayed = matches!(decision.operation, Operation::Scroll | Operation::ScrollUp | Operation::SwitchTab);
            state.changed_page = (executed && changed && !stayed).then(|| read_key(observation));
            // A value edit answers "was this progress" itself, and its answer beats `changed`: the popup a fill draws
            // IS a page change, so `changed` alone kept crediting the identical re-fill. `changed` decides every other
            // operation, by the page state it lands on.
            let written = state.written.entry(address(&observation.url)).or_default();
            let edit = edit_progress(decision, &action, written);
            // Every write is remembered, including one the page judged for itself: a second, different value on a
            // control has to be recorded or the same text coming back later reads as new.
            if executed
                && let Some(target) = &decision.target
                && let Some(held) = held_value(&action)
            {
                written.record(target, &held);
            }
            let judged = match edit {
                _ if !executed => Some(false),
                Some(edited) => Some(edited),
                // The fingerprint reads neither field values nor focus, so a field left reported no change even when
                // its change handler rewrote it. Nor does it read a control without text, so a hover that showed a
                // card's icon-only buttons also reported no change. The page state each lands on judges it, as for a
                // page change.
                None if matches!(decision.operation, Operation::Blur | Operation::Hover) => None,
                None => (!changed).then_some(false),
            };
            if edit == Some(false) && executed {
                // The field already held the value, or was given it before, so this step moved nothing and cannot
                // count: three of them trip the stall budget. A run that went back to a credentials form re-entered the
                // password it already had exactly this way, and nothing told it the form was ready to submit instead.
                // An edit always has a target, so the label is there; "None" is what Python's f-string would write.
                let named = label.as_deref().unwrap_or("None");
                // A field showing something else was given this value before, and the page put that in its place.
                let shown = decision.target.as_ref().and_then(|target| target.value.as_deref());
                let hint = match shown.filter(|shown| Some(*shown) != held_value(&action).as_deref()) {
                    Some(shown) => format!(
                        "{named} was given that value before, and the page shows {} there instead.",
                        text::repr(shown)
                    ),
                    None => format!(
                        "{named} already holds that value: submit the form, or read the page, instead of typing it again."
                    ),
                };
                state.hint = Some(self.redact(&hint));
            }
            state.acted_from = Some(observation.clone());
            state.showing = executed.then(|| doing(decision.operation, label.as_deref()));
            // The address a click or navigation asked for, so reading where it landed also reads that address: a
            // site that redirects `/page/2` to `/page/2/` left the asked one unread, and a whole-list answer unverified.
            let asked = match action.operation {
                Operation::Click => decision.target.as_ref().and_then(|target| target.href.as_deref()),
                Operation::Navigate => decision.url.as_deref(),
                _ => None,
            };
            state.followed = asked
                .filter(|_| executed)
                .and_then(|href| url::Url::parse(&observation.url).ok()?.join(href).ok())
                .map(String::from);
            if executed
                && let Some(target) = &decision.target
                && SETTING_ROLES.contains(&target.role.as_str())
            {
                // A menu that closed without the value changing still changes the page, so what the choice set is
                // stated: Google Flights' "One way" was clicked to the step limit while the history said only "changed".
                let landed = match self.observe().await {
                    Ok(landed) => landed,
                    Err(Flow::Browser(BrowserError::PageUnresponsive(_)))
                        if Self::may_reopen(state, decision, &action) =>
                    {
                        return self.reopen_hung_page(state, observation, decision, started, frame).await;
                    }
                    Err(error) => return Err(error),
                };
                let done = effect(observation, &landed, Some(target), false);
                if done.changed {
                    uncover(&mut state.idle);
                }
                state.acted_from = None;
                let showing = state.showing.take();
                state.note_shown(observation, &landed, &done.shown, showing);
                state.settle_change(&landed);
                setting = done.set_something;
                set = done
                    .set
                    .iter()
                    .filter_map(|(id, held)| Some((describe(landed.controls.iter().find(|c| c.id == *id)?), held)))
                    .map(|(field, held)| {
                        let left = match held {
                            Setting::Value(Some(value)) => text::repr(value),
                            Setting::Value(None) => "empty".to_string(),
                            Setting::Checked(checked) => (if *checked { "checked" } else { "unchecked" }).to_string(),
                            Setting::Selected(selected) => {
                                (if *selected { "selected" } else { "not selected" }).to_string()
                            }
                        };
                        (field, left)
                    })
                    .collect();
                // A page that shows a choice only in a label or in text holds it in no field it reports, so the run
                // cannot say which field it set: it says what it clicked.
                let stayed = observation.url == landed.url && observation.document_key == landed.document_key;
                if set.is_empty() && done.set_something && stayed {
                    set.push((describe(target), UNREPORTED.to_string()));
                }
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
            set,
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
            .map(|fact| self.public_fact(fact, &state.notes))
            .collect();
        let step = StepResult {
            index: state.steps.len() as i64,
            operation: decision.operation,
            decided_by: decision.decider(),
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
                let stalled = Idle { action: withheld(decision), outcome: act.outcome };
                self.count_progress(state, progressed, observation, stalled, Status::Stuck).await
            }
            None => {
                state.landing = Some(withheld(decision));
                Ok(())
            }
        }
    }

    /// The one stall rule: `stall.unchanged_actions` steps in a row that moved nothing ask Jev again with the stalled
    /// action left out. A step moves the run on when it gathers evidence, writes a new value, reaches a page state never
    /// seen, or makes the first return since the last evidence to a page state seen before it.
    pub(super) async fn count_progress(
        &self,
        state: &mut RunState,
        progressed: bool,
        observation: &Observation,
        stalled: Idle,
        gives_up_as: Status,
    ) -> Step {
        if progressed {
            state.unchanged = 0;
            state.hint = None;
            state.idle.clear();
            return Ok(());
        }
        state.unchanged += 1;
        state.idle.push(stalled.clone());
        if state.unchanged >= self.config.stall.unchanged_actions {
            let reason = format!("no_progress ({})", state.unchanged);
            Box::pin(self.recover(state, observation, &reason, gives_up_as, stalled)).await?;
        }
        Ok(())
    }

    /// Record the step whose page stopped answering as failed, and open that page again once. Its frame was taken
    /// before the action, so the step still shows what was acted on.
    async fn reopen_hung_page(
        &self,
        state: &mut RunState,
        observation: &Observation,
        decision: &Decision,
        started: Instant,
        frame: Option<Vec<u8>>,
    ) -> Step {
        state.hung_page_reopened = true;
        if decision.replayed
            && let Some(replay) = state.replay.as_mut()
        {
            replay.settled(false);
        }
        let label = decision_label(decision);
        // A click or navigation may have reached a new page before its renderer hung: reopen that page, not the one
        // the step started on. The target metadata answers without the renderer.
        let url = match self.page.active_url().await {
            Ok(Some(url)) => url,
            Ok(None) | Err(_) => observation.url.clone(),
        };
        state.acted_from = None;
        state.followed = None;
        self.event("hung_page_reload", || Map::from_iter([("url".into(), url.clone().into())]));
        let effect = "The page stopped answering and was reopened.".to_string();
        state.history.push(HistoryEntry {
            effect: Some(effect.clone()),
            ..HistoryEntry::new(Some(decision.operation), label.as_deref(), StepOutcome::Failed, false)
        });
        let step = StepResult {
            index: state.steps.len() as i64,
            operation: decision.operation,
            decided_by: decision.decider(),
            outcome: StepOutcome::Failed,
            page_changed: Some(url != observation.url),
            url: url.clone(),
            target: label,
            confidence: Some(decision.confidence()),
            note: Some(effect),
            facts: Vec::new(),
            duration_ms: started.elapsed().as_millis() as i64,
            choices: Vec::new(),
        };
        self.record_step(state, step, false, frame).await?;
        self.page.navigate(&url, None).await?;
        Ok(())
    }

    /// A run reopens one hung page, and never after an authorized commit the renderer may have received: reopening
    /// would let the run replay it.
    fn may_reopen(state: &RunState, decision: &Decision, action: &Action) -> bool {
        if state.hung_page_reopened {
            return false;
        }
        if !state.authorization.irreversible_actions {
            return true;
        }
        let accepts_dialog = decision.operation == Operation::Dialog && action.accept_dialog == Some(true);
        // Visiting an address can commit with no control behind it.
        let navigates = decision.operation == Operation::Navigate;
        !accepts_dialog && !navigates && !may_be_irreversible(decision.operation)
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
        // A drag's authorization covers both ends; a source twin cannot authorize a changed destination. A blur leaves
        // the field that was focused when Jev chose it; a redrawn twin is another field, and leaving it is not that.
        if matches!(action.operation, Operation::Drag | Operation::Blur) {
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
                // A replacement field need not inherit focus. Ordinary actions verify and acquire their own target;
                // Blur, which must leave the original focused node, is excluded before this comparison.
                Control {
                    id: target.id.clone(),
                    offscreen: target.offscreen,
                    focused: target.focused,
                    ..(*control).clone()
                } == *target
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

    /// Note the page state reached and judge the last page change by it: a state never seen, or the first return to
    /// one seen before the last evidence was gathered, is progress. A state never seen renews the recovery budget.
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
            state.recoveries = 0;
        }
        if let Some(stalled) = state.landing.take() {
            let returned =
                last.is_some_and(|seen_at| seen_at < state.evidence_reads) && state.returned_at < state.evidence_reads;
            if returned {
                state.returned_at = state.evidence_reads;
            }
            let progressed = last.is_none() || returned;
            Box::pin(self.count_progress(state, progressed, observation, Idle::performed(stalled), Status::Stuck))
                .await?;
        }
        Ok(())
    }

    /// Record on the last action what it did, which the next choice and recovery both read.
    pub(super) fn note_effect(&self, state: &mut RunState, observation: &Observation) {
        if let Some(status) = observation.response_status {
            if status < 400 && state.start_failure.as_ref().is_some_and(|failure| failure.url == observation.url) {
                state.start_failure = None;
            }
            if status >= 400 {
                let since = match &state.site_failure {
                    Some(failure) if failure.url == observation.url => failure.since,
                    _ => state.steps.len(),
                };
                state.site_failure = Some(SiteFailure::http(&observation.url, status, since));
                if state.first_url.as_deref() == Some(observation.url.as_str()) {
                    state.start_failure = state.site_failure.clone();
                }
            } else if status >= 200
                && state.site_failure.as_ref().is_some_and(|failure| {
                    failure.url == observation.url && !matches!(failure.cause, FailureCause::Reported)
                })
            {
                // A page that loads can still report the site failed; only Jev, reading it, clears that.
                state.site_failure = None;
            }
        }
        let showing = state.showing.take();
        let Some(before) = state.acted_from.take() else { return };
        state.settle_change(observation);
        let landed = read_key(observation);
        if landed != read_key(&before)
            && state.history.last().is_some_and(|last| last.operation != Some(Operation::SwitchTab))
        {
            state.navigation_from = Some((landed.clone(), before.clone()));
        }
        if let Some(href) = state.followed.take()
            && landed != read_key(&before)
        {
            state.arrived_by = Some((landed, href));
        }
        let Some(last) = state.history.last_mut() else { return };
        let waited = last.operation == Some(Operation::Wait);
        let done = if last.operation == Some(Operation::SwitchTab) {
            // Another tab's controls were there all along: compared with the tab left, every one read as added.
            Effect {
                summary: format!("showed the tab at {}", observation.url),
                set_something: false,
                set: Vec::new(),
                added: false,
                changed: false,
                shown: Vec::new(),
            }
        } else {
            effect(&before, observation, None, waited)
        };
        last.added = done.added;
        // A Wait is told how far a playing clock moved, but a clock that moves by itself removes no cover.
        let moved = done.changed && (!waited || effect(&before, observation, None, false).changed);
        if moved && last.outcome == StepOutcome::Executed {
            uncover(&mut state.idle);
        }
        // A page can change while an attempted action goes stale. Reconsider it on the new page, including Back,
        // but keep stale actions withheld when the next observation still shows the same page state.
        if done.changed && last.outcome == StepOutcome::Stale {
            let attempted = last.operation.map(|operation| (operation, last.target.clone()));
            state.idle.retain(|idle| idle.action != attempted);
        }
        // Text or controls changed after the click attempt, so the earlier read is out of date even when the browser
        // did not perform the click. A covered click's own focus change must not make its target available again.
        if done.changed && last.operation == Some(Operation::Click) {
            if last.outcome != StepOutcome::Covered {
                let clicked = Some((Operation::Click, last.target.clone()));
                state.idle.retain(|idle| idle.action != clicked);
            }
            // Field by field, since `last` borrows the run's steps: this is `RunState::page_changed`.
            let page = read_key(observation);
            if let Some(read) = state.read_documents.get_mut(&page) {
                *read = true;
            }
            state.read_tries.remove(&page);
        }
        last.effect = Some(match last.effect.take() {
            Some(detail) => format!("{detail}; {}", done.summary),
            None => done.summary,
        });
        state.note_shown(&before, observation, &done.shown, showing);
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
            offscreen_text: mask(&observation.offscreen_text),
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
            // A page can name its modal with what it echoes back, a resolved secret among it.
            modal: mask_opt(&observation.modal),
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
                // An address can carry a token in its query, and a link's text is page text like the rest.
                links: block
                    .links
                    .iter()
                    .map(|link| fastbrowse_core::models::Link {
                        text: mask(&link.text),
                        href: mask(&link.href),
                        ..link.clone()
                    })
                    .collect(),
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

    /// A drawn page's controls do not describe its imagery, so a step's question carries the viewport too.
    pub(super) async fn drawn_viewport(&self, observation: &Observation) -> Step<Option<Vec<u8>>> {
        if !observation.visual_content {
            return Ok(None);
        }
        self.viewport().await
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
        // A delayed page update can reveal a secret after the decision's observation. The glance and the image are
        // taken together, and the image is dropped unseen when the glance shows a secret.
        let (now, image) = tokio::join!(self.trace.phase("screenshot", self.page.glance()), self.page.screenshot());
        if self.reveals(&now?) {
            return Ok(Vec::new());
        }
        match image {
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
        let now = self.trace.phase("frame", self.page.glance()).await?;
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

/// Rewriting the observed field value vetoes progress even when the page changes, and the first new value a field
/// receives is progress even when the page does not: a page's fingerprint ignores field values, so a three-field
/// checkout form counted three unchanged steps and tripped the stall recovery every run.
pub(super) fn edit_progress(decision: &Decision, action: &Action, written: &Written) -> Option<bool> {
    if !matches!(decision.operation, Operation::Fill | Operation::Select) {
        return None;
    }
    let target = decision.target.as_ref()?;
    // A list that allows several choices toggles the option named, so naming a chosen one again takes it out: the
    // page the step lands on, which reads every chosen option, judges it.
    if target.input_type.as_deref() == Some("select-multiple") {
        return None;
    }
    let held = held_value(action)?;
    if target.value.as_deref() == Some(held.as_str()) {
        return Some(false);
    }
    // A widget can redraw while rejecting repeated text, so a page change alone does not prove the value was set.
    // A field the form emptied can still receive the same text again.
    let kept_another = target.value.as_deref().is_some_and(|value| !value.is_empty());
    if kept_another && written.holds(target, &held) {
        return Some(false);
    }
    if written.wrote(target) { None } else { Some(true) }
}

/// The value an action writes, as the page would show it. A secret's value never leaves the page, so only its length
/// is observed.
fn held_value(action: &Action) -> Option<String> {
    let text = action.text.as_ref()?;
    Some(if action.secret { "\u{2022}".repeat(text.chars().count()) } else { text.clone() })
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

#[cfg(test)]
mod recipe_tests {
    use super::*;
    use fastbrowse_core::recipe::RecipeText;
    use fastbrowse_core::safety::Redactor;

    #[test]
    fn recipe_recording_omits_raw_secret_metadata_after_model_masking() {
        let mut redactor = Redactor::new();
        redactor.register("account", "hunter2", Some("https://shop.test"));
        for (url, label) in
            [("https://shop.test/?account=hunter2", "Continue"), ("https://shop.test/", "Continue as hunter2")]
        {
            let target = Control::new("next", "button", label, [Operation::Click]);
            let raw: Observation = serde_json::from_value(json!({
                "url": url, "title": "t", "page_key": "k", "captured_at": "2026-09-26T20:00:00Z",
                "controls": [target.dump()], "viewport_text": "", "tabs": [],
            }))
            .unwrap();
            let masked = Control { label: redactor.mask(label), ..target };
            let decision = super::super::tests::code_decision(Operation::Click, Some(masked));
            let action = Action::new(Operation::Click);
            assert!(
                super::super::replay::step_of(&redactor.mask(url), &decision, &action, None, |text| redactor
                    .reveals(text))
                .is_some()
            );
            assert!(recipe_step(&decision, &raw, &action, None, |text| redactor.reveals(text)).is_none());
        }
    }

    #[test]
    fn recipe_recording_keeps_named_secret_fill_on_an_ordinary_field() {
        let mut redactor = Redactor::new();
        redactor.register("account", "hunter2", Some("https://shop.test"));
        let target = Control::new("account", "textbox", "Account", [Operation::Fill]);
        let raw: Observation = serde_json::from_value(json!({
            "url": "https://shop.test/", "title": "t", "page_key": "k", "captured_at": "2026-09-26T20:00:00Z",
            "controls": [target.dump()], "viewport_text": "", "tabs": [],
        }))
        .unwrap();
        let decision = super::super::tests::code_decision(Operation::Fill, Some(target));
        let action = Action { text: Some("hunter2".into()), secret: true, ..Action::new(Operation::Fill) };
        let source = RecipeText::Secret("account".into());
        let step = recipe_step(&decision, &raw, &action, Some(source.clone()), |text| redactor.reveals(text)).unwrap();
        assert_eq!(step.text, Some(source));
    }
}
