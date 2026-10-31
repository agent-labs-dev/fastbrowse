//! The stall rule's other half: Jev is asked again, told what went wrong, with the action that moved nothing left out
//! of its options, until the rule's budget runs out and the run ends.

use fastbrowse_core::effects::state_key;
use fastbrowse_core::models::{Operation, Status};
use fastbrowse_core::page::Observation;
use fastbrowse_core::py::text;
use fastbrowse_core::safety::{origin_of, starts_download};
use serde_json::{Map, json};

use super::Agent;
use super::helpers::{decision_label, describe};
use super::state::{RunState, Step, Stop};
use crate::policy::{Decision, StepContext, decide};

/// The action a stall is about, which the next ask leaves out: its operation, and the control it acted on by label.
pub(super) type Withheld = Option<(Operation, Option<String>)>;

/// A refused DONE, withheld from the ask that follows it.
pub(super) const DONE: Withheld = Some((Operation::Done, None));

/// Ask Jev about `problem` on `offered`, with the action it is about left out of the options.
pub(super) fn withhold(offered: &mut Observation, context: &mut StepContext, problem: &str, withheld: Withheld) {
    let mut problem = problem.to_string();
    if let Some((operation, label)) = withheld {
        let mut left_out = false;
        match &label {
            Some(label) => {
                for control in offered.controls.iter_mut().filter(|control| describe(control) == *label) {
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
            problem += &format!("\n{what} did not move the task on from this page, so it is not offered this time.");
        }
    }
    context.problem = Some(problem);
}

/// Leave the actions that moved nothing out of an ask on `offered`.
pub(super) fn leave_out_idle(state: &RunState, offered: &mut Observation, context: &mut StepContext) {
    for action in &state.idle {
        let problem = context.problem.take().unwrap_or_else(|| "The last action changed nothing.".to_string());
        withhold(offered, context, &problem, action.clone());
    }
    // Told once, Jev still reaches for a field it has no value for, by any operation it offers.
    offered.controls.retain(|control| {
        !state
            .missing
            .iter()
            .any(|(url, label, field)| *url == offered.url && control.label == *label && control.context == *field)
    });
}

/// `decision`'s action, withheld from the ask that follows it.
pub(super) fn withheld(decision: &Decision) -> Withheld {
    Some((decision.operation, decision_label(decision)))
}

impl Agent {
    /// Spend one recovery on `reason`. Past `stall.max_recoveries` the run ends: as the failure standing on this page,
    /// or as `gives_up_as`.
    pub(super) async fn recover(
        &self,
        state: &mut RunState,
        observation: &Observation,
        reason: &str,
        gives_up_as: Status,
        withheld: Withheld,
    ) -> Step {
        let reason = self.redact(reason);
        state.recoveries += 1;
        // Recovery spends the stall's evidence so the same threshold crossing cannot trigger it again.
        state.unchanged = 0;
        if observation.controls.iter().any(starts_download) {
            self.downloads_forbidden(state).await?;
        }
        if state.recoveries > self.config.stall.max_recoveries {
            if gives_up_as != Status::NeedsInput
                && let Some(failure) = &state.site_failure
            {
                return Err(failure.stop().into());
            }
            let without_value = state.missing.iter().any(|(url, ..)| *url == observation.url);
            let status = if without_value { Status::NeedsInput } else { gives_up_as };
            return Err(Stop::new(status, reason).into());
        }
        self.ask_jev_again(state, observation, &reason, withheld).await
    }

    /// Jev asked again on this page, told what went wrong, its pick taken as the next decision there.
    async fn ask_jev_again(
        &self,
        state: &mut RunState,
        observation: &Observation,
        reason: &str,
        withheld: Withheld,
    ) -> Step {
        let secrets = self.secret_names(&origin_of(&observation.url));
        let mut context = self.context(state, observation, secrets, false, false)?;
        let mut offered = observation.clone();
        context.problem = Some(reason.to_string());
        leave_out_idle(state, &mut offered, &mut context);
        let reason = context.problem.take().unwrap_or_default();
        withhold(&mut offered, &mut context, &reason, withheld);
        context.screenshot = self.viewport().await?;
        let decision = decide(
            &*self.jev,
            &offered,
            &context,
            &self.config,
            Some(&state.ledger),
            Some(&state.relevance_cache),
            &self.trace,
        )
        .await?;
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
