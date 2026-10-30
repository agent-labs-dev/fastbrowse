//! What one run keeps between steps: `_RunState` and the small records it holds, and the stops a step can raise.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fastbrowse_core::decision::{DecisionError, NoulQuestion};
use fastbrowse_core::llm::{Generation, LLMError};
use fastbrowse_core::models::{Attachment, Authorization, Evidence, Operation, Status, StepResult, WallKind};
use fastbrowse_core::page::{BrowserError, Control, Observation};
use fastbrowse_core::telemetry::{BudgetExceeded, Ledger};
use indexmap::IndexMap;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::helpers::EvidenceKey;
use super::recovery::Withheld;
use crate::comparison::NumericComparison;
use crate::error::AgentError;
use crate::memory::{Fact, Notes};
use crate::planner::{OutputTarget, Plan};
use crate::policy::{Decision, HistoryEntry, RelevanceCache};
use crate::retrieval::{ComposedAnswer, TallyReader};

/// A run ends here, as what it is.
#[derive(Debug, Clone)]
pub(crate) struct Stop {
    pub status: Status,
    pub error: Option<String>,
    pub evidence: Vec<Evidence>,
    /// The wall this stop names when the page alone cannot say: a bot check reads like any other page.
    pub wall: Option<WallKind>,
}

impl Stop {
    pub fn new(status: Status, error: impl Into<String>) -> Stop {
        Stop { status, error: Some(error.into()), evidence: Vec::new(), wall: None }
    }
}

/// The next action is authorized but not confidently the right one: a case for recovery, not for the caller.
#[derive(Debug, Clone)]
pub(crate) struct Unsure {
    pub reason: String,
    /// How the run ends if recovery finds no other way: a missing value is still the user's to give.
    pub gives_up_as: Status,
}

impl Unsure {
    pub fn new(reason: impl Into<String>) -> Unsure {
        Unsure { reason: reason.into(), gives_up_as: Status::Stuck }
    }
}

/// What can cut a step short: Python's `_Stop`, `_Unsure` and the errors the run catches by class.
#[derive(Debug, Clone)]
pub(crate) enum Flow {
    Stop(Stop),
    Unsure(Unsure),
    Agent(AgentError),
    Browser(BrowserError),
}

impl From<Stop> for Flow {
    fn from(stop: Stop) -> Self {
        Flow::Stop(stop)
    }
}

impl From<Unsure> for Flow {
    fn from(unsure: Unsure) -> Self {
        Flow::Unsure(unsure)
    }
}

impl From<AgentError> for Flow {
    fn from(error: AgentError) -> Self {
        Flow::Agent(error)
    }
}

impl From<BrowserError> for Flow {
    fn from(error: BrowserError) -> Self {
        Flow::Browser(error)
    }
}

impl From<BudgetExceeded> for Flow {
    fn from(error: BudgetExceeded) -> Self {
        Flow::Agent(AgentError::Budget(error))
    }
}

impl From<DecisionError> for Flow {
    fn from(error: DecisionError) -> Self {
        Flow::Agent(AgentError::Jev(error))
    }
}

impl From<LLMError> for Flow {
    fn from(error: LLMError) -> Self {
        Flow::Agent(error.into())
    }
}

impl Flow {
    /// Whether this is an `LLMError` a caller catches, as Python's `except LLMError` does: never a budget stop.
    pub fn is_llm(&self) -> bool {
        matches!(self, Flow::Agent(AgentError::Llm(_)))
    }
}

pub(crate) type Step<T = ()> = Result<T, Flow>;

/// What says the site failed on a page: its HTTP status, the browser's navigation error, or Jev reading the page.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FailureCause {
    Http(i64),
    /// The browser could not open the address, after its own retries: the error it gave.
    Navigation(String),
    /// Jev judged that the page reports the site failed to deliver what was asked, such as an error shown in place
    /// of search results, a maintenance notice or a load that never finished.
    Reported,
}

/// A site failure on one address, stated to Jev and the done check while the run is on it, and the status a run
/// ends with when recovery runs out there.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SiteFailure {
    pub url: String,
    pub cause: FailureCause,
    /// The run's step count when the failure was first seen, so how long it has stood is stated in steps.
    pub since: usize,
}

impl SiteFailure {
    pub fn http(url: &str, status: i64, since: usize) -> SiteFailure {
        SiteFailure { url: url.to_string(), cause: FailureCause::Http(status), since }
    }

    pub fn message(&self) -> String {
        match &self.cause {
            FailureCause::Http(status) => {
                format!("HTTP {status} at {} in this browser session; the cause is unknown.", self.url)
            }
            FailureCause::Navigation(error) => {
                format!("opening {} failed in this browser session: {error}.", self.url)
            }
            FailureCause::Reported => {
                format!("the page at {} reported that the site failed to deliver what was asked.", self.url)
            }
        }
    }

    /// Whether the failure can clear by itself, so trying the address again is worth offering.
    pub fn transient(&self) -> bool {
        self.stop().status == Status::Unavailable
    }

    /// A status that can clear with time, a navigation the browser could not complete and a failure the page reports
    /// all leave the site unavailable for now; the same run later may pass.
    pub fn stop(&self) -> Stop {
        let status = match self.cause {
            FailureCause::Http(401) => Status::NeedsLogin,
            FailureCause::Http(403) => Status::Blocked,
            FailureCause::Http(status) if matches!(status, 408 | 419 | 429) || status >= 500 => Status::Unavailable,
            FailureCause::Http(_) => Status::Stuck,
            FailureCause::Navigation(_) | FailureCause::Reported => Status::Unavailable,
        };
        Stop::new(status, format!("Could not complete the task after {}", self.message()))
    }
}

/// The recovery a run is following: the page state it was asked on, the problem it was asked about and the action
/// that problem withheld, all stated again in the next decision there so the refused action does not undo the
/// recovery's subgoal.
#[derive(Debug, Clone)]
pub(crate) struct Recovering {
    pub key: String,
    pub problem: String,
    pub withheld: Withheld,
}

#[derive(Debug, Clone)]
pub(crate) struct TransactionCandidate {
    pub question: NoulQuestion,
    pub from_url: String,
    pub landed_url: Option<String>,
    pub landed_status: Option<i64>,
    pub committing: Option<bool>,
    pub outcome_read: bool,
}

/// What the LLM said when it last gave up in recovery: the status the run ends with once recovery runs out.
#[derive(Debug, Clone)]
pub(crate) struct GaveUp {
    pub status: Status,
    /// Whether it named a sign-in as what blocks the task, which Jev must confirm on the page.
    pub said_login: bool,
    pub diagnosis: String,
}

/// A spawned task whose result any number of waiters can read, as an `asyncio.Task` can be awaited by many.
///
/// Waiting never cancels it, as `asyncio.shield` does not; only [`Spawned::cancel`] does, and a task cancelled
/// before it finished reports no result.
pub(crate) struct Spawned<T: Clone + Send + Sync + 'static> {
    handle: Option<JoinHandle<()>>,
    result: watch::Receiver<Option<Result<T, AgentError>>>,
    /// Set by whichever comes first: the task beginning, or a cancel of a task not yet begun.
    claimed: Arc<AtomicBool>,
}

impl<T: Clone + Send + Sync + 'static> Spawned<T> {
    pub fn spawn<F>(future: F) -> Spawned<T>
    where
        F: Future<Output = Result<T, AgentError>> + Send + 'static,
    {
        let (sender, result) = watch::channel(None);
        let claimed = Arc::new(AtomicBool::new(false));
        let begun = claimed.clone();
        let handle = tokio::spawn(async move {
            if begun.swap(true, Ordering::AcqRel) {
                return;
            }
            let outcome = future.await;
            let _ = sender.send(Some(outcome));
        });
        Spawned { handle: Some(handle), result, claimed }
    }

    /// Cancel the task if it has not begun, and say whether it was cancelled. A model call runs up to its request in
    /// its first poll, so one that has begun has a request out, which the provider bills whether or not anyone reads
    /// its receipt. The check and the cancel are one step, so a task cannot begin between them.
    pub fn cancel_unstarted(&self) -> bool {
        let cancelled = !self.claimed.swap(true, Ordering::AcqRel);
        if cancelled {
            self.cancel();
        }
        cancelled
    }

    /// The result, once it finished; None while it runs or after it was cancelled first.
    pub fn result(&self) -> Option<Result<T, AgentError>> {
        self.result.borrow().clone()
    }

    pub fn done(&self) -> bool {
        self.result.borrow().is_some() || self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// The value of a task that finished without an error.
    pub fn succeeded(&self) -> Option<T> {
        self.result().and_then(Result::ok)
    }

    /// Wait for the result; None when the task was cancelled before it finished.
    pub async fn wait(&self) -> Option<Result<T, AgentError>> {
        let mut result = self.result.clone();
        match result.wait_for(Option::is_some).await {
            Ok(outcome) => outcome.clone(),
            Err(_) => self.result(),
        }
    }

    pub fn cancel(&self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}

/// The task a shield was cancelled out of: Python's `CancelledError` reaching the waiter.
pub(crate) fn cancelled() -> AgentError {
    AgentError::Invalid("the task was cancelled".into())
}

pub(crate) struct RunState {
    pub task: String,
    /// The caller's background: shown beside the task, never read as what it asks for.
    pub context: Option<String>,
    pub inputs: IndexMap<String, String>,
    pub attachments: Vec<Attachment>,
    pub authorization: Authorization,
    pub ledger: Arc<Ledger>,
    pub planning: Arc<Spawned<Generation<Plan>>>,
    /// The plan's receipt reached the ledger as the plan arrived, so taking the plan must not record it again.
    pub plan_billed: bool,
    pub notes: Notes,
    pub finish_partitions: Vec<Plan>,
    pub output_targets: IndexMap<String, OutputTarget>,
    pub oversized_answer: bool,
    pub read_source_sizes: IndexMap<String, i64>,
    pub pending_collections: IndexMap<String, (String, Vec<Fact>)>,
    pub partition_answers: IndexMap<String, ComposedAnswer>,
    pub partial_answers: IndexMap<String, ComposedAnswer>,
    pub verified_answer: Option<ComposedAnswer>,
    /// Bot-check documents this run has already reloaded once, so a challenge that survives is reported.
    pub challenge_reloads: HashSet<String>,
    pub open_answer_outputs: Vec<String>,
    pub rejected_answer_evidence: Option<BTreeSet<EvidenceKey>>,
    pub relevance_cache: RelevanceCache,
    pub steps: Vec<StepResult>,
    pub history: Vec<HistoryEntry>,
    pub site_failure: Option<SiteFailure>,
    pub http_seen: HashSet<(String, String, i64)>,
    pub hint: Option<String>,
    /// The operation and control id recovery named, taken when Jev is still unsure of the next step.
    pub directed: Option<(Operation, Option<String>)>,
    /// Jev's pick when asked again after a stall or an unsure pick, by the page state it was asked on, taken as the
    /// next decision there.
    pub picked: Option<(String, Decision)>,
    /// The LLM recovery last asked, until the next decision on its page state.
    pub recovering: Option<Recovering>,
    pub unchanged: i64,
    /// Controls a fill or select has written, by document, so only a field's first new value counts as progress by
    /// itself. A new document restarts control ids, and its fields would otherwise inherit the last page's writes.
    pub written: HashMap<String, HashSet<String>>,
    pub recoveries: i64,
    pub blank_opening_checked: bool,
    pub recovery_log: VecDeque<String>,
    pub gave_up: Option<GaveUp>,
    pub last_page: Option<(String, String)>,
    /// Each page state reached, with `evidence_reads` when it was last on screen: going back to one with nothing read
    /// since is going round, not forward.
    pub reached: HashMap<String, usize>,
    /// Reads and pages of a list that added evidence, counted over the run.
    pub evidence_reads: usize,
    /// The action whose page change the next observation judges: a new page state, or one revisited after new
    /// evidence, is progress.
    pub landing: Option<Withheld>,
    /// The page the last action was taken on, until the next observation says what it did.
    pub acted_from: Option<Observation>,
    pub ready_plan: Option<Plan>,
    /// Addresses this run built from the task rather than reached by clicking.
    pub invented: HashSet<String>,
    /// Jev's word that the task rules out downloading, asked once a control the page says downloads is in play.
    pub downloads_forbidden: Option<bool>,
    /// Documents read, by key, each with whether an action changed it since it was last read.
    pub read_documents: HashMap<String, bool>,
    /// Open this page's next page, set when the reader says a list the run needs goes on past the page it read.
    pub next_page: bool,
    /// The page state a next-page click left, so the page it opens is read without a decision.
    pub paged_from: Option<String>,
    /// Next pages opened by code this run.
    pub pages: i64,
    /// Every unresolved requirement needs the entire list, so intermediate pages can be read concurrently.
    pub through_end: bool,
    pub tally_readers: Vec<TallyReader>,
    pub comparisons: IndexMap<String, NumericComparison>,
    pub paging_failed: bool,
    /// The initial address, retained even when an HTTP failure removes it from completion evidence.
    pub started_url: Option<String>,
    pub caller_start: Option<String>,
    pub start_landing_url: Option<String>,
    /// The first page the run looked at, which is what a task's "this page" means once the run has moved on.
    pub first_url: Option<String>,
    /// Addresses observed without an HTTP error, in order, including the starting address.
    pub visited: IndexMap<String, ()>,
    /// A decision was dropped because the page redrew under it, so nothing is watched until an action is taken.
    pub redecided: bool,
    /// Fields the task gives no value for, which recovery has been told about once, by label and context.
    pub missing: HashSet<(String, Option<String>)>,
    /// Requirements the reader said range over a list that goes on past the page it read.
    pub continuing: HashSet<String>,
    /// Comparisons a page of their list lost a record from.
    pub incomplete: BTreeSet<String>,
    /// Authorized clicks can just navigate, so only candidates with notes are classified when answering.
    pub transaction_candidates: Vec<TransactionCandidate>,
    /// Values the form writer wrote for a form's fields, by document and field, used by the fills Jev picks there.
    pub form_values: HashMap<(String, String), (Control, FieldText)>,
}

/// `_FieldText`: what a field writer answers.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FieldText {
    #[serde(deserialize_with = "fastbrowse_core::py::lax::boolean")]
    pub missing: bool,
    pub text: String,
    #[serde(default)]
    pub values: Vec<String>,
}

impl RunState {
    /// The address of the page at `url` when a site failure that can clear stands on it: opening it again reloads it.
    pub fn reload_target(&self, url: &str) -> Option<String> {
        self.site_failure.as_ref().filter(|failure| failure.url == url && failure.transient()).map(|f| f.url.clone())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        task: String,
        inputs: IndexMap<String, String>,
        attachments: Vec<Attachment>,
        authorization: Authorization,
        ledger: Arc<Ledger>,
        planning: Arc<Spawned<Generation<Plan>>>,
        plan_billed: bool,
    ) -> RunState {
        RunState {
            task,
            context: None,
            inputs,
            attachments,
            authorization,
            ledger,
            planning,
            plan_billed,
            notes: Notes::default(),
            finish_partitions: Vec::new(),
            output_targets: IndexMap::new(),
            oversized_answer: false,
            read_source_sizes: IndexMap::new(),
            pending_collections: IndexMap::new(),
            partition_answers: IndexMap::new(),
            partial_answers: IndexMap::new(),
            verified_answer: None,
            challenge_reloads: HashSet::new(),
            open_answer_outputs: Vec::new(),
            rejected_answer_evidence: None,
            relevance_cache: RelevanceCache::default(),
            steps: Vec::new(),
            history: Vec::new(),
            site_failure: None,
            http_seen: HashSet::new(),
            hint: None,
            directed: None,
            picked: None,
            recovering: None,
            unchanged: 0,
            written: HashMap::new(),
            recoveries: 0,
            blank_opening_checked: false,
            recovery_log: VecDeque::new(),
            gave_up: None,
            last_page: None,
            reached: HashMap::new(),
            evidence_reads: 0,
            landing: None,
            acted_from: None,
            ready_plan: None,
            invented: HashSet::new(),
            downloads_forbidden: None,
            read_documents: HashMap::new(),
            next_page: false,
            paged_from: None,
            pages: 0,
            through_end: false,
            tally_readers: Vec::new(),
            comparisons: IndexMap::new(),
            paging_failed: false,
            started_url: None,
            caller_start: None,
            start_landing_url: None,
            first_url: None,
            visited: IndexMap::new(),
            redecided: false,
            missing: HashSet::new(),
            continuing: HashSet::new(),
            incomplete: BTreeSet::new(),
            transaction_candidates: Vec::new(),
            form_values: HashMap::new(),
        }
    }

    /// The plan, for code that only runs after `await_plan`: reading, judging DONE and answering.
    pub fn plan(&self) -> &Plan {
        self.ready_plan.as_ref().expect("the plan was used before it was awaited")
    }

    /// Record a recovery's record.
    pub fn log_recovery(&mut self, record: String) {
        self.recovery_log.push_back(record);
    }

    pub async fn await_plan(&mut self) -> Result<Plan, AgentError> {
        self.take_plan().await
    }

    async fn take_plan(&mut self) -> Result<Plan, AgentError> {
        if let Some(plan) = &self.ready_plan {
            return Ok(plan.clone());
        }
        // Waiting never cancels the one plan every later step needs, though a step abandoned mid-wait may drop this
        // future. Teardown still cancels the task itself.
        let planned = self.planning.wait().await.unwrap_or_else(|| Err(cancelled()))?;
        // The plan arrives with its answer checks written by the same call, so taking it takes them too.
        let plan = planned.data;
        self.ready_plan = Some(plan.clone());
        if self.plan_billed {
            // Billed on arrival; a receipt that broke a limit still stops the run here.
            self.ledger.check(0.0)?;
        } else {
            self.ledger.record([planned.cost])?;
        }
        Ok(plan)
    }

    /// The plan with its answer checks, which only finishing and answering read.
    ///
    /// The plan call writes the checks, so this returns the same plan `await_plan` does; it stays a seam so the
    /// finishing code that reads checks keeps one place to wait.
    pub async fn await_checks(&mut self) -> Result<Plan, AgentError> {
        self.take_plan().await
    }
}
