"""A task that rules out downloads is never handed a control the page says downloads, by the step or by recovery."""

from unittest.mock import AsyncMock, Mock

import pytest

from fastbrowse.agent import Agent, _Unsure
from fastbrowse.config import Config
from fastbrowse.models import Authorization, Operation, StepOutcome
from fastbrowse.page import ActResult, Control, Page
from fastbrowse.policy import decide
from tests.test_agent import run_state
from tests.test_policy import ScriptedJev, context, observation
from tests.test_retrieval import ScriptedLLM

READ_ONLY = "Tell me which game version release 2.1.0 supports. Read-only, do not download anything."


def link(control_id: str, label: str, href: str, *, download: bool | None = None) -> Control:
    return Control(
        id=control_id,
        frame_id=None,
        role="link",
        label=label,
        href=href,
        download=download,
        operations=frozenset({Operation.CLICK}),
    )


ARCHIVE = link("zip", "Download", "/files/addon-2.1.0.zip?sig=1")
NOTES = link("notes", "Release notes", "/releases/2.1.0")


async def test_recovery_cannot_name_a_download_the_task_rules_out() -> None:
    # Recovery, unable to see a release's address, clicked its download link to look for it in a task that said
    # not to download, and the release archive was saved.
    page = Mock(spec=Page)
    page.screenshot = AsyncMock(return_value=b"png")
    llm = ScriptedLLM(
        [
            {
                "diagnosis": "The address is behind the download link",
                "next_subgoal": "Open the first link",
                "give_up": False,
                "control": 0,
                "operation": "click",
            }
        ]
    )
    jev = ScriptedJev({}, nouls={"downloads": 1.0})
    state = await run_state()
    state.task = READ_ONLY

    await Agent(page, jev, llm)._recover(state, observation((ARCHIVE, NOTES)), "uncertain next step (0.40)")

    prompt = "\n".join(str(message.content) for message in llm.calls[-1][1])
    assert "addon-2.1.0.zip" not in prompt and "Release notes" in prompt
    assert state.directed == (Operation.CLICK, NOTES.id)


@pytest.mark.parametrize("authorized", [False, True])
async def test_a_download_the_task_rules_out_is_refused_before_dispatch(authorized: bool) -> None:
    obs = observation((ARCHIVE, NOTES))
    jev = ScriptedJev({"operation": "click", "click_target": ARCHIVE.id}, noul=0.0, nouls={"downloads": 1.0})
    decision = await decide(jev, obs, context(task=READ_ONLY), Config())
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    state = await run_state()
    state.task = READ_ONLY
    # Authorization lets the run commit what the task asks for; it does not let it do what the task forbids.
    state.authorization = Authorization(irreversible_actions=authorized)

    with pytest.raises(_Unsure, match="download"):
        await Agent(page, jev, ScriptedLLM([]))._step(state, obs, decision)
    page.act.assert_not_called()
    assert state.steps[-1].outcome is StepOutcome.FAILED


async def test_a_download_the_task_permits_is_dispatched_and_the_answer_is_asked_once() -> None:
    attribute = link("export", "Export changelog", "/export", download=True)
    obs = observation((ARCHIVE, attribute))
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    state = await run_state()
    state.task = "Download the release archive and the changelog."
    for target in (ARCHIVE, attribute):
        jev = ScriptedJev({"operation": "click", "click_target": target.id}, noul=0.0)
        decision = await decide(jev, obs, context(task=state.task), Config())
        await Agent(page, jev, ScriptedLLM([]))._step(state, obs, decision)
        asked = ["downloads" in request for request in jev.requests]
        assert asked.count(True) == (target is ARCHIVE)
    assert page.act.await_count == 2


async def test_a_control_with_no_download_evidence_costs_no_question() -> None:
    obs = observation((NOTES,))
    jev = ScriptedJev({"operation": "click", "click_target": NOTES.id}, noul=0.0)
    decision = await decide(jev, obs, context(task=READ_ONLY), Config())
    page = Mock(spec=Page)
    page.act = AsyncMock(return_value=ActResult(outcome=StepOutcome.EXECUTED, page_changed=True))
    state = await run_state()
    state.task = READ_ONLY

    await Agent(page, jev, ScriptedLLM([]))._step(state, obs, decision)
    page.act.assert_awaited_once()
    assert not any("downloads" in request for request in jev.requests)
