//! The done check's questions to Jev.

use fastbrowse_core::models::UNTRUSTED;

pub(super) const REACHED: &str = "When the requested outcome is to open or reach a page, that destination must be showing now. An earlier visit followed by Back does not satisfy it. An executed click proves only the interaction, not that its destination loaded; an HTTP error is not a successful visit. For a search or filter, set fields and executed clicks do not prove the resulting content loaded. Require current matching results or an explicit empty-result state, not a pending search or loading view. A starting address or an intermediate visit in a longer task may be evidenced by the visited addresses, the first of which is where the run began; it does not have to remain open.";

pub(super) fn unmet_action(requirement_text: &str) -> String {
    format!(
        "{UNTRUSTED}\nIs this requirement not satisfied? The page and the actions in state are the evidence: the actions are the run's own record of what each one visibly did, so an earlier action's effect counts after its page is gone. A requirement that orders actions is satisfied only when they show that order. {REACHED}\n\n{requirement_text}"
    )
}

pub(super) fn unmet_information(requirement_text: &str, cited: bool) -> String {
    let fact = if cited { "" } else { " No note in state cites evidence for this requirement." };
    format!(
        "{UNTRUSTED}\nIs this requirement not answered by the cited notes in state? It is answered only when a note quotes the page supporting it, for every entity and constraint it names.{fact}\n\n{requirement_text}"
    )
}

pub(super) fn unmet_task() -> String {
    format!("{UNTRUSTED}\nIs the task in state not yet done? The actions in state are the run's own record. {REACHED}")
}

pub(super) fn draft_needs_writing() -> String {
    format!(
        "{UNTRUSTED}\nDoes the draft in state need rewriting before it answers the task? It does if it misses part of what was asked, repeats or contradicts itself, includes unrelated facts, leaves a comparison, count or calculation undone, or gives a value without saying which part of the task it answers when that is not plain. A short quoted value or labelled field can answer as written; it does not need a full sentence. A comparison that states its result can include the values it compared. These are supporting facts, not unrelated ones. Judge the requested answer: when a task asks to perform actions and report a fact, the answer need only report that fact, not recap each action. Completion of the actions is checked separately. Requested run_reports are appended by code from browser state and the action record, so their absence from this page-fact draft is not an omission."
    )
}
