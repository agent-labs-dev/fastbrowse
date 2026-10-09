"""Closing the last owned tab must produce a classified browser failure."""

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
