//! The loop's stateless helpers: judgements over the notes and a page that hold no state of their own.

use std::collections::BTreeSet;

use fastbrowse_core::decision::NoulQuestion;

use fastbrowse_core::models::{Evidence, Operation, StepOutcome, UNTRUSTED};
use fastbrowse_core::page::{Control, Dialog, Observation};
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::py::text;
use fastbrowse_core::safety::irreversible_question;
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

pub(crate) fn navigation_question(task: &str, url: &str) -> NoulQuestion {
    irreversible_question(task, Operation::Click, &caller_link(url), None)
}

pub(crate) fn decision_label(decision: &Decision) -> Option<String> {
    match &decision.target {
        Some(target) => Some(describe(target)),
        None => decision.url.clone().or_else(|| decision.tab_id.clone()),
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
            Some(Operation::Read | Operation::Select | Operation::Scroll | Operation::ScrollUp) => {}
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
