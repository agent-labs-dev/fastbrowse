"""Record a run's active tab to a video: Chrome's screencast frames, resampled to a steady rate by ffmpeg.

The screencast sends a frame only when the page repaints, so a ticker repeats the latest frame at `_FPS`;
the video's timing is then the run's real timing. The recording follows tab switches, captions each step from
when its action began, and ends on a card showing the run's outcome, so one file shows both what the agent did
and what it answered. A second ffmpeg pass over a lossless first one writes it twice: with the step captions
burned in, and plain beside it as `<name>.plain.mp4`.

A recording shows whatever the page shows. Typed passwords are masked by the page, but an email address,
an order history or an echoed key is not, so check a recording before sharing it.
"""

import asyncio
import base64
import html
import itertools
import logging
import re
import shutil
import tempfile
import time
from contextlib import suppress
from pathlib import Path
from types import TracebackType
from typing import Self, assert_never
from urllib.parse import urlsplit

from cdp_use.cdp.page.events import ScreencastFrameEvent

from fastbrowse.browser.session import BrowserSession
from fastbrowse.models import Decider, Operation, RunResult, StepResult
from fastbrowse.page import BrowserError

logger = logging.getLogger(__name__)

_FPS = 25
_RECAST_SECONDS = 1.0
"""A cast with no frame this long is restarted. A start sent while the tab swaps renderer on a cross-site
navigation can go unanswered, and GitHub runs then recorded nothing; restarting a live cast only costs a frame."""
_COMMAND_SECONDS = 0.5
_RESULT_SECONDS = 4.0
"""Long enough to read a one-line answer in a shared clip."""
_CITATION_LINK = re.compile(r"\s*\[(\d+)\]\(<[^>]*>\)")
"""A numbered answer link, `[3](<url>)`: the card shows the number and lists the quote, not the deep link."""


class RecordingError(RuntimeError):
    """ffmpeg is missing. Later failures are logged instead: a video is never worth a run's result."""


class Recording:
    """Async context manager writing the session's active tab to `path` (an .mp4) until it exits."""

    def __init__(self, session: BrowserSession, path: Path) -> None:
        self._session = session
        self._path = path
        self._frame: bytes | None = None
        self._frame_at = 0.0
        self._casting: str | None = None
        self._ffmpeg: asyncio.subprocess.Process | None = None
        self._ticker: asyncio.Task[None] | None = None
        self._started = time.monotonic()
        # Monotonic times. The video starts at its first frame, which the browser sends some time after entry.
        self._first_frame_at: float | None = None
        self._captions: list[tuple[float, str]] = []
        self._card_at: float | None = None
        self._scratch = tempfile.TemporaryDirectory(prefix="fastbrowse-recording-")
        self._uncaptioned = Path(self._scratch.name, "uncaptioned.mkv")
        self.outputs: tuple[Path, ...] = ()
        """The finished videos, set only once both are encoded: a failed run must not report a file it did not write."""

    async def __aenter__(self) -> Self:
        ffmpeg = shutil.which("ffmpeg")
        if ffmpeg is None:
            raise RecordingError("recording needs ffmpeg on PATH")
        self._ffmpeg_path = ffmpeg
        self._path.parent.mkdir(parents=True, exist_ok=True)
        self._ffmpeg = await asyncio.create_subprocess_exec(
            ffmpeg,
            *("-loglevel", "error", "-y", "-f", "image2pipe", "-framerate", str(_FPS), "-i", "-"),
            # Lossless, so the captioning pass is the only lossy encode; 4:4:4 also takes the odd sizes a tab has.
            *("-c:v", "libx264", "-qp", "0", "-preset", "ultrafast", "-pix_fmt", "yuv444p", str(self._uncaptioned)),
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
        if self._session._on_frame is not None:
            self._session.client.register.Page.screencastFrame(self._session._on_screencast_frame)
        try:
            await self._close()
        finally:
            self._scratch.cleanup()

    @property
    def _plain_path(self) -> Path:
        return self._path.with_suffix(".plain" + self._path.suffix)

    async def _close(self) -> None:
        if self._ffmpeg is None or self._ffmpeg.stdin is None:
            return
        if self._first_frame_at is None:
            self._ffmpeg.kill()
            await self._ffmpeg.wait()
            logger.warning("the browser sent no frames, so nothing was recorded to %s", self._path)
            return
        self._ffmpeg.stdin.close()
        _, stderr = await self._ffmpeg.communicate()
        if self._ffmpeg.returncode != 0:
            logger.warning("ffmpeg could not write %s: %s", self._path, stderr.decode(errors="replace").strip()[:400])
            return
        await self._finish(self._first_frame_at)

    def caption(self, step: StepResult) -> None:
        """Caption `step` from when its action began; it arrives once the step is over."""
        self._captions.append((time.monotonic() - step.duration_ms / 1000, f"{step.index + 1} · {_describe(step)}"))

    async def _finish(self, origin: float) -> None:
        """Encode the final video twice from the lossless pass: with step captions, and plain."""
        subtitles = Path(self._scratch.name, "steps.srt")
        # A run that opens straight onto its answer takes no steps, and has no captions to end.
        timed = [*self._captions, (self._card_at or time.monotonic(), "")]
        cues = [
            # libass reads `{...}` as a style override, so a page's braces are escaped to stay text.
            f"{_srt_time(max(0.0, at - origin))} --> {_srt_time(until - origin)}\n{text.replace('{', '\\{')}\n"
            for (at, text), (until, _) in itertools.pairwise(timed)
            # A step can end before the browser sends its first frame; its caption starts with the video.
            if until > max(at, origin)
        ]
        await asyncio.to_thread(
            subtitles.write_text, "\n".join(f"{n}\n{cue}" for n, cue in enumerate(cues, 1)), encoding="utf-8"
        )
        # BorderStyle 3 draws a box behind the text, coloured by OutlineColour (alpha first, 00 opaque).
        style = "Fontsize=10,BorderStyle=3,Outline=6,Shadow=0,OutlineColour=&H50000000,MarginV=16,Alignment=2"
        # libass cannot open an empty subtitle file. The filter names it relative to ffmpeg's working directory:
        # an absolute path's drive colon and backslashes are filter syntax, and a Windows run failed on them.
        burn = f"subtitles={subtitles.name}:force_style='{style}'" if cues else "null"
        error = await self._encode(burn)
        if error and cues:
            # An ffmpeg built without libass has no subtitles filter; the plain video is still worth keeping.
            logger.warning("ffmpeg could not caption %s, so it is written without captions: %s", self._path, error)
            error = await self._encode("null")
        if error:
            logger.warning("ffmpeg could not write %s: %s", self._path, error)
            return
        # Encoded in scratch, then copied beside each path and renamed over it: a failed encode or copy leaves
        # whatever was already there untouched, and scratch cleanup takes the partial files with it.
        written = []
        for name, path in (("captioned.mp4", self._path), ("plain.mp4", self._plain_path)):
            try:
                await asyncio.to_thread(_replace, Path(self._scratch.name, name), path)
            except OSError as error:
                logger.warning("could not write %s: %s", path, error)
                continue
            written.append(path)
        self.outputs = tuple(written)

    async def _encode(self, burn: str) -> str | None:
        """Write both videos into scratch through `burn` on the captioned one; return ffmpeg's error, if any."""
        # H.264 needs even dimensions, and yuv420p is what phones and social sites play.
        graph = f"[0:v]scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p,split=2[plain][steps];[steps]{burn}[captioned]"
        # Page text is the subject of a shared clip; x264's default quality blurs small type.
        encode = ("-c:v", "libx264", "-crf", "18", "-preset", "slow", "-movflags", "+faststart")
        process = await asyncio.create_subprocess_exec(
            self._ffmpeg_path,
            *("-loglevel", "error", "-y", "-i", str(self._uncaptioned), "-filter_complex", graph),
            *("-map", "[captioned]", *encode, "captioned.mp4", "-map", "[plain]", *encode, "plain.mp4"),
            stderr=asyncio.subprocess.PIPE,
            cwd=self._scratch.name,
        )
        try:
            _, stderr = await process.communicate()
        except asyncio.CancelledError:
            process.kill()
            await process.wait()
            raise
        if process.returncode == 0:
            return None
        return stderr.decode(errors="replace").strip()[:400] or f"exit status {process.returncode}"

    async def show_result(self, task: str, result: RunResult) -> None:
        """End the video on the task and its outcome, in the tab being recorded."""
        parts = _CITATION_LINK.split(result.answer or result.error or "")
        cited = {int(number) for number in parts[1::2]}
        answer = "".join(
            f"<sup style='margin-left:4px;font-size:18px;color:#58a6ff'>{part}</sup>" if i % 2 else html.escape(part)
            for i, part in enumerate(parts)
        )
        sources = "".join(
            "<div style='white-space:nowrap;overflow:hidden;text-overflow:ellipsis'>"
            f"<span style='color:#58a6ff'>{citation.id}</span> {html.escape(urlsplit(citation.url).netloc)}"
            f" <span style='color:#e6edf3'>\u201c{html.escape(citation.quote)}\u201d</span></div>"
            for citation in result.citations
            if citation.id in cited
        )
        self._card_at = time.monotonic()
        card = (
            "<meta charset=utf-8><body style='margin:0;height:100vh;display:grid;place-content:center;gap:28px;"
            "grid-template-columns:minmax(0,1fr);padding:0 8vw;background:#0d1117;color:#e6edf3;"
            "font:24px system-ui,sans-serif'>"
            f"<div style='color:#8b949e'>{html.escape(task)}</div>"
            f"<div style='font-size:40px;font-weight:600'>{answer}</div>"
            + (f"<div style='display:grid;gap:6px;color:#8b949e;font-size:20px'>{sources}</div>" if sources else "")
            + f"<div style='color:#3fb950'>{html.escape(result.status.value)} in {self._card_at - self._started:.1f}s, "
            f"{len(result.steps)} steps, ${result.cost.known_dollars:.4f}</div></body>"
        )
        await self._session.client.send_raw(
            "Page.navigate",
            {"url": "data:text/html;base64," + base64.b64encode(card.encode()).decode()},
            self._session.active_session_id,
        )
        await asyncio.sleep(_RESULT_SECONDS)

    def _on_frame(self, event: ScreencastFrameEvent, session_id: str | None) -> None:
        # CDP keeps one subscriber per event, so recording must also forward frames to the live view.
        self._session._on_screencast_frame(event, session_id)
        self._frame_at = time.monotonic()
        # The video holds its last clean frame rather than showing a secret.
        if not self._session.frames_withheld:
            self._frame = base64.b64decode(event["data"])

    async def _tick(self) -> None:
        if self._ffmpeg is None or self._ffmpeg.stdin is None:
            return
        written = 0
        while True:
            active = self._session.active_session_id
            if self._session._on_frame is None and (
                active != self._casting or time.monotonic() - self._frame_at > _RECAST_SECONDS
            ):
                await self._cast(active)
            if self._frame is not None:
                self._ffmpeg.stdin.write(self._frame)
                await self._ffmpeg.stdin.drain()
                if self._first_frame_at is None:
                    self._first_frame_at = time.monotonic()
                written += 1
            # The schedule starts at the first frame, like the captions: one started earlier wrote a burst of
            # catch-up frames after a slow first cast, and the video ran ahead of its captions.
            due = time.monotonic() + 1 / _FPS if self._first_frame_at is None else self._first_frame_at + written / _FPS
            await asyncio.sleep(max(0.0, due - time.monotonic()))

    async def _cast(self, session_id: str) -> None:
        # The previous tab may already be closed, taking its screencast with it; an unanswered start is retried.
        if self._casting is not None:
            await self._command("Page.stopScreencast", None, self._casting)
        await self._command("Page.startScreencast", {"format": "jpeg", "quality": 95}, session_id)
        self._casting = session_id
        self._frame_at = time.monotonic()

    async def _command(self, method: str, params: dict[str, object] | None, session_id: str) -> None:
        with suppress(BrowserError, TimeoutError):
            await asyncio.wait_for(self._session.client.send_raw(method, params, session_id), _COMMAND_SECONDS)


def _describe(step: StepResult) -> str:
    target = f" {step.target}" if step.target else ""
    match step.decided_by:
        case Decider.JEV:
            by = "picked by Jev"
        case Decider.LLM:
            by = "picked by the LLM"
        case _:
            assert_never(step.decided_by)
    match step.operation:
        case Operation.READ:
            return f"read the page · {by}"
        case Operation.ESCALATE:
            return "ask the planner"
        case Operation.DONE:
            return "check the answer"
        case (
            Operation.CLICK
            | Operation.HOVER
            | Operation.FILL
            | Operation.SELECT
            | Operation.ENTER
            | Operation.ESCAPE
            | Operation.SCROLL
            | Operation.BACK
            | Operation.SWITCH_TAB
            | Operation.UPLOAD
            | Operation.DIALOG
        ):
            return f"{step.operation.value.replace('_', ' ')}{target} · {by}"
        case _:
            assert_never(step.operation)


def _srt_time(seconds: float) -> str:
    millis = round(seconds * 1000)
    return f"{millis // 3_600_000:02}:{millis // 60_000 % 60:02}:{millis // 1000 % 60:02},{millis % 1000:03}"


def _replace(source: Path, path: Path) -> None:
    with tempfile.NamedTemporaryFile(dir=path.parent, prefix=f".{path.name}.", delete=False) as partial:
        staged = Path(partial.name)
    try:
        shutil.copy(source, staged)  # with the encoded file's mode, not the private one of a temporary file
        staged.replace(path)
    finally:
        staged.unlink(missing_ok=True)
