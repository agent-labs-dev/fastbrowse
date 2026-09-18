"""Record a run's active tab to a video: Chrome's screencast frames, resampled to a steady rate by ffmpeg.

The screencast sends a frame only when the page repaints, so a ticker repeats the latest frame at `_FPS`;
the video's timing is then the run's real timing. The recording follows tab switches, and ends on a card
showing the run's outcome, so one file shows both what the agent did and what it answered.

A recording shows whatever the page shows. Typed passwords are masked by the page, but an email address,
an order history or an echoed key is not, so check a recording before sharing it.
"""

import asyncio
import base64
import html
import itertools
import logging
import shutil
import time
from contextlib import suppress
from pathlib import Path
from types import TracebackType
from typing import Self

from cdp_use.cdp.page.events import ScreencastFrameEvent

from fastbrowse.browser.session import BrowserSession
from fastbrowse.models import RunResult
from fastbrowse.page import BrowserError

logger = logging.getLogger(__name__)

_FPS = 25
_RECAST_SECONDS = 1.0
"""A cast with no frame this long is restarted. A start sent while the tab swaps renderer on a cross-site
navigation can go unanswered, and GitHub runs then recorded nothing; restarting a live cast only costs a frame."""
_COMMAND_SECONDS = 0.5
_RESULT_SECONDS = 4.0
"""Long enough to read a one-line answer in a shared clip."""


class RecordingError(RuntimeError):
    """ffmpeg is missing. Later failures are logged instead: a video is never worth a run's result."""


class Recording:
    """Async context manager writing the session's active tab to `path` (an .mp4) until it exits."""

    def __init__(self, session: BrowserSession, path: Path) -> None:
        self._session = session
        self._path = path
        self._frame: bytes | None = None
        self._frame_at = 0.0
        self._written = 0
        self._casting: str | None = None
        self._ffmpeg: asyncio.subprocess.Process | None = None
        self._ticker: asyncio.Task[None] | None = None
        self._acks: set[asyncio.Task[object]] = set()
        self._started = time.monotonic()

    async def __aenter__(self) -> Self:
        ffmpeg = shutil.which("ffmpeg")
        if ffmpeg is None:
            raise RecordingError("recording needs ffmpeg on PATH")
        self._path.parent.mkdir(parents=True, exist_ok=True)
        self._ffmpeg = await asyncio.create_subprocess_exec(
            ffmpeg,
            *("-loglevel", "error", "-y", "-f", "image2pipe", "-framerate", str(_FPS), "-i", "-"),
            # H.264 needs even dimensions, and yuv420p is what phones and social sites play.
            *("-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2", "-c:v", "libx264", "-pix_fmt", "yuv420p"),
            *("-movflags", "+faststart", str(self._path)),
            stdin=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        self._session.client.register.Page.screencastFrame(self._on_frame)
        self._started = time.monotonic()
        self._ticker = asyncio.create_task(self._tick())
        return self

    async def __aexit__(
        self, exc_type: type[BaseException] | None, exc: BaseException | None, tb: TracebackType | None
    ) -> None:
        if self._ticker is not None:
            self._ticker.cancel()
            await asyncio.gather(self._ticker, return_exceptions=True)
        await asyncio.gather(*self._acks, return_exceptions=True)
        if self._ffmpeg is None or self._ffmpeg.stdin is None:
            return
        if not self._written:
            self._ffmpeg.kill()
            await self._ffmpeg.wait()
            logger.warning("the browser sent no frames, so nothing was recorded to %s", self._path)
            return
        self._ffmpeg.stdin.close()
        _, stderr = await self._ffmpeg.communicate()
        if self._ffmpeg.returncode != 0:
            logger.warning("ffmpeg could not write %s: %s", self._path, stderr.decode(errors="replace").strip()[:400])

    async def show_result(self, task: str, result: RunResult) -> None:
        """End the video on the task and its outcome, in the tab being recorded."""
        answer = result.answer or result.error or ""
        seconds = time.monotonic() - self._started
        card = (
            "<meta charset=utf-8><body style='margin:0;height:100vh;display:grid;place-content:center;gap:28px;"
            "padding:0 8vw;background:#0d1117;color:#e6edf3;font:24px system-ui,sans-serif'>"
            f"<div style='color:#8b949e'>{html.escape(task)}</div>"
            f"<div style='font-size:40px;font-weight:600'>{html.escape(answer)}</div>"
            f"<div style='color:#3fb950'>{html.escape(result.status.value)} in {seconds:.1f}s, "
            f"{len(result.steps)} steps, ${result.cost.known_dollars:.4f}</div></body>"
        )
        await self._session.client.send_raw(
            "Page.navigate",
            {"url": "data:text/html;base64," + base64.b64encode(card.encode()).decode()},
            self._session.active_session_id,
        )
        await asyncio.sleep(_RESULT_SECONDS)

    def _on_frame(self, event: ScreencastFrameEvent, session_id: str | None) -> None:
        self._frame = base64.b64decode(event["data"])
        self._frame_at = time.monotonic()
        # Chrome sends no further frames until each one is acknowledged.
        ack = asyncio.ensure_future(
            self._session.client.send_raw("Page.screencastFrameAck", {"sessionId": event["sessionId"]}, session_id)
        )
        self._acks.add(ack)
        ack.add_done_callback(self._acks.discard)

    async def _tick(self) -> None:
        if self._ffmpeg is None or self._ffmpeg.stdin is None:
            return
        started = time.monotonic()
        for frame in itertools.count():
            active = self._session.active_session_id
            if active != self._casting or time.monotonic() - self._frame_at > _RECAST_SECONDS:
                await self._cast(active)
            if self._frame is not None:
                self._ffmpeg.stdin.write(self._frame)
                await self._ffmpeg.stdin.drain()
                self._written += 1
            await asyncio.sleep(max(0.0, started + (frame + 1) / _FPS - time.monotonic()))

    async def _cast(self, session_id: str) -> None:
        # The previous tab may already be closed, taking its screencast with it; an unanswered start is retried.
        if self._casting is not None:
            await self._command("Page.stopScreencast", None, self._casting)
        await self._command("Page.startScreencast", {"format": "jpeg", "quality": 85}, session_id)
        self._casting = session_id
        self._frame_at = time.monotonic()

    async def _command(self, method: str, params: dict[str, object] | None, session_id: str) -> None:
        with suppress(BrowserError, TimeoutError):
            await asyncio.wait_for(self._session.client.send_raw(method, params, session_id), _COMMAND_SECONDS)
