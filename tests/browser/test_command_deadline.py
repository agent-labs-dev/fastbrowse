"""A live browser can leave one page command unanswered indefinitely."""

import asyncio

import pytest

from fastbrowse.browser import BrowserSession
from fastbrowse.browser import session as browser_session
from tests.browser.conftest import RecordingArtifactSink
from tests.browser.test_lifecycle import CONNECTION, CdpTransport


async def test_unanswered_command_from_live_browser_has_a_deadline(monkeypatch: pytest.MonkeyPatch) -> None:
    transport = CdpTransport(monkeypatch)
    monkeypatch.setattr(browser_session, "CDP_REPLY_SECONDS", 0.01)
    monkeypatch.setattr(browser_session, "CDP_COMMAND_SECONDS", 0.05, raising=False)
    async with BrowserSession(CONNECTION, RecordingArtifactSink()) as session:
        transport.blocked["Runtime.evaluate"] = asyncio.Event()
        async with asyncio.timeout(1):
            with pytest.raises(browser_session.BrowserUnresponsive, match="command deadline"):
                await session.client.send_raw("Runtime.evaluate", {"expression": "aPromiseThatNeverSettles()"})
    assert "Browser.getVersion" in transport.calls
    assert "Runtime.evaluate" in transport.finished
