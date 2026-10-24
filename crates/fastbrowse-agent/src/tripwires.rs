//! Port of `src/fastbrowse/tripwires.py`: signals that a run is grinding rather than progressing, evaluated from what
//! the run already recorded.
//!
//! One unchanged page is the signal fastbrowse has always acted on, and it misses the two ways a run can move without
//! getting anywhere: repeating one interaction, and changing pages while resolving nothing the plan asked for. Skyvern
//! runs the same three (`fail_fast/shadow.py`) and, at the time of writing, still runs all of them in shadow mode
//! while it measures how often each would fire on a run that went on to succeed. That is the reason for
//! `TripwireMode`: a tripwire that ends a healthy run is worse than one that never fires.
//!
//! Pure: no IO, no mutation, so a would-fire evaluation costs nothing and can run on every step.

use std::collections::HashMap;
use std::fmt;

use fastbrowse_core::models::{Operation, Tripwire};

use crate::policy::HistoryEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tripped {
    pub tripwire: Tripwire,
    pub streak: usize,
}

impl fmt::Display for Tripped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.tripwire, self.streak)
    }
}

/// What makes two history entries the same interaction, or None when the entry is not one.
///
/// READ and DONE are excluded because neither targets an element: a run that legitimately reads the same page twice
/// is not repeating an action, and counting it would fire this tripwire on every careful run. The visible effect is
/// part of it for the same reason: "Next" that opens a new page each time is walking, and only "Next" that does the
/// same thing again is grinding.
pub fn action_signature(entry: &HistoryEntry) -> Option<String> {
    let operation = entry.operation.filter(|op| !matches!(op, Operation::Read | Operation::Done))?;
    let target = entry.target.as_ref()?;
    Some(format!(
        "{operation}:{target}:{}:{}",
        entry.text.as_deref().unwrap_or_default(),
        entry.effect.as_deref().unwrap_or_default()
    ))
}

pub fn repeated_action(history: &[HistoryEntry], limit: usize) -> Option<Tripped> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for signature in history.iter().filter_map(action_signature) {
        *counts.entry(signature).or_default() += 1;
    }
    let count = counts.into_values().max()?;
    (count >= limit).then_some(Tripped { tripwire: Tripwire::ActionRepetition, streak: count })
}

/// How many equal marks the list ends with.
pub fn trailing_run<S: PartialEq>(marks: &[S]) -> usize {
    let Some(last) = marks.last() else { return 0 };
    marks.iter().rev().take_while(|mark| *mark == last).count()
}

/// `marks` is one fingerprint of the unresolved requirements per step, oldest first.
pub fn stagnant_plan<S: PartialEq>(marks: &[S], limit: usize) -> Option<Tripped> {
    if marks.is_empty() {
        return None;
    }
    let run = trailing_run(marks);
    (run >= limit).then_some(Tripped { tripwire: Tripwire::PlanStagnation, streak: run })
}

#[cfg(test)]
mod tests {
    //! tests/test_tripwires.py: pure functions over recorded history, so these are the cheapest proof they hold.
    use fastbrowse_core::models::StepOutcome;

    use super::*;

    fn entry(
        operation: Option<Operation>,
        target: Option<&str>,
        text: Option<&str>,
        effect: Option<&str>,
    ) -> HistoryEntry {
        HistoryEntry {
            text: text.map(str::to_string),
            effect: effect.map(str::to_string),
            ..HistoryEntry::new(operation, target, StepOutcome::Executed, true)
        }
    }

    #[test]
    fn test_reads_are_not_repeatable_actions() {
        // A run that reads the same page repeatedly is being careful, not grinding; only the unchanged-page count
        // should have an opinion about it.
        assert_eq!(action_signature(&entry(Some(Operation::Read), None, None, None)), None);
        assert_eq!(action_signature(&entry(Some(Operation::Done), None, None, None)), None);
        assert_eq!(action_signature(&entry(None, None, None, None)), None);
        assert_eq!(action_signature(&entry(Some(Operation::Click), Some("Next"), None, None)).unwrap(), "click:Next::");
    }

    #[test]
    fn test_paging_forward_is_walking_and_the_same_click_to_the_same_result_is_grinding() {
        let walk: Vec<HistoryEntry> = (2..6)
            .map(|n| entry(Some(Operation::Click), Some("Next"), None, Some(&format!("address: /results?page={n}"))))
            .collect();
        assert_eq!(repeated_action(&walk, 3), None);
        let grind = vec![entry(Some(Operation::Click), Some("Stops"), None, Some("address: /flights?tfs=ABC")); 3];
        assert!(repeated_action(&grind, 3).is_some());
    }

    #[test]
    fn test_same_target_with_a_different_value_is_not_a_repetition() {
        let history = [
            entry(Some(Operation::Fill), Some("Email"), Some("a@example.com"), None),
            entry(Some(Operation::Fill), Some("Email"), Some("b@example.com"), None),
        ];
        assert_eq!(repeated_action(&[history.clone(), history].concat(), 3), None);
    }

    #[test]
    fn test_one_interaction_recurring_trips() {
        let history = vec![entry(Some(Operation::Click), Some("Submit"), None, None); 3];
        let tripped = repeated_action(&history, 3).unwrap();
        assert_eq!(tripped.tripwire, Tripwire::ActionRepetition);
        assert_eq!(tripped.streak, 3);
        assert_eq!(tripped.to_string(), "action_repetition (3)");
    }

    #[test]
    fn test_trailing_run_counts_only_the_end() {
        assert_eq!(trailing_run(&["a", "b", "b", "b"]), 3);
        assert_eq!(trailing_run(&["b", "b", "a"]), 1);
        assert_eq!(trailing_run::<&str>(&[]), 0);
    }

    #[test]
    fn test_plan_stagnation_needs_an_unbroken_tail() {
        // Resolving one requirement changes the mark, which is what clears the tripwire.
        assert_eq!(stagnant_plan(&["r1,r2", "r1,r2", "r1"], 3), None);
        assert_eq!(stagnant_plan::<&str>(&[], 3), None);
        let tripped = stagnant_plan(&["r1", "r1,r2", "r1,r2", "r1,r2"], 3).unwrap();
        assert_eq!(tripped.tripwire, Tripwire::PlanStagnation);
        assert_eq!(tripped.streak, 3);
    }
}
