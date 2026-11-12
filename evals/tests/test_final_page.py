"""The observer retains visual evidence from the selected tab without exposing task secrets."""

import base64
import struct
import zlib
from contextlib import asynccontextmanager
from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest
from fastbrowse.evals.live_tasks import Outcome
from parallax import images

from fastbrowse_evals import final_page, live, live_tasks


def png(marked: bool) -> str:
    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    raw = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(b"\0" + (b"\0\0\0" if marked else b"\xff\xff\xff")))
        + chunk(b"IEND", b"")
    )
    return base64.b64encode(raw).decode()


@pytest.mark.parametrize("marked", [False, True])
@pytest.mark.parametrize("secrets", [{}, {"password": "hidden"}, None])
async def test_selected_session_frame_precedes_unhide_and_reaches_judge(monkeypatch, marked, secrets):
    events = []

    async def evaluate(*, params, session_id):
        assert session_id == "selected"
        expression = params["expression"]
        events.append(expression)
        value = {
            final_page.EVIDENCE: {"text": "Item Award", "inner_width": 1},
            final_page.LOCATION: "https://fixture.test/final",
            final_page.FRAME_VIEWPORT: {"inner_width": 2, "inner_height": 3, "device_pixel_ratio": 1.5},
            "document.hasFocus()": True,
        }.get(expression)
        return {"result": {"value": value}}

    async def capture(*, params, session_id):
        assert session_id == "selected"
        assert params == {"format": "png", "captureBeyondViewport": False}
        assert final_page.UNHIDE not in events
        events.append("capture")
        return {"data": png(marked)}

    client = SimpleNamespace(
        start=AsyncMock(),
        stop=AsyncMock(),
        send=SimpleNamespace(
            Target=SimpleNamespace(
                getTargets=AsyncMock(
                    return_value={
                        "targetInfos": [
                            {"type": "page", "targetId": "background", "url": "https://fixture.test/background"},
                            {"type": "page", "targetId": "selected", "url": "https://fixture.test/final"},
                        ]
                    }
                ),
                attachToTarget=AsyncMock(side_effect=lambda params: {"sessionId": params["targetId"]}),
            ),
            Runtime=SimpleNamespace(evaluate=AsyncMock(side_effect=evaluate)),
            Page=SimpleNamespace(captureScreenshot=AsyncMock(side_effect=capture)),
        ),
    )

    @asynccontextmanager
    async def connect(**kwargs):
        assert kwargs["target_match"] == "https://fixture.test/final"
        yield SimpleNamespace(observe=AsyncMock(return_value=SimpleNamespace(url=kwargs["target_match"], controls=[])))

    monkeypatch.setattr(final_page, "CDPClient", lambda _: client)
    monkeypatch.setattr(final_page, "connect_cdp", connect)
    observed = await final_page.observe_browser("ws://fixture", url="https://fixture.test/final", secrets=secrets)
    assert observed.evidence.text == "Item Award"
    assert observed.controls == ()
    if secrets != {}:
        assert observed.frame is None and observed.frame_withheld
        client.send.Page.captureScreenshot.assert_not_awaited()
        return
    assert observed.frame == "data:image/png;base64," + png(marked)
    assert observed.frame_withheld is None
    assert observed.frame_viewport.model_dump() == {"inner_width": 2, "inner_height": 3, "device_pixel_ratio": 1.5}
    assert events.index("capture") < events.index(final_page.UNHIDE)
    report = live.ArmReport(
        status="complete",
        seconds=0,
        dollars=0,
        final_page_capture=observed.frame,
        final_page_capture_viewport=observed.frame_viewport,
    )
    state = live.judge_state(live_tasks.TASKS[0], None, Outcome(None, None, observed.url), report)
    shown = live.shown_to(live.JUDGE_POLICY, state, live.Ending.DONE)
    split, attached = images.split(shown)
    assert shown["final_page_capture_viewport"] == observed.frame_viewport.model_dump()
    assert split["final_page_capture"] == next(iter(attached.values())).reference
    assert next(iter(attached.values())).data_url == observed.frame
    assert not live.no_evidence(shown)
    assert png(True) != png(False)


async def test_capture_failure_keeps_text_and_controls(monkeypatch):
    client = SimpleNamespace(
        start=AsyncMock(),
        stop=AsyncMock(),
        send=SimpleNamespace(
            Page=SimpleNamespace(captureScreenshot=AsyncMock(side_effect=RuntimeError("socket lost")))
        ),
    )
    monkeypatch.setattr(final_page, "CDPClient", lambda _: client)
    monkeypatch.setattr(final_page, "_pages", AsyncMock(return_value=[]))
    monkeypatch.setattr(final_page, "_focused", AsyncMock(return_value="selected"))
    monkeypatch.setattr(
        final_page,
        "_evaluate",
        AsyncMock(
            side_effect=[
                {"text": "retained"},
                "https://fixture.test/final",
                {"inner_width": 1, "inner_height": 1, "device_pixel_ratio": 1},
                None,
                None,
            ]
        ),
    )

    @asynccontextmanager
    async def connect(**kwargs):
        yield SimpleNamespace(
            observe=AsyncMock(
                return_value=SimpleNamespace(
                    url=kwargs["target_match"], controls=[SimpleNamespace(label="Name", value="Ada")]
                )
            )
        )

    monkeypatch.setattr(final_page, "connect_cdp", connect)
    observed = await final_page.observe_browser("ws://fixture", secrets={})
    assert observed.evidence.text == "retained" and observed.controls == (("Name", "Ada"),)
    assert observed.error is None and observed.frame is None
    assert observed.frame_withheld == "final capture failed (RuntimeError)"


async def test_ambiguous_tab_never_captures(monkeypatch):
    capture = AsyncMock()
    client = SimpleNamespace(
        start=AsyncMock(), stop=AsyncMock(), send=SimpleNamespace(Page=SimpleNamespace(captureScreenshot=capture))
    )
    monkeypatch.setattr(final_page, "CDPClient", lambda _: client)
    monkeypatch.setattr(final_page, "_pages", AsyncMock(return_value=[]))
    monkeypatch.setattr(final_page, "_focused", AsyncMock(return_value=2))
    observed = await final_page.observe_browser("ws://fixture", secrets={})
    assert observed.frame is None and observed.frame_viewport is None
    assert observed.error == "expected one final tab, found 2"
    capture.assert_not_awaited()


def test_image_attachment_bytes_do_not_split_page_text():
    from fastbrowse_evals.evidence import parts

    # Valid base64 with a PNG header suffices for the attachment transport; no browser or renderer is used.
    image = "data:image/png;base64," + base64.b64encode(base64.b64decode(png(True)) + b"\0" * 12000).decode()
    state = {"pages": [{"id": "page-1", "text": "short"}], "final_page_capture": image}
    assert parts(state, limit=4096) == [state]
    long = "whole page " * 1000
    state["pages"][0]["text"] = long
    divided = parts(state, limit=4096)
    assert len(divided) > 1
    assert all(part["final_page_capture"] == image for part in divided)
    assert "".join(page["text"] for part in divided for page in part["pages"]) == long


def test_withheld_image_is_not_evidence():
    assert live.no_evidence({"final_page_capture_withheld": "the task declares secrets"})


@pytest.mark.parametrize(
    "policy", [live.JEV_JUDGE_POLICY, live.OPUS_V4_POLICY, live.OPUS_V3_POLICY, live.OPUS_V2_POLICY]
)
def test_previous_policies_do_not_receive_new_image_evidence(policy):
    image = "data:image/png;base64," + png(True)
    report = live.ArmReport(status="complete", seconds=0, dollars=0, final_page_capture=image)
    state = live.judge_state(live_tasks.TASKS[0], None, Outcome(None, None, None), report)
    assert "final_page_capture" not in state["report"]
    shown = live.shown_to(policy, state, live.Ending.DONE)
    assert not images.split(shown)[1]


@pytest.mark.parametrize("controls_fail", [False, True])
async def test_selected_session_frame_survives_a_control_observer_failure(monkeypatch, controls_fail):
    client = SimpleNamespace(start=AsyncMock(), stop=AsyncMock())
    monkeypatch.setattr(final_page, "CDPClient", lambda _: client)
    monkeypatch.setattr(final_page, "_pages", AsyncMock(return_value=[]))
    monkeypatch.setattr(final_page, "_focused", AsyncMock(return_value="selected"))
    monkeypatch.setattr(final_page, "_shot", AsyncMock(return_value=png(True)))
    monkeypatch.setattr(
        final_page,
        "_evaluate",
        AsyncMock(
            side_effect=[
                {"text": "retained"},
                "https://fixture.test/final",
                {"inner_width": 1, "inner_height": 1, "device_pixel_ratio": 1},
                None,
                None,
            ]
        ),
    )

    @asynccontextmanager
    async def connect(**kwargs):
        yield SimpleNamespace(
            observe=AsyncMock(
                side_effect=RuntimeError("gone") if controls_fail else None,
                return_value=SimpleNamespace(url="https://fixture.test/other", controls=[]),
            )
        )

    monkeypatch.setattr(final_page, "connect_cdp", connect)
    observed = await final_page.observe_browser("ws://fixture", secrets={})
    assert observed.url == "https://fixture.test/final"
    assert observed.frame == "data:image/png;base64," + png(True)
    assert observed.frame_viewport.inner_width == 1
    assert observed.error and observed.controls is None
