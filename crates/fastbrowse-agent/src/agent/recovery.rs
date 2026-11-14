//! The stall rule's other half: Jev is asked again, told what went wrong, with the action that moved nothing left out
//! of its options, until the rule's budget runs out and the run ends.

use fastbrowse_core::effects::{leavable, state_key};
use fastbrowse_core::models::{Operation, Status, StepOutcome};
use fastbrowse_core::page::{Control, Observation};
use fastbrowse_core::py::text;
use fastbrowse_core::safety::{changes_a_value, origin_of, starts_download};
use serde_json::{Map, json};

use super::Agent;
use super::helpers::{decision_label, describe};
use super::state::{Edits, RunState, Step, Stop};
use crate::policy::{Decision, StepContext, decide, takes};
use crate::start::{accept_start, propose_route, recovery_pages};

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
}

impl Idle {
    pub(super) fn performed(action: Withheld) -> Idle {
        Idle { action, outcome: StepOutcome::Executed }
    }

    /// What is true of the action, stated after its name when an ask leaves it out.
    fn why(&self) -> &'static str {
        match self.outcome {
            StepOutcome::Covered => COVERED,
            StepOutcome::Stale => STALE,
            _ => MOVED_NOTHING,
        }
    }
}

impl From<Withheld> for Idle {
    fn from(action: Withheld) -> Idle {
        Idle::performed(action)
    }
}

const MOVED_NOTHING: &str = "did not move the task on from this page, so it is not offered this time.";

/// A covered action was never performed, so it is no evidence about what the action does, only that the cover stood.
const COVERED: &str =
    "was not performed: something covers its target, so it is not offered until an action changes the page.";

/// A stale action never ran, but its outcome alone does not prove that the next observation will show a change.
const STALE: &str = "was not performed by the browser, so it is not offered this time.";

/// Ask Jev about `problem` on `offered`, with the action it is about left out of the options and what is true of it
/// stated after its name.
pub(super) fn withhold(offered: &mut Observation, context: &mut StepContext, problem: &str, withheld: &Idle) {
    let mut problem = problem.to_string();
    if let Some((operation, label)) = withheld.action.clone() {
        let mut left_out = false;
        match &label {
            Some(label) => {
                for control in offered.controls.iter_mut().filter(|control| describe(control) == *label) {
                    if operation == Operation::Blur && leavable(control) {
                        context.withheld_targets.push((operation, control.id.clone()));
                        left_out = true;
                    }
                    if operation == Operation::Press && takes(control, operation) {
                        context.withheld_targets.push((operation, control.id.clone()));
                        left_out = true;
                    }
                    left_out |= control.operations.remove(&operation);
                }
                offered.controls.retain(|control| !control.operations.is_empty());
            }
            None => {
                context.withheld.push(operation);
                left_out = true;
            }
        }
        if left_out {
            let what =
                label.map_or_else(|| operation.to_string(), |label| format!("{operation} {}", text::repr(&label)));
            if !problem.is_empty() {
                problem.push('\n');
            }
            problem += &format!("{what} {}", withheld.why());
        }
    }
    context.problem = (!problem.is_empty()).then_some(problem);
}

/// The labels of the fields standing on this page that the task gave no value for, in page order. A field the task asks
/// nothing of is one only when the page marks it required.
fn unfilled<'a>(observation: &'a Observation, state: &RunState) -> Vec<&'a str> {
    let mut labels: Vec<&str> = Vec::new();
    // A value is typed into a field or chosen in a select, and the task can leave either without one.
    let holds_value =
        |control: &&Control| [Operation::Fill, Operation::Select].iter().any(|op| control.operations.contains(op));
    for control in observation.controls.iter().filter(holds_value) {
        let here = |(url, label, field): &(String, String, Option<String>)| {
            *url == observation.url && control.label == *label && control.context == *field
        };
        let needed = control.required == Some(true) || !state.untouched.iter().any(here);
        if needed && state.missing.iter().any(here) && !labels.contains(&control.label.as_str()) {
            labels.push(&control.label);
        }
    }
    labels
}

/// Leave the actions that moved nothing out of an ask on `offered`. Each is named with what is true of it: the last
/// action may well have changed the page, as a Close that returns to a state already seen does.
pub(super) fn leave_out_idle(state: &RunState, offered: &mut Observation, context: &mut StepContext) {
    for idle in &state.idle {
        let problem = context.problem.take().unwrap_or_default();
        withhold(offered, context, &problem, idle);
    }
    // Told once, Jev still reaches for a field it has no value for, by any operation it offers.
    offered.controls.retain(|control| {
        !state
            .missing
            .iter()
            .any(|(url, label, field)| *url == offered.url && control.label == *label && control.context == *field)
    });
    // A refused edit spends a recovery, so a page of checkboxes offered one at a time would end the run stuck (#838).
    if state.edits == Some(Edits::Forbidden) {
        let mut left_out = false;
        offered.controls.retain_mut(|control| {
            let edits: Vec<Operation> =
                control.operations.iter().copied().filter(|&operation| changes_a_value(operation, control)).collect();
            for operation in &edits {
                control.operations.remove(operation);
            }
            left_out |= !edits.is_empty();
            edits.is_empty() || !control.operations.is_empty()
        });
        if left_out {
            let mut problem = context.problem.take().unwrap_or_default();
            if !problem.is_empty() {
                problem.push('\n');
            }
            problem += EDITS_FORBIDDEN;
            context.problem = Some(problem);
        }
    }
}

/// What an ask is told once the task rules edits out and their operations are no longer offered.
const EDITS_FORBIDDEN: &str =
    "The task rules out changing values on the page, so its fields and settings are not offered.";

/// Offer covered actions again once a performed action changed the page, since it may have removed what covered them. A
/// change the browser made while refusing one, as leaving the field a cover came from, removed no cover.
pub(super) fn uncover(idle: &mut Vec<Idle>) {
    idle.retain(|idle| idle.outcome != StepOutcome::Covered);
}

/// `decision`'s action, withheld from the ask that follows it.
pub(super) fn withheld(decision: &Decision) -> Withheld {
    Some((decision.operation, decision_label(decision)))
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
        withheld: impl Into<Idle>,
    ) -> fastbrowse_core::BoxFuture<'a, Step> {
        let withheld = withheld.into();
        let reason = self.redact(reason);
        // Route proposals add model futures to recovery, which otherwise overflow the native worker stack.
        Box::pin(async move {
            state.recoveries += 1;
            // Recovery spends the stall's evidence so the same threshold crossing cannot trigger it again.
            state.unchanged = 0;
            if observation.controls.iter().any(starts_download) {
                self.downloads_forbidden(state).await?;
            }
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
                // A run that ends needing a value names it: the stall's own reason names no field, so a caller told only
                // "no_progress (3)" has nothing to supply before running the same task again.
                let unfilled = unfilled(observation, state);
                let status = if unfilled.is_empty() { gives_up_as } else { Status::NeedsInput };
                // A drawn page's answer may be only in its pixels. Read the screenshot before the run gives up, so the
                // answer it composes cites what the screen showed instead of ending empty. The read is the same one
                // `conclude` makes, moved onto the path a stalled run reaches without ever concluding.
                if status == Status::Stuck {
                    self.read_drawn_page(state, observation).await?;
                }
                if status == Status::Stuck && state.recoveries == self.config.stall.max_recoveries + 1 {
                    self.known_pages_arrived(state, &observation.url).await?;
                    let mut routes: Vec<_> = state.routes.iter().collect();
                    routes.sort();
                    let mut visited: Vec<_> = state.visited_addresses.iter().collect();
                    visited.sort();
                    let mut context = json!({
                        "task": state.task,
                        "page": observation,
                        "evidence": state.notes.render_for_navigation(),
                        "history": state.history,
                        "already_offered": state.known_pages,
                        "routes_opened": routes,
                        "visited_addresses": visited,
                        "walled": state.walled,
                        "reason": reason,
                    });
                    self.mask_strings(&mut context);
                    let proposal = match recovery_pages(&*self.llm, &context, Some(&state.ledger)).await {
                        Ok(proposal) => {
                            state.ledger.record([proposal.cost])?;
                            proposal.data.urls
                        }
                        Err(crate::error::AgentError::Budget(budget)) => return Err(budget.into()),
                        Err(error) => {
                            self.event("recovery_pages_failed", || {
                                Map::from_iter([("error".into(), error.to_string().into())])
                            });
                            Vec::new()
                        }
                    };
                    let mut fresh = false;
                    for route in proposal.iter().filter_map(|url| accept_start(Some(url))) {
                        let same = |url: &String| fastbrowse_core::navigation::same_address(&route, url);
                        if !same(&observation.url)
                            && !state.known_pages.iter().any(same)
                            && !state.routes.iter().any(same)
                            && !state.walled.iter().any(same)
                            && !state.visited_addresses.iter().any(same)
                            && !self.reveals_text(&route)
                        {
                            state.known_pages.push(route);
                            fresh = true;
                        }
                    }
                    if fresh {
                        return self.ask_jev_again(state, observation, &reason, withheld).await;
                    }
                }
                // A canvas or picture carries what its text layer does not, so a run that gave up on one is told apart
                // from a stall on a page it could read: a caller otherwise retries the same task on a page it cannot read.
                let reason = if status == Status::Stuck && observation.visual_content && !state.notes.answered() {
                    format!("{reason} {DRAWN_UNREAD}")
                } else {
                    reason
                };
                let reason = match unfilled.as_slice() {
                    [] => reason,
                    [only] => format!("{} needs a value the task does not give: {reason}", text::repr(only)),
                    many => format!(
                        "{} need a value the task does not give: {reason}",
                        many.iter().map(|label| text::repr(label)).collect::<Vec<_>>().join(", ")
                    ),
                };
                return Err(Stop::new(status, reason).into());
            }
            self.ask_jev_again(state, observation, &reason, withheld).await
        })
    }

    /// Open another route to what the task needs after a bot check at `walled` did not clear, as the first address
    /// was proposed: a run whose search met PyPI's challenge stopped blocked, while hosted agents opened the project
    /// page itself (#953). False when no route is left: none is proposed, the one proposed was opened before or is
    /// walled, or the run's recoveries are spent.
    pub(super) async fn route_around(
        &self,
        state: &mut RunState,
        observation: &Observation,
        walled: &str,
    ) -> Step<bool> {
        if !state.walled.iter().any(|known| known == walled) {
            state.walled.push(walled.to_string());
        }
        if state.recoveries >= self.config.stall.max_recoveries {
            return Ok(false);
        }
        let walled: Vec<String> = state.walled.iter().map(|url| self.redact(url)).collect();
        let mut opened: Vec<String> = state.routes.iter().map(|url| self.redact(url)).collect();
        opened.sort();
        let proposal = propose_route(&*self.llm, &state.task, &walled, &opened, Some(&state.ledger)).await?;
        state.ledger.record([proposal.cost])?;
        let Some(route) = accept_start(proposal.data.url.as_deref())
            .filter(|route| !state.walled.contains(route) && state.routes.insert(route.clone()))
        else {
            return Ok(false);
        };
        state.recoveries += 1;
        self.event("route_around", || Map::from_iter([("url".into(), self.redact(&route).into())]));
        self.open_by_code(state, observation, route).await?;
        Ok(true)
    }

    /// Jev asked again on this page, told what went wrong, its pick taken as the next decision there.
    async fn ask_jev_again(
        &self,
        state: &mut RunState,
        observation: &Observation,
        reason: &str,
        withheld: Idle,
    ) -> Step {
        let secrets = self.secret_names(&origin_of(&observation.url));
        let mut context = self.context(state, observation, secrets, false, false)?;
        let mut offered = observation.clone();
        context.problem = Some(reason.to_string());
        leave_out_idle(state, &mut offered, &mut context);
        state.mark_read_links(&mut offered);
        state.mark_shown(&mut offered);
        let reason = context.problem.take().unwrap_or_default();
        withhold(&mut offered, &mut context, &reason, &withheld);
        context.screenshot = self.viewport().await?;
        let decision = decide(&*self.jev, &offered, &context, &self.config, Some(&state.ledger), &self.trace).await?;
        self.event("recover", || {
            Map::from_iter([
                ("reason".into(), reason.into()),
                ("operation".into(), json!(decision.operation)),
                ("target".into(), json!(decision_label(&decision).map(|label| self.redact(&label)))),
            ])
        });
        state.picked = Some((state_key(observation), decision));
        Ok(())
    }
}
