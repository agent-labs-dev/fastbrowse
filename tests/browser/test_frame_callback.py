from __future__ import annotations

import asyncio

import pytest

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
