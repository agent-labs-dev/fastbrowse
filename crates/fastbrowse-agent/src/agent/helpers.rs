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

use super::state::{Kept, Typed};
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

/// A step carried out, as what showed a control: "clicking Depart (Flights)".
pub(crate) fn doing(operation: Operation, target: Option<&str>) -> String {
    let doing = match operation {
        Operation::Click => "clicking",
        Operation::RightClick => "right-clicking",
        Operation::DoubleClick => "double-clicking",
        Operation::Hover => "hovering over",
        Operation::Drag => "dragging",
        Operation::Fill => "typing in",
        Operation::Press => "pressing a key in",
        Operation::Blur => "leaving",
        Operation::Select => "choosing in",
        Operation::Enter => "pressing Enter in",
        Operation::Escape => "pressing Escape",
        Operation::Scroll => "scrolling down",
        Operation::ScrollUp => "scrolling up",
        Operation::Back => "going back",
        Operation::SwitchTab => "switching tabs",
        Operation::Upload => "uploading to",
        Operation::Dialog => "answering the dialog",
        Operation::Wait => "waiting",
        Operation::Read => "reading",
        Operation::Done => "finishing",
        Operation::Navigate => "opening",
        Operation::Search => "searching",
    };
    match target {
        Some(target) => format!("{doing} {target}"),
        None => doing.to_string(),
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

/// What the run typed into the form a send submitted: each field's last fill since the last send, or since the run
/// left a page, before the send at the end of `history`, with the value it sent. A field cleared or given a secret is
/// listed with no value to check, as a secret's value is never in the record: a field cleared and saved again is still
/// one the send replaced, and keeping its earlier value told Jev the superseded value was missing. When `page`, the
/// page the send was made on, puts the sending control in a form, only that form's fields are listed: a filter typed
/// beside a setting saved in place was checked for after the reload had reset it.
pub(crate) fn typed_before_send(
    history: &[HistoryEntry],
    since: usize,
    sent: &std::collections::HashMap<String, usize>,
    page: &Observation,
) -> Vec<(String, Option<String>)> {
    let Some((send, before)) = history.split_last() else { return Vec::new() };
    let form_of = |name: &str| page.controls.iter().find(|c| describe(c) == name).map(|c| c.form_id.as_deref());
    let form = send.target.as_deref().and_then(form_of).flatten();
    let mut typed = Vec::new();
    let mut filled = std::collections::HashSet::new();
    let steps = before.iter().enumerate().skip(since).rev().filter(|(_, e)| e.outcome == StepOutcome::Executed);
    for (at, entry) in steps {
        match (entry.operation, entry.target.as_ref(), entry.text.as_ref()) {
            (Some(Operation::Navigate | Operation::Back | Operation::SwitchTab), ..) => break,
            // A fill an earlier send carried went with it; another form's send leaves it pending.
            (Some(Operation::Fill), Some(field), _) if sent.get(field).is_some_and(|sent| at < *sent) => {
                filled.insert(field.clone());
            }
            // Walking back, the first fill of a field is its last: a value typed over before the send never went.
            (Some(Operation::Fill), Some(field), Some(value)) if filled.insert(field.clone()) => {
                // A field no longer on the page cannot be placed in a form, so it is kept as the send's.
                if form.is_some() && form_of(field).is_some_and(|own| own != form) {
                    continue;
                }
                let checked = (value != "<secret>" && !value.trim().is_empty()).then(|| value.clone());
                typed.push((field.clone(), checked));
            }
            _ => {}
        }
    }
    typed.reverse();
    typed
}

/// `observation`'s text, whitespace normalised, as `occurrences` counts in it.
pub(crate) fn page_text(observation: &Observation) -> String {
    flat(&format!("{}\n{}", observation.viewport_text, observation.offscreen_text))
}

fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// How many times `value`, whitespace normalised, appears in `page`, a `page_text`.
pub(crate) fn occurrences(page: &str, value: &str) -> usize {
    let value = flat(value);
    if value.is_empty() {
        return 0;
    }
    page.matches(&value).count()
}

/// A step as the words a fact names it by: "click 'Apply'".
pub(crate) fn step_words(operation: Option<Operation>, target: Option<&str>) -> String {
    let word = match operation {
        Some(Operation::Enter) => "Enter in".to_string(),
        Some(operation) => operation.to_string(),
        None => "step".to_string(),
    };
    match target {
        Some(target) => format!("{word} {}", text::repr(target)),
        None => word,
    }
}

/// The fact, while a page the run changed by a send does not show what it typed, that the page may not show what the
/// site kept.
///
/// Whether the run's own value is absent is an exact check, whitespace normalised, not a keyword match over page
/// content, and one source for both facts. A value counts as shown in the page's text or as a control's current value:
/// after a saved description was loaded again, its textarea held the new last line, which is not page text, and both
/// facts kept saying the value did not appear until the run reloaded five times and ended `needs_input`. Before the page is loaded again the fact also states that
/// the run's steps changed what the site keeps; only a value that does not appear is named beside it: that a value
/// appears is true but says nothing about staleness. In a replay of a gallery run whose reorder list was drawn before
/// an upload, Jev chose navigate at 0.19-0.29 with no fact, 0.46-0.51 told that the typed title appeared in the page's
/// text, and 0.73-0.79 told only that the run's click changed what the site keeps and the page was not loaded since.
/// "Opening this page's own address asks the site for its copy again" is unreplayed: it replaces "loads what the
/// site kept", which a live run showed false, as CurseForge served its copy from before a post for about a minute
/// and the loaded page lacked the reply.
///
/// After the page is loaded again at the same address the fact stays while a typed value is still absent, and stops
/// once every one appears. Told nothing then, Jev read the stale page and chose done at 0.71-0.74 in five replays;
/// told the load came after the send, that the value is absent and that a later copy loads after waiting, it chose
/// navigate at 0.33-0.64 (top pick in four of five) and done at 0.20-0.40. A wording ending "so a copy loaded at
/// once can still leave it out" kept done at 0.46-0.62.
/// Each value `kept` holds that `observation` does not show: no more often in its text than just before the send,
/// and in none of its controls' values.
pub(crate) fn kept_absent<'a>(kept: &'a Kept, observation: &Observation) -> Vec<&'a Typed> {
    let page = page_text(observation);
    let values: Vec<String> = observation.controls.iter().filter_map(|c| c.value.as_deref()).map(flat).collect();
    let shown = |typed: &Typed| {
        let value = flat(&typed.value);
        occurrences(&page, &value) > typed.before || values.iter().any(|held| held.contains(&value))
    };
    kept.typed.iter().filter(|typed| !shown(typed)).collect()
}

pub(crate) fn kept_fact(kept: &Kept, observation: &Observation, reloadable: bool) -> Option<String> {
    if kept.url != observation.url || kept.steps.is_empty() {
        return None;
    }
    let absent: Vec<String> = kept_absent(kept, observation)
        .into_iter()
        .map(|typed| format!("{} typed into {}", text::repr(&typed.value), text::repr(&typed.field)))
        .collect();
    if kept.loaded {
        if absent.is_empty() {
            return None;
        }
        let mut fact = format!(
            "This page was loaded after the run's {} was sent, and {} does not appear anywhere in its text: a site can serve its copy from before a send for a while",
            kept.steps.join(" and "),
            absent.join(" and ")
        );
        fact += if reloadable {
            ", and waiting then opening this page's own address again loads a later copy."
        } else {
            "."
        };
        return Some(fact);
    }
    let mut fact = format!(
        "The run's {} changed what the site keeps, and this page has not been loaded since: it shows the site's redraw, which can leave out what the site kept.",
        kept.steps.join(" and ")
    );
    for value in absent {
        fact += &format!(" {value} does not appear anywhere in this page's text.");
    }
    if reloadable {
        fact += " Opening this page's own address asks the site for its copy again.";
    }
    Some(fact)
}

/// The fields still empty here on a task to carry out, leaving out those the task gives no value for: Jev confirmed
/// a form with its one field empty when nothing said so.
pub(crate) fn empty_fields_fact(
    missing: &std::collections::HashSet<(String, String, Option<String>)>,
    observation: &Observation,
) -> Option<String> {
    let empty: Vec<String> = observation
        .controls
        .iter()
        .filter(|control| control.operations.contains(&Operation::Fill))
        .filter(|control| control.value.as_deref().is_none_or(str::is_empty))
        .filter(|control| !missing.iter().any(|(_, label, _)| *label == control.label))
        .map(|control| text::repr(&control.label))
        .collect();
    (!empty.is_empty()).then(|| {
        format!(
            "No value has been entered on this page: {} are empty. A value the task states goes into its field before the form is confirmed.",
            empty.join(", ")
        )
    })
}

/// Observed page changes since a click attempt, without inferring causality or whether a list is complete.
pub(crate) fn page_change_fact(history: &[HistoryEntry], observation: &Observation) -> Option<String> {
    let last = history.last().filter(|entry| entry.added && entry.operation == Some(Operation::Click))?;
    let target = last.target.as_ref()?;
    observation.controls.iter().any(|control| describe(control) == *target).then(|| {
        format!(
            "Since the last attempt to click {target}, page text or controls changed without removing earlier controls. {target} is still offered."
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
