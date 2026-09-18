"""Field context and recovery state regressions, with model outputs scripted."""

import asyncio
import json
from unittest.mock import AsyncMock, Mock

import pytest

# pyright: reportPrivateUsage=false
from fastbrowse.agent import Agent, _follow_recovery, _history, _RunState, _Stop, _unread
from fastbrowse.config import Config, ObservationLimits
from fastbrowse.llm import Generation
from fastbrowse.memory import Notes
from fastbrowse.models import Authorization, Decider, Limits, LLMPurpose, Operation, Status, StepOutcome, StepResult
from fastbrowse.page import ActResult, Control, Page
from fastbrowse.planner import Plan, Requirement, RequirementKind
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

    assert await agent._generate_text(state, obs, target) == "Bristol"
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
    agent = Agent(page, ScriptedJev({}, noul=0.1), ScriptedLLM([{"missing": True, "text": ""}]))
    with pytest.raises(_Stop) as stopped:
        await agent._generate_text(await run_state(), observation((target,)), target)
    assert stopped.value.status is Status.NEEDS_INPUT
    page.act.assert_not_called()


async def test_a_value_the_task_states_is_asked_for_again_rather_than_ending_the_run() -> None:
    target = field("Last Name")
    llm = ScriptedLLM([{"missing": True, "text": ""}, {"missing": False, "text": "Lovelace"}])
    agent = Agent(Mock(spec=Page), ScriptedJev({}, noul=0.9), llm)

    assert await agent._generate_text(await run_state(), observation((target,)), target) == "Lovelace"
    assert "never invent one" in llm.calls[1][1][-1].content


async def test_a_second_missing_verdict_ends_the_run() -> None:
    target = field("Account number")
    llm = ScriptedLLM([{"missing": True, "text": ""}, {"missing": True, "text": ""}])
    agent = Agent(Mock(spec=Page), ScriptedJev({}, noul=0.9), llm)
    with pytest.raises(_Stop) as stopped:
        await agent._generate_text(await run_state(), observation((target,)), target)
    assert stopped.value.status is Status.NEEDS_INPUT


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
    await agent._step(state, obs, decision)
    assert state.hint == (None if outcome is StepOutcome.EXECUTED else "Open the origin picker")


async def test_step_log_names_which_twin_was_clicked() -> None:
    twins = tuple(
        Control(
            id=f"add{i}",
            frame_id=None,
            role="button",
            label="Add to cart",
            context=name,
            operations=frozenset({Operation.CLICK}),
        )
        for i, name in enumerate(("Sauce Labs Backpack", "Sauce Labs Bike Light"))
    )
    obs = observation(twins)
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    jev = ScriptedJev({"operation": "click", "click_target": "add1"})
    decision = await decide(jev, obs, context(), Config())
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    agent = Agent(page, jev, ScriptedLLM([]))
    await agent._step(state, obs, decision)
    assert state.steps[0].target == "Add to cart (Sauce Labs Bike Light)"
    assert state.history[0].target == "Add to cart (Sauce Labs Bike Light)"


async def test_going_round_between_pages_stops_counting_as_progress() -> None:
    link = Control(id="about", frame_id=None, role="link", label="(about)", operations=frozenset({Operation.CLICK}))
    obs = observation((link,))
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    jev = ScriptedJev({"operation": "click", "click_target": "about"})
    decision = await decide(jev, obs, context(), Config())
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    agent = Agent(page, jev, ScriptedLLM([]))
    for _ in range(2):
        await agent._step(state, obs, decision)
    assert state.unchanged == 0
    # The page changes every time, but the same click from the same page a third time is a cycle.
    await agent._step(state, obs, decision)
    assert state.unchanged == 1


def test_an_answer_owed_with_nothing_read_is_unread() -> None:
    action_only = Plan(
        requirements=(Requirement(id="r1", text="Search for the quote", kind=RequirementKind.ACTION),),
        answer_expected=True,
    )
    assert _unread(action_only, Notes())
    assert not _unread(action_only.model_copy(update={"answer_expected": False}), Notes())


async def test_an_option_click_that_changes_no_value_is_not_progress() -> None:
    trigger = Control(
        id="trip",
        frame_id=None,
        role="combobox",
        label="Ticket type",
        value="Round trip",
        operations=frozenset({Operation.CLICK}),
    )
    option = Control(id="one", frame_id=None, role="option", label="One way", operations=frozenset({Operation.CLICK}))
    before = observation((trigger, option))
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    # The menu closed, which changes the page, but the ticket type still reads Round trip.
    page.observe = AsyncMock(return_value=observation((trigger,)))
    jev = ScriptedJev({"operation": "click", "click_target": "one"})
    decision = await decide(jev, before, context(), Config())
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    agent = Agent(page, jev, ScriptedLLM([]))
    await agent._step(state, before, decision)
    assert state.unchanged == 1
    assert state.history[-1].effect == "removed 1 control: One way"
    assert (state.steps[-1].note or "").startswith("no effect")


async def test_the_next_observation_records_what_an_action_did() -> None:
    field_before = field()
    state = await run_state()
    state.acted_from = observation((field_before,))
    state.history.append(
        HistoryEntry(
            operation=Operation.FILL, target="Search elsewhere", outcome=StepOutcome.EXECUTED, page_changed=True
        )
    )
    Agent._note_effect(state, observation((field_before.model_copy(update={"value": "York"}),)))
    assert state.history[-1].effect == "changed Search elsewhere value: Bath -> York"
    assert state.acted_from is None


async def test_jev_still_unsure_after_recovery_takes_the_action_recovery_named() -> None:
    buttons = tuple(
        Control(id=key, frame_id=None, role="button", label=label, operations=frozenset({Operation.CLICK}))
        for key, label in (("done", "Done"), ("search", "Search"))
    )
    obs = observation(buttons)
    page = Mock(spec=Page)
    page.screenshot = AsyncMock(return_value=b"png")
    recovery = {"diagnosis": "not submitted", "next_subgoal": "Click Search", "give_up": False}
    llm = ScriptedLLM([{**recovery, "control": 1, "operation": "click"}])
    jev = ScriptedJev({"operation": "click", "click_target": "done"})
    state = await run_state()
    await Agent(page, jev, llm)._recover(state, obs, "uncertain next step (0.49)")
    unsure = await decide(jev, obs, context(), Config())
    followed = _follow_recovery(state, obs, unsure, uncertain=True)
    assert followed is not None and followed.target == buttons[1]
    # Used once: the next unsure step is Jev's to recover from again.
    assert _follow_recovery(state, obs, unsure, uncertain=True) is None


async def test_a_named_action_is_not_taken_over_a_confident_choice_or_on_a_control_that_went() -> None:
    button = Control(id="search", frame_id=None, role="button", label="Search", operations=frozenset({Operation.CLICK}))
    jev = ScriptedJev({"operation": "click", "click_target": "search"})
    decision = await decide(jev, observation((button,)), context(), Config())
    state = await run_state()
    state.directed = (Operation.CLICK, "search")
    assert _follow_recovery(state, observation((button,)), decision, uncertain=False) is None
    state.directed = (Operation.CLICK, "search")
    assert _follow_recovery(state, observation(()), decision, uncertain=True) is None


def test_earlier_actions_stay_in_view_without_their_effects() -> None:
    entries = [
        HistoryEntry(
            operation=Operation.FILL, target=f"field {i}", outcome=StepOutcome.EXECUTED, page_changed=True, effect="e"
        )
        for i in range(10)
    ]
    shown = _history(entries, ObservationLimits(history_entries=3, earlier_history_entries=4))
    assert [entry.target for entry in shown] == [f"field {i}" for i in range(3, 10)]
    assert [entry.effect for entry in shown] == [None] * 4 + ["e"] * 3
    assert _history(entries[:2], ObservationLimits(history_entries=3)) == tuple(entries[:2])


@pytest.mark.parametrize(("typed", "reads"), [(True, 1), (False, 0)])
async def test_the_results_of_a_typed_search_are_read_once_before_leaving(typed: bool, reads: int) -> None:
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="When was httpx released?", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    home, results = "https://example.test/", "https://example.test/search?q=httpx"
    first = Operation.FILL if typed else Operation.CLICK
    for operation, url in ((first, home), (Operation.ENTER, home), (Operation.FILL, results)):
        state.steps.append(
            StepResult(
                index=len(state.steps),
                operation=operation,
                decided_by=Decider.JEV,
                outcome=StepOutcome.EXECUTED,
                url=url,
                duration_ms=0,
            )
        )
    agent = Agent(Mock(spec=Page), ScriptedJev({}), ScriptedLLM([]))
    agent._capture = AsyncMock()
    agent._read = AsyncMock(return_value=True)
    button = Control(id="go", frame_id=None, role="button", label="Search", operations=frozenset({Operation.CLICK}))
    obs = observation((button,)).model_copy(update={"url": results})
    decision = await decide(ScriptedJev({"operation": "enter", "enter_target": "go"}), obs, context(), Config())
    for _ in range(2):
        state.read_here = False
        await agent._read_before_leaving(state, obs, decision)
    await asyncio.gather(*state.leaving)
    assert agent._read.await_count == reads
