from __future__ import annotations

import asyncio
import contextlib

import pytest
from cdp_use.cdp.page.events import ScreencastFrameEvent

from fastbrowse.browser import session as session_module
from fastbrowse.browser.session import BrowserSession, _Frame

pytestmark = pytest.mark.asyncio


async def test_close_cancels_a_blocked_frame_callback(browser_session: BrowserSession) -> None:
    started = asyncio.Event()

    async def blocked(_: bytes) -> None:
        started.set()
        await asyncio.Event().wait()

    browser_session._on_frame = blocked
    browser_session._pending_frame = _Frame("aGVsbG8=", 1, browser_session.active_session_id)
    browser_session._frame_scheduled = True
    browser_session._spawn(browser_session._deliver_frames())
    await asyncio.wait_for(started.wait(), timeout=1)

    await asyncio.wait_for(browser_session._close(), timeout=2)

    assert browser_session._closing
    assert not browser_session._background
    assert browser_session._pending_frame is None


async def test_close_does_not_wait_on_a_frame_callback_that_ignores_cancellation(
    browser_session: BrowserSession, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(session_module, "_FRAME_RELEASE_SECONDS", 0.1, raising=False)
    started = asyncio.Event()
    released = asyncio.Event()

    async def stubborn(_: bytes) -> None:
        started.set()
        while not released.is_set():
            # A caller's handler can swallow its cancellation and keep waiting, as a retry loop that catches
            # everything does; the run must still return and stop its browser.
            with contextlib.suppress(asyncio.CancelledError):
                await released.wait()

    browser_session._on_frame = stubborn
    frame: ScreencastFrameEvent = {
        "data": "aGVsbG8=",
        "sessionId": 1,
        "metadata": {
            "offsetTop": 0,
            "pageScaleFactor": 1,
            "deviceWidth": 1,
            "deviceHeight": 1,
            "scrollOffsetX": 0,
            "scrollOffsetY": 0,
        },
    }
    browser_session._on_screencast_frame(frame, browser_session.active_session_id)
    await asyncio.wait_for(started.wait(), timeout=1)

    closing = asyncio.ensure_future(browser_session._close())
    finished, _ = await asyncio.wait({closing}, timeout=2)
    released.set()
    await closing
    await asyncio.gather(*browser_session._background, return_exceptions=True)
    assert finished, "close waited on the frame callback"
    assert browser_session._client is None
