"""The recording's completion screen must leave an attached application available for the next run."""

import asyncio
import shutil
from pathlib import Path
from typing import Any

import pytest

from fastbrowse.agent import Agent
from fastbrowse.browser.page import CdpPage
from fastbrowse.browser.session import BrowserSession
from fastbrowse.models import BrowserConnection, CostBreakdown, RunResult, Status
from fastbrowse.run import run_task
from tests.browser.conftest import RecordingArtifactSink
from tests.browser.test_browser import eval_value
from tests.test_policy import ScriptedJev
from tests.test_retrieval import ScriptedLLM


async def test_recording_preserves_attached_page_geometry_and_reattachment(
    page: CdpPage,
    chrome_connection: BrowserConnection,
    main_site: str,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    if shutil.which("ffmpeg") is None:
        pytest.skip("recording needs ffmpeg")
    await page.navigate(f"{main_site}/dispatch.html")
    session = page._session
    await eval_value(
        session,
        session.active_session_id,
        "document.body.style.height = '5000px'; window.scrollTo(0, 500)",
    )
    geometry = "JSON.stringify([location.href, innerHeight, scrollY, document.documentElement.scrollHeight])"
    before = await eval_value(session, session.active_session_id, geometry)

    async def finish(agent: Agent, task: str, **_kwargs: Any) -> RunResult:
        await agent._page.capture()
        await asyncio.sleep(0.4)
        return RunResult(
            status=Status.COMPLETE, answer=task, data=None, evidence=(), steps=(),
            cost=CostBreakdown(lines=()), artifacts=(),
        )  # fmt: skip

    monkeypatch.setattr(Agent, "run", finish)
    recording = tmp_path / "attached.mp4"
    result = await run_task(
        "Inspect this page",
        cdp_url=chrome_connection.cdp_url,
        target_match="/dispatch.html",
        record=recording,
        jev=ScriptedJev({}),
        llm=ScriptedLLM([]),
    )
    assert result.status is Status.COMPLETE
    assert recording.is_file() and recording.stat().st_size > 0
    assert await eval_value(session, session.active_session_id, geometry) == before
    connection = chrome_connection.model_copy(update={"attach": True, "target_match": "/dispatch.html"})
    async with BrowserSession(connection, RecordingArtifactSink()) as attached:
        assert await eval_value(attached, attached.active_session_id, geometry) == before
