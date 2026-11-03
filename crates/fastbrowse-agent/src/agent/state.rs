//! What one run keeps between steps: `_RunState` and the small records it holds, and the stops a step can raise.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use fastbrowse_core::decision::DecisionError;
use fastbrowse_core::llm::LLMError;
use fastbrowse_core::models::{Attachment, Authorization, Evidence, Status, StepChoice, StepResult, WallKind};
use fastbrowse_core::page::{BrowserError, Capture, Observation};
use fastbrowse_core::telemetry::{BudgetExceeded, Ledger};
use indexmap::IndexMap;

use super::recovery::{Idle, Withheld};
use crate::error::AgentError;
use crate::memory::Notes;
use crate::policy::{Decision, HistoryEntry, RelevanceCache};
use crate::retrieval::whole_page_facts;

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

/// What a fill or select has written into one document: the controls it has written to, and each control with the
/// exact text or selection it received. A widget that redraws on every keystroke (a date picker's grid) changes the
/// page while the text lands nowhere, so the pair is what tells a repeat write from a new one.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Written {
    pub controls: HashSet<String>,
    values: HashSet<String>,
}

impl Written {
    /// One control with one value, kept as a single key because a document holds many of both.
    fn key(control: &str, value: &str) -> String {
        format!("{control}\u{0}{value}")
    }

    pub(crate) fn record(&mut self, control: &str, value: &str) {
        self.controls.insert(control.to_string());
        self.values.insert(Self::key(control, value));
    }

    pub(crate) fn holds(&self, control: &str, value: &str) -> bool {
        self.values.contains(&Self::key(control, value))
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

/// A stop, recovery request or typed failure that interrupts the current step.
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
    /// What a fill or select has written, by document, so only a field's first new value counts as progress by
    /// itself. A new document restarts control ids, and its fields would otherwise inherit the last page's writes.
    pub written: HashMap<String, Written>,
    pub recoveries: i64,
    pub blank_opening_checked: bool,
    /// Blank states waited out during this loading episode, cleared once a readable page appears.
    pub blank_waited: HashSet<(String, String)>,
    pub last_page: Option<(String, String)>,
    /// The source of the latest navigation, scoped to the destination that followed it.
    pub navigation_from: Option<((String, String), Observation)>,
    /// Jev's sign-in readings stay with the document they described, not with every later route.
    pub login_readings: HashMap<(String, String), f64>,
    /// Each page state reached, with `evidence_reads` when it was last on screen: going back to one with nothing read
    /// since is going round, not forward.
    pub reached: HashMap<String, usize>,
    /// Reads that added evidence, counted over the run.
    pub evidence_reads: usize,
    /// The action whose page change the next observation judges: a new page state, or one revisited after new
    /// evidence, is progress.
    pub landing: Option<Withheld>,
    /// The actions that moved nothing since the last one that did. Done again they do the same, so asks leave them out;
    /// one the browser refused as covered is left out only until a performed action changes the page.
    pub idle: Vec<Idle>,
    /// The page the last action was taken on, until the next observation says what it did.
    pub acted_from: Option<Observation>,
    /// Jev's word that the task rules out downloading, asked once a control the page says downloads is in play.
    pub downloads_forbidden: Option<bool>,
    /// Pages read, by `read_key`, each with whether an action changed it since it was last read.
    pub read_documents: HashMap<(String, String), bool>,
    /// Screenshots that a successful image read found held no answer for this run's task.
    pub unanswered_images: HashSet<String>,
    /// The page the last action changed, until the next observation shows it is still the page in view.
    pub changed_page: Option<(String, String)>,
    /// Jev's latest word is that this page reports the site failed, so nothing on it is read as evidence.
    pub page_failed: bool,
    /// Jev said a read's answer must be composed, so the run ends on the LLM's writing of the notes.
    pub compose: bool,
    /// Pages read for one fact, whole: Jev can pick one fact from a list's first page and only see on the next that
    /// the answer is composed. They join the notes for the composer alone, since steps deciding on them stopped early.
    pub read_whole: Vec<Capture>,
    /// A DONE was refused because the task is something to carry out, not to be told.
    pub to_carry_out: bool,
    /// Addresses read before the run moved from fetching to a browser, and not read in the browser yet.
    pub fetched: HashSet<String>,
    /// The action being taken commits, by Jev's judgement, so a failure it lands on must not be retried.
    pub committing: bool,
    /// Pages read, by content: a document can show several steps of one flow.
    pub read_pages: HashSet<String>,
    /// The addresses of pages read, so Jev can be told which links lead back to one.
    pub read_urls: HashSet<String>,
    /// The address of the link the last click followed, until the next observation shows where it landed.
    pub followed: Option<String>,
    /// The page a followed link landed on, by `read_key`, with that link's address. A site can redirect a link to
    /// another address, and the link still leads to that page once it is read.
    pub arrived_by: Option<((String, String), String)>,
    /// The initial address, retained even when an HTTP failure removes it from completion evidence.
    pub started_url: Option<String>,
    pub caller_start: Option<String>,
    pub start_landing_url: Option<String>,
    /// The first page the run looked at, which is what a task's "this page" means once the run has moved on.
    pub first_url: Option<String>,
    /// A decision was dropped because the page redrew under it, so nothing is watched until an action is taken.
    pub redecided: bool,
    /// Fields the task gives no value for, which recovery has been told about once, by page, label and context.
    pub missing: HashSet<(String, String, Option<String>)>,
}

impl RunState {
    /// The address of the page at `url` when a site failure that can clear stands on it: opening it again reloads it.
    pub fn reload_target(&self, url: &str) -> Option<String> {
        self.site_failure.as_ref().filter(|failure| failure.url == url && failure.transient()).map(|f| f.url.clone())
    }

    pub fn read_note(&self) -> &'static str {
        if self.notes.answered() {
            "Read this content. The notes hold evidence that answers the task."
        } else if self.compose && (!self.notes.evidence().is_empty() || !self.read_whole.is_empty()) {
            "Read this content. The notes hold source material for an answer that still needs composing."
        } else {
            "Read this content. The notes hold nothing that answers the task."
        }
    }

    /// The notes with the pages read for one fact whole.
    pub fn notes_to_compose(&self) -> Notes {
        let mut notes = self.notes.clone();
        for fact in self.read_whole.iter().flat_map(whole_page_facts) {
            notes.add(fact);
        }
        notes
    }

    /// An action changed the page it was taken on. Leaving that page leaves what was read of it as it was: going back
    /// shows it again, not a change to it.
    pub fn settle_change(&mut self, landed: &Observation) {
        if let Some(page) = self.changed_page.take()
            && page == read_key(landed)
            && let Some(read) = self.read_documents.get_mut(&page)
        {
            *read = true;
        }
    }

    /// The page is read now, so the notes hold what it shows, and every link known to lead to it leads to a read page.
    pub fn mark_read(&mut self, observation: &Observation) {
        let page = read_key(observation);
        if let Some((landed, href)) = &self.arrived_by
            && *landed == page
        {
            self.read_urls.insert(href.clone());
        }
        self.read_documents.insert(page, false);
        self.read_pages.insert(observation.page_key.clone());
        self.read_urls.insert(observation.url.clone());
    }

    pub fn carried_on(self, ledger: Arc<Ledger>) -> RunState {
        RunState {
            ledger,
            fetched: self.read_urls.clone(),
            picked: None,
            landing: None,
            idle: Vec::new(),
            acted_from: None,
            followed: None,
            changed_page: None,
            unchanged: 0,
            committing: false,
            redecided: false,
            last_page: None,
            navigation_from: None,
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
            blank_waited: HashSet::new(),
            last_page: None,
            navigation_from: None,
            login_readings: HashMap::new(),
            reached: HashMap::new(),
            evidence_reads: 0,
            landing: None,
            idle: Vec::new(),
            acted_from: None,
            downloads_forbidden: None,
            read_documents: HashMap::new(),
            unanswered_images: HashSet::new(),
            changed_page: None,
            page_failed: false,
            compose: false,
            read_whole: Vec::new(),
            to_carry_out: false,
            fetched: HashSet::new(),
            committing: false,
            read_pages: HashSet::new(),
            read_urls: HashSet::new(),
            followed: None,
            arrived_by: None,
            started_url: None,
            caller_start: None,
            start_landing_url: None,
            first_url: None,
            redecided: false,
            missing: HashSet::new(),
        }
    }
}

/// What a read is filed under: the document, and its address within it. A single-page app routes between addresses
/// in one document, and each address shows its own page.
pub(crate) fn read_key(observation: &Observation) -> (String, String) {
    (observation.document_key.clone(), observation.url.clone())
}
