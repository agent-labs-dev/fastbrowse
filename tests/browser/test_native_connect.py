"""`connect_cdp` on the Rust core: a page with the Python page's methods, reading and acting as it does."""

import asyncio
import importlib
import json
import time
from pathlib import Path
from urllib.parse import urlsplit
from urllib.request import urlopen

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


def pages(cdp_url: str) -> set[str]:
    """The ids of the pages Chrome has open, read from its HTTP endpoint without a session of our own."""
    address = urlsplit(cdp_url)
    with urlopen(f"http://{address.netloc}/json/list", timeout=5) as response:
        return {target["id"] for target in json.load(response) if target["type"] == "page"}


def hold_until_open(cdp_url: str, before: set[str]) -> None:
    """Block the event loop until `connect_cdp` has opened a tab and returned, so its answer waits on the loop."""
    deadline = time.monotonic() + 10
    while not pages(cdp_url) - before and time.monotonic() < deadline:
        time.sleep(0.05)
    assert pages(cdp_url) - before, "connect_cdp opened no tab"
    time.sleep(1)  # The tab is set up within milliseconds of appearing; this lets the open return.


async def cancelled_once_open(cdp_url: str, *, after_settling: bool) -> set[str]:
    """Cancel `connect_cdp` once its tab is open, and answer the pages it left behind.

    The loop is held while the core finishes opening, so its answer is queued behind the hold: the cancel lands either
    before the answer settles the core's future, or after it settled but before the waiting task resumed.
    """
    before = pages(cdp_url)

    async def enter() -> None:
        async with connect_cdp(cdp_url=cdp_url, attach=False):
            pytest.fail("a cancelled open yielded its page")

    task = asyncio.create_task(enter())
    await asyncio.sleep(0)
    hold_until_open(cdp_url, before)
    if after_settling:
        asyncio.get_running_loop().call_soon(task.cancel)
    else:
        task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    for _ in range(100):
        if not pages(cdp_url) - before:
            break
        await asyncio.sleep(0.05)
    return pages(cdp_url) - before


@pytest.mark.timeout(60)
@pytest.mark.parametrize("after_settling", [False, True], ids=["before-the-page-arrives", "as-the-page-arrives"])
async def test_a_cancelled_open_closes_the_tab_it_opened(
    chrome_connection: BrowserConnection, monkeypatch: pytest.MonkeyPatch, after_settling: bool
) -> None:
    monkeypatch.setenv("FASTBROWSE_PYTHON_CORE", "0")
    assert await cancelled_once_open(chrome_connection.cdp_url, after_settling=after_settling) == set()
