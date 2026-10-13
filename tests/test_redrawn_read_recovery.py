"""A page already read that redraws under a repeated click does not renew recovery (#340).

A read-only release check read a project's file list, then clicked its Files tab, its version row and its
download link again and again. Each click redrew the same addresses a little (a row expanded, a countdown ended,
a cookie banner closed), and each redraw was a page state never seen, so every one restored the recovery budget
and the run went round until its step limit."""

from unittest.mock import Mock

from fastbrowse.agent import Agent, _RunState
from fastbrowse.models import Authorization, Operation, StepOutcome
from fastbrowse.page import BlockKind, Control, Observation, Page
from fastbrowse.policy import HistoryEntry
from tests.test_agent import _button, run_state
from tests.test_policy import ScriptedJev, observation
from tests.test_retrieval import ScriptedLLM, capture

FILES = "https://addons.example.test/project/files"
DOWNLOAD = "https://addons.example.test/project/download/1"


def _at(url: str, *labels: str) -> Observation:
    return observation(tuple(_button(label) for label in labels)).model_copy(update={"url": url})


def _clicked(label: str) -> HistoryEntry:
    return HistoryEntry(operation=Operation.CLICK, target=label, outcome=StepOutcome.EXECUTED, page_changed=True)


async def _recovered_on_read_list() -> tuple[Agent, _RunState]:
    """A run that read the file list, then spent one recovery on it."""
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    agent = Agent(Mock(spec=Page), ScriptedJev({}), ScriptedLLM([]))
    files = _at(FILES, "Files", "v1.14.1", "Download")
    agent._settle(state, files)
    state.notes.remember_capture(capture((BlockKind.RECORD, "v1.14.1 Oct 13"), url=FILES))
    state.history.append(
        HistoryEntry(operation=Operation.READ, target=None, outcome=StepOutcome.EXECUTED, page_changed=False)
    )
    state.recoveries = 1
    state.recovered_at = len(state.history)
    state.history.append(_clicked("v1.14.1"))
    state.left = "files"
    return agent, state


async def test_a_read_page_redrawn_by_a_repeated_click_does_not_renew_recovery() -> None:
    agent, state = await _recovered_on_read_list()
    # The version row expanded: one more control at the same address, a state never seen before.
    agent._settle(state, _at(FILES, "Files", "v1.14.1", "Download", "Collapse"))
    assert state.recoveries == 1


async def test_reaching_an_address_not_yet_read_still_renews_recovery() -> None:
    agent, state = await _recovered_on_read_list()
    agent._settle(state, _at(DOWNLOAD, "try again"))
    assert state.recoveries == 0


async def test_a_read_page_redrawn_after_a_read_that_paid_out_still_renews_recovery() -> None:
    agent, state = await _recovered_on_read_list()
    # Loading more of a list keeps its address; a read that found new quotes since recovery is progress.
    state.history.insert(
        -1,
        HistoryEntry(
            operation=Operation.READ, target=None, outcome=StepOutcome.EXECUTED, page_changed=False, read_progress=True
        ),
    )
    agent._settle(state, _at(FILES, "Files", "v1.14.1", "Download", "v1.14.0"))
    assert state.recoveries == 0


def _field(label: str) -> Control:
    return Control(id=label.lower(), frame_id=None, role="textbox", label=label, operations=frozenset({Operation.FILL}))


async def test_a_new_step_at_the_same_address_still_renews_recovery() -> None:
    """A wizard that keeps its address is read on its first step; the next step's new fields are a real move."""
    state = await run_state()
    state.authorization = Authorization(irreversible_actions=True)
    agent = Agent(Mock(spec=Page), ScriptedJev({}), ScriptedLLM([]))
    first = observation((_field("Name"), _button("Next"))).model_copy(update={"url": FILES})
    agent._settle(state, first)
    state.notes.remember_capture(capture((BlockKind.RECORD, "Step 1 of 3"), url=FILES))
    state.recoveries = 1
    state.recovered_at = len(state.history)
    state.history.append(_clicked("Next"))
    state.left = "step 1"
    agent._settle(state, observation((_field("Email"), _button("Next"))).model_copy(update={"url": FILES}))
    assert state.recoveries == 0
