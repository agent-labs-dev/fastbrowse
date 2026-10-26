"""`record` and `cursor` on the Rust core: both videos written and reported as Python reports them, and a cursor
that cannot be drawn leaves the run to go on without it."""

import asyncio
import importlib
import shutil
from collections.abc import Mapping
from pathlib import Path

import pytest
from pydantic import JsonValue

from fastbrowse import run_task
from fastbrowse.browser.recording import RecordingError
from fastbrowse.jev import Evaluation, Question
from fastbrowse.models import BrowserConnection, Status
from fastbrowse.scripted import ScriptedJev, ScriptedLLM, steps

try:
    NATIVE = hasattr(importlib.import_module("fastbrowse_native"), "run_task")
except ImportError:
    NATIVE = False

pytestmark = pytest.mark.skipif(not NATIVE, reason="fastbrowse_native with run_task is not installed")
needs_ffmpeg = pytest.mark.skipif(shutil.which("ffmpeg") is None, reason="recording needs ffmpeg")


@needs_ffmpeg
@pytest.mark.timeout(120)
@pytest.mark.parametrize("core", ["python", "rust"])
async def test_a_recorded_run_reports_both_videos(
    core: str,
    chrome_connection: BrowserConnection,
    main_site: str,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    if core == "rust":
        monkeypatch.setenv("FASTBROWSE_RUST_CORE", "1")
    else:
        monkeypatch.delenv("FASTBROWSE_RUST_CORE", raising=False)
    record = tmp_path / "run.mp4"
    result = await run_task(
        "Open the dispatch page",
        start=f"{main_site}/dispatch.html",
        cdp_url=chrome_connection.cdp_url,
        record=record,
        jev=ScriptedJev(steps("")),
        llm=ScriptedLLM(),
    )
    assert result.status is Status.COMPLETE, result.error
    assert sorted(path.name for path in result.recordings) == ["run.mp4", "run.plain.mp4"]
    assert all(path.stat().st_size > 0 for path in result.recordings)


async def test_a_recording_that_cannot_start_raises_recording_error_on_the_rust_core(
    chrome_connection: BrowserConnection, main_site: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("FASTBROWSE_RUST_CORE", "1")
    monkeypatch.setenv("PATH", str(tmp_path))
    with pytest.raises(RecordingError, match="ffmpeg"):
        await run_task(
            "Open the dispatch page",
            start=f"{main_site}/dispatch.html",
            cdp_url=chrome_connection.cdp_url,
            record=tmp_path / "run.mp4",
            jev=ScriptedJev(steps("")),
            llm=ScriptedLLM(),
        )


async def test_cursor_runs_on_the_rust_core_and_draws_nothing_where_it_cannot(
    chrome_connection: BrowserConnection, main_site: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("FASTBROWSE_RUST_CORE", "1")
    # No cua-driver on PATH, so there is nothing to draw with and the run goes on without it, as on Python.
    monkeypatch.setenv("PATH", "")
    result = await run_task(
        "Open the dispatch page",
        start=f"{main_site}/dispatch.html",
        cdp_url=chrome_connection.cdp_url,
        cursor=True,
        jev=ScriptedJev(steps("")),
        llm=ScriptedLLM(),
    )
    assert result.status is Status.COMPLETE, result.error


@needs_ffmpeg
@pytest.mark.timeout(120)
@pytest.mark.parametrize("core", ["python", "rust"])
async def test_a_run_cancelled_during_the_result_card_still_writes_both_videos(
    core: str,
    chrome_connection: BrowserConnection,
    main_site: str,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    if core == "rust":
        monkeypatch.setenv("FASTBROWSE_RUST_CORE", "1")
    else:
        monkeypatch.delenv("FASTBROWSE_RUST_CORE", raising=False)
    finished = asyncio.Event()
    jev = ScriptedJev(steps(""))
    evaluate = jev.evaluate

    async def judged(state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        # The done check is the run's last question; the card is shown right after it.
        if "complete" in questions:
            finished.set()
        return await evaluate(state, questions)

    jev.evaluate = judged  # ty: ignore[invalid-assignment]
    record = tmp_path / "run.mp4"
    run = asyncio.create_task(
        run_task(
            "Open the dispatch page",
            start=f"{main_site}/dispatch.html",
            cdp_url=chrome_connection.cdp_url,
            record=record,
            jev=jev,
            llm=ScriptedLLM(),
        )
    )
    await asyncio.wait_for(finished.wait(), 60)
    await asyncio.sleep(1.5)
    run.cancel()
    with pytest.raises(asyncio.CancelledError):
        await run
    assert record.stat().st_size > 0
    assert record.with_name("run.plain.mp4").stat().st_size > 0
