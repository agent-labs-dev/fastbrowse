"""Optional on-screen cursor feedback for a visible browser, drawn by the Cua Driver.

Cua Driver's `move_cursor` moves a synthetic overlay and leaves the user's pointer, focus and the browser alone
(only `scope=desktop` would move the real pointer, which is never sent here). It is purely visual: the click itself
is still sent over CDP, so nothing this module does can change what an action did, and it is never retried.

Every failure turns the feature off for the rest of the run, quietly: a missing binary, a headless machine, an
old driver, a window that cannot be mapped. The driver is one `cua-driver mcp` child owned by the run and
killed when it ends, so a crashed run cannot leave a daemon behind.
"""

import asyncio
import contextlib
import json
import logging
import os
import secrets
import shutil
import sys
import time
from collections.abc import AsyncGenerator, Awaitable, Callable
from contextlib import asynccontextmanager
from typing import Any

from fastbrowse.models import Frozen
from fastbrowse.spawn import system_environment

logger = logging.getLogger(__name__)

_TOOLS = frozenset({"start_session", "end_session", "list_windows", "move_cursor", "set_agent_cursor_enabled"})
_CALL_SECONDS = 2.0
_START_SECONDS = 5.0
_STOP_SECONDS = 1.0
# The driver ends an idle session after about five minutes; start_session is idempotent, so it is the keepalive.
_KEEPALIVE_SECONDS = 60.0
# The window frame can be a few pixels off from the driver's own report; a different size means a scaled display.
_BOUNDS_TOLERANCE = 2
# The page sits below the browser's own toolbar. Past this, a docked DevTools panel is likelier than chrome.
_MAX_TOP_INSET = 300
_MAX_SIDE_INSET = 40
_OVERLAY_TITLE = "Cua.AgentCursorOverlay"


class Geometry(Frozen):
    """What the page itself reports about where its viewport sits in its window, in device-independent pixels."""

    browser_pid: int | None = None
    screen_x: float
    screen_y: float
    outer_width: float
    outer_height: float
    inner_width: float
    """In CSS pixels, as `window.innerWidth`."""
    inner_height: float
    zoom: float
    """Device-independent pixels per CSS pixel: the browser's page zoom."""
    pinch_scale: float
    visible: bool


class Window(Frozen):
    id: int
    pid: int | None
    x: float
    y: float
    width: float
    height: float
    z_index: int | None


def viewport_point(geometry: Geometry, x: float, y: float) -> tuple[float, float] | None:
    """A point in the page's viewport (CSS pixels) as a point on the screen, or None when the mapping is not certain.

    The window's frame is not the page: the toolbar sits above it, and the side borders, where there are any, split
    the width evenly. Both insets come from the page's own report (`outerHeight - innerHeight`), so nothing here is
    a guess about a particular browser's chrome; a result that looks wrong draws nothing.
    """
    if not geometry.visible or abs(geometry.pinch_scale - 1) > 0.01 or geometry.zoom <= 0:
        return None
    width = geometry.inner_width * geometry.zoom
    height = geometry.inner_height * geometry.zoom
    side = (geometry.outer_width - width) / 2
    top = geometry.outer_height - height - max(side, 0)
    if not (-1 <= side <= _MAX_SIDE_INSET and -1 <= top <= _MAX_TOP_INSET):
        return None
    if not (0 <= x <= geometry.inner_width and 0 <= y <= geometry.inner_height):
        return None
    return geometry.screen_x + max(side, 0) + x * geometry.zoom, geometry.screen_y + max(top, 0) + y * geometry.zoom


def browser_window(windows: list[Window], geometry: Geometry) -> Window | None:
    """The browser's own window, when it is the one frontmost, else None.

    Matching by the page's reported frame also proves the driver and the browser share one coordinate scale: on a
    scaled display the sizes differ and nothing matches. The overlay is dropped when windows are read.
    """
    matches = [
        window
        for window in windows
        if geometry.browser_pid is not None
        and window.pid == geometry.browser_pid
        and abs(window.x - geometry.screen_x) <= _BOUNDS_TOLERANCE
        and abs(window.y - geometry.screen_y) <= _BOUNDS_TOLERANCE
        and abs(window.width - geometry.outer_width) <= _BOUNDS_TOLERANCE
        and abs(window.height - geometry.outer_height) <= _BOUNDS_TOLERANCE
    ]
    if len(matches) != 1:
        return None
    match = matches[0]
    front = match.z_index
    # Null means the stacking order is unknown, so it cannot be shown to be in front.
    if front is None:
        return None
    for other in windows:
        if other.id != match.id and (other.z_index is None or other.z_index > front):
            return None
    return match


class _Driver:
    """A minimal JSON-RPC line client for `cua-driver mcp`, avoiding an MCP dependency in the frozen binary."""

    def __init__(self, process: asyncio.subprocess.Process) -> None:
        self._process = process
        self._next = 0
        self._lock = asyncio.Lock()

    @property
    def running(self) -> bool:
        return self._process.returncode is None

    async def request(self, method: str, params: dict[str, Any] | None = None, *, seconds: float) -> Any:
        async with self._lock:
            self._next += 1
            ident = self._next
            message = {"jsonrpc": "2.0", "id": ident, "method": method}
            if params is not None:
                message["params"] = params
            await self._send(message)
            async with asyncio.timeout(seconds):
                # A reply to a request cancelled earlier can still arrive; only this id's reply is this call's.
                while True:
                    line = await self._read()
                    if line.get("id") == ident:
                        break
        if "error" in line:
            raise RuntimeError("driver rejected the request")
        return line["result"]

    async def notify(self, method: str) -> None:
        await self._send({"jsonrpc": "2.0", "method": method})

    async def tool(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        result = await self.request("tools/call", {"name": name, "arguments": arguments}, seconds=_CALL_SECONDS)
        if result.get("isError"):
            raise RuntimeError(f"{name} failed")
        return result.get("structuredContent") or {}

    async def _send(self, message: dict[str, Any]) -> None:
        stdin = self._process.stdin
        assert stdin is not None
        stdin.write(json.dumps(message).encode() + b"\n")
        await stdin.drain()

    async def _read(self) -> dict[str, Any]:
        stdout = self._process.stdout
        assert stdout is not None
        line = await stdout.readline()
        if not line:
            raise RuntimeError("driver exited")
        return json.loads(line)

    async def close(self) -> None:
        """Close its input first so it can clean up, then end it; each step is bounded."""
        process = self._process
        if process.returncode is None:
            with contextlib.suppress(Exception):
                assert process.stdin is not None
                process.stdin.close()
            try:
                await asyncio.wait_for(process.wait(), _STOP_SECONDS)
            except (TimeoutError, asyncio.CancelledError):
                with contextlib.suppress(ProcessLookupError):
                    process.kill()
                with contextlib.suppress(Exception):
                    await asyncio.wait_for(process.wait(), _STOP_SECONDS)


class CursorFeedback:
    """Shows where the agent is about to act. Reads and drawings never touch the page or its focus."""

    def __init__(self, driver: _Driver, session: str) -> None:
        self._driver = driver
        self._session = session
        self._disabled = False
        self._shown: bool | None = True
        self._last_call = time.monotonic()
        self._pending: asyncio.Task[None] | None = None
        self._watching: asyncio.Task[None] | None = None

    @property
    def running(self) -> bool:
        return self._driver.running

    async def settled(self) -> None:
        """Wait for the move in flight, if any."""
        if self._pending is not None:
            await asyncio.gather(self._pending, return_exceptions=True)

    def follow(self, geometry: Callable[[], Awaitable[Geometry | None]], point: tuple[float, float]) -> None:
        """Glide to a point in the viewport without waiting. Only the latest request matters, so it replaces one
        still in flight rather than queueing behind it."""
        if self._disabled:
            return
        if self._pending is not None:
            self._pending.cancel()
        if self._watching is not None:
            self._watching.cancel()
        self._pending = asyncio.create_task(self._move(geometry, point))

    async def _move(self, geometry: Callable[[], Awaitable[Geometry | None]], point: tuple[float, float]) -> None:
        try:
            async with asyncio.timeout(_CALL_SECONDS * 3):
                reading = await geometry()
                target = None if reading is None else viewport_point(reading, *point)
                if reading is None or target is None:
                    await self._show(False)
                    return
                if self._last_call + _KEEPALIVE_SECONDS < time.monotonic():
                    await self._driver.tool("start_session", {"session": self._session})
                listed = await self._driver.tool("list_windows", {"on_screen_only": True})
                window = browser_window([w for w in map(_window, listed.get("windows", ())) if w], reading)
                await self._show(window is not None)
                if window is not None:
                    await self._driver.tool(
                        "move_cursor", {"session": self._session, "x": target[0], "y": target[1], "scope": "window"}
                    )
                self._last_call = time.monotonic()
                if window is not None:
                    self._watching = asyncio.create_task(self._watch(geometry, reading))
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            # Never the message: it can carry what the driver was shown. The kind is enough to tell why.
            logger.warning("cursor feedback turned off for this run (%s)", type(exc).__name__)
            self._disabled = True
            await self._driver.close()

    async def _watch(self, geometry: Callable[[], Awaitable[Geometry | None]], initial: Geometry) -> None:
        """Hide a previous drawing when its page or foreground window changes."""
        try:
            for _ in range(80):
                await asyncio.sleep(0.25)
                async with asyncio.timeout(_CALL_SECONDS):
                    reading = await geometry()
                    if reading is None or not reading.visible or reading != initial:
                        await self._show(False)
                        return
                    listed = await self._driver.tool("list_windows", {"on_screen_only": True})
                    if browser_window([w for w in map(_window, listed.get("windows", ())) if w], reading) is None:
                        await self._show(False)
                        return
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            logger.warning("cursor feedback turned off for this run (%s)", type(exc).__name__)
            self._disabled = True
            await self._driver.close()

    async def _show(self, shown: bool) -> None:
        if shown != self._shown:
            self._shown = None
            await self._driver.tool("set_agent_cursor_enabled", {"session": self._session, "enabled": shown})
            self._shown = shown

    async def close(self) -> None:
        self._disabled = True
        try:
            tasks = [task for task in (self._pending, self._watching) if task is not None]
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
            with contextlib.suppress(Exception):
                await self._driver.tool("end_session", {"session": self._session})
        finally:
            await self._driver.close()


def _window(raw: dict[str, Any]) -> Window | None:
    # The overlay is the cursor itself and never covers the page. A window with no process id can still cover it.
    if str(raw.get("title", "")).startswith(_OVERLAY_TITLE):
        return None
    bounds = raw.get("bounds") or {}
    try:
        return Window(
            id=raw["window_id"],
            x=bounds["x"],
            y=bounds["y"],
            pid=raw.get("pid"),
            width=bounds["width"],
            height=bounds["height"],
            z_index=raw.get("z_index"),
        )
    except (KeyError, ValueError):
        return None


async def _start() -> CursorFeedback | None:
    binary = shutil.which("cua-driver")
    # X11 only: native Wayland and XWayland coordinate mapping are not verified.
    if (
        binary is None
        or not sys.platform.startswith("linux")
        or not os.environ.get("DISPLAY")
        or os.environ.get("WAYLAND_DISPLAY")
    ):
        logger.info("cursor feedback needs cua-driver and an X11 display; running without it")
        return None
    opening = asyncio.create_task(
        asyncio.create_subprocess_exec(
            binary,
            "mcp",
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
            limit=2**20,
            env=system_environment(),
        )
    )
    try:
        process = await asyncio.shield(opening)
    except asyncio.CancelledError:
        result = await asyncio.gather(opening, return_exceptions=True)
        if isinstance(result[0], asyncio.subprocess.Process):
            await _Driver(result[0]).close()
        raise
    except OSError as exc:
        logger.info("cursor feedback unavailable (%s); running without it", type(exc).__name__)
        return None
    driver = _Driver(process)
    session = f"fastbrowse-{secrets.token_hex(4)}"
    try:
        async with asyncio.timeout(_START_SECONDS):
            await driver.request(
                "initialize",
                {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "fastbrowse", "version": "0"},
                },
                seconds=_START_SECONDS,
            )
            await driver.notify("notifications/initialized")
            listed = await driver.request("tools/list", seconds=_START_SECONDS)
            tools = {tool["name"]: tool for tool in listed.get("tools", ())}
            if not tools.keys() >= _TOOLS:
                raise RuntimeError("driver lacks the cursor tools")
            scope = tools["move_cursor"].get("inputSchema", {}).get("properties", {}).get("scope", {})
            if "window" not in scope.get("enum", ()):
                raise RuntimeError("driver lacks visual-only window scope")
            await driver.tool("start_session", {"session": session})
    except asyncio.CancelledError:
        await driver.close()
        raise
    except Exception as exc:
        logger.info("cursor feedback unavailable (%s); running without it", type(exc).__name__)
        await driver.close()
        return None
    return CursorFeedback(driver, session)


@asynccontextmanager
async def cursor_feedback(enabled: bool) -> AsyncGenerator[CursorFeedback | None]:
    """The run's cursor, or None when it is off or cannot work here; the driver is stopped when the block ends."""
    feedback = await _start() if enabled else None
    try:
        yield feedback
    finally:
        if feedback is not None:
            # Shielded: a cancelled run still ends the driver instead of leaving it to the operating system.
            closing = asyncio.create_task(feedback.close())
            try:
                await asyncio.shield(closing)
            finally:
                await asyncio.gather(closing, return_exceptions=True)
