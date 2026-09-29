"""The Fastbrowse navigation arm runs in the viewport the competing arm's browser has, and evidence records it."""

from unittest.mock import Mock

import pytest

from fastbrowse.adapters.local_chrome import find_chrome, local_chrome
from fastbrowse.agent import Agent
from fastbrowse.browser import BrowserSession
from fastbrowse.config import Config
from fastbrowse.evals import live
from fastbrowse.evals.local import fixture_server
from fastbrowse.evals.observe import GradedPage
from fastbrowse.models import LocalChrome


async def _navigate(session: BrowserSession, url: str) -> None:
    sid = session.active_session_id
    await session.client.send.Page.navigate(params={"url": url}, session_id=sid)
    await session.client.send.Runtime.evaluate(
        params={
            "expression": "new Promise(r => document.readyState === 'complete' ? r() : "
            "window.addEventListener('load', r, {once: true}))",
            "awaitPromise": True,
        },
        session_id=sid,
    )


@pytest.mark.asyncio
async def test_navigation_run_is_emulated_before_it_navigates_and_evidence_records_the_size(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    if find_chrome(None) is None:
        pytest.skip("Chrome is not installed")
    with fixture_server() as (base, _), local_chrome(LocalChrome()) as browser:
        async with BrowserSession(browser, Mock()) as session:
            page = GradedPage(session, Config())
            seen: list[tuple[int | None, int | None, float | None]] = []

            async def navigating(self: Agent, *args: object, **kwargs: object) -> object:
                # The stand-in for Agent.run: two real navigations, each read as the grader reads a page.
                for path in ("/contact.html", "/contact.html?again"):
                    await _navigate(session, base + path)
                    e = await page.evidence()
                    seen.append((e.inner_width, e.inner_height, e.device_pixel_ratio))
                return Mock()

            monkeypatch.setattr(Agent, "run", navigating)
            agent = object.__new__(live._ObservedAgent)
            agent._page = page
            token = live._observed.set(live._Observed(emulate_viewport=True, capture_evidence=True))
            try:
                await agent.run("task")
                recorded = live._observed.get().evidence
            finally:
                live._observed.reset(token)
    assert seen == [(1120, 780, 1.0)] * 2
    assert recorded is not None
    assert (recorded.inner_width, recorded.inner_height, recorded.device_pixel_ratio) == (1120, 780, 1.0)
