"""Grade an observed outcome separately from an agent's completion claim."""

from typing import Protocol

from fastbrowse.evals.live_tasks import Category, LiveTask, Outcome, page_defect
from fastbrowse.evals.status import Ending, normalize, status_matches

__fingerprint_namespace__ = "fastbrowse.evals.live"


class ArmReport(Protocol):
    status: str
    answered: bool | None


def grade(
    arm: str, task: LiveTask, truth: object, outcome: Outcome, report: ArmReport
) -> tuple[bool, str | None, Ending]:
    try:
        failure = task.check(outcome, truth)
    except Exception as exc:
        # One task's grader must not discard every other run in the suite: gather propagates, and a 114-run
        # pass is an hour and real money. A grader that raises is that row's failure and nobody else's.
        failure = f"check raised {type(exc).__name__}: {exc}"
    # Right and proven are graded apart: a correct answer the agent could not back with quotes is a
    # different defect from a wrong one, and one pass/fail column hid which the suite was showing.
    if failure is None and task.category is Category.NAVIGATE and not outcome.unobservable:
        # An HTTP error document can have the requested address, so arrival also needs document evidence.
        failure = page_defect(outcome.evidence)
    correct = failure is None
    if failure is None and not status_matches(arm, report.status, task.expect, bool(report.answered)):
        failure = f"status {report.status}, expected {task.expect.value}"
    ending = normalize(report.status, hosted=arm == "browser-use", answered=bool(report.answered))
    return correct, failure, ending
