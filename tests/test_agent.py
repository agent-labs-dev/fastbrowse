"""Field context and recovery state regressions, with model outputs scripted."""

import asyncio
import json
from unittest.mock import AsyncMock, Mock

import pytest
from pydantic import JsonValue

# pyright: reportPrivateUsage=false
from fastbrowse import agent as agent_module
from fastbrowse.agent import (
    Agent,
    _follow_recovery,
    _history,
    _RunState,
    _Stop,
    _try_unsure,
    _unread,
    _Unsure,
    _verified,
)
from fastbrowse.config import Config, ObservationLimits
from fastbrowse.llm import Generation
from fastbrowse.memory import Fact, Notes
from fastbrowse.models import Authorization, Decider, Limits, LLMPurpose, Operation, Status, StepOutcome, StepResult
from fastbrowse.page import ActResult, BlockKind, Control, Observation, Page
from fastbrowse.planner import Plan, Requirement, RequirementKind
from fastbrowse.policy import HistoryEntry, decide
from fastbrowse.telemetry import Ledger
from fastbrowse.verification import LLMVerdict
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


async def test_an_unsure_pick_is_acted_on_once_per_page_state() -> None:
    state = await run_state()
    first, second = observation((_button("Done"),)), observation((_button("Close dialog"),))
    assert _try_unsure(state, first)
    assert not _try_unsure(state, first)
    assert _try_unsure(state, second)


@pytest.mark.parametrize(("confidence", "raised"), [(0.3, _Unsure), (0.9, _Stop)])
async def test_an_unsure_pick_that_may_commit_something_recovers_rather_than_asking_the_user(
    confidence: float, raised: type[Exception]
) -> None:
    button = _button("Place order")
    jev = ScriptedJev({"operation": "click", "click_target": button.id}, noul=0.9)
    obs = observation((button,))
    decision = (await decide(jev, obs, context(), Config())).model_copy(update={"operation_confidence": confidence})
    with pytest.raises(raised):
        await Agent(Mock(spec=Page), jev, ScriptedLLM([]))._gate_irreversible(await run_state(), obs, decision)


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


async def test_recovery_can_direct_a_read_with_no_control_to_name() -> None:
    button = Control(id="next", frame_id=None, role="button", label="Next", operations=frozenset({Operation.CLICK}))
    obs = observation((button,))
    page = Mock(spec=Page)
    page.screenshot = AsyncMock(return_value=b"png")
    recovery = {"diagnosis": "the answer is further down", "next_subgoal": "Read the page", "give_up": False}
    llm = ScriptedLLM([{**recovery, "control": None, "operation": "read"}])
    jev = ScriptedJev({"operation": "scroll"})
    state = await run_state()
    await Agent(page, jev, llm)._recover(state, obs, "uncertain next step (0.47)")
    unsure = await decide(jev, obs, context(), Config())
    followed = _follow_recovery(state, obs, unsure, uncertain=True)
    assert followed is not None and followed.operation is Operation.READ and followed.target is None


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
    note = (
        "back to a page state first reached 2 actions ago; "
        "the actions since (click Search, click Done) undid each other"
    )
    assert agent._settle(state, form) == note
    assert state.history[-1].effect == note


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
        Fact(requirement_id=r, text=r, evidence=evidence(start=i)) for i, r in enumerate(("httpx", "compare"))
    )
    assert _verified(LLMVerdict(complete=complete, missing=missing), plan, notes) is accepted


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
    before = observation((_button("Done"),))
    redrawn = tuple(_button("Done").model_copy(update={"id": f"done-{n}"}) for n in range(twins))
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=observation(redrawn))
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
    control = agent_module._next_page_control(_at("https://example.test/list/", *controls))
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
                    "source_id": "s0",
                    "quote": "Sharp Objects £47.82",
                }
            ],
            "answered": True,
            "continues": ["r1"],
        },
        {
            "claims": [
                {
                    "requirement_id": "r1",
                    "text": "Tastes Like Fear is cheapest at £10.69",
                    "source_id": "s0",
                    "quote": "Tastes Like Fear £10.69",
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
    assert "next-page control ('next')" in llm.calls[0][1][-1].content
    click = agent_module._paging(state, first, Config().max_pages)
    assert click is not None and click.operation is Operation.CLICK and click.target is not None
    await agent._step(state, first, click, Decider.CODE)
    assert page.act.await_args is not None and page.act.await_args.args[0].target_id == "next"

    opened = agent_module._paging(state, second, Config().max_pages)
    assert opened is not None and opened.operation is Operation.READ
    await agent._step(state, second, opened, Decider.CODE)
    assert state.notes.evidenced("r1")
    assert agent_module._paging(state, second, Config().max_pages) is None
    assert [(s.operation, s.decided_by) for s in state.steps] == [
        (Operation.READ, Decider.JEV),
        (Operation.CLICK, Decider.CODE),
        (Operation.READ, Decider.CODE),
    ]


async def test_a_list_goes_on_to_jev_with_a_hint_when_code_finds_no_next_page() -> None:
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="r1", text="How many quotes by Einstein?", kind=RequirementKind.INFORMATION),),
        answer_expected=True,
    )
    here = _at("https://example.test/quotes/", _button("Load more"))
    reads: list[JsonValue] = [{"claims": [], "answered": False, "continues": ["r1"]}]
    agent = Agent(Mock(spec=Page), ScriptedJev({"r1": "none"}), ScriptedLLM(reads))
    await agent._read(state, capture((BlockKind.PARAGRAPH, "Einstein quote")), here)
    assert state.next_page is None
    assert state.hint is not None and "go on past this page" in state.hint


async def test_the_pages_code_opens_are_capped() -> None:
    state = await run_state()
    here = _at("https://example.test/list/", _link("next", "next", "/list/2"))
    state.next_page, state.pages = "next", 2
    assert agent_module._paging(state, here, 2) is None
    assert state.next_page is None


async def test_a_click_that_changed_nothing_is_not_taken_again_from_the_same_page() -> None:
    search = _button("Search")
    form = observation((search,))
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=False))
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]))
    decision = await decide(ScriptedJev({"operation": "click", "click_target": "search"}), form, context(), Config())
    await agent._step(state, form, decision)
    assert agent_module._signature(decision, form) in state.idle
    # From a page that has since changed, the same click is a new try.
    filled = observation((search, field("Return").model_copy(update={"value": "Fri, Oct 23"})))
    assert agent_module._signature(decision, filled) not in state.idle
