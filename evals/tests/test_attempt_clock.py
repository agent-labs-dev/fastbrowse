"""One clock for every arm: seconds start at session ready, the agent has no limit, and opening and closing are bounded."""

import asyncio
import contextlib
import time
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest.mock import AsyncMock

import httpx
import pytest

from fastbrowse_evals import attempt_clock, live, official
from fastbrowse_evals.attempt_clock import (
    TIMING_BASIS,
    AttemptClock,
    elapsed,
    session_answered,
    session_ready,
)
from fastbrowse_evals.browsers import Browser
from fastbrowse_evals.live_tasks import TASKS

ARXIV = next(t for t in TASKS if t.id == "arxiv-title")


@pytest.fixture
def short(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(attempt_clock, "SESSION_OPEN_SECONDS", 0.02)
    monkeypatch.setattr(attempt_clock, "SESSION_CLOSE_SECONDS", 0.02)
    monkeypatch.setattr(live, "semantic_grade", AsyncMock(return_value=(True, "ok", {})))


async def test_seconds_run_from_ready_to_the_answer_and_the_phases_follow() -> None:
    async with AttemptClock(bounded=True) as clock:
        assert clock.phase == "open"
        assert clock.seconds() == 0.0
        await asyncio.sleep(0.03)
        began = session_ready()
        session_ready()  # a second mark moves nothing
        assert clock.ready_at == began and clock.phase == "agent"
        assert clock.open_seconds is not None and clock.open_seconds >= 0.02
        await asyncio.sleep(0.02)
        ended = session_answered()
        await asyncio.sleep(0.03)
    assert clock.phase == "close"
    assert clock.seconds() == pytest.approx(ended - began) == elapsed()


async def test_a_diagnostic_clock_stops_nothing(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(attempt_clock, "SESSION_OPEN_SECONDS", 0.0)
    async with AttemptClock(bounded=False) as clock:
        await asyncio.sleep(0.01)
    assert not clock.expired()


async def test_the_agent_phase_has_no_deadline(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(attempt_clock, "SESSION_OPEN_SECONDS", 0.02)
    monkeypatch.setattr(attempt_clock, "SESSION_CLOSE_SECONDS", 0.02)
    async with AttemptClock(bounded=True) as clock:
        session_ready()
        assert clock._timeout.when() is None
        await asyncio.sleep(0.1)  # well past both session bounds
        session_answered()
    assert not clock.expired()


async def run_with(runner: Any, monkeypatch: pytest.MonkeyPatch) -> live.EvalRow:
    monkeypatch.setitem(live.ARMS, "fastbrowse", live.ARMS["fastbrowse"].model_copy(update={"runner": runner}))
    async with httpx.AsyncClient() as http:
        return await live.run_arm("fastbrowse", ARXIV, None, http, Path(), record=None)


@pytest.mark.usefixtures("short")
async def test_a_hung_open_is_an_outage_never_an_agent_failure(monkeypatch: pytest.MonkeyPatch) -> None:
    async def hung(*_: object, **__: object) -> Any:
        await asyncio.Event().wait()

    row = await run_with(hung, monkeypatch)
    assert row.status == "unavailable" and "did not open" in (row.failure or "")
    assert row.seconds == 0.0 and row.timing_basis == TIMING_BASIS


@pytest.mark.usefixtures("short")
async def test_a_hung_close_after_the_answer_is_an_outage_never_an_agent_failure(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def hung(*_: object, **__: object) -> Any:
        session_ready()
        session_answered()
        await asyncio.Event().wait()

    row = await run_with(hung, monkeypatch)
    assert row.status == "unavailable" and "did not close" in (row.failure or "")


def hosted_listing(*items: SimpleNamespace) -> Any:
    return SimpleNamespace(browsers=SimpleNamespace(list=AsyncMock(return_value=SimpleNamespace(items=list(items)))))


async def test_a_hosted_session_is_ready_when_its_browser_is_listed() -> None:
    now = object()
    client = hosted_listing(SimpleNamespace(cdp_url="wss://x", agent_session_id="s1", started_at=now))
    finishing: asyncio.Future[None] = asyncio.get_running_loop().create_future()
    async with AttemptClock(bounded=True) as clock:
        assert await live.hosted_session_ready(client, "s1", finishing) is now
        assert clock.phase == "agent"
    finishing.cancel()


async def test_a_hosted_session_with_no_browser_never_marks_ready() -> None:
    finishing = asyncio.ensure_future(asyncio.sleep(0))
    async with AttemptClock(bounded=True) as clock:
        assert await live.hosted_session_ready(hosted_listing(), "s1", finishing) is None
    assert clock.phase == "open"


async def test_browser_setup_leaves_the_clock_until_the_first_page() -> None:
    clock = AttemptClock(bounded=True)
    async with clock:
        clock.ready(100.0)
        clock.asked(103.0)
        clock.page_ready(107.5)
        clock.page_ready(109.0)  # a page already told is not told again
        assert clock.opened == 4.5
        clock.answered(120.0)
    assert clock.seconds() == 15.5


async def test_setup_that_never_reached_a_page_is_none_of_the_agents() -> None:
    clock = AttemptClock(bounded=False)
    async with clock:
        clock.ready(100.0)
        clock.asked(103.0)
        assert clock.seconds() == 3.0
        clock.answered(110.0)
    assert clock.seconds() == 3.0 and clock.opened == 7.0


async def test_setup_marks_reach_the_clock_from_another_context() -> None:
    import contextvars

    clock = AttemptClock(bounded=False)
    async with clock:
        session_ready()
        asked, page_ready = attempt_clock.setup_marks()
        asked()
        await asyncio.create_task(page_ready(), context=contextvars.Context())
    assert clock.asked_at is None and clock.opened >= 0.0


def test_browser_round_trips_add_up_from_the_trace_phases() -> None:
    events = [
        {"event": "phase", "phase": "open", "began": 0.0, "ended": 1.0},
        {"event": "phase", "phase": "act", "began": 1.0, "ended": 2.5},
        {"event": "phase", "phase": "observe", "began": 2.5, "ended": 3.0},
        {"event": "phase", "phase": "decide", "began": 3.0, "ended": 9.0},
    ]
    assert live.phase_seconds(events, live.BROWSER_PHASES) == 2.0


def test_old_ledgers_read_with_no_basis() -> None:
    fields = {"arm": "a", "task": "t", "category": "c", "at": 0.0, "status": "s", "seconds": 1.0, "dollars": None}
    row = live.EvalRow.model_validate(
        fields | {"normalized_status": "done", "correct": None, "passed": None, "failure": None}
    )
    assert row.timing_basis is None


def task_args(tmp_path: Path) -> SimpleNamespace:
    return SimpleNamespace(out=tmp_path, suite=official.Suite.BU_BENCH, authorize=False)


@contextlib.asynccontextmanager
async def hanging_open(*_: object, **__: object) -> Any:
    await asyncio.Event().wait()
    yield Browser("wss://never")


async def test_official_failed_open_is_retried_never_an_agent_failure(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(attempt_clock, "SESSION_OPEN_SECONDS", 0.01)
    monkeypatch.setattr(official.backend, "environment", lambda: {"BROWSER_USE_API_KEY": "k"})
    monkeypatch.setattr(official, "cloud_browser", hanging_open)
    with pytest.raises(official.Interrupted, match="did not open") as raised:
        await official.execute_task(task_args(tmp_path), {"task_id": "b1", "confirmed_task": "g"}, 0)
    assert raised.value.run["agent_cost_unknown"] is False and raised.value.run["timing_basis"] == TIMING_BASIS
    saved = official.saved_agent(tmp_path / "b1-0")
    assert saved["timing_basis"] == TIMING_BASIS and saved["seconds"] == 0.0


def test_agent_seconds_leave_out_browser_calls_and_stay_unmeasured_without_them() -> None:
    assert live.agent_seconds(7.7, 5.02) == 2.68
    assert live.agent_seconds(7.7, None) is None


async def test_teardown_after_the_run_phase_is_off_the_agents_clock() -> None:
    """Encoding a recording runs inside `run_task` after the agent answered (#951): the clock stops where the trace's
    run phase ended, not when `run_task` returned."""
    async with AttemptClock(bounded=False) as clock:
        began = session_ready()
        await asyncio.sleep(0.02)
        events: list[object] = [
            {"event": "phase", "phase": "open", "began": began, "ended": began + 0.005},
            {"event": "phase", "phase": "run", "began": began, "ended": time.monotonic()},
        ]
        await asyncio.sleep(0.05)  # the encode
        ended = session_answered(live.run_ended_at(events))
    assert clock.seconds() == pytest.approx(ended - began)
    assert clock.seconds() < 0.045
    assert live.run_ended_at([{"event": "phase", "phase": "open", "ended": 1.0}]) is None
