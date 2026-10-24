"""Finishing waits on no call that does not need the other's result."""

import asyncio
import json
from collections.abc import Mapping
from unittest.mock import Mock

import pytest
from pydantic import JsonValue

from fastbrowse.agent import Agent, HeadStart
from fastbrowse.jev import ChoiceAnswer, ChoiceQuestion, Evaluation, NoulAnswer, Question
from fastbrowse.llm import Generation, LLMError
from fastbrowse.models import CostBasis, CostComponent, CostLine, Limits, LLMPurpose, Status
from fastbrowse.page import Page
from fastbrowse.planner import Plan, Requirement, RequirementKind
from tests.test_agent import _fare, _finishing, run_state
from tests.test_policy import FREE, ScriptedJev
from tests.test_retrieval import ScriptedLLM, scripted_identities

# Long enough for any scheduler, short enough that a serial call order fails the test rather than hanging it.
_TOGETHER_SECONDS = 2.0


class _Jev(ScriptedJev):
    """Doubts nothing and finds the draft ready, so only the verifier's place in the order is in question."""

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        return Evaluation(
            model="test",
            answers={
                key: ChoiceAnswer(choice="claim_0", probabilities={"claim_0": 1.0}, confidence=1.0)
                if isinstance(question, ChoiceQuestion)
                else NoulAnswer(probability=0.0)
                for key, question in questions.items()
            },
            input_tokens=10,
            cost=FREE,
        )


async def test_the_verifier_runs_beside_the_ready_drafts_output_audit() -> None:
    """Jev doubted a run whose draft was ready, and the verifier's vision call used to wait for the audit."""
    verifier_started = asyncio.Event()
    purposes: list[str] = []

    class LLM(ScriptedLLM):
        async def generate(self, purpose, messages, schema, **kwargs):
            purposes.append(schema.__name__)
            cost = CostLine(component=CostComponent.LLM, basis=CostBasis.METERED, dollars=0.001, purpose=purpose)
            if schema.__name__ == "LLMVerdict":
                verifier_started.set()
                return Generation(data=schema.model_validate({"missing": [], "complete": True}), cost=cost)
            if schema.__name__ == "_OutputIdentities":
                data = scripted_identities(messages)
            else:
                await asyncio.wait_for(verifier_started.wait(), _TOGETHER_SECONDS)
                fields = json.loads(messages[-1].content)["criteria"]
                data = {"judgments": dict.fromkeys(fields, "yes"), "reason": "Quoted."}
            return Generation(data=schema.model_validate(data), cost=cost)

    state = await run_state()
    agent, on = await _finishing(state, LLM([]), noul=0.0)
    agent._jev = _Jev({})
    state.ready_plan = state.plan.model_copy(
        update={"answer_expected": True, "answer_checks": ("Report the cheapest nonstop fare.",)}
    )
    # Read off an address the run built itself, so the done check's doubt goes to the verifier.
    state.invented = {on.url}
    state.notes.add(_fare(on.url, "fare"))
    async with asyncio.timeout(5):
        result = await agent._finish(state, None, None)
    assert result is not None and result.status is Status.COMPLETE
    assert purposes.count("LLMVerdict") == 1


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


async def test_under_a_cap_the_verifier_waits_for_the_audit_receipts() -> None:
    order: list[str] = []

    class LLM(ScriptedLLM):
        async def generate(self, purpose, messages, schema, **kwargs):
            order.append(schema.__name__)
            kwargs["ledger"].reserve(CostComponent.LLM)
            cost = CostLine(component=CostComponent.LLM, basis=CostBasis.METERED, dollars=0.001, purpose=purpose)
            if schema.__name__ == "LLMVerdict":
                data = {"missing": [], "complete": True}
            elif schema.__name__ == "_OutputIdentities":
                data = scripted_identities(messages)
            else:
                fields = json.loads(messages[-1].content)["criteria"]
                data = {"judgments": dict.fromkeys(fields, "yes"), "reason": "Quoted."}
            return Generation(data=schema.model_validate(data), cost=cost)

    state = await run_state()
    agent, on = await _finishing(state, LLM([]), noul=0.0)
    agent._jev = _Jev({})
    state.ledger.limits = Limits(max_dollars=1.0)
    state.ready_plan = state.plan.model_copy(
        update={"answer_expected": True, "answer_checks": ("Report the cheapest nonstop fare.",)}
    )
    state.invented = {on.url}
    state.notes.add(_fare(on.url, "fare"))
    result = await agent._finish(state, None, None)
    assert result is not None and result.status is Status.COMPLETE
    assert order[-1] == "LLMVerdict" and "_OutputAssessment" in order


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


async def test_a_verifier_that_finished_is_billed_when_the_audit_beside_it_fails() -> None:
    verified = asyncio.Event()

    class LLM(ScriptedLLM):
        async def generate(self, purpose, messages, schema, **kwargs):
            kwargs["ledger"].reserve(CostComponent.LLM)
            if schema.__name__ == "LLMVerdict":
                verified.set()
                return Generation(
                    data=schema.model_validate({"missing": [], "complete": True}),
                    cost=CostLine(component=CostComponent.LLM, basis=CostBasis.METERED, dollars=0.5, purpose=purpose),
                )
            await asyncio.wait_for(verified.wait(), _TOGETHER_SECONDS)
            # Give the finished verifier its turn before this audit fails.
            await asyncio.sleep(0.01)
            raise LLMError("audit provider failed")

    state = await run_state()
    agent, on = await _finishing(state, LLM([]), noul=0.0)
    agent._jev = _Jev({})
    state.ready_plan = state.plan.model_copy(
        update={"answer_expected": True, "answer_checks": ("Report the cheapest nonstop fare.",)}
    )
    state.invented = {on.url}
    state.notes.add(_fare(on.url, "fare"))
    with pytest.raises(LLMError):
        await agent._finish(state, None, None)
    assert [line.dollars for line in state.ledger.lines].count(0.5) == 1
