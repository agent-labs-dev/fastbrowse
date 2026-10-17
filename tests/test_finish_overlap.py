"""Finishing waits on no call that does not need the other's result."""

import asyncio
import json
from collections.abc import Mapping

from pydantic import JsonValue

from fastbrowse.agent import HeadStart
from fastbrowse.jev import ChoiceAnswer, ChoiceQuestion, Evaluation, NoulAnswer, Question
from fastbrowse.llm import Generation
from fastbrowse.models import CostBasis, CostComponent, CostLine, LLMPurpose, Status
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


async def test_reading_does_not_wait_for_the_answer_checks() -> None:
    """The checks only finishing reads are normalized after the plan, while the run browses."""
    release = asyncio.Event()
    plan = Plan(
        requirements=(Requirement(id="r1", text="Find the price", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
        answer_checks=("price",),
    )

    class LLM(ScriptedLLM):
        async def generate(self, purpose, messages, schema, **kwargs):
            if purpose is LLMPurpose.PLAN and schema.__name__ == "_AnswerChecks":
                await release.wait()
                self.responses.append({"checks": ["Report the price of the item."]})
            else:
                self.responses.append(plan.model_dump(mode="json"))
            return await super().generate(purpose, messages, schema, **kwargs)

    head = HeadStart.begin(LLM([]), "Find the price")
    state = await run_state()
    state.ledger, state.planning, state.checking, state.ready_plan = head.ledger, head.planning, head.checking, None
    async with asyncio.timeout(_TOGETHER_SECONDS):
        browsing = await state.await_plan()
    # Draft checks are never offered before they are normalized.
    assert browsing.requirements == plan.requirements and browsing.answer_checks == ()
    release.set()
    async with asyncio.timeout(_TOGETHER_SECONDS):
        finishing = await state.await_checks()
    assert finishing.answer_checks == ("Report the price of the item.",)
    assert state.ledger.llm_calls == 2
    assert sum(line.purpose is LLMPurpose.PLAN for line in state.ledger.lines) == 2
