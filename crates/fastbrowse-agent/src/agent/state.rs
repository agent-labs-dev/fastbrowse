//! What one run keeps between steps: `_RunState` and the small records it holds, and the stops a step can raise.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use fastbrowse_core::effects::{ControlKey, ControlValue, Move};
use fastbrowse_core::jev::{JevError, NoulQuestion};
use fastbrowse_core::llm::{Generation, LLMError};
use fastbrowse_core::models::{Attachment, Authorization, Evidence, Operation, Status, StepResult, Tripwire};
use fastbrowse_core::page::{BrowserError, Control, Observation};
use fastbrowse_core::telemetry::{BudgetExceeded, Ledger};
use indexmap::IndexMap;
use serde::Serialize;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::helpers::{EvidenceKey, LinkKey};
use crate::comparison::NumericComparison;
use crate::error::AgentError;
use crate::memory::{Fact, Notes};
use crate::planner::{OutputTarget, Plan};
use crate::policy::{HistoryEntry, RelevanceCache};
use crate::retrieval::{AnswerCorrection, ComposedAnswer, TallyReader};
use crate::verification::{LLMVerdict, OutputAuditCache};

/// How many recovery records the next recovery is shown.
pub(crate) const RECOVERY_RECORDS: usize = 4;

/// A run ends here, as what it is.
#[derive(Debug, Clone)]
pub(crate) struct Stop {
    pub status: Status,
    pub error: Option<String>,
    pub evidence: Vec<Evidence>,
}

impl Stop {
    pub fn new(status: Status, error: impl Into<String>) -> Stop {
        Stop { status, error: Some(error.into()), evidence: Vec::new() }
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

impl From<JevError> for Flow {
    fn from(error: JevError) -> Self {
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

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HttpFailure {
    pub url: String,
    pub status: i64,
}

impl HttpFailure {
    pub fn message(&self) -> String {
        format!("HTTP {} at {} in this browser session; the cause is unknown.", self.status, self.url)
    }

    pub fn stop(&self) -> Stop {
        let mut status = match self.status {
            401 => Status::NeedsLogin,
            403 => Status::Blocked,
            _ => Status::Stuck,
        };
        if matches!(self.status, 408 | 419 | 429) || self.status >= 500 {
            status = Status::Unavailable;
        }
        Stop::new(status, format!("Could not complete the task after {}", self.message()))
    }
}

/// One action on one target, from one page state: how often it was taken, and what the last one did.
#[derive(Debug, Clone, Default)]
pub(crate) struct Attempts {
    pub count: i64,
    /// The last attempt left the page as it was, so taking it again is a loop rather than a retry.
    pub idle: bool,
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

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ObservedLink {
    pub source: String,
    pub title: String,
    pub label: String,
    pub url: String,
}

/// Whether the page was on the guess, kept per browser history entry and per read, never per address.
///
/// A form or single-page app can keep its search outside the address, so a run that left a guessed search and set the
/// same search up itself arrived back at the guessed address. The browser's own history says which page the run is
/// on: Back, Forward and a reload return to an entry, and its lineage with it. A read is judged by what the run was
/// doing when it read, so a quote read on the guess stays guessed.
///
/// A hold `Notes::discredit` keeps asks the lineage again each time it is checked, as Python's closure over the run's
/// one `_Lineage` does, so the readings and addresses are shared by every clone rather than copied: a snapshot kept a
/// reading held after the run read the same page again off the guess.
#[derive(Debug, Clone, Default)]
pub(crate) struct Lineage {
    /// Each history entry, by whether the run was on the guess when the entry was made.
    pub entries: HashMap<String, bool>,
    /// Each history entry's address when last observed, so a site action that moved it there is told apart.
    pub entry_addresses: HashMap<String, String>,
    /// Each address by the lineage of the entry last observed there, for a quote with no recorded reading.
    addresses: Arc<Mutex<HashMap<String, bool>>>,
    /// Each reading, by source address and capture, by whether it was read on the guess.
    captures: Arc<Mutex<HashMap<(String, String), bool>>>,
}

fn locked<T>(shared: &Mutex<T>) -> MutexGuard<'_, T> {
    shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Lineage {
    pub fn read(&mut self, capture: &str, sources: &[String], guessing: bool) {
        // The same page read again off the guess is the page the run reached itself, so that reading stands. Text
        // alone does not name a page: another address drawing identical text released the guessed one's hold.
        let mut captures = locked(&self.captures);
        for source in sources {
            let key = (fastbrowse_core::addresses::search_address(source), capture.to_string());
            if guessing {
                captures.entry(key).or_insert(true);
            } else {
                captures.insert(key, false);
            }
        }
    }

    /// Whether this reading, by search address and capture, was taken on the guess.
    pub fn reading(&self, address: &str, capture: &str) -> Option<bool> {
        locked(&self.captures).get(&(address.to_string(), capture.to_string())).copied()
    }

    /// Whether the entry last observed at this search address was on the guess.
    pub fn address(&self, address: &str) -> Option<bool> {
        locked(&self.addresses).get(address).copied()
    }

    pub fn set_address(&mut self, address: String, guessing: bool) {
        locked(&self.addresses).insert(address, guessing);
    }
}

/// Checked answers by task, plan, answer and transaction evidence: the answer held, its missing outputs and
/// corrections.
pub(crate) type AnswerChecks =
    HashMap<(String, String, String, Vec<String>), (Option<ComposedAnswer>, Vec<String>, Vec<AnswerCorrection>)>;

pub(crate) type ReadKey = (String, String, Vec<String>, Vec<String>);
/// One action on its targets, from one page state: the key both the cycle count and the no-op memory are kept by.
pub(crate) type Signature = (Operation, Option<String>, Option<String>, String);
/// An operation, the id and description of the control it acts on, and the address of the page.
pub(crate) type Commit = (Operation, Option<String>, String, String);

/// A spawned task whose result any number of waiters can read, as an `asyncio.Task` can be awaited by many.
///
/// Waiting never cancels it, as `asyncio.shield` does not; only [`Spawned::cancel`] does, and a task cancelled
/// before it finished reports no result.
pub(crate) struct Spawned<T: Clone + Send + Sync + 'static> {
    handle: Option<JoinHandle<()>>,
    result: watch::Receiver<Option<Result<T, AgentError>>>,
}

impl<T: Clone + Send + Sync + 'static> Spawned<T> {
    pub fn spawn<F>(future: F) -> Spawned<T>
    where
        F: Future<Output = Result<T, AgentError>> + Send + 'static,
    {
        let (sender, result) = watch::channel(None);
        let handle = tokio::spawn(async move {
            let outcome = future.await;
            let _ = sender.send(Some(outcome));
        });
        Spawned { handle: Some(handle), result }
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
    pub partition_failures: HashMap<String, (Vec<String>, Vec<AnswerCorrection>)>,
    pub recovered_outputs: HashSet<String>,
    pub recovered_sources: HashSet<String>,
    pub recovered_collections: HashSet<(String, Option<String>)>,
    pub verified_answer: Option<ComposedAnswer>,
    pub missing_answer_outputs: Vec<String>,
    pub answer_corrections: Vec<AnswerCorrection>,
    pub output_audit_cache: OutputAuditCache,
    /// Verifier verdicts by the digest of the exact request that produced them, within this run.
    pub verify_cache: HashMap<String, Generation<LLMVerdict>>,
    /// Bot-check documents this run has already reloaded once, so a challenge that survives is reported.
    pub challenge_reloads: HashSet<String>,
    pub open_answer_outputs: Vec<String>,
    pub rejected_answer_evidence: Option<BTreeSet<EvidenceKey>>,
    pub answer_check_evidence: Option<String>,
    pub answer_check_cache: AnswerChecks,
    pub relevance_cache: RelevanceCache,
    pub steps: Vec<StepResult>,
    pub would_fire: Vec<Tripwire>,
    pub history: Vec<HistoryEntry>,
    pub http_failure: Option<HttpFailure>,
    pub failed_links: IndexMap<LinkKey, HttpFailure>,
    /// Repeated stale links survive intervening reads, which otherwise reset progress while the same click fails.
    pub stale_links: HashMap<LinkKey, i64>,
    pub pending_link: Option<LinkKey>,
    pub http_seen: HashSet<(String, String, i64)>,
    pub hint: Option<String>,
    /// The operation and control id recovery named, taken when Jev is still unsure of the next step.
    pub directed: Option<(Operation, Option<String>)>,
    /// An observed output section must be opened and read before another model chooses an action.
    pub section_read: bool,
    pub recovery_sections: HashSet<String>,
    /// The commit an authorized run was last refused as unsure it is the one the task means.
    pub unsure_commit: Option<Commit>,
    /// That refused commit once recovery, shown the page and the task, named it as the next step. It covers the next
    /// action only, on that control and page: anything later is asked about afresh.
    pub confirmed_commit: Option<Commit>,
    pub unchanged: i64,
    /// Controls a fill or select has written, by document, so only a field's first new value counts as progress by
    /// itself. A new document restarts control ids, and its fields would otherwise inherit the last page's writes.
    pub written: HashMap<String, HashSet<String>>,
    pub recoveries: i64,
    pub recovered_requirements: HashSet<String>,
    pub terminal_verification_attempted: bool,
    /// Steps, by `step_key`, whose reader a finish refused for an unread requirement asked once more.
    pub unread_rereads: HashSet<String>,
    /// The `step_key` of the capture the last read read.
    pub read_step: Option<String>,
    pub blank_opening_checked: bool,
    pub recovery_log: VecDeque<String>,
    /// `history.len()` when a tripwire last recovered the run. Evidence a recovery already acted on is spent:
    /// `history` only grows, so a repetition count that reached the limit once would hold forever.
    pub recovered_at: usize,
    /// One fingerprint of the still-unevidenced requirements per step, for `stagnant_plan`.
    pub plan_marks: Vec<String>,
    /// What became of each action taken from each page state, which is how a cycle is told from progress.
    pub attempts: HashMap<Signature, Attempts>,
    pub last_page: Option<(String, String)>,
    /// The address of a sign-in wall an access inspection read instead of stopping on.
    pub passed_wall: Option<String>,
    /// Each page state and the action count when it was first reached.
    pub reached: HashMap<String, usize>,
    /// Source evidence already available at each action state; repeated quotes cannot renew recovery there.
    pub reached_with_evidence: HashMap<String, BTreeSet<EvidenceKey>>,
    /// The state the last action that changed the page was taken from, until the next observation judges it.
    pub left: Option<String>,
    /// Each move the run has made: the state it left, the action, and the state it reached.
    pub crossed: HashSet<(String, String, String)>,
    /// The page the last action was taken on, until the next observation says what it did.
    pub acted_from: Option<Observation>,
    pub pending_move: Option<Move>,
    pub moves: Vec<(usize, Move)>,
    /// Each document's committed control values seen so far, which a setting put back to cannot renew recovery.
    pub settings_held: HashSet<(String, BTreeSet<(ControlKey, ControlValue)>)>,
    pub ready_plan: Option<Plan>,
    /// Addresses this run built from the task rather than reached by clicking.
    pub invented: HashSet<String>,
    /// The page is an invented address, or one the page itself moved to from one.
    pub guessing: bool,
    pub lineage: Lineage,
    /// The last action itself took the page elsewhere, as the page reports it; read by the next observation only.
    pub moved_by_action: bool,
    /// Jev's word that the task rules out downloading, asked once a control the page says downloads is in play.
    pub downloads_forbidden: Option<bool>,
    /// An interaction changed the page after the notes last read it.
    pub owes_read: bool,
    /// This page has been read since it last changed.
    pub read_here: bool,
    /// Page states where an unsure pick has been acted on instead of recovering; the next one there recovers.
    pub tried_unsure: HashSet<String>,
    /// The fields, settings and submit controls each address has shown.
    pub form_controls_at: HashMap<String, HashSet<String>>,
    /// Attempted reads by document, exact content and outstanding requirements, independent of URL edits.
    pub reads: HashSet<ReadKey>,
    /// Documents whose pagination records were read outside the ordinary read cache.
    pub record_read_documents: HashSet<String>,
    /// Reads by document, page state and outstanding requirements that added no fact.
    pub barren: HashMap<ReadKey, i64>,
    /// Page states, by `seen`, the run acted on without reading them: Jev judged them not worth a read then.
    pub passed: HashSet<String>,
    /// Every action since the run was last on a passed state only typed into a field on it.
    pub typed_on_passed: bool,
    /// The page state, by `seen`, the last action was taken on.
    pub acted_on: Option<String>,
    /// By page state, each requirement a read of it evidenced and the capture it read.
    pub evidenced_at: HashMap<String, IndexMap<String, String>>,
    /// By page state, the capture its last read read, evidence or not.
    pub read_at: HashMap<String, String>,
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
    pub observed_links: IndexMap<String, ObservedLink>,
    pub read_urls: HashSet<String>,
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
    pub form_continues: bool,
    pub form_rejected: HashSet<(String, Option<String>, String)>,
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
            partition_failures: HashMap::new(),
            recovered_outputs: HashSet::new(),
            recovered_sources: HashSet::new(),
            recovered_collections: HashSet::new(),
            verified_answer: None,
            missing_answer_outputs: Vec::new(),
            answer_corrections: Vec::new(),
            output_audit_cache: Default::default(),
            verify_cache: HashMap::new(),
            challenge_reloads: HashSet::new(),
            open_answer_outputs: Vec::new(),
            rejected_answer_evidence: None,
            answer_check_evidence: None,
            answer_check_cache: HashMap::new(),
            relevance_cache: RelevanceCache::default(),
            steps: Vec::new(),
            would_fire: Vec::new(),
            history: Vec::new(),
            http_failure: None,
            failed_links: IndexMap::new(),
            stale_links: HashMap::new(),
            pending_link: None,
            http_seen: HashSet::new(),
            hint: None,
            directed: None,
            section_read: false,
            recovery_sections: HashSet::new(),
            unsure_commit: None,
            confirmed_commit: None,
            unchanged: 0,
            written: HashMap::new(),
            recoveries: 0,
            recovered_requirements: HashSet::new(),
            terminal_verification_attempted: false,
            unread_rereads: HashSet::new(),
            read_step: None,
            blank_opening_checked: false,
            recovery_log: VecDeque::new(),
            recovered_at: 0,
            plan_marks: Vec::new(),
            attempts: HashMap::new(),
            last_page: None,
            passed_wall: None,
            reached: HashMap::new(),
            reached_with_evidence: HashMap::new(),
            left: None,
            crossed: HashSet::new(),
            acted_from: None,
            pending_move: None,
            moves: Vec::new(),
            settings_held: HashSet::new(),
            ready_plan: None,
            invented: HashSet::new(),
            guessing: false,
            lineage: Lineage::default(),
            moved_by_action: false,
            downloads_forbidden: None,
            owes_read: false,
            read_here: false,
            tried_unsure: HashSet::new(),
            form_controls_at: HashMap::new(),
            reads: HashSet::new(),
            record_read_documents: HashSet::new(),
            barren: HashMap::new(),
            passed: HashSet::new(),
            typed_on_passed: false,
            acted_on: None,
            evidenced_at: HashMap::new(),
            read_at: HashMap::new(),
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
            observed_links: IndexMap::new(),
            read_urls: HashSet::new(),
            redecided: false,
            missing: HashSet::new(),
            continuing: HashSet::new(),
            incomplete: BTreeSet::new(),
            transaction_candidates: Vec::new(),
            form_continues: false,
            form_rejected: HashSet::new(),
            form_values: HashMap::new(),
        }
    }

    pub fn reset_answer_check(&mut self) {
        self.missing_answer_outputs.clear();
        self.answer_corrections.clear();
    }

    /// The plan, for code that only runs after `await_plan`: reading, judging DONE and answering.
    pub fn plan(&self) -> &Plan {
        self.ready_plan.as_ref().expect("the plan was used before it was awaited")
    }

    /// Record a recovery's record, keeping only the most recent few.
    pub fn log_recovery(&mut self, record: String) {
        if self.recovery_log.len() == RECOVERY_RECORDS {
            self.recovery_log.pop_front();
        }
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
