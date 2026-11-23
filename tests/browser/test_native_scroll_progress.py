"""#1137: scrolling a long page shows parts never shown before, which is new evidence the stall rule must count."""

import pytest

from fastbrowse import run_task
from fastbrowse.config import Config
from fastbrowse.models import Operation, StepOutcome
from tests.scripted import ScriptedJev, ScriptedLLM, steps


async def _scrolled(cdp_url: str, main_site: str, script: str) -> list[Operation]:
    result = await run_task(
        script,
        start=f"{main_site}/long-page.html",
        cdp_url=cdp_url,
        config=Config(),
        jev=ScriptedJev(steps(script)),
        llm=ScriptedLLM(),
    )
    assert all(step.outcome is StepOutcome.EXECUTED for step in result.steps), result.error
    return [step.operation for step in result.steps]


@pytest.mark.timeout(120)
async def test_scrolling_to_parts_never_shown_is_not_withdrawn_after_three_scrolls(
    cdp_url: str, main_site: str
) -> None:
    # The page's controls never change, so the stall rule used to read the fourth scroll as the same page state again.
    operations = await _scrolled(cdp_url, main_site, "scroll; scroll; scroll; scroll; scroll")
    assert operations == [Operation.SCROLL] * 5


@pytest.mark.timeout(120)
async def test_scrolling_back_over_parts_already_shown_is_withdrawn(cdp_url: str, main_site: str) -> None:
    operations = await _scrolled(
        cdp_url, main_site, "scroll; scroll; scroll_up; scroll_up; scroll; scroll_up; scroll; scroll_up"
    )
    # Two new parts, then three scrolls over parts already shown: the third is the stall budget's, so the sixth step
    # is not the scroll the script asked for.
    assert operations[:5] == [
        Operation.SCROLL,
        Operation.SCROLL,
        Operation.SCROLL_UP,
        Operation.SCROLL_UP,
        Operation.SCROLL,
    ]
    assert operations[5] not in (Operation.SCROLL, Operation.SCROLL_UP)
