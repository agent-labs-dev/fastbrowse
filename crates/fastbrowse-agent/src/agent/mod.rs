//! The run loop: Jev picks every operation and target, reads, and says DONE, and an unsure Jev still acts on its best
//! pick. Code owns the safety gates (irreversible actions, sign-in walls, bot checks, authorization, secrets) and the
//! stall rule. The answer is what Jev read, each fact with its citations.
//!
//! Every Jev request built here is a pure function of its inputs, since a run is replayed from recorded model traffic
//! keyed on a hash of the request body.

mod acting;
mod batching;
mod driving;
mod finishing;
pub mod helpers;
mod reading;
mod recovery;
mod replay;
mod search;
mod seeing;
pub(crate) mod state;
mod superseding;

#[cfg(test)]
mod tests;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use fastbrowse_clients::validation::{Lines, jev_spend};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::citations::text_fragment;
use fastbrowse_core::config::Config;
use fastbrowse_core::decision::DecisionClient;
use fastbrowse_core::llm::LLMClient;
use fastbrowse_core::models::{
    Artifact, Attachment, Authorization, BudgetResource, BudgetStop, Citation, CostBreakdown, CostLine, EventHandler,
    Evidence, FailureKind, Limits, MaybeUnavailable, Operation, RunResult, SecretResolver, Status, StepFact,
    StepOutcome, UntilCheck, Wall, WallKind,
};
use fastbrowse_core::page::{BrowserError, Observation, Page, Unread};
use fastbrowse_core::safety::{
    CredentialField, Redactor, asks_for_credentials, credential_field, origin_of, secret_allowed, wall_kind,
};
use fastbrowse_core::telemetry::{Ledger, Trace};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use acting::Carried;
use finishing::outcome_notes;
use state::{Flow, RunState, Stop};

use crate::error::AgentError;
use crate::memory::{Fact, Notes};
use crate::policy::{HistoryEntry, StepContext};
use crate::retrieval::{
    ComposedAnswer, cited_quote, compose_reviewed, draft_answer, marked_partial, noticed, partial_answer,
};
use crate::verification::RecordSchema;

/// A fresh observation can wait three seconds for loading, and a background capture needs time to activate.
const ENDING_FRAME_SECONDS: f64 = 6.0;
/// Longer than deciding takes: a watch ends with the work it watches, and this only bounds a stuck one.
const REDRAW_WATCH_SECONDS: f64 = 10.0;

/// What one run is asked to do, beyond its task.
#[derive(Default)]
pub struct RunOptions {
    pub start: Option<String>,
    pub choose_start: bool,
    /// What the caller knows about the task, shown to Jev as background and never as part of the task.
    pub context: Option<String>,
    pub inputs: IndexMap<String, String>,
    pub attachments: Vec<Attachment>,
    pub output_schema: Option<Arc<RecordSchema>>,
    pub limits: Option<Limits>,
    /// A ledger the caller reads after the run. It carries its own limits, so `limits` is not read beside it.
    pub ledger: Option<Arc<Ledger>>,
    pub authorization: Option<Authorization>,
    pub until: Option<UntilCheck>,
    /// The run a page without a browser ended, which this one carries on from where it stopped.
    pub resume: Option<Resume>,
    pub recipe: Option<fastbrowse_core::recipe::Recipe>,
    pub recipe_sink: Option<RecipeSink>,
}

/// The executed steps and replay accounting, returned after a run ends.
#[derive(Clone)]
pub struct RecipeOutcome {
    pub recipe: fastbrowse_core::recipe::Recipe,
    pub replayed: usize,
    pub fell_back: usize,
}

pub type RecipeSink = Arc<Mutex<Option<RecipeOutcome>>>;

/// A run that ended on a page without a browser: what it knew, and where it stood.
pub struct Resume {
    // A browser handoff moves this state across nested futures; keep its fields off their poll stacks.
    state: Box<RunState>,
    url: Option<String>,
}

/// What the agent learns about the page as it goes, kept apart from the run so the page and the models can be asked
/// side by side.
#[derive(Default)]
struct Seen {
    secret_on_screen: bool,
    /// A secret has been typed this run: no step or final frame is taken after it, whatever a reading shows.
    secret_typed: bool,
    raw: Option<Observation>,
    /// The last observation `observe` returned, which the finish judges again only if the page changed since.
    observed: Option<Observation>,
    artifact_start: usize,
    /// The masked text of each capture a read took facts from, by digest, for the captures a result cites.
    read_captures: HashMap<String, String>,
    screenshot_artifacts: Vec<Artifact>,
    screenshot_attempts: HashSet<String>,
    /// The ended run, kept for a browser to carry on from.
    resume: Option<Resume>,
}

/// Told once the run's first page is open: see [`Agent::with_page_ready`].
pub type PageReady = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

pub struct Agent {
    page: Arc<dyn Page>,
    jev: Arc<dyn DecisionClient>,
    llm: Arc<dyn LLMClient>,
    config: Config,
    secrets: Option<Arc<dyn SecretResolver>>,
    on_event: Option<EventHandler>,
    page_ready: Option<PageReady>,
    trace: Trace,
    /// The run's secrets, shared with its trace.
    redactor: Arc<Mutex<Redactor>>,
    seen: Mutex<Seen>,
}

impl Agent {
    pub fn new(page: Arc<dyn Page>, jev: Arc<dyn DecisionClient>, llm: Arc<dyn LLMClient>) -> Agent {
        let config = jev.profile().fit(None);
        let redactor = Arc::default();
        Agent {
            page,
            jev: Arc::new(superseding::Decisions(jev)),
            llm: Arc::new(superseding::Generations(llm)),
            config,
            secrets: None,
            on_event: None,
            page_ready: None,
            trace: Trace::disabled().redacting(Arc::clone(&redactor)),
            redactor,
            seen: Mutex::new(Seen::default()),
        }
    }

    /// Run with `config`, its limits brought within the decision model's.
    pub fn with_config(mut self, config: Config) -> Agent {
        self.config = self.jev.profile().fit(Some(config));
        self
    }

    pub fn with_secrets(mut self, secrets: Option<Arc<dyn SecretResolver>>) -> Agent {
        self.secrets = secrets;
        self
    }

    pub fn with_events(mut self, on_event: Option<EventHandler>) -> Agent {
        self.on_event = on_event;
        self
    }

    /// `page_ready` is called once the opening page is open, before the first decision.
    pub fn with_page_ready(mut self, page_ready: Option<PageReady>) -> Agent {
        self.page_ready = page_ready;
        self
    }

    /// Keep the run's secrets in `redactor`, so a trace the run's clients write through is redacted too.
    pub fn with_redactor(mut self, redactor: Arc<Mutex<Redactor>>) -> Agent {
        self.trace = self.trace.redacting(Arc::clone(&redactor));
        self.redactor = redactor;
        self
    }

    pub fn with_trace(mut self, trace: Trace) -> Agent {
        self.trace = trace.redacting(Arc::clone(&self.redactor));
        self
    }

    /// The run just ended, for one with a browser to carry on from. Only a run that reached a page has one.
    pub fn resume(&self) -> Option<Resume> {
        self.seen().resume.take()
    }

    fn seen(&self) -> MutexGuard<'_, Seen> {
        self.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn redactor(&self) -> MutexGuard<'_, Redactor> {
        self.redactor.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn redact(&self, value: &str) -> String {
        self.redactor().redact(value)
    }

    fn store_recipe(&self, state: &RunState, sink: &RecipeSink) {
        let mut sink = sink.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let task = self.redact(&state.given_task);
        let start = state.started_url.as_deref().map(|url| self.redact(url));
        if task != state.given_task || start.as_deref() != state.started_url.as_deref() {
            *sink = None;
            // Redaction changes exact replay identity; retaining the original would persist a resolved secret.
            eprintln!(
                "warning: the run used a resolved secret in its task or start URL, so no replay recipe was saved"
            );
            return;
        }
        let recipe = fastbrowse_core::recipe::Recipe {
            version: fastbrowse_core::recipe::RECIPE_VERSION,
            task,
            start,
            steps: replay::safe_steps(state.recipe_log.clone(), |text| self.reveals_text(text)),
        };
        let (replayed, fell_back) = state.replay.as_ref().map_or((0, 0), |replay| (replay.replayed, replay.fell_back));
        *sink = Some(RecipeOutcome { recipe, replayed, fell_back });
    }

    fn mask(&self, value: &str) -> String {
        self.redactor().mask(value)
    }

    // A task scoped to "the whole site" loses its referent after navigation unless its starting address stays
    // with it. It is stated as what happened, not as an instruction: worded "Start at <url>." it read as one more
    // step no action had taken, and Jev chose DONE over typing a sign-in code it had just read. Mask at the call
    // boundary: a secret may have been resolved since the run began.
    fn model_task(&self, state: &RunState) -> String {
        let task = self.mask(&state.task);
        match state.caller_start.as_deref() {
            Some(start) => format!("The browser opened {} for this task. {task}", self.mask(start)),
            None => task,
        }
    }

    // What the caller asked, without where the run opened. The authorization gate and the done check judge this: the
    // start is not a step the caller authorized or one any action shows, and beside it a form posting to /list/delete
    // under "Tidy it up" scored 0.49 to 0.59 as irreversible, where the task alone scored 0.56 to 0.65.
    fn caller_task(&self, state: &RunState) -> String {
        self.mask(&state.task)
    }

    fn reveals_text(&self, value: &str) -> bool {
        self.redactor().reveals(value)
    }

    fn raw(&self) -> Option<Observation> {
        self.seen().raw.clone()
    }

    fn observed(&self) -> Option<Observation> {
        self.seen().observed.clone()
    }

    fn event(&self, event: &str, fields: impl FnOnce() -> Map<String, Value>) {
        self.trace.event(event, fields);
    }

    /// Pursue `task`.
    pub async fn run(&self, task: &str, mut options: RunOptions) -> Result<RunResult, AgentError> {
        let ledger = match options.ledger.take() {
            Some(ledger) => ledger,
            None => Arc::new(Ledger::new(options.limits.take().unwrap_or_default())),
        };
        let began = Instant::now();
        ledger.set_started(Some(began));
        {
            let mut seen = self.seen();
            seen.artifact_start = self.page.artifacts().len();
            seen.screenshot_artifacts.clear();
            seen.screenshot_attempts.clear();
        }
        // The ledger checks `max_seconds` between operations; only a deadline around the awaits bounds a browser or
        // provider call that never returns.
        let deadline = ledger.limits.max_seconds.map(|seconds| began + Duration::from_secs_f64(seconds));
        let mut slot: Option<RunState> = None;
        let mut requested_frame = false;
        let mut changed: Option<String> = None;
        let resume = options.resume.take();
        let spending = jev_spend(Lines::Ledger(ledger.clone()), Some(ledger.clone()), async {
            let running = Box::pin(async {
                let body = Box::pin(self.body(task, &options, resume, &ledger, &mut slot));
                match body.await {
                    Ok(result) => (result, true, false),
                    Err(Flow::Browser(BrowserError::NeedsBrowser(message))) => {
                        (blocked_without_browser(message), false, true)
                    }
                    Err(flow) => (
                        self.stopped(flow, slot.as_mut(), &ledger, options.output_schema.as_deref()).await,
                        false,
                        false,
                    ),
                }
            });
            // Reporting can await providers too, so the same deadline must cover the stop path.
            let outcome = match deadline {
                Some(deadline) => tokio::time::timeout_at(deadline.into(), running).await.ok(),
                None => Some(running.await),
            };
            let ended = match outcome {
                Some(ended) => ended,
                None => {
                    let limit = ledger.stated().max_seconds.unwrap_or_default();
                    let notes = slot.as_ref().map(|state| state.notes.clone()).unwrap_or_default();
                    let mut result = self.partial_result(
                        &notes,
                        slot.as_ref(),
                        &ledger,
                        Status::BudgetExceeded,
                        Some(time_limit(limit)),
                        Some(BudgetStop { resource: BudgetResource::Seconds, limit }),
                    );
                    result.failure_kind = Some(BROWSING_TIME.into());
                    (result, false, false)
                }
            };
            // A run that ends before it acts still records what its known pages request cost. One handed on to a
            // browser takes the request with it, and the browser run records it.
            if !ended.2
                && let Some(state) = slot.as_mut()
            {
                state.known_pages_unused().await;
            }
            requested_frame = slot.as_ref().is_some_and(|state| state.output_intent.attaches_screenshot());
            if let (Some(sink), Some(state)) = (&options.recipe_sink, &slot) {
                self.store_recipe(state, sink);
            }
            changed = slot.as_ref().and_then(|state| finishing::changed_fields(&state.history));
            if let Some(state) = slot.take() {
                let url = self.page.browser_address().or_else(|| self.raw().map(|page| page.url));
                self.seen().resume = Some(Resume { state: Box::new(state), url });
            }
            ended
        });
        // A question the run already asked beside the same state is answered from what Jev said then.
        let scoped = crate::batches::knowing(Default::default(), spending);
        let (mut result, mut loop_returned, needs_browser) = self.trace.phase("run", scoped).await;
        let screenshot_artifacts = std::mem::take(&mut self.seen().screenshot_artifacts);
        let result = if needs_browser {
            // A run that needs a browser reports what the ledger spent and nothing else: no budget check and no ending
            // frame.
            result.artifacts.extend(screenshot_artifacts);
            result.cost = ledger.breakdown();
            result
        } else {
            self.keep_captures(&mut result).await;
            if over_spend(&mut result, &ledger) {
                loop_returned = false;
            }
            if result.status.waits_on_person() {
                self.page.hand_over_tab();
            }
            result.cost = ledger.breakdown();
            if loop_returned { result } else { self.ending_frame(result, requested_frame).await }
        };
        Ok(self.with_changes(result, changed))
    }

    /// `result` with the fields its run changed named first in its answer when the run ended without finishing (#939).
    /// Every result leaves the run here, once its status is final, so no path that builds one can drop them.
    fn with_changes(&self, mut result: RunResult, changed: Option<String>) -> RunResult {
        if result.status != Status::Complete
            && let Some(changed) = changed
        {
            let changed = self.redact(&changed);
            result.answer = Some(match result.answer.take() {
                Some(answer) => format!("{changed}\n\n{answer}"),
                None => changed,
            });
        }
        // Spend checks and final screenshots can change an earlier result's status.
        self.event("run_finished", || {
            Map::from_iter([
                ("status".into(), json!(result.status)),
                ("error_present".into(), result.error.is_some().into()),
                ("budget_resource".into(), json!(result.budget.as_ref().map(|budget| budget.resource))),
                ("wall_kind".into(), json!(result.wall.as_ref().map(|wall| wall.kind))),
                ("steps".into(), result.steps.len().into()),
                ("artifacts".into(), result.artifacts.len().into()),
                ("final_frame".into(), result.final_frame.is_some().into()),
            ])
        });
        result
    }

    /// Keeps each page capture the result's evidence quotes as a `capture` artifact, so a citation can be checked
    /// against the text it was read from, and names that artifact on the evidence.
    async fn keep_captures(&self, result: &mut RunResult) {
        let mut kept: HashMap<String, Option<String>> = HashMap::new();
        for index in 0..result.evidence.len() {
            let digest = result.evidence[index].capture_sha256.clone();
            if !kept.contains_key(&digest) {
                let artifact = self.keep_capture(&digest).await;
                kept.insert(digest.clone(), artifact.as_ref().map(|artifact| artifact.uri.clone()));
                result.artifacts.extend(artifact);
            }
            result.evidence[index].capture_artifact = kept[&digest].clone();
        }
    }

    /// The artifact a read capture is kept as, if the run read it and the page has somewhere to keep it.
    async fn keep_capture(&self, digest: &str) -> Option<Artifact> {
        let text = self.seen().read_captures.get(digest).cloned()?;
        match self.page.keep_capture(&format!("capture-{digest}.txt"), &text).await {
            Ok(artifact) => artifact,
            Err(error) => {
                self.event("capture_not_kept", || Map::from_iter([("error".into(), error.to_string().into())]));
                None
            }
        }
    }

    async fn tell_page_ready(&self) {
        if let Some(page_ready) = &self.page_ready {
            page_ready().await;
        }
    }

    /// The run up to its loop's result: the opening page, the run state, then the loop.
    async fn body(
        &self,
        task: &str,
        options: &RunOptions,
        resume: Option<Resume>,
        ledger: &Arc<Ledger>,
        slot: &mut Option<RunState>,
    ) -> Result<RunResult, Flow> {
        let prepared = match resume {
            Some(Resume { state, url: Some(url) }) => {
                self.trace.phase("open", self.page.navigate(&url, None)).await?;
                self.tell_page_ready().await;
                let state = slot.insert((*state).carried_on(ledger.clone()));
                return Box::pin(self.run_loop(state, options.output_schema.as_deref(), options.until.as_ref())).await;
            }
            Some(Resume { state, url: None }) => Some(Box::new((*state).carried_on(ledger.clone()))),
            None => None,
        };
        let start = options.start.as_deref();
        let mut opening = prepared.as_ref().and_then(|state| state.started_url.clone());
        // `start=None` leaves the browser where it is, which is what a caller stepping a run on from a page it opened
        // itself wants. `choose_start` is the other case: a caller with a goal and no page at all, who wants the first
        // address worked out from the task.
        let open = async {
            let mut chosen = false;
            if opening.is_none() {
                opening = match start {
                    Some(start) => Some(start.to_string()),
                    None if options.choose_start => {
                        chosen = true;
                        self.first_page(task, ledger).await?
                    }
                    None => None,
                };
            }
            // Left holding the address the run began with when none opens, so a run that moves to a browser opens it
            // instead of choosing its start page again.
            if let Some(first) = opening.as_mut() {
                self.open_start(first, chosen, task, ledger).await?;
            }
            if let (None, Some(opening)) = (start, &opening) {
                // The caller gave a goal and no page, so this address was worked out from the task.
                self.front_page_if_blank(opening, browsing_deadline(ledger, false)).await?;
            }
            Ok::<_, Flow>(())
        };
        // The composer's context window is looked up from the provider's listing and kept by the client. Asked only
        // when the answer is written, the lookup held every run's last step for 0.5 to 2.7 s; asked now, it is known
        // by then, and the run never waits on it here.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let llm = self.llm.clone();
            runtime.spawn(async move { llm.context_tokens(fastbrowse_core::models::LLMPurpose::Compose).await });
        }
        let prepare = async {
            if let Some(state) = prepared {
                return Ok::<_, Flow>(state);
            }
            let given = state::with_inputs(task, &options.inputs);
            // The known pages are asked beside the first page's open, observation and decision, and taken before the
            // run first reads for, builds or acts on a decision, or proposes a route (`known_pages_arrived`). Awaited
            // here, the text LLM's answer held every run's first step, for about a second and up to 3.4 s. A run with a
            // dollar cap still waits: a call checks that cap against the receipts already recorded, and this one's comes
            // later. Call caps need no wait, since reserving counts the call.
            let asked =
                ledger.limits.max_dollars.is_none().then(|| tokio::runtime::Handle::try_current().ok()).flatten().map(
                    |runtime| {
                        let (llm, given, start, ledger) =
                            (self.llm.clone(), given.clone(), start.map(String::from), ledger.clone());
                        state::KnownAsked::new(runtime.spawn(async move {
                            crate::start::known_pages(&*llm, &given, start.as_deref(), Some(&ledger)).await
                        }))
                    },
                );
            let (judgments, known) = if asked.is_some() {
                // Polled once, the request reserves its call and leaves now, as it did when it was awaited here: a
                // current-thread runtime would otherwise first poll it when the run waits on I/O.
                tokio::task::yield_now().await;
                (self.task_judgments(&given, ledger).await, Ok(Vec::new()))
            } else {
                tokio::join!(self.task_judgments(&given, ledger), self.known_pages(&given, start, ledger))
            };
            // Dropped on an error, the request is aborted and records what it was billed so far.
            let (judgments, known) = (judgments?, known?);
            let mut state = RunState::new(
                task.to_string(),
                options.inputs.clone(),
                options.attachments.clone(),
                options.authorization.clone().unwrap_or_default(),
                ledger.clone(),
            );
            state.dates = judgments.dates;
            state.output_answers = judgments.output;
            state.known_pages = known;
            state.known_asked = asked;
            state.replay = options.recipe.clone().map(replay::Replay::new);
            state.context = options.context.clone();
            state.caller_start = start.map(String::from);
            Ok(Box::new(state))
        };
        let (opened, prepared) = tokio::join!(open, prepare);
        let prepared = prepared.map(|mut state| {
            state.started_url = opening;
            state
        });
        // HTTP can need a browser before any capture exists. Keep task judgments for that handoff, without
        // treating other opening failures as a run that reached its first page.
        if let Err(flow) = opened {
            match prepared {
                Ok(state) if matches!(&flow, Flow::Browser(BrowserError::NeedsBrowser(_))) => {
                    self.seen().resume = Some(Resume { state, url: None });
                }
                Ok(mut state) => state.known_pages_unused().await,
                Err(_) => {}
            }
            return Err(flow);
        }
        let state = slot.insert(*prepared?);
        self.tell_page_ready().await;
        if let Some(opening) = state.started_url.clone() {
            let landed = self.page.address().await?;
            state.opened = Some(HistoryEntry {
                effect: Some(format!("went to {landed}")),
                ..HistoryEntry::new(Some(Operation::Navigate), Some(&opening), StepOutcome::Executed, true)
            });
            state.start_landing_url = Some(landed);
        }
        Box::pin(self.run_loop(state, options.output_schema.as_deref(), options.until.as_ref())).await
    }

    /// The result of a run a step stopped: the error the loop raised, as the status it means.
    async fn stopped(
        &self,
        flow: Flow,
        mut state: Option<&mut RunState>,
        ledger: &Ledger,
        output_schema: Option<&RecordSchema>,
    ) -> RunResult {
        let schema = state.as_deref_mut().and_then(|state| state.schema_partial.take());
        // Browser and other agent errors name their kind where `failed` reports them; a stop names its rule.
        let kind = match &flow {
            Flow::Stop(stop) => Some(stop.rule.clone()),
            Flow::Agent(error) => Some(error.failure_kind()),
            Flow::Browser(_) | Flow::Unsure(_) => None,
        };
        let mut result = self.stopped_result(flow, state.as_deref_mut(), ledger, output_schema).await;
        // A stop whose evidence answers the task after all ends complete, and a complete run names no failure.
        if result.status != Status::Complete && result.failure_kind.is_none() {
            result.failure_kind = kind;
        }
        // The schema was filled before the draft was sent back; a rewrite that fails too does not unfill it.
        if let Some(extraction) = schema
            && result.data.is_null()
        {
            result.data = extraction.data.unwrap_or(Value::Null);
            result.evidence.extend(extraction.evidence);
            let notes = state.as_deref().map(|state| state.notes.clone()).unwrap_or_default();
            result.evidence = finishing::one_citation_per_quote(result.evidence, &notes);
        }
        result
    }

    async fn stopped_result(
        &self,
        flow: Flow,
        state: Option<&mut RunState>,
        ledger: &Ledger,
        output_schema: Option<&RecordSchema>,
    ) -> RunResult {
        let (flow, exhausted) = match flow {
            Flow::Agent(AgentError::Budget(error)) => (
                Flow::Stop(state::Stop::new(Status::BudgetExceeded, error.failure_kind(), error.to_string())),
                Some(error.budget),
            ),
            flow => (flow, None),
        };
        if let Flow::Stop(stop) = &flow
            && let Some(state) = state
        {
            // A stop's message can quote the page, such as a field's label, and a page can echo a typed secret there;
            // redacting at each raise missed one, so every stop is redacted here before the caller sees it.
            let error = stop.error.as_deref().map(|error| self.redact(error));
            // Loop clock stops have no typed budget error; other limits retain their original resource.
            let budget = exhausted.or_else(|| {
                (stop.status == Status::BudgetExceeded).then(|| BudgetStop {
                    resource: BudgetResource::Seconds,
                    limit: ledger.stated().max_seconds.unwrap_or_default(),
                })
            });
            if state.composition_partial.is_some() {
                let notes = state.notes_to_compose();
                if let Some(partial) = state.checked_composition(&notes) {
                    let mut result = self.answered(partial, &notes, Some(state), ledger, stop.status, error, budget);
                    result.wall = self.wall(stop);
                    result.evidence.extend(stop.evidence.clone());
                    result.evidence = finishing::one_citation_per_quote(result.evidence, &state.notes);
                    return result;
                }
            }
            if matches!(stop.status, Status::Stuck | Status::Unverified | Status::BudgetExceeded)
                && !state.notes.evidence().is_empty()
            {
                // What was read is still cited evidence; a caller told only "stuck" has to browse again for it.
                let notes = if state.compose { state.notes_to_compose() } else { state.notes.clone() };
                // Records listed one after another answer no count: a list read to its end came back as a dump of it.
                let reporting = ledger.for_answer();
                let steps = state.executed_steps();
                let kept = state.committed_steps();
                // A draft rejected as the run stopped is a rewrite's to correct: written without the rejection, after a
                // stall had spent the recovery that would have sent it back, this was the first request byte for byte,
                // and it returned the rejected draft again (#1377).
                let (task, feedback) = (self.model_task(state), state.composition_feedback.clone());
                if state.compose
                    && let Ok(composed) = compose_reviewed(
                        &*self.jev,
                        &*self.llm,
                        &task,
                        &notes,
                        Some(&reporting),
                        feedback.as_deref(),
                        true,
                        self.screenshot(state.output_intent),
                        &steps,
                        &kept,
                    )
                    .await
                    .map(|reviewed| reviewed.answer)
                    && !composed.claims.is_empty()
                {
                    // Only a rejected composition stops a run unverified, so one that passes every claim and coverage
                    // check, and that answers what that stop was about, as it would
                    // have at done. A run stuck going back and
                    // forth over pages it had read held such an answer too, and came back stuck under "Partial
                    // evidence only": when the task asks only to be told, the stop was over navigation, not the
                    // answer. A task asking for something done is not done because what was read holds.
                    // A step the site refused is still not carried out, however well the answer holds (#779).
                    let told = matches!(
                        state.output_intent,
                        state::OutputIntent::Text | state::OutputIntent::Image | state::OutputIntent::TextAndImage
                    );
                    let mut answers = match stop.status {
                        Status::Unverified => true,
                        Status::Stuck => told,
                        _ => false,
                    } && composed.problem.is_none()
                        && !state.refused
                        && state.undone.is_none();
                    // A task asking to be told can also ask for steps, such as setting a field and then reporting it,
                    // and a run stuck before any finish check judged its steps may have left one undone: that check's
                    // own question decides, asked of the last page.
                    if answers
                        && stop.status == Status::Stuck
                        && (state.history.iter().any(HistoryEntry::acts) || state.asks_steps)
                    {
                        answers = match self.observed() {
                            Some(page) => matches!(self.actions_done(state, &page).await, Ok(Carried::Done)),
                            None => false,
                        };
                    }
                    // A schema run's data is drawn from this answer, as at done: the extraction kept from the draft it
                    // replaces holds only that draft's passed claims, so a value the rewrite corrected was in the prose
                    // and missing from the data of a complete result. Data that cannot be drawn keeps the stop's status.
                    let extraction = match output_schema {
                        _ if !answers => None,
                        None => Some(None),
                        Some(_) => match self.schema_fields(state, output_schema, Some(&composed)).await {
                            Ok(Some(extraction)) if extraction.problem.is_none() => Some(Some(extraction)),
                            _ => None,
                        },
                    };
                    if let Some(extraction) = extraction {
                        let mut result =
                            self.answered(&composed, &notes, Some(state), ledger, Status::Complete, None, None);
                        if let Some(extraction) = extraction {
                            result.data = extraction.data.unwrap_or(Value::Null);
                            result.evidence.extend(extraction.evidence);
                            result.evidence = finishing::one_citation_per_quote(result.evidence, &state.notes);
                        }
                        return self.ending_frame(result, state.output_intent.attaches_screenshot()).await;
                    }
                    // A draft that does not answer keeps the stop's own status.
                    let partial = marked_partial(composed);
                    let error = error.or_else(|| state.refused.then(|| self.redact(finishing::REFUSED)));
                    return self.answered(&partial, &notes, Some(state), ledger, stop.status, error, budget);
                }
                // A task that only carries something out reports what it read after its last step, as at done, and
                // whether it is one is asked only when that would leave out an answer read before such a step. Asking
                // can fail on a spent budget, and the notes then answer as they stand.
                if state.output_intent == state::OutputIntent::Unknown
                    && outcome_notes(state).supporting().len() < state.notes.supporting().len()
                {
                    let _ = self.output_intent(state).await;
                }
                return self.partial_result(&notes, Some(state), ledger, stop.status, error, budget);
            }
            let wall = self.wall(stop);
            let outcome = Outcome { error, evidence: stop.evidence.clone(), wall, budget, ..Outcome::default() };
            return self.result(Some(state), ledger, stop.status, outcome);
        }
        let state = state.map(|state| &*state);
        let notes = || state.map(|state| state.notes.clone()).unwrap_or_default();
        match flow {
            Flow::Stop(stop) => {
                let error = stop.error.as_deref().map(|error| self.redact(error));
                let wall = self.wall(&stop);
                self.result(
                    state,
                    ledger,
                    stop.status,
                    Outcome { error, evidence: stop.evidence, wall, budget: exhausted, ..Outcome::default() },
                )
            }
            Flow::Agent(AgentError::NotesTooLarge(message)) => {
                let message = self.redact(&message);
                self.partial_result(&notes(), state, ledger, Status::ObservationLimit, Some(message), None)
            }
            Flow::Agent(AgentError::ObservationTooLarge(message)) => {
                self.partial_result(&notes(), state, ledger, Status::ObservationLimit, Some(message), None)
            }
            Flow::Browser(error) => {
                let unavailable = error.is_unavailable()
                    || matches!(error, BrowserError::PageUnresponsive(_))
                    || (state.is_none()
                        && matches!(error, BrowserError::NavigationTimeout(_) | BrowserError::SiteUnreachable(_)));
                let kind = error.failure_kind();
                self.failed(kind, error.to_string(), unavailable, state, ledger)
            }
            Flow::Agent(error) => {
                let unavailable = error.is_unavailable();
                self.failed(error.failure_kind(), error.to_string(), unavailable, state, ledger)
            }
            Flow::Unsure(unsure) => self.failed("Unsure".into(), unsure.reason, false, state, ledger),
        }
    }

    fn failed(
        &self,
        kind: String,
        message: String,
        unavailable: bool,
        state: Option<&RunState>,
        ledger: &Ledger,
    ) -> RunResult {
        let failure_kind = kind.clone();
        self.event("run_failed", || Map::from_iter([("kind".into(), kind.into())]));
        let mut message = self.redact(&message);
        // `unavailable` tells a caller the same run may pass later, which is safe only while the run has changed
        // nothing: running it again after a post that went through posts twice.
        let changed: Vec<String> = state
            .into_iter()
            .flat_map(|state| &state.history)
            .filter(|entry| entry.changed_site())
            .map(|entry| {
                let operation = entry.operation.map_or("", |operation| operation.as_str());
                entry.target.as_deref().map_or_else(|| operation.to_string(), |target| format!("{operation} {target}"))
            })
            .collect();
        let status = if unavailable && changed.is_empty() { Status::Unavailable } else { Status::Error };
        if unavailable && !changed.is_empty() {
            let steps = changed.join("; ");
            message = format!(
                "{message}. This came after steps that may have changed the site ({steps}), so check the site before running the task again."
            );
        }
        let notes = state.map(|state| state.notes.clone()).unwrap_or_default();
        let mut result = self.partial_result(&notes, state, ledger, status, Some(message), None);
        result.failure_kind = Some(failure_kind);
        result
    }

    async fn ending_frame(&self, mut result: RunResult, requested: bool) -> RunResult {
        if !(self.config.step_frames || requested) {
            return result;
        }
        // A blocked or failed run still needs its ending page, checked for secrets at capture time.
        result.final_frame =
            match tokio::time::timeout(Duration::from_secs_f64(ENDING_FRAME_SECONDS), self.frame()).await {
                Ok(Ok(frame)) => frame,
                _ => None,
            };
        if requested && result.final_frame.is_none() && result.status == Status::Complete {
            result.end_as(
                Status::Unverified,
                "FinalFrame/uncaptured".into(),
                "The requested final screenshot could not be captured.".into(),
            );
        }
        result
    }

    /// A run that could not finish still returns the claims it read, each with its evidence.
    fn partial_result(
        &self,
        notes: &Notes,
        state: Option<&RunState>,
        ledger: &Ledger,
        status: Status,
        error: Option<String>,
        budget: Option<BudgetStop>,
    ) -> RunResult {
        if let Some(state) = state.filter(|state| state.composition_partial.is_some()) {
            let notes = state.notes_to_compose();
            if let Some(partial) = state.checked_composition(&notes) {
                return self.answered(partial, &notes, Some(state), ledger, status, error, budget);
            }
        }
        // A run stuck on a desktop form after choosing Germany cited "United Kingdom", the combo box's value as its
        // first read saw it, before the choice: a task that only carries something out answers, stopped or not, with
        // what it read after its last step that changes the site (#989).
        let outcome = state.filter(|state| state.output_intent == state::OutputIntent::CarriedOut).map(outcome_notes);
        let notes = outcome.as_ref().unwrap_or(notes);
        // A composing run that lost its browser came back with its answering facts unmarked, read as the whole answer to
        // a task it never finished; every stopped run's draft says it is partial (#962).
        match partial_answer(notes) {
            Some(answer) => self.answered(&answer, notes, state, ledger, status, error, budget),
            None => self.result(state, ledger, status, Outcome { error, budget, ..Outcome::default() }),
        }
    }

    /// `partial` as the result of a run that ended as `status`, with the evidence it cites.
    #[allow(clippy::too_many_arguments)]
    fn answered(
        &self,
        partial: &ComposedAnswer,
        notes: &Notes,
        state: Option<&RunState>,
        ledger: &Ledger,
        status: Status,
        error: Option<String>,
        budget: Option<BudgetStop>,
    ) -> RunResult {
        let undone = (status != Status::Complete)
            .then(|| {
                state
                    .and_then(|state| state.undone.as_deref())
                    .map(|step| noticed(&finishing::undone_notice(step), partial.clone()))
            })
            .flatten();
        let partial = undone.as_ref().unwrap_or(partial);
        let cited =
            notes.expand_evidence_ids(partial.claims.iter().flat_map(|c| c.evidence_ids.iter().map(String::as_str)));
        let (answer, citations) = self.public_answer(partial);
        let evidence = notes.evidence().into_iter().filter(|(key, _)| cited.contains(key)).map(|(_, e)| e).collect();
        self.result(
            state,
            ledger,
            status,
            Outcome { answer: Some(answer), citations, evidence, error, budget, ..Outcome::default() },
        )
    }

    fn result(&self, state: Option<&RunState>, ledger: &Ledger, status: Status, outcome: Outcome) -> RunResult {
        let observed = {
            let seen = self.seen();
            seen.raw.clone().or_else(|| seen.observed.clone())
        };
        let start = self.seen().artifact_start;
        let artifacts = self.page.artifacts();
        let mut artifacts = artifacts.get(start..).map(<[_]>::to_vec).unwrap_or_default();
        artifacts.extend(self.seen().screenshot_artifacts.iter().cloned());
        RunResult {
            browser: None,
            status,
            budget: outcome.budget,
            answer: outcome.answer,
            data: outcome.data,
            evidence: finishing::one_citation_per_quote(
                outcome.evidence,
                &state.map(|state| state.notes.clone()).unwrap_or_default(),
            ),
            steps: state.map(|state| state.steps.clone()).unwrap_or_default(),
            cost: ledger.breakdown(),
            artifacts,
            citations: outcome.citations,
            error: outcome.error,
            failure_kind: None,
            final_url: match (state, observed) {
                (Some(_), Some(observed)) => Some(self.redact(&observed.url)),
                _ => None,
            },
            final_frame: None,
            recordings: Vec::new(),
            wall: outcome.wall,
        }
    }

    /// The wall a stop ended at, read from the page the run last observed, so every way a run reaches needs_login
    /// (a field it cannot fill, a page that offers only sign-in, a stuck run standing on a wall, an HTTP 401) names
    /// the same thing. A caller routes on the kind: a one-time code it can fetch, a password only the user has.
    fn wall(&self, stop: &state::Stop) -> Option<Wall> {
        let kind = match (stop.status, stop.wall) {
            (Status::NeedsLogin, hint) | (Status::Blocked, hint @ Some(_)) => hint,
            _ => return None,
        };
        let (raw, observed) = {
            let seen = self.seen();
            (seen.raw.clone(), seen.observed.clone())
        };
        let page = observed.as_ref().or(raw.as_ref());
        let origin = page.map_or_else(String::new, |page| self.redact(&origin_of(&page.url)));
        let mut credentials: Vec<CredentialField> = Vec::new();
        let mut fields: Vec<String> = Vec::new();
        if kind != Some(WallKind::BotCheck)
            && let Some(raw) = &raw
        {
            // Which fields ask for credentials is read from the raw page, whose flags masking cannot have changed;
            // their labels from what models saw, as the error quotes them.
            for control in &raw.controls {
                let Some(field) = credential_field(control) else { continue };
                credentials.push(field);
                let shown = observed.as_ref().and_then(|o| o.controls.iter().find(|c| c.id == control.id));
                let label = shown.map_or(&control.label, |c| &c.label);
                if !label.is_empty() {
                    fields.push(self.redact(label));
                }
            }
        }
        // A page offering nothing to act on cannot be signed into (#635): a refusal that leaves no way in is a bot or
        // access check, and naming it `sign_in` sends a caller to a password that cannot pass it.
        let kind = match kind {
            Some(kind) => kind,
            None if raw.as_ref().is_some_and(|raw| raw.controls.is_empty()) => WallKind::BotCheck,
            None => wall_kind(&credentials),
        };
        Some(Wall { kind, origin, fields, seen_at_ms: now_ms() })
    }

    fn public_fact(&self, fact: &Fact, notes: &Notes) -> StepFact {
        let text = self.redact(&fact.text);
        let Some(evidence) = &fact.evidence else {
            return StepFact { text, quote: None, url: None, reader: fact.reader, deep_link: None };
        };
        let (quote, deep_link) = cited_quote(evidence, notes, |value| self.redact(value));
        StepFact {
            text,
            quote: Some(quote),
            url: Some(self.redact(&evidence.url)),
            reader: fact.reader,
            deep_link: Some(deep_link),
        }
    }

    /// The answer and its citations with secrets redacted, links included.
    ///
    /// A link percent-encodes its quote, where redacting the text cannot see a secret, so each link is rebuilt from the
    /// redacted address and quote, and only the prose between links is redacted as text.
    fn public_answer(&self, composed: &ComposedAnswer) -> (String, Vec<Citation>) {
        let mut citations = Vec::new();
        let mut links: IndexMap<String, String> = IndexMap::new();
        for citation in &composed.citations {
            let (url, quote) = (self.redact(&citation.url), self.redact(&citation.quote));
            let deep_link = if citation.deep_link == citation.url {
                url.clone()
            } else {
                text_fragment(&url, &quote).unwrap_or_else(|_| url.clone())
            };
            links.insert(citation.deep_link.clone(), deep_link.clone());
            citations.push(Citation { text: self.redact(&citation.text), url, quote, deep_link, ..citation.clone() });
        }
        // Every link destination in one pass. Replacing one link at a time rewrote the start of any longer link sharing
        // its prefix, which then no longer matched and kept its percent-encoded secret.
        // Split keeps each destination at an odd index, between the prose around it.
        let answer: String = helpers::split_answer_links(&composed.linked_answer)
            .into_iter()
            .enumerate()
            .map(|(n, piece)| {
                if n % 2 == 1 {
                    format!("](<{}>)", links.get(piece).cloned().unwrap_or_else(|| piece.to_string()))
                } else {
                    self.redact(piece)
                }
            })
            .collect();
        (answer, citations)
    }

    /// The output intent, once something read stands as an answer: whether it is one depends on it, and the intent is
    /// otherwise judged only where the run needs it, which a read of the start page could skip.
    async fn resolve_answer_intent(&self, state: &mut RunState) -> state::Step {
        if state.notes.answered() {
            self.output_intent(state).await?;
        }
        Ok(())
    }

    /// The notes the run would answer with now. A task that only carries something out answers with what it read
    /// after its last step that changes the site: a map's start page read before its pin was pressed stood as the
    /// answer "Map", and the step after the right pin's click, told the task was answered, pressed the map again.
    fn answer_notes(state: &RunState) -> std::borrow::Cow<'_, Notes> {
        if state.output_intent == state::OutputIntent::CarriedOut {
            std::borrow::Cow::Owned(outcome_notes(state))
        } else {
            std::borrow::Cow::Borrowed(&state.notes)
        }
    }

    fn decision_notes(&self, state: &RunState) -> String {
        // Retained evidence can contain a requested value that the selected answer leaves out.
        let selected = (!state.compose && !state.extraction_pending)
            .then(|| draft_answer(&Self::answer_notes(state)))
            .flatten()
            .map(|answer| self.public_answer(&answer).0);
        fastbrowse_core::py::json::dumps(&json!({
            "selected_answer": selected,
            "composition_pending": state.compose,
            "structured_output_pending": state.extraction_pending,
            "retained_notes": state.notes.render_for_navigation(),
        }))
    }

    fn context(
        &self,
        state: &RunState,
        observation: &Observation,
        secrets: Vec<String>,
        check_login: bool,
        check_bot: bool,
    ) -> Result<StepContext, Flow> {
        let mut page_facts = Self::page_facts(state, observation, self.page.unread());
        let page_origin = origin_of(&observation.url);
        let credential_origins: BTreeSet<_> = observation
            .controls
            .iter()
            .filter(|control| asks_for_credentials(control))
            .map(|control| self.target_origin(control))
            .collect();
        for origin in credential_origins {
            let available =
                if origin == page_origin { !secrets.is_empty() } else { !self.secret_names(&origin).is_empty() };
            if !available {
                page_facts.push(if origin == page_origin {
                    "No stored secrets are available for this page's origin.".into()
                } else {
                    format!("No stored secrets are available for credential fields at {}.", self.mask(&origin))
                });
            }
        }
        // Historical text is fitted from the end, before the current credential availability fact.
        if let Some(source) = self.navigation_fact(state, observation, check_login) {
            page_facts.push(source);
        }
        Ok(StepContext {
            task: self.model_task(state),
            literal_task: Some(state.task.clone()),
            web_search: self.page.search_price().is_some(),
            subgoal: state.hint.clone(),
            dates: state.dates.clone(),
            notes: self.decision_notes(state),
            answered_in_notes: Self::answer_notes(state).answered(),
            history: state.history.clone(),
            check_login,
            check_answer: check_bot && !state.read_pages.contains(&observation.page_key),
            check_bot,
            // Set by the step loop, which knows the document it last asked in.
            check_surface: false,
            has_attachments: !state.attachments.is_empty(),
            secrets,
            start_url: state.caller_start.clone(),
            start_landing_url: state.start_landing_url.clone(),
            known_pages: state
                .known_pages
                .iter()
                .filter(|url| !state.reached_known_pages.contains(url))
                .cloned()
                .collect(),
            background: state.context.clone(),
            omitted_actions: 0,
            problem: None,
            withheld_targets: Vec::new(),
            // A page read and unchanged since has nothing more to give a read.
            withheld: if Self::page_read(state, observation) {
                vec![fastbrowse_core::models::Operation::Read]
            } else {
                Vec::new()
            },
            no_value_fields: state
                .missing
                .iter()
                .map(|(_, label, _)| label.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            page_facts,
            reload: state.reload_target(&observation.url),
            screenshot: None,
            looks: IndexMap::new(),
            ahead: self.ahead(state, observation),
            plan_ahead: self.config.plan_ahead,
            list_read: crate::policy::ListRead::None,
        })
    }

    /// How the run reached the page `observation` shows, as the action and the page it was taken on; None for a page it
    /// did not navigate to.
    fn reached_by(&self, state: &RunState, observation: &Observation) -> Option<String> {
        let (destination, source) = state.navigation_from.as_ref()?;
        let action = state.navigated_by.as_deref().filter(|_| *destination == state::read_key(observation))?;
        Some(self.mask(&format!("{action} on {}", source.url)))
    }

    /// A wall on the route just opened does not describe the alternatives observed before it.
    ///
    /// The source page's text and controls are given only while a wall is suspected on the destination: the sign-in
    /// question is asked on this step, the destination's sign-in reading is above even odds, or it answered with a
    /// 4xx. The sign-in judgment is what uses them. Otherwise the fact names where the run came from.
    fn navigation_fact(&self, state: &RunState, observation: &Observation, check_login: bool) -> Option<String> {
        let (destination, source) = state.navigation_from.as_ref()?;
        if *destination != state::read_key(observation) {
            return None;
        }
        let wall = check_login
            || state.login_readings.get(destination).is_some_and(|p| *p > fastbrowse_core::decision::EVEN_ODDS)
            || observation.response_status.is_some_and(|status| (400..500).contains(&status));
        if !wall {
            let previous = json!({"page": {"url": self.mask(&source.url), "title": self.mask(&source.title)}});
            return Some(format!(
                "The latest navigation began on this observed page. This source is historical context, not content of the current page: {previous}"
            ));
        }
        let controls: Vec<Value> = source
            .controls
            .iter()
            .map(|control| {
                json!({
                    "label": self.mask(&control.label),
                    "role": self.mask(&control.role),
                    "context": control.context.as_deref().map(|value| self.mask(value)),
                    "href": control.href.as_deref().map(|value| self.mask(value)),
                    "operations": control.operations,
                })
            })
            .collect();
        let previous = json!({
            "page": {
                "url": self.mask(&source.url),
                "title": self.mask(&source.title),
                "text": self.mask(&source.viewport_text),
                "offscreen_text": self.mask(&source.offscreen_text),
                "response_status": source.response_status,
            },
            "login_required_probability": state.login_readings.get(&state::read_key(source)),
            "controls": controls,
        });
        Some(format!(
            "The latest navigation began on this observed page. These controls were available there before navigation; their current state is unknown. This source is historical context, not content of the current page: {previous}"
        ))
    }

    /// This page was read and no action has changed it since.
    pub(super) fn page_read(state: &RunState, observation: &Observation) -> bool {
        !observation.document_key.is_empty() && state.read_documents.get(&state::read_key(observation)) == Some(&false)
    }

    /// Whether this step already judged that the page holds evidence for the task: Jev said it shows a fact the task
    /// needs, or the run is composing from records and this page is unread. A read of such a page is not asked again
    /// whether it holds any: the run that read ten pages of quotes for a count was told page 10 held none by its read,
    /// at 0.58 against the step's 0.89, and its answer said page 10 was never read.
    pub(super) fn page_shows(state: &RunState, observation: &Observation, decision: &crate::policy::Decision) -> bool {
        decision.shows_answer.is_some_and(|probability| probability > fastbrowse_core::decision::EVEN_ODDS)
            || (state.compose && !Self::page_read(state, observation))
    }

    /// Whether the page is read before the action Jev chose takes the run off it, and if so whether the read may take
    /// it that the page holds evidence for the task.
    ///
    /// Jev's operation can leave a page, or end the run, before anything it shows has been read: an order total is
    /// gone once the order is placed. Jev has said the page shows what the task needs, so the read is not asked again
    /// whether it does: the page of a list judged absent was a third of the list lost. Once a read has said the answer
    /// is written from several records, every new page is one more of them, however it came: a shop's fifth page
    /// scored under even odds, and a list's More changes no address. The page the run began on is read whatever Jev
    /// scored it: asked to count a site's Einstein quotes, Jev scored the quote list under even odds, left it for an
    /// author page, and never came back to the list. Only that read may find nothing: on a bookshop's home page, a
    /// read that could not say so picked the category link "Travel" as which Travel book costs most.
    pub(super) fn reads_before_leaving(
        state: &RunState,
        observation: &Observation,
        decision: &crate::policy::Decision,
    ) -> Option<bool> {
        let unread = !Self::page_read(state, observation);
        let first = state.first_url.as_deref() == Some(observation.url.as_str());
        let shows = Self::page_shows(state, observation, decision);
        (decision.operation != fastbrowse_core::models::Operation::Read && (shows || (first && unread)))
            .then_some(shows)
    }

    /// What code knows about this page that bears on reading it or leaving it. Jev decides what to do with each.
    /// `unread` is what a browser would show that the page did not take in, as a document fetched over HTTP leaves out.
    fn page_facts(state: &RunState, observation: &Observation, unread: Option<Unread>) -> Vec<String> {
        let mut facts = Vec::new();
        if let Some(status) = observation.response_status.filter(|status| *status >= 400) {
            facts.push(format!("This page answered HTTP {status}."));
        }
        if Self::page_read(state, observation) {
            facts
                .push("This page was read and has not changed since: the notes hold what a read takes from it.".into());
        }
        // A link a script adds, or one an unread frame holds, is missing from markup read without a browser, so a page
        // whose markup has none is not known to lead nowhere: example.com's served page builds its one link with a
        // script, and a run told the page led nowhere ended complete without following it (#1380). Told that the
        // scripts did not run and that a step opens a browser, Jev picked a wait over done on the same request; told
        // that no browser can open, done and read led again. The cost is a browser opened for a page whose scripts
        // add nothing: on a task its markup answers, done stayed Jev's pick either way.
        match unread {
            _ if !observation.controls.is_empty() => {}
            None => facts.push("This page offers no links or controls: nothing on it leads anywhere else.".into()),
            Some(unread) => facts.push(unread_fact(unread)),
        }
        if !crate::policy::drawn_surfaces(&observation.controls).is_empty() {
            facts.push("An element marked `drawn` is a drawn surface, such as a streamed remote browser or a canvas: what it shows is pixels, with no elements inside it for this run to fill or press. A click on it lands on the surface as one element, not on a field or button drawn inside it.".into());
        }
        let document = &observation.document_key;
        let read = state.read_documents.get(&state::read_key(observation)).copied();
        if !document.is_empty() && read == Some(true) {
            facts.push(
                "This page changed after it was last read, so the notes taken from it predate the change.".into(),
            );
        }
        facts.extend(helpers::unsent_fact(&state.history, observation));
        if state.to_carry_out {
            facts.extend(helpers::empty_fields_fact(&state.missing, observation));
        }
        if state.compose {
            facts.extend(helpers::page_change_fact(&state.history, observation));
        }
        if let Some(failure) = state.site_failure.as_ref().filter(|failure| failure.transient()) {
            let stood = state.steps.len().saturating_sub(failure.since);
            let mut fact = format!(
                "The site failed: {} It has stood for {stood} steps. A failure is not the task's answer, and neither is an empty result shown beside it.",
                failure.message()
            );
            if failure.url == observation.url {
                fact += " This page's own address is offered to navigate to, which reloads it; a retry control the page offers, or another route, may also get past the failure.";
            }
            facts.push(fact);
        }
        facts
    }

    /// Stored secrets this origin may receive. With any, a sign-in wall is a step to take, not a stop.
    fn secret_names(&self, origin: &str) -> Vec<String> {
        let Some(secrets) = &self.secrets else { return Vec::new() };
        secrets
            .available()
            .into_iter()
            .filter(|secret| secret_allowed(secret, origin))
            .map(|secret| secret.name)
            .collect()
    }
}

/// The parts of a `RunResult` a run's ending fills in.
#[derive(Default)]
struct Outcome {
    answer: Option<String>,
    data: Value,
    evidence: Vec<Evidence>,
    citations: Vec<Citation>,
    error: Option<String>,
    budget: Option<BudgetStop>,
    wall: Option<Wall>,
}

/// The fact a page read without a browser states when its markup has no links or controls: what it left out, and
/// whether a step on it opens a browser that shows the rest.
fn unread_fact(unread: Unread) -> String {
    let frames = match unread.frames {
        0 => String::new(),
        1 => "1 frame in it was not read".into(),
        n => format!("{n} frames in it were not read"),
    };
    let missing = match (unread.scripts, frames.is_empty()) {
        (true, true) => "its scripts did not run".to_string(),
        (true, false) => format!("its scripts did not run and {frames}"),
        (false, _) => frames,
    };
    let (shown, unshown) = match (unread.scripts, unread.frames > 0) {
        (true, true) => ("runs its scripts and reads its frames", "runs its scripts or reads its frames"),
        (true, false) => ("runs them", "runs them"),
        _ => ("reads them", "reads them"),
    };
    let step = if unread.browser {
        format!(
            "A read takes in only this markup; a wait, scroll or other step on this page opens it in a browser, which {shown}."
        )
    } else {
        format!("This run cannot open a browser, so no step on this page {unshown}: a read takes in only this markup.")
    };
    format!("This page was read over HTTP: its markup holds no links or controls, but {missing}. {step}")
}

/// The time a run that composes its answer keeps back from its limit to write it: composing waits on one LLM call.
fn compose_reserve(limit: f64) -> f64 {
    (limit * 0.1).min(60.0)
}

/// When the run must stop browsing: its time limit, less the time a run that composes its answer keeps back to write
/// it. None for a run with no time limit.
pub(super) fn browsing_deadline(ledger: &Ledger, compose: bool) -> Option<tokio::time::Instant> {
    let limit = ledger.limits.max_seconds?;
    let browsing = if compose { limit - compose_reserve(limit) } else { limit };
    let left = (browsing - ledger.started()?.elapsed().as_secs_f64()).max(0.0);
    Some(tokio::time::Instant::now() + Duration::from_secs_f64(left))
}

/// A run that composes its answer has browsed for as long as it can and still write it within its time limit.
pub(super) fn out_of_browsing_time(state: &RunState) -> Option<Stop> {
    if !state.compose {
        return None;
    }
    let deadline = browsing_deadline(&state.ledger, true)?;
    let stated = state.ledger.stated().max_seconds.or(state.ledger.limits.max_seconds)?;
    (deadline <= tokio::time::Instant::now())
        .then(|| Stop::new(Status::BudgetExceeded, BROWSING_TIME, time_limit(stated)))
}

/// The kind of a stop the run's clock made, named as a ledger's `BudgetExceeded` on seconds names its own.
const BROWSING_TIME: &str = "Budget/seconds";

/// Whether the run's ledger is over a spend limit once the run is over, ending `result` on that limit if so.
fn over_spend(result: &mut RunResult, ledger: &Ledger) -> bool {
    let Err(error) = ledger.check_spend(0.0) else { return false };
    // A run that already failed keeps its error: a refused Jev key's unpriced line reported the run as over a spend
    // limit instead of naming the key.
    if result.status != Status::Error
        && result.budget.as_ref().is_none_or(|budget| budget.resource != BudgetResource::Seconds)
    {
        result.end_as(Status::BudgetExceeded, error.failure_kind(), error.to_string());
        result.budget = Some(error.budget);
    }
    true
}

fn time_limit(limit: f64) -> String {
    format!("time limit {}s reached", fastbrowse_core::py::json::float_str(limit))
}

/// A run's cost lines, for a result built outside the loop.
pub fn breakdown(lines: Vec<CostLine>) -> CostBreakdown {
    CostBreakdown { lines }
}

/// A run whose page has no browser behind it reached a step only a browser can take. The document was never the
/// problem and a second attempt cannot help, so it ends `blocked` with the reason, carrying nothing it read.
fn blocked_without_browser(message: String) -> RunResult {
    RunResult {
        browser: None,
        status: Status::Blocked,
        budget: None,
        answer: None,
        data: Value::Null,
        evidence: Vec::new(),
        steps: Vec::new(),
        cost: CostBreakdown { lines: Vec::new() },
        artifacts: Vec::new(),
        citations: Vec::new(),
        error: Some(message),
        failure_kind: Some("NeedsBrowser".into()),
        final_url: None,
        final_frame: None,
        recordings: Vec::new(),
        wall: None,
    }
}

fn now_ms() -> i64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
}
