"""Fixture consent remains visible through the runner's real browser session."""

import asyncio
from pathlib import Path
from unittest.mock import Mock

import httpx
import pytest

from fastbrowse.adapters.local_chrome import find_chrome, local_chrome
from fastbrowse.artifacts import DirectorySink
from fastbrowse.browser import BrowserSession, CdpPage
from fastbrowse.config import Config
from fastbrowse.evals import runner
from fastbrowse.evals.mock import mock_site
from fastbrowse.evals.mock_tasks import TASKS
from fastbrowse.models import CostBreakdown, LocalChrome, RunResult, Status


async def _labels_after_modal(refuse_cookie_banners: bool) -> set[str]:
    """The controls on /portal after the modal appears, run the way the fixture runner builds its session."""
    with mock_site() as (base, _site), local_chrome(LocalChrome()) as browser:
        async with BrowserSession(browser, Mock(), refuse_cookie_banners=refuse_cookie_banners) as session:
            page = CdpPage(session, Config(refuse_cookie_banners=refuse_cookie_banners))
            await page.navigate(base + "/portal", 10.0)
            await page.observe()
            # The modal is scripted to appear 1200ms after load.
            await asyncio.sleep(1.5)
            return {control.label for control in (await page.observe()).controls}


def _skip_without_chrome() -> None:
    if find_chrome(None) is None:
        pytest.skip("Chrome is not installed")


async def test_fixture_session_leaves_the_scripted_banner_for_the_agent(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    _skip_without_chrome()

    class Observer:
        def __init__(self, page: CdpPage, *args: object, **kwargs: object) -> None:
            self.page = page

        async def run(self, task: str, *, start: str, **kwargs: object) -> RunResult:
            await self.page.navigate(start, 10.0)
            await self.page.observe()
            await asyncio.sleep(1.5)
            labels = [control.label for control in (await self.page.observe()).controls]
            return RunResult(
                status=Status.COMPLETE,
                answer=None,
                data=labels,
                evidence=(),
                steps=(),
                cost=CostBreakdown(),
                artifacts=(),
            )

    monkeypatch.setattr(runner, "Agent", Observer)
    task = next(task for task in TASKS if task.id == "mock-support-portal")
    with mock_site() as (base, _site), local_chrome(LocalChrome()) as browser:
        async with httpx.AsyncClient() as http:
            result, _, _ = await runner._drive(
                task, base + task.start, browser, http, DirectorySink(tmp_path), Mock(), None, runner.MOCK_LIMITS
            )
    assert isinstance(result.data, list)
    assert "Accept all" in result.data
    assert "Dismiss" in result.data


@pytest.mark.asyncio
async def test_the_default_browser_config_would_remove_the_banner() -> None:
    """The mechanism the fixture config opts out of, pinned so the reason for the opt-out cannot be lost."""
    _skip_without_chrome()
    labels = await _labels_after_modal(Config().refuse_cookie_banners)
    assert "Accept all" not in labels
