"""Common endings retain each framework's raw status for diagnosis."""

from enum import StrEnum

from fastbrowse.models import Status


class Ending(StrEnum):
    DONE = "done"
    STOPPED = "stopped"
    BUDGET = "budget"
    TIMEOUT = "timeout"
    ERROR = "error"
    BLOCKED = "blocked"
    UNAVAILABLE = "unavailable"


def normalize(status: str | None, *, hosted: bool = False, answered: bool = False) -> Ending:
    """`answered` is the hosted agent's own `done`: it delivered a result that was not an error. Browser Use's
    `is_task_successful` is not used; it is Browser Use's own later judgement of the session, which failed correct
    answers whose sessions showed no sign of failing, and fastbrowse is held only to its own completion."""
    value = (status or "").lower()
    if hosted:
        # The SDK ends a finished session as `stopped`, or `idle` where it keeps the session open.
        if value in {"stopped", "idle"}:
            return Ending.DONE if answered else Ending.STOPPED
        if value in {"complete", "done"}:
            return Ending.ERROR
    if value in {"complete", "done"}:
        return Ending.DONE
    if value in {"budget", "budget_exceeded"}:
        return Ending.BUDGET
    if value in {"timeout", "timed_out"}:
        return Ending.TIMEOUT
    if value in {"blocked", "unavailable"}:
        return Ending(value)
    if value in {
        "stopped",
        "paused",
        "needs_confirmation",
        "needs_login",
        "needs_input",
        "unverified",
        "stuck",
        "observation_limit",
        "cancelled",
        "canceled",
    }:
        return Ending.STOPPED
    return Ending.ERROR


def status_matches(arm: str, status: str | None, expected: Status, answered: bool = False) -> bool:
    # Distinct safe stops share a summary class, but a login wall cannot pass a confirmation task.
    if expected != Status.COMPLETE:
        return arm == "fastbrowse" and status == expected.value
    return normalize(status, hosted=arm == "browser-use", answered=answered) == Ending.DONE
