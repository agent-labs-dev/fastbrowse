//! The loop's stateless helpers: judgements over the notes and a page that hold no state of their own.

use std::collections::BTreeSet;

use fastbrowse_core::decision::NoulQuestion;

use fastbrowse_core::models::{Evidence, Operation, StepOutcome, UNTRUSTED};
use fastbrowse_core::page::{Control, Dialog, Observation};
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::py::text;
use fastbrowse_core::safety::{irreversible_question, mask_userinfo};
use serde_json::json;
use url::Url;

use crate::memory::{Notes, unique};
use crate::policy::{Decision, HistoryEntry};

pub(crate) fn parse(url: &str) -> Option<Url> {
    Url::parse(url).ok()
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

/// The sources the notes quote. A new summary over retained quotes is not a new source, so it cannot restore the budget
/// for finding one.
pub(crate) fn read_sources(notes: &Notes) -> BTreeSet<(String, Option<String>, String)> {
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
    notes
        .evidence()
        .values()
        .map(|evidence| (evidence.url.clone(), evidence.frame_id.clone(), source(evidence)))
        .collect()
}

/// Whether this page has anything on it for a run to act on or read.
pub(crate) fn drew_something(observation: &Observation) -> bool {
    !observation.controls.is_empty() || !text::strip(&observation.viewport_text).is_empty()
}

/// Name which one was chosen, not just what it read: a label alone cannot identify one of six identically labelled
/// buttons, in the step log or in the history the next choice is made from.
pub(crate) fn describe(control: &Control) -> String {
    match &control.context {
        Some(context) if !context.is_empty() => format!("{} ({context})", control.label),
        _ => control.label.clone(),
    }
}

/// A caller-supplied address as the link it stands for, so it meets the same gates a link on a page does.
pub(crate) fn caller_link(url: &str) -> Control {
    let mut control = Control::new("caller-address", "link", url, [Operation::Click]);
    control.href = Some(url.to_string());
    control
}

pub(crate) fn navigation_question(url: &str) -> NoulQuestion {
    irreversible_question(Operation::Click, &caller_link(url), None, &[])
}

pub(crate) fn decision_label(decision: &Decision) -> Option<String> {
    match &decision.target {
        Some(target) => Some(describe(target)),
        // A navigation's address is the step's label in the history every model is shown, and one the caller's
        // credentialed start resolved a path on carries its userinfo (#1381).
        None => decision.url.as_deref().map(mask_userinfo).or_else(|| decision.tab_id.clone()),
    }
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

/// The fact, when there is one, that fields on this page hold a value the run typed with no later action that could
/// have sent it.
///
/// A typed value sits in the page until something sends it, which neither the field nor a read of it shows: a read of
/// a filled form quoted the value beside the form's unpressed button, and Jev took that for the task done.
pub(crate) fn unsent_fact(history: &[HistoryEntry], observation: &Observation) -> Option<String> {
    let mut fields = Vec::new();
    for entry in history.iter().rev().filter(|entry| entry.outcome == StepOutcome::Executed) {
        match entry.operation {
            Some(Operation::Fill) => fields.extend(entry.target.iter().cloned()),
            Some(Operation::Read | Operation::Select | Operation::Scroll | Operation::ScrollUp | Operation::Wait) => {}
            Some(Operation::Click) if entry.setting == Some(true) => {}
            _ => break,
        }
    }
    let shown = |field: &String| {
        observation.controls.iter().any(|c| describe(c) == *field && c.value.as_deref().is_some_and(|v| !v.is_empty()))
    };
    let fields = unique(fields.into_iter().filter(shown).rev());
    (!fields.is_empty()).then(|| {
        format!(
            "No action has sent what was typed into {}: the page holds the typed value, and it reaches the site only once the form's own control sends it.",
            fields.join(", ")
        )
    })
}

/// A commit's step as its result names it: the operation, then the quoted target ("click 'Post'").
pub(crate) fn commit_step(entry: &HistoryEntry) -> String {
    let operation = entry.operation.map_or_else(String::new, |operation| operation.to_string());
    let target = entry.target.as_deref().map(text::repr).unwrap_or_default();
    [operation, target].join(" ").trim().to_string()
}

/// One fact per commit the run has sent, with the text the page then showed, as the step's own record has it.
///
/// A site can serve the copy from before a send for a while: a reply posted on a signed-in page was missing after a
/// reload, the carried-out question scored its step 0.22, and the run ended stuck over a reply that was live. Replayed
/// on the real request with its screenshot, five times each, the question scored 0.29 to 0.48 as sent, 0.32 to 0.54
/// told only that the click was sent, and 0.48 to 0.63 told also that the page then showed "Your reply was posted."
/// The site's own answer to the send moves it; a claim that the site keeps what it sent did not, and code does not
/// know that.
pub(crate) fn sent_facts(history: &[HistoryEntry]) -> Vec<String> {
    history
        .iter()
        .filter(|entry| entry.commit == Some(crate::policy::Commit::Taken))
        .map(|entry| {
            let step = commit_step(entry);
            match &entry.put {
                Some((_, put)) if !put.is_empty() => {
                    let shown: Vec<String> = put.iter().map(|piece| text::repr(piece)).collect();
                    format!("The run's {step} was sent, and the page then showed the text {}.", shown.join(", "))
                }
                _ => format!("The run's {step} was sent."),
            }
        })
        .collect()
}

/// The fact, when there is one, naming text on this page that an earlier step of the run put there.
///
/// A notice stays on the page after the step that raised it: two threads to reply in, the first reply's "Your reply
/// was posted." still showed beside the second thread's open, unsent reply box, and Jev judged both replies posted and
/// let the run finish with one. Told which step put the text there, in these words, it judged the second reply still to
/// do; told it as a list of steps and their text, or with "after the run's step", it did not. The same names the run's
/// own posts, so a check whether a thread already holds a reply can tell them from the thread's.
pub(crate) fn earlier_text_fact(history: &[HistoryEntry], observation: &Observation) -> Option<String> {
    let (_, earlier) = history.split_last()?;
    // Each line the page shows is one step's: a notice an earlier post raised, gone and raised again by a later post,
    // is the later post's, so the latest step to have put a line takes one of its occurrences first.
    let mut shown: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for line in [&observation.viewport_text, &observation.offscreen_text].into_iter().flat_map(|text| text.lines()) {
        *shown.entry(text::strip(line)).or_default() += 1;
    }
    let mut owned: Vec<Vec<&String>> = vec![Vec::new(); earlier.len()];
    for (at, entry) in earlier.iter().enumerate().rev().filter(|(_, entry)| entry.outcome == StepOutcome::Executed) {
        let Some((_, put)) = entry.put.as_ref().filter(|(url, _)| *url == observation.url) else { continue };
        for piece in put.iter().rev() {
            if let Some(left) = shown.get_mut(piece.as_str()).filter(|left| **left > 0) {
                *left -= 1;
                owned[at].push(piece);
            }
        }
        owned[at].reverse();
    }
    let told: Vec<String> = earlier
        .iter()
        .zip(owned)
        .filter(|(_, pieces)| !pieces.is_empty())
        .map(|(entry, pieces)| {
            let target = entry.target.as_deref().map(text::repr).unwrap_or_default();
            let did = match entry.operation {
                Some(Operation::Click) => format!("clicked {target}"),
                Some(Operation::Enter) => format!("pressed Enter in {target}"),
                Some(Operation::Press) => format!("pressed a key in {target}"),
                Some(Operation::Drag) => format!("dragged {target}"),
                Some(Operation::Dialog) => "answered the dialog".to_string(),
                Some(operation) => format!("took the step {operation} {target}"),
                None => "opened the page".to_string(),
            };
            let shown: Vec<String> = pieces.iter().map(|piece| format!("{piece:?}")).collect();
            format!(
                "The text {} appeared when the run {did}; it tells of that step, not of any later one.",
                shown.join(", ")
            )
        })
        .collect();
    (!told.is_empty()).then(|| told.join(" "))
}

/// The fields still empty here on a task to carry out: Jev confirmed a form with its one field empty when nothing
/// said so.
pub(crate) fn empty_fields_fact(observation: &Observation) -> Option<String> {
    let empty: Vec<String> = observation
        .controls
        .iter()
        .filter(|control| control.operations.contains(&Operation::Fill))
        .filter(|control| control.value.as_deref().is_none_or(str::is_empty))
        .map(|control| text::repr(&control.label))
        .collect();
    (!empty.is_empty()).then(|| {
        format!(
            "No value has been entered on this page: {} are empty. A value the task states goes into its field before the form is confirmed.",
            empty.join(", ")
        )
    })
}

/// The prose between an answer's links, with each link's destination between them.
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
