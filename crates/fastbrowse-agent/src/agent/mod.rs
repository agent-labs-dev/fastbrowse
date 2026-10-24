//! Port of `src/fastbrowse/agent.py`, the run loop: Jev chooses each action, the LLM plans, reads, writes and
//! recovers, and code owns every gate.
//!
//! Only `Status::Complete` is success. Anything the loop cannot prove (an answer whose claims fail their checks, a
//! DONE the verifier rejects at the end of the budget) is reported as what it is rather than rounded up.
//!
//! The module docstring of `agent.py` holds the invariants for guessed addresses, readings and holds; `Lineage`,
//! `note_lineage` and `Notes::discredit` hold the detail. Every prompt and Jev request built here is byte for byte the
//! one Python builds for the same inputs, since a run is proven by replaying recorded model traffic keyed on a hash of
//! the request body. Python runs some calls side by side; here they run side by side too, and only their send order
//! can differ.

mod acting;
mod driving;
mod finishing;
pub mod helpers;
mod reading;
mod recovery;
pub(crate) mod state;

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use fastbrowse_clients::validation::{Lines, jev_spend};
use fastbrowse_core::citations::text_fragment;
use fastbrowse_core::config::Config;
use fastbrowse_core::jev::JevClient;
use fastbrowse_core::llm::{Generation, LLMClient};
use fastbrowse_core::models::{
    Attachment, Authorization, BudgetResource, BudgetStop, Citation, CostBreakdown, CostLine, EventHandler, Evidence,
    Limits, MaybeUnavailable, Operation, RunResult, SecretResolver, Status, StepFact, StepOutcome, Tripwire,
    TripwireMode, UntilCheck,
};
use fastbrowse_core::page::{BrowserError, Observation, Page};
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::py::text;
use fastbrowse_core::safety::{Redactor, origin_of, secret_allowed};
use fastbrowse_core::telemetry::{Ledger, Trace};
use indexmap::IndexMap;
use serde_json::{Map, Value};

use helpers::{history, partition_proof, recovery_memory};
pub use helpers::{next_page_control, next_page_notice, read_question};
use state::{Flow, RunState, Spawned};

use crate::error::AgentError;
use crate::memory::{Fact, Notes};
use crate::planner::{Plan, RequirementKind, RunReport, make_plan};
use crate::policy::StepContext;
use crate::retrieval::{ComposedAnswer, assemble_answer, partial_answer};
use crate::shortcut::{Shortcut, propose_shortcut};
use crate::tripwires::{Tripped, repeated_action, stagnant_plan};
use crate::verification::RecordSchema;

/// Long enough for a browser-verification page to run its check and hand over, short enough that a page which never
/// moves still ends as needs_login well inside a run's time budget.
const INTERSTITIAL_SECONDS: f64 = 12.0;
const INTERSTITIAL_POLL_SECONDS: f64 = 0.5;
/// A fresh observation can wait three seconds for loading, and a background capture needs time to activate.
const ENDING_FRAME_SECONDS: f64 = 6.0;
/// How long the first load waits for a shortcut. Flash-lite answers in 0.5 to 1s, and a start page loads in one to
/// four, so a proposal later than this is an outlier and the start page is opened instead.
const SHORTCUT_WAIT_SECONDS: f64 = 2.0;
/// Longer than deciding takes: a watch ends with the work it watches, and this only bounds a stuck one.
const REDRAW_WATCH_SECONDS: f64 = 10.0;
/// Times one action may be taken from one page and still count as progress. Scrolling is exempt: a long page takes
/// many scrolls, each of which shows something new.
const REPEATS_BEFORE_CYCLE: i64 = 2;
const REVERSAL_WINDOW: usize = 6;
/// How much of a wall's text a blocked result quotes: enough for its message, not a whole page.
const WALL_QUOTE_CHARS: i64 = 600;
/// The choice a field's text is written for when no input or secret is the answer.
const GENERATE: &str = "generate";

/// What one run is asked to do, beyond its task: Python's keyword arguments to `Agent.run`.
#[derive(Default)]
pub struct RunOptions {
    pub start: Option<String>,
    pub choose_start: bool,
    pub inputs: IndexMap<String, String>,
    pub attachments: Vec<Attachment>,
    pub output_schema: Option<Arc<RecordSchema>>,
    pub limits: Option<Limits>,
    pub authorization: Option<Authorization>,
    pub until: Option<UntilCheck>,
    pub head_start: Option<HeadStart>,
}

/// The calls a run can make from the task alone, begun before there is a browser to drive.
///
/// Starting a cloud browser and opening its tab takes 2 to 3 seconds, and the plan and the shortcut need only the task
/// and the start page, so `run_task` begins them first: the shortcut is waiting when the tab opens instead of being
/// asked for then. The calls reserve against `ledger`, which the run takes over, so they count against its limits and
/// are billed in its result.
pub struct HeadStart {
    pub ledger: Arc<Ledger>,
    task: String,
    start: Option<String>,
    planning: Arc<Spawned<Generation<Plan>>>,
    proposing: Option<Arc<Spawned<Shortcut>>>,
    plan_billed: bool,
}

impl HeadStart {
    /// Begin the plan and, with a site's entry point to start on, the shortcut. Needs a tokio runtime.
    pub fn begin(llm: Arc<dyn LLMClient>, task: &str, start: Option<&str>, limits: Option<Limits>) -> HeadStart {
        let ledger = Arc::new(Ledger::new(limits.unwrap_or_default()));
        // The run's clock starts when the run takes the ledger over. Until then, a call finishing past `max_seconds` of
        // browser startup would end the run over time the limit does not count.
        ledger.set_started(None);
        // The plan is needed to read, to judge DONE and to answer, and the start page, the first fills and clicks all
        // come before those, so it is written from the task while they run. Its answer checks are written by the same
        // call, so a run can browse as soon as the plan arrives.
        let planning = Arc::new(Spawned::spawn({
            let (llm, ledger, task, start) = (llm.clone(), ledger.clone(), task.to_string(), start.map(String::from));
            async move {
                let planned = make_plan(&*llm, &task, start.as_deref(), Some(&ledger)).await?;
                // The plan's receipt is billed the moment it exists: recorded only when the run took the plan, a cap
                // the plan alone had spent used to let a later paid request through.
                ledger.state().lines.push(planned.cost.clone());
                Ok(planned)
            }
        }));
        let proposing = start
            .filter(|start| shortcut_worth_proposing(start))
            .map(|start| Arc::new(propose(llm, task, start, ledger.clone())));
        HeadStart {
            ledger,
            task: task.to_string(),
            start: start.map(String::from),
            planning,
            proposing,
            plan_billed: true,
        }
    }

    fn tasks(&self) -> Vec<Box<dyn Fn() + Send + Sync + '_>> {
        let mut tasks: Vec<Box<dyn Fn() + Send + Sync + '_>> = vec![Box::new(|| self.planning.cancel())];
        if let Some(proposing) = &self.proposing {
            tasks.push(Box::new(|| proposing.cancel()));
        }
        tasks
    }

    /// Wait for every startup call to finish.
    async fn gather(&self) {
        self.planning.wait().await;
        if let Some(proposing) = &self.proposing {
            proposing.wait().await;
        }
    }

    /// Cancel what is still being written, so nothing bills a run that has already ended.
    pub async fn discard(&self) {
        // One transport can take time to close, so cancel every paid call before joining any cleanup.
        for cancel in self.tasks() {
            cancel();
        }
        self.gather().await;
    }

    /// What a run that never began spent: a shortcut bills itself, and a plan that finished is billed here.
    pub async fn abandon(&self) -> Vec<CostLine> {
        self.discard().await;
        if !self.plan_billed
            && let Some(planned) = self.planning.succeeded()
        {
            self.ledger.state().lines.push(planned.cost);
        }
        self.ledger.state().lines.clone()
    }
}

/// Whether to ask for a direct address from `start`.
///
/// The shortcut synthesizes a deeper page on the start site from the task's own words, worth asking for at a site's
/// entry point, where the task names the page but not its address. When the caller already handed the run a page
/// inside the site (a path, not just the origin) the run is at a destination the caller chose, and a proposal to go
/// elsewhere on that origin second-guesses it.
pub(crate) fn shortcut_worth_proposing(start: &str) -> bool {
    // WHATWG gives a special scheme's empty path as "/", which is the same entry point `urlsplit` reads as "".
    url::Url::parse(start).is_ok_and(|url| matches!(url.path(), "" | "/"))
}

/// The shortcut for `task` on `start`'s site, its receipt recorded where it was made: a proposal that finished is
/// billed even when the run ends first.
fn propose(llm: Arc<dyn LLMClient>, task: &str, start: &str, ledger: Arc<Ledger>) -> Spawned<Shortcut> {
    let (task, start) = (task.to_string(), start.to_string());
    Spawned::spawn(async move {
        let generation = propose_shortcut(&*llm, &task, &start, Some(&ledger)).await?;
        ledger.record([generation.cost])?;
        Ok(generation.data)
    })
}

/// What the agent learns about the page as it goes, kept apart from the run so the page and the models can be asked
/// side by side.
#[derive(Default)]
struct Seen {
    redactor: Redactor,
    secret_on_screen: bool,
    raw: Option<Observation>,
    /// The last observation `observe` returned, which the finish judges again only if the page changed since.
    observed: Option<Observation>,
    artifact_start: usize,
}

pub struct Agent {
    page: Arc<dyn Page>,
    jev: Arc<dyn JevClient>,
    llm: Arc<dyn LLMClient>,
    config: Config,
    secrets: Option<Arc<dyn SecretResolver>>,
    on_event: Option<EventHandler>,
    trace: Trace,
    seen: Mutex<Seen>,
}

impl Agent {
    pub fn new(page: Arc<dyn Page>, jev: Arc<dyn JevClient>, llm: Arc<dyn LLMClient>) -> Agent {
        Agent {
            page,
            jev,
            llm,
            config: Config::default(),
            secrets: None,
            on_event: None,
            trace: Trace::disabled(),
            seen: Mutex::new(Seen::default()),
        }
    }

    pub fn with_config(mut self, config: Config) -> Agent {
        self.config = config;
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

    pub fn with_trace(mut self, trace: Trace) -> Agent {
        self.trace = trace;
        self
    }

    fn seen(&self) -> MutexGuard<'_, Seen> {
        self.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn redact(&self, value: &str) -> String {
        self.seen().redactor.redact(value)
    }

    fn mask(&self, value: &str) -> String {
        self.seen().redactor.mask(value)
    }

    fn reveals_text(&self, value: &str) -> bool {
        self.seen().redactor.reveals(value)
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

    /// Pursue `task`. A head start is the plan and shortcut already asked for with this task and start; it carries the
    /// limits it was begun with, so `limits` is not read alongside it.
    pub async fn run(&self, task: &str, mut options: RunOptions) -> Result<RunResult, AgentError> {
        let head = options.head_start.take();
        if let Some(head) = &head
            && (options.limits.is_some() || head.task != task || head.start != options.start)
        {
            // A plan written for another task or start would be followed without complaint.
            return Err(AgentError::Invalid(
                "a head start carries its own limits and must be begun with this task and start".into(),
            ));
        }
        let mut head = match head {
            Some(head) => head,
            None => HeadStart::begin(self.llm.clone(), task, options.start.as_deref(), options.limits.take()),
        };
        let ledger = head.ledger.clone();
        // The run's clock starts here, as it did before there was a head start: a browser slow to start is not time
        // spent on the task, and `max_seconds` bounds the deadline below from this moment too.
        let began = Instant::now();
        ledger.set_started(Some(began));
        self.seen().artifact_start = self.page.artifacts().len();
        // The ledger checks `max_seconds` between operations; only a deadline around the awaits bounds a browser or
        // provider call that never returns.
        let deadline = ledger.limits.max_seconds.map(|seconds| began + Duration::from_secs_f64(seconds));
        let mut slot: Option<RunState> = None;
        let scoped = jev_spend(Lines::Ledger(ledger.clone()), Some(ledger.clone()), async {
            let body = Box::pin(self.body(task, &options, &mut head, &mut slot));
            let outcome = match deadline {
                Some(deadline) => tokio::time::timeout_at(deadline.into(), body).await.ok(),
                None => Some(body.await),
            };
            let mut loop_returned = false;
            let mut result = match outcome {
                Some(Ok(result)) => {
                    loop_returned = true;
                    result
                }
                Some(Err(flow)) => self.stopped(flow, slot.as_ref(), &ledger),
                None => {
                    let limit = ledger.limits.max_seconds.unwrap_or_default();
                    let notes = slot.as_ref().map(|state| state.notes.clone()).unwrap_or_default();
                    self.partial_result(
                        &notes,
                        slot.as_ref(),
                        &ledger,
                        Status::BudgetExceeded,
                        Some(time_limit(limit)),
                        Some(BudgetStop { resource: BudgetResource::Seconds, limit }),
                    )
                }
            };
            // Cancelling a paid startup call loses its receipt, so a finished run joins it before reporting cost.
            if loop_returned {
                let joined = match deadline {
                    Some(deadline) => tokio::time::timeout_at(deadline.into(), head.gather()).await.is_ok(),
                    None => {
                        head.gather().await;
                        true
                    }
                };
                if !joined {
                    head.discard().await;
                    let limit = ledger.limits.max_seconds.unwrap_or_default();
                    result.status = Status::BudgetExceeded;
                    result.error = Some(time_limit(limit));
                    result.budget = Some(BudgetStop { resource: BudgetResource::Seconds, limit });
                    loop_returned = false;
                }
            } else {
                head.discard().await;
            }
            if slot.as_ref().is_none_or(|state| state.ready_plan.is_none())
                && !head.plan_billed
                && let Some(planned) = head.planning.succeeded()
            {
                // First observation can fail before the completed plan is consumed, but its receipt still bills.
                ledger.state().lines.push(planned.cost);
            }
            (result, loop_returned)
        });
        let (mut result, mut loop_returned) = scoped.await;
        // Startup calls can settle during teardown, after the loop has made its last budget check.
        if let Err(error) = ledger.check_spend(0.0) {
            if result.budget.as_ref().is_none_or(|budget| budget.resource != BudgetResource::Seconds) {
                result.status = Status::BudgetExceeded;
                result.error = Some(error.to_string());
                result.budget = Some(error.budget);
            }
            loop_returned = false;
        }
        result.cost = ledger.breakdown();
        Ok(if loop_returned { result } else { self.ending_frame(result, false).await })
    }

    /// The run up to its loop's result: the opening page, the run state, then the loop.
    async fn body(
        &self,
        task: &str,
        options: &RunOptions,
        head: &mut HeadStart,
        slot: &mut Option<RunState>,
    ) -> Result<RunResult, Flow> {
        let ledger = head.ledger.clone();
        let start = options.start.as_deref();
        // `start=None` leaves the browser where it is, which is what a caller stepping a run on from a page it opened
        // itself wants. `choose_start` is the other case: a caller with a goal and no page at all, who wants the first
        // address worked out from the task.
        let opening = match start {
            Some(start) => Some(start.to_string()),
            None if options.choose_start => Some(self.first_page(task, &ledger).await?),
            None => None,
        };
        if let Some(opening) = &opening
            && head.proposing.is_none()
            && shortcut_worth_proposing(opening)
        {
            // A chosen first address arrives after startup; its shortcut still needs owned, bounded waiting.
            head.proposing = Some(Arc::new(propose(self.llm.clone(), task, opening, ledger.clone())));
        }
        let (opened, mut invented) = match &opening {
            None => (Vec::new(), Default::default()),
            Some(opening) => self.open(task, opening, &ledger, head.proposing.clone()).await?,
        };
        let mut fell_back = false;
        if let (None, Some(opening)) = (start, &opening) {
            // The caller gave a goal and no page, so this address was worked out from the task too.
            invented.insert(opening.clone());
            fell_back = self.front_page_if_blank(opening).await?;
        }
        let mut state = RunState::new(
            task.to_string(),
            options.inputs.clone(),
            options.attachments.clone(),
            options.authorization.clone().unwrap_or_default(),
            ledger.clone(),
            head.planning.clone(),
            head.plan_billed,
        );
        state.started_url = opening.clone();
        state.caller_start = start.map(String::from);
        if start.is_some() && opened.is_empty() {
            state.start_landing_url = Some(self.page.address().await?);
        }
        state.history.extend(opened);
        // The run begins on the guess only if the guess is still the page open: a guessed path that opened nothing was
        // left for the site's front page, and a form there led back to the same path with every read of it held
        // guessed.
        state.guessing = !invented.is_empty() && !fell_back;
        state.invented = invented;
        if let Some(opening) = &opening {
            // A shortcut leaves the start page before the loop observes anything.
            state.visited.insert(opening.clone(), ());
        }
        let state = slot.insert(state);
        Box::pin(self.run_loop(state, options.output_schema.as_deref(), options.until.as_ref())).await
    }

    /// The result of a run a step stopped: the error the loop raised, as the status it means.
    fn stopped(&self, flow: Flow, state: Option<&RunState>, ledger: &Ledger) -> RunResult {
        let notes = || state.map(|state| state.notes.clone()).unwrap_or_default();
        match flow {
            Flow::Stop(stop) => {
                // A stop's message can quote the page, such as a field's label, and a page can echo a typed secret
                // there; redacting at each raise missed one, so every stop is redacted here before the caller sees it.
                let error = stop.error.as_deref().map(|error| self.redact(error));
                match state {
                    Some(state)
                        if matches!(stop.status, Status::Stuck | Status::Unverified)
                            && !state.notes.evidence().is_empty() =>
                    {
                        // What was read is still cited evidence; a caller told only "stuck" has to browse again.
                        self.partial_result(&state.notes, Some(state), ledger, stop.status, error, None)
                    }
                    _ => self.result(
                        state,
                        ledger,
                        stop.status,
                        Outcome { error, evidence: stop.evidence, ..Outcome::default() },
                    ),
                }
            }
            Flow::Agent(AgentError::Budget(error)) => self.partial_result(
                &notes(),
                state,
                ledger,
                Status::BudgetExceeded,
                Some(error.to_string()),
                Some(error.budget),
            ),
            Flow::Agent(AgentError::NotesTooLarge(message)) => {
                let message = self.redact(&message);
                self.partial_result(&notes(), state, ledger, Status::ObservationLimit, Some(message), None)
            }
            Flow::Agent(AgentError::ObservationTooLarge(message)) => {
                self.partial_result(&notes(), state, ledger, Status::ObservationLimit, Some(message), None)
            }
            Flow::Browser(error) => {
                let unavailable = error.is_unavailable()
                    || (state.is_none()
                        && matches!(error, BrowserError::NavigationTimeout(_) | BrowserError::SiteUnreachable(_)));
                self.failed(error.to_string(), unavailable, state, ledger)
            }
            Flow::Agent(error) => {
                let unavailable = error.is_unavailable();
                self.failed(error.to_string(), unavailable, state, ledger)
            }
            // An unsure step that reached the run carries no recovery left to ask; it is an error, not a stop.
            Flow::Unsure(unsure) => self.failed(unsure.reason, false, state, ledger),
        }
    }

    fn failed(&self, message: String, unavailable: bool, state: Option<&RunState>, ledger: &Ledger) -> RunResult {
        let message = text::prefix(&self.redact(&message), 500).to_string();
        let status = if unavailable { Status::Unavailable } else { Status::Error };
        let notes = state.map(|state| state.notes.clone()).unwrap_or_default();
        self.partial_result(&notes, state, ledger, status, Some(message), None)
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
        if notes.facts().is_empty() {
            return self.result(state, ledger, status, Outcome { error, budget, ..Outcome::default() });
        }
        let mut partial = partial_answer(notes, self.config.observation.working_notes_chars);
        if let Some(state) = state
            && (!state.partition_answers.is_empty() || !state.partial_answers.is_empty())
            && !state.authorization.irreversible_actions
        {
            let mut verified: Vec<ComposedAnswer> = Vec::new();
            let mut pending: Vec<String> = Vec::new();
            for plan in &state.finish_partitions {
                let ids: Vec<&str> = plan.requirements.iter().map(|r| r.id.as_str()).collect();
                let proof = partition_proof(plan, &notes.for_requirements(ids, None), &state.invented);
                if let Some(answer) = state.partition_answers.get(&proof) {
                    verified.push(answer.clone());
                } else if plan.page_answer_expected() {
                    let partial_fields = state.partial_answers.get(&proof);
                    if let Some(fields) = partial_fields {
                        verified.push(fields.clone());
                    }
                    let checks: Vec<String> = if plan.answer_checks.is_empty() {
                        plan.requirements.iter().map(|r| r.text.clone()).collect()
                    } else {
                        plan.answer_checks.clone()
                    };
                    pending.extend(checks.into_iter().filter(|check| {
                        partial_fields.is_none_or(|fields| {
                            !fields.output_claims.as_ref().is_some_and(|outputs| outputs.contains_key(check))
                        })
                    }));
                }
            }
            if !verified.is_empty() {
                let mut claims = Vec::new();
                for claim in verified.iter().flat_map(|answer| answer.claims.iter()) {
                    if !claims.contains(claim) {
                        claims.push(claim.clone());
                    }
                }
                partial = assemble_answer(&claims, notes, &[], 0);
                let mut notice = String::from("Partial answer.");
                let missing =
                    if state.open_answer_outputs.is_empty() { pending } else { state.open_answer_outputs.clone() };
                if !missing.is_empty() {
                    notice += &format!(" Unverified outputs: {}", missing.join("; "));
                }
                partial.linked_answer = format!("{notice}\n\n{}", partial.linked_answer);
            }
        }
        let cited =
            notes.expand_evidence_ids(partial.claims.iter().flat_map(|c| c.evidence_ids.iter().map(String::as_str)));
        let (answer, citations) = self.public_answer(&partial);
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
        RunResult {
            status,
            budget: outcome.budget,
            answer: outcome.answer,
            data: outcome.data,
            evidence: outcome.evidence,
            steps: state.map(|state| state.steps.clone()).unwrap_or_default(),
            cost: ledger.breakdown(),
            artifacts: artifacts.get(start..).map(<[_]>::to_vec).unwrap_or_default(),
            citations: outcome.citations,
            error: outcome.error,
            final_url: match (state, observed) {
                (Some(_), Some(observed)) => Some(self.redact(&observed.url)),
                _ => None,
            },
            final_frame: None,
            would_fire: state.map(|state| state.would_fire.clone()).unwrap_or_default(),
            recordings: Vec::new(),
        }
    }

    fn public_fact(&self, fact: &Fact) -> StepFact {
        let text = self.redact(&fact.text);
        let requirement_id = fact.requirement_id.as_deref().map(|id| self.redact(id));
        let Some(evidence) = &fact.evidence else {
            return StepFact { text, requirement_id, quote: None, url: None, reader: fact.reader, deep_link: None };
        };
        let (url, quote) = (self.redact(&evidence.url), self.redact(&evidence.quote));
        let deep_link = if evidence.rendered_text {
            text_fragment(&url, &quote).unwrap_or_else(|_| url.clone())
        } else {
            url.clone()
        };
        StepFact {
            text,
            requirement_id,
            quote: Some(quote),
            url: Some(url),
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

    /// The final answer with the run reports the plan asked for, copied from browser state.
    fn run_report(&self, state: &RunState, final_url: Option<&str>, captured: bool) -> String {
        // A raw image has no page quotes for its address or the clicks that reached it. These reports copy runtime
        // values; the composer cannot add claims to them.
        let reports = &state.plan().run_reports;
        let mut parts: Vec<String> = Vec::new();
        if reports.contains(&RunReport::NavigationSteps) {
            let mut steps: Vec<String> = Vec::new();
            if let Some(start) = state.started_url.as_ref().or(state.first_url.as_ref()).filter(|s| !s.is_empty()) {
                steps.push(format!("Started at: {}", self.redact(start)));
            }
            for entry in &state.history {
                if entry.outcome != StepOutcome::Executed || !entry.page_changed {
                    continue;
                }
                match entry.operation {
                    None => {
                        if let Some(note) = entry.note.as_deref().filter(|note| !note.is_empty()) {
                            steps.push(self.redact(note));
                        }
                    }
                    Some(
                        operation @ (Operation::Click | Operation::Enter | Operation::Back | Operation::SwitchTab),
                    ) => {
                        let target = fastbrowse_core::py::json::write(
                            &Value::from(self.redact(entry.target.as_deref().unwrap_or_default())),
                            fastbrowse_core::py::json::Style::DUMPS.unicode(),
                        );
                        steps.push(format!("{operation}: {target}"));
                    }
                    Some(_) => {}
                }
            }
            let listed: Vec<String> = steps.iter().map(|step| format!("- {step}")).collect();
            parts.push(format!("Recorded navigation steps:\n{}", listed.join("\n")));
        }
        if reports.contains(&RunReport::FinalUrl)
            && let Some(url) = final_url
        {
            parts.push(format!("Final URL: {url}"));
        }
        // The title is not viewport text, so no reader could ever quote it.
        if reports.contains(&RunReport::PageTitle)
            && let Some(observed) = self.raw().or_else(|| self.observed())
        {
            parts.push(format!("Page title: {}", self.redact(&observed.title)));
        }
        if reports.contains(&RunReport::Screenshot) {
            parts.push(
                if captured {
                    "A screenshot of the final page was captured."
                } else {
                    "A screenshot of the final page could not be captured."
                }
                .to_string(),
            );
        }
        parts.join("\n\n")
    }

    /// What the plan still wants. Requirement ids, not model prose: this asks whether the run resolved anything, and a
    /// plan the agent cannot influence by rewording is the only honest way to ask it.
    fn plan_mark(&self, state: &RunState) -> String {
        let Some(plan) = &state.ready_plan else { return String::new() };
        let mut ids: Vec<String> = state.notes.unresolved(plan).into_iter().map(|r| r.id).collect();
        ids.sort();
        ids.join(",")
    }

    /// Every grinding signal that holds right now, the long-standing unchanged-page count first.
    fn tripwires(&self, state: &RunState) -> Vec<Tripped> {
        let stall = &self.config.stall;
        let mut tripped = Vec::new();
        if state.unchanged >= stall.unchanged_actions {
            tripped.push(Tripped { tripwire: Tripwire::NoProgress, streak: state.unchanged as usize });
        }
        let since = state.recovered_at.min(state.history.len());
        if let Some(repeated) = repeated_action(&state.history[since..], stall.repeated_actions.max(0) as usize) {
            tripped.push(repeated);
        }
        // An empty mark means the plan was not written yet, and every step before it would look identical.
        if state.ready_plan.is_some() {
            let marks: Vec<&String> = state.plan_marks.iter().filter(|mark| !mark.is_empty()).collect();
            if let Some(stagnant) = stagnant_plan(&marks, stall.stagnant_plan_steps.max(0) as usize) {
                tripped.push(stagnant);
            }
        }
        tripped
    }

    fn armed(&self, tripped: &Tripped) -> bool {
        tripped.tripwire == Tripwire::NoProgress || self.config.stall.tripwires == TripwireMode::Armed
    }

    fn context(
        &self,
        state: &RunState,
        secrets: Vec<String>,
        check_login: bool,
        check_bot: bool,
    ) -> Result<StepContext, Flow> {
        let mut unread = None;
        if let Some(plan) = &state.ready_plan {
            let mut wanted: Vec<String> = state
                .notes
                .unresolved(plan)
                .into_iter()
                .filter(|r| r.kind == RequirementKind::Information)
                .map(|r| r.text)
                .collect();
            if wanted.is_empty() && helpers::unread(plan, &state.notes) {
                wanted = vec![state.task.clone()];
            }
            unread = Some(wanted);
        }
        Ok(StepContext {
            task: state.task.clone(),
            subgoal: state.hint.clone(),
            requirements: state
                .ready_plan
                .as_ref()
                .map_or_else(Vec::new, |plan| plan.requirements.iter().map(|r| r.text.clone()).collect()),
            notes: state.notes.render_for_navigation(self.config.observation.working_notes_chars, false)?,
            history: history(&state.history, &self.config.observation),
            check_login,
            check_bot,
            has_attachments: !state.attachments.is_empty(),
            secrets,
            unread_requirements: unread,
            start_url: state.caller_start.clone(),
            start_landing_url: state.start_landing_url.clone(),
            recovery_memory: recovery_memory(state, self.config.stall.max_recoveries, |value| self.redact(value)),
        })
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

    /// Stuck becomes needs_login when a sign-in is what the run stopped at: a caller told "stuck" narrows the task
    /// rather than signing in.
    fn walled(&self, state: &RunState, observation: &Observation, status: Status, said: bool) -> Status {
        if status != Status::Stuck {
            return status;
        }
        let walled = said
            || state
                .passed_wall
                .as_deref()
                .is_some_and(|wall| fastbrowse_core::navigation::same_address(wall, &observation.url));
        // A held secret makes the sign-in work the run can do, so a run holding one did not stop for want of it.
        let origin = origin_of(&observation.url);
        if walled && !(!origin.is_empty() && !self.secret_names(&origin).is_empty()) {
            Status::NeedsLogin
        } else {
            status
        }
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
}

fn time_limit(limit: f64) -> String {
    format!("time limit {}s reached", fastbrowse_core::py::json::float_str(limit))
}

/// A run's cost lines, for a result built outside the loop.
pub fn breakdown(lines: Vec<CostLine>) -> CostBreakdown {
    CostBreakdown { lines }
}

/// Python's `json.dumps` of a list of strings, as the recovery prompt lists addresses.
fn dumps_strings(values: &[String]) -> String {
    dumps(&Value::from(values.to_vec()))
}
