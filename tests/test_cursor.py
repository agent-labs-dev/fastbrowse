import asyncio
import json
import os
import stat
import sys
from collections.abc import Callable
from pathlib import Path

import pytest

from fastbrowse.browser.cursor import (
    CursorFeedback,
    Geometry,
    Window,
    _Driver,
    browser_window,
    cursor_feedback,
    viewport_point,
)

FAKE_DRIVER = """#!{python}
import json, os, sys, time
log = open(os.environ["FAKE_LOG"], "a")
mode = os.environ.get("FAKE_MODE", "ok")
windows = json.loads(os.environ["FAKE_WINDOWS"])
tools = ["start_session", "end_session", "list_windows", "move_cursor", "set_agent_cursor_enabled"]
if mode == "old":
    tools.remove("move_cursor")
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    if "id" not in message:
        continue
    if method == "initialize":
        if mode == "hang_initialize":
            log.write(json.dumps(["initializing", {{"pid": os.getpid()}}]) + "\\n")
            log.flush()
            time.sleep(30)
        result = {{"capabilities": {{}}}}
    elif method == "tools/list":
        scope = {{"enum": ["window", "desktop"] if mode != "unsafe_scope" else ["desktop"]}}
        schema = {{"properties": {{"scope": scope}}}}
        result = {{"tools": [{{"name": name, "inputSchema": schema}} for name in tools]}}
    else:
        name, arguments = message["params"]["name"], message["params"]["arguments"]
        log.write(json.dumps([name, arguments]) + "\\n")
        log.flush()
        if mode == "die" and name == "move_cursor":
            sys.exit(3)
        if mode == "garbage" and name == "move_cursor":
            print("not json", flush=True)
            continue
        structured = {{"windows": windows}} if name == "list_windows" else {{}}
        result = {{"content": [], "structuredContent": structured}}
    print(json.dumps({{"jsonrpc": "2.0", "id": message["id"], "result": result}}), flush=True)
log.write(json.dumps(["stdin_closed", {{}}]) + "\\n")
"""

GEOMETRY = Geometry(
    browser_pid=5,
    screen_x=100,
    screen_y=80,
    outer_width=900,
    outer_height=700,
    inner_width=900,
    inner_height=613,
    zoom=1,
    pinch_scale=1,
    visible=True,
)


def window(z_index: int | None = 0, **overrides: float) -> dict[str, object]:
    bounds = {"x": 100, "y": 80, "width": 900, "height": 700, **overrides}
    return {"window_id": 1, "pid": 5, "title": "page - Chrome", "bounds": bounds, "z_index": z_index}


def test_viewport_point_adds_the_toolbar_inset() -> None:
    assert viewport_point(GEOMETRY, 10, 20) == (110, 80 + 87 + 20)


def test_viewport_point_follows_page_zoom() -> None:
    zoomed = GEOMETRY.model_copy(update={"zoom": 1.5, "inner_width": 600, "inner_height": 613 / 1.5})
    assert viewport_point(zoomed, 100, 100) == pytest.approx((100 + 150, 80 + 87 + 150))


@pytest.mark.parametrize(
    "update",
    [
        {"visible": False},
        {"pinch_scale": 2},
        {"zoom": 0},
        {"inner_height": 800},  # taller than the window: a negative inset
        {"inner_height": 300},  # a docked panel, not a toolbar
        {"inner_width": 400},  # a side panel
    ],
)
def test_viewport_point_declines_what_it_cannot_place(update: dict[str, object]) -> None:
    assert viewport_point(GEOMETRY.model_copy(update=update), 10, 10) is None


def test_viewport_point_declines_points_outside_the_viewport() -> None:
    assert viewport_point(GEOMETRY, 10, 700) is None
    assert viewport_point(GEOMETRY, -1, 10) is None


def win(identifier: int, z_index: int | None, **overrides: float) -> Window:
    base = {"x": 100.0, "y": 80.0, "width": 900.0, "height": 700.0, **overrides}
    return Window(id=identifier, pid=5, z_index=z_index, **base)


def test_browser_window_must_match_and_be_frontmost() -> None:
    assert browser_window([win(1, 5)], GEOMETRY) is not None
    assert browser_window([win(1, 5), win(2, 3, x=900)], GEOMETRY) is not None
    assert browser_window([win(1, 5), win(2, 6, x=900)], GEOMETRY) is None  # covered
    assert browser_window([win(1, None)], GEOMETRY) is None  # stacking unknown
    assert browser_window([win(1, 5), win(2, None, x=900)], GEOMETRY) is None
    assert browser_window([win(1, 5, width=1800, height=1400)], GEOMETRY) is None  # scaled display
    assert browser_window([win(1, 5), win(2, 4)], GEOMETRY) is None  # two windows claim the frame


@pytest.fixture
def driver(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Callable[..., Path]:
    def install(mode: str = "ok", windows: list[dict[str, object]] | None = None) -> Path:
        binary = tmp_path / "bin" / "cua-driver"
        binary.parent.mkdir(exist_ok=True)
        binary.write_text(FAKE_DRIVER.format(python=sys.executable))
        binary.chmod(binary.stat().st_mode | stat.S_IEXEC)
        log = tmp_path / "log"
        log.touch()
        monkeypatch.setenv("PATH", str(binary.parent))
        monkeypatch.setenv("DISPLAY", ":99")
        monkeypatch.delenv("WAYLAND_DISPLAY", raising=False)
        monkeypatch.setenv("FAKE_LOG", str(log))
        monkeypatch.setenv("FAKE_MODE", mode)
        monkeypatch.setenv("FAKE_WINDOWS", json.dumps([window()] if windows is None else windows))
        return log

    return install


def calls(log: Path) -> list[str]:
    return [json.loads(line)[0] for line in log.read_text().splitlines()]


async def reading() -> Geometry:
    return GEOMETRY


async def settle(feedback: CursorFeedback) -> None:
    await feedback.settled()


async def test_off_by_default_starts_nothing(driver: Callable[..., Path]) -> None:
    log = driver()
    async with cursor_feedback(False) as feedback:
        assert feedback is None
    assert log.read_text() == ""


async def test_missing_binary_or_display_is_not_an_error(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setenv("PATH", str(tmp_path))
    async with cursor_feedback(True) as feedback:
        assert feedback is None


async def test_no_display_runs_without_feedback(driver: Callable[..., Path], monkeypatch: pytest.MonkeyPatch) -> None:
    driver()
    monkeypatch.delenv("DISPLAY")
    async with cursor_feedback(True) as feedback:
        assert feedback is None


async def test_an_old_driver_without_the_tools_is_skipped(driver: Callable[..., Path]) -> None:
    driver("old")
    async with cursor_feedback(True) as feedback:
        assert feedback is None


async def test_moves_the_overlay_and_ends_its_session(driver: Callable[..., Path]) -> None:
    log = driver()
    async with cursor_feedback(True) as feedback:
        assert feedback is not None
        feedback.follow(reading, (10, 20))
        await settle(feedback)
    entries = [json.loads(line) for line in log.read_text().splitlines()]
    moves = [arguments for name, arguments in entries if name == "move_cursor"]
    assert [(m["x"], m["y"], m["scope"]) for m in moves] == [(110, 187, "window")]
    assert [name for name, _ in entries][-2:] == ["end_session", "stdin_closed"]


async def test_a_covered_window_hides_the_cursor_and_a_clear_one_shows_it(driver: Callable[..., Path]) -> None:
    log = driver(windows=[window(1), {**window(9, x=900), "window_id": 2}])
    async with cursor_feedback(True) as feedback:
        assert feedback is not None
        feedback.follow(reading, (10, 20))
        await settle(feedback)
    entries = [json.loads(line) for line in log.read_text().splitlines()]
    assert [a["enabled"] for n, a in entries if n == "set_agent_cursor_enabled"] == [False]
    assert "move_cursor" not in calls(log)


async def test_the_overlay_never_counts_as_a_covering_window(driver: Callable[..., Path]) -> None:
    overlay = {
        "window_id": 7,
        "pid": None,
        "title": "Cua.AgentCursorOverlay.default",
        "z_index": 9,
        "bounds": {"x": 0, "y": 0, "width": 1280, "height": 900},
    }
    log = driver(windows=[window(1), overlay])
    async with cursor_feedback(True) as feedback:
        assert feedback is not None
        feedback.follow(reading, (10, 20))
        await settle(feedback)
    assert "move_cursor" in calls(log)


async def test_a_window_without_a_process_id_still_covers_the_page(driver: Callable[..., Path]) -> None:
    pidless = {
        "window_id": 8,
        "pid": None,
        "title": "tk",
        "z_index": 8,
        "bounds": {"x": 700, "y": 300, "width": 500, "height": 400},
    }
    log = driver(windows=[window(1), pidless])
    async with cursor_feedback(True) as feedback:
        assert feedback is not None
        feedback.follow(reading, (10, 20))
        await settle(feedback)
    assert "move_cursor" not in calls(log)


async def test_an_uncertain_mapping_draws_nothing(driver: Callable[..., Path]) -> None:
    log = driver()

    async def unknown() -> Geometry | None:
        return None

    async with cursor_feedback(True) as feedback:
        assert feedback is not None
        feedback.follow(unknown, (10, 20))
        await settle(feedback)
    assert "move_cursor" not in calls(log)
    assert "list_windows" not in calls(log)


@pytest.mark.parametrize("mode", ["die", "garbage"])
async def test_a_failing_driver_turns_the_feature_off_and_is_cleaned_up(driver: Callable[..., Path], mode: str) -> None:
    log = driver(mode)
    async with cursor_feedback(True) as feedback:
        assert feedback is not None
        feedback.follow(reading, (10, 20))
        await settle(feedback)
        feedback.follow(reading, (11, 21))  # off for the rest of the run: nothing more is sent
        await settle(feedback)
        assert calls(log).count("move_cursor") == 1
    assert not feedback.running


async def test_geometry_failure_cannot_escape_the_follower(driver: Callable[..., Path]) -> None:
    driver()

    async def broken() -> Geometry | None:
        raise RuntimeError("secret page text")

    async with cursor_feedback(True) as feedback:
        assert feedback is not None
        feedback.follow(broken, (1, 1))
        await settle(feedback)


async def test_cancellation_still_stops_the_driver(driver: Callable[..., Path]) -> None:
    driver()
    holder: list[CursorFeedback] = []
    started = asyncio.Event()

    async def run() -> None:
        async with cursor_feedback(True) as feedback:
            assert feedback is not None
            holder.append(feedback)
            started.set()
            await asyncio.sleep(30)

    task = asyncio.create_task(run())
    await asyncio.wait_for(started.wait(), 3)
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    assert not holder[0].running


def test_viewport_point_excludes_the_bottom_window_border() -> None:
    geometry = GEOMETRY.model_copy(update={"inner_width": 892, "inner_height": 609})
    assert viewport_point(geometry, 10, 20) == (114, 80 + 87 + 20)


async def test_runtime_without_visual_only_scope_is_skipped(driver: Callable[..., Path]) -> None:
    log = driver("unsafe_scope")
    async with cursor_feedback(True) as feedback:
        assert feedback is None
    assert "move_cursor" not in calls(log)


async def test_cancellation_during_initialization_stops_the_child(
    driver: Callable[..., Path], monkeypatch: pytest.MonkeyPatch
) -> None:
    driver("hang_initialize")
    started = asyncio.Event()
    children: list[int] = []
    original = _Driver.request

    async def request(self: _Driver, method: str, params: dict[str, object] | None = None, *, seconds: float) -> object:
        children.append(self._process.pid)
        started.set()
        return await original(self, method, params, seconds=seconds)

    monkeypatch.setattr(_Driver, "request", request)

    async def opening() -> None:
        async with cursor_feedback(True):
            pytest.fail("initialization should remain pending")

    task = asyncio.create_task(opening())
    await asyncio.wait_for(started.wait(), 3)
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    with pytest.raises(ProcessLookupError):
        os.kill(children[0], 0)


async def test_a_spawn_failure_does_not_break_the_run(
    driver: Callable[..., Path], monkeypatch: pytest.MonkeyPatch
) -> None:
    driver()

    async def denied(*args: object, **kwargs: object) -> asyncio.subprocess.Process:
        raise PermissionError("not executable")

    monkeypatch.setattr(asyncio, "create_subprocess_exec", denied)
    async with cursor_feedback(True) as feedback:
        assert feedback is None


async def test_cancellation_while_spawning_reaps_the_child(
    driver: Callable[..., Path], monkeypatch: pytest.MonkeyPatch
) -> None:
    driver()
    original = asyncio.create_subprocess_exec
    spawned = asyncio.Event()
    release = asyncio.Event()
    children: list[asyncio.subprocess.Process] = []

    async def spawn(
        binary: str, mode: str, *, stdin: int, stdout: int, stderr: int, limit: int, env: dict[str, str]
    ) -> asyncio.subprocess.Process:
        process = await original(binary, mode, stdin=stdin, stdout=stdout, stderr=stderr, limit=limit, env=env)
        children.append(process)
        spawned.set()
        await release.wait()
        return process

    monkeypatch.setattr(asyncio, "create_subprocess_exec", spawn)

    async def opening() -> None:
        async with cursor_feedback(True):
            pytest.fail("opening should be cancelled")

    task = asyncio.create_task(opening())
    await asyncio.wait_for(spawned.wait(), 3)
    task.cancel()
    release.set()
    with pytest.raises(asyncio.CancelledError):
        await task
    assert children[0].returncode is not None


async def test_a_later_page_change_hides_the_previous_cursor(driver: Callable[..., Path]) -> None:
    log = driver()
    changed = asyncio.Event()

    async def geometry() -> Geometry | None:
        return None if changed.is_set() else GEOMETRY

    async with cursor_feedback(True) as feedback:
        assert feedback is not None
        feedback.follow(geometry, (10, 20))
        await feedback.settled()
        changed.set()
        assert feedback._watching is not None
        await asyncio.wait_for(feedback._watching, 3)
    entries = [json.loads(line) for line in log.read_text().splitlines()]
    assert [args["enabled"] for name, args in entries if name == "set_agent_cursor_enabled"] == [False]
    assert sum(name == "move_cursor" for name, _ in entries) == 1


def test_matching_bounds_do_not_prove_a_different_process_is_the_browser() -> None:
    other = win(1, 0).model_copy(update={"pid": 42})
    assert browser_window([other], GEOMETRY) is None
    assert browser_window([win(1, 0)], GEOMETRY.model_copy(update={"browser_pid": None})) is None


async def test_wayland_is_skipped_even_with_an_xwayland_display(
    driver: Callable[..., Path], monkeypatch: pytest.MonkeyPatch
) -> None:
    log = driver()
    monkeypatch.setenv("WAYLAND_DISPLAY", "wayland-0")
    async with cursor_feedback(True) as feedback:
        assert feedback is None
    assert not calls(log)


async def test_cancelled_visibility_reply_is_resent_before_the_next_move(
    driver: Callable[..., Path], monkeypatch: pytest.MonkeyPatch
) -> None:
    log = driver()
    async with cursor_feedback(True) as feedback:
        assert feedback is not None
        original = feedback._driver.tool
        hidden = asyncio.Event()

        async def tool(name: str, arguments: dict[str, object]) -> dict[str, object]:
            result = await original(name, arguments)
            if name == "set_agent_cursor_enabled" and not arguments["enabled"]:
                hidden.set()
                await asyncio.Event().wait()
            return result

        monkeypatch.setattr(feedback._driver, "tool", tool)
        hiding = asyncio.create_task(feedback._show(False))
        await asyncio.wait_for(hidden.wait(), 3)
        hiding.cancel()
        with pytest.raises(asyncio.CancelledError):
            await hiding
        await feedback._show(True)
    entries = [json.loads(line) for line in log.read_text().splitlines()]
    assert [args["enabled"] for name, args in entries if name == "set_agent_cursor_enabled"] == [False, True]


async def test_cancelling_session_cleanup_still_reaps_the_driver(
    driver: Callable[..., Path], monkeypatch: pytest.MonkeyPatch
) -> None:
    driver()
    async with cursor_feedback(True) as feedback:
        assert feedback is not None
        original = feedback._driver.tool
        ending = asyncio.Event()

        async def tool(name: str, arguments: dict[str, object]) -> dict[str, object]:
            result = await original(name, arguments)
            if name == "end_session":
                ending.set()
                await asyncio.Event().wait()
            return result

        monkeypatch.setattr(feedback._driver, "tool", tool)
        closing = asyncio.create_task(feedback.close())
        await asyncio.wait_for(ending.wait(), 3)
        closing.cancel()
        with pytest.raises(asyncio.CancelledError):
            await closing
        assert not feedback.running


async def test_cancelling_driver_close_reaps_child_and_propagates_cancellation() -> None:
    from unittest.mock import AsyncMock, Mock

    process = Mock()
    process.returncode = None
    started = asyncio.Event()

    calls = 0

    async def wait():
        nonlocal calls
        calls += 1
        if calls == 1:
            started.set()
            await asyncio.Event().wait()
        return 0

    process.wait = AsyncMock(side_effect=wait)
    closing = asyncio.create_task(_Driver(process).close())
    await started.wait()
    closing.cancel()
    with pytest.raises(asyncio.CancelledError):
        await closing
    process.kill.assert_called_once()
    assert process.wait.await_count == 2
