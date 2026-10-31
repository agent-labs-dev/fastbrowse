//! What one run keeps between steps: `_RunState` and the small records it holds, and the stops a step can raise.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use fastbrowse_core::decision::DecisionError;
use fastbrowse_core::llm::LLMError;
use fastbrowse_core::models::{Attachment, Authorization, Evidence, Status, StepChoice, StepResult, WallKind};
use fastbrowse_core::page::{BrowserError, Observation};
use fastbrowse_core::telemetry::{BudgetExceeded, Ledger};
use indexmap::IndexMap;

use super::recovery::Withheld;
use crate::error::AgentError;
use crate::memory::Notes;
use crate::policy::{Decision, HistoryEntry, RelevanceCache};

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

/// The step cannot go on as Jev picked it: Jev is asked again, not the caller.
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

pub(crate) struct RunState {
    pub task: String,
    /// The caller's background: shown beside the task, never read as what it asks for.
    pub context: Option<String>,
    pub inputs: IndexMap<String, String>,
    pub attachments: Vec<Attachment>,
    pub authorization: Authorization,
    pub ledger: Arc<Ledger>,
    pub notes: Notes,
    /// Bot-check documents this run has already reloaded once, so a challenge that survives is reported.
    pub challenge_reloads: HashSet<String>,
    pub relevance_cache: RelevanceCache,
    pub steps: Vec<StepResult>,
    /// What Jev was offered for the latest decision, for the next step recorded, which is the one that decision led to.
    pub choices: Vec<StepChoice>,
    pub history: Vec<HistoryEntry>,
    pub site_failure: Option<SiteFailure>,
    pub http_seen: HashSet<(String, String, i64)>,
    pub hint: Option<String>,
    /// Jev's pick when asked again after a stall, by the page state it was asked on, taken as the
    /// next decision there.
    pub picked: Option<(String, Decision)>,
    pub unchanged: i64,
    /// Controls a fill or select has written, by document, so only a field's first new value counts as progress by
    /// itself. A new document restarts control ids, and its fields would otherwise inherit the last page's writes.
    pub written: HashMap<String, HashSet<String>>,
    pub recoveries: i64,
    pub blank_opening_checked: bool,
    pub last_page: Option<(String, String)>,
    /// Each page state reached, with `evidence_reads` when it was last on screen: going back to one with nothing read
    /// since is going round, not forward.
    pub reached: HashMap<String, usize>,
    /// Reads that added evidence, counted over the run.
    pub evidence_reads: usize,
    /// The action whose page change the next observation judges: a new page state, or one revisited after new
    /// evidence, is progress.
    pub landing: Option<Withheld>,
    /// The page the last action was taken on, until the next observation says what it did.
    pub acted_from: Option<Observation>,
    /// Jev's word that the task rules out downloading, asked once a control the page says downloads is in play.
    pub downloads_forbidden: Option<bool>,
    /// Documents read, by key, each with whether an action changed it since it was last read.
    pub read_documents: HashMap<String, bool>,
    /// Jev's latest word is that this page reports the site failed, so nothing on it is read as evidence.
    pub page_failed: bool,
    /// Jev said a read's answer must be composed, so the run ends on the LLM's writing of the notes.
    pub compose: bool,
    /// The action being taken commits, by Jev's judgement, so a failure it lands on must not be retried.
    pub committing: bool,
    /// Pages read, by content: a document can show several steps of one flow.
    pub read_pages: HashSet<String>,
    /// The addresses of pages read, so Jev can be told which links lead back to one.
    pub read_urls: HashSet<String>,
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
}

impl RunState {
    /// The address of the page at `url` when a site failure that can clear stands on it: opening it again reloads it.
    pub fn reload_target(&self, url: &str) -> Option<String> {
        self.site_failure.as_ref().filter(|failure| failure.url == url && failure.transient()).map(|f| f.url.clone())
    }

    /// This run on a new page under `ledger`: everything it learned, and nothing about an action still in flight.
    pub fn carried_on(self, ledger: Arc<Ledger>) -> RunState {
        RunState {
            ledger,
            picked: None,
            landing: None,
            acted_from: None,
            unchanged: 0,
            committing: false,
            redecided: false,
            last_page: None,
            ..self
        }
    }

    pub fn new(
        task: String,
        inputs: IndexMap<String, String>,
        attachments: Vec<Attachment>,
        authorization: Authorization,
        ledger: Arc<Ledger>,
    ) -> RunState {
        RunState {
            task,
            context: None,
            inputs,
            attachments,
            authorization,
            ledger,
            notes: Notes::default(),
            challenge_reloads: HashSet::new(),
            relevance_cache: RelevanceCache::default(),
            steps: Vec::new(),
            choices: Vec::new(),
            history: Vec::new(),
            site_failure: None,
            http_seen: HashSet::new(),
            hint: None,
            picked: None,
            unchanged: 0,
            written: HashMap::new(),
            recoveries: 0,
            blank_opening_checked: false,
            last_page: None,
            reached: HashMap::new(),
            evidence_reads: 0,
            landing: None,
            acted_from: None,
            downloads_forbidden: None,
            read_documents: HashMap::new(),
            page_failed: false,
            compose: false,
            committing: false,
            read_pages: HashSet::new(),
            read_urls: HashSet::new(),
            started_url: None,
            caller_start: None,
            start_landing_url: None,
            first_url: None,
            visited: IndexMap::new(),
            redecided: false,
            missing: HashSet::new(),
        }
    }
}
