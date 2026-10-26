"""Finishing waits on no call that does not need the other's result."""

import asyncio
from unittest.mock import Mock

from fastbrowse.agent import Agent, HeadStart
from fastbrowse.models import Limits, LLMPurpose, Status
from fastbrowse.page import Page
from fastbrowse.planner import Plan, Requirement, RequirementKind
from tests.test_agent import run_state
from tests.test_policy import ScriptedJev
from tests.test_retrieval import ScriptedLLM

# Long enough for any scheduler, short enough that a serial call order fails the test rather than hanging it.
_TOGETHER_SECONDS = 2.0


async def test_the_plan_arrives_with_its_answer_checks() -> None:
    """One plan call writes the checks, so taking the plan takes them and nothing is normalized afterwards."""
    plan = Plan(
        requirements=(Requirement(id="r1", text="Find the price", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
        answer_checks=("Report the price of the item.",),
    )
    head = HeadStart.begin(ScriptedLLM([plan.model_dump(mode="json")]), "Find the price")
    state = await run_state()
    state.ledger, state.planning, state.ready_plan = head.ledger, head.planning, None
    state.plan_billed = head.plan_billed
    async with asyncio.timeout(_TOGETHER_SECONDS):
        taken = await state.await_plan()
    assert taken.requirements == plan.requirements and taken.answer_checks == plan.answer_checks
    assert state.ledger.llm_calls == 1
    assert sum(line.purpose is LLMPurpose.PLAN for line in state.ledger.lines) == 1


async def test_a_plan_that_spent_the_dollar_cap_stops_before_browsing(monkeypatch) -> None:
    """The plan's receipt must be in the ledger before any browsing call reserves: a cap the plan alone spent
    used to let a later paid request through, since the run recorded the plan only when it took it."""
    llm = ScriptedLLM(
        [
            {
                "requirements": [{"id": "r1", "text": "Find the price", "kind": "information"}],
                "answer_expected": True,
                "answer_checks": ["price"],
            }
        ]
    )
    # Each scripted call costs $0.001, so the plan alone is over this cap.
    head = HeadStart.begin(llm, "Find the price", limits=Limits(max_dollars=0.0005))
    planned = await head.planning
    page = Mock(spec=Page)
    page.artifacts = ()
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]))

    async def consume(state, output_schema, until):
        await state.await_plan()
        raise AssertionError("taking a plan over the cap must stop the run")

    monkeypatch.setattr(agent, "_loop", consume)
    result = await agent.run(head.task, head_start=head)
    assert result.status is Status.BUDGET_EXCEEDED
    assert result.budget is not None and result.budget.resource == "dollars"
    # Only the plan was sent.
    assert len(llm.calls) == 1 and llm.calls[0][1][0].content.startswith("# Planner")
    assert result.cost.lines == (planned.cost,)


async def test_under_a_cap_the_plan_is_taken_with_its_checks() -> None:
    """A capped run takes the plan with its checks, so the plan's receipt is recorded before browsing reserves."""
    plan = Plan(
        requirements=(Requirement(id="r1", text="Find the price", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
        answer_checks=("Report the price of the item.",),
    )
    llm = ScriptedLLM([plan.model_dump(mode="json")])
    head = HeadStart.begin(llm, "Find the price", limits=Limits(max_dollars=1.0))
    state = await run_state()
    state.ledger, state.planning, state.ready_plan = head.ledger, head.planning, None
    state.plan_billed = head.plan_billed
    taken = await state.await_plan()
    assert taken.answer_checks == ("Report the price of the item.",)
    assert len(state.ledger.lines) == 1
