import asyncio
from unittest.mock import AsyncMock, Mock

import pytest

from fastbrowse.agent import Agent, _code_decision, _PageRead, _Stop
from fastbrowse.memory import Fact
from fastbrowse.models import FactReader, Operation, Status
from fastbrowse.page import BlockKind, Dialog, Page
from fastbrowse.planner import Plan, Requirement, RequirementKind
from fastbrowse.retrieval import ReadOutcome
from tests.test_agent import run_state
from tests.test_memory import evidence
from tests.test_policy import ScriptedJev, observation
from tests.test_retrieval import ScriptedLLM, capture


@pytest.mark.parametrize("kind", [RequirementKind.ACTION, RequirementKind.INFORMATION])
async def test_unread_departure_keeps_a_pending_requirement_value(kind: RequirementKind) -> None:
    operation = Operation.BACK
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="reference", text="Use the return reference", kind=kind),),
        answer_expected=False,
    )
    quote = "Return reference: R-418"
    obs = observation(()).model_copy(update={"document_key": "receipt"})
    page = Mock(spec=Page)
    page.capture = AsyncMock(return_value=capture((BlockKind.PARAGRAPH, quote)))
    llm = ScriptedLLM([{"claims": [{"text": quote, "cite": {"first": "s0", "last": "s0"}}], "answered": False}])
    agent = Agent(page, ScriptedJev({"fact": "synthesis"}), llm)
    decision = _code_decision(operation, None)

    assert await agent._read_before_interaction(state, obs, decision)
    assert [step.operation for step in state.steps] == [Operation.READ]
    assert quote in [e.quote for e in state.notes.evidence.values()]
    if kind is RequirementKind.ACTION:
        assert not state.notes.evidenced("reference")
    assert not await agent._read_before_interaction(state, obs, decision)
    if kind is RequirementKind.ACTION:
        assert len(llm.calls) == 1


async def test_back_is_reconsidered_after_reading_with_information_already_answered() -> None:
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(
            Requirement(id="return", text="Enter the receipt reference", kind=RequirementKind.ACTION),
            Requirement(id="email", text="Find the account email", kind=RequirementKind.INFORMATION),
        ),
        answer_expected=False,
    )
    state.notes.add(Fact(text="user@example.test", requirement_id="email", evidence=evidence(), reader=FactReader.LLM))
    quote = "Return reference: R-418"
    obs = observation((), can_go_back=True).model_copy(update={"document_key": "receipt", "viewport_text": quote})
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=obs)
    page.capture = AsyncMock(return_value=capture((BlockKind.PARAGRAPH, quote)))
    llm = ScriptedLLM([{"claims": [{"text": quote, "cite": {"first": "s0", "last": "s0"}}], "answered": False}])
    jev = ScriptedJev({"operation": "back"}, noul=0.0)
    agent = Agent(page, jev, llm)

    async def leave(*args: object) -> None:
        assert quote in [e.quote for e in state.notes.evidence.values()]
        assert len(jev.requests) == 2
        raise _Stop(Status.UNVERIFIED)

    page.act = AsyncMock(side_effect=leave)
    with pytest.raises(_Stop):
        await agent._loop(state, None, None)
    assert [step.operation for step in state.steps] == [Operation.READ]


@pytest.mark.parametrize(
    "operation",
    [op for op in Operation if op not in {Operation.BACK, Operation.SCROLL, Operation.SCROLL_UP}],
)
async def test_non_departure_without_read_evidence_is_unchanged(operation: Operation) -> None:
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="return", text="Enter the receipt reference", kind=RequirementKind.ACTION),),
        answer_expected=False,
    )
    page = Mock(spec=Page)
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]))
    assert not await agent._read_before_interaction(state, observation(()), _code_decision(operation, None))
    page.capture.assert_not_called()
    assert not state.steps


@pytest.mark.parametrize("exemption", ["read_document", "no_requirements", "answered_requirement", "dialog"])
async def test_departure_exemptions_do_not_add_a_read(exemption: str) -> None:
    state = await run_state()
    obs = observation(()).model_copy(update={"document_key": "receipt"})
    state.ready_plan = Plan(
        requirements=(Requirement(id="reference", text="Find the reference", kind=RequirementKind.INFORMATION),),
        answer_expected=False,
    )
    if exemption == "read_document":
        state.reads.add((obs.document_key, "earlier-content", (), ()))
        state.read_here = False
    elif exemption == "no_requirements":
        state.ready_plan = Plan(requirements=(), answer_expected=False)
    elif exemption == "answered_requirement":
        state.notes.add(Fact(text="R-418", requirement_id="reference", evidence=evidence(), reader=FactReader.LLM))
    else:
        obs = obs.model_copy(update={"dialog": Dialog(kind="alert", message="Notice")})
    page = Mock(spec=Page)
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]))
    assert not await agent._read_before_interaction(state, obs, _code_decision(Operation.BACK, None))
    page.capture.assert_not_called()
    assert not state.steps


async def test_pipelined_records_count_as_a_read_after_returning_to_the_document() -> None:
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="return", text="Enter the receipt reference", kind=RequirementKind.ACTION),),
        answer_expected=False,
    )
    obs = observation(()).model_copy(update={"document_key": "receipt"})
    page = Mock(spec=Page)
    page.capture = AsyncMock(side_effect=AssertionError("the document was already read by the pipeline"))
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]))

    async def records() -> ReadOutcome:
        return ReadOutcome(facts=(), coverage=(), rejected_claims=0, cost_lines=())

    reading = _PageRead(capture((BlockKind.PARAGRAPH, "Receipt records")), obs, asyncio.create_task(records()), 0, None)
    await agent._join_page(state, reading, ())
    state.read_here = False
    assert not await agent._read_before_interaction(state, obs, _code_decision(Operation.BACK, None))
    page.capture.assert_not_called()
