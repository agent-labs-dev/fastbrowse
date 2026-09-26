"""Field context and recovery state regressions, with model outputs scripted."""

import asyncio
import json
from collections.abc import Mapping
from itertools import pairwise
from typing import Any
from unittest.mock import AsyncMock, Mock

import httpx
import pytest
from pydantic import JsonValue

from fastbrowse import agent as agent_module
from fastbrowse.agent import (
    Agent,
    HeadStart,
    _answered,
    _code_decision,
    _follow_recovery,
    _guessed,
    _history,
    _plan_ids,
    _record,
    _RunState,
    _Stop,
    _try_unsure,
    _unread,
    _Unsure,
    _verified,
    _visited,
)
from fastbrowse.batches import evaluate_batches
from fastbrowse.citations import text_fragment
from fastbrowse.clients.typesafe import TypeSafeJevClient
from fastbrowse.config import Config, ObservationLimits, StallRules, Thresholds
from fastbrowse.effects import state_key
from fastbrowse.jev import Answer, Evaluation, JevError, JevRetriesExhausted, NoulAnswer, NoulQuestion, Question
from fastbrowse.llm import Generation
from fastbrowse.memory import Fact, Notes, NotesTooLarge, Tally, evidence_id, fact_id
from fastbrowse.models import (
    Authorization,
    BrowserEvent,
    Citation,
    CostComponent,
    Decider,
    FactReader,
    Limits,
    LLMPurpose,
    Operation,
    RunResult,
    Status,
    StepEvent,
    StepOutcome,
    StepResult,
)
from fastbrowse.page import (
    Action,
    ActResult,
    BlockKind,
    BrowserError,
    Capture,
    Control,
    Dialog,
    NavigationTimeout,
    Observation,
    Page,
    SiteUnreachable,
)
from fastbrowse.planner import Plan, Requirement, RequirementKind
from fastbrowse.policy import Decision, HistoryEntry, ReadAssessment, build_request, decide
from fastbrowse.retrieval import TRANSACTION_CONTRADICTED, ComposedAnswer
from fastbrowse.safety import Redactor, ScopedSecrets
from fastbrowse.shortcut import Shortcut
from fastbrowse.telemetry import BudgetExceeded, Ledger
from fastbrowse.tripwires import Tripwire
from fastbrowse.verification import LLMVerdict, _grounding
from tests.test_memory import evidence
from tests.test_policy import FREE, ScriptedJev, context, observation
from tests.test_retrieval import ScriptedLLM, capture


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
    steps = ("Set the origin to Bath and search.", "Go back and change the origin to Bristol.")
    state.ready_plan = Plan(
        requirements=tuple(
            Requirement(id=f"r{i}", text=text, kind=RequirementKind.ACTION) for i, text in enumerate(steps)
        ),
        answer_expected=False,
    )
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
    # In order: told only the task, the writer typed a later correction on the first pass.
    assert prompt["requirements"] == list(steps)


async def test_a_field_given_values_in_turn_types_them_in_order() -> None:
    """Told the order, the writer still typed the correction first; the values it lists are typed in turn."""
    target = field("First Name")
    state = await run_state()
    listed: JsonValue = {"missing": False, "text": "Priya Sharman", "values": ["Priya Sharma", "Priya Sharman"]}
    llm = ScriptedLLM([listed, listed, listed])
    agent = Agent(Mock(spec=Page), ScriptedJev({}), llm)
    obs = observation((target,))

    assert await agent._generate_text(state, obs, target) == "Priya Sharma"
    typed = HistoryEntry(
        operation=Operation.FILL,
        target="First Name",
        outcome=StepOutcome.EXECUTED,
        page_changed=False,
        text="Priya Sharma",
    )
    state.history.append(typed.model_copy(update={"outcome": StepOutcome.COVERED}))
    assert await agent._generate_text(state, obs, target) == "Priya Sharma"
    state.history.append(typed)
    assert await agent._generate_text(state, obs, target) == "Priya Sharman"


def test_a_value_typed_before_the_recent_window_stays_in_the_record() -> None:
    """A long wizard pushed its first fill out of the recent actions, and the correction then showed no order."""
    first = HistoryEntry(
        operation=Operation.FILL, target="Name", outcome=StepOutcome.EXECUTED, page_changed=False, text="Ada"
    )
    click = HistoryEntry(operation=Operation.CLICK, target="Next", outcome=StepOutcome.EXECUTED, page_changed=True)
    limits = ObservationLimits(history_entries=2, earlier_history_entries=1)
    record = _record([first, *[click] * 5], limits)
    assert record[0].text == "Ada"
    assert len(record) == 4


def test_a_box_ticked_before_the_recent_window_stays_in_the_record() -> None:
    """A wizard's newsletter tick fell out of the recent actions, and the verifier held the form unfilled."""
    tick = HistoryEntry(
        operation=Operation.CLICK, target="Newsletter", outcome=StepOutcome.EXECUTED, page_changed=True, setting=True
    )
    click = HistoryEntry(operation=Operation.CLICK, target="Next", outcome=StepOutcome.EXECUTED, page_changed=True)
    record = _record([tick, *[click] * 5], ObservationLimits(history_entries=2, earlier_history_entries=1))
    assert record[0].target == "Newsletter"
    assert len(record) == 4


async def test_a_step_abandoned_while_waiting_for_the_plan_leaves_the_plan_to_the_rest_of_the_run() -> None:
    """A fill waits for the plan, and a redraw cancels the fill: the plan every later step needs survives."""
    plan = Plan(requirements=(), answer_expected=False)
    release = asyncio.Event()

    async def planned() -> Generation[Plan]:
        await release.wait()
        return Generation(data=plan, cost=FREE)

    state = await run_state()
    state.ready_plan, state.planning = None, asyncio.create_task(planned())
    waiting = asyncio.create_task(state.await_plan())
    await asyncio.sleep(0)
    waiting.cancel()
    with pytest.raises(asyncio.CancelledError):
        await waiting
    release.set()
    assert await state.await_plan() is plan


async def test_missing_personal_information_stops_once_recovery_returns_to_it() -> None:
    """The first missing verdict is recovery's to route around, since most such fields are optional; a field
    recovery sends the run back to is required, and ends it."""
    target = field("Account number")
    page = Mock(spec=Page)
    llm = ScriptedLLM([{"missing": True, "text": ""}, {"missing": True, "text": ""}])
    agent = Agent(page, ScriptedJev({}, noul=0.1), llm)
    state = await run_state()
    with pytest.raises(_Unsure):
        await agent._generate_text(state, observation((target,)), target)
    with pytest.raises(_Stop) as stopped:
        await agent._generate_text(state, observation((target,)), target)
    assert stopped.value.status is Status.NEEDS_INPUT
    page.act.assert_not_called()


async def test_a_value_the_task_states_is_asked_for_again_rather_than_ending_the_run() -> None:
    target = field("Last Name")
    llm = ScriptedLLM([{"missing": True, "text": ""}, {"missing": False, "text": "Lovelace"}])
    agent = Agent(Mock(spec=Page), ScriptedJev({}, noul=0.9), llm)

    assert await agent._generate_text(await run_state(), observation((target,)), target) == "Lovelace"
    assert "never invent one" in llm.calls[1][1][-1].content


async def test_a_second_missing_verdict_goes_to_recovery() -> None:
    target = field("Account number")
    llm = ScriptedLLM([{"missing": True, "text": ""}, {"missing": True, "text": ""}])
    agent = Agent(Mock(spec=Page), ScriptedJev({}, noul=0.9), llm)
    with pytest.raises(_Unsure):
        await agent._generate_text(await run_state(), observation((target,)), target)


@pytest.mark.parametrize("max_recoveries", [0, 1])
async def test_recovery_giving_up_on_a_missing_value_ends_the_run_needing_input(max_recoveries: int) -> None:
    """Out of budget, or recovery saying only the user can go on: either way the caller is owed the value, and
    `stuck` would tell it to narrow the task instead."""
    target = field("Account number")
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=observation((target,)))
    page.redrawn = AsyncMock(return_value=False)
    page.screenshot = AsyncMock(return_value=b"")
    page.artifacts = ()
    recovery: JsonValue = {
        "diagnosis": "Only the user knows the account number",
        "next_subgoal": "Ask",
        "give_up": True,
        "needs_input": True,
    }
    agent = Agent(
        page,
        ScriptedJev({"operation": "fill", "fill_target": target.id}, noul=0.1),
        ScriptedLLM([{"missing": True, "text": ""}, recovery]),
        config=Config(stall=StallRules(max_recoveries=max_recoveries)),
    )
    with pytest.raises(_Stop) as stopped:
        await agent._loop(await run_state(), None, None)
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
    page.observe = AsyncMock(return_value=obs)
    jev = ScriptedJev({"operation": "click", "click_target": target.id})
    decision = await decide(jev, obs, context(), Config())
    agent = Agent(page, jev, ScriptedLLM([]))
    await agent._step(state, obs, decision)
    agent._settle(state, observation((target.model_copy(update={"value": "Bristol"}),)))
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
    recovery = {"diagnosis": "not submitted", "next_subgoal": "Click Search for hunter2", "give_up": False}
    llm = ScriptedLLM([{**recovery, "control": 1, "operation": "click"}])
    jev = ScriptedJev({"operation": "click", "click_target": "done"})
    state = await run_state()
    agent = Agent(page, jev, llm)
    agent._redactor.register("password", "hunter2")
    await agent._recover(state, obs, "uncertain next step (0.49): hunter2")
    assert state.steps[-1].note == (
        "uncertain next step (0.49): [secret:password]\nnot submitted\nClick Search for [secret:password]"
    )
    unsure = await decide(jev, obs, context(), Config())
    followed = _follow_recovery(state, obs, unsure, uncertain=True)
    assert followed is not None and followed.target == buttons[1]
    # Used once: the next unsure step is Jev's to recover from again.
    assert _follow_recovery(state, obs, unsure, uncertain=True) is None
    # Even authorized, recovery's click is put to Jev: the decision's confidence was Jev's in "Done", not this.
    state.authorization = Authorization(irreversible_actions=True)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    with pytest.raises(_Unsure):
        await agent._step(state, obs, followed, Decider.LLM)
    assert state.steps[-1].outcome is StepOutcome.FAILED and state.steps[-1].confidence is None
    jev.noul = 0.1
    await agent._step(state, obs, followed, Decider.LLM)
    assert state.steps[-1].note == "Click Search for [secret:password]"
    assert state.steps[-1].confidence is None


async def test_an_unsure_pick_is_acted_on_once_per_page_state() -> None:
    state = await run_state()
    first, second = observation((_button("Done"),)), observation((_button("Close dialog"),))
    done, close = _code_decision(Operation.CLICK, _button("Done")), _code_decision(Operation.CLICK, _button("Close"))
    assert _try_unsure(state, first, done)
    assert not _try_unsure(state, first, done)
    assert _try_unsure(state, second, close)


async def test_an_unsure_pick_the_run_already_took_from_this_state_recovers() -> None:
    """One Back from a wizard's Review, Jev was unsure and clicked Next to Review again."""
    state = await run_state()
    step = observation((_button("Back"), _button("Next")))
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    state.authorization = Authorization(irreversible_actions=True)
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]))
    following = _code_decision(Operation.CLICK, _button("Next"))
    await agent._step(state, step, following)
    assert not _try_unsure(state, step, following)
    assert _try_unsure(state, step, _code_decision(Operation.CLICK, _button("Back")))


@pytest.mark.parametrize(("confidence", "raised"), [(0.3, _Unsure), (0.9, _Stop)])
async def test_an_unsure_pick_that_may_commit_something_recovers_rather_than_asking_the_user(
    confidence: float, raised: type[Exception]
) -> None:
    button = _button("Place order for hunter2")
    jev = ScriptedJev({"operation": "click", "click_target": button.id}, noul=0.9)
    obs = observation((button,))
    decision = (await decide(jev, obs, context(), Config())).model_copy(update={"operation_confidence": confidence})
    state = await run_state()
    page = Mock(spec=Page)
    on_event = AsyncMock()
    agent = Agent(page, jev, ScriptedLLM([]), on_event=on_event)
    agent._redactor.register("password", "hunter2")
    with pytest.raises(raised) as refused:
        await agent._step(state, obs, decision)
    step = state.steps[-1]
    assert step.decided_by is Decider.JEV and step.outcome is StepOutcome.FAILED
    assert step.note == agent._redactor.redact(str(refused.value))
    assert step.facts == ()
    on_event.assert_awaited_once_with(StepEvent(step=step))
    assert "hunter2" not in step.model_dump_json()
    page.act.assert_not_called()


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
    assert _history(entries, ObservationLimits(history_entries=0, earlier_history_entries=0)) == ()


@pytest.mark.parametrize("authorized", [False, True])
@pytest.mark.parametrize("operation", [Operation.CLICK, Operation.FILL, Operation.SELECT])
async def test_url_edits_are_not_reads_but_each_result_in_one_document_is_preserved(
    authorized: bool, operation: Operation
) -> None:
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=authorized)
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="Compare both results", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    control = field().model_copy(update={"operations": frozenset({operation})})
    obs = observation((control,)).model_copy(update={"document_key": "same-document"})
    page = Mock(spec=Page)
    jev = ScriptedJev({"operation": operation.value, f"{operation.value}_target": control.id, "r1": "synthesis"})
    llm = ScriptedLLM(
        [
            {"claims": [{"text": text, "cite": {"first": "s0", "last": "s0"}}], "answered": False}
            for text in ("First result: 12", "Second result: 18")
        ]
    )
    agent = Agent(page, jev, llm)
    agent._action = AsyncMock(return_value=Action(operation=operation, target_id=control.id, text="new query"))
    expected_quote = None

    async def replace_content(*args: object) -> ActResult:
        if expected_quote is not None:
            assert expected_quote in [evidence.quote for evidence in state.notes.evidence.values()]
        page.capture = AsyncMock(return_value=capture((BlockKind.PARAGRAPH, "Editing")))
        return ActResult(outcome=StepOutcome.EXECUTED, page_changed=True)

    page.act = AsyncMock(side_effect=replace_content)
    decision = await decide(jev, obs, context(), Config())
    for query in ("B", "Br", "Bristol"):
        obs = obs.model_copy(update={"url": f"https://example.test/?q={query}"})
        editing = decision.model_copy(update={"read_assessment": ReadAssessment.EDITING})
        assert not await agent._read_before_interaction(state, obs, editing)
        await agent._step(state, obs, editing)
    page.capture.assert_not_called()
    relevant = decision.model_copy(update={"read_assessment": ReadAssessment.EVIDENCE})
    results_url = obs.url
    for text in ("First result: 12", "Second result: 18"):
        # The page shows each result set, so the observation differs from the one the edits left unread.
        obs = obs.model_copy(update={"url": results_url, "viewport_text": text})
        page.capture = AsyncMock(return_value=capture((BlockKind.PARAGRAPH, text)).model_copy(update={"url": obs.url}))
        assert await agent._read_before_interaction(state, obs, relevant)
        # Even a URL rewrite and another choice to READ cannot re-read this content and requirement set.
        obs = obs.model_copy(update={"url": obs.url + "&view=compact"})
        assert not await agent._read_before_interaction(state, obs, relevant)
        await agent._read(state, await agent._capture(), obs)
        expected_quote = text
        await agent._step(state, obs, decision)
    assert [evidence.quote for evidence in state.notes.evidence.values()] == ["First result: 12", "Second result: 18"]
    assert len(llm.calls) == 2
    assert [evidence.url for evidence in state.notes.evidence.values()] == [results_url, results_url]
    assert [step.operation for step in state.steps].count(Operation.READ) == 2
    assert page.act.await_count == 5


@pytest.mark.parametrize("assessment", [ReadAssessment.EVIDENCE, ReadAssessment.ABSENT])
@pytest.mark.parametrize("role", ["button", "link"])
async def test_a_message_is_read_before_mutation_and_the_next_action_is_reconsidered(
    assessment: ReadAssessment, role: str
) -> None:
    message = "The account is locked out" if assessment is ReadAssessment.EVIDENCE else "Your preferences were saved"
    control = _button("Continue").model_copy(update={"role": role})
    obs = observation((control,)).model_copy(update={"viewport_text": message})
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="Report why login failed", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=obs)
    page.capture = AsyncMock(return_value=capture((BlockKind.PARAGRAPH, message)))
    page.artifacts = ()

    async def remove_message(*args: object) -> ActResult:
        page.observe.return_value = obs.model_copy(update={"viewport_text": ""})
        return ActResult(outcome=StepOutcome.EXECUTED, page_changed=True)

    page.act = AsyncMock(side_effect=remove_message)

    class MessageJev(ScriptedJev):
        async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
            # A preserved message makes the next decision DONE instead of the stale dismissal.
            if "operation" in questions and isinstance(state, dict) and state.get("notes"):
                self.pick = {"operation": "done"}
            return await super().evaluate(state, questions)

    jev = MessageJev(
        {"operation": "click", "click_target": control.id, "read_assessment": assessment.value, "r1": "c0"},
        noul=0.0,
    )
    agent = Agent(page, jev, ScriptedLLM([]))
    # Stop at the next decision after the preservation, or immediately after an irrelevant message is removed.
    state.ledger.limits = Limits(max_steps=1 if assessment is ReadAssessment.ABSENT else 2)
    expected = agent._result(state, state.ledger, Status.COMPLETE)
    agent._finish = AsyncMock(return_value=expected)
    if assessment is ReadAssessment.EVIDENCE:
        assert await agent._loop(state, None, None) is expected
        assert next(iter(state.notes.evidence.values())).quote == message
        assert state.steps[0].operation is Operation.READ
        page.act.assert_not_awaited()
        agent._finish.assert_awaited_once()
    else:
        with pytest.raises(BudgetExceeded):
            await agent._loop(state, None, None)
        page.act.assert_awaited_once()
        page.capture.assert_not_awaited()
        assert not state.notes.facts


@pytest.mark.parametrize("second", ["read", "click"])
async def test_after_a_forced_read_jev_decides_again_and_a_repeat_read_recovers(
    second: str,
) -> None:
    fare = "Oslo to Rome, 1 stop, $320"
    nonstop, pager = _button("Nonstop"), _button("Next")
    obs = observation((nonstop, pager)).model_copy(update={"viewport_text": fare})
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="Find the cheapest nonstop fare", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=obs)
    page.capture = AsyncMock(return_value=capture((BlockKind.PARAGRAPH, fare)))
    page.screenshot = AsyncMock(return_value=b"")
    page.artifacts = ()
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))

    class RereadingJev(ScriptedJev):
        async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
            if "operation" in questions and isinstance(state, dict) and state.get("notes"):
                self.pick = {
                    "operation": second,
                    "click_target": pager.id,
                    "read_assessment": "evidence",
                    "r1": "synthesis",
                }
            return await super().evaluate(state, questions)

    jev = RereadingJev(
        {"operation": "click", "click_target": nonstop.id, "read_assessment": "evidence", "r1": "synthesis"},
        noul=0.0,
    )
    llm = ScriptedLLM([{"claims": [{"text": fare, "cite": {"first": "s0", "last": "s0"}}], "answered": False}])
    agent = Agent(page, jev, llm)
    agent._recover = AsyncMock(side_effect=_Stop(Status.STUCK, "recovering"))
    state.ledger.limits = Limits(max_steps=2)
    from fastbrowse.telemetry import BudgetExceeded

    with pytest.raises(_Stop if second == "read" else BudgetExceeded):
        await agent._loop(state, None, None)
    if second == "read":
        agent._recover.assert_awaited_once()
        page.act.assert_not_awaited()
    else:
        agent._recover.assert_not_awaited()
        page.act.assert_awaited_once()
        assert state.steps[-1].target == pager.label
    assert len(llm.calls) == 1


async def test_an_interaction_is_not_replayed_on_a_control_that_changed_during_the_read() -> None:
    fare = "Oslo to Rome, 1 stop, $320"
    preview = _button("Preview draft")
    obs = observation((preview,)).model_copy(update={"viewport_text": fare})
    relabelled = obs.model_copy(update={"controls": (preview.model_copy(update={"label": "Send message"}),)})
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="Find the cheapest nonstop fare", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    page = Mock(spec=Page)
    page.observe = AsyncMock(side_effect=[obs, relabelled, relabelled])
    page.capture = AsyncMock(return_value=capture((BlockKind.PARAGRAPH, fare)))
    page.screenshot = AsyncMock(return_value=b"")
    page.artifacts = ()

    class RereadingJev(ScriptedJev):
        async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
            if "operation" in questions and isinstance(state, dict) and state.get("notes"):
                self.pick = {"operation": "read", "read_assessment": "evidence", "r1": "synthesis"}
            return await super().evaluate(state, questions)

    jev = RereadingJev(
        {"operation": "click", "click_target": preview.id, "read_assessment": "evidence", "r1": "synthesis"},
        noul=0.0,
    )
    llm = ScriptedLLM([{"claims": [{"text": fare, "cite": {"first": "s0", "last": "s0"}}], "answered": False}])
    agent = Agent(page, jev, llm)
    agent._recover = AsyncMock(side_effect=_Stop(Status.STUCK, "recovering"))
    agent._finish = AsyncMock(side_effect=_Stop(Status.STUCK, "finishing"))
    with pytest.raises(_Stop):
        await agent._loop(state, None, None)
    page.act.assert_not_called()
    assert Operation.CLICK not in [step.operation for step in state.steps]


@pytest.mark.parametrize(
    ("operation", "page_changed", "reread"),
    [
        (Operation.FILL, False, False),
        (Operation.FILL, True, True),
        (Operation.CLICK, False, True),
    ],
)
async def test_a_page_read_before_typing_is_not_read_again_for_the_typed_text(
    operation: Operation, page_changed: bool, reread: bool
) -> None:
    """pypi-newer read its httpx results again after typing "requests": the capture held the box's new text."""
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="Find the newer release", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    button = _button("Search")
    obs = observation((button,))
    page = Mock(spec=Page)
    page.capture = AsyncMock(return_value=capture((BlockKind.PARAGRAPH, "httpx 0.28.1")))
    llm = ScriptedLLM([{"claims": [], "answered": False}] * 2)
    jev = ScriptedJev(
        {"operation": "click", "click_target": button.id, "read_assessment": "evidence", "r1": "synthesis"}
    )
    decision = await decide(jev, obs, context(), Config())
    agent = Agent(page, jev, llm)
    assert await agent._read_before_interaction(state, obs, decision)
    state.history.append(
        HistoryEntry(operation=operation, target="Search", outcome=StepOutcome.EXECUTED, page_changed=page_changed)
    )
    state.read_here = not page_changed
    page.capture = AsyncMock(return_value=capture((BlockKind.PARAGRAPH, "httpx 0.28.1 requests")))
    assert await agent._read_before_interaction(state, obs, decision) is reread
    assert len(llm.calls) == 1 + reread


async def test_unchanged_unsuccessful_preservation_does_not_loop_or_authorize_the_action() -> None:
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="Find the total", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    button = _button("Place order")
    obs = observation((button,))
    page = Mock(spec=Page)
    page.capture = AsyncMock(return_value=capture((BlockKind.PARAGRAPH, "Pending total")))
    llm = ScriptedLLM([{"claims": [], "answered": False}])
    jev = ScriptedJev(
        {"operation": "click", "click_target": button.id, "read_assessment": "evidence", "r1": "synthesis"}
    )
    decision = await decide(jev, obs, context(), Config())
    agent = Agent(page, jev, llm)
    assert await agent._read_before_interaction(state, obs, decision)
    assert not await agent._read_before_interaction(state, obs, decision)
    await agent._read(state, await agent._capture(), obs)
    assert len(llm.calls) == 1 and not state.notes.facts
    with pytest.raises(_Stop) as stopped:
        await agent._step(state, obs, decision)
    assert stopped.value.status is Status.NEEDS_CONFIRMATION
    page.act.assert_not_called()


@pytest.mark.parametrize("operation", [Operation.READ, Operation.DONE])
async def test_an_exhausted_read_recovers_instead_of_repeating_even_when_jev_is_confident(operation: Operation) -> None:
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="Find the total", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    more = _button("Show total")
    obs = observation((more,))
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=obs)
    page.capture = AsyncMock(return_value=capture((BlockKind.PARAGRAPH, "Pending total")))
    page.screenshot = AsyncMock(return_value=b"png")
    page.artifacts = ()

    async def show_total(*args: object) -> ActResult:
        # The text changes without changing the URL or controls used by the action cycle detector.
        page.capture.return_value = capture((BlockKind.PARAGRAPH, "Total: $12"))
        return ActResult(outcome=StepOutcome.EXECUTED, page_changed=True)

    page.act = AsyncMock(side_effect=show_total)
    jev = ScriptedJev({"operation": operation.value, "r1": "synthesis"}, noul=0.0)
    llm = ScriptedLLM(
        [
            {"claims": [], "answered": False},
            {
                "diagnosis": "This content has no total; reading it again cannot supply one",
                "next_subgoal": "Click Show total to load the missing evidence",
                "operation": "click",
                "control": 0,
                "give_up": False,
            },
            {
                "claims": [{"text": "Total: $12", "cite": {"first": "s0", "last": "s0"}, "requirement_id": "r1"}],
                "answered": True,
            },
        ]
    )
    agent = Agent(page, jev, llm)
    finish = agent._finish

    async def finish_when_evidenced(*args: object) -> agent_module.RunResult | None:
        if state.notes.evidenced("r1"):
            return agent._result(state, state.ledger, Status.COMPLETE)
        return await finish(state, obs, None, None)

    agent._finish = AsyncMock(side_effect=finish_when_evidenced)
    result = await agent._loop(state, None, None)
    assert result.status is Status.COMPLETE
    page.act.assert_awaited_once()
    assert state.recoveries == 1
    assert [purpose for purpose, _ in llm.calls] == [LLMPurpose.READ, LLMPurpose.RECOVER, LLMPurpose.READ]
    assert next(iter(state.notes.evidence.values())).quote == "Total: $12"


@pytest.mark.parametrize("operation", [Operation.READ, Operation.DONE])
async def test_recovery_can_direct_a_page_operation_with_no_control_to_name(operation: Operation) -> None:
    # A Flights run holding every answer was told twice to finish; a dropped DONE left Jev to stall until stuck.
    button = Control(id="next", frame_id=None, role="button", label="Next", operations=frozenset({Operation.CLICK}))
    obs = observation((button,))
    page = Mock(spec=Page)
    page.screenshot = AsyncMock(return_value=b"png")
    recovery = {"diagnosis": "the answer is further down", "next_subgoal": "Read the page", "give_up": False}
    llm = ScriptedLLM([{**recovery, "control": None, "operation": operation.value}])
    jev = ScriptedJev({"operation": "scroll"})
    state = await run_state()
    await Agent(page, jev, llm)._recover(state, obs, "uncertain next step (0.47)")
    unsure = await decide(jev, obs, context(), Config())
    followed = _follow_recovery(state, obs, unsure, uncertain=True)
    assert followed is not None and followed.operation is operation and followed.target is None


@pytest.mark.parametrize("recover_below", [0.55, 1.0])
async def test_directed_done_still_requires_verification_after_an_exhausted_read(recover_below: float) -> None:
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="Find the total", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    obs = observation((_button("Show total"),))
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=obs)
    page.capture = AsyncMock(return_value=capture((BlockKind.PARAGRAPH, "Pending total")))
    llm = ScriptedLLM([{"claims": [], "answered": False}])
    agent = Agent(
        page,
        ScriptedJev({"operation": "read", "r1": "synthesis"}, noul=0.0),
        llm,
        config=Config(thresholds=Thresholds(recover_below=recover_below), stall=StallRules(max_recoveries=0)),
    )
    await agent._read(state, await page.capture(), obs)
    state.directed = (Operation.DONE, None)
    with pytest.raises(_Stop) as stopped:
        await agent._loop(state, None, None)
    assert stopped.value.status is Status.STUCK
    assert any(step.operation is Operation.DONE and step.outcome is StepOutcome.FAILED for step in state.steps)
    assert len(llm.calls) == 1
    page.act.assert_not_called()


@pytest.mark.parametrize(("lookup", "recovered"), [(True, False), (False, False), (True, True)])
async def test_a_read_that_answers_a_lookup_finishes_on_the_page_it_read(lookup: bool, recovered: bool) -> None:
    """A read does not change the page, so observing it again and deciding only arrived at DONE. A plan with
    something left to do on the site, or a read whose tripwire sent the run to recovery, is decided again."""
    kinds = (RequirementKind.INFORMATION,) if lookup else (RequirementKind.INFORMATION, RequirementKind.ACTION)
    state = await run_state()
    state.ready_plan = Plan(
        requirements=tuple(Requirement(id=f"r{i}", text="The license", kind=kind) for i, kind in enumerate(kinds, 1)),
        answer_expected=True,
    )
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=observation((_button("Code"),)))
    page.redrawn = AsyncMock(return_value=False)
    page.capture = AsyncMock(return_value=capture((BlockKind.PARAGRAPH, "BSD-3-Clause license")))
    page.artifacts = ()
    claim: JsonValue = {"text": "BSD-3-Clause", "cite": {"first": "s0", "last": "s0"}, "requirement_id": "r1"}
    agent = Agent(
        page, ScriptedJev({"operation": "read"}, noul=0.0), ScriptedLLM([{"claims": [claim], "answered": True}])
    )
    agent._finish = AsyncMock(return_value=agent._result(state, state.ledger, Status.COMPLETE))
    if recovered:
        step = agent._step

        async def tripped(*args: Any, **kwargs: Any) -> bool:
            skipped = await step(*args, **kwargs)
            state.recoveries += 1
            return skipped

        agent._step = tripped  # ty: ignore[invalid-assignment]
    # One step: a lookup reaches its finish on it, and anything else is stopped deciding its second.
    state.ledger.limits = Limits(max_steps=1)
    if lookup and not recovered:
        await asyncio.wait_for(agent._loop(state, None, None), timeout=1)
        page.observe.assert_awaited_once()
        assert agent._finish.await_args is not None and agent._finish.await_args.args[1] is agent._observed
    else:
        with pytest.raises(BudgetExceeded):
            await asyncio.wait_for(agent._loop(state, None, None), timeout=1)
        agent._finish.assert_not_awaited()


async def test_a_finish_judges_the_last_observation_without_observing_again() -> None:
    """Nothing acts between the loop's observation and `_finish`; one that is stale by then is judged afresh."""
    state = await run_state()
    state.notes.add(_fare("https://example.test/flights/results", "results"))
    agent, on = await _finishing(state, ScriptedLLM([]), noul=0.99)
    observe = agent._page.observe
    assert isinstance(observe, AsyncMock)
    last = await agent._observe()
    observe.reset_mock()
    assert await agent._finish(state, last, None, None) is not None
    observe.assert_not_awaited()
    assert await agent._finish(state, on, None, None) is not None
    observe.assert_awaited_once()


def _button(label: str) -> Control:
    return Control(id=label.lower(), frame_id=None, role="button", label=label, operations=frozenset({Operation.CLICK}))


async def _click(agent: Agent, state: _RunState, on: Observation, label: str) -> None:
    decision = await decide(ScriptedJev({"operation": "click", "click_target": label.lower()}), on, context(), Config())
    await agent._step(state, on, decision)


async def test_a_change_that_leads_back_to_an_earlier_state_is_not_progress() -> None:
    # A date picker (open), the form it closes to, and the picker opened again because the form will not submit.
    picker = observation((_button("Done"), _button("Friday")))
    form = observation((_button("Search"),))
    reopened = observation((_button("Done"), _button("Friday"), _button("Return")))
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]))

    assert agent._settle(state, picker) is None
    await _click(agent, state, picker, "Done")
    assert agent._settle(state, form) is None
    await _click(agent, state, form, "Search")
    assert agent._settle(state, reopened) is None
    assert state.unchanged == 0
    await _click(agent, state, reopened, "Done")
    # Back on the form by a move not made before; Search from it again is the round the run already went.
    assert agent._settle(state, form) is None
    await _click(agent, state, form, "Search")
    note = (
        "back to a page state first reached 2 actions ago; "
        "the actions since (click Done, click Search) undid each other"
    )
    assert agent._settle(state, reopened) == note
    assert state.history[-1].effect == note


async def test_walking_back_through_a_wizard_to_correct_a_step_is_not_a_loop() -> None:
    steps = [observation((_button("Back"), _button("Next"), _button(f"Step {n}"))) for n in range(1, 5)]
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]))
    assert agent._settle(state, steps[0]) is None
    for here, there in pairwise(steps):
        await _click(agent, state, here, "Next")
        assert agent._settle(state, there) is None
    # Review reached; back to the first step, each step once more.
    for here, there in pairwise(reversed(steps)):
        await _click(agent, state, here, "Back")
        assert agent._settle(state, there) is None
    # Going round Next and Back again retraces moves already made.
    await _click(agent, state, steps[0], "Next")
    assert agent._settle(state, steps[1]) is not None


async def test_a_change_to_text_alone_still_counts_as_progress() -> None:
    before = observation((_button("Show more"),))
    after = before.model_copy(update={"viewport_text": "The rest of the article"})
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    state.unchanged = 2
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]))
    agent._settle(state, before)
    await _click(agent, state, before, "Show more")
    assert agent._settle(state, after) is None
    assert state.unchanged == 0


async def test_going_back_after_reading_a_page_is_progress() -> None:
    results, package = observation((_button("httpx"),)), observation((_button("Homepage"),))
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]))
    agent._settle(state, results)
    await _click(agent, state, results, "httpx")
    agent._settle(state, package)
    state.history.append(
        HistoryEntry(operation=Operation.READ, target=None, outcome=StepOutcome.EXECUTED, page_changed=False)
    )
    state.history.append(
        HistoryEntry(operation=Operation.BACK, target=None, outcome=StepOutcome.EXECUTED, page_changed=True)
    )
    state.left = "package"
    assert agent._settle(state, results) is None
    assert state.unchanged == 0
    assert state.history[-1].effect is None


@pytest.mark.parametrize("changed", [False, True])
async def test_a_filter_returning_to_its_prior_value_routes_to_recovery(changed: bool) -> None:
    toggle = _button("Direct only").model_copy(update={"role": "checkbox", "checked": False})
    current = observation((toggle,)).model_copy(update={"document_key": "results"})
    page = Mock(spec=Page)

    async def observe() -> Observation:
        return current

    async def act(*args: object) -> ActResult:
        nonlocal current
        toggle = current.controls[0]
        current = current.model_copy(update={"controls": (toggle.model_copy(update={"checked": not toggle.checked}),)})
        return ActResult(outcome=StepOutcome.EXECUTED, page_changed=changed)

    page.observe = AsyncMock(side_effect=observe)
    page.act = AsyncMock(side_effect=act)
    page.screenshot = AsyncMock(return_value=b"")
    llm = ScriptedLLM([{"diagnosis": "The filter is cycling", "next_subgoal": "Read results", "give_up": True}])
    agent = Agent(page, ScriptedJev({"operation": "click", "click_target": toggle.id}, noul=0.0), llm)
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    with pytest.raises(_Stop):
        await agent._loop(state, None, None)
    assert page.act.await_count == 2
    assert llm.calls[0][0] is LLMPurpose.RECOVER
    prompt = llm.calls[0][1][-1].content
    assert "Direct only keeps returning to checked=False" in prompt
    assert "intervening actions: click Direct only, click Direct only" in prompt
    assert len(llm.calls) == 1


async def test_a_filter_that_keeps_redrawing_the_results_cannot_renew_the_recovery_budget() -> None:
    toggle = _button("Direct only").model_copy(update={"role": "checkbox", "checked": False})
    current = observation((toggle, _button("0 results"))).model_copy(update={"document_key": "results"})
    page = Mock(spec=Page)

    async def observe() -> Observation:
        return current

    async def act(*args: object) -> ActResult:
        nonlocal current
        box, results = current.controls
        # Every toggle redraws the results, so each state is one the run has never seen.
        redrawn = results.model_copy(update={"label": f"{page.act.await_count} results"})
        current = current.model_copy(
            update={"controls": (box.model_copy(update={"checked": not box.checked}), redrawn)}
        )
        return ActResult(outcome=StepOutcome.EXECUTED, page_changed=True)

    page.observe = AsyncMock(side_effect=observe)
    page.act = AsyncMock(side_effect=act)
    page.screenshot = AsyncMock(return_value=b"")
    llm = ScriptedLLM([{"diagnosis": "The filter is cycling", "next_subgoal": "Read results", "give_up": False}] * 4)
    agent = Agent(
        page,
        ScriptedJev({"operation": "click", "click_target": toggle.id}, noul=0.0),
        llm,
        config=Config(stall=StallRules(max_recoveries=1)),
    )
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    with pytest.raises(_Stop):
        await agent._loop(state, None, None)
    assert len(llm.calls) == 1


def _filtered(checked: bool, nth: int) -> Observation:
    """A results page whose rows redraw on every toggle, so each page state is one never seen before."""
    box = _button("Direct only").model_copy(update={"role": "checkbox", "checked": checked})
    return observation((box, _button(f"{nth} results"))).model_copy(update={"document_key": "results"})


async def _toggle(agent: Agent, page: Mock, state: _RunState, here: Observation, there: Observation) -> None:
    """One click on the setting, settled the way the loop settles it."""
    page.observe = AsyncMock(return_value=there)
    await agent._step(state, here, _code_decision(Operation.CLICK, here.controls[0]))
    agent._note_effect(state, there)
    _, renews, put_back = agent._reversal(state)
    agent._settle(state, there, renews=renews, put_back=put_back)


async def test_a_filter_put_back_to_a_state_its_page_already_held_is_not_progress() -> None:
    """A read between the clicks disarms both loop detectors, which is what the toggling runs actually did."""
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    page.screenshot = AsyncMock(return_value=b"")
    page.artifacts = ()
    llm = ScriptedLLM([{"diagnosis": "The filter is cycling", "next_subgoal": "Read results", "give_up": False}] * 8)
    agent = Agent(page, ScriptedJev({}), llm)
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    here = _filtered(False, 0)
    agent._settle(state, here)
    for nth in range(1, 6):
        there = _filtered(nth % 2 == 1, nth)
        await _toggle(agent, page, state, here, there)
        # A read between the toggles is what made every earlier check treat the return as a comparison.
        state.history.append(
            HistoryEntry(operation=Operation.READ, target=None, outcome=StepOutcome.EXECUTED, page_changed=False)
        )
        here = there
    # The filter only ever holds two states, so every click from the second one puts it back to one seen
    # before. Three of those reach the no-progress tripwire, and the run recovers instead of toggling on to
    # its step limit. Before this, each redrawn results page was a state never seen and nothing counted.
    assert any(purpose is LLMPurpose.RECOVER for purpose, _ in llm.calls)


async def test_a_setting_given_a_value_its_page_has_not_held_is_still_progress() -> None:
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    page.screenshot = AsyncMock(return_value=b"")
    page.artifacts = ()
    llm = ScriptedLLM([{"diagnosis": "d", "next_subgoal": "n", "give_up": False}] * 8)
    agent = Agent(page, ScriptedJev({}), llm)
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    sort = _button("Sort").model_copy(update={"role": "option", "value": "relevance"})
    here = observation((sort,)).model_copy(update={"document_key": "results"})
    agent._settle(state, here)
    for value in ("price", "rating", "distance"):
        there = observation((sort.model_copy(update={"value": value}),)).model_copy(update={"document_key": "results"})
        await _toggle(agent, page, state, here, there)
        state.history.append(
            HistoryEntry(operation=Operation.READ, target=None, outcome=StepOutcome.EXECUTED, page_changed=False)
        )
        here = there
    # Each value is one the page has not held, so none of them is a put-back and the run is left alone.
    assert state.unchanged == 0
    assert not any(purpose is LLMPurpose.RECOVER for purpose, _ in llm.calls)


async def _filtered_fares(*, answer_expected: bool) -> tuple[Agent, Mock, _RunState, ScriptedLLM]:
    """A fare list read unfiltered, then a click on Nonstop only that redraws it, with Jev picking DONE."""
    nonstop = _button("Nonstop only")
    here = observation((nonstop,)).model_copy(update={"document_key": "results"})
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=here)
    page.capture = AsyncMock(return_value=capture((BlockKind.PARAGRAPH, "$320 1 stop")))
    page.screenshot = AsyncMock(return_value=b"")
    page.artifacts = ()

    async def settled(*args: object, **kwargs: object) -> bool:
        await asyncio.sleep(5)  # a real watch on a settled page polls until its deadline
        return False

    page.redrawn = AsyncMock(side_effect=settled)

    async def filtered(*args: object) -> ActResult:
        page.capture.return_value = capture((BlockKind.PARAGRAPH, "$410 nonstop"))
        return ActResult(outcome=StepOutcome.EXECUTED, page_changed=True)

    page.act = AsyncMock(side_effect=filtered)
    llm = ScriptedLLM(
        [
            {
                "claims": [{"text": fare, "cite": {"first": "s0", "last": "s0"}, "requirement_id": "r1"}],
                "answered": True,
            }
            for fare in ("$320 1 stop", "$410 nonstop")
        ]
    )
    agent = Agent(page, ScriptedJev({"operation": "done", "r1": "synthesis"}, noul=0.0), llm)
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    requirements = (Requirement(id="r1", text="Find the cheapest nonstop fare", kind=RequirementKind.INFORMATION),)
    state.ready_plan = Plan(requirements=requirements if answer_expected else (), answer_expected=answer_expected)
    if answer_expected:
        await agent._read(state, await page.capture(), here)
    await agent._step(state, here, _code_decision(Operation.CLICK, nonstop))
    return agent, page, state, llm


async def _finished(agent: Agent, state: _RunState) -> list[str]:
    """Run the loop to its first finish, returning the quotes the done check would have judged."""
    judged: list[str] = []

    async def finish(*args: object) -> agent_module.RunResult:
        judged.extend(e.quote for e in state.notes.evidence.values())
        return agent._result(state, state.ledger, Status.COMPLETE)

    agent._finish = AsyncMock(side_effect=finish)
    await asyncio.wait_for(agent._loop(state, None, None), timeout=1)
    return judged


async def test_an_owed_read_of_unchanged_text_finishes_rather_than_recovering() -> None:
    """A scroll changes the page but not its text, so the owed read repeats one already made."""
    agent, page, state, llm = await _filtered_fares(answer_expected=True)
    page.capture.return_value = capture((BlockKind.PARAGRAPH, "$320 1 stop"))
    await agent._read(state, await page.capture(), await page.observe())
    state.owes_read = True
    assert "$320 1 stop" in await _finished(agent, state)
    assert [purpose for purpose, _ in llm.calls] == [LLMPurpose.READ]


async def test_a_finish_after_an_interaction_reads_what_it_drew() -> None:
    """A run clicked a filter and called itself done on notes read off the list before the filter applied."""
    agent, _, state, llm = await _filtered_fares(answer_expected=True)
    judged = await _finished(agent, state)
    assert [purpose for purpose, _ in llm.calls] == [LLMPurpose.READ, LLMPurpose.READ]
    assert "$410 nonstop" in judged


async def test_a_finish_that_owes_no_answer_neither_reads_nor_waits() -> None:
    """A submit then DONE finishes at once: the redraw watch on a settled page runs to its deadline, and waiting
    on it held every such run for two seconds."""
    agent, page, state, llm = await _filtered_fares(answer_expected=False)
    finish = agent._finish
    await _finished(agent, state)
    llm.responses = [{"missing": [], "complete": True}]
    result = await asyncio.wait_for(finish(state, await page.observe(), None, None), timeout=1)
    assert result is not None and result.status is Status.COMPLETE
    assert LLMPurpose.READ not in [purpose for purpose, _ in llm.calls]


async def test_an_owed_read_is_not_skipped_by_a_budget_the_page_spent_before_the_interaction() -> None:
    """A filter that keeps its address and controls shares the barren budget of the page before it."""
    agent, page, state, llm = await _filtered_fares(answer_expected=True)
    here = await page.observe()
    state.barren[here.document_key, state_key(here), ("r1",)] = agent._config.stall.barren_reads
    assert "$410 nonstop" in await _finished(agent, state)
    assert len(llm.calls) == 2
    assert not state.owes_read


async def test_scrolling_controls_in_and_out_of_view_is_not_a_reversal() -> None:
    top = observation((_button("1"), _button("Search"))).model_copy(update={"document_key": "doc"})
    below = observation((_button("Search"),)).model_copy(update={"document_key": "doc"})
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]))
    state = await run_state()
    agent._settle(state, top)
    for before, after in ((top, below), (below, top), (top, below)):
        await agent._step(state, before, _code_decision(Operation.SCROLL, None))
        agent._note_effect(state, after)
        agent._settle(state, after)
        assert agent._reversal(state) == (None, True, False)


def _wizard_step(label: str, value: str, viewport_text: str = "") -> Observation:
    box = field(label).model_copy(update={"id": label.lower(), "value": value, "role": "textbox"})
    search = field("Search modules").model_copy(update={"id": "search", "value": "", "role": "textbox"})
    controls = (search, box, _button("Back"), _button("Next"))
    return observation(controls).model_copy(update={"document_key": "wizard", "viewport_text": viewport_text})


async def test_stepping_a_wizard_through_values_it_already_holds_is_not_a_setting_put_back() -> None:
    """After a correction, each Next showed a step whose fields held the values typed there before, and three
    Nexts counted as three settings put back tripped the stall recovery on the way to Review."""
    name, city = _wizard_step("First Name", "Priya"), _wizard_step("City", "Manchester")
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]))
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    agent._settle(state, name)
    for here, there, label in ((name, city, "Next"), (city, name, "Back"), (name, city, "Next")):
        await agent._step(state, here, _code_decision(Operation.CLICK, _button(label)))
        agent._note_effect(state, there)
        assert agent._reversal(state) == (None, True, False)


async def test_typing_nothing_into_an_empty_field_goes_to_recovery_rather_than_acting() -> None:
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=False))
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]))
    state = await run_state()
    empty = field("Search modules").model_copy(update={"value": None})
    agent._action = AsyncMock(return_value=Action(operation=Operation.FILL, target_id=empty.id, text=""))
    with pytest.raises(_Unsure):
        await agent._step(state, observation((empty,)), _code_decision(Operation.FILL, empty))
    page.act.assert_not_called()
    assert not state.missing
    # Clearing a field that holds text is an edit.
    await agent._step(state, observation((field(),)), _code_decision(Operation.FILL, field()))
    page.act.assert_awaited_once()


@pytest.mark.parametrize("changed", [False, True])
async def test_a_page_left_unread_is_not_read_on_the_way_back_unless_it_changed(changed: bool) -> None:
    """Walking back through a wizard, Jev called each earlier step evidence, and each read cost 4s."""
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="Find the confirmation", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    state.authorization = Authorization(irreversible_actions=True)
    name, review = _wizard_step("First Name", "Priya", "Step 1"), _wizard_step("City", "Manchester", "Review")
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    page.capture = AsyncMock(return_value=capture((BlockKind.PARAGRAPH, "Step 1")))
    llm = ScriptedLLM([{"claims": [], "answered": False}])
    jev = ScriptedJev({"operation": "click", "click_target": "next", "read_assessment": "evidence", "r1": "synthesis"})
    agent = Agent(page, jev, llm)
    decision = await decide(jev, name, context(), Config())
    await agent._step(state, name, decision)
    await agent._step(state, review, _code_decision(Operation.CLICK, _button("Back")))
    back = name.model_copy(update={"viewport_text": "Step 1, with an error"}) if changed else name
    assert await agent._read_before_interaction(state, back, decision) is changed
    assert len(llm.calls) == changed


async def test_a_page_read_before_is_read_again_when_what_it_evidenced_is_gone() -> None:
    """A wizard's Review was read, the run went back and corrected the name, and the answer named the value Review
    showed before the correction, because nothing read Review again."""
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="Find the name Review shows last", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    review = _wizard_step("City", "Manchester", "Review")
    page = Mock(spec=Page)
    page.screenshot = AsyncMock(return_value=b"")
    page.artifacts = ()
    llm = ScriptedLLM(
        [
            {"claims": [{"text": n, "cite": {"first": "s0", "last": "s0"}, "requirement_id": "r1"}], "answered": True}
            for n in ("Priya Sharma", "Priya Sharman")
        ]
    )
    agent = Agent(page, ScriptedJev({}), llm)
    page.capture = AsyncMock(return_value=capture((BlockKind.PARAGRAPH, "Priya Sharma")))
    await agent._read(state, await page.capture(), review)
    arrived = HistoryEntry(operation=Operation.CLICK, target="Next", outcome=StepOutcome.EXECUTED, page_changed=True)
    state.history.append(arrived)
    assert not await agent._reread_if_changed(state, review)
    page.capture.return_value = capture((BlockKind.PARAGRAPH, "Priya Sharman"))
    state.history.append(arrived)
    assert await agent._reread_if_changed(state, review)
    assert [e.quote for e in state.notes.supporting_evidence("r1")] == ["Priya Sharman"]


@pytest.mark.parametrize("recoveries", [2, 6])
async def test_recovery_prompts_and_requests_remember_the_last_four_diagnoses_redacted(recoveries: int) -> None:
    page = Mock(spec=Page)
    page.screenshot = AsyncMock(return_value=b"")
    llm = ScriptedLLM(
        [
            {"diagnosis": f"Diagnosis {n} hunter2", "next_subgoal": f"Subgoal {n} hunter2", "give_up": False}
            for n in range(recoveries)
        ]
    )
    config = Config(stall=StallRules(max_recoveries=recoveries))
    agent = Agent(page, ScriptedJev({}), llm, config=config)
    agent._redactor.register("password", "hunter2")
    state = await run_state()
    obs = observation((_button("Search"),))
    for n in range(recoveries):
        await agent._recover(state, obs, f"Reason {n} hunter2")
    second = llm.calls[1][1][-1].content
    assert f"recovery 2 of {recoveries} in this stall" in second
    for field in ("Reason", "Diagnosis", "Subgoal"):
        assert f"{field}: {field} 0 [secret:password]" in second
    assert "hunter2" not in second
    request = build_request(obs, obs.controls, agent._context(state, (), check_login=False, check_bot=False), config)
    assert isinstance(request.state, dict)
    memory = request.state["recovery_memory"]
    assert isinstance(memory, str)
    assert f"recovery {recoveries} of {recoveries}" in memory
    for n in range(recoveries):
        for field in ("Reason", "Diagnosis", "Subgoal"):
            assert (f"{field}: {field} {n} [secret:password]" in memory) is (n >= recoveries - 4)
    assert "hunter2" not in memory


@pytest.mark.parametrize(
    ("complete", "missing", "accepted"),
    [
        (True, (), True),
        (False, ("compare",), True),
        (False, ("signed-in",), False),
        (False, ("requests",), False),
        (False, (), False),
    ],
)
def test_the_verifier_cannot_hold_open_a_requirement_the_notes_cite(
    complete: bool, missing: tuple[str, ...], accepted: bool
) -> None:
    info = RequirementKind.INFORMATION
    plan = Plan(
        requirements=(
            Requirement(id="httpx", text="Find httpx's latest release date", kind=info),
            Requirement(id="requests", text="Find requests' latest release date", kind=info),
            Requirement(id="compare", text="Compare the two dates", kind=info),
            Requirement(id="signed-in", text="Be signed in", kind=RequirementKind.ACTION),
        ),
        answer_expected=True,
    )
    notes = Notes(
        Fact(reader=FactReader.LLM, requirement_id=r, text=r, evidence=evidence(start=i))
        for i, r in enumerate(("httpx", "compare"))
    )
    assert _verified(LLMVerdict(complete=complete, missing=missing), plan, notes, set()) is accepted


@pytest.mark.parametrize(
    ("ungrounded", "invented", "accepted", "complete"),
    [
        ((), {"https://example.test/"}, True, True),
        # The citing excusal cannot see this: the requirement has evidence, read on an address the run guessed.
        (("httpx",), {"https://example.test/?q=httpx"}, False, True),
        # Read on the page the caller named or one the run clicked to, the doubt is the verifier's alone. The
        # synthesis requirement is satisfied from notes, and no page ever shows a comparison.
        (("httpx",), set(), True, True),
        (("compare",), set(), True, True),
        # A verifier that calls the run incomplete only for the excused doubt is excused with it, as for missing.
        (("httpx",), set(), True, False),
        # A verdict naming something the plan never asked for says nothing about this run.
        (("invented-id",), {"https://example.test/"}, True, True),
    ],
)
def test_evidence_does_not_excuse_a_requirement_read_off_a_guessed_address(
    ungrounded: tuple[str, ...], invented: set[str], accepted: bool, complete: bool
) -> None:
    """A proposed address opened a flights summary, the reader quoted a price from it, and the requirement
    counted as cited, so the verifier could not hold it open however plainly it was the wrong search."""
    info = RequirementKind.INFORMATION
    plan = Plan(
        requirements=(
            Requirement(id="httpx", text="Find httpx's latest release date", kind=info),
            Requirement(id="compare", text="Compare the two dates", kind=info),
        ),
        answer_expected=True,
    )
    notes = Notes(
        Fact(reader=FactReader.LLM, requirement_id=r, text=r, evidence=evidence(start=i))
        for i, r in enumerate(("httpx", "compare"))
    )
    verdict = LLMVerdict(complete=complete, missing=(), ungrounded=ungrounded)
    assert _verified(verdict, plan, notes, invented) is accepted


async def _finishing(state: _RunState, llm: ScriptedLLM, *, noul: float) -> tuple[Agent, Observation]:
    on = _at("https://example.test/flights/results")
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=on)
    page.screenshot = AsyncMock(return_value=b"")
    page.artifacts = ()
    plan = Plan(
        requirements=(Requirement(id="r1", text="The cheapest nonstop fare", kind=RequirementKind.INFORMATION),),
        answer_expected=False,
    )

    async def planned() -> Generation[Plan]:
        return Generation(data=plan, cost=FREE)

    state.planning = asyncio.create_task(planned())
    await state.planning
    state.ready_plan = plan
    return Agent(page, ScriptedJev({}, noul=noul), llm), on


def _fare(url: str, sha: str) -> Fact:
    read = evidence(sha=sha).model_copy(update={"url": url})
    return Fact(reader=FactReader.LLM, requirement_id="r1", text="$320", evidence=read)


async def test_a_requirement_read_off_a_guessed_address_reopens_until_the_right_page_is_read() -> None:
    """Refused as read off the wrong page with the fare left as the answer, every click after became DONE and
    every DONE the same refusal, told only "completion not confirmed", until the run stopped stuck."""
    summary = "https://example.test/flights/summary"
    state = await run_state()
    state.invented = {summary}
    state.notes.add(_fare(f"{summary}?from=BRS", "summary"))
    recovery: JsonValue = {
        "diagnosis": "the fare came from a summary",
        "next_subgoal": "Run the real search",
        "give_up": False,
    }
    llm = ScriptedLLM(
        [{"missing": [], "ungrounded": ["r1"], "complete": True}, recovery, {"missing": [], "complete": True}]
    )
    agent, on = await _finishing(state, llm, noul=0.5)

    assert await agent._finish(state, on, None, None) is None
    assert state.notes.unresolved(state.plan) == state.plan.requirements
    assert f"r1: The cheapest nonstop fare (read off {summary}?from=BRS" in (state.history[-1].effect or "")

    state.notes.add(_fare("https://example.test/flights/results?from=BRS", "results"))
    result = await agent._finish(state, on, None, None)
    assert result is not None and result.status is Status.COMPLETE


_SHORTCUT = HistoryEntry(operation=None, target=None, outcome=StepOutcome.EXECUTED, page_changed=True)


@pytest.mark.parametrize(
    ("history", "complete"),
    [
        # A run chose DONE on the start page, Jev held the requirement unmet and the verifier called it complete,
        # so it reported complete on the wrong page.
        pytest.param((), False, id="nothing-acted"),
        # A shortcut had already opened the project page; held idle, the run clicked on through to GitHub.
        pytest.param((_SHORTCUT,), True, id="shortcut-opened"),
    ],
)
async def test_only_a_run_that_has_not_acted_is_held_to_an_action_jev_holds_undone(
    history: tuple[HistoryEntry, ...], complete: bool
) -> None:
    state = await run_state()
    state.history.extend(history)
    plan = Plan(
        requirements=(Requirement(id="r1", text="Open httpx's project page", kind=RequirementKind.ACTION),),
        answer_expected=False,
    )
    state.ready_plan = plan
    on = _at("https://pypi.org/")
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=on)
    page.screenshot = AsyncMock(return_value=b"")
    page.artifacts = ()
    llm = ScriptedLLM([{"missing": [], "complete": True}, {"diagnosis": "", "next_subgoal": "", "give_up": False}])
    agent = Agent(page, ScriptedJev({}, noul=0.99), llm)

    result = await agent._finish(state, on, None, None)

    assert (result is not None and result.status is Status.COMPLETE) is complete
    assert llm.calls[0][0] is LLMPurpose.VERIFY


async def test_the_page_a_run_began_on_is_shown_to_the_checks_after_a_shortcut_leaves_it(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """ruff-release: "Start at https://github.com/astral-sh/ruff." was planned as a requirement to navigate there,
    the shortcut opened the release page straight from it, and the verifier rejected DONE because nothing it saw
    said the run had ever been on the repository page."""
    start, release = "https://github.test/astral-sh/ruff", "https://github.test/astral-sh/ruff/releases/tag/0.16.9"
    plan = Plan(
        requirements=(
            Requirement(id="req-1", text=f"Navigate to {start}", kind=RequirementKind.ACTION),
            Requirement(id="req-2", text="Find the latest release of ruff", kind=RequirementKind.INFORMATION),
        ),
        answer_expected=True,
    )

    async def planned(*args: object, **kwargs: object) -> Generation[Plan]:
        return Generation(data=plan, cost=FREE)

    monkeypatch.setattr(agent_module, "make_plan", planned)
    on = _at(release)
    page = Mock(spec=Page)
    page.artifacts = ()
    page.navigate = AsyncMock()
    page.address = AsyncMock(return_value=release)
    page.response_status = AsyncMock(return_value=200)
    page.observe = AsyncMock(return_value=on)
    page.screenshot = AsyncMock(return_value=b"")
    llm = ScriptedLLM(
        [{"missing": ["req-1"], "complete": False}, {"diagnosis": "", "next_subgoal": "", "give_up": False}]
    )
    agent = Agent(page, ScriptedJev({}, noul=0.5), llm)
    monkeypatch.setattr(agent_module, "_propose", AsyncMock(return_value=Shortcut(url=release)))

    async def finish(state: _RunState, output_schema: object, until: object) -> RunResult:
        state.notes.add(_fare(release, "release").model_copy(update={"requirement_id": "req-2", "text": "0.16.9"}))
        await agent._finish(state, on, None, None)
        raise _Stop(Status.STUCK, "checked")

    monkeypatch.setattr(agent, "_loop", finish)
    await agent.run("Start at https://github.test/astral-sh/ruff. What is the latest release?", start=start)

    purpose, messages = llm.calls[0][:2]
    assert purpose is LLMPurpose.VERIFY
    assert f"## Visited addresses\n- {start}\n" in messages[-1].content


def test_visited_addresses_are_redacted_when_shown_and_keep_where_the_run_began() -> None:
    """A value in an early address can become a secret only when a later page asks for it."""
    redactor = Redactor()
    visited = dict.fromkeys(["https://a.test/?user=ada", *(f"https://a.test/{i}" for i in range(5))])
    redactor.register("username", "ada")
    shown = _visited(visited, ObservationLimits(history_entries=1, earlier_history_entries=1), redactor.redact)
    assert shown == ("https://a.test/?user=[secret:username]", "https://a.test/3", "https://a.test/4")


class _ConfirmingJev(ScriptedJev):
    """Confirms every requirement and the task as complete, so the done check accepts outright."""

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        return Evaluation(
            model="test",
            answers={key: NoulAnswer(probability=0.0 if key.startswith("unmet_") else 0.99) for key in questions},
            input_tokens=10,
            cost=FREE,
        )


@pytest.mark.parametrize(
    ("invented", "verified"),
    [
        ("https://example.test/flights/summary?from=BRS", True),
        # A plain page the run built is that page or fails to load; only a search can show the wrong results.
        ("https://example.test/flights/summary", False),
        (None, False),
    ],
)
async def test_a_confident_finish_resting_on_a_guessed_search_is_still_verified(
    invented: str | None, verified: bool
) -> None:
    state = await run_state()
    state.invented = {invented} if invented else set()
    state.notes.add(_fare(invented or "https://example.test/flights/summary", "summary"))
    llm = ScriptedLLM([{"missing": [], "complete": True}])
    agent, on = await _finishing(state, llm, noul=0.99)
    agent._jev = _ConfirmingJev({})

    result = await agent._finish(state, on, None, None)

    assert result is not None and result.status is Status.COMPLETE
    assert [purpose for purpose, _ in llm.calls] == ([LLMPurpose.VERIFY] if verified else [])


@pytest.mark.parametrize("draws", [True, False])
async def test_a_read_waits_for_an_empty_page_to_draw_and_never_reads_nothing(
    monkeypatch: pytest.MonkeyPatch, draws: bool
) -> None:
    monkeypatch.setattr(agent_module, "_INTERSTITIAL_SECONDS", 0.05)
    monkeypatch.setattr(agent_module, "_INTERSTITIAL_POLL_SECONDS", 0.01)
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="When was httpx released?", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    empty = observation(())
    drawn = observation((_button("Search"),)).model_copy(update={"page_key": "drawn"})
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=drawn if draws else empty)
    llm = ScriptedLLM([])
    agent = Agent(page, ScriptedJev({}), llm)
    agent._capture = AsyncMock(side_effect=[capture(), capture((BlockKind.PARAGRAPH, "httpx 0.28.1"))])
    agent._read = AsyncMock(wraps=agent._read)
    decision = await decide(ScriptedJev({"operation": "read"}), empty, context(), Config())
    await agent._step(state, empty, decision)
    assert agent._read.await_args is not None
    read_text = agent._read.await_args.args[1].text
    assert read_text == ("httpx 0.28.1" if draws else "")
    assert llm.calls == []


@pytest.mark.parametrize("twins", [1, 2])
async def test_a_click_on_a_redrawn_control_lands_on_its_one_twin_without_deciding_again(twins: int) -> None:
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    target = _button("Done").model_copy(update={"retarget_key": "same-document-and-guard"})
    before = observation((target,)).model_copy(update={"document_key": "document"})
    redrawn = tuple(target.model_copy(update={"id": f"done-{n}"}) for n in range(twins))
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=before.model_copy(update={"controls": redrawn}))
    page.act = AsyncMock(
        side_effect=[
            ActResult(outcome=StepOutcome.STALE, page_changed=False, detail="target disconnected"),
            ActResult(outcome=StepOutcome.EXECUTED, page_changed=True),
        ]
    )
    await _click(Agent(page, ScriptedJev({}), ScriptedLLM([])), state, before, "Done")
    targets = [call.args[0].target_id for call in page.act.await_args_list]
    assert targets == (["done", "done-0"] if twins == 1 else ["done"])
    assert state.steps[-1].outcome is (StepOutcome.EXECUTED if twins == 1 else StepOutcome.STALE)


@pytest.mark.parametrize(
    "changed",
    ["document_key", "title", "retarget_key", "frame_origin", "submit_semantics", "operations", "value"],
)
async def test_stale_retargeting_preserves_document_and_authorization_context(changed: str) -> None:
    target = field().model_copy(update={"retarget_key": "guard"})
    before = observation((target,)).model_copy(update={"document_key": "document"})
    twin = target.model_copy(update={"id": "replacement"})
    after = before.model_copy(update={"controls": (twin,)})
    if changed in {"document_key", "title"}:
        after = after.model_copy(update={changed: "changed"})
    else:
        twin = twin.model_copy(update={changed: frozenset({Operation.CLICK}) if changed == "operations" else "changed"})
        after = after.model_copy(update={"controls": (twin,)})
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=after)
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]))
    assert await agent._act_on_twin(Action(operation=Operation.ENTER, target_id=target.id), before, target) is None
    page.act.assert_not_called()


@pytest.mark.parametrize("outcome", [StepOutcome.FAILED, StepOutcome.COVERED])
async def test_only_stale_actions_can_be_retargeted(outcome: StepOutcome) -> None:
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    before = observation((_button("Done"),))
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=outcome, page_changed=False))
    await _click(Agent(page, ScriptedJev({}), ScriptedLLM([])), state, before, "Done")
    page.act.assert_awaited_once()
    page.observe.assert_not_called()


def _link(key: str, label: str, href: str) -> Control:
    return Control(id=key, frame_id=None, role="link", label=label, href=href, operations=frozenset({Operation.CLICK}))


def _at(url: str, *controls: Control) -> Observation:
    return observation(controls).model_copy(update={"url": url, "page_key": url})


@pytest.mark.parametrize(
    ("controls", "found"),
    [
        ((_link("n", "Next →", "/page/2/"),), "n"),
        ((_link("n", "next", "/c/mystery/page-2.html"), _link("m", "Mystery", "/c/mystery/index.html")), "n"),
        # A pager above and below the list is one next page.
        ((_link("top", "»", "/page/2/"), _link("bottom", "»", "/page/2/")), "top"),
        # Two different next pages is doubt, left to Jev.
        ((_link("a", "Next", "/page/2/"), _link("b", "Next", "/other/2/")), None),
        # A load-more button keeps the earlier records on the page, so reading again would count them twice.
        ((_button("Next"),), None),
        ((_link("n", "Next", "/list/"),), None),
        ((_link("n", "Next steps for your account", "/help/"),), None),
    ],
)
def test_the_next_page_of_a_list_is_one_link_to_another_address(
    controls: tuple[Control, ...], found: str | None
) -> None:
    control = agent_module.next_page_control(_at("https://example.test/list/", *controls))
    assert (control.id if control else None) == found


async def test_a_list_the_reader_needs_whole_is_read_page_by_page_without_deciding() -> None:
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(
            Requirement(id="r1", text="The cheapest book in Mystery and its price", kind=RequirementKind.INFORMATION),
        ),
        answer_expected=True,
    )
    first = _at("https://example.test/mystery/", _link("next", "next", "/mystery/page-2.html"))
    second = _at("https://example.test/mystery/page-2.html", _link("prev", "previous", "/mystery/"))
    pages = [
        capture((BlockKind.PARAGRAPH, "Sharp Objects £47.82"), (BlockKind.PARAGRAPH, "Page 1 of 2")),
        capture((BlockKind.PARAGRAPH, "Tastes Like Fear £10.69"), (BlockKind.PARAGRAPH, "Page 2 of 2")),
    ]
    reads: list[JsonValue] = [
        # Page one's winner is only the cheapest so far: tagged with the requirement, it must not close it.
        {
            "claims": [
                {
                    "requirement_id": "r1",
                    "text": "Sharp Objects is cheapest",
                    "cite": {"first": "s0", "last": "s0"},
                }
            ],
            "answered": True,
            "continues": [{"requirement_id": "r1", "records": [{"first": "s0", "last": "s0"}]}],
        },
        {
            "claims": [
                {
                    "requirement_id": "r1",
                    "text": "Tastes Like Fear is cheapest at £10.69",
                    "cite": {"first": "s0", "last": "s0"},
                }
            ],
            "answered": True,
        },
    ]
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    llm = ScriptedLLM(reads)
    agent = Agent(page, ScriptedJev({"r1": "none"}), llm)
    agent._capture = AsyncMock(side_effect=pages)
    read = await decide(ScriptedJev({"operation": "read"}), first, context(), Config())

    await agent._step(state, first, read)
    assert not state.notes.evidenced("r1")
    assert state.next_page
    click = agent_module._paging(state, first)
    assert click is not None and click.operation is Operation.CLICK and click.target is not None
    await agent._step(state, first, click, Decider.LLM, gate=False)
    assert page.act.await_args is not None and page.act.await_args.args[0].target_id == "next"

    opened = agent_module._paging(state, second)
    assert opened is not None and opened.operation is Operation.READ
    await agent._step(state, second, opened, Decider.LLM)
    assert state.notes.evidenced("r1")
    assert agent_module._paging(state, second) is None
    assert [(s.operation, s.decided_by) for s in state.steps] == [
        (Operation.READ, Decider.JEV),
        (Operation.CLICK, Decider.LLM),
        (Operation.READ, Decider.LLM),
    ]


async def test_a_list_goes_on_to_jev_with_a_hint_when_code_finds_no_next_page() -> None:
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="How many quotes by Einstein?", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    here = _at("https://example.test/quotes/", _button("Load more"))
    reads: list[JsonValue] = [
        {
            "claims": [],
            "answered": False,
            "continues": [{"requirement_id": "r1", "records": [{"first": "s0", "last": "s0"}]}],
        }
    ]
    agent = Agent(Mock(spec=Page), ScriptedJev({"r1": "none"}), ScriptedLLM(reads))
    await agent._read(state, capture((BlockKind.PARAGRAPH, "Einstein quote")), here)
    assert not state.next_page
    assert state.hint is not None


async def test_the_control_the_reader_names_opens_the_rest_of_the_list() -> None:
    """A reader that can name the control showing the rest gives the run a way forward, where a hint alone
    sent Flights runs round their recovery budget looking for View more flights."""
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="The cheapest fare", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    here = _at("https://example.test/flights/", _button("View more flights"))
    reads: list[JsonValue] = [
        {
            "claims": [],
            "answered": False,
            "continues": [
                {
                    "requirement_id": "r1",
                    "records": [{"first": "s0", "last": "s0"}],
                    "expands": "View more flights",
                }
            ],
        }
    ]
    agent = Agent(Mock(spec=Page), ScriptedJev({"r1": "none"}), ScriptedLLM(reads))
    await agent._read(state, capture((BlockKind.PARAGRAPH, "From 1061 US dollars")), here)
    assert state.directed == (Operation.CLICK, here.controls[0].id)
    assert not state.next_page


async def test_a_read_jev_chose_opens_the_control_its_reader_named() -> None:
    """The direction a read leaves outlives the read: cleared straight after it, Flights runs went to recovery
    and never clicked the View more flights the reader had named."""
    more = _button("View more flights")
    here = _at("https://example.test/flights/", more)
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="The cheapest fare", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=here)
    page.capture = AsyncMock(return_value=capture((BlockKind.PARAGRAPH, "From 1061 US dollars")))
    page.screenshot = AsyncMock(return_value=b"")
    page.artifacts = ()
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    jev = ScriptedJev({"operation": "read", "read_assessment": "evidence", "r1": "none"}, noul=0.0)
    reads: list[JsonValue] = [
        {
            "claims": [],
            "answered": False,
            "continues": [{"requirement_id": "r1", "records": [{"first": "s0", "last": "s0"}], "expands": more.label}],
        }
    ]
    agent = Agent(page, jev, ScriptedLLM(reads))
    agent._recover = AsyncMock(side_effect=_Stop(Status.STUCK, "recovering"))
    state.ledger.limits = Limits(max_steps=3)
    from fastbrowse.telemetry import BudgetExceeded

    with pytest.raises((_Stop, BudgetExceeded)):
        await agent._loop(state, None, None)
    page.act.assert_awaited_once()
    assert [step.target for step in state.steps if step.operation is Operation.CLICK] == [more.label]


async def test_a_control_the_reader_invents_directs_nothing() -> None:
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="The cheapest fare", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    here = _at("https://example.test/flights/", _button("Filters"))
    reads: list[JsonValue] = [
        {
            "claims": [],
            "answered": False,
            "continues": [
                {"requirement_id": "r1", "records": [{"first": "s0", "last": "s0"}], "expands": "Show all 240"}
            ],
        }
    ]
    agent = Agent(Mock(spec=Page), ScriptedJev({"r1": "none"}), ScriptedLLM(reads))
    await agent._read(state, capture((BlockKind.PARAGRAPH, "From 1061 US dollars")), here)
    assert state.directed is None
    assert state.hint is not None


async def test_the_pages_code_opens_are_capped() -> None:
    """Past the cap the reader's continuation goes to Jev with a hint, rather than another silent hop."""
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="How many books?", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    state.pages = Config().max_pages
    here = _at("https://example.test/list/", _link("next", "next", "/list/2"))
    reads: list[JsonValue] = [
        {
            "claims": [],
            "answered": False,
            "continues": [{"requirement_id": "r1", "records": [{"first": "s0", "last": "s0"}]}],
        }
    ]
    agent = Agent(Mock(spec=Page), ScriptedJev({"r1": "none"}), ScriptedLLM(reads))
    await agent._read(state, capture((BlockKind.PARAGRAPH, "a book")), here)
    assert not state.next_page
    assert state.hint is not None
    assert agent_module._paging(state, here) is None


@pytest.mark.parametrize("outcome", [StepOutcome.EXECUTED, StepOutcome.COVERED])
async def test_a_click_that_changed_nothing_is_not_taken_again_from_the_same_page(outcome: StepOutcome) -> None:
    search = _button("Search")
    form = observation((search,))
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=outcome, page_changed=False))
    page.observe = AsyncMock(return_value=form)
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    jev = ScriptedJev({"operation": "click", "click_target": "search"}, noul=0.0)
    agent = Agent(page, jev, ScriptedLLM([]))
    decision = await decide(jev, form, context(), Config())
    await agent._step(state, form, decision)
    assert state.attempts[agent_module._signature(decision, form)].idle
    # From a page that has since changed, the same click is a new try.
    filled = observation((search, field("Return").model_copy(update={"value": "Fri, Oct 23"})))
    assert agent_module._signature(decision, filled) not in state.attempts
    agent._recover = AsyncMock(side_effect=_Stop(Status.STUCK, "recovering"))
    with pytest.raises(_Stop, match="recovering"):
        await agent._loop(state, None, None)
    agent._recover.assert_awaited_once_with(state, form, "click Search already did nothing here")
    page.act.assert_awaited_once()


def test_a_pager_the_page_marks_rel_next_is_followed_whatever_its_label() -> None:
    # A pager drawn as an icon, or in another language, says so only in the markup.
    icon = _link("n", "→→→", "/page/2/").model_copy(update={"label": "Weiter", "next_page": True})
    found = agent_module.next_page_control(_at("https://example.test/list/", icon))
    assert found is not None and found.id == "n"


@pytest.mark.parametrize(("frames", "sent"), [(True, b"png"), (False, None)])
async def test_a_step_carries_the_page_it_acted_on_when_frames_are_asked_for(frames: bool, sent: bytes | None) -> None:
    events: list[StepEvent] = []

    async def collect(event: StepEvent | BrowserEvent) -> None:
        if isinstance(event, StepEvent):
            events.append(event)

    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=observation((_button("Done"),)))
    page.screenshot = AsyncMock(return_value=b"png")
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]), config=Config(step_frames=frames), on_event=collect)
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    await _click(agent, state, observation((_button("Done"),)), "Done")
    assert [event.frame for event in events] == [sent]


async def test_a_secret_the_step_itself_put_on_the_page_suppresses_its_frame() -> None:
    """The page before the action is not evidence about the page after it: the fill may be what revealed it."""
    events: list[StepEvent] = []

    async def collect(event: StepEvent | BrowserEvent) -> None:
        if isinstance(event, StepEvent):
            events.append(event)

    save = _button("Save")
    # The page mirrors what was typed into ordinary text, which only the observation AFTER the step shows.
    mirrored = observation((save,)).model_copy(update={"viewport_text": "signed in as hunter2"})
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=mirrored)
    page.screenshot = AsyncMock(return_value=b"png")
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]), config=Config(step_frames=True), on_event=collect)
    agent._redactor.register("password", "hunter2")
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)

    await _click(agent, state, observation((save,)), "Save")
    assert [event.frame for event in events] == [None]
    page.screenshot.assert_not_awaited()
    page.withhold_frames.assert_called_with(True)


def test_a_plan_is_answered_when_what_it_asks_to_find_is_evidenced_whatever_actions_it_lists() -> None:
    plan = Plan(
        requirements=(
            Requirement(id="r1", text="Count the books on the first page", kind=RequirementKind.INFORMATION),
            Requirement(id="r2", text="Open the next page", kind=RequirementKind.ACTION),
            Requirement(id="r3", text="Count the books on the next page", kind=RequirementKind.INFORMATION),
        ),
        answer_expected=True,
    )
    notes = Notes()
    assert not _answered(plan, notes)
    notes.add(Fact(reader=FactReader.LLM, requirement_id="r1", text="20 books", evidence=evidence(start=0, end=4)))
    assert not _answered(plan, notes)
    notes.add(Fact(reader=FactReader.LLM, requirement_id="r3", text="20 books", evidence=evidence(start=10, end=14)))
    assert _answered(plan, notes)


def test_a_task_of_only_actions_is_never_answered() -> None:
    plan = Plan(
        requirements=(Requirement(id="r1", text="Open page 3 of the results", kind=RequirementKind.ACTION),),
        answer_expected=False,
    )
    assert not _answered(plan, Notes())


class DoubtingJev(ScriptedJev):
    """Doubts any claim that mentions the whole list, and accepts a claim about one record."""

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        answers: dict[str, Answer] = {
            key: NoulAnswer(probability=0.9 if "WHOLE LIST" in question.instructions else 0.05)
            for key, question in questions.items()
            if isinstance(question, NoulQuestion)
        }
        return Evaluation(model="test", answers=answers, input_tokens=10, cost=FREE)


async def test_a_composed_answer_that_fails_its_check_falls_back_to_the_readers_quoted_facts() -> None:
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="List the books", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    first, second = evidence(start=0, end=4), evidence(start=10, end=14)
    state.notes.add(Fact(reader=FactReader.LLM, requirement_id="r1", text="Book A is listed", evidence=first))
    state.notes.add(Fact(reader=FactReader.LLM, requirement_id="r1", text="Book B is listed", evidence=second))
    whole: JsonValue = {"claims": [{"text": "WHOLE LIST: Book A and Book B", "evidence_ids": [evidence_id(first)]}]}
    agent = Agent(Mock(spec=Page), DoubtingJev({}), ScriptedLLM([whole]))

    composed, verified = await agent._answer(state, None)
    answer = composed.answer

    assert verified
    assert "WHOLE LIST" not in answer and "Book A is listed" in answer and "Book B is listed" in answer


async def test_a_bot_check_stops_the_run_even_where_a_secret_is_held_for_the_site() -> None:
    """A credential makes a sign-in wall work to do; no credential passes a CAPTCHA, so the check still runs."""
    captcha = _at("https://shop.test/login", _button("Verify you are human"))
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=captcha)
    page.artifacts = ()
    jev = ScriptedJev({}, noul=0.9)
    agent = Agent(
        page,
        jev,
        ScriptedLLM([{"requirements": [], "answer_expected": False}]),
        secrets=ScopedSecrets({"PASSWORD": "hunter2"}, "https://shop.test"),
    )
    agent._outwait = AsyncMock(return_value=False)

    result = await agent.run("Sign in and open my orders", limits=Limits(max_steps=2))

    assert result.status is Status.BLOCKED
    assert result.error is not None and "bot check" in result.error
    asked = jev.requests[0]
    assert "bot_check" in asked
    # The sign-in question is the one a held credential answers, so it is not asked.
    assert "login_required" not in asked


async def test_a_decision_dropped_for_a_redraw_still_asks_the_bot_check_again() -> None:
    captcha = _at("https://shop.test/login", _button("Verify you are human"))
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=captcha)
    page.redrawn = AsyncMock(side_effect=[True, False])
    page.artifacts = ()

    class SlowFirstJev(ScriptedJev):
        async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
            if not self.requests:
                await asyncio.sleep(0.1)  # still deciding when the redraw is seen
            return await super().evaluate(state, questions)

    jev = SlowFirstJev({}, noul=0.9)
    agent = Agent(page, jev, ScriptedLLM([{"requirements": [], "answer_expected": False}]))
    agent._outwait = AsyncMock(return_value=False)

    result = await agent.run("Open my orders", limits=Limits(max_steps=3))

    assert result.status is Status.BLOCKED
    assert all("bot_check" in asked for asked in jev.requests)


@pytest.mark.parametrize(
    ("proposed", "opened"),
    [
        ("https://news.ycombinator.com/", "https://news.ycombinator.com/"),
        # A run that began at `file:` or `javascript:` would be reading this process, not the web.
        ("file:///etc/passwd", None),
        (None, None),
    ],
)
async def test_a_run_with_no_page_named_works_the_first_address_out_of_the_task(
    proposed: str | None, opened: str | None
) -> None:
    page = Mock(spec=Page)
    page.navigate = AsyncMock()
    page.origin = AsyncMock(return_value="https://news.ycombinator.com")
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([{"url": proposed}]))
    ledger = Ledger(Limits())
    if opened is None:
        with pytest.raises(_Stop) as stopped:
            await agent._first_page("What is the top story?", ledger)
        assert stopped.value.status is Status.NEEDS_INPUT
    else:
        assert await agent._first_page("What is the top story?", ledger) == opened


@pytest.mark.parametrize(("status", "stays"), [(404, False), (200, True), (None, True)])
async def test_a_shortcut_the_site_does_not_serve_returns_to_the_start_page(status: int | None, stays: bool) -> None:
    start, guessed = "https://books.test/", "https://books.test/catalogue/mysteryfile_3/index.html"
    page = Mock(spec=Page)
    page.navigate = AsyncMock()
    # The site redirects the guess, and the facts will cite where it landed.
    landed = "https://books.test/catalogue/category/books/mystery_3/index.html"
    page.address = AsyncMock(return_value=landed)
    page.response_status = AsyncMock(return_value=status)
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([{"url": guessed}]))

    history, invented = await agent._open("Which is the cheapest mystery book?", start, Ledger(Limits()), None)

    assert bool(history) is stays
    assert page.navigate.await_args_list[-1].args == ((guessed,) if stays else (start,))
    # The guess is opened in place of the start page, which stays one BACK away rather than being loaded first.
    assert page.navigate.await_args_list[0].args == (guessed,)
    assert page.navigate.await_args_list[0].kwargs == {"back_to": start}
    # Only an address the run actually stayed on is one the verifier has to weigh.
    assert invented == ({guessed, landed} if stays else set())


async def test_a_shortcut_begun_before_the_browser_is_opened_without_being_asked_for_again() -> None:
    """`run_task` asks for the shortcut while the browser starts, so the tab opens straight onto it."""
    start, guessed = "https://books.test/", "https://books.test/catalogue/"
    page = Mock(spec=Page)
    page.navigate = AsyncMock()
    page.address = AsyncMock(return_value=guessed)
    page.response_status = AsyncMock(return_value=200)
    llm = ScriptedLLM([{"url": guessed}])
    ledger = Ledger(Limits())
    proposing = asyncio.create_task(agent_module._propose(llm, "Which is the cheapest book?", start, ledger))
    await proposing

    history, _ = await Agent(page, ScriptedJev({}), ScriptedLLM([]))._open(
        "Which is the cheapest book?", start, ledger, proposing
    )

    assert history and page.navigate.await_args_list[0].args == (guessed,)
    assert len(llm.calls) == 1 and len(ledger.lines) == 1


async def test_a_head_start_a_run_never_took_bills_what_finished_and_cancels_the_rest() -> None:
    """A browser that fails to start ends the run before it begins; a plan already written was still paid for."""
    head = HeadStart.begin(ScriptedLLM([{"requirements": [], "answer_expected": True}]), "What is the top story?")
    await head.planning
    proposing = head.proposing = asyncio.create_task(asyncio.Event().wait())  # ty: ignore[invalid-assignment]

    lines = await head.abandon()

    assert len(lines) == 1 and proposing.cancelled()


@pytest.mark.parametrize(
    ("task", "start", "limits"),
    [
        ("Another task", "https://news.test/", None),
        ("What is the top story?", None, None),
        ("What is the top story?", "https://news.test/", Limits(max_steps=3)),
    ],
)
async def test_a_head_start_begun_for_another_run_is_refused(
    task: str, start: str | None, limits: Limits | None
) -> None:
    """Its plan would be followed, and its limits enforced, without complaint."""
    head = HeadStart.begin(
        ScriptedLLM([{"requirements": [], "answer_expected": True}, {"url": None}]),
        "What is the top story?",
        start="https://news.test/",
    )
    try:
        with pytest.raises(ValueError, match="head start"):
            await Agent(Mock(spec=Page), ScriptedJev({}), ScriptedLLM([])).run(
                task, start=start, limits=limits, head_start=head
            )
    finally:
        await head.discard()


@pytest.mark.parametrize(
    ("opened", "controls", "text", "goes_to"),
    [
        # The case from a live run: a path the site does not serve, read as an empty page.
        ("https://shop.test/login", (), "", "https://shop.test"),
        # A path that served something is the page the run wanted.
        ("https://shop.test/login", (), "Sign in to continue", None),
        ("https://shop.test/login", (_button("Login"),), "", None),
        # Nothing to fall back to: this is already the front page.
        ("https://shop.test/", (), "", None),
    ],
)
async def test_a_start_page_this_run_guessed_falls_back_to_the_front_page_when_it_opens_nothing(
    opened: str, controls: tuple[Control, ...], text: str, goes_to: str | None, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The wait for a page that might still be drawing is covered below; here it only has to elapse.
    monkeypatch.setattr(agent_module, "_INTERSTITIAL_SECONDS", 0.05)
    monkeypatch.setattr(agent_module, "_INTERSTITIAL_POLL_SECONDS", 0.01)
    page = Mock(spec=Page)
    page.navigate = AsyncMock()
    page.observe = AsyncMock(
        return_value=observation(controls).model_copy(update={"url": opened, "viewport_text": text})
    )
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]))

    await agent._front_page_if_blank(opened)

    if goes_to is None:
        page.navigate.assert_not_called()
    else:
        page.navigate.assert_awaited_once_with(goes_to)


async def test_a_guessed_page_that_is_still_drawing_is_waited_for_rather_than_abandoned(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Navigation returns at `readyState`; a script-built page has not drawn yet. The route may be right."""
    monkeypatch.setattr(agent_module, "_INTERSTITIAL_POLL_SECONDS", 0.01)
    blank = _at("https://app.test/dashboard").model_copy(update={"page_key": "loading"})
    drawn = _at("https://app.test/dashboard", _button("Sign out")).model_copy(update={"page_key": "drawn"})
    page = Mock(spec=Page)
    page.navigate = AsyncMock()
    page.observe = AsyncMock(side_effect=[blank, drawn, drawn])
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]))

    await agent._front_page_if_blank("https://app.test/dashboard")

    page.navigate.assert_not_called()


async def test_a_recovery_spends_the_evidence_every_tripwire_read() -> None:
    """Armed, a tripwire must not re-fire on the crossing recovery already handled.

    `history` never shrinks and `plan_marks` grows only on a step that got nowhere, so a threshold crossed
    once holds for the rest of the run. Leaving it standing meant recovery re-entered on every later step,
    however well the run then went, and `max_recoveries` returned STUCK three steps later.
    """
    page = Mock(spec=Page)
    page.screenshot = AsyncMock(return_value=b"png")
    recovery: dict[str, JsonValue] = {
        "diagnosis": "going round",
        "next_subgoal": "Click Search",
        "give_up": False,
        "control": None,
    }
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([recovery]))
    state = await run_state()
    state.history = [
        HistoryEntry(operation=Operation.FILL, target="Search", outcome=StepOutcome.EXECUTED, page_changed=True)
        for _ in range(3)
    ]
    state.plan_marks = ["same"] * 4

    tripped = {t.tripwire for t in agent._tripwires(state)}
    assert Tripwire.ACTION_REPETITION in tripped and Tripwire.PLAN_STAGNATION in tripped

    await agent._recover(state, observation(()), "action_repetition (3)")
    assert agent._tripwires(state) == []


def edit(
    value: str, *, holds: str | None = None, secret: bool = False, operation: Operation = Operation.FILL
) -> tuple[Decision, Action]:
    """A decision to write `value` into a field that currently holds `holds`, and the action doing it."""
    target = Control(
        id="f",
        frame_id=None,
        role="textbox",
        label="Name",
        value=holds,
        operations=frozenset({Operation.FILL}),
    )
    decision = _code_decision(operation, target)
    return decision, Action(operation=operation, target_id=target.id, text=value, secret=secret)


def test_writing_a_value_a_field_already_holds_is_not_progress() -> None:
    """The invariant is read off the field, so the same value in a DIFFERENT empty field still counts.

    A checkout writes one name into billing and the same name into shipping, and the second write is the whole
    point. A remembered (operation, label, value) key called it a repeat; the field's own value does not.
    """
    assert Agent._edit_progress(*edit("Ada", holds="Ada"), set()) is False
    # A field's first value is progress: a checkout's three fields changed no page fingerprint and tripped the
    # stall recovery. Only the first: every new value credited let a PyPI run grind on one search box.
    assert Agent._edit_progress(*edit("Ada", holds=None), set()) is True
    assert Agent._edit_progress(*edit("Ada", holds=""), {"f"}) is None
    # A corrected value is not vetoed, even though the field is not empty.
    assert Agent._edit_progress(*edit("Ada", holds="Adz"), {"f"}) is None


def test_a_secret_is_compared_by_the_length_the_page_reveals() -> None:
    """A password's value never leaves the page: only bullets of its length are observed."""
    assert Agent._edit_progress(*edit("hunter2", holds="\u2022" * 7, secret=True), set()) is False
    assert Agent._edit_progress(*edit("hunter2", holds="\u2022" * 4, secret=True), {"f"}) is None


def test_an_upload_is_judged_by_the_page_not_by_a_value() -> None:
    """A file input's value is not the file, so every upload after the first read as the same nothing."""
    assert Agent._edit_progress(*edit("a.pdf", holds=None, operation=Operation.UPLOAD), set()) is None


def _ticker(nth: int) -> Capture:
    """One observation of a page that rewrites its own text every time it is looked at."""
    return capture((BlockKind.PARAGRAPH, f"Live results, updated {nth} seconds ago"))


async def _reading_state() -> _RunState:
    state = await run_state()
    requirement = Requirement(id="r1", text="Find the total", kind=RequirementKind.INFORMATION)
    state.ready_plan = Plan(requirements=(requirement,), answer_expected=True)
    return state


async def test_a_read_carries_incomplete_comparisons_to_later_reads(monkeypatch: pytest.MonkeyPatch) -> None:
    state = await _reading_state()
    llm = ScriptedLLM(
        [
            {
                "claims": [],
                "answered": False,
                "continues": [{"requirement_id": "r1", "records": [{"first": "s9", "last": "s9"}]}],
            },
            {"claims": [], "answered": False},
            {
                "claims": [{"text": "The total is 12", "cite": {"first": "s0", "last": "s0"}, "requirement_id": "r1"}],
                "answered": True,
            },
        ]
    )
    calls: list[set[str]] = []
    original = agent_module.read

    async def reading(*args, **kwargs):
        calls.append(set(kwargs.get("incomplete", ())))
        return await original(*args, **kwargs)

    monkeypatch.setattr(agent_module, "read", reading)
    agent = Agent(Mock(spec=Page), ScriptedJev({"r1": "synthesis"}), llm)
    for text in ("First page", "Second page", "The total is 12"):
        await agent._read(state, capture((BlockKind.PARAGRAPH, text)))

    assert calls == [set(), {"r1"}, {"r1"}]
    assert state.incomplete == {"r1"}
    assert not state.notes.evidenced("r1")


async def test_stale_clicks_exceed_the_step_limit_but_still_stall() -> None:
    target = _button("Continue")
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=observation((target,)))
    page.redrawn = AsyncMock(return_value=False)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.STALE, page_changed=False))
    page.artifacts = ()
    llm = ScriptedLLM([{"requirements": [], "answer_expected": False}])
    agent = Agent(
        page,
        ScriptedJev({"operation": "click", "click_target": target.id}, noul=0.0),
        llm,
        config=Config(stall=StallRules(unchanged_actions=4, max_recoveries=0)),
    )

    result = await agent.run("Continue", limits=Limits(max_steps=2))

    assert result.status is Status.STUCK
    assert len(result.steps) == 4
    assert [step.index for step in result.steps] == list(range(4))
    assert all(step.outcome is StepOutcome.STALE for step in result.steps)


@pytest.mark.parametrize(
    ("operation", "outcome"),
    [
        (Operation.CLICK, StepOutcome.EXECUTED),
        (Operation.ENTER, StepOutcome.EXECUTED),
        (Operation.CLICK, StepOutcome.STALE),
        (Operation.HOVER, StepOutcome.EXECUTED),
    ],
)
async def test_only_executed_committing_actions_supply_transaction_evidence_to_the_answer(
    monkeypatch: pytest.MonkeyPatch, operation: Operation, outcome: StepOutcome
) -> None:
    state = await _reading_state()
    state.authorization = Authorization(irreversible_actions=True)
    target = _button("Pay").model_copy(
        update={"operations": frozenset({Operation.CLICK, Operation.ENTER, Operation.HOVER})}
    )
    checkout = _at("https://shop.test/checkout", target)
    receipt = _at("https://shop.test/receipt")
    for index, (url, quote) in enumerate(
        (("https://shop.test/search", "Black pen £7"), (checkout.url, "Red pen £11.55"), (receipt.url, "Paid £11.55"))
    ):
        state.notes.add(
            Fact(
                reader=FactReader.LLM,
                requirement_id="r1" if index == 0 else None,
                text=quote,
                evidence=evidence(sha=str(index)).model_copy(update={"url": url, "quote": quote}),
            )
        )
    listed, *committed = state.notes.evidence
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=receipt)
    page.redrawn = AsyncMock(return_value=False)
    page.act = AsyncMock(return_value=ActResult(outcome=outcome, page_changed=outcome is StepOutcome.EXECUTED))
    jev = TransactionJev(0.9)
    llm = ScriptedLLM([{"claims": [{"text": "Bought a black pen for £7", "evidence_ids": [listed]}]}])
    composing = AsyncMock(wraps=agent_module.compose)
    monkeypatch.setattr(agent_module, "compose", composing)
    agent = Agent(page, jev, llm)
    decision = _code_decision(operation, target)

    await agent._step(state, checkout, decision)
    agent._note_effect(state, receipt)
    agent._note_effect(state, _at("https://shop.test/help"))
    _, held = await agent._answer(state, None)

    committing = operation in {Operation.CLICK, Operation.ENTER} and outcome is StepOutcome.EXECUTED
    assert len(state.transaction_candidates) == len(jev.classifications) == int(committing)
    assert held and composing.await_args is not None
    assert composing.await_args.kwargs["transaction_evidence_ids"] == (tuple(committed) if committing else ())
    questions = {key: question for request in jev.requests for key, question in request.items()}
    assert (TRANSACTION_CONTRADICTED in questions) is committing
    if committing:
        assert "Red pen £11.55" in questions[TRANSACTION_CONTRADICTED].instructions
        assert "Paid £11.55" in questions[TRANSACTION_CONTRADICTED].instructions


class TransactionJev(ScriptedJev):
    def __init__(self, probability: float) -> None:
        super().__init__({}, noul=0.0)
        self.probability = probability
        self.classifications: list[tuple[JsonValue, Mapping[str, Question]]] = []

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        result = await super().evaluate(state, questions)
        if isinstance(state, dict) and "pages" in state:
            self.classifications.append((state, questions))
            return result.model_copy(
                update={"answers": {key: NoulAnswer(probability=self.probability) for key in questions}}
            )
        return result


async def _transaction_run(
    monkeypatch: pytest.MonkeyPatch, *, authorized: bool = True, probability: float = 0.9
) -> tuple[_RunState, Agent, TransactionJev, AsyncMock]:
    state = await _reading_state()
    state.authorization = Authorization(irreversible_actions=authorized)
    state.task = "Buy a pen"
    for index, (path, quote) in enumerate(
        (("search", "Black pen £7"), ("checkout", "Red pen £11.55"), ("receipt", "Paid £11.55"))
    ):
        state.notes.add(
            Fact(
                reader=FactReader.LLM,
                requirement_id="r1" if index == 0 else None,
                text=quote,
                evidence=evidence(sha=str(index)).model_copy(
                    update={"url": f"https://shop.test/{path}", "quote": quote}
                ),
            )
        )
    listed = next(iter(state.notes.evidence))
    page = Mock(spec=Page)
    page.redrawn = AsyncMock(return_value=False)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    jev = TransactionJev(probability)
    llm = ScriptedLLM([{"claims": [{"text": "Bought a black pen for £7", "evidence_ids": [listed]}]}] * 3)
    composing = AsyncMock(wraps=agent_module.compose)
    monkeypatch.setattr(agent_module, "compose", composing)
    return state, Agent(page, jev, llm), jev, composing


async def test_reversible_navigation_contributes_no_transaction_evidence(monkeypatch: pytest.MonkeyPatch) -> None:
    state, agent, jev, composing = await _transaction_run(monkeypatch, probability=0.1)
    await _click(agent, state, _at("https://shop.test/search", _button("Details")), "Details")
    agent._note_effect(state, _at("https://shop.test/checkout"))

    _, held = await agent._answer(state, None)

    assert held and composing.await_args is not None
    assert composing.await_args.kwargs["transaction_evidence_ids"] == ()
    assert len(jev.classifications) == 1
    assert all(TRANSACTION_CONTRADICTED not in questions for questions in jev.requests)


async def test_unauthorized_navigation_records_no_candidates_or_extra_calls(monkeypatch: pytest.MonkeyPatch) -> None:
    state, agent, jev, composing = await _transaction_run(monkeypatch, authorized=False)
    await _click(agent, state, _at("https://shop.test/search", _button("Details")), "Details")
    agent._note_effect(state, _at("https://shop.test/checkout"))

    _, held = await agent._answer(state, None)

    assert held and composing.await_args is not None
    assert composing.await_args.kwargs["transaction_evidence_ids"] == ()
    assert not state.transaction_candidates
    assert len(jev.requests) == 2
    assert not jev.classifications
    assert all(TRANSACTION_CONTRADICTED not in questions for questions in jev.requests)


@pytest.mark.parametrize("kind", ["confirm", "prompt", "beforeunload"])
async def test_committing_click_and_dialog_include_checkout_and_receipt(
    monkeypatch: pytest.MonkeyPatch, kind: str
) -> None:
    state, agent, jev, composing = await _transaction_run(monkeypatch)
    checkout = _at("https://shop.test/checkout", _button("Buy"))
    dialog = checkout.model_copy(update={"dialog": Dialog(kind=kind, message="Pay £11.55?")})
    await _click(agent, state, checkout, "Buy")
    agent._note_effect(state, dialog)
    await agent._step(state, dialog, _code_decision(Operation.DIALOG, None))
    agent._note_effect(state, _at("https://shop.test/receipt"))
    agent._note_effect(state, _at("https://shop.test/help"))

    _, held = await agent._answer(state, None)

    assert held and composing.await_args is not None
    assert composing.await_args.kwargs["transaction_evidence_ids"] == tuple(state.notes.evidence)[1:]
    assert len(jev.classifications) == 1
    pages, questions = jev.classifications[0]
    assert pages == {"pages": [{"url": checkout.url}, {"url": "https://shop.test/receipt"}]}
    assert len(questions) == 2
    assert any(f"{kind} dialog" in question.instructions for question in questions.values())
    transaction = next(q[TRANSACTION_CONTRADICTED] for q in jev.requests if TRANSACTION_CONTRADICTED in q)
    assert "Red pen £11.55" in transaction.instructions and "Paid £11.55" in transaction.instructions


async def test_candidates_without_evidence_are_classified_only_when_read(monkeypatch: pytest.MonkeyPatch) -> None:
    state, agent, jev, composing = await _transaction_run(monkeypatch)
    await _click(agent, state, _at("https://shop.test/unread", _button("Buy")), "Buy")
    agent._note_effect(state, _at("https://shop.test/unread-receipt"))
    await agent._answer(state, None)
    assert not jev.classifications
    assert composing.await_args is not None
    assert composing.await_args.kwargs["transaction_evidence_ids"] == ()

    receipt = evidence(sha="later").model_copy(update={"url": "https://shop.test/unread-receipt", "quote": "Paid £7"})
    state.notes.add(Fact(reader=FactReader.LLM, text=receipt.quote, evidence=receipt))
    await agent._answer(state, None)

    assert len(jev.classifications) == 1
    assert composing.await_args.kwargs["transaction_evidence_ids"] == (evidence_id(receipt),)


@pytest.mark.parametrize("probability", [0.1, 0.9])
async def test_transaction_verdicts_are_cached_across_answer_attempts(
    monkeypatch: pytest.MonkeyPatch, probability: float
) -> None:
    state, agent, jev, composing = await _transaction_run(monkeypatch, probability=probability)
    await _click(agent, state, _at("https://shop.test/checkout", _button("Buy")), "Buy")
    agent._note_effect(state, _at("https://shop.test/receipt"))

    await agent._answer(state, None)
    await agent._answer(state, None)

    assert len(jev.classifications) == 1
    ids = tuple(state.notes.evidence)[1:] if probability > Config().thresholds.irreversible_above else ()
    assert [call.kwargs["transaction_evidence_ids"] for call in composing.await_args_list] == [ids, ids]
    assert state.ledger.jev_calls == len(jev.requests) == (5 if ids else 3)
    assert state.ledger.lines.count(FREE) == len(jev.requests)


async def test_an_unclassified_candidate_still_supplies_transaction_evidence(monkeypatch: pytest.MonkeyPatch) -> None:
    state, agent, jev, composing = await _transaction_run(monkeypatch)
    failing = AsyncMock(side_effect=JevError("service unavailable"))
    original = jev.evaluate

    async def evaluate(state_: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        if isinstance(state_, dict) and "pages" in state_:
            return await failing(state_, questions)
        return await original(state_, questions)

    monkeypatch.setattr(jev, "evaluate", evaluate)
    await _click(agent, state, _at("https://shop.test/checkout", _button("Buy")), "Buy")
    agent._note_effect(state, _at("https://shop.test/receipt"))

    await agent._answer(state, None)
    await agent._answer(state, None)

    assert failing.await_count > 1, "a failed classification is not cached"
    ids = tuple(state.notes.evidence)[1:]
    assert [call.kwargs["transaction_evidence_ids"] for call in composing.await_args_list] == [ids, ids]
    assert any(TRANSACTION_CONTRADICTED in questions for questions in jev.requests)


async def test_a_page_that_rewrites_its_own_text_is_read_only_while_it_pays_out() -> None:
    """The exact-content key never matches on a ticker, so without a budget the run reads it for ever."""
    state = await _reading_state()
    here = _at("https://example.test/live/", _button("Refresh"))
    llm = ScriptedLLM([{"claims": [], "answered": False}] * 6)
    agent = Agent(Mock(spec=Page), ScriptedJev({"r1": "synthesis"}), llm)
    skipped = []
    for nth in range(5):
        _, was_skipped = await agent._read(state, _ticker(nth), here)
        skipped.append(was_skipped)
    assert len(llm.calls) == Config().stall.barren_reads
    assert skipped == [False, False, True, True, True]


async def test_a_read_that_pays_out_restores_the_budget_of_the_page_state_it_read() -> None:
    state = await _reading_state()
    here = _at("https://example.test/live/", _button("Refresh"))
    paid: JsonValue = {"claims": [{"text": "Total: 12", "cite": {"first": "s0", "last": "s0"}}], "answered": False}
    barren: JsonValue = {"claims": [], "answered": False}
    llm = ScriptedLLM([barren, paid, barren, barren])
    agent = Agent(Mock(spec=Page), ScriptedJev({"r1": "synthesis"}), llm)
    for nth in range(3):
        await agent._read(state, _ticker(nth), here)
    assert state.notes.facts, "the second read added a fact"
    # Without the payout clearing the first barren read, the third would have spent the budget.
    _, was_skipped = await agent._read(state, _ticker(3), here)
    assert not was_skipped
    assert len(llm.calls) == 4


async def test_a_spent_read_budget_belongs_to_one_page_state_not_to_the_run() -> None:
    state = await _reading_state()
    llm = ScriptedLLM([{"claims": [], "answered": False}] * 4)
    agent = Agent(Mock(spec=Page), ScriptedJev({"r1": "synthesis"}), llm)
    here = _at("https://example.test/live/", _button("Refresh"))
    for nth in range(3):
        await agent._read(state, _ticker(nth), here)
    assert len(llm.calls) == Config().stall.barren_reads
    # A control the earlier state did not offer is a page state of its own, with a budget of its own.
    changed = _at("https://example.test/live/", _button("Refresh"), _button("Show all"))
    _, was_skipped = await agent._read(state, _ticker(3), changed)
    assert not was_skipped
    assert len(llm.calls) == Config().stall.barren_reads + 1


async def test_a_list_paged_in_place_gives_each_page_its_own_read_budget() -> None:
    """A client-side pager keeps its document and its buttons, so only the address tells the pages apart."""
    state = await _reading_state()
    llm = ScriptedLLM([{"claims": [], "answered": False}] * 3)
    agent = Agent(Mock(spec=Page), ScriptedJev({"r1": "synthesis"}), llm)
    skipped = []
    for nth in (1, 2, 3):
        here = _at(f"https://example.test/orders?page={nth}", _button("Previous"), _button("Next"))
        _, was_skipped = await agent._read(state, capture((BlockKind.PARAGRAPH, f"Order {nth}00")), here)
        skipped.append(was_skipped)
    assert skipped == [False, False, False]
    assert len(llm.calls) == 3


async def test_rereading_the_same_records_off_a_ticking_page_is_not_payout() -> None:
    """Each capture of a ticking page mints the records it quotes afresh, though the notes already hold them."""
    state = await _reading_state()
    here = _at("https://example.test/flights/", _button("Refresh"))
    carried: JsonValue = {
        "claims": [],
        "answered": False,
        "continues": [{"requirement_id": "r1", "records": [{"first": "s0", "last": "s0"}]}],
    }
    llm = ScriptedLLM([carried] * 6)
    agent = Agent(Mock(spec=Page), ScriptedJev({"r1": "synthesis"}), llm)
    for nth in range(6):
        flights = capture(
            (BlockKind.PARAGRAPH, "AB12 London to Paris $410"),
            (BlockKind.PARAGRAPH, f"Prices updated {nth} seconds ago"),
        )
        await agent._read(state, flights, here)
    # The first read's records are new to the notes and pay out; the same records again do not.
    assert len(llm.calls) == Config().stall.barren_reads + 1


async def test_a_starved_read_lets_the_interaction_jev_chose_proceed() -> None:
    """The point of the budget: the run stops reading the ticker and does the thing the task needs."""
    state = await _reading_state()
    refresh = _button("Refresh")
    here = _at("https://example.test/live/", refresh)
    jev = ScriptedJev(
        {"operation": "click", "click_target": refresh.id, "read_assessment": "evidence", "r1": "synthesis"}
    )
    llm = ScriptedLLM([{"claims": [], "answered": False}] * 6)
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=here)
    page.capture = AsyncMock(return_value=_ticker(99))
    page.screenshot = AsyncMock(return_value=b"")
    page.artifacts = ()
    agent = Agent(page, jev, llm)
    for nth in range(Config().stall.barren_reads):
        await agent._read(state, _ticker(nth), here)
    clicking = await decide(jev, here, context(), Config())
    assert clicking.read_assessment is ReadAssessment.EVIDENCE
    # A forced read here would be the third barren one on this page state, so the click goes ahead instead.
    assert await agent._read_before_interaction(state, here, clicking) is False


async def test_same_text_can_be_read_for_a_new_document_or_new_requirement() -> None:
    state = await run_state()
    requirement = Requirement(id="r1", text="Find the total", kind=RequirementKind.INFORMATION)
    state.ready_plan = Plan(requirements=(requirement,), answer_expected=True)
    obs = observation(()).model_copy(update={"document_key": "document-a"})
    page = capture((BlockKind.PARAGRAPH, "Waiting for a result"))
    llm = ScriptedLLM([{"claims": [], "answered": False}] * 3)
    agent = Agent(Mock(spec=Page), ScriptedJev({"r1": "synthesis", "r2": "synthesis"}), llm)
    await agent._read(state, page, obs)
    await agent._read(state, page, obs)
    assert len(llm.calls) == 1
    obs = obs.model_copy(update={"document_key": "document-b"})
    await agent._read(state, page, obs)
    assert len(llm.calls) == 2
    state.ready_plan = Plan(requirements=(requirement.model_copy(update={"id": "r2"}),), answer_expected=True)
    await agent._read(state, page, obs)
    assert len(llm.calls) == 3


@pytest.mark.parametrize("reader", list(FactReader))
async def test_a_secret_quoted_by_a_citation_is_redacted_from_its_links_too(reader: FactReader) -> None:
    quote = "signed in as hunter 2&x today"
    url = "https://example.test/account?token=hunter%202%26x"
    requirement_id = "r1 hunter 2&x"
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id=requirement_id, text="Who is signed in?", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    state.notes.add(Fact(reader=FactReader.LLM, text="Already known", evidence=evidence()))
    events: list[StepEvent] = []

    async def collect(event: StepEvent | BrowserEvent) -> None:
        if isinstance(event, StepEvent):
            events.append(event)

    claim: JsonValue = {"requirement_id": requirement_id, "text": quote, "cite": {"first": "s0", "last": "s0"}}
    llm = ScriptedLLM([{"claims": [claim], "answered": True}] if reader is FactReader.LLM else [])
    jev = ScriptedJev({requirement_id: "synthesis" if reader is FactReader.LLM else "c0"})
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    agent = Agent(page, jev, llm, on_event=collect)
    agent._redactor.register("password", "hunter 2&x")
    captured = capture((BlockKind.PARAGRAPH, quote)).model_copy(update={"url": url})
    await agent._step(state, observation(()), _code_decision(Operation.READ, None), capture=captured)

    (fact,) = events[0].step.facts
    assert events[0].step == state.steps[0]
    assert fact.quote == "signed in as [secret:password] today"
    assert fact.text == fact.quote
    assert fact.requirement_id == "r1 [secret:password]"
    assert fact.url == "https://example.test/account?token=[secret:password]"
    assert fact.reader is reader
    assert fact.deep_link == text_fragment(fact.url, fact.quote)
    assert "hunter" not in events[0].model_dump_json()
    assert len(state.notes.facts) == 2
    assert [*state.notes.evidence.values()][-1].quote == quote
    await agent._step(state, observation(()), _code_decision(Operation.SCROLL, None))
    assert events[1].step.facts == ()
    assert events[1].step.note is None

    link = text_fragment(url, quote)
    cited = Citation(id=1, text=quote, url=url, quote=quote, deep_link=link)
    composed = ComposedAnswer(answer=quote, linked_answer=f"{quote} [1](<{link}>)", claims=(), citations=(cited,))

    answer, (public,) = agent._public_answer(composed)

    assert "hunter" not in answer and "hunter" not in public.model_dump_json()
    assert public.deep_link == text_fragment(public.url, "signed in as [secret:password] today")
    assert public.deep_link in answer


async def test_a_link_sharing_another_links_start_is_still_redacted() -> None:
    # Rewriting one link at a time changed the start of the longer link, which then kept its encoded secret.
    agent = Agent(Mock(spec=Page), ScriptedJev({}), ScriptedLLM([]))
    agent._redactor.register("password", "alpha-beta")
    url = "https://example.test/page"
    quotes = ("token alpha-beta", "token alpha-beta repeated alpha-beta")
    cited = tuple(
        Citation(id=n, text=quote, url=url, quote=quote, deep_link=text_fragment(url, quote))
        for n, quote in enumerate(quotes, 1)
    )
    body = " ".join(f"[{c.id}](<{c.deep_link}>)" for c in cited)
    answer, public = agent._public_answer(ComposedAnswer(answer="", linked_answer=body, claims=(), citations=cited))
    assert "alpha" not in answer
    assert all(p.deep_link in answer for p in public)


async def test_a_secret_the_site_uses_as_its_hostname_leaves_the_cited_address_readable() -> None:
    # The username `practice` is also the site's subdomain; redacting it there left links no browser opens.
    agent = Agent(Mock(spec=Page), ScriptedJev({}), ScriptedLLM([]))
    agent._redactor.register("username", "practice", "https://practice.example.test")
    url, quote = "https://practice.example.test/secure?user=practice", "Welcome, practice"
    cited = Citation(id=1, text=quote, url=url, quote=quote, deep_link=text_fragment(url, quote))
    linked = f"Signed in as practice [1](<{cited.deep_link}>)"
    answer, (public,) = agent._public_answer(
        ComposedAnswer(answer="", linked_answer=linked, claims=(), citations=(cited,))
    )
    assert public.url == "https://practice.example.test/secure?user=[secret:username]"
    assert answer == f"Signed in as [secret:username] [1](<{public.deep_link}>)"


async def test_what_the_model_sees_keeps_the_host_a_secret_was_typed_on_and_blanks_it_everywhere_else() -> None:
    # Masking the observed address as `https://••••••••.example.test/secure` put that broken address in every step,
    # fact and citation link the run reported, since those are built from what the model saw.
    page = Mock(spec=Page)
    url = "https://practice.example.test/secure?user=practice"
    links = (_link("home", "Home", "https://practice.example.test/"), _link("x", "x", "https://practice.evil.test/"))
    seen = _at(url, *links).model_copy(update={"viewport_text": "practice hunter2 at https://practice.example.test/"})
    page.observe = AsyncMock(return_value=seen)
    page.capture = AsyncMock(
        return_value=capture((BlockKind.PARAGRAPH, "Welcome, practice")).model_copy(update={"url": url})
    )
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]))
    agent._redactor.register("username", "practice", "https://practice.example.test/login")
    agent._redactor.register("password", "hunter2", "https://practice.example.test/login")

    observed, captured = await agent._observe(), await agent._capture()

    assert observed.url == captured.url == "https://practice.example.test/secure?user=••••••••"
    assert [c.href for c in observed.controls] == ["https://practice.example.test/", "https://••••••••.evil.test/"]
    assert observed.viewport_text == "•••••••• ••••••• at https://practice.example.test/"
    assert captured.text.endswith("Welcome, ••••••••")


@pytest.mark.parametrize(
    ("failure", "status"),
    [
        (JevRetriesExhausted("Jev request failed; last: HTTP 503", seconds=1.0, unaccounted_requests=0), "unavailable"),
        (JevError("invalid answer"), "error"),
    ],
)
async def test_a_provider_outage_ends_the_run_apart_from_a_failure(failure: JevError, status: str) -> None:
    """The eval harness runs an unavailable run again; an error is the agent's own failure and counts."""
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=_at("https://shop.test/", _button("Buy")))
    page.artifacts = ()

    class DownJev(ScriptedJev):
        async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
            raise failure

    agent = Agent(page, DownJev({}), ScriptedLLM([{"requirements": [], "answer_expected": False}]))

    result = await agent.run("Buy it", limits=Limits(max_steps=2))

    assert result.status == status


@pytest.mark.parametrize("batched", [False, True])
@pytest.mark.parametrize("max_dollars", [0.00006, 0.0005])
async def test_run_checks_split_spend_without_double_counting(
    monkeypatch: pytest.MonkeyPatch, batched: bool, max_dollars: float
) -> None:
    sent: list[tuple[str, ...]] = []

    def handler(request: httpx.Request) -> httpx.Response:
        keys = tuple(json.loads(request.content)["questions"])
        sent.append(keys)
        if len(keys) > 1:
            return httpx.Response(503)
        return httpx.Response(
            200,
            json={"answers": {keys[0]: {"type": "noul", "noul": 1}}, "usage": {"input_tokens": 1000}},
        )

    real_sleep = asyncio.sleep

    async def immediate(_: float) -> None:
        await real_sleep(0)

    monkeypatch.setattr("fastbrowse.clients.validation.asyncio.sleep", immediate)
    planned = Generation(data=Plan(requirements=(), answer_expected=False), cost=FREE)
    monkeypatch.setattr(agent_module, "make_plan", AsyncMock(return_value=planned))
    page = Mock(spec=Page)
    page.artifacts = ()
    questions = {f"q{i}": NoulQuestion(instructions="Is it?") for i in range(10)}
    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        jev = TypeSafeJevClient("key", http=http)
        agent = Agent(page, jev, ScriptedLLM([]))

        async def loop(state: _RunState, output_schema: object, until: object) -> RunResult:
            if batched:
                await evaluate_batches(jev, "page", questions, tokens=Config().tokens, ledger=state.ledger)
            else:
                state.ledger.reserve(CostComponent.JEV)
                result = await jev.evaluate("page", questions)
                state.ledger.record(result.cost)
            state.ledger.reserve(CostComponent.JEV)
            result = await jev.evaluate("page", {"next": NoulQuestion(instructions="Is it?")})
            state.ledger.record(result.cost)
            return agent._result(state, state.ledger, Status.COMPLETE)

        monkeypatch.setattr(agent, "_loop", loop)
        result = await agent.run("Read the page", limits=Limits(max_dollars=max_dollars))

    answered = 2 if max_dollars == 0.00006 else 11
    assert result.status is (Status.BUDGET_EXCEEDED if answered == 2 else Status.COMPLETE)
    assert len([keys for keys in sent if len(keys) == 1]) == answered
    assert result.cost.known_dollars == pytest.approx(answered * 0.000042)
    assert sum(line.input_tokens for line in result.cost.lines) == answered * 1000


async def test_a_page_nobody_read_is_read_before_it_is_scrolled() -> None:
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="Find Mongolia's population", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    obs = observation((_button("Next"),)).model_copy(update={"document_key": "countries"})
    page = Mock(spec=Page)
    page.capture = AsyncMock(return_value=capture((BlockKind.PARAGRAPH, "Mongolia: population 3086918")))
    llm = ScriptedLLM([{"claims": [], "answered": False}])
    jev = ScriptedJev({"operation": "scroll", "read_assessment": "absent", "r1": "synthesis"})
    decision = await decide(jev, obs, context(), Config())
    agent = Agent(page, jev, llm)
    assert await agent._read_before_interaction(state, obs, decision)
    assert len(llm.calls) == 1
    # Once this document is read, scrolling it is how the rest of a lazily drawn list comes in.
    assert not await agent._read_before_interaction(state, obs, decision)


@pytest.mark.parametrize("failed", [NavigationTimeout, SiteUnreachable])
@pytest.mark.parametrize("opening", [True, False])
async def test_navigation_timeout_is_unavailable_only_while_opening(
    monkeypatch: pytest.MonkeyPatch, opening: bool, failed: type[BrowserError]
) -> None:
    page = Mock(spec=Page)
    page.artifacts = ()
    page.navigate = AsyncMock(side_effect=failed("navigation timed out"))
    # No shortcut is proposed, whichever of the plan and the proposal asks first, so the start page is opened.
    llm = ScriptedLLM([{"url": None}, {"url": None}])
    agent = Agent(page, ScriptedJev({}), llm)

    async def loop(state: _RunState, output_schema: object, until: object) -> RunResult:
        state.steps.append(
            StepResult(
                index=0,
                operation=Operation.READ,
                decided_by=Decider.JEV,
                outcome=StepOutcome.EXECUTED,
                url="https://example.test",
                duration_ms=0,
            )
        )
        await page.navigate("https://example.test/next")
        raise AssertionError("unreachable")

    monkeypatch.setattr(agent, "_loop", loop)
    result = await agent.run("Open the page", start="https://example.test" if opening else None)
    assert result.status is (Status.UNAVAILABLE if opening else Status.ERROR)
    assert result.error == "navigation timed out"
    assert len(result.steps) == (0 if opening else 1)


async def test_opening_deadline_still_exhausts_the_budget() -> None:
    page = Mock(spec=Page)
    page.artifacts = ()

    async def navigate(url: str) -> None:
        await asyncio.Event().wait()

    page.navigate = navigate
    result = await Agent(page, ScriptedJev({}), ScriptedLLM([{"url": None}, {"url": None}])).run(
        "Open the page", start="https://example.test", limits=Limits(max_seconds=0.01)
    )
    assert result.status is Status.BUDGET_EXCEEDED


async def test_notes_overflow_returns_bounded_grounded_partial_evidence(monkeypatch: pytest.MonkeyPatch) -> None:
    page = Mock(spec=Page)
    page.artifacts = ()
    agent = Agent(
        page, ScriptedJev({}), ScriptedLLM([]), config=Config(observation=ObservationLimits(working_notes_chars=800))
    )

    async def loop(state: _RunState, output_schema: object, until: object) -> RunResult:
        state.notes.add(Fact(text="A supported fact", evidence=evidence(), reader=FactReader.LLM))
        state.notes.add(
            Fact(text="Too long " * 1000, evidence=evidence(sha="large"), requirement_id="r", reader=FactReader.LLM)
        )
        raise NotesTooLarge("Requirement evidence exceeds the 800 character notes budget")

    monkeypatch.setattr(agent, "_loop", loop)
    result = await agent.run("Read the records")
    assert result.status is Status.OBSERVATION_LIMIT
    assert result.answer and "fact" in result.answer and "omitted" in result.answer.lower()
    assert result.citations and result.evidence
    assert len(result.answer) < 800
    assert "Too long" not in result.answer


async def test_tallies_keep_guessed_sources_subject_to_verification() -> None:
    source = evidence().model_copy(update={"quote": "A quote by Ada", "url": "https://example.test/quotes?tag=ada"})
    record = Fact(text=source.quote, evidence=source, reader=FactReader.LLM)
    notes = Notes((record,))
    notes.add_tally(Tally(requirement_id="r", key="Ada", records=(fact_id(record),)))
    notes.complete_tallies("r")
    plan = Plan(
        requirements=(Requirement(id="r", text="Count quotes", kind=RequirementKind.INFORMATION),), answer_expected=True
    )
    assert _guessed(plan, notes, {source.url}) == {"r"}
    assert source.url in _grounding(notes, plan, ())


def test_a_verifier_naming_a_requirement_by_another_spelling_names_that_requirement() -> None:
    """0.5.7's verifier wrote `req_1` and `1` for `req-1` in 4 of 6 refusals; each was a doubt nothing could excuse."""
    plan = Plan(
        requirements=(
            Requirement(id="req-1", text="a", kind=RequirementKind.INFORMATION),
            Requirement(id="req-2", text="b", kind=RequirementKind.ACTION),
        ),
        answer_expected=True,
    )
    assert _plan_ids(["req_1", "2", "REQ-2", "req-9", "summary"], plan) == {"req-1", "req-2"}


async def test_a_doubted_lookup_with_every_requirement_cited_finishes_without_the_verifier() -> None:
    """The verifier excuses a cited requirement, so on a lookup it could only refuse naming nothing, which it never
    did in 76 0.5.7 verifications; each cost a screenshot and a vision call. The claims are still checked."""
    state = await run_state()
    state.notes.add(_fare("https://example.test/flights/results", "results"))
    llm = ScriptedLLM([])
    agent, on = await _finishing(state, llm, noul=0.5)

    result = await agent._finish(state, on, None, None)

    assert result is not None and result.status is Status.COMPLETE
    assert LLMPurpose.VERIFY not in [purpose for purpose, _ in llm.calls]


def _continued(*, through_end: bool = True) -> dict[str, Any]:
    return {
        "claims": [],
        "answered": False,
        "continues": [{"requirement_id": "r1", "through_end": through_end, "records": [{"first": "s0", "last": "s0"}]}],
    }


async def _pipeline_fixture(
    *, config: Config | None = None, limits: Limits | None = None, through_end: bool = True
) -> tuple[Agent, _RunState, Mock, list[Observation], list[Capture], ScriptedLLM]:
    state = await run_state()
    state.task = "Find the cheapest item across the entire list"
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text=state.task, kind=RequirementKind.INFORMATION),), answer_expected=True
    )
    state.ledger.limits = limits or Limits()
    observations = [
        _at(f"https://example.test/list/{i}", *([_link("next", "Next", f"/list/{i + 1}")] if i < 4 else []))
        for i in range(1, 5)
    ]
    captures = [
        capture((BlockKind.RECORD, f"Item {i}: ${5 - i}")).model_copy(update={"url": obs.url})
        for i, obs in enumerate(observations, 1)
    ]
    page = Mock(spec=Page)
    page.artifacts = ()
    page.observe = AsyncMock(side_effect=observations[1:])
    page.capture = AsyncMock(side_effect=captures[1:])
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    llm = ScriptedLLM(
        [
            _continued(through_end=through_end),
            _continued(),
            _continued(),
            {
                "answered": True,
                "claims": [
                    {
                        "requirement_id": "r1",
                        "text": "Item 4 is cheapest at $1",
                        "cite": {"first": "s0", "last": "s0"},
                        "draws_on": [f"{c.sha256}:0:{len(c.text)}" for c in captures[:3]],
                    }
                ],
            },
        ]
    )
    agent = Agent(page, ScriptedJev({}), llm, config=config)
    state.first_url = observations[0].url
    await agent._step(state, observations[0], _code_decision(Operation.READ, None), capture=captures[0])
    return agent, state, page, observations, captures, llm


async def test_pager_reads_overlap_and_final_read_sees_records_in_page_order() -> None:
    agent, state, page, observations, captures, llm = await _pipeline_fixture()
    original = agent._page_records
    second_started, third_done = asyncio.Event(), asyncio.Event()

    async def overlapping(state: _RunState, captured: Capture, wanted: Any) -> Any:
        if captured.url == captures[1].url:
            second_started.set()
            result = await original(state, captured, wanted)
            await third_done.wait()
            return result
        assert second_started.is_set()
        result = await original(state, captured, wanted)
        third_done.set()
        return result

    agent._page_records = AsyncMock(side_effect=overlapping)
    async with asyncio.timeout(2):
        await agent._pipeline_pages(state, observations[0])
    assert third_done.is_set()
    assert state.notes.evidenced("r1")
    assert list(state.notes.evidence) == [f"{c.sha256}:0:{len(c.text)}" for c in captures]
    final_prompt = llm.calls[-1][1][-1].content
    collected = final_prompt.split("# Capture")[0]
    for captured in captures[:3]:
        assert captured.text in collected and captured.url in collected
    assert [e.url for e in state.notes.supporting_evidence("r1")] == [c.url for c in captures]
    assert [step.url for step in state.steps if step.operation is Operation.READ] == [c.url for c in captures]
    assert [step.index for step in state.steps] == list(range(7))
    assert len(state.history) == len(state.steps) == state.ledger.steps == 7
    assert page.act.await_count == 3
    assert agent._result(state, state.ledger, Status.COMPLETE).final_url == observations[-1].url


@pytest.mark.parametrize(
    "limits,config,pages",
    [
        (Limits(), Config(max_pages=1), 1),
        (Limits(max_steps=5), Config(), 2),
        (Limits(max_llm_calls=2), Config(), 1),
    ],
)
async def test_pipeline_reserves_read_steps_and_respects_page_and_call_caps(
    limits: Limits, config: Config, pages: int
) -> None:
    agent, state, page, observations, _, _ = await _pipeline_fixture(config=config, limits=limits)
    await agent._pipeline_pages(state, observations[0])
    assert page.act.await_count == state.pages == pages
    assert state.ledger.steps == 1 + 2 * pages <= limits.max_steps
    assert state.ledger.llm_calls == pages + 1 <= limits.max_llm_calls
    assert not state.notes.evidenced("r1")


async def test_bounded_page_requirement_keeps_serial_paging() -> None:
    _, state, _, observations, _, _ = await _pipeline_fixture(through_end=False)
    assert state.next_page and not state.through_end
    decision = agent_module._paging(state, observations[0])
    assert decision is not None and decision.operation is Operation.CLICK


async def test_pipeline_read_failure_retries_saved_capture_and_returns_to_serial_loop() -> None:
    agent, state, page, observations, captures, _ = await _pipeline_fixture()
    original = agent._page_records
    calls = 0

    async def fail_once(state: _RunState, captured: Capture, wanted: Any) -> Any:
        nonlocal calls
        calls += 1
        if calls == 1:
            raise agent_module.LLMError("reader unavailable")
        return await original(state, captured, wanted)

    agent._page_records = AsyncMock(side_effect=fail_once)
    await agent._pipeline_pages(state, observations[0])
    assert calls == 2 and state.paging_failed
    assert page.act.await_count == 1
    assert any(e.url == captures[1].url for e in state.notes.evidence.values())
    assert not state.incomplete
    assert not state.notes.evidenced("r1")
    # A records read does not spend the serial read of this page.
    assert not await agent._step(state, observations[1], _code_decision(Operation.READ, None), capture=captures[1])
    assert state.next_page


async def test_pipeline_load_failure_keeps_captured_records_and_stops_navigating() -> None:
    agent, state, page, observations, captures, _ = await _pipeline_fixture()
    page.act.side_effect = [
        ActResult(outcome=StepOutcome.EXECUTED, page_changed=True),
        NavigationTimeout("load failed"),
    ]
    await agent._pipeline_pages(state, observations[0])
    assert state.paging_failed and not state.next_page
    assert page.act.await_count == 2
    assert [e.url for e in state.notes.evidence.values()] == [c.url for c in captures[:2]]
    assert not state.notes.evidenced("r1")


async def test_pipeline_cancellation_joins_background_readers() -> None:
    agent, state, page, observations, _, _ = await _pipeline_fixture()
    started, cancelled = asyncio.Event(), asyncio.Event()

    async def blocked(*args: Any) -> Any:
        started.set()
        try:
            await asyncio.Event().wait()
        finally:
            cancelled.set()

    agent._page_records = AsyncMock(side_effect=blocked)
    running = asyncio.create_task(agent._pipeline_pages(state, observations[0]))
    await started.wait()
    running.cancel()
    with pytest.raises(asyncio.CancelledError):
        await running
    assert cancelled.is_set()
    assert page.act.await_count < 3


@pytest.mark.parametrize("href", ["https://other.test/list/2", "//other.test/list/2", "other.test/list/2", "#next"])
def test_code_pager_never_follows_another_site_or_same_page(href: str) -> None:
    assert agent_module.next_page_control(_at("https://example.test/list/1", _link("n", "Next", href))) is None


def test_disabled_pager_is_not_followed() -> None:
    disabled = _link("next", "Next", "/list/2").model_copy(update={"operations": frozenset()})
    assert agent_module.next_page_control(_at("https://example.test/list/1", disabled)) is None


async def test_pipeline_final_read_error_leaves_current_capture_readable() -> None:
    agent, state, _, observations, captures, llm = await _pipeline_fixture(config=Config(max_pages=1))
    original = llm.generate
    calls = 0

    async def fail_once(*args: Any, **kwargs: Any) -> Any:
        nonlocal calls
        calls += 1
        if calls == 1:
            raise agent_module.LLMError("read failed")
        return await original(*args, **kwargs)

    llm.generate = AsyncMock(side_effect=fail_once)
    await agent._pipeline_pages(state, observations[0])
    assert state.paging_failed and state.steps[-1].outcome is StepOutcome.FAILED
    assert not await agent._step(state, observations[1], _code_decision(Operation.READ, None), capture=captures[1])
    assert calls == 2


async def test_pipeline_persistent_read_failure_leaves_comparison_incomplete() -> None:
    agent, state, _, observations, _, _ = await _pipeline_fixture()
    agent._page_records = AsyncMock(side_effect=agent_module.LLMError("read failed"))
    await agent._pipeline_pages(state, observations[0])
    assert state.paging_failed and state.incomplete == {"r1"}
    assert state.steps[-1].outcome is StepOutcome.FAILED
    assert state.history[-1].operation is Operation.READ
    assert not state.notes.evidenced("r1")


async def test_pipeline_time_limit_cancels_and_joins_readers() -> None:
    agent, state, _, observations, _, _ = await _pipeline_fixture()
    cancelled = asyncio.Event()

    async def blocked(*args: Any, **kwargs: Any) -> Any:
        try:
            await asyncio.Event().wait()
        finally:
            cancelled.set()

    agent._page_records = AsyncMock(side_effect=blocked)
    state.ledger.limits = Limits(max_seconds=0.02)
    state.ledger.started = agent_module.time.monotonic()
    with pytest.raises((TimeoutError, BudgetExceeded)):
        async with asyncio.timeout(state.ledger.limits.max_seconds):
            await agent._pipeline_pages(state, observations[0])
    assert cancelled.is_set()


async def test_loop_enters_pipeline_without_jev_between_pages(monkeypatch: pytest.MonkeyPatch) -> None:
    agent, state, page, observations, _, _ = await _pipeline_fixture()
    page.observe.side_effect = [*observations, observations[-1]]
    decisions = 0

    async def done(*args: Any, **kwargs: Any) -> Decision:
        nonlocal decisions
        decisions += 1
        assert state.notes.evidenced("r1")
        assert agent._observed is not None and agent._observed.url == observations[-1].url
        return _code_decision(Operation.DONE, None)

    monkeypatch.setattr(agent_module, "decide", done)
    agent._finish = AsyncMock(side_effect=lambda *args: agent._result(state, state.ledger, Status.COMPLETE))
    result = await agent._loop(state, None, None)
    assert result.status is Status.COMPLETE and decisions == 1
    assert page.act.await_count == 3


@pytest.mark.parametrize("outcome", [StepOutcome.EXECUTED, StepOutcome.STALE, StepOutcome.COVERED])
async def test_pipeline_stops_after_a_pager_click_that_does_not_load_a_page(outcome: StepOutcome) -> None:
    agent, state, page, observations, _, _ = await _pipeline_fixture()
    page.act.return_value = ActResult(outcome=outcome, page_changed=False)
    agent._act_on_twin = AsyncMock(return_value=None)
    await agent._pipeline_pages(state, observations[0])
    assert page.act.await_count == 1
    page.capture.assert_not_called()
    assert not state.next_page and not state.notes.evidenced("r1")


async def test_pipeline_read_events_keep_the_frame_of_the_captured_page() -> None:
    agent, state, _, observations, captures, _ = await _pipeline_fixture(config=Config(step_frames=True))
    events: list[StepEvent] = []

    async def collect(event: StepEvent | BrowserEvent) -> None:
        assert isinstance(event, StepEvent)
        events.append(event)

    agent._on_event = collect
    agent._frame = AsyncMock(side_effect=[b"click2", b"read2", b"click3", b"read3", b"click4", b"read4"])
    await agent._pipeline_pages(state, observations[0])
    reads = [event for event in events if event.step.operation is Operation.READ]
    assert [(event.step.url, event.frame) for event in reads] == list(
        zip([c.url for c in captures[1:]], [b"read2", b"read3", b"read4"], strict=True)
    )


async def test_result_reports_latest_observed_url_with_result_redaction() -> None:
    state = await run_state()
    state.last_page = ("https://example.test/list/1", "old")
    url = "https://example.test/list/2?token=hunter2"
    page = Mock(spec=Page)
    page.artifacts = ()
    page.observe = AsyncMock(return_value=_at(url))
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]))
    agent._redactor.register("password", "hunter2")
    await agent._observe()
    result = agent._result(state, state.ledger, Status.BUDGET_EXCEEDED)
    assert result.final_url == agent._redactor.redact(url)
    assert result.final_url != agent._redactor.mask(url)
