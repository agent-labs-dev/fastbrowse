"""Closing the last owned tab must produce a classified browser failure."""

import asyncio

import pytest

from fastbrowse.browser import BrowserSession
from fastbrowse.models import Unavailable
from tests.browser.conftest import RecordingArtifactSink
from tests.browser.test_lifecycle import CONNECTION, CdpTransport


async def test_capture_session_after_last_target_closes_is_unavailable(monkeypatch: pytest.MonkeyPatch) -> None:
    CdpTransport(monkeypatch)
    async with BrowserSession(CONNECTION, artifact_sink=RecordingArtifactSink()) as session:
        session._on_target_destroyed({"targetId": session.active_target_id}, None)
        with pytest.raises(Unavailable, match="active browser tab closed"):
            _ = session.active_session_id


async def test_a_closed_active_tab_hands_over_to_its_opener_else_the_last_active_tab(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """An OAuth popup closes itself once it has signed the run in: the run goes back to the tab that opened it, not
    to the first tab, and a tab with no opener left hands over to the one the run was on before it."""
    transport = CdpTransport(monkeypatch)
    async with BrowserSession(CONNECTION, artifact_sink=RecordingArtifactSink()) as session:
        for target, session_id in (("second", "second-session"), ("third", "third-session")):
            transport.results["Target.createTarget"] = [{"targetId": target}]
            transport.results["Target.attachToTarget"] = [{"sessionId": session_id}]
            await session._open_owned_tab("about:blank")
        await session.switch_tab("second")
        await session.switch_tab("third")
        transport.results["Target.attachToTarget"] = [{"sessionId": "popup-session"}]
        transport.results["Target.getTargetInfo"] = [{"targetInfo": {"url": "https://example.com/", "title": "Popup"}}]
        session._popups["popup"] = ("second", asyncio.get_running_loop().create_future())
        await session._adopt_popup("popup", "second")
        await session.switch_tab("popup")
        for closed, after in (("popup", "second"), ("second", "third"), ("third", "owned")):
            session._on_target_destroyed({"targetId": closed}, None)
            assert session.active_target_id == after, f"after {closed} closed"
