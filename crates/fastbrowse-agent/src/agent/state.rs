//! What one run keeps between steps, and the stops a step can raise.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use fastbrowse_core::decision::{Answer, DecisionError};
use fastbrowse_core::llm::{Generation, LLMError};
use fastbrowse_core::models::{Attachment, Authorization, Evidence, Status, StepChoice, StepResult, WallKind};
use fastbrowse_core::page::{BrowserError, Capture, Control, Observation};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::telemetry::{BudgetExceeded, Ledger};
use indexmap::IndexMap;

use super::recovery::{Idle, Withheld};
use crate::error::AgentError;
use crate::memory::{Fact, Notes, address, fact_id};
use crate::policy::{Decision, HistoryEntry};
use crate::retrieval::ComposedAnswer;
use crate::retrieval::whole_page_facts;
use crate::start::KnownPages;

/// Reads a page gets between finished reads: one that did not finish is tried once more, then the page counts as read.
const READ_TRIES: u8 = 2;

/// How much of a list the task covers, by Jev's reading of the task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Extent {
    /// The records one page shows, or a single item.
    #[default]
    Shown,
    /// Everything the list holds across all of its pages.
    EveryPage,
    /// The pages of the list the task names, first and last, numbered from the list's first page. `None` when the
    /// task names pages but no numbers could be read from it, so no page is walked or left out.
    Pages(Option<(usize, usize)>),
}

impl Extent {
    /// Whether the run reads the list's next pages itself before it finishes.
    pub fn walks(&self) -> bool {
        matches!(self, Self::EveryPage | Self::Pages(Some(_)))
    }
}

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
        Flow::Agent(error.into())
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
            // A 404 or 410 says what happened as plainly: runs on a project page awaiting moderation ended "the cause is
            // unknown" (#931), though the site had said the page does not exist.
            FailureCause::Http(status @ (404 | 410)) => {
                format!("HTTP {status} at {}: the page does not exist.", self.url)
            }
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

/// The known pages request a run sent while its first page opened, answered with its cost unrecorded: the run that
/// takes the answer records it, which after an escalation to a browser is the browser run's ledger. Dropped, the request
/// is aborted, so a cancelled run's request stops with it and records what it was billed.
pub(crate) struct KnownAsked(tokio::task::JoinHandle<crate::error::Result<Generation<KnownPages>>>);

impl KnownAsked {
    pub fn new(asked: tokio::task::JoinHandle<crate::error::Result<Generation<KnownPages>>>) -> KnownAsked {
        KnownAsked(asked)
    }

    pub async fn answer(mut self) -> crate::error::Result<Generation<KnownPages>> {
        (&mut self.0).await.unwrap_or_else(|error| Err(AgentError::Invalid(format!("not answered: {error}"))))
    }
}

impl Drop for KnownAsked {
    fn drop(&mut self) {
        self.0.abort();
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
    /// The task as the caller wrote it, before `with_inputs`, which is what a recipe keeps.
    pub given_task: String,
    /// The caller's background: shown beside the task, never read as what it asks for.
    pub context: Option<String>,
    pub inputs: IndexMap<String, String>,
    pub attachments: Vec<Attachment>,
    pub authorization: Authorization,
    pub ledger: Arc<Ledger>,
    pub notes: Notes,
    /// Bot-check documents this run has already reloaded once, so a challenge that survives is reported.
    pub challenge_reloads: HashSet<String>,
    /// Addresses where a bot check did not clear, and the routes proposed around them, each opened once.
    pub walled: Vec<String>,
    pub routes: HashSet<String>,
    /// Pages the site is known to serve for the items the task names, offered as places to navigate until each has
    /// been open (#983).
    pub known_pages: Vec<String>,
    /// The request for `known_pages` while it is out. A run decides its first step while it is, and waits for it only
    /// before acting on a decision or proposing another route, so the request is not on every run's first step.
    pub known_asked: Option<KnownAsked>,
    /// The known pages the run has been on: each stays one the run may navigate to, and is not offered again.
    pub reached_known_pages: Vec<String>,
    pub steps: Vec<StepResult>,
    /// What Jev was offered for the latest decision, for the next step recorded, which is the one that decision led to.
    pub choices: Vec<StepChoice>,
    pub history: Vec<HistoryEntry>,
    pub site_failure: Option<SiteFailure>,
    /// A failure on the address the run started at, kept after the run leaves it until that address loads. A run
    /// whose start page answered 404 searched the site, stalled on the results and ended `no_progress`, since leaving
    /// the address forgot its failure (#931).
    pub start_failure: Option<SiteFailure>,
    pub http_seen: HashSet<(String, String, i64)>,
    pub hint: Option<String>,
    /// Jev's pick when asked again after a stall, by the page state it was asked on, taken as the
    /// next decision there.
    pub picked: Option<(String, Decision)>,
    /// The action the next step carries out, built while the page was read.
    pub(super) built: Option<Step<super::driving::Watched<fastbrowse_core::page::Action>>>,
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
    /// The document in which Jev was last asked whether the task needs an action inside a drawn surface.
    pub surface_judged: Option<(String, String)>,
    /// The source of the latest navigation, scoped to the destination that followed it.
    pub navigation_from: Option<((String, String), Observation)>,
    /// Jev's sign-in readings stay with the document they described, not with every later route.
    pub login_readings: HashMap<(String, String), f64>,
    /// Each page state reached, with `evidence_reads` when it was last on screen: going back to one with nothing read
    /// since is going round, not forward.
    pub reached: HashMap<String, usize>,
    /// Addresses actually observed, including pages reached through ordinary links. Recovery proposes unseen routes.
    pub visited_addresses: HashSet<String>,
    /// Reads that added evidence, counted over the run.
    pub evidence_reads: usize,
    /// `evidence_reads` when a return to a page state already reached last counted as progress. One read buys one
    /// return: a run going round a cycle of several states otherwise had each of them judged a return after new
    /// evidence in turn, so one read on Google Flights renewed progress four times while it cleared and reapplied a
    /// filter the site failed on (#976).
    pub returned_at: usize,
    /// The action whose page change the next observation judges: a new page state, or the first return after new
    /// evidence to one seen before it, is progress.
    pub landing: Option<Withheld>,
    /// The actions that moved nothing since the last one that did. Done again they do the same, so asks leave them out;
    /// one the browser refused as covered is left out only until a performed action changes the page.
    pub idle: Vec<Idle>,
    /// The page the last action was taken on, until the next observation says what it did.
    pub acted_from: Option<Observation>,
    /// Jev's word that the task rules out downloading, asked once a control the page says downloads is in play.
    pub downloads_forbidden: Option<bool>,
    /// Jev's word on which changes to the page's fields and settings the task allows, asked once an edit is in play.
    pub edits: Option<Edits>,
    /// Pages read, by `read_key`, each with whether an action changed it since it was last read.
    pub read_documents: HashMap<(String, String), bool>,
    /// Reads begun on each page, by `read_key`, since its last finished read, so a read that did not finish is retried
    /// once and never loops.
    pub read_tries: HashMap<(String, String), u8>,
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
    pub composition_partial: Option<PartialComposition>,
    /// The caller's schema result filled when a draft was sent back, so a run that ends on the rewrite still returns it.
    pub schema_partial: Option<crate::verification::Extraction>,
    /// Pages read for one fact, whole: Jev can pick one fact from a list's first page and only see on the next that
    /// the answer is composed. They join the notes for the composer alone, since steps deciding on them stopped early.
    pub read_whole: Vec<Capture>,
    /// A DONE was refused because the task is something to carry out, not to be told.
    pub to_carry_out: bool,
    /// Over HTTP, the task asks for a step besides opening and reading pages, whatever it wants reported.
    pub asks_steps: bool,
    /// The latest finish check found a step the task asks for that the site does not allow.
    pub refused: bool,
    /// The latest finish check found this step the task asks to be carried out not yet done.
    pub undone: Option<String>,
    pub output_intent: OutputIntent,
    /// Startup judgments are applied when first needed. Kept behind a pointer because growing the run state
    /// overflowed a debug worker stack during a fill.
    pub output_answers: Option<Box<IndexMap<String, Answer>>>,
    /// What the task says about signing in, asked of the task alone at the first sign-in wall. None until then, and
    /// when Jev gave no answer.
    pub sign_in: Option<SignInNeeds>,
    /// The dates the task names, as the parts it states, read once before the first page. Code works out the date
    /// each means against the page's capture date.
    pub dates: Vec<crate::dates::DateReference>,
    /// Jev's judgement, asked with `output_intent`, that the task asks how the page looks, which only its pixels show.
    pub appearance: bool,
    /// Jev's reading of how much of a list the task covers: a whole list across its pages, such as a count over a
    /// site, or only the pages the task names.
    pub extent: Extent,
    /// Each next page a list named, by absolute address, in the order the run saw them.
    pub pager_pages: Vec<String>,
    /// The page that began the list `pager_pages` continues.
    pub pager_start: Option<String>,
    /// Each page of the list's number, by `crate::memory::address`, counted along its next links from `pager_start`
    /// as page 1. A list the run entered part way through is numbered from where it entered.
    pub pager_ordinals: HashMap<String, usize>,
    /// Next pages the run opened itself because a whole-list task tried to finish before reading them.
    pub pager_followed: HashSet<String>,
    /// Addresses read before the run moved from fetching to a browser, and not read in the browser yet.
    pub fetched: HashSet<String>,
    /// The action being taken commits, by Jev's judgement, so a failure it lands on must not be retried.
    pub committing: bool,
    /// Commits this run has carried out, each counted once the page after it did not fail. A later commit is checked
    /// against the page and the run's own steps, so one the task asks for once is not sent again (#882).
    pub commits: usize,
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
    /// The fields among `missing` the task asks nothing of. One the page does not mark required is no input the run
    /// needs: three runs that replaced a description and stalled ended `needs_input` naming blank links and a summary
    /// the task never mentioned (#935).
    pub untouched: HashSet<(String, String, Option<String>)>,
    /// The step that first showed each control on the page it stayed on, by document and control id. A page can
    /// attach what a step opens at the end of its body, such as a date popup, where nothing near it names the field
    /// that opened it.
    pub shown_by: HashMap<(String, String), String>,
    /// Every control seen on the current document, by document and control id. One the page showed before and a step
    /// brought back is not new: a wizard that returns to its first step after Submit shows the same fields it opened
    /// with, and marked as shown by Submit they read as the next step to fill, so a run submitted the wizard again.
    pub seen: HashSet<(String, String)>,
    /// The step the next observation judges, as what showed the controls it finds.
    pub showing: Option<String>,
}

/// A rejected draft's claims that held, under a notice naming what was withheld, the notes it was written from, and how
/// many steps the run had taken then.
pub type PartialComposition = (ComposedAnswer, Vec<(Fact, bool)>, usize);

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

    /// The steps this run carried out that act on a page, as the models deciding steps are shown them.
    pub fn executed_steps(&self) -> Vec<serde_json::Value> {
        self.history.iter().filter(|entry| entry.changed_site()).map(Dump::dump_exclude_none).collect()
    }

    pub fn checked_composition(&self, notes: &Notes) -> Option<&ComposedAnswer> {
        let (partial, sources, steps) = self.composition_partial.as_ref()?;
        let current = *sources == Self::composition_sources(notes) && *steps == self.executed_steps().len();
        (!partial.claims.is_empty() && current).then_some(partial)
    }

    /// What one read leaves in the notes: its facts, the page kept whole when one fact was picked from it, and the
    /// earlier reads of that address it supersedes.
    pub fn record_read(&mut self, capture: &Capture, facts: Vec<Fact>, compose: bool) {
        let added = facts.len();
        let steps_taken = self.history.iter().filter(|entry| entry.changed_site()).count();
        self.notes.remember_capture(capture, steps_taken);
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

    /// The notes with the pages read for one fact whole. A task over some pages of a list leaves out what was read on
    /// its other pages; a page that is not one of the list's, such as a record's own, keeps its notes.
    pub fn notes_to_compose(&self) -> Notes {
        let mut notes = self.notes.clone();
        for fact in self.read_whole.iter().flat_map(whole_page_facts) {
            notes.add(fact);
        }
        if let Extent::Pages(Some(_)) = self.extent {
            notes.retain(|fact| {
                fact.evidence.as_ref().is_none_or(|evidence| {
                    !self.pager_ordinals.contains_key(&crate::memory::address(&evidence.url))
                        || self.in_range(&evidence.url)
                })
            });
        }
        notes
    }

    /// Whether this page of the list is one the task covers: every page for a whole list, and for a task over some
    /// pages, a page whose number is among them.
    pub fn in_range(&self, url: &str) -> bool {
        match self.extent {
            Extent::EveryPage => true,
            Extent::Pages(Some((first, last))) => self
                .pager_ordinals
                .get(&crate::memory::address(url))
                .is_some_and(|ordinal| (first..=last).contains(ordinal)),
            _ => false,
        }
    }

    /// Whether the run reads this page of the list to cover the task: every page for a whole list, and for a task over
    /// some pages, a page up to the last of them, since the pages before the first are the way to it.
    fn on_the_way(&self, url: &str) -> bool {
        match self.extent {
            Extent::EveryPage => true,
            Extent::Pages(Some((_, last))) => {
                self.pager_ordinals.get(&crate::memory::address(url)).is_some_and(|ordinal| *ordinal <= last)
            }
            _ => false,
        }
    }

    /// An action changed the page it was taken on. Leaving that page leaves what was read of it as it was: going back
    /// shows it again, not a change to it.
    pub fn settle_change(&mut self, landed: &Observation) {
        if let Some(page) = self.changed_page.take()
            && page == read_key(landed)
        {
            self.page_changed(page);
        }
    }

    /// An action changed this page, so what was read of it is out of date and a read of it starts its tries again.
    pub fn page_changed(&mut self, page: (String, String)) {
        if let Some(read) = self.read_documents.get_mut(&page) {
            *read = true;
        }
        self.read_tries.remove(&page);
    }

    /// Count a read begun on this page, returning how many have begun since its last finished read.
    pub fn begin_read(&mut self, observation: &Observation) -> u8 {
        let tries = self.read_tries.entry(read_key(observation)).or_default();
        *tries = tries.saturating_add(1);
        *tries
    }

    /// A read of this page ended. It counts the page read once it finished, or once the last try did not.
    pub fn end_read(&mut self, observation: &Observation, finished: bool, tries: u8) {
        if finished {
            self.read_tries.remove(&read_key(observation));
        }
        if finished || tries >= READ_TRIES {
            self.mark_read(observation);
        }
    }

    /// The page is read now: the notes hold what it shows, or a read of it failed to finish twice. Every link known to
    /// lead to it leads to a read page.
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
    ///
    /// A page with a pager that is not itself one of the noted next pages starts another list, and the next pages of
    /// the one before are dropped: a shop's category opened from its front page is read to its end, not the front
    /// page's list of every book. A page with no pager, such as one record's, leaves the list it came from as it was.
    pub fn note_pager_pages(&mut self, observation: &Observation) {
        let Ok(here) = url::Url::parse(&observation.url) else { return };
        let named: Vec<String> = observation
            .controls
            .iter()
            .filter(|control| fastbrowse_core::page::pager_link(control))
            .filter_map(|control| control.href.as_deref().and_then(|href| here.join(href).ok()))
            .filter(|page| page.origin() == here.origin())
            .map(String::from)
            .collect();
        // A noted next page that redirected, as `/page/2/` to `/page/2`, continues the list by the address asked for.
        let requested = self
            .arrived_by
            .as_ref()
            .filter(|(landed, _)| *landed == read_key(observation))
            .map(|(_, href)| href.as_str());
        let continues = [Some(here.as_str()), requested]
            .into_iter()
            .flatten()
            .any(|page| self.pager_start.as_deref() == Some(page) || self.pager_pages.iter().any(|next| next == page));
        if !named.is_empty() && !continues {
            self.pager_pages.clear();
            self.pager_start = Some(here.to_string());
            self.pager_ordinals.clear();
            self.pager_ordinals.insert(crate::memory::address(here.as_str()), 1);
        } else if continues
            && let Some(requested) = requested
            && !self.pager_pages.contains(&here.to_string())
        {
            // Where a next page landed is a page of the list too, so a note read there counts as the list's, and it is
            // the page of the list that was asked for.
            self.pager_pages.push(here.to_string());
            if let Some(&ordinal) = self.pager_ordinals.get(&crate::memory::address(requested)) {
                self.pager_ordinals.entry(crate::memory::address(here.as_str())).or_insert(ordinal);
            }
        }
        let ordinal = self.pager_ordinals.get(&crate::memory::address(here.as_str())).copied();
        for page in named {
            if let Some(ordinal) = ordinal {
                self.pager_ordinals.entry(crate::memory::address(&page)).or_insert(ordinal + 1);
            }
            if !self.pager_pages.contains(&page) {
                self.pager_pages.push(page);
            }
        }
    }

    /// The next pages of the list this run reads that it has not read. A list none of whose pages gave the task a note
    /// is not the one it reads: a shop's front page lists every book, and a one-page category opened from it has no
    /// pager of its own, so the front page's 49 next pages were opened one by one for a question about the category.
    pub fn unread_list_pages(&self) -> impl Iterator<Item = &String> {
        let list: Vec<String> =
            self.pager_start.iter().chain(&self.pager_pages).map(|page| crate::memory::address(page)).collect();
        let noted = self.notes.facts().iter().any(|fact| {
            fact.evidence.as_ref().is_some_and(|evidence| list.contains(&crate::memory::address(&evidence.url)))
        });
        self.pager_pages
            .iter()
            .filter(move |page| noted && !self.read_urls.contains(page.as_str()) && self.on_the_way(page))
    }

    /// The first unread next page of the list this run reads that it has not opened for that reason before.
    pub fn unread_pager_page(&self) -> Option<&String> {
        self.unread_list_pages().find(|page| !self.pager_followed.contains(page.as_str()))
    }

    /// Mark each link that leads to a page already read. A mark on the link itself tells apart links that share a
    /// label, as every quote's "(about)" does, where a list of labels could not.
    ///
    /// A path with and without its trailing slash is one page: a server serving one redirects the other to it, as a
    /// directory-style site does. A page reached by an address the run built, such as a known page, is read under the
    /// form the run asked for, while the site's own links can use the other form; unmarked, a link to the author page
    /// the run had read and left was taken again for the page holding that author's quotes.
    pub fn mark_read_links(&self, observation: &mut Observation) {
        let here = url::Url::parse(&observation.url).ok();
        for control in &mut observation.controls {
            let leads_to = control.href.as_ref().and_then(|href| here.as_ref()?.join(href).ok());
            if leads_to.is_some_and(|url| {
                self.read_urls.contains(url.as_str())
                    || slash_twin(&url).is_some_and(|twin| self.read_urls.contains(twin.as_str()))
            }) {
                control.already_read = Some(true);
            }
        }
    }

    /// Record the step `by` as what showed the controls `shown` on `after` that this document never showed before,
    /// and remember every control `before` and `after` hold as seen.
    pub fn note_shown(&mut self, before: &Observation, after: &Observation, shown: &[String], by: Option<String>) {
        let document = &after.document_key;
        self.shown_by.retain(|(on, _), _| on == document);
        self.seen.retain(|(on, _)| on == document);
        if before.document_key == *document {
            self.seen.extend(before.controls.iter().map(|c| (document.clone(), c.id.clone())));
        }
        if let Some(by) = by {
            for id in shown {
                let key = (document.clone(), id.clone());
                if !self.seen.contains(&key) {
                    self.shown_by.insert(key, by.clone());
                }
            }
        }
        self.seen.extend(after.controls.iter().map(|c| (document.clone(), c.id.clone())));
    }

    /// Mark each control with the step that showed it, so a later step can tell what it belongs to from what opened
    /// it as well as from where it sits.
    pub fn mark_shown(&self, observation: &mut Observation) {
        for control in &mut observation.controls {
            control.shown_by = self.shown_by.get(&(observation.document_key.clone(), control.id.clone())).cloned();
        }
    }

    /// Records what the known pages request cost, for a run that ends without taking its answer. An ending run does not
    /// wait for it, so a time limit still ends the run on time: one still out is aborted.
    pub async fn known_pages_unused(&mut self) {
        if let Some(asked) = self.known_asked.take()
            && asked.0.is_finished()
            && let Ok(answer) = asked.answer().await
        {
            let _ = self.ledger.for_answer().record([answer.cost]);
        }
    }

    pub fn carried_on(self, ledger: Arc<Ledger>) -> RunState {
        RunState {
            ledger,
            fetched: self.read_urls.clone(),
            picked: None,
            built: None,
            landing: None,
            idle: Vec::new(),
            acted_from: None,
            followed: None,
            changed_page: None,
            unchanged: 0,
            committing: false,
            commits: 0,
            redecided: false,
            last_page: None,
            surface_judged: None,
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
            task: with_inputs(&task, &inputs),
            given_task: task,
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
            walled: Vec::new(),
            routes: HashSet::new(),
            known_pages: Vec::new(),
            known_asked: None,
            reached_known_pages: Vec::new(),
            steps: Vec::new(),
            choices: Vec::new(),
            history: Vec::new(),
            site_failure: None,
            start_failure: None,
            http_seen: HashSet::new(),
            hint: None,
            picked: None,
            built: None,
            unchanged: 0,
            written: HashMap::new(),
            recoveries: 0,
            blank_opening_checked: false,
            blank_waited: HashSet::new(),
            hung_page_reopened: false,
            last_page: None,
            surface_judged: None,
            navigation_from: None,
            login_readings: HashMap::new(),
            reached: HashMap::new(),
            visited_addresses: HashSet::new(),
            evidence_reads: 0,
            returned_at: 0,
            landing: None,
            idle: Vec::new(),
            acted_from: None,
            downloads_forbidden: None,
            edits: None,
            read_documents: HashMap::new(),
            read_tries: HashMap::new(),
            unanswered_images: HashSet::new(),
            changed_page: None,
            page_failed: false,
            compose: false,
            extraction_pending: false,
            composition_feedback: None,
            composition_partial: None,
            schema_partial: None,
            read_whole: Vec::new(),
            to_carry_out: false,
            asks_steps: false,
            refused: false,
            undone: None,
            output_intent: OutputIntent::Unknown,
            output_answers: None,
            sign_in: None,
            dates: Vec::new(),
            appearance: false,
            extent: Extent::Shown,
            pager_pages: Vec::new(),
            pager_start: None,
            pager_ordinals: HashMap::new(),
            pager_followed: HashSet::new(),
            fetched: HashSet::new(),
            committing: false,
            commits: 0,
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
            untouched: HashSet::new(),
            shown_by: HashMap::new(),
            seen: HashSet::new(),
            showing: None,
        }
    }
}

/// What a read is filed under: the document, and its address within it. A single-page app routes between addresses
/// in one document, and each address shows its own page.
pub(crate) fn read_key(observation: &Observation) -> (String, String) {
    (observation.document_key.clone(), observation.url.clone())
}

/// `url` with its path's trailing slash added or taken off; none for a site's root, which has only the one form.
fn slash_twin(url: &url::Url) -> Option<url::Url> {
    let path = url.path();
    if path == "/" || path.is_empty() {
        return None;
    }
    let mut twin = url.clone();
    match path.strip_suffix('/') {
        Some(bare) => twin.set_path(bare),
        None => twin.set_path(&format!("{path}/")),
    }
    Some(twin)
}

/// The task with each input it names by `input <name>` followed by the input's value. Jev, offered an input as a
/// separate source for a field, judged that nothing gave the field a value whatever the input was called; a value
/// in the task is one it copies.
pub(crate) fn with_inputs(task: &str, inputs: &IndexMap<String, String>) -> String {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut shown = String::with_capacity(task.len());
    let mut at = 0;
    // One pass over the task as written, so a value is never read as naming another input, and the longest name
    // wins where one name begins another.
    while let Some(found) = task[at..].find("input ").map(|found| at + found) {
        let name_at = found + "input ".len();
        let named = (!task[..found].ends_with(word))
            .then(|| {
                inputs
                    .iter()
                    .filter(|(name, _)| {
                        task[name_at..].starts_with(name.as_str()) && !task[name_at + name.len()..].starts_with(word)
                    })
                    .max_by_key(|(name, _)| name.len())
            })
            .flatten();
        match named {
            Some((name, value)) => {
                let end = name_at + name.len();
                shown.push_str(&task[at..end]);
                shown.push_str(&format!(" ({value})"));
                at = end;
            }
            None => {
                shown.push_str(&task[at..name_at]);
                at = name_at;
            }
        }
    }
    shown.push_str(&task[at..]);
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_shows_each_input_it_names_beside_the_name() {
        let inputs = IndexMap::from([("term".to_string(), "asyncio".to_string())]);
        assert_eq!(
            with_inputs("Search for input term, not input terms or reinput term", &inputs),
            "Search for input term (asyncio), not input terms or reinput term"
        );
        let inputs =
            IndexMap::from([("a".to_string(), "see input b".to_string()), ("b".to_string(), "two".to_string())]);
        assert_eq!(with_inputs("Use input a", &inputs), "Use input a (see input b)");
    }
}

/// Which changes to what the page's fields and settings hold a task allows, as Jev reads the task alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edits {
    Allowed,
    /// The task asks for some changes and rules out the rest, so each edit goes ahead only if the task asks for it.
    RequestedOnly,
    /// The task rules out every change, authorized or not (#838).
    Forbidden,
}

/// Jev's reading of the task at a sign-in wall, each a question of its own, so code decides what the wall means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignInNeeds {
    /// The task needs a signed-in account: its own orders, messages, settings or other private pages.
    pub account: bool,
    /// The task gives the credentials to sign in with, or says to sign in with saved or configured ones: asked only
    /// whether it gives them, a task to sign in with the saved password and report a 2FA prompt read the wall instead.
    pub credentials: bool,
    /// The task asks about the access restriction itself, which the wall shows.
    pub inspects: bool,
}
