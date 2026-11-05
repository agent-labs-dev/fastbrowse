//! What one run keeps between steps, and the stops a step can raise.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use fastbrowse_core::decision::DecisionError;
use fastbrowse_core::llm::LLMError;
use fastbrowse_core::models::{Attachment, Authorization, Evidence, Status, StepChoice, StepResult, WallKind};
use fastbrowse_core::page::{BrowserError, Capture, Control, Observation};
use fastbrowse_core::telemetry::{BudgetExceeded, Ledger};
use indexmap::IndexMap;

use super::recovery::{Idle, Withheld};
use crate::error::AgentError;
use crate::memory::{Fact, Notes, address, fact_id};
use crate::policy::{Decision, HistoryEntry, RelevanceCache};
use crate::retrieval::ComposedAnswer;
use crate::retrieval::whole_page_facts;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputIntent {
    Unknown,
    Text,
    Image,
    TextAndImage,
    CarriedOut,
}

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

/// What a fill or select has written into one document: the fields it has written to, and each field with the
/// exact text or selection it received. A widget that redraws on every keystroke (a date picker's grid) changes the
/// page while the text lands nowhere, so the pair is what tells a repeat write from a new one.
///
/// A field is known by what the page says it is, not by its element: a form that rejects a submit can draw its fields
/// again, and the new element is still the field the run already wrote.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Written {
    fields: HashSet<String>,
    values: HashSet<String>,
}

impl Written {
    fn field(control: &Control) -> String {
        serde_json::json!([control.role, control.label, control.context, control.input_name]).to_string()
    }

    /// One field with one value, kept as a single key because a document holds many of both.
    fn key(control: &Control, value: &str) -> String {
        format!("{}\u{0}{value}", Self::field(control))
    }

    pub(crate) fn record(&mut self, control: &Control, value: &str) {
        self.fields.insert(Self::field(control));
        self.values.insert(Self::key(control, value));
    }

    /// Whether this field has received any value.
    pub(crate) fn wrote(&self, control: &Control) -> bool {
        self.fields.contains(&Self::field(control))
    }

    pub(crate) fn holds(&self, control: &Control, value: &str) -> bool {
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
            // A 403 is a refusal of access, and "the cause is unknown" says less than the status does. The status alone
            // does not say whether it is a bot check or a sign-in that credentials pass, so it names no wall: the page's
            // own classification names a bot check. Every other status keeps the recorded wording, so replaying
            // recorded Python traffic still matches byte for byte.
            FailureCause::Http(403) => format!(
                "HTTP 403 at {}: the site refused this browser session, which may be an access or bot check or a sign-in it requires.",
                self.url
            ),
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
    /// The recipe this run replays, while it has steps left to offer.
    pub replay: Option<super::replay::Replay>,
    /// The steps this run carried out, kept as a recipe a later run can replay.
    pub recipe_log: Vec<fastbrowse_core::recipe::RecipeStep>,
    /// Where the text of the fill being built came from, when a caller input or a stored secret gave it.
    pub text_source: Option<fastbrowse_core::recipe::RecipeText>,
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
    /// What a fill or select has written, by page address, so only a field's first new value counts as progress by
    /// itself. A submit that loads the same address again is still the same form: a new document there does not make
    /// its fields new.
    pub written: HashMap<String, Written>,
    pub recoveries: i64,
    pub blank_opening_checked: bool,
    /// Blank states waited out during this loading episode, cleared once a readable page appears.
    pub blank_waited: HashSet<(String, String)>,
    /// The one renderer hang this run may recover without risking a replay of the action.
    pub hung_page_reopened: bool,
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
    /// A caller's schema is filled after DONE, so its result is not a selected prose answer yet.
    pub extraction_pending: bool,
    /// A rejected draft is feedback, never a new page fact or evidence of progress.
    pub composition_feedback: Option<String>,
    pub composition_partial: Option<(ComposedAnswer, Vec<(Fact, bool)>)>,
    /// Pages read for one fact, whole: Jev can pick one fact from a list's first page and only see on the next that
    /// the answer is composed. They join the notes for the composer alone, since steps deciding on them stopped early.
    pub read_whole: Vec<Capture>,
    /// A DONE was refused because the task is something to carry out, not to be told.
    pub to_carry_out: bool,
    pub output_intent: OutputIntent,
    /// Jev's judgement, asked with `output_intent`, that the task asks how the page looks, which only its pixels show.
    pub appearance: bool,
    /// Jev's reading that the task covers a whole list across its pages, such as a count over a site.
    pub every_page: bool,
    /// Each next page a list named, by absolute address, in the order the run saw them.
    pub pager_pages: Vec<String>,
    /// Next pages the run opened itself because a whole-list task tried to finish before reading them.
    pub pager_followed: HashSet<String>,
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

    pub fn composition_sources(notes: &Notes) -> Vec<(Fact, bool)> {
        notes.facts().iter().map(|fact| ((*fact).clone(), notes.superseded(&fact_id(fact)))).collect()
    }

    pub fn checked_composition(&self, notes: &Notes) -> Option<&ComposedAnswer> {
        let (partial, sources) = self.composition_partial.as_ref()?;
        (!partial.claims.is_empty() && *sources == Self::composition_sources(notes)).then_some(partial)
    }

    /// What one read leaves in the notes: its facts, the page kept whole when one fact was picked from it, and the
    /// earlier reads of that address it supersedes.
    pub fn record_read(&mut self, capture: &Capture, facts: Vec<Fact>, compose: bool) {
        let added = facts.len();
        self.notes.remember_capture(capture);
        for fact in facts {
            self.notes.add(fact);
        }
        // A fragment names a view of the same page: an earlier view kept whole would come back as current notes when
        // composing, beside the read that superseded it.
        let page = address(&capture.url);
        self.read_whole.retain(|read| address(&read.url) != page);
        if !compose && added > 0 {
            self.read_whole.push(capture.clone());
        }
        // An absent read must not mistake a stored quote from a revisited capture for fresh evidence.
        if added > 0 {
            self.notes.supersede(&capture.url, &capture.sha256, &capture.text);
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

    /// Remember each next page the observed list names, by its absolute address. Only this site's pages count: a link
    /// to another origin is no more of this list, and its href is not a path joined against this page.
    pub fn note_pager_pages(&mut self, observation: &Observation) {
        let Ok(here) = url::Url::parse(&observation.url) else { return };
        for control in observation.controls.iter().filter(|control| fastbrowse_core::page::pager_link(control)) {
            let Some(page) = control.href.as_deref().and_then(|href| here.join(href).ok()) else { continue };
            if page.origin() != here.origin() {
                continue;
            }
            let page = page.to_string();
            if !self.pager_pages.contains(&page) {
                self.pager_pages.push(page);
            }
        }
    }

    /// The first next page a list named that this run has neither read nor opened for that reason before.
    pub fn unread_pager_page(&self) -> Option<&String> {
        self.pager_pages
            .iter()
            .find(|page| !self.read_urls.contains(page.as_str()) && !self.pager_followed.contains(page.as_str()))
    }

    /// Mark each link that leads to a page already read. A mark on the link itself tells apart links that share a
    /// label, as every quote's "(about)" does, where a list of labels could not.
    pub fn mark_read_links(&self, observation: &mut Observation) {
        let here = url::Url::parse(&observation.url).ok();
        for control in &mut observation.controls {
            let leads_to = control.href.as_ref().and_then(|href| here.as_ref()?.join(href).ok());
            if leads_to.is_some_and(|url| self.read_urls.contains(url.as_str())) {
                control.already_read = Some(true);
            }
        }
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
            replay: None,
            recipe_log: Vec::new(),
            text_source: None,
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
            hung_page_reopened: false,
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
            extraction_pending: false,
            composition_feedback: None,
            composition_partial: None,
            read_whole: Vec::new(),
            to_carry_out: false,
            output_intent: OutputIntent::Unknown,
            appearance: false,
            every_page: false,
            pager_pages: Vec::new(),
            pager_followed: HashSet::new(),
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
