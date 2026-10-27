"""Fixture consent remains visible to the agent the runner starts."""

import asyncio
from pathlib import Path
from typing import Any

import pytest

from fastbrowse import connect_cdp
from fastbrowse.config import Config
from fastbrowse.evals import runner
from fastbrowse.evals.harness import chrome, find_chrome
from fastbrowse.evals.mock import mock_site
from fastbrowse.evals.mock_tasks import TASKS
from fastbrowse.models import CostBreakdown, RunResult, Status

needs_chrome = pytest.mark.skipif(find_chrome() is None, reason="Chrome is not installed")


async def _labels_after_modal(config: Config) -> set[str]:
    """The controls on /portal after the modal appears, on a page opened with `config`."""
    with mock_site() as (base, _site), chrome() as cdp_url:
        async with connect_cdp(cdp_url=cdp_url, attach=False, config=config) as page:
            await page.navigate(base + "/portal", 10.0)
            await page.observe()
            # The modal is scripted to appear 1200ms after load.
            await asyncio.sleep(1.5)
            return {control.label for control in (await page.observe()).controls}


async def test_the_runner_runs_every_task_with_the_fixture_config(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    asked: dict[str, Any] = {}

    async def run_task(task: str, **kwargs: Any) -> RunResult:
        asked.update(kwargs)
        return RunResult(
            status=Status.COMPLETE, answer=None, data=None, evidence=(), steps=(), cost=CostBreakdown(), artifacts=()
        )

    monkeypatch.setattr(runner, "run_fastbrowse", run_task)
    task = next(task for task in TASKS if task.id == "mock-support-portal")
    await runner._drive(task, "http://127.0.0.1:1/portal", "ws://unused", tmp_path, None, runner.MOCK_LIMITS)
    assert asked["config"] == runner.fixture_config()
    assert asked["downloads"] == tmp_path


@needs_chrome
async def test_the_fixture_config_leaves_the_scripted_banner_for_the_agent() -> None:
    labels = await _labels_after_modal(runner.fixture_config())
    assert "Accept all" in labels
    assert "Dismiss" in labels


@needs_chrome
async def test_the_default_browser_config_would_remove_the_banner() -> None:
    """The mechanism the fixture config opts out of, pinned so the reason for the opt-out cannot be lost."""
    labels = await _labels_after_modal(Config())
    assert "Accept all" not in labels
