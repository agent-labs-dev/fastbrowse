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


def normalize(status: str | None, *, hosted: bool = False, hosted_success: bool | None = None) -> Ending:
    value = (status or "").lower()
    if hosted:
        # The SDK ends a finished session as `stopped`, or `idle` where it keeps the session open.
        if value in {"stopped", "idle"} and hosted_success is True:
            return Ending.DONE
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


def status_matches(arm: str, status: str | None, expected: Status, success: bool | None = None) -> bool:
    # Distinct safe stops share a summary class, but a login wall cannot pass a confirmation task.
    if expected != Status.COMPLETE:
        return arm == "fastbrowse" and status == expected.value
    return normalize(status, hosted=arm == "browser-use", hosted_success=success) == Ending.DONE
