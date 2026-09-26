"""Checkable plans from the task alone.

The plan is written while the start page loads, so it never sees page content: a live comparison found the
same requirements with and without the first observation, and page text is the one input an attacker writes.
Callers redact task text before this seam; the planner has no secret resolver.
"""

from enum import StrEnum
from typing import Self

from pydantic import Field, model_validator

from fastbrowse.llm import Generation, LLMClient, Message
from fastbrowse.models import Frozen, LLMPurpose
from fastbrowse.telemetry import Ledger


class RequirementKind(StrEnum):
    ACTION = "action"
    INFORMATION = "information"


class Requirement(Frozen):
    id: str = Field(min_length=1)
    text: str = Field(min_length=1)
    kind: RequirementKind


class Plan(Frozen):
    requirements: tuple[Requirement, ...]
    answer_expected: bool

    @model_validator(mode="after")
    def validate_ids(self) -> Self:
        if len({requirement.id for requirement in self.requirements}) != len(self.requirements):
            raise ValueError("requirement ids must be unique")
        return self


def _instructions() -> Message:
    return Message(
        role="system",
        content=(
            "# Planner\nList the outcomes the user asked for as individually checkable requirements, each one short "
            "sentence. Split compound requests into separate requirements. An information requirement is a fact "
            "to find; an action requirement is a change the user asked for (log in, add to cart, submit). "
            "Navigating, searching or opening a page is how the work gets done, not a requirement, even when the "
            "task names the search to run or the page to open before what it asks: 'search for X, open its page and "
            "tell me Y' has one requirement, to find Y. That is only searching and opening pages: a click, entry or "
            "submission the task names is still an action requirement. But when reaching a page is all the user "
            "asked for, reaching it is the one action requirement. An address the task says to start at or go to "
            "before asking for something else is where the work begins, and the "
            "browser may already be there: it is not a requirement of its own. A search the "
            "user asked only to run is such a page: it is one action requirement, to leave the search showing with "
            "the filters the user named applied, and not a fact to find. Every task has "
            "at least one requirement. Answering is not a requirement either: say whether the user expects an "
            "answer.\n\n"
            "Each information requirement must retain the relevant constraints from the task, including dates, "
            "filters and comparison criteria, and adds none the task did not state: a total the user wants "
            "reported is the order's total, not the total once the order is finished. Keep related output fields "
            "together when they identify one result. "
            "Do not create a separate requirement to find that same result again.\n\n"
            "# Secrets\nNever write a password, token or other secret value into a requirement."
        ),
    )


async def make_plan(
    llm: LLMClient, task: str, *, start: str | None = None, ledger: Ledger | None = None
) -> Generation[Plan]:
    site = "" if start is None else f"\n\n# Start page\n{start}"
    return await llm.generate(
        LLMPurpose.PLAN,
        [
            _instructions(),
            Message(role="user", content=f"# Task\n{task}{site}"),
        ],
        Plan,
        ledger=ledger,
    )
