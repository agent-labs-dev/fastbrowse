//! The loop's module-level helpers in `agent.py`: judgements over the plan, the notes and a page that hold no state of
//! their own.

use std::collections::{BTreeSet, HashSet};

use fastbrowse_core::config::ObservationLimits;
use fastbrowse_core::decision::NoulQuestion;
use fastbrowse_core::effects::{SETTING_ROLES, state_key};
use fastbrowse_core::models::{Evidence, Operation, StepOutcome, UNTRUSTED};
use fastbrowse_core::navigation::navigation_urls;
use fastbrowse_core::page::{Capture, Control, Dialog, Observation, pager_link};
use fastbrowse_core::py::dump::{Dump, include, to_value};
use fastbrowse_core::py::json::{Style, dumps, write};
use fastbrowse_core::py::{sha256_hex, text};
use fastbrowse_core::safety::{irreversible_question, origin_of, starts_download};
use indexmap::IndexMap;
use serde_json::{Value, json};
use url::Url;

use super::state::{Lineage, ObservedLink, RunState, Signature};
use crate::memory::{Notes, fact_id};
use crate::planner::{Plan, Requirement, RequirementKind};
use crate::policy::{Decision, HistoryEntry, Reduction};
use crate::retrieval::{names_subject, subject_words};
use crate::verification::LLMVerdict;

pub(crate) const STALE_LINK_FAILURES: i64 = 2;
pub(crate) const RECOVERY_CHARS: usize = 240;

/// Operations that do not act on the page: a read changes nothing, and DONE is judged again by the finish.
pub(crate) fn not_acting(operation: Operation) -> bool {
    matches!(operation, Operation::Read | Operation::Done)
}

/// What the run does through the page's own controls, so what follows is the site's doing, not the run's guess.
pub(crate) fn site_action(operation: Operation) -> bool {
    operation.is_targeted() || operation == Operation::Dialog
}

/// Recovery can direct page operations without a control; directed DONE still requires verification.
pub(crate) fn page_operation(operation: Operation) -> bool {
    matches!(
        operation,
        Operation::Read
            | Operation::Scroll
            | Operation::ScrollUp
            | Operation::Back
            | Operation::Escape
            | Operation::Done
    )
}

pub(crate) fn parse(url: &str) -> Option<Url> {
    Url::parse(url).ok()
}

/// `urljoin(base, href)`, or None when either cannot be read as an address.
pub(crate) fn join(base: &str, href: &str) -> Option<String> {
    parse(base)?.join(href).ok().map(String::from)
}

pub(crate) fn path_of(url: &str) -> String {
    parse(url).map(|u| u.path().to_string()).unwrap_or_default()
}

pub(crate) fn scheme_of(url: &str) -> String {
    parse(url).map(|u| u.scheme().to_string()).unwrap_or_default()
}

fn netloc(url: &Url) -> String {
    match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        _ => String::new(),
    }
}

pub(crate) fn netloc_of(url: &str) -> String {
    parse(url).map(|u| netloc(&u)).unwrap_or_default()
}

pub(crate) fn unread(plan: &Plan, notes: &Notes) -> bool {
    // A plan can file a question under an action ("find the quote using the search form"), and a run that owes an
    // answer with nothing read would hand the composer empty notes: one did, and ended complete on "".
    let unresolved = notes.unresolved(plan).iter().any(|r| r.kind == RequirementKind::Information);
    unresolved || (plan.page_answer_expected() && notes.facts().is_empty())
}

/// A fact's identity for progress: its source quote, where it was read and what it answers.
pub(crate) type EvidenceKey = (String, Option<String>, String);

pub(crate) fn answer_evidence(notes: &Notes, include_answer: bool) -> BTreeSet<EvidenceKey> {
    let source = |evidence: &Evidence| -> String {
        let page = notes.captured_page(evidence);
        dumps(&json!({
            "url": evidence.url,
            "frame_id": evidence.frame_id,
            "quote": evidence.quote,
            "source_id": evidence.source_id,
            "heading_path": evidence.heading_path,
            "page_title": page.map(|page| page.title.clone()),
        }))
    };
    let known = notes.evidence();
    // A new summary over retained quotes cannot restore the budget for finding missing source evidence.
    if !include_answer {
        let mut keys: BTreeSet<EvidenceKey> = known
            .values()
            .map(|evidence| (evidence.url.clone(), evidence.frame_id.clone(), source(evidence)))
            .collect();
        for fact in notes.facts() {
            if fact.is_collection()
                && fact.comparison.as_ref().is_some_and(|c| c.complete)
                && notes.current(&fact_id(fact))
            {
                keys.insert((String::new(), None, fact.dump_json()));
            }
        }
        return keys;
    }
    notes
        .facts()
        .into_iter()
        .map(|fact| {
            let mut basis: Vec<String> = notes
                .expand_evidence_ids(fact.basis.iter().map(String::as_str))
                .iter()
                .filter_map(|key| known.get(key))
                .map(source)
                .collect();
            basis.sort();
            let value = match &fact.evidence {
                Some(evidence) => source(evidence),
                None => fact.text.clone(),
            };
            (
                fact.evidence.as_ref().map(|e| e.url.clone()).unwrap_or_default(),
                fact.evidence.as_ref().and_then(|e| e.frame_id.clone()),
                dumps(&json!({
                    "value": value,
                    "text": fact.text,
                    "requirements": notes.fact_requirements(&fact_id(fact)),
                    "comparison": fact.comparison.as_ref().map(Dump::dump),
                    "basis": basis,
                })),
            )
        })
        .collect()
}

/// Whether the plan only asks to find things, with nothing to do on the site.
pub(crate) fn lookup(plan: &Plan) -> bool {
    !plan.requirements.is_empty() && plan.requirements.iter().all(|r| r.kind == RequirementKind::Information)
}

/// Whether the plan asks to find something and every such requirement is evidenced.
pub(crate) fn answered(plan: &Plan, notes: &Notes) -> bool {
    let asked: Vec<&Requirement> =
        plan.requirements.iter().filter(|r| r.kind == RequirementKind::Information).collect();
    !asked.is_empty() && asked.iter().all(|r| notes.evidenced(&r.id))
}

/// Whether this page has anything on it for a run to act on or read.
pub(crate) fn drew_something(observation: &Observation) -> bool {
    !observation.controls.is_empty() || !text::strip(&observation.viewport_text).is_empty()
}

pub(crate) fn read_exhausted(state: &RunState) -> String {
    let missing: Vec<String> = state
        .notes
        .unresolved(state.plan())
        .iter()
        .filter(|r| r.kind == RequirementKind::Information)
        .map(|r| format!("{}: {}", r.id, r.text))
        .collect();
    let missing = if missing.is_empty() { state.task.clone() } else { missing.join("; ") };
    format!(
        "Still missing evidence for {missing}. Re-reading this unchanged content will not supply it; find additional content."
    )
}

/// Name which one was chosen, not just what it read: a label alone cannot identify one of six identically labelled
/// buttons, in the step log or in the history the next choice is made from.
pub(crate) fn describe(control: &Control) -> String {
    match &control.context {
        Some(context) if !context.is_empty() => format!("{} ({context})", control.label),
        _ => control.label.clone(),
    }
}

/// An address as origin and path: the query and fragment a site writes back do not make it another page.
pub(crate) fn place(url: &str) -> (String, String) {
    let path = path_of(url);
    (origin_of(url), if path.is_empty() { "/".into() } else { path })
}

/// The requirements with a fact read on an address the run built from the task rather than clicked to.
pub(crate) fn guessed(
    plan: &Plan,
    notes: &Notes,
    invented: &HashSet<String>,
    searches: bool,
    lineage: Option<&Lineage>,
) -> HashSet<String> {
    let judge = guessed_page(invented, lineage, searches);
    plan.requirements
        .iter()
        .filter(|r| notes.supporting_evidence(&r.id).iter().any(&judge))
        .map(|r| r.id.clone())
        .collect()
}

/// Whether a fact read at an address was read on the guess, judged as `guessed` judges it, so what a refusal blocks
/// is exactly what the verifier was shown as guessed.
pub(crate) fn guessed_page(
    invented: &HashSet<String>,
    lineage: Option<&Lineage>,
    searches: bool,
) -> impl Fn(&Evidence) -> bool + Send + Sync + 'static {
    let places: IndexMap<(String, String), bool> =
        invented.iter().map(|url| (place(url), fastbrowse_core::addresses::searched(url))).collect();
    // A clone shares the run's lineage, so a later reading off the guess frees what this judges held.
    let known = lineage.cloned().unwrap_or_default();
    move |evidence: &Evidence| {
        let url = &evidence.url;
        // Lineage only clears a path match, never adds one: a page clicked to off the guess is judged by its path.
        let address = fastbrowse_core::addresses::search_address(url);
        let on_guess = known.reading(&address, &evidence.capture_sha256).or_else(|| known.address(&address));
        let here = place(url);
        on_guess != Some(false)
            && places.contains_key(&here)
            && (!searches || places[&here] || fastbrowse_core::addresses::searched(url))
    }
}

/// Every address a quote cut from this capture can cite: the page's, and each child frame's own.
pub(crate) fn sources(capture: &Capture) -> Vec<String> {
    let mut found: Vec<String> = vec![capture.url.clone()];
    for block in &capture.blocks {
        if let Some(url) = &block.source_url
            && !url.is_empty()
            && !found.contains(url)
        {
            found.push(url.clone());
        }
    }
    found
}

/// The requirements the verifier says were read off the wrong page, where that page was a guessed address.
pub(crate) fn misread(
    verdict: &LLMVerdict,
    plan: &Plan,
    notes: &Notes,
    invented: &HashSet<String>,
    lineage: Option<&Lineage>,
) -> HashSet<String> {
    let named = plan_ids(&verdict.ungrounded, plan);
    let guessed = guessed(plan, notes, invented, false, lineage);
    named.intersection(&guessed).cloned().collect()
}

fn id_key(raw: &str) -> String {
    raw.to_lowercase().chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).collect()
}

/// The plan's requirements the verifier named, however it spelled their ids.
pub(crate) fn plan_ids(ids: &[String], plan: &Plan) -> HashSet<String> {
    let known: IndexMap<String, String> = plan.requirements.iter().map(|r| (id_key(&r.id), r.id.clone())).collect();
    let numbered: IndexMap<String, String> = plan
        .requirements
        .iter()
        .filter_map(|r| {
            let digits: String = r.id.chars().rev().take_while(|c| c.is_ascii_digit()).collect();
            (!digits.is_empty()).then(|| (digits.chars().rev().collect(), r.id.clone()))
        })
        .collect();
    let mut found = HashSet::new();
    for raw in ids {
        let key = id_key(raw);
        let matched = known.get(&key).cloned().or_else(|| {
            (!key.is_empty() && key.chars().all(|c| c.is_ascii_digit())).then(|| numbered.get(&key).cloned()).flatten()
        });
        if let Some(matched) = matched {
            found.insert(matched);
        }
    }
    found
}

/// Whether an action the run chose was covered or failed: it may still be on the page it meant to leave.
pub(crate) fn blocked_action(history: &[HistoryEntry]) -> bool {
    history.iter().any(|entry| {
        matches!(entry.outcome, StepOutcome::Covered | StepOutcome::Failed)
            && entry.operation.is_some_and(|op| !matches!(op, Operation::Done | Operation::Read))
    })
}

/// Whether the verifier's doubts leave the run finished.
///
/// An information requirement the notes cite facts for is not left open by the verifier alone: the answer's claims are
/// checked against those quotes before it is given. On flash-lite the verifier named "compare the two release dates"
/// missing with both dates in the notes, one run in three, until the run stopped stuck. Evidence read on a guessed
/// address is not excused, since that is the wrong page the citing check cannot see. Nor is a requirement Jev's done
/// check also held unmet: the claim check sees a quote and its claim, not which page the quote came from, and a run
/// whose link was covered by an ad read the heading of the page it was still on and called it the new window's.
pub(crate) fn verified(
    verdict: &LLMVerdict,
    plan: &Plan,
    notes: &Notes,
    invented: &HashSet<String>,
    lineage: Option<&Lineage>,
    held_unmet: &[String],
) -> bool {
    let cited: HashSet<String> = plan
        .requirements
        .iter()
        .filter(|r| r.kind == RequirementKind::Information && notes.evidenced(&r.id))
        .map(|r| r.id.clone())
        .collect();
    let misread = misread(verdict, plan, notes, invented, lineage);
    let named: HashSet<String> =
        plan_ids(&verdict.missing, plan).union(&plan_ids(&verdict.ungrounded, plan)).cloned().collect();
    let excused: HashSet<String> =
        cited.iter().filter(|id| !misread.contains(*id) && !held_unmet.contains(*id)).cloned().collect();
    let doubted: Vec<&String> = named.iter().filter(|id| !excused.contains(*id)).collect();
    doubted.is_empty() && (verdict.complete || !named.is_empty())
}

/// The recent actions in full, after the earlier ones without the effects that make an entry long.
pub(crate) fn history(entries: &[HistoryEntry], limits: &ObservationLimits) -> Vec<HistoryEntry> {
    let split = entries.len() as i64 - limits.history_entries;
    let from = (split - limits.earlier_history_entries).max(0) as usize;
    let to = split.max(0) as usize;
    let mut shown: Vec<HistoryEntry> =
        entries[from.min(to)..to].iter().map(|entry| HistoryEntry { effect: None, ..entry.clone() }).collect();
    shown.extend(entries[to..].iter().cloned());
    shown
}

/// The kind of value the field declares it takes, when the text cannot be one, else None.
pub(crate) fn misfit(target: &Control, text: &str) -> Option<&'static str> {
    let autocomplete = target.autocomplete.as_deref().unwrap_or_default().to_lowercase();
    let email = target.input_type.as_deref().unwrap_or_default().to_lowercase() == "email"
        || autocomplete.split_whitespace().any(|token| token == "email");
    (email && !text.contains('@')).then_some("an email address")
}

/// The next of a field's values in turn after those its fills have typed so far, in order, else the last.
pub(crate) fn next_value(values: &[String], entries: &[HistoryEntry], label: Option<&str>) -> String {
    let mut done = 0;
    for entry in entries {
        if entry.operation == Some(Operation::Fill)
            && entry.outcome == StepOutcome::Executed
            && entry.target.as_deref() == label
            && done < values.len()
            && entry.text.as_deref() == Some(values[done].as_str())
        {
            done += 1;
        }
    }
    values[done.min(values.len() - 1)].clone()
}

/// A page state with its visible text, which a state key leaves out: a review step keeps its controls when the
/// values it shows change.
pub(crate) fn seen(observation: &Observation) -> String {
    format!("{}:{}", state_key(observation), sha256_hex(observation.viewport_text.as_bytes()))
}

/// Whether every action since the last read typed into a field and changed nothing else on the page.
pub(crate) fn only_typed_since_read(entries: &[HistoryEntry]) -> bool {
    for entry in entries.iter().rev() {
        if entry.operation == Some(Operation::Read) {
            return entry.outcome == StepOutcome::Executed;
        }
        if entry.operation != Some(Operation::Fill) || entry.page_changed {
            return false;
        }
    }
    false
}

/// Whether a page state is a redraw of an address already read, reached with no read paying out since the last
/// recovery.
pub(crate) fn redrawn_read(state: &mut RunState, observation: &Observation) -> bool {
    let form: HashSet<String> = observation
        .controls
        .iter()
        .filter(|c| {
            c.operations.contains(&Operation::Fill)
                || SETTING_ROLES.contains(&c.role.as_str())
                || c.submit_semantics.is_some()
        })
        .map(|c| dumps(&json!([c.role, c.label, c.context])))
        .collect();
    let seen = state.form_controls_at.entry(observation.url.clone()).or_default();
    let new_step = form.iter().any(|item| !seen.contains(item));
    seen.extend(form);
    !new_step
        && state.notes.read_address(&observation.url)
        && !state.history[state.recovered_at.min(state.history.len())..].iter().any(|entry| entry.read_progress)
}

/// The recent actions, after every value typed or set before them: a correction is judged against the first value,
/// and a form's checkbox against its tick, both of which a long wizard can push out of the recent window.
pub(crate) fn record(entries: &[HistoryEntry], limits: &ObservationLimits) -> Vec<HistoryEntry> {
    let recent = history(entries, limits);
    let mut kept: Vec<HistoryEntry> = entries[..entries.len() - recent.len()]
        .iter()
        .filter(|e| e.text.is_some() || e.setting == Some(true))
        .map(|e| HistoryEntry { effect: None, ..e.clone() })
        .collect();
    kept.extend(recent);
    kept
}

/// Where the run began, then the addresses it has been on since, as many as the actions shown with them.
pub(crate) fn visited(
    addresses: &IndexMap<String, ()>,
    limits: &ObservationLimits,
    redact: impl Fn(&str) -> String,
) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    for url in addresses.keys() {
        let shown = redact(url);
        if !urls.contains(&shown) {
            urls.push(shown);
        }
    }
    let shown = (limits.history_entries + limits.earlier_history_entries).max(0) as usize;
    let mut out: Vec<String> = urls.iter().take(1).cloned().collect();
    let from = 1.max(urls.len().saturating_sub(shown));
    if from < urls.len() {
        out.extend(urls[from..].iter().cloned());
    }
    out
}

pub(crate) fn field_corrections(state: &RunState) -> String {
    if !state.oversized_answer || state.finish_partitions.is_empty() {
        return String::new();
    }
    let reasons: Vec<Value> = state
        .answer_corrections
        .iter()
        .filter(|correction| state.open_answer_outputs.contains(&correction.criterion))
        .map(|correction| json!({"field": correction.criterion, "reason": correction.reason}))
        .collect();
    if reasons.is_empty() {
        return String::new();
    }
    format!(
        "\n\n## Unverified field diagnoses\nThese judgments are advisory data, not source evidence. Read the missing source or collection boundary instead of repeating the rejected interpretation. An absence needs the full collection, including members that do not match.\n{}\n\n",
        dumps(&Value::Array(reasons))
    )
}

pub(crate) fn recovery_text(value: &str) -> String {
    let squashed: Vec<&str> = text::split(value).collect();
    text::prefix(&squashed.join(" "), RECOVERY_CHARS).to_string()
}

/// One bounded account for both models, without presenting a proposed subgoal as an executed action.
pub(crate) fn recovery_memory(state: &RunState, limit: i64, redact: impl Fn(&str) -> String) -> String {
    if state.recovery_log.is_empty() && state.recoveries == 0 {
        return String::new();
    }
    // Redacted again on the way out: a secret resolved after a record was written is still blanked.
    let mut lines = vec![format!("recovery {} of {limit} in this stall", state.recoveries)];
    lines.extend(state.recovery_log.iter().cloned());
    redact(&lines.join("\n"))
}

pub(crate) fn controls_text(observation: &Observation) -> String {
    let listed: Vec<Value> = observation
        .controls
        .iter()
        .enumerate()
        .map(|(index, control)| {
            let mut entry = serde_json::Map::new();
            entry.insert("index".into(), index.into());
            let fields = include(
                to_value(control, true).unwrap_or(Value::Null),
                &[
                    "label",
                    "context",
                    "role",
                    "value",
                    "operations",
                    "selected",
                    "expanded",
                    "blocking",
                    "input_type",
                    "input_name",
                    "autocomplete",
                ],
            );
            if let Value::Object(fields) = fields {
                entry.extend(fields);
            }
            Value::Object(entry)
        })
        .collect();
    dumps(&Value::Array(listed))
}

/// A caller-supplied address as the link it stands for, so it meets the same gates a link on a page does.
pub(crate) fn caller_link(url: &str) -> Control {
    let mut control = Control::new("caller-address", "link", url, [Operation::Click]);
    control.href = Some(url.to_string());
    control
}

pub(crate) fn navigation_question(task: &str, url: &str) -> NoulQuestion {
    irreversible_question(task, Operation::Click, &caller_link(url), None)
}

pub(crate) fn signature(decision: &Decision, observation: &Observation) -> Signature {
    (decision.operation, decision_label(decision), decision.destination.as_ref().map(describe), state_key(observation))
}

pub(crate) fn decision_label(decision: &Decision) -> Option<String> {
    match &decision.target {
        Some(target) => Some(describe(target)),
        None => decision.url.clone().or_else(|| decision.tab_id.clone()),
    }
}

/// The reader's question. `began_at` is the run's first page, given once a list may run past one page.
pub fn read_question(task: &str, wanted: &[Requirement], began_at: Option<&str>) -> String {
    let lines: Vec<String> = wanted.iter().map(|r| format!("- {}", r.text)).collect();
    // Unresolved requirements may refer to an earlier one; keep the task's constraints in every read.
    let question = format!("{task}\n\nRequirements still to evidence:\n{}", lines.join("\n"));
    match began_at {
        None => question,
        Some(began) => format!(
            "{question}\n\nThe task's \"this page\" is {began}, where the run began, and \"the next page\" is the one after it. A task that names how many pages it covers ends at the last one it names."
        ),
    }
}

pub fn next_page_notice(following: Option<&Control>) -> String {
    // The capture is text: a "Next" link reads the same as any other word unless the page's controls say so.
    match following {
        None => String::new(),
        Some(control) => format!(
            "This page has a next-page control ({}): a list on it may continue. A requirement about the page that control opens is not answered by this page, whatever this page holds: leave it unanswered until that page is read.",
            text::repr(&control.label)
        ),
    }
}

pub(crate) type LinkKey = (String, String);

pub(crate) fn link_key(observation: &Observation, control: &Control) -> Option<LinkKey> {
    // Use the observed link, including cross-origin display hrefs, so redirects retain their source attempt.
    if control.role != "link" {
        return None;
    }
    let href = control.href.as_ref()?;
    Some((state_key(observation), dumps(&json!([href, control.label, control.context]))))
}

fn without_fragment(url: &Url) -> Url {
    let mut url = url.clone();
    url.set_fragment(None);
    url
}

pub(crate) fn stale_link_key(observation: &Observation, control: &Control) -> Option<LinkKey> {
    if observation.document_key.is_empty() || control.role != "link" {
        return None;
    }
    let href = control.href.as_ref()?;
    let mut identity = vec![json!(observation.url), json!(control.frame_id), json!(control.frame_origin), json!(href)];
    let navigation = match (parse(&observation.url), parse(&observation.url).and_then(|base| base.join(href).ok())) {
        (Some(here), Some(target)) => {
            control.frame_id.is_none()
                && !href.is_empty()
                && matches!(target.scheme(), "http" | "https")
                && without_fragment(&target) != without_fragment(&here)
        }
        _ => false,
    };
    // Related-product cards can change their captions while the same destination keeps failing freshness.
    // Same-page links and frames without a known document address still need their separate action identity.
    if !navigation {
        identity.push(json!(control.label));
        identity.push(json!(control.context));
    }
    Some((observation.document_key.clone(), dumps(&Value::Array(identity))))
}

/// The page without the links the run cannot use: failed, repeatedly stale, or downloads the task rules out.
pub(crate) fn without_failed_links(state: &RunState, observation: &Observation) -> Observation {
    if state.failed_links.is_empty() && state.stale_links.is_empty() && state.downloads_forbidden != Some(true) {
        return observation.clone();
    }
    let forbidden = state.downloads_forbidden == Some(true);
    Observation {
        controls: observation
            .controls
            .iter()
            .filter(|control| {
                let failed = link_key(observation, control).is_some_and(|key| state.failed_links.contains_key(&key));
                !(failed || (forbidden && starts_download(control)))
                    && stale_link_key(observation, control)
                        .is_none_or(|key| state.stale_links.get(&key).copied().unwrap_or(0) < STALE_LINK_FAILURES)
            })
            .cloned()
            .collect(),
        ..observation.clone()
    }
}

/// Where a pager link goes: its origin, path and query.
type PagerTarget = (String, String, Vec<(String, String)>);

fn query_pairs(url: &Url) -> Vec<(String, String)> {
    url.query().map_or_else(Vec::new, |query| {
        url::form_urlencoded::parse(query.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
    })
}

fn pager_query(pairs: &[(String, String)]) -> Vec<(String, String)> {
    // A server can use the first repeated value, so only unique keys may be reordered.
    let keys: HashSet<&String> = pairs.iter().map(|(key, _)| key).collect();
    if keys.len() == pairs.len() {
        let mut sorted = pairs.to_vec();
        sorted.sort();
        sorted
    } else {
        pairs.to_vec()
    }
}

fn digits(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_ascii_digit())
}

fn numbered_next_page(here: &Url, there: &Url, label: &str) -> bool {
    let label = text::strip(label);
    if here.path() != there.path() || !digits(label) {
        return false;
    }
    let (before, after) = (query_pairs(here), query_pairs(there));
    for parameter in ["page", "pageno", "page_number"] {
        // Canonical first-page URLs omit the page parameter; an observed 2 is the only consecutive target.
        let mut old: Vec<&String> = before.iter().filter(|(k, _)| k == parameter).map(|(_, v)| v).collect();
        let one = "1".to_string();
        if old.is_empty() {
            old.push(&one);
        }
        let new: Vec<&String> = after.iter().filter(|(k, _)| k == parameter).map(|(_, v)| v).collect();
        if old.len() != 1 || new.len() != 1 || !digits(old[0]) || !digits(new[0]) {
            continue;
        }
        let (Ok(previous), Ok(following), Ok(displayed)) =
            (old[0].parse::<u64>(), new[0].parse::<u64>(), label.parse::<u64>())
        else {
            continue;
        };
        let rest = |pairs: &[(String, String)]| {
            pager_query(&pairs.iter().filter(|(k, _)| k != parameter).cloned().collect::<Vec<_>>())
        };
        if following == previous + 1 && displayed == following && rest(&before) == rest(&after) {
            return true;
        }
    }
    false
}

/// The one control that opens the next page of a list on this page, or None when there is none or doubt.
pub fn next_page_control(observation: &Observation) -> Option<Control> {
    let here = parse(&observation.url)?;
    let current = pager_query(&query_pairs(&here));
    let mut found: IndexMap<PagerTarget, Control> = IndexMap::new();
    for control in &observation.controls {
        let Some(href) = &control.href else { continue };
        if control.role != "link" || !control.operations.contains(&Operation::Click) {
            continue;
        }
        // Paging clicks without the step's gates, so a link that saves a file is never taken for the next page.
        if starts_download(control) {
            continue;
        }
        // The snapshot gives a same-origin link as a path; a bare host/path denotes another site.
        if !(href.starts_with('/') || href.starts_with("http://") || href.starts_with("https://")) {
            continue;
        }
        let Ok(target) = here.join(href) else { continue };
        if origin_of(target.as_str()) != origin_of(&observation.url) {
            continue;
        }
        // A numbered pager may omit Next; only an observed consecutive page with the same filters qualifies.
        if !pager_link(control) && !numbered_next_page(&here, &target, &control.label) {
            continue;
        }
        let query = pager_query(&query_pairs(&target));
        if (target.path(), &query) == (here.path(), &current) {
            continue;
        }
        found.entry((origin_of(target.as_str()), target.path().to_string(), query)).or_insert_with(|| control.clone());
    }
    if found.len() == 1 { found.into_values().next() } else { None }
}

/// The step code takes to walk a list the reader needs whole: open its next page, then read what that opened.
pub(crate) fn paging(state: &mut RunState, observation: &Observation) -> Option<Decision> {
    if state.next_page {
        state.next_page = false;
        let target = next_page_control(observation)?;
        state.pages += 1;
        state.paged_from = Some(state_key(observation));
        return Some(code_decision(Operation::Click, Some(target)));
    }
    if let Some(left) = state.paged_from.take() {
        // A click that did not open anything new leaves nothing to read; Jev decides from here.
        if state_key(observation) != left && !state.read_here {
            return Some(code_decision(Operation::Read, None));
        }
    }
    None
}

pub(crate) fn dialog_question(task: &str, dialog: Option<&Dialog>) -> Option<NoulQuestion> {
    let dialog = dialog?;
    if !["confirm", "prompt", "beforeunload"].contains(&dialog.kind.as_str()) {
        return None;
    }
    Some(NoulQuestion::new(
        format!(
            "{UNTRUSTED}\nTask: {task}\nThe agent is about to accept this {} dialog: {}. Would accepting commit an irreversible or externally visible change, such as deleting data, sending a message or spending money?",
            dialog.kind,
            text::repr(&dialog.message)
        ),
        Some("Acceptance commits a destructive or externally visible change."),
        Some("Acceptance only navigates, reveals information or edits a reversible draft."),
    ))
}

pub(crate) fn access_inspection(plan: &Plan) -> bool {
    plan.inspect_access
        && plan.answer_expected
        && !plan.requirements.is_empty()
        && plan.requirements.iter().all(|r| r.kind == RequirementKind::Information)
}

pub(crate) fn unread_transaction(state: &RunState) -> bool {
    // Possible submissions also include searches and filters. Only a requested state change needs a receipt read.
    state.ready_plan.as_ref().is_some_and(|plan| plan.requirements.iter().any(|r| r.kind == RequirementKind::Action))
        && state.transaction_candidates.iter().any(|c| !c.outcome_read && c.committing != Some(false))
}

pub(crate) fn code_decision(operation: Operation, target: Option<Control>) -> Decision {
    Decision {
        operation,
        url: None,
        target_confidence: target.as_ref().map(|_| 1.0),
        target,
        tab_id: None,
        operation_confidence: 1.0,
        login_required: None,
        offered_controls: 0,
        reduction: Reduction::None,
        cost: Vec::new(),
        input_tokens: 0,
        bot_check: None,
        read_assessment: crate::policy::ReadAssessment::Absent,
        directed: false,
        destination: None,
    }
}

/// Try uncertain observation moves once; state-changing choices need recovery's second opinion.
pub(crate) fn try_unsure(state: &mut RunState, observation: &Observation, decision: &Decision) -> bool {
    if !(decision.operation.is_scrolling() || decision.operation == Operation::Hover) {
        return false;
    }
    let key = state_key(observation);
    if state.tried_unsure.contains(&key) || state.attempts.contains_key(&signature(decision, observation)) {
        return false;
    }
    state.tried_unsure.insert(key);
    true
}

/// The page as Jev is offered it: without the fields the task was already found to give no value for.
pub(crate) fn without_missing(observation: Observation, missing: &HashSet<(String, Option<String>)>) -> Observation {
    if missing.is_empty() {
        return observation;
    }
    let kept: Vec<Control> = observation
        .controls
        .iter()
        .filter(|c| {
            !c.operations.contains(&Operation::Fill) || !missing.contains(&(c.label.clone(), c.context.clone()))
        })
        .cloned()
        .collect();
    if kept.len() == observation.controls.len() { observation } else { Observation { controls: kept, ..observation } }
}

fn rstrip_slash(path: &str) -> &str {
    path.trim_end_matches('/')
}

fn rpartition_slash(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(at) => (&path[..at], &path[at + 1..]),
        None => ("", path),
    }
}

pub(crate) fn unread_output_section(state: &RunState) -> Option<String> {
    let mut read: HashSet<String> = state.read_urls.iter().cloned().collect();
    read.extend(state.recovery_sections.iter().cloned());
    read.extend(state.notes.current_evidence(None).values().map(|e| e.url.clone()));
    let failed: HashSet<&String> = state.failed_links.values().map(|failure| &failure.url).collect();
    for output in &state.open_answer_outputs {
        let Some(target) = state.output_targets.get(output) else { continue };
        let (field, subject) = (&target.field, &target.subject);
        let words = subject_words(subject);
        if words.is_empty() {
            continue;
        }
        let mut roots: HashSet<(String, String)> = HashSet::new();
        for link in state.observed_links.values() {
            let (Some(source), Some(destination)) = (parse(&link.source), parse(&link.url)) else { continue };
            if names_subject(&link.label, subject) {
                roots.insert((netloc(&destination), rstrip_slash(destination.path()).to_string()));
            }
            if names_subject(&link.title, subject) {
                for address in [&source, &destination] {
                    let path = rstrip_slash(address.path());
                    let tail = subject_words(rpartition_slash(path).1);
                    if !tail.is_empty() && tail.iter().all(|word| words.contains(word)) {
                        roots.insert((netloc(address), path.to_string()));
                    }
                }
            }
            if names_subject(rpartition_slash(destination.path()).1, subject) {
                roots.insert((netloc(&destination), rstrip_slash(destination.path()).to_string()));
            }
        }
        let mut candidates: Vec<(i64, usize, String)> = Vec::new();
        for link in state.observed_links.values() {
            if read.contains(&link.url) || failed.contains(&link.url) {
                continue;
            }
            let Some(destination) = parse(&link.url) else { continue };
            let path = rstrip_slash(destination.path());
            let host = netloc(&destination);
            // A target's directory and another target beside it are not sections of that target.
            if roots.contains(&(host.clone(), path.to_string()))
                || roots.contains(&(host, rpartition_slash(path).0.to_string()))
            {
                let label: HashSet<String> = subject_words(&link.label).into_iter().collect();
                let overlap = subject_words(field).into_iter().collect::<HashSet<_>>().intersection(&label).count();
                candidates.push((-(overlap as i64), text::len(path), link.url.clone()));
            }
        }
        if let Some(best) = candidates.into_iter().min() {
            return Some(best.2);
        }
    }
    None
}

fn words(value: &str) -> HashSet<String> {
    let folded = text::casefold(value);
    let mut found = HashSet::new();
    let mut word = String::new();
    for c in folded.chars() {
        if text::is_word(c) {
            word.push(c);
        } else if !word.is_empty() {
            found.insert(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        found.insert(word);
    }
    found
}

pub(crate) fn recovery_routes(state: &RunState, max_chars: i64) -> String {
    if state.open_answer_outputs.is_empty() {
        return "[]".into();
    }
    let wanted = words(&state.open_answer_outputs.join(" "));
    let mut read: HashSet<String> = state.read_urls.iter().cloned().collect();
    read.extend(state.notes.current_evidence(None).values().map(|e| e.url.clone()));
    let failed: HashSet<&String> = state.failed_links.values().map(|failure| &failure.url).collect();
    let mut links: Vec<&ObservedLink> =
        state.observed_links.values().filter(|link| !failed.contains(&link.url)).collect();
    let score = |link: &ObservedLink| -> (i64, bool) {
        let in_url = wanted.intersection(&words(&link.url)).count() as i64;
        let around = wanted.intersection(&words(&format!("{} {} {}", link.source, link.title, link.label))).count();
        (-(2 * in_url + around as i64), read.contains(&link.url))
    };
    // A stable sort, as Python's `sorted` is.
    links.sort_by_key(|link| score(link));
    let mut offered = Vec::new();
    let mut used = 2;
    for link in links {
        let mut entry = match link.dump() {
            Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        entry.insert("read".into(), read.contains(&link.url).into());
        let entry = Value::Object(entry);
        let size = text::len(&dumps(&entry)) as i64 + 2;
        if used + size <= max_chars {
            offered.push(entry);
            used += size;
        }
    }
    dumps(&Value::Array(offered))
}

/// The action recovery named, when Jev cannot act or repeats a failed read and the control still offers it.
pub(crate) fn follow_recovery(
    state: &mut RunState,
    observation: &Observation,
    decision: &Decision,
    uncertain: bool,
) -> Option<Decision> {
    let directed = state.directed.take()?;
    if state.open_answer_outputs.is_empty() && !uncertain && decision.operation != Operation::Escalate {
        return None;
    }
    let (operation, control_id) = directed;
    if operation == Operation::Navigate {
        let control_id = control_id?;
        if observation.dialog.is_some()
            || (!state.observed_links.contains_key(&control_id)
                && !navigation_urls(
                    &state.task,
                    &observation.url,
                    state.caller_start.as_deref(),
                    state.start_landing_url.as_deref(),
                    true,
                )
                .contains(&control_id))
        {
            return None;
        }
        return Some(Decision { operation, target: None, url: Some(control_id), directed: true, ..decision.clone() });
    }
    // Recovery names one control and a drag needs two, so the drop target has to be one Jev chose for a drag.
    if operation == Operation::Drag && decision.operation != Operation::Drag {
        return None;
    }
    let Some(control_id) = control_id else {
        return Some(Decision { operation, target: None, directed: true, ..decision.clone() });
    };
    let target = observation.controls.iter().find(|c| c.id == control_id && c.operations.contains(&operation))?;
    Some(Decision { operation, target: Some(target.clone()), directed: true, ..decision.clone() })
}

pub(crate) fn partition_proof(plan: &Plan, notes: &Notes, invented: &HashSet<String>) -> String {
    let places: HashSet<(String, String)> = notes.evidence().values().map(|source| place(&source.url)).collect();
    let facts: Vec<Value> = notes
        .facts()
        .into_iter()
        .filter_map(|fact| {
            let evidence = fact.evidence.as_ref()?;
            Some(json!({
                "evidence": evidence.dump(),
                "current": notes.current(&fact_id(fact)),
                "page": notes.captured_page(evidence).map(Dump::dump),
            }))
        })
        .collect();
    let support: serde_json::Map<String, Value> = plan
        .requirements
        .iter()
        .map(|requirement| {
            let mut ids: Vec<String> =
                notes.supporting_evidence(&requirement.id).iter().map(crate::memory::evidence_id).collect();
            ids.sort();
            (requirement.id.clone(), json!(ids))
        })
        .collect();
    let mut inactive: Vec<String> = notes.facts().into_iter().map(fact_id).filter(|key| !notes.current(key)).collect();
    inactive.sort();
    let collections: BTreeSet<String> = notes
        .facts()
        .into_iter()
        .filter(|fact| fact.tally.is_some() || fact.comparison.is_some())
        .map(|fact| {
            let mut entry = serde_json::Map::new();
            entry.insert("tally".into(), fact.tally.as_ref().map_or(Value::Null, Dump::dump));
            entry.insert("comparison".into(), fact.comparison.as_ref().map_or(Value::Null, Dump::dump));
            if fact.is_collection() {
                entry.insert("scope".into(), json!(fact.scope));
            }
            entry.insert("requirements".into(), json!(notes.fact_requirements(&fact_id(fact))));
            write(&Value::Object(entry), Style::DUMPS.sorted())
        })
        .collect();
    let mut guessed: Vec<&String> = invented.iter().filter(|url| places.contains(&place(url))).collect();
    guessed.sort();
    let proof = json!({
        "plan": plan.dump(),
        "facts": facts,
        "support": support,
        "inactive": inactive,
        "collections": collections.into_iter().collect::<Vec<_>>(),
        "invented": guessed,
        "refused": notes.refused_ids(),
    });
    sha256_hex(write(&proof, Style::DUMPS.sorted()).as_bytes())
}

/// `ANSWER_LINK.split(text)`: the prose between an answer's links, with each link's destination between them.
pub(crate) fn split_answer_links(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while let Some((start, from, to)) = find_answer_link(rest) {
        pieces.push(&rest[..start]);
        pieces.push(&rest[from..to]);
        rest = &rest[to + 2..];
    }
    pieces.push(rest);
    pieces
}

/// The next `](<destination>)`: where it starts, and where its destination starts and ends.
fn find_answer_link(text: &str) -> Option<(usize, usize, usize)> {
    let mut from = 0;
    while let Some(found) = text[from..].find("](<") {
        let start = from + found;
        let destination = start + 3;
        let end = destination + text[destination..].find('>')?;
        if text[end..].starts_with(">)") {
            return Some((start, destination, end));
        }
        from = start + 1;
    }
    None
}
