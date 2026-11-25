//! The stall rule's other half: the driver is told what went wrong and not let take the action that moved nothing
//! again, until the rule's budget runs out and the run ends.

use fastbrowse_core::models::{Operation, Status, StepOutcome};
use fastbrowse_core::page::{Control, Observation};
use fastbrowse_core::py::text;
use fastbrowse_core::safety::starts_download;

use super::Agent;
use super::helpers::{decision_label, describe};
use super::state::{RunState, Step, Stop};
use crate::policy::{Decision, Given, takes};

/// The action a stall is about, which the next ask leaves out: its operation, and the control it acted on by label.
pub(super) type Withheld = Option<(Operation, Option<String>)>;

/// A refused DONE, withheld from the ask that follows it.
pub(super) const DONE: Withheld = Some((Operation::Done, None));

/// What a run that gave up with a picture in view says of it: only what it knows. A picture in view does not show that
/// the page draws what its text leaves out, and saying so had a CurseForge comments page, plain HTML with member
/// avatars, reported as a canvas the run could not read (#987).
pub(super) const DRAWN_UNREAD: &str = "This page shows a canvas, picture or video beside its text, and its screenshot was not read as answering the task.";

/// An action the stall rule counted as moving nothing, with the outcome the browser gave it: one it performed, one it
/// refused because something covered the target, or one whose observed target or page state it could not use.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Idle {
    pub action: Withheld,
    pub outcome: StepOutcome,
    /// The observed target of a covered action, distinct from any later control with the same label.
    pub covered_target_id: Option<String>,
    /// Why a failed action was not performed, in the words of the browser or app that refused it. Told only that a
    /// click on a row out of view "did not move the task on", a run pressed OK next and submitted the
    /// dialog's default row, though the page had said to scroll the row into view (#1403).
    pub refused: Option<String>,
    /// The driver's exact action, from `tried`, which it is not let take again on the same page.
    pub tried: Option<String>,
}

impl Idle {
    pub(super) fn performed(action: Withheld) -> Idle {
        Idle { action, outcome: StepOutcome::Executed, covered_target_id: None, refused: None, tried: None }
    }

    /// The driver's `decision`, which moved nothing.
    pub(super) fn tried(decision: &Decision) -> Idle {
        Idle { tried: Some(tried(decision)), ..Idle::performed(withheld(decision)) }
    }
}

impl From<Withheld> for Idle {
    fn from(action: Withheld) -> Idle {
        Idle::performed(action)
    }
}

/// Offer covered actions again once a performed action changed the page, since it may have removed what covered them. A
/// change the browser made while refusing one, as leaving the field a cover came from, removed no cover.
pub(super) fn uncover(idle: &mut Vec<Idle>, before: &Observation, after: &Observation, summary: &mut String) {
    // Closing a route-backed overlay can change the URL while keeping the same document and control.
    let same_page =
        before.document_key == after.document_key && (!before.document_key.is_empty() || before.url == after.url);
    let mut available = Vec::new();
    idle.retain(|idle| {
        if idle.outcome != StepOutcome::Covered {
            return true;
        }
        if same_page
            && let Some((operation, Some(label))) = &idle.action
            && let Some(id) = &idle.covered_target_id
            && after
                .controls
                .iter()
                .any(|control| control.id == *id && takes(control, *operation) && describe(control) == *label)
        {
            available.push(format!("{operation} {}", text::repr(label)));
        }
        false
    });
    if !available.is_empty() {
        summary.push_str(&format!(
            "; Previously covered, with the target still on the page: {}. These actions did not execute. If the task requires them, carry them out before any later step that needs them.",
            available.join(", ")
        ));
    }
}

/// `decision`'s action, withheld from the ask that follows it.
pub(super) fn withheld(decision: &Decision) -> Withheld {
    Some((decision.operation, decision_label(decision)))
}

/// The exact action the driver took: its operation, the control by id, what it was given (the key, text, option, file or
/// secret name) and where a drag dropped. Keyed by label alone, Space on a widget where ArrowDown changed nothing was
/// refused as already tried, and a no-op click on one "Next" link refused the other.
pub(super) fn tried(decision: &Decision) -> String {
    let given = match &decision.given {
        Some(Given::Text(given)) => format!("text {}", text::repr(given)),
        Some(Given::Secret(name)) => format!("secret {name}"),
        Some(Given::Seconds(_)) | None => String::new(),
    };
    let id = |control: &Option<Control>| control.as_ref().map(|control| control.id.clone()).unwrap_or_default();
    format!(
        "{} {} {} {given} {}",
        decision.operation,
        id(&decision.target),
        decision_label(decision).unwrap_or_default(),
        id(&decision.destination)
    )
}

impl Agent {
    /// Spend one recovery on `reason`. Past `stall.max_recoveries` the run ends: as the failure standing on this page or
    /// on the address the run started at, or as `gives_up_as`.
    pub(super) fn recover<'a>(
        &'a self,
        state: &'a mut RunState,
        observation: &'a Observation,
        reason: &str,
        gives_up_as: Status,
    ) -> fastbrowse_core::BoxFuture<'a, Step> {
        let reason = self.redact(reason);
        // Route proposals add model futures to recovery, which otherwise overflow the native worker stack.
        Box::pin(async move {
            state.recoveries += 1;
            // Recovery spends the stall's evidence so the same threshold crossing cannot trigger it again.
            state.unchanged = 0;
            if observation.controls.iter().any(starts_download) {
                self.downloads_forbidden(state).await?;
            }
            // The driver is told why and plans again from what it knows; past the budget the run ends.
            if state.recoveries > self.config.stall.max_recoveries {
                // A page that reports the site failed ends as that failure, whatever the run was last unsure about: a
                // field the task gives no value for, on a page whose own data requests failed, is not the caller's to
                // supply. BU Bench bu2-196 was asked for a 2024 USITC report on a DataWeb page reading "Http failure
                // response for ...: 0 Unknown Error", was asked for input on a control labelled "undefined", and ended
                // needs_input as if a person could supply it.
                if let Some(failure) = state.site_failure.as_ref().filter(|failure| failure.url == observation.url) {
                    return Err(failure.stop().into());
                }
                // A run that left a start address that failed and stalled elsewhere ends as that failure, not as a stall.
                if gives_up_as != Status::NeedsInput
                    && let Some(failure) = state.site_failure.as_ref().or(state.start_failure.as_ref())
                {
                    return Err(failure.stop().into());
                }
                let status = gives_up_as;
                // A drawn page's answer may be only in its pixels. Read the screenshot before the run gives up, so the
                // answer it composes cites what the screen showed instead of ending empty. The read is the same one
                // `conclude` makes, moved onto the path a stalled run reaches without ever concluding.
                if status == Status::Stuck {
                    self.read_drawn_page(state, observation).await?;
                }
                // A canvas or picture carries what its text layer does not, so a run that gave up on one is told apart
                // from a stall on a page it could read: a caller otherwise retries the same task on a page it cannot read.
                let reason = if status == Status::Stuck && observation.visual_content && !state.notes.answered() {
                    format!("{reason} {DRAWN_UNREAD}")
                } else {
                    reason
                };
                // Recovery needs the draft, but the caller must not receive its unfilled field notation (#1247).
                let reason = if gives_up_as == Status::Unverified
                    && let Some(rejection) = &state.composition_feedback
                {
                    self.redact(&super::finishing::rejection_reason(rejection, true))
                } else {
                    reason
                };
                return Err(Stop::new(status, "Recovery/exhausted", reason).into());
            }
            // The caller tells the driver why: a refused done told "3 actions in a row changed nothing" sent it looking
            // for a stall it never made.
            state.chat.queue_clear();
            Ok(())
        })
    }
}
