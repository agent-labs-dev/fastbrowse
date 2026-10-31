//! The run loop: Jev picks every operation and target, reads, and says DONE, and an unsure Jev still acts on its best
//! pick. Code owns the safety gates (irreversible actions, sign-in walls, bot checks, authorization, secrets) and the
//! stall rule. The answer is what Jev read, each fact with its citations.
//!
//! Every Jev request built here is a pure function of its inputs, since a run is replayed from recorded model traffic
//! keyed on a hash of the request body.

mod acting;
mod driving;
mod finishing;
pub mod helpers;
mod reading;
mod recovery;
mod seeing;
pub(crate) mod state;

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use fastbrowse_clients::validation::{Lines, jev_spend};
use fastbrowse_core::citations::text_fragment;
use fastbrowse_core::config::Config;
use fastbrowse_core::decision::DecisionClient;
use fastbrowse_core::llm::LLMClient;
use fastbrowse_core::models::{
    Attachment, Authorization, BudgetResource, BudgetStop, Citation, CostBreakdown, CostLine, EventHandler, Evidence,
    Limits, MaybeUnavailable, RunResult, SecretResolver, Status, StepFact, UntilCheck, Wall, WallKind,
};
use fastbrowse_core::page::{BrowserError, Observation, Page};
use fastbrowse_core::safety::{CredentialField, Redactor, credential_field, origin_of, secret_allowed, wall_kind};
use fastbrowse_core::telemetry::{Ledger, Trace};
use indexmap::IndexMap;
use serde_json::{Map, Value};

use state::{Flow, RunState};

use crate::error::AgentError;
use crate::memory::{Fact, Notes};
use crate::policy::StepContext;
use crate::retrieval::{ComposedAnswer, partial_answer};
use crate::verification::RecordSchema;

/// Long enough for a browser-verification page to run its check and hand over, short enough that a page which never
/// moves still ends as needs_login well inside a run's time budget.
const INTERSTITIAL_SECONDS: f64 = 12.0;
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
}

/// What the agent learns about the page as it goes, kept apart from the run so the page and the models can be asked
/// side by side.
#[derive(Default)]
struct Seen {
    secret_on_screen: bool,
    raw: Option<Observation>,
    /// The last observation `observe` returned, which the finish judges again only if the page changed since.
    observed: Option<Observation>,
    artifact_start: usize,
}

pub struct Agent {
    page: Arc<dyn Page>,
    jev: Arc<dyn DecisionClient>,
    llm: Arc<dyn LLMClient>,
    config: Config,
    secrets: Option<Arc<dyn SecretResolver>>,
    on_event: Option<EventHandler>,
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
            jev,
            llm,
            config,
            secrets: None,
            on_event: None,
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

    pub fn with_trace(mut self, trace: Trace) -> Agent {
        self.trace = trace.redacting(Arc::clone(&self.redactor));
        self
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

    fn mask(&self, value: &str) -> String {
        self.redactor().mask(value)
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
        self.seen().artifact_start = self.page.artifacts().len();
        // The ledger checks `max_seconds` between operations; only a deadline around the awaits bounds a browser or
        // provider call that never returns.
        let deadline = ledger.limits.max_seconds.map(|seconds| began + Duration::from_secs_f64(seconds));
        let mut slot: Option<RunState> = None;
        let scoped = jev_spend(Lines::Ledger(ledger.clone()), Some(ledger.clone()), async {
            let body = Box::pin(self.body(task, &options, &ledger, &mut slot));
            let outcome = match deadline {
                Some(deadline) => tokio::time::timeout_at(deadline.into(), body).await.ok(),
                None => Some(body.await),
            };
            match outcome {
                Some(Ok(result)) => (result, true, false),
                Some(Err(Flow::Browser(BrowserError::NeedsBrowser(message)))) => {
                    (blocked_without_browser(message), false, true)
                }
                Some(Err(flow)) => (self.stopped(flow, slot.as_mut(), &ledger).await, false, false),
                None => {
                    let limit = ledger.limits.max_seconds.unwrap_or_default();
                    let notes = slot.as_ref().map(|state| state.notes.clone()).unwrap_or_default();
                    let result = self.partial_result(
                        &notes,
                        slot.as_ref(),
                        &ledger,
                        Status::BudgetExceeded,
                        Some(time_limit(limit)),
                        Some(BudgetStop { resource: BudgetResource::Seconds, limit }),
                    );
                    (result, false, false)
                }
            }
        });
        let (mut result, mut loop_returned, needs_browser) = self.trace.phase("run", scoped).await;
        if needs_browser {
            // A run that needs a browser reports what the ledger spent and nothing else: no budget check and no ending
            // frame.
            result.cost = ledger.breakdown();
            return Ok(result);
        }
        // A run that already failed keeps its error: a refused Jev key's unpriced line reported the run as over a spend
        // limit instead of naming the key.
        if let Err(error) = ledger.check_spend(0.0) {
            if result.status != Status::Error
                && result.budget.as_ref().is_none_or(|budget| budget.resource != BudgetResource::Seconds)
            {
                result.status = Status::BudgetExceeded;
                result.error = Some(error.to_string());
                result.budget = Some(error.budget);
            }
            loop_returned = false;
        }
        result.cost = ledger.breakdown();
        Ok(if loop_returned { result } else { self.ending_frame(result).await })
    }

    /// The run up to its loop's result: the opening page, the run state, then the loop.
    async fn body(
        &self,
        task: &str,
        options: &RunOptions,
        ledger: &Arc<Ledger>,
        slot: &mut Option<RunState>,
    ) -> Result<RunResult, Flow> {
        let start = options.start.as_deref();
        // `start=None` leaves the browser where it is, which is what a caller stepping a run on from a page it opened
        // itself wants. `choose_start` is the other case: a caller with a goal and no page at all, who wants the first
        // address worked out from the task.
        let opening = match start {
            Some(start) => Some(start.to_string()),
            None if options.choose_start => Some(self.first_page(task, ledger).await?),
            None => None,
        };
        if let Some(opening) = &opening {
            self.trace.phase("open", self.page.navigate(opening, None)).await?;
        }
        if let (None, Some(opening)) = (start, &opening) {
            // The caller gave a goal and no page, so this address was worked out from the task.
            self.front_page_if_blank(opening).await?;
        }
        let mut state = RunState::new(
            task.to_string(),
            options.inputs.clone(),
            options.attachments.clone(),
            options.authorization.clone().unwrap_or_default(),
            ledger.clone(),
        );
        state.context = options.context.clone();
        state.started_url = opening.clone();
        state.caller_start = start.map(String::from);
        if start.is_some() {
            state.start_landing_url = Some(self.page.address().await?);
        }
        if let Some(opening) = &opening {
            state.visited.insert(opening.clone(), ());
        }
        let state = slot.insert(state);
        Box::pin(self.run_loop(state, options.output_schema.as_deref(), options.until.as_ref())).await
    }

    /// The result of a run a step stopped: the error the loop raised, as the status it means.
    async fn stopped(&self, flow: Flow, state: Option<&mut RunState>, ledger: &Ledger) -> RunResult {
        if let Flow::Stop(stop) = &flow
            && let Some(state) = state
        {
            // A stop's message can quote the page, such as a field's label, and a page can echo a typed secret there;
            // redacting at each raise missed one, so every stop is redacted here before the caller sees it.
            let error = stop.error.as_deref().map(|error| self.redact(error));
            if matches!(stop.status, Status::Stuck | Status::Unverified) && !state.notes.evidence().is_empty() {
                // What was read is still cited evidence; a caller told only "stuck" has to browse again for it.
                let notes = state.notes.clone();
                return self.partial_result(&notes, Some(state), ledger, stop.status, error, None);
            }
            let wall = self.wall(stop);
            let outcome = Outcome { error, evidence: stop.evidence.clone(), wall, ..Outcome::default() };
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
                    Outcome { error, evidence: stop.evidence, wall, ..Outcome::default() },
                )
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
            Flow::Unsure(unsure) => self.failed(unsure.reason, false, state, ledger),
        }
    }

    fn failed(&self, message: String, unavailable: bool, state: Option<&RunState>, ledger: &Ledger) -> RunResult {
        let message = self.redact(&message);
        let status = if unavailable { Status::Unavailable } else { Status::Error };
        let notes = state.map(|state| state.notes.clone()).unwrap_or_default();
        self.partial_result(&notes, state, ledger, status, Some(message), None)
    }

    async fn ending_frame(&self, mut result: RunResult) -> RunResult {
        if !self.config.step_frames {
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
        let partial = partial_answer(notes);
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
        Some(Wall { kind: kind.unwrap_or_else(|| wall_kind(&credentials)), origin, fields, seen_at_ms: now_ms() })
    }

    fn public_fact(&self, fact: &Fact) -> StepFact {
        let text = self.redact(&fact.text);
        let Some(evidence) = &fact.evidence else {
            return StepFact { text, quote: None, url: None, reader: fact.reader, deep_link: None };
        };
        let (url, quote) = (self.redact(&evidence.url), self.redact(&evidence.quote));
        let deep_link = if evidence.rendered_text {
            text_fragment(&url, &quote).unwrap_or_else(|_| url.clone())
        } else {
            url.clone()
        };
        StepFact { text, quote: Some(quote), url: Some(url), reader: fact.reader, deep_link: Some(deep_link) }
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

    fn context(
        &self,
        state: &RunState,
        observation: &Observation,
        secrets: Vec<String>,
        check_login: bool,
        check_bot: bool,
    ) -> Result<StepContext, Flow> {
        let page_facts = Self::page_facts(state, observation);
        Ok(StepContext {
            task: state.task.clone(),
            subgoal: state.hint.clone(),
            notes: state.notes.render_for_navigation(),
            answered_in_notes: state.notes.answered(),
            history: state.history.clone(),
            check_login,
            check_answer: check_bot && !state.read_pages.contains(&observation.page_key),
            check_bot,
            has_attachments: !state.attachments.is_empty(),
            secrets,
            start_url: state.caller_start.clone(),
            start_landing_url: state.start_landing_url.clone(),
            background: state.context.clone(),
            omitted_actions: 0,
            problem: None,
            withheld: Vec::new(),
            no_value_fields: state
                .missing
                .iter()
                .map(|(label, _)| label.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            page_facts,
            reload: state.reload_target(&observation.url),
            screenshot: None,
        })
    }

    /// This page was read and no action has changed it since.
    fn page_read(state: &RunState, observation: &Observation) -> bool {
        !observation.document_key.is_empty() && state.read_documents.get(&observation.document_key) == Some(&false)
    }

    /// What code knows about this page that bears on reading it or leaving it. Jev decides what to do with each.
    fn page_facts(state: &RunState, observation: &Observation) -> Vec<String> {
        let mut facts = Vec::new();
        if let Some(status) = observation.response_status.filter(|status| *status >= 400) {
            facts.push(format!(
                "This page answered HTTP {status}: the address did not load successfully in this browser session."
            ));
        }
        if Self::page_read(state, observation) {
            facts
                .push("This page was read and has not changed since: the notes hold what a read takes from it.".into());
        }
        let document = &observation.document_key;
        let read = state.read_documents.get(document).copied();
        if !document.is_empty() && read == Some(true) {
            facts.push(
                "This page changed after it was last read, so the notes taken from it predate the change.".into(),
            );
        }
        facts.extend(helpers::unsent_fact(&state.history, observation));
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
