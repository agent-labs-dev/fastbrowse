from collections.abc import Sequence

import pytest
from pydantic import BaseModel, ValidationError

from fastbrowse.llm import Generation, Message
from fastbrowse.models import (
    CostBasis,
    CostComponent,
    CostLine,
    LLMPurpose,
)
from fastbrowse.planner import Plan, Requirement, RequirementKind, make_plan
from fastbrowse.telemetry import Ledger


def example_plan() -> Plan:
    return Plan(
        requirements=(Requirement(id="r1", text="Find the price", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )


class PlannerLLM:
    def __init__(self, plan: Plan | None = None) -> None:
        self.calls: list[tuple[LLMPurpose, tuple[Message, ...]]] = []
        self.plan = plan if plan is not None else example_plan()
        self.cost = CostLine(
            component=CostComponent.LLM, basis=CostBasis.METERED, dollars=0.01, purpose=LLMPurpose.PLAN
        )

    async def generate[T: BaseModel](
        self,
        purpose: LLMPurpose,
        messages: Sequence[Message],
        schema: type[T],
        *,
        max_output_tokens: int = 2000,
        ledger: Ledger | None = None,
    ) -> Generation[T]:
        # Reserve exactly as the real client does, so a test can see a budget stop a request.
        if ledger is not None:
            ledger.reserve(CostComponent.LLM)
        self.calls.append((purpose, tuple(messages)))
        return Generation(data=schema.model_validate_json(self.plan.model_dump_json()), cost=self.cost)


async def test_planning_reads_only_the_task_and_start_address_and_preserves_cost() -> None:
    llm = PlannerLLM()
    result = await make_plan(llm, "Find the price", start="https://shop.test/")
    purpose, messages = llm.calls[0]
    prompt = "\n".join(message.content for message in messages)
    assert purpose is LLMPurpose.PLAN
    assert result.data == example_plan() and result.cost == llm.cost
    assert "# Task\nFind the price" in prompt and "# Start page\nhttps://shop.test/" in prompt
    assert "individually checkable" in prompt


def test_plan_rejects_duplicate_ids() -> None:
    with pytest.raises(ValidationError, match="unique"):
        Plan(requirements=example_plan().requirements * 2, answer_expected=True)


async def test_planning_keeps_a_spelled_out_search_out_of_the_requirements() -> None:
    # "Search for X, open that article, and tell me the year" became an action requirement to search in 22 plans
    # of 30. A shortcut that opened the article directly then failed the verifier on it, and recovery typed into
    # the site's search box for up to 70s more. A click the task names is work of its own and stays one.
    llm = PlannerLLM()
    await make_plan(llm, "Search for X, open that article, and tell me the year.")
    prompt = "\n".join(message.content for message in llm.calls[0][1])
    assert "'search for X, open its page and tell me Y' has one requirement, to find Y" in prompt
    assert "a click, entry or submission the task names is still an action requirement" in prompt
