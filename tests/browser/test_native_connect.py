"""`connect_cdp` on the Rust core: a page with the Python page's methods, reading and acting as it does."""

import asyncio
import importlib
from pathlib import Path

import pytest

from fastbrowse.browser.session import OriginNotAllowed
from fastbrowse.models import BrowserConnection, Operation, StepOutcome
from fastbrowse.page import Action, BrowserError
from fastbrowse.run import connect_cdp
from tests.browser.conftest import RecordingArtifactSink

try:
    NATIVE = hasattr(importlib.import_module("fastbrowse._native"), "connect_cdp")
except ImportError:
    NATIVE = False

pytestmark = pytest.mark.skipif(not NATIVE, reason="fastbrowse._native with connect_cdp is not installed")


async def read(cdp_url: str, url: str) -> dict[str, object]:
    """What a caller reads and does through `connect_cdp` on one page, in a tab of its own."""
    async with connect_cdp(cdp_url=cdp_url, attach=False) as page:
        await page.navigate(url)
        observation = await page.observe()
        capture = await page.capture()
        username = next(control for control in observation.controls if control.label == "Username")
        typed = await page.act(Action(operation=Operation.FILL, target_id=username.id, text="ada"), observation)
        return {
            "address": await page.address(),
            "origin": await page.origin(),
            "status": await page.response_status(),
            "title": observation.title,
            "labels": sorted(control.label for control in observation.controls),
            "text": capture.text,
            "blocks": [(block.kind, block.start, block.end) for block in capture.blocks],
            "typed": typed.outcome,
            "changed": await page.document_changed(observation),
            "png": (await page.screenshot())[:8],
            "artifacts": page.artifacts,
        }


@pytest.mark.timeout(120)
async def test_both_cores_read_and_act_alike(
    chrome_connection: BrowserConnection, main_site: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    url = f"{main_site}/index.html"
    monkeypatch.setenv("FASTBROWSE_PYTHON_CORE", "1")
    python = await read(chrome_connection.cdp_url, url)
    monkeypatch.setenv("FASTBROWSE_PYTHON_CORE", "0")
    rust = await read(chrome_connection.cdp_url, url)
    assert python["typed"] is StepOutcome.EXECUTED
    assert python["png"] == b"\x89PNG\r\n\x1a\n"
    assert rust == python


async def test_a_page_outside_the_allowed_origins_raises_origin_not_allowed(
    chrome_connection: BrowserConnection, main_site: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("FASTBROWSE_PYTHON_CORE", "0")
    async with connect_cdp(
        cdp_url=chrome_connection.cdp_url, attach=False, allowed_origins=["https://elsewhere.example"]
    ) as page:
        with pytest.raises(OriginNotAllowed):
            await page.navigate(f"{main_site}/index.html")


async def test_check_access_refusing_raises_browser_error_naming_its_type(
    chrome_connection: BrowserConnection, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setenv("FASTBROWSE_PYTHON_CORE", "0")

    async def refused() -> None:
        raise PermissionError("revoked for jane@example.com")

    with pytest.raises(BrowserError, match=r"\(PermissionError, caller supplied\)") as raised:
        async with connect_cdp(cdp_url=chrome_connection.cdp_url, check_access=refused, downloads=tmp_path):
            pass
    assert "jane" not in str(raised.value)


@pytest.mark.timeout(60)
async def test_a_download_goes_to_the_callers_artifact_sink(
    chrome_connection: BrowserConnection, main_site: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("FASTBROWSE_PYTHON_CORE", "0")
    sink = RecordingArtifactSink()
    async with connect_cdp(cdp_url=chrome_connection.cdp_url, attach=False, artifact_sink=sink) as page:
        await page.navigate(f"{main_site}/index.html")
        observation = await page.observe()
        link = next(control for control in observation.controls if control.label == "Download attachment")
        await page.act(Action(operation=Operation.CLICK, target_id=link.id), observation)
        for _ in range(100):
            if page.artifacts:
                break
            await asyncio.sleep(0.05)
        assert [artifact.uri for artifact in page.artifacts] == [artifact.uri for artifact in sink.artifacts]
    assert len(sink.artifacts) == 1
