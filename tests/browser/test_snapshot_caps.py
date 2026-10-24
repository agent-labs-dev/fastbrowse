"""The snapshot sends whole rows only within the observation caps; the observation is the one sending all would give."""

from __future__ import annotations

from collections.abc import Iterator

import pytest
from pydantic import JsonValue

from benchmarks.fixtures import open_page, serve
from fastbrowse.browser.page import CdpPage
from fastbrowse.browser.session import BrowserSession
from fastbrowse.config import Config, ObservationLimits


@pytest.fixture(scope="module")
def origin() -> Iterator[str]:
    with serve() as origin:
        yield origin


@pytest.mark.parametrize("name", ["links", "cards", "forms", "frames"])
async def test_capped_snapshot_observes_what_an_uncapped_one_would(
    browser_session: BrowserSession, origin: str, name: str
) -> None:
    page = CdpPage(browser_session, Config(observation=ObservationLimits(max_controls=12, max_offscreen_controls=4)))
    await open_page(page, origin, name, 200)
    # A load-more button and a next-page link at the foot of the page, past both caps, which only a pager keeps.
    await page._evaluate(
        browser_session.active_session_id,
        "(() => { const foot = document.createElement('div'); foot.style.marginTop = '3000px'; "
        "foot.innerHTML = '<button>Show 20 more</button><a href=\"/page/3\">Next page</a>'; "
        "document.body.append(foot); })()",
    )
    capped = await page.observe()
    capped_state = page._last.controls if page._last else None

    sends_all = page._page_js
    page._page_js = f"((mode) => ({sends_all})(mode))"
    whole = await page.observe()
    whole_state = page._last.controls if page._last else None

    assert any(c.label == "Show 20 more" for c in capped.controls)
    assert any(c.label == "Next page" for c in capped.controls)
    assert capped.controls == whole.controls
    assert capped.omitted_controls == whole.omitted_controls
    assert capped.page_key == whole.page_key
    assert capped_state == whole_state


async def test_rows_lost_before_they_are_read_make_observe_read_the_page_again(
    browser_session: BrowserSession, origin: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    page = CdpPage(browser_session, Config(observation=ObservationLimits(max_controls=12, max_offscreen_controls=4)))
    await open_page(page, origin, "links", 200)
    evaluate = page._evaluate
    lost = 0

    async def losing(
        session_id: str, expression: str, *, context_id: int | None = None, unique_context_id: str | None = None
    ) -> JsonValue:
        nonlocal lost
        if "r.sent.rows" in expression and not lost:
            # As if another snapshot, or a new document, had come between the read and the fetch.
            lost += 1
            await evaluate(session_id, "delete window.__fastbrowse.sent")
        return await evaluate(session_id, expression, context_id=context_id, unique_context_id=unique_context_id)

    monkeypatch.setattr(page, "_evaluate", losing)
    observation = await page.observe()
    assert lost == 1
    assert any(c.next_page for c in observation.controls)
