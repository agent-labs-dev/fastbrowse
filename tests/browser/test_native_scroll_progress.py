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
        llm=ScriptedLLM(script=steps(script)),
    )
    assert all(step.outcome is StepOutcome.EXECUTED for step in result.steps), result.error
    return [step.operation for step in result.steps]


@pytest.mark.timeout(120)
async def test_scrolling_to_parts_never_shown_is_not_withdrawn_after_three_scrolls(
    cdp_url: str, main_site: str
) -> None:
    # The page's controls never change, so the stall rule used to read the fourth scroll as the same page state again.
    operations = await _scrolled(cdp_url, main_site, "scroll; scroll; scroll; scroll; scroll")
    # The driver's closing read of the page follows the scrolls.
    assert operations[:5] == [Operation.SCROLL] * 5, operations
