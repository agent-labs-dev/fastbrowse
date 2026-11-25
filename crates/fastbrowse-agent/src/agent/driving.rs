//! The loop and its step: observe, judge what the last action did, decide, gate and act.

use std::future::Future;
use std::time::Instant;

use fastbrowse_core::decision::{EVEN_ODDS, jev_takes_png};
use fastbrowse_core::effects::{Effect, SETTING_ROLES, Setting, effect, state_key};
use fastbrowse_core::models::{
    Decider, Event, Image, Operation, Status, StepChoice, StepEvent, StepFailure, StepOutcome, StepResult,
};
use fastbrowse_core::page::{ActResult, Action, BrowserError, Capture, Control, Observation};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::sha256_hex;
use fastbrowse_core::py::text;
use fastbrowse_core::safety::may_be_irreversible;
use serde_json::{Map, Value, json};

use super::finishing::UNREPORTED;
use super::helpers::{decision_label, describe, drew_something, netloc_of, path_of, scheme_of};
use super::recovery::{Idle, uncover, withheld};
use super::state::{FailureCause, Flow, RunState, SiteFailure, Step, Stop, Unsure, Written, read_key, tab_key};

use super::{Agent, REDRAW_WATCH_SECONDS, browsing_deadline};
use crate::memory::address;
use crate::policy::{Commit, Decision, FieldChange, Form, HistoryEntry, Submitted};
use crate::start::{accept_start, propose_readable_start, propose_start};
use crate::verification::RecordSchema;
use fastbrowse_core::navigation::written_addresses;

pub(super) struct TaskJudgments {
    pub dates: Vec<crate::dates::DateReference>,
    pub output: Option<Box<indexmap::IndexMap<String, fastbrowse_core::decision::Answer>>>,
}

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
        self.chat_loop(state, output_schema, until).await
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
        Self::discard_decision(state);
    }

    /// Nothing was sent: the commit the gate cleared is not one the run has made, and a pictured target or look left
    /// set would be attached to the previous action by the next observation.
    fn discard_decision(state: &mut RunState) {
        state.committing = false;
        state.keeping = false;
        state.sighted = None;
        state.look = None;
        state.choices.clear();
    }

    /// `work`'s result, or `Redrawn` when the page redrew under it: `target`, or with none any control offered.
    ///
    /// A page can draw after it settles, and nothing says it will: picking a day in Google Flights' date picker blanks
    /// the prices, the page waits about 400ms with no request, spinner or mutation, and then fetches and draws them.
    /// Watching while the driver's action is gated finds the work doomed, and the next decision is made on the settled page.
    /// Only one redraw per action: a control that never stops changing is still acted on.
    ///
    /// Doomed work still runs before `Redrawn` is returned, to finish the requests it already sent and record what they
    /// cost (#1218), but it sends no new one ([`fastbrowse_core::superseding`]). That costs the rest of one request, about half a
    /// second on Online-Mind2Web, and stays inside the run's time limit.
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
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let work = fastbrowse_core::superseding::superseding(flag.clone(), work);
        tokio::pin!(work);
        tokio::pin!(watching);
        tokio::select! {
            biased;
            done = &mut work => done.map(Watched::Done),
            redrawn = &mut watching => match redrawn {
                Ok(true) => {
                    fastbrowse_core::superseding::supersede(&flag);
                    // Its outcome was decided on a page that is gone, so it is discarded, an error included; the next
                    // decision meets any limit it reached.
                    let _ = work.await;
                    Ok(Watched::Redrawn)
                }
                Ok(false) => work.await.map(Watched::Done),
                Err(error) => Err(error.into()),
            },
        }
    }

    /// The page to begin on when the caller named none, proposed from the task.
    pub(super) async fn first_page(
        &self,
        task: &str,
        ledger: &std::sync::Arc<fastbrowse_core::telemetry::Ledger>,
    ) -> Step<Option<String>> {
        if self.page.search_price().is_some() {
            return self.search_entry(task, ledger).await;
        }
        let proposal = propose_start(&*self.llm, &self.mask(task), Some(ledger)).await?;
        ledger.record([proposal.cost])?;
        let proposed = proposal.data.url.as_deref();
        // The model was shown the task masked, so an address the task writes with credentials comes back as its
        // bullets, and opens as the caller wrote it; any other address with userinfo is refused (`accept_start`).
        let written = proposed.and_then(|proposed| {
            written_addresses(task).into_iter().find(|address| address != proposed && self.mask(address) == proposed)
        });
        let Some(opening) = written.or_else(|| accept_start(proposed)) else {
            return Err(Stop::new(
                Status::NeedsInput,
                "Start/no-page",
                "the task names no page to start from, and none was given",
            )
            .into());
        };
        self.learn_credentials([opening.as_str()]);
        self.event("start_page", || Map::from_iter([("url".into(), opening.clone().into())]));
        Ok(Some(opening))
    }

    /// Opens the page the run begins on. A start page the run chose itself that an HTTP read cannot use is replaced by
    /// one proposed knowing why each address failed, until a page opens or none is known: a `--fetch` run has no other
    /// way past it, and ended `blocked` before its first step (#1330). When none opens, `opening` is left the address
    /// the run first chose, so a run that moves to a browser opens that page rather than a guess an HTTP read also
    /// failed on, which may be an address the site does not serve.
    pub(super) async fn open_start(
        &self,
        opening: &mut String,
        chosen: bool,
        task: &str,
        ledger: &std::sync::Arc<fastbrowse_core::telemetry::Ledger>,
    ) -> Step {
        let first = opening.clone();
        let mut unreadable: Vec<(String, String)> = Vec::new();
        let mut first_error = None;
        loop {
            let error = match self.trace.phase("open", self.page.navigate(opening, None)).await {
                Ok(()) => return Ok(()),
                Err(error) if chosen && error.needs_browser() && self.page.over_http() => error,
                Err(error) => return Err(error.into()),
            };
            self.event("start_page_unreadable", || {
                Map::from_iter([("url".into(), opening.clone().into()), ("error".into(), error.to_string().into())])
            });
            unreadable.push((opening.clone(), error.to_string()));
            let first_error = first_error.get_or_insert(error);
            let proposal = propose_readable_start(&*self.llm, task, &unreadable, Some(ledger)).await?;
            ledger.record([proposal.cost])?;
            let tried = |url: &String| unreadable.iter().any(|(seen, _)| seen == url);
            let Some(next) = accept_start(proposal.data.url.as_deref()).filter(|url| !tried(url)) else {
                *opening = first;
                return Err(first_error.clone().into());
            };
            self.event("start_page", || Map::from_iter([("url".into(), next.clone().into())]));
            *opening = next;
        }
    }

    /// Independent task judgments share startup's request. Date extraction runs only when Jev finds a date;
    /// output judgments are kept for their existing point of use, so list walking does not start sooner.
    pub(super) async fn task_judgments(
        &self,
        given: &str,
        ledger: &fastbrowse_core::telemetry::Ledger,
    ) -> Step<TaskJudgments> {
        use fastbrowse_core::decision::{Answer, DecisionError, Question};
        ledger.reserve(fastbrowse_core::models::CostComponent::Jev, 0.0, 1)?;
        // These judgments all read the task alone. Sending them separately reread the same task on every run.
        let mut questions = self.output_questions();
        questions.insert("dates".to_string(), Question::Noul(crate::dates::dates_question()));
        let state = json!({"task": given});
        let asked = self.jev.evaluate(&state, &questions);
        // As with the consent question, a task whose dates could not be asked about runs on with today's date alone:
        // the step that needs Jev next meets the same failure and handles it. Only the budget stops the run here.
        let failed = |error: String, output| {
            self.event("task_dates_failed", || Map::from_iter([("error".into(), error.into())]));
            Ok(TaskJudgments { dates: Vec::new(), output })
        };
        let mut batched = true;
        let evaluation =
            match fastbrowse_clients::validation::jev_purpose(fastbrowse_core::models::JevPurpose::Action, asked).await
            {
                Ok(evaluation) => evaluation,
                Err(DecisionError::Budget(budget)) => return Err(budget.into()),
                Err(DecisionError::InputTooLarge(_)) => {
                    // A provider's input limit can admit the date question on its own while refusing the batch.
                    // Keep its old request available; the output questions are asked at their point of use.
                    batched = false;
                    questions.retain(|key, _| key == "dates");
                    ledger.reserve(fastbrowse_core::models::CostComponent::Jev, 0.0, 1)?;
                    let asked = self.jev.evaluate(&state, &questions);
                    match fastbrowse_clients::validation::jev_purpose(
                        fastbrowse_core::models::JevPurpose::Action,
                        asked,
                    )
                    .await
                    {
                        Ok(evaluation) => evaluation,
                        Err(DecisionError::Budget(budget)) => return Err(budget.into()),
                        Err(error) => return failed(error.to_string(), None),
                    }
                }
                Err(error) => return failed(error.to_string(), None),
            };
        ledger.record([evaluation.cost.clone()])?;
        let names_dates =
            matches!(evaluation.answers.get("dates"), Some(Answer::Noul(answer)) if answer.probability > EVEN_ODDS);
        let output = batched.then(|| Box::new(evaluation.answers));
        if !names_dates {
            return Ok(TaskJudgments { dates: Vec::new(), output });
        }
        let extracted = match crate::dates::task_dates(&*self.llm, given, Some(ledger)).await {
            Ok(extracted) => extracted,
            Err(crate::error::AgentError::Budget(budget)) => return Err(budget.into()),
            Err(error) => return failed(error.to_string(), output),
        };
        ledger.record([extracted.cost])?;
        let references = extracted.data.dates;
        self.event("task_dates", || Map::from_iter([("dates".into(), json!(references))]));
        Ok(TaskJudgments { dates: references, output })
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

    /// Re-observe until the page is no longer `stuck`, returning whether it moved.
    ///
    /// The page is waited on only while something says it is still on its way: the browser's CAPTCHA solver is working
    /// on it, or its document or a frame in it is still loading. Each wait is for the page's next lifecycle event, and
    /// the run's browsing deadline `until` bounds them all. A run with no time limit waits for the event, though for a
    /// load no longer than a navigation is given: a server that never answers sends none. With neither signal the
    /// page is re-read once and the wait ends, and a CAPTCHA the solver gave up on ends it too: nothing else will
    /// clear it.
    pub(super) async fn outwait(&self, stuck: &Observation, until: Option<tokio::time::Instant>) -> Step<bool> {
        let mut mark = self.page.lifecycle_mark();
        let mut solving = false;
        loop {
            if self.observe().await?.page_key != stuck.page_key {
                return Ok(true);
            }
            let bound = match self.page.captcha_solve().filter(|solve| solve.url == stuck.url) {
                Some(solve) if solve.working => {
                    if !solving {
                        self.event("captcha_solving", || {
                            Map::from_iter([("url".into(), solve.url.into()), ("vendor".into(), solve.vendor.into())])
                        });
                    }
                    solving = true;
                    f64::INFINITY
                }
                Some(solve) => {
                    self.event("captcha_unsolved", || {
                        Map::from_iter([("url".into(), solve.url.into()), ("vendor".into(), solve.vendor.into())])
                    });
                    return Ok(false);
                }
                None => match self.page.loading() {
                    Some(left) => left,
                    None => return Ok(false),
                },
            };
            let seconds = match until {
                Some(until) => until.saturating_duration_since(tokio::time::Instant::now()).as_secs_f64(),
                None => f64::INFINITY,
            }
            .min(bound);
            if seconds <= 0.0 {
                return Ok(false);
            }
            self.page.wait_for_lifecycle(mark, seconds).await;
            mark = self.page.lifecycle_mark();
        }
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
        // Nesting action construction inline with browser dispatch overflowed the native worker stack.
        let building = self.trace.phase("build_action", Box::pin(self.action(state, observation, decision)));
        self.unless_redrawn(redecided, building, observation, target).await
    }

    /// Take one step and judge whether it moved the run on, which the one stall rule counts.
    pub(super) async fn step(&self, state: &mut RunState, observation: &Observation, decision: &Decision) -> Step {
        let started = Instant::now();
        let facts_before = state.notes.facts().len();
        let label = decision_label(decision);
        let mut typed: Option<String> = None;
        let mut destination = None;
        let mut effect_now: Option<String> = None;
        let mut put_now: Option<(String, Vec<String>)> = None;
        let mut setting = false;
        let mut set: Vec<FieldChange> = Vec::new();
        let frames = self.on_event.is_some() && self.config.step_frames;
        let act: ActResult;
        let changed: bool;
        // None for a page change, which the next observation judges by the page state it lands on.
        let progressed: Option<bool>;
        let frame: Option<Vec<u8>>;
        if decision.operation == Operation::Read {
            frame = if frames { self.frame().await? } else { None };
            let shows = Self::page_shows(state, observation);
            let capture = self.capture().await?;
            let read_progress =
                self.trace.phase("read", Box::pin(self.read(state, capture, observation, shows))).await?;
            progressed = Some(read_progress);
            changed = false;
            let text = state.read_note();
            let text = self.redact(text);
            effect_now = Some(text.clone());
            act = ActResult {
                outcome: StepOutcome::Executed,
                failure: None,
                page_changed: false,
                detail: Some(text),
                moved: false,
                landed: None,
            };
        } else {
            let action = match self.build(state, observation, decision).await? {
                Watched::Done(action) => action,
                Watched::Redrawn => {
                    self.redecide(state, observation);
                    return Ok(());
                }
            };
            state.redecided = false;
            if state.committing {
                // What this commits can still be redirecting or awaiting a 3-D Secure check when the run ends, and
                // closing the tab would abort it (#748). Handed over here, not when built: a read can discard the built
                // action, and the tab would stay handed over for a commit never sent.
                self.page.hand_over_tab();
            }
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
                self.seen().secret_typed = true;
            }
            let raw = self.raw().unwrap_or_else(|| observation.clone());
            // A drawn control pressed as one element changes pixels, not elements: a canvas switch or gesture that read
            // as "nothing visible changed" was withheld as idle, and the run went looking elsewhere. Its frame before
            // the action lets the one after judge it.
            // The driver names a thing it saw inside a drawn control: Jev places it there, as pressing the whole surface
            // clicked its centre and selected nothing.
            let inside = match (&decision.given, decision.target.as_ref()) {
                (Some(crate::policy::Given::Text(on)), Some(target))
                    if action.operation == Operation::Click && target.drawn == Some(true) =>
                {
                    Some((on.clone(), target.clone()))
                }
                _ => None,
            };
            let drawn_before = match decision.target.as_ref() {
                Some(target)
                    if inside.is_none()
                        && matches!(action.operation, Operation::Click | Operation::Drag)
                        && target.drawn == Some(true) =>
                {
                    self.viewport().await?
                }
                _ => None,
            };
            let acted = if let Some((on, surface)) = &inside {
                let sighted = self.trace.phase("act", self.click_inside_drawn(state, &raw, surface, on)).await;
                sighted.map(|clicked| {
                    clicked.unwrap_or_else(|| ActResult {
                        outcome: StepOutcome::Failed,
                        failure: Some(fastbrowse_core::models::StepFailure::MissingTarget),
                        page_changed: false,
                        detail: Some(format!(
                            "{} was not seen inside {}, so nothing was clicked",
                            text::repr(on),
                            text::repr(&describe(surface))
                        )),
                        moved: false,
                        landed: None,
                    })
                })
            } else {
                let dispatched = async {
                    if action.operation == Operation::Wait {
                        // A WAIT lasts until the page changes, which may be later than a few seconds: a status line
                        // rewritten every ten seconds was never seen to move. The driver says how long it will wait, as a
                        // page that never changes held the run to its deadline with the model never told nothing happened.
                        // The run's own deadline bounds it too, and with neither the page ends it after one armed watch.
                        let left = browsing_deadline(&state.ledger, state.compose).map_or(f64::INFINITY, |until| {
                            until.saturating_duration_since(tokio::time::Instant::now()).as_secs_f64()
                        });
                        let seconds = match decision.given {
                            Some(crate::policy::Given::Seconds(asked)) => left.min(asked.max(0.0)),
                            _ => left,
                        };
                        self.page.wait(&action, &raw, seconds).await.map_err(Flow::from)
                    } else {
                        self.page.act(&action, &raw).await.map_err(Flow::from)
                    }
                };
                self.trace.phase("act", dispatched).await
            };
            let mut result = match acted {
                Ok(result) => result,
                Err(Flow::Browser(BrowserError::PageUnresponsive(_))) if Self::may_reopen(state, decision, &action) => {
                    return self.reopen_hung_page(state, observation, decision, started, frame).await;
                }
                Err(error) => return Err(error),
            };
            // A drawn surface's twin is the same surface, which pressed as one element would press its centre.
            if result.outcome == StepOutcome::Stale
                && let Some(target) = &decision.target
                && let Some(retried) = self.act_on_twin(&action, observation, target).await?
            {
                result = retried;
            }
            if let Some(before) = drawn_before.filter(|_| !result.page_changed) {
                self.judge_by_pixels(state, before, &mut result).await?;
            }
            if result.outcome == StepOutcome::Covered
                && action.operation == Operation::Click
                && let Some(target) = &decision.target
                && let Some(clicked) = self.click_where_seen(state, &raw, target).await?
            {
                result = clicked;
            }
            let executed = result.outcome == StepOutcome::Executed;
            if decision.replayed
                && let Some(replay) = state.replay.as_mut()
            {
                replay.settled(executed);
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
            if executed
                && action.operation == Operation::Drag
                && let Some(id) = &action.destination_id
            {
                // The resulting label or page text may change without naming the control the drag was sent to.
                destination = observation.controls.iter().find(|control| &control.id == id).map(describe);
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
            // A click placed by sight that changed no element was judged by the pixels it changed.
            let drawn = state.drawn_changed.take();
            let judged = match edit {
                _ if !executed => Some(false),
                _ if drawn.is_some() => drawn,
                Some(edited) => Some(edited),
                // The fingerprint reads neither field values nor focus, so a field left reported no change even when
                // its change handler rewrote it. Nor does it read a control without text, so a hover that showed a
                // card's icon-only buttons also reported no change. The page state each lands on judges it, as for a
                // page change.
                None if matches!(decision.operation, Operation::Blur | Operation::Hover) => None,
                None => (!changed).then_some(false),
            };
            // The DOM diff of a surface whose picture changed as meant would add that nothing visible changed.
            state.acted_from = (drawn != Some(true)).then(|| observation.clone());
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
                let mut done = effect(observation, &landed, Some(target), false);
                if done.changed {
                    uncover(&mut state.idle, observation, &landed, &mut done.summary);
                }
                state.acted_from = None;
                state.settle_change(&landed);
                setting = done.set_something;
                set = done
                    .set
                    .iter()
                    .filter_map(|(id, held)| {
                        let field = landed.controls.iter().find(|c| c.id == *id)?;
                        let left = match held {
                            Setting::Value(Some(value)) => text::repr(value),
                            Setting::Value(None) => "empty".to_string(),
                            Setting::Checked(checked) => (if *checked { "checked" } else { "unchecked" }).to_string(),
                            Setting::Selected(selected) => {
                                (if *selected { "selected" } else { "not selected" }).to_string()
                            }
                        };
                        Some(FieldChange {
                            label: describe(field),
                            value: left,
                            form: Form::of(&landed, field),
                            was_target: observation.document_key == landed.document_key
                                && field.frame_id == target.frame_id
                                && field.id == target.id,
                        })
                    })
                    .collect();
                // A page that shows a choice only in a label or in text holds it in no field it reports, so the run
                // cannot say which field it set: it says what it clicked.
                let stayed = observation.url == landed.url && observation.document_key == landed.document_key;
                if set.is_empty() && done.set_something && stayed {
                    set.push(FieldChange {
                        label: describe(target),
                        value: UNREPORTED.to_string(),
                        form: None,
                        was_target: false,
                    });
                }
                put_now = (!done.put.is_empty()).then(|| (landed.url.clone(), done.put));
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
        let sent = state.committing && act.outcome == StepOutcome::Executed;
        let form = if decision.operation == Operation::Dialog && act.outcome == StepOutcome::Executed {
            state.dialog_form.take().filter(|_| sent)
        } else {
            decision.target.as_ref().and_then(|target| Form::of(observation, target))
        };
        if act.outcome == StepOutcome::Executed && observation.dialog.is_none() {
            state.dialog_form.clone_from(&form);
        }
        // The field's value when the step began, which a later save states beside what the run typed over it.
        let before = typed.as_ref().and(decision.target.as_ref()).and_then(|target| {
            let held = observation.controls.iter().find(|c| c.id == target.id).filter(|c| !c.sensitive)?;
            Some(self.mask(held.value.as_deref().unwrap_or_default()))
        });
        state.history.push(HistoryEntry {
            destination,
            text: typed,
            before,
            effect: effect_now,
            put: put_now,
            look: state.look.take(),
            setting: setting.then_some(true),
            set,
            read_progress: decision.operation == Operation::Read && progressed == Some(true),
            // A step that dispatched nothing submitted nothing.
            submitted: decision
                .target
                .as_ref()
                .filter(|_| act.outcome == StepOutcome::Executed)
                .and_then(|target| Submitted::of(decision.operation, target, observation)),
            commit: sent.then_some(Commit::Sent),
            form,
            document_key: Some(observation.document_key.clone()),
            control_id: decision.target.as_ref().map(|target| target.id.clone()),
            form_id: decision.target.as_ref().and_then(|target| target.form_id.clone()),
            frame_id: decision.target.as_ref().and_then(|target| target.frame_id.clone()),
            ..HistoryEntry::new(Some(decision.operation), label.as_deref(), act.outcome, changed)
        });
        if let (Some(entry), Some(landed)) = (state.history.last(), &act.landed)
            && let Some(text) = &entry.text
            && entry.changed_site()
        {
            // The control is the one the browser reported the text in, which a later read of that control reports.
            let step = state.history.iter().filter(|entry| entry.changed_site()).count();
            state.notes.remember_typing(step, &landed.document, &landed.element, text);
        }
        if let Some(value) = state.history.last().and_then(|entry| entry.text.clone()) {
            self.event("typed", || {
                Map::from_iter([("step".into(), state.steps.len().into()), ("value".into(), value.into())])
            });
        }
        let note = self.redact(act.detail.as_deref().unwrap_or_default());
        let refused = (act.outcome == StepOutcome::Failed && !note.is_empty()).then(|| note.clone());
        let facts: Vec<_> = state.notes.facts()[facts_before.min(state.notes.facts().len())..]
            .iter()
            .map(|fact| self.public_fact(fact, &state.notes))
            .collect();
        let step = StepResult {
            index: state.steps.len() as i64,
            operation: decision.operation,
            decided_by: decision.decider(),
            outcome: act.outcome,
            failure: act.failure,
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
                let covered_target_id = decision
                    .target
                    .as_ref()
                    .filter(|_| act.outcome == StepOutcome::Covered)
                    .map(|target| target.id.clone());
                let stalled = Idle { outcome: act.outcome, covered_target_id, refused, ..Idle::tried(decision) };
                self.count_progress(state, progressed, observation, stalled, Status::Stuck).await
            }
            None => {
                state.landing = Some(withheld(decision));
                Ok(())
            }
        }
    }

    /// The one stall rule: `stall.unchanged_actions` steps in a row that moved nothing tell the driver so, with the stalled
    /// action refused if taken again. A step moves the run on when it gathers evidence, writes a new value, reaches a page state never
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
            state.renew_stall();
            return Ok(());
        }
        state.unchanged += 1;
        state.idle.push(stalled.clone());
        if state.unchanged >= self.config.stall.unchanged_actions {
            let unchanged = state.unchanged;
            let reason = format!("no_progress ({unchanged})");
            Box::pin(self.recover(state, observation, &reason, gives_up_as)).await?;
            state.chat.notices.push(format!(
                "{unchanged} actions in a row changed nothing. Try a different control or way to the goal."
            ));
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
            failure: Some(StepFailure::BrowserError),
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
        state.visited_addresses.insert(observation.url.clone());
        let state_last = state.reached.insert(state_key(observation), state.evidence_reads);
        let tab_last = state.shown.insert(tab_key(observation), state.evidence_reads);
        // The viewport is part of what a page shows: a scroll that lands on a part never shown moved the run on even
        // though the page state, keyed by its controls, is one it has been in.
        let unseen_view =
            state.viewed.insert(sha256_hex(json!([observation.url, observation.viewport_key]).to_string().as_bytes()));
        let scrolled = matches!(state.landing, Some(Some((Operation::Scroll | Operation::ScrollUp, _))));
        // A tab switch moves nothing inside either tab, so it lands on a new place only when that tab was not
        // shown before. Its controls are not compared: a live page ticks them between visits.
        let switched = matches!(state.landing, Some(Some((Operation::SwitchTab, _))));
        let last = if switched { tab_last } else { state_last };
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
        let replied = std::mem::take(&mut state.replied);
        if let Some(stalled) = state.landing.take() {
            let returned =
                last.is_some_and(|seen_at| seen_at < state.evidence_reads) && state.returned_at < state.evidence_reads;
            if returned {
                state.returned_at = state.evidence_reads;
            }
            // A scroll back to a part already shown is going round, however long ago the last evidence was gathered.
            let progressed =
                if scrolled { last.is_none() || unseen_view || replied } else { last.is_none() || returned || replied };
            Box::pin(self.count_progress(state, progressed, observation, Idle::performed(stalled), Status::Stuck))
                .await?;
        }
        Ok(())
    }

    /// A sent notice's capture is kept as a read's is: cited but never kept, a result quoting the notice carried no
    /// capture artifact to verify the quote against.
    pub(super) fn record_sent_notice(
        &self,
        state: &mut RunState,
        capture: &Capture,
        shown: &[fastbrowse_core::effects::ShownPiece],
    ) {
        if state.record_sent_notice(capture, shown) {
            self.seen().read_captures.entry(capture.sha256.clone()).or_insert_with(|| capture.text.clone());
        }
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
        // Taken before any return, so it names only the click it was placed for: a map pin judged by its pixels left no
        // page to diff, kept the step, and the next click on the map was shown as made for it instead.
        let sighted = std::mem::take(&mut state.sighted);
        let keeping = std::mem::take(&mut state.keeping);
        // An observation with no action since the last one (a decision made again after a read) has no click to
        // name, so it leaves the step the previous observation gave that click.
        if (sighted.is_some() || state.acted_from.is_some())
            && let Some(last) = state.history.last_mut()
        {
            last.for_step = sighted.clone();
        }
        state.sent_text.clear();
        let Some(before) = state.acted_from.take() else { return };
        state.settle_change(observation);
        let landed = read_key(observation);
        if landed != read_key(&before)
            && state.history.last().is_some_and(|last| last.operation != Some(Operation::SwitchTab))
        {
            state.navigation_from = Some((landed.clone(), before.clone()));
            // A step that dispatched nothing, or only waited, scrolled or hovered, did not lead the run here: the page
            // changed by itself.
            state.navigated_by =
                state.history.last().filter(|last| last.outcome == StepOutcome::Executed && last.acts()).and_then(
                    |last| {
                        let operation = last.operation?;
                        Some(match &last.target {
                            Some(target) => format!("{operation} {}", text::repr(target)),
                            None => operation.to_string(),
                        })
                    },
                );
        }
        if let Some(href) = state.followed.take()
            && landed != read_key(&before)
        {
            // Landing on a page already read shows that the address followed leads to it, so the link is marked read
            // from now on. Only a read records the address otherwise, and a read page is not read again: a link whose
            // address the site redirects to an author page the run had read and left was taken back to it repeatedly.
            if state.read_documents.contains_key(&landed) {
                state.read_urls.insert(href.clone());
            }
            state.arrived_by = Some((landed, href));
        }
        let Some(last) = state.history.last_mut() else { return };
        let waited = last.operation == Some(Operation::Wait);
        let mut done = if last.operation == Some(Operation::SwitchTab) {
            // Another tab's controls were there all along: compared with the tab left, every one read as added.
            Effect {
                summary: format!("showed the tab at {}", observation.url),
                set_something: false,
                set: Vec::new(),
                added: false,
                changed: false,
                shown: Vec::new(),
                put: Vec::new(),
            }
        } else if (state.committing || keeping || sighted.is_some() || last.look.is_some())
            && last.outcome == StepOutcome::Executed
            && observation.dialog.is_none()
        {
            // Still set from the gate that let this step go: the next decision is what takes it. Only a commit that
            // was sent has a reply: one the page redrew or covered before it went, or one waiting on a dialog, did
            // not put there the text a page added meanwhile. A click inside a drawn surface changes no element it
            // names, so the text the page added in reply is all that records it: the popup a click on an OpenLayers
            // marker opened read as "only the page text changed", and the step was judged never carried out. So does a
            // control told apart only by its pixels (it has a card): Leaflet's unnamed triangle swapped one popup for
            // another, read as "showed 1 control: Close popup; removed 1 control: Close popup", and was clicked again.
            let shown = fastbrowse_core::effects::shown_text(&before, observation);
            state.replied =
                (state.committing || keeping || last.look.is_some()) && shown.iter().any(|piece| piece.added);
            if state.committing || keeping {
                state.sent_text = shown;
            }
            fastbrowse_core::effects::effect_of_commit(&before, observation, None, waited)
        } else {
            effect(&before, observation, None, waited)
        };
        if last.operation == Some(Operation::Click) && last.outcome == StepOutcome::Executed {
            last.opened_forms = observation
                .controls
                .iter()
                .filter_map(|control| Some((control.frame_id.clone(), control.form_id.clone()?)))
                .filter(|(frame, form)| {
                    !before
                        .controls
                        .iter()
                        .any(|control| control.frame_id == *frame && control.form_id.as_deref() == Some(form.as_str()))
                })
                .collect();
            last.opened_forms.dedup();
            if !last.opened_forms.is_empty() {
                last.document_key = Some(observation.document_key.clone());
            }
        }
        last.added = done.added;
        // A Wait is told how far a playing clock moved, but a clock that moves by itself removes no cover.
        let moved = done.changed && (!waited || effect(&before, observation, None, false).changed);
        if moved && last.outcome == StepOutcome::Executed {
            uncover(&mut state.idle, &before, observation, &mut done.summary);
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
        // What the effect says the step put on this page, which a commit that stayed on its document names (a step that
        // went to another page added that page's text, not its own).
        if !done.put.is_empty() {
            let (_, put) = last.put.get_or_insert_with(|| (observation.url.clone(), Vec::new()));
            for piece in &done.put {
                if !put.contains(piece) {
                    put.push(piece.clone());
                }
            }
        }
        last.effect = Some(match last.effect.take() {
            Some(detail) => format!("{detail}; {}", done.summary),
            None => done.summary,
        });
    }

    /// Every observation models see has resolved secret values blanked, wherever the page echoes them (`masking`).
    pub(super) async fn observe(&self) -> Step<Observation> {
        let observation = self.trace.phase("observe", self.page.observe()).await?;
        let revealed = self.reveals(&observation);
        let masked = {
            let mut redactor = self.redactor();
            super::masking::keep_code_made(&mut redactor, &observation);
            super::masking::observation(&redactor, &observation)
        };
        let mut seen = self.seen();
        seen.raw = Some(observation);
        seen.secret_on_screen = revealed;
        seen.observed = Some(masked.clone());
        Ok(masked)
    }

    pub(super) async fn capture(&self) -> Step<Capture> {
        let capture = self.trace.phase("capture", self.page.capture()).await?;
        if let Ok(shown) = serde_json::to_value(&capture) {
            self.learn_credentials_shown(&shown);
        }
        Ok(self.masked(&capture))
    }

    /// `capture` with every secret resolved so far blanked. A capture the driver was shown before a secret resolved is
    /// masked again once it resolves: it held a value the page showed before the run typed it, and a done quoting it
    /// carried the value into the notes the composer and Jev read.
    pub(super) fn masked(&self, capture: &Capture) -> Capture {
        let mut redactor = self.redactor();
        super::masking::keep_capture_ids(&mut redactor, capture);
        let mut masked = super::masking::capture(&redactor, capture);
        // Masking keeps lengths, so spans blanked before stay where they were, though the text no longer shows them.
        masked.masked.extend(capture.masked.iter().cloned());
        masked.masked.sort_unstable();
        masked.masked.dedup();
        masked
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
        Ok(self.image(Shot::Model).await?.into_iter().collect())
    }

    /// Whether no image of the page may leave the run: from the first address credential it learns to its end, as
    /// its live frames and recording are held (`learn_credentials`). A page can paint its own address, the password
    /// in it, on a canvas, where no text reading finds it.
    pub(super) fn images_withheld(&self) -> bool {
        self.redactor().holds_credentials()
    }

    /// The one place an image of the page is taken for anything that leaves the run: a model's screenshot, a step
    /// frame, the final frame and every screenshot artifact. None once the run holds an address credential, and while
    /// a resolved secret may show; a frame is also refused from the first secret typed (`frame`).
    async fn image(&self, shot: Shot) -> Step<Option<Vec<u8>>> {
        if self.images_withheld() {
            return Ok(None);
        }
        let refused = match shot {
            Shot::Model => self.seen().secret_on_screen,
            // A reading cannot show what a frame will: the page may echo the secret as pixels after it.
            Shot::Frame => self.seen().secret_typed,
        };
        if refused {
            return Ok(None);
        }
        // A delayed page update can reveal a secret after the decision's observation. The glance and the image are
        // taken together, and the image is dropped unseen when the glance shows a secret or a credential it brings.
        let phase = match shot {
            Shot::Model => "screenshot",
            Shot::Frame => "frame",
        };
        let (now, image) = tokio::join!(self.trace.phase(phase, self.page.glance()), self.page.screenshot());
        if self.reveals(&now?) || self.images_withheld() {
            return Ok(None);
        }
        match image {
            Ok(image) => Ok(Some(image)),
            Err(BrowserError::ScreenshotsUnavailable(_)) => Ok(None),
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
        // A read can learn a credential after it buffered this frame.
        let frame = if self.images_withheld() { None } else { frame.map(Image) };
        on_event(Event::Step(StepEvent { kind: Default::default(), step, frame })).await;
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
        let step = (operation, reason, target, confidence, page_changed, decided_by);
        self.record_undone(state, observation, StepOutcome::Failed, step).await
    }

    /// Record a step the authorization gate did not take. Recorded as failed with no failure named, a Post held for
    /// confirmation read as a failure the caller could not get past, and it asked a person to post by hand (#1413).
    pub(super) async fn record_refusal(
        &self,
        state: &mut RunState,
        observation: &Observation,
        decision: &Decision,
        reason: &str,
        label: &str,
    ) -> Step {
        let decided_by = decision.decider();
        let step = (
            decision.operation,
            reason,
            Some(self.redact(label)),
            Some(decision.confidence()),
            Some(false),
            decided_by,
        );
        self.record_undone(state, observation, StepOutcome::Refused, step).await
    }

    async fn record_undone(
        &self,
        state: &mut RunState,
        observation: &Observation,
        outcome: StepOutcome,
        (operation, reason, target, confidence, page_changed, decided_by): (
            Operation,
            &str,
            Option<String>,
            Option<f64>,
            Option<bool>,
            Decider,
        ),
    ) -> Step {
        let step = StepResult {
            index: state.steps.len() as i64,
            operation,
            decided_by,
            outcome,
            failure: None,
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

    /// A PNG of the page as it is now, or None once a secret has been typed or while one is showing on it.
    ///
    /// The check is made against a fresh reading of the page rather than the last observation, which was taken before
    /// the action ran, and the action may be the one that put the secret there.
    pub(super) async fn frame(&self) -> Step<Option<Vec<u8>>> {
        self.image(Shot::Frame).await
    }

    /// Learn the credentials of every address a page shows, in its text, a link or a field. Chrome can drop the
    /// userinfo of an address a redirect brought from the tab's address once it has answered the server's 401 with
    /// it, so the page's own `document.URL` and its echo of the password were the only places the run met it, and
    /// the bare echo and the `Authorization` value the page showed reached Jev unmasked (#1381). Each string is
    /// scanned on its own: a JSON dump writes a quote as `\"`, and `\` ends a special scheme's authority.
    ///
    /// Only an authority with userinfo after a scheme or `//` is learnt, so an email or a `mailto:` link is not. One a
    /// page shows holds every image for the rest of the run by design: the page may draw it again on a canvas or in
    /// a field, where no text check can find it.
    fn learn_credentials_shown(&self, shown: &Value) {
        fn strings<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
            match value {
                Value::String(text) => out.push(text),
                Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
                Value::Object(fields) => fields.values().for_each(|item| strings(item, out)),
                _ => {}
            }
        }
        let mut found = Vec::new();
        strings(shown, &mut found);
        self.learn_credentials(found);
    }

    /// Whether a resolved secret shows on the page; live and recorded frames are held back while one does.
    pub(super) fn reveals(&self, observation: &Observation) -> bool {
        // A dialog hides the page text, so its observation cannot prove the pixels contain no secret.
        // An address a redirect or a new tab brings with credentials is learnt here, before its observation is masked
        // for a model or a trace, and from then on the run's frames are held to its end (`learn_credentials`).
        self.learn_credentials(
            std::iter::once(observation.url.as_str())
                .chain(observation.tabs.iter().map(|tab| tab.url.as_str()))
                .chain(observation.history_entry.as_deref()),
        );
        self.learn_credentials_shown(&observation.dump());
        // The page's own address and its tabs' are not drawn on the page, so they are set aside: Chrome keeps a
        // start's credentials in a tab's address, and every model image of such a run was withheld.
        let mut drawn = observation.dump();
        if let Some(fields) = drawn.as_object_mut() {
            fields.remove("url");
            if let Some(tabs) = fields.get_mut("tabs").and_then(Value::as_array_mut) {
                tabs.iter_mut().filter_map(Value::as_object_mut).for_each(|tab| {
                    tab.remove("url");
                });
            }
        }
        let redactor = self.redactor();
        let revealed = observation.dialog.is_some() || redactor.reveals_value(&drawn);
        let held = redactor.holds_credentials();
        drop(redactor);
        self.page.withhold_frames(revealed || held);
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
        Flow::from(fastbrowse_core::decision::DecisionError::failed("operation needs a target and Jev offered none"))
    })
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

/// What an image of the page is taken for (`Agent::image`).
#[derive(Clone, Copy)]
enum Shot {
    /// For a model to look at.
    Model,
    /// For a caller: a step frame or the final frame.
    Frame,
}
