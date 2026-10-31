"""`record`, `cursor` and step frames on the Rust core: both videos written and reported, a cursor that cannot be drawn
leaves the run to go on without it, and the final frame reaches the caller."""

import asyncio
import shutil
from collections.abc import Mapping
from pathlib import Path

import pytest
from pydantic import JsonValue

from fastbrowse import run_task
from fastbrowse.config import Config
from fastbrowse.jev import Evaluation, Question
from fastbrowse.models import RecordingError, Status
from tests.scripted import ScriptedJev, ScriptedLLM, steps

needs_ffmpeg = pytest.mark.skipif(shutil.which("ffmpeg") is None, reason="recording needs ffmpeg")


@needs_ffmpeg
@pytest.mark.timeout(120)
async def test_a_recorded_run_reports_both_videos(
    cdp_url: str,
    main_site: str,
    tmp_path: Path,
) -> None:
    record = tmp_path / "run.mp4"
    result = await run_task(
        "Open the dispatch page",
        start=f"{main_site}/dispatch.html",
        cdp_url=cdp_url,
        record=record,
        jev=ScriptedJev(steps("")),
        llm=ScriptedLLM(),
    )
    assert result.status is Status.COMPLETE, result.error
    assert sorted(path.name for path in result.recordings) == ["run.mp4", "run.plain.mp4"]
    assert all(path.stat().st_size > 0 for path in result.recordings)


@pytest.mark.timeout(120)
async def test_a_run_with_step_frames_returns_its_final_frame(cdp_url: str, main_site: str) -> None:
    result = await run_task(
        "Open the dispatch page",
        start=f"{main_site}/dispatch.html",
        cdp_url=cdp_url,
        config=Config(step_frames=True),
        jev=ScriptedJev(steps("")),
        llm=ScriptedLLM(),
    )
    assert result.status is Status.COMPLETE, result.error
    assert result.final_frame is not None and result.final_frame.startswith(b"\x89PNG")


async def test_a_recording_that_cannot_start_raises_recording_error(
    cdp_url: str, main_site: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("PATH", str(tmp_path))
    with pytest.raises(RecordingError, match="ffmpeg"):
        await run_task(
            "Open the dispatch page",
            start=f"{main_site}/dispatch.html",
            cdp_url=cdp_url,
            record=tmp_path / "run.mp4",
            jev=ScriptedJev(steps("")),
            llm=ScriptedLLM(),
        )


async def test_a_cursor_draws_nothing_where_it_cannot_and_the_run_goes_on(
    cdp_url: str, main_site: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    # No cua-driver on PATH, so there is nothing to draw with and the run goes on without it.
    monkeypatch.setenv("PATH", "")
    result = await run_task(
        "Open the dispatch page",
        start=f"{main_site}/dispatch.html",
        cdp_url=cdp_url,
        cursor=True,
        jev=ScriptedJev(steps("")),
        llm=ScriptedLLM(),
    )
    assert result.status is Status.COMPLETE, result.error


@needs_ffmpeg
@pytest.mark.timeout(120)
async def test_a_run_cancelled_during_the_result_card_still_writes_both_videos(
    cdp_url: str,
    main_site: str,
    tmp_path: Path,
) -> None:
    finished = asyncio.Event()
    jev = ScriptedJev(steps(""))
    evaluate = jev.evaluate

    async def judged(state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        # The plan requires nothing, so the done check asks Jev nothing and the step that chose done is the run's
        # last question; the card is shown right after it.
        if "operation" in questions:
            finished.set()
        return await evaluate(state, questions)

    jev.evaluate = judged  # ty: ignore[invalid-assignment]
    record = tmp_path / "run.mp4"
    run = asyncio.create_task(
        run_task(
            "Open the dispatch page",
            start=f"{main_site}/dispatch.html",
            cdp_url=cdp_url,
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
