"""A missing item on a fragment address still needs a fresh document."""

import pytest

from fastbrowse import run_task
from fastbrowse.models import Limits
from tests.browser.test_target_not_here import site as site
from tests.scripted import ScriptedJev, ScriptedLLM, steps


@pytest.mark.timeout(120)
async def test_a_missing_item_reloads_a_page_with_a_fragment(
    cdp_url: str, site: tuple[str, list[str], list[int]]
) -> None:
    base, clicked, loads = site
    await run_task(
        "Click the new image",
        start=base + "#gallery",
        cdp_url=cdp_url,
        jev=ScriptedJev(steps("click New image; navigate; click New image")),
        llm=ScriptedLLM(),
        limits=Limits(max_steps=3, max_seconds=30),
    )
    assert clicked[:1] == ["New image"], (
        "The missing-item reload must fetch a fresh document even when its URL has a fragment",
        clicked,
        loads,
    )
