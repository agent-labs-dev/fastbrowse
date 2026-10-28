"""Live view delivery must not hold up the browser's control connection."""

import asyncio
import base64
import logging
import shutil
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock

import pytest
from cdp_use.client import CDPClient

from fastbrowse.browser import BrowserSession
from fastbrowse.browser.recording import Recording
from tests.browser.conftest import RecordingArtifactSink
from tests.browser.test_lifecycle import CONNECTION, CdpTransport


async def emit_frame(session: BrowserSession, number: int, data: bytes, tab_session: str = "session") -> None:
    await session.client.emit_event(
        "Page.screencastFrame",
        {"sessionId": number, "data": base64.b64encode(data).decode(), "metadata": {}},
        tab_session,
    )


async def test_live_frames_follow_tabs_ack_after_delivery_and_keep_only_the_latest(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    transport = CdpTransport(monkeypatch)
    frames: list[bytes] = []
    delivering = asyncio.Event()
    release = asyncio.Event()
    release.set()

    async def receive(frame: bytes) -> None:
        frames.append(frame)
        delivering.set()
        await release.wait()

    async with BrowserSession(CONNECTION, RecordingArtifactSink(), on_frame=receive) as session:
        await asyncio.gather(*session._background)
        assert (
            "Page.startScreencast",
            {"format": "jpeg", "quality": 60, "maxWidth": 1280, "maxHeight": 800},
            "session",
        ) in transport.requests

        await emit_frame(session, 1, b"first")
        await asyncio.gather(*session._background)
        await emit_frame(session, 2, b"superseded")
        await emit_frame(session, 3, b"latest")
        await asyncio.gather(*session._background)
        assert frames == [b"first", b"latest"]

        release.clear()
        delivering.clear()
        await emit_frame(session, 4, b"slow")
        await delivering.wait()
        await emit_frame(session, 5, b"arrived while busy")
        await session.client.emit_event(
            "Page.javascriptDialogOpening", {"type": "alert", "message": "ready"}, "session"
        )
        assert session.pending_dialog() is not None
        await session.handle_dialog(True)
        await asyncio.sleep(0)
        # Back-pressure: Chrome is not told to send more until the frame in hand has been delivered.
        assert ("Page.screencastFrameAck", {"sessionId": 4}, "session") not in transport.requests
        release.set()
        await asyncio.gather(*session._background)
        assert frames == [b"first", b"latest", b"slow", b"arrived while busy"]

        transport.results["Target.createTarget"] = [{"targetId": "second"}]
        transport.results["Target.attachToTarget"] = [{"sessionId": "second-session"}]
        await session._open_tab("about:blank", owned=True)
        await asyncio.gather(*session._background)
        await session.switch_tab("owned")
        await asyncio.gather(*session._background)
        await session.switch_tab("second")
        await asyncio.gather(*session._background)
        await emit_frame(session, 6, b"old tab")
        await emit_frame(session, 7, b"new tab", "second-session")
        await asyncio.gather(*session._background)
        assert frames[-1] == b"new tab"
        assert b"old tab" not in frames

        await session.client.emit_event("Target.targetDestroyed", {"targetId": "second"})
        await asyncio.gather(*session._background)
        assert session.active_target_id == "owned"

        # Recording owns the single CDP event slot while it runs, without replacing the live consumer.
        recording = Recording(session, tmp_path / "clip.mp4")
        session.client.register.Page.screencastFrame(recording._on_frame)
        await emit_frame(session, 8, b"recorded too")
        await asyncio.gather(*session._background)
        assert frames[-1] == recording._frame == b"recorded too"
        # A frame from another session, as the cast that keeps a background tab painting sends, is acked unseen.
        await emit_frame(session, 80, b"no picture of the tab", "keeper")
        await asyncio.gather(*session._background)
        assert frames[-1] == recording._frame == b"recorded too"
        transport.requests.remove(("Page.screencastFrameAck", {"sessionId": 80}, "keeper"))

        release.clear()
        delivering.clear()
        await emit_frame(session, 9, b"cancel on close")
        await delivering.wait()

    assert not session._background
    assert not session._frame_scheduled
    assert [request for request in transport.requests if request[0] == "Page.screencastFrameAck"] == [
        ("Page.screencastFrameAck", {"sessionId": number}, "second-session" if number == 7 else "session")
        # 9 was cancelled mid-delivery by close, which stops the cast; an ack would be for a cast already gone.
        for number in range(1, 9)
    ]
    assert [(method, sid) for method, _, sid in transport.requests if method.endswith("Screencast")] == [
        ("Page.startScreencast", "session"),
        ("Page.stopScreencast", "session"),
        ("Page.startScreencast", "second-session"),
        ("Page.stopScreencast", "second-session"),
        ("Page.startScreencast", "session"),
        ("Page.stopScreencast", "session"),
        ("Page.startScreencast", "second-session"),
        ("Page.stopScreencast", "second-session"),
        ("Page.startScreencast", "session"),
        ("Page.stopScreencast", "session"),
    ]
    assert transport.calls.index("Page.stopScreencast") < transport.calls.index("stop")

    transport.requests.clear()
    async with BrowserSession(CONNECTION, RecordingArtifactSink()) as session:
        assert not await session.client.emit_event("Page.screencastFrame", {})
    assert not any("Screencast" in method or "screencast" in method for method, _, _ in transport.requests)


@pytest.mark.parametrize(
    "failure", ["Page.startScreencast", "Page.stopScreencast", "Page.screencastFrameAck", "handler"]
)
async def test_live_view_failures_are_warnings_and_leave_the_session_usable(
    monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture, failure: str
) -> None:
    transport = CdpTransport(monkeypatch)
    calls = 0

    async def receive(frame: bytes) -> None:
        nonlocal calls
        calls += 1
        if failure == "handler":
            raise ValueError("private page text")

    if failure != "handler":
        transport.failures[failure] = RuntimeError("private endpoint")
    with caplog.at_level(logging.WARNING):
        async with BrowserSession(CONNECTION, RecordingArtifactSink(), on_frame=receive) as session:
            await asyncio.gather(*session._background)
            await emit_frame(session, 1, b"jpeg")
            await asyncio.gather(*session._background)
            await emit_frame(session, 2, b"next jpeg")
            await asyncio.gather(*session._background)
            await session.switch_tab("owned")
        assert calls == 2
        assert "Page.screencastFrameAck" in transport.calls
        assert "Target.activateTarget" in transport.calls
        assert caplog.records
        assert all(record.levelno == logging.WARNING for record in caplog.records)
        assert "private" not in caplog.text


async def test_a_stalled_screencast_does_not_delay_switching_tabs(monkeypatch: pytest.MonkeyPatch) -> None:
    transport = CdpTransport(monkeypatch)
    started = transport.blocked["Page.startScreencast"] = asyncio.Event()

    async def receive(frame: bytes) -> None:
        pass

    async with BrowserSession(CONNECTION, RecordingArtifactSink(), on_frame=receive) as session:
        await started.wait()
        await session.switch_tab("owned")
        assert "Page.startScreencast" not in transport.finished
    assert "Page.startScreencast" in transport.finished


@pytest.mark.parametrize(
    "page", ["clear", "showing a secret", "revealing a secret during the capture", "after access is revoked"]
)
async def test_a_recording_whose_cast_sent_no_frame_ends_on_a_screenshot_unless_frames_are_withheld(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, caplog: pytest.LogCaptureFixture, page: str
) -> None:
    """Chrome can acknowledge `Page.startScreencast` and then send nothing, and a short run ends before the restart."""
    if shutil.which("ffmpeg") is None:
        pytest.skip("recording needs ffmpeg")
    still = tmp_path / "still.jpg"
    ffmpeg = await asyncio.create_subprocess_exec(
        *("ffmpeg", "-loglevel", "error", "-f", "lavfi", "-i", "color=c=teal:s=64x48", "-frames:v", "1", str(still))
    )
    assert await ffmpeg.wait() == 0
    transport = CdpTransport(monkeypatch)
    transport.results["Page.captureScreenshot"] = [{"data": base64.b64encode(still.read_bytes()).decode()}]
    path = tmp_path / "clip.mp4"
    revoked = False

    async def check_access() -> None:
        if revoked:
            raise PermissionError("revoked")

    async with BrowserSession(CONNECTION, RecordingArtifactSink(), check_access=check_access) as session:
        session.frames_withheld = page == "showing a secret"

        async def send(method: str, params: Any = None, session_id: str | None = None) -> dict[str, Any]:
            reply = await transport.send(method, params, session_id)
            if method == "Page.captureScreenshot" and page == "revealing a secret during the capture":
                session.frames_withheld = True
            return reply

        monkeypatch.setattr(CDPClient, "send_raw", AsyncMock(side_effect=send))
        with caplog.at_level(logging.WARNING):
            async with Recording(session, path) as recording:
                await asyncio.sleep(0.2)
                revoked = page == "after access is revoked"
    assert ("Page.startScreencast", {"format": "jpeg", "quality": 95}, "session") in transport.requests
    if page == "clear":
        assert recording.outputs == (path, path.with_suffix(".plain.mp4"))
        assert all(output.stat().st_size > 0 for output in recording.outputs)
        assert "no frames" not in caplog.text
        return
    # A recording keeps its last clean frame, and with none it records nothing rather than the secret.
    assert ("Page.captureScreenshot" in transport.calls) is (page == "revealing a secret during the capture")
    assert recording.outputs == ()
    assert not path.exists()
    assert "the browser sent no frames" in caplog.text
