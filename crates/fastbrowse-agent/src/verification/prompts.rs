//! The verifier's prompts, copied character for character from verification.py's literals: a request differing
//! from Python's by one byte is a replay miss.

use fastbrowse_core::models::UNTRUSTED;

pub(super) const REACHED: &str = "When the requested outcome is to open or reach a page, that destination must be showing now. An earlier visit followed by Back does not satisfy it. An executed click proves only the interaction, not that its destination loaded; an HTTP error is not a successful visit. For a search or filter, set fields and executed clicks do not prove the resulting content loaded. Require current matching results or an explicit empty-result state, not a pending search or loading view. A starting address or an intermediate visit in a longer task may be evidenced by the visited addresses, the first of which is where the run began; it does not have to remain open.";

pub(super) const UNGROUNDED: &str = "Requirement ids whose facts were read from a page that is not the one the task described: a summary, preview or default listing reached by a guessed address, or a page that does not show the task's own query, filters, dates, sort or category in force. Name one here even when it has evidence, because the evidence is from the wrong page.";

pub(super) fn complete() -> String {
    format!(
        "{UNTRUSTED}\nDoes the page or the notes visibly confirm completion of the task in state? Requested run_reports are copied from browser state and the action record after completion; they do not need page quotes or a written answer yet. Be strict: a matching link, a filled but unsubmitted form, or a partial result is not done. For comparisons, require the requested constraints and ordering or a comparison of all matching results. A highlighted result or query preview alone is insufficient."
    )
}

pub(super) fn unmet_action(requirement_text: &str) -> String {
    format!(
        "{UNTRUSTED}\nIs this requirement not visibly satisfied? The actions in state are the run's own record; a requirement that orders actions is satisfied only when they show that order. {REACHED}\n\n{requirement_text}"
    )
}

pub(super) fn unmet_information(requirement_text: &str) -> String {
    format!(
        "{UNTRUSTED}\nDo the notes lack the facts this requirement needs? A comparison or conclusion needs every fact it is drawn from, for the right entities; the conclusion itself need not be written.\n\n{requirement_text}"
    )
}

pub(super) fn draft_needs_writing() -> String {
    format!(
        "{UNTRUSTED}\nDoes the draft in state need rewriting before it answers the task? It does if it misses part of what was asked, repeats or contradicts itself, includes unrelated facts, leaves a comparison, count or calculation undone, or gives a value without saying which part of the task it answers when that is not plain. A short quoted value or labelled field can answer as written; it does not need a full sentence. A comparison that states its result can include the values it compared. These are supporting facts, not unrelated ones. Judge the requested answer: when a task asks to perform actions and report a fact, the answer need only report that fact, not recap each action. Completion of the actions is checked separately. Requested run_reports are appended by code from browser state and the action record, so their absence from this page-fact draft is not an omission."
    )
}

pub(super) fn verdict(evidence: &str) -> String {
    format!(
        "## Verdict\nDecide from {evidence} whether the task is finished. Be strict and name every requirement id that is not visibly satisfied. An action requirement (clicking a link or button, filling a field, submitting a form, navigating) is satisfied when the steps taken show it executed on the control the task meant and produced the requested outcome. The task may paraphrase the control's label; the acted-on control is its equivalent when the page offered no closer match, which is the match the agent made when it acted. The steps are the run's own record, and a click that navigated is not visible on the page it left. Be strict about whether the action happened, not about the task's wording of the label. A requirement that orders actions (enter one value, then go back and change it) is satisfied only when the steps show that order. {REACHED} A date relative to today is judged against the current date. A requirement to compare, count or conclude from facts is satisfied when the notes hold those facts: the answer draws the conclusion, and no page shows it. A requirement to narrow a search or listing (a filter, option or sort) is satisfied when the page shows it applied, in a set control, the address or the page's own filter text, not when the rows in view happen to match it.\n\nThen judge where the facts came from. A page holding values of the right kind is not the page the task described unless it shows that task's own query, filters, dates, sort or category in force. A summary, preview, default or related listing can show prices or rows that are not the ones asked for. Name in ungrounded every requirement whose facts were read from such a page, even when it has evidence. An address this run built from the task can land on the wrong search: judge such a page by what it shows, not by how the run reached it. An answer states the terms a value was quoted under, such as what it covers and the dates it applies to."
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn verify_user(
    task: &str,
    today: &str,
    requirements: &str,
    run_reports: &str,
    steps: &str,
    controls: &str,
    grounding: &str,
    visited: &str,
    url: &str,
) -> String {
    format!(
        "## Task\n{task}\n\n## Current date\n{today}\n\n## Requirements\n{requirements}\n\n## Run reports\n{run_reports}\nThese reports are copied from browser state and the action record after completion; they do not need page quotes or a written answer yet.\n\n## Steps taken\n{steps}\n\n## Set controls\n{controls}\n\n{grounding}{visited}## Page\n{url}\n"
    )
}

pub(super) const STALE_REPAIR: &str = "Some cited facts are no longer current or collection records are missing. Use current source quotes and preserve their collection scope; do not cite superseded conclusions.";

pub(super) const UNFINISHED_REPAIR: &str = "A count, ranking, winner or absence rests on a list that was not read to its end. Read the rest of the list before reporting it.";

pub(super) const UNSTATED_REPAIR: &str =
    "The answer does not state this output. Report it from the notes or a supported bounded absence.";

pub(super) fn removed(requirements: &str, remaining: &str, claim_text: &str) -> String {
    format!(
        "{UNTRUSTED}\n\n# Requirements\n{requirements}\n\n# Remaining answer\n{remaining}\n\n# Removed claim\n{claim_text}\n\nDoes removing this claim leave any requested output missing from the remaining answer? Check each requested field for each requested entity separately. The removed claim is not evidence and need not be true. Judge whether its subject still needs an answer. An optional detail or an output already stated elsewhere does not count as missing."
    )
}
