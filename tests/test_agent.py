"""Field context and recovery state regressions, with model outputs scripted."""

import asyncio
import json
from unittest.mock import AsyncMock, Mock

import pytest

from fastbrowse.agent import Agent, _RunState, _Stop  # pyright: ignore[reportPrivateUsage]
from fastbrowse.config import Config
from fastbrowse.llm import Generation
from fastbrowse.models import Authorization, Limits, LLMPurpose, Operation, Status, StepOutcome
from fastbrowse.page import ActResult, Control, Page
from fastbrowse.planner import Plan
from fastbrowse.policy import HistoryEntry, decide
from fastbrowse.telemetry import Ledger
from tests.test_policy import FREE, ScriptedJev, context, observation
from tests.test_retrieval import ScriptedLLM


async def run_state() -> _RunState:
    plan = Plan(requirements=(), answer_expected=False)

    async def planned() -> Generation[Plan]:
        return Generation(data=plan, cost=FREE)

    planning = asyncio.create_task(planned())
    await planning
    return _RunState(
        task="Find a train from Bristol to York on 16 October 2026",
        inputs={},
        attachments=(),
        authorization=Authorization(),
        ledger=Ledger(Limits()),
        planning=planning,
        ready_plan=plan,
    )


def field(label: str = "Search elsewhere") -> Control:
    return Control(
        id="field",
        frame_id=None,
        role="combobox",
        label=label,
        value="Bath",
        operations=frozenset({Operation.FILL, Operation.CLICK, Operation.ENTER}),
    )


async def test_field_writer_receives_popup_context_and_other_field_values() -> None:
    target = field()
    other = field("Destination").model_copy(update={"id": "destination", "value": "York"})
    obs = observation((target, other)).model_copy(update={"viewport_text": "Choose a station"})
    state = await run_state()
    state.hint = "Replace the origin"
    state.history.append(
        HistoryEntry(operation=Operation.CLICK, target="Origin", outcome=StepOutcome.EXECUTED, page_changed=True)
    )
    llm = ScriptedLLM([{"missing": False, "text": "Bristol"}])
    agent = Agent(Mock(spec=Page), ScriptedJev({}), llm)

    assert await agent._generate_text(state, obs, target) == "Bristol"  # pyright: ignore[reportPrivateUsage]
    purpose, messages = llm.calls[0]
    prompt = json.loads(messages[1].content)
    assert purpose is LLMPurpose.FIELD_TEXT
    assert prompt["field"]["value"] == "Bath"
    assert prompt["other_fields"][0]["value"] == "York"
    assert prompt["recent_actions"][0]["target"] == "Origin"
    assert prompt["subgoal"] == "Replace the origin"
    assert prompt["page"]["text"] == "Choose a station"


async def test_missing_personal_information_still_stops_without_filling() -> None:
    target = field("Account number")
    page = Mock(spec=Page)
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([{"missing": True, "text": ""}]))
    with pytest.raises(_Stop) as stopped:
        await agent._generate_text(await run_state(), observation((target,)), target)  # pyright: ignore[reportPrivateUsage]
    assert stopped.value.status is Status.NEEDS_INPUT
    page.act.assert_not_called()


@pytest.mark.parametrize("outcome", [StepOutcome.EXECUTED, StepOutcome.STALE])
async def test_recovery_hint_is_consumed_only_when_action_progresses(outcome: StepOutcome) -> None:
    target = field()
    obs = observation((target,))
    state = await run_state()
    state.hint = "Open the origin picker"
    state.authorization = Authorization(irreversible_actions=True)
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=outcome, page_changed=outcome is StepOutcome.EXECUTED))
    jev = ScriptedJev({"operation": "click", "click_target": target.id})
    decision = await decide(jev, obs, context(), Config())
    agent = Agent(page, jev, ScriptedLLM([]))
    await agent._step(state, obs, decision)  # pyright: ignore[reportPrivateUsage]
    assert state.hint == (None if outcome is StepOutcome.EXECUTED else "Open the origin picker")
