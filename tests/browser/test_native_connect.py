"""`connect_cdp` on the Rust core: a page a caller reads and acts on, in this process."""

import asyncio
import json
import os
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, cast
from urllib.parse import urlsplit
from urllib.request import urlopen

import pytest

from fastbrowse.models import Operation, StepOutcome
from fastbrowse.page import Action, BrowserError, OriginNotAllowed
from fastbrowse.run import connect_cdp
from tests.browser.conftest import RecordingArtifactSink


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
async def test_a_page_reads_and_acts(cdp_url: str, main_site: str) -> None:
    url = f"{main_site}/index.html"
    read_back = await read(cdp_url, url)
    assert read_back["typed"] is StepOutcome.EXECUTED
    assert read_back["png"] == b"\x89PNG\r\n\x1a\n"
    assert (read_back["address"], read_back["origin"], read_back["status"]) == (url, main_site, 200)
    assert "Username" in cast(list[str], read_back["labels"])
    assert read_back["text"]
    assert read_back["changed"] is False
    assert read_back["artifacts"] == ()


async def test_a_page_outside_the_allowed_origins_raises_origin_not_allowed(cdp_url: str, main_site: str) -> None:
    async with connect_cdp(cdp_url=cdp_url, attach=False, allowed_origins=["https://elsewhere.example"]) as page:
        with pytest.raises(OriginNotAllowed):
            await page.navigate(f"{main_site}/index.html")


async def test_check_access_refusing_raises_browser_error_naming_its_type(cdp_url: str, tmp_path: Path) -> None:

    async def refused() -> None:
        raise PermissionError("revoked for jane@example.com")

    with pytest.raises(BrowserError, match=r"\(PermissionError, caller supplied\)") as raised:
        async with connect_cdp(cdp_url=cdp_url, check_access=refused, downloads=tmp_path):
            pass
    assert "jane" not in str(raised.value)


@pytest.mark.timeout(60)
async def test_a_download_goes_to_the_callers_artifact_sink(cdp_url: str, main_site: str) -> None:
    sink = RecordingArtifactSink()
    async with connect_cdp(cdp_url=cdp_url, attach=False, artifact_sink=sink) as page:
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
async def test_a_cancelled_open_closes_the_tab_it_opened(cdp_url: str, after_settling: bool) -> None:
    assert await cancelled_once_open(cdp_url, after_settling=after_settling) == set()


async def viewport_text(page: Any, url: str) -> str:
    """The page's text once a PDF it shows has been read or said unread: the read finishes after the navigation."""
    await page.navigate(url)
    for _ in range(100):
        text = (await page.observe()).viewport_text
        if "[Downloaded file: map.pdf" in text:
            return text
        await asyncio.sleep(0.1)
    return text


@pytest.mark.timeout(60)
async def test_a_pdf_is_read_by_this_interpreter_as_its_helper(cdp_url: str, main_site: str) -> None:
    async with connect_cdp(cdp_url=cdp_url, attach=False) as page:
        text = await viewport_text(page, f"{main_site}/map.pdf")
    assert f"[Downloaded file: map.pdf from {main_site}/map.pdf]\nFairbanks Wind" in text, text


# A process of its own, since a helper is registered once per process, as the package is imported. `prelude` runs
# before that import.
ELSEWHERE = """
import asyncio, sys
cdp_url, url, prelude = sys.argv[1:]
exec(prelude)
from tests.browser.test_native_connect import connect_cdp, viewport_text

async def main() -> None:
    async with connect_cdp(cdp_url=cdp_url, attach=False) as page:
        print(await viewport_text(page, url))

asyncio.run(main())
"""


def read_elsewhere(cdp_url: str, url: str, prelude: str) -> str:
    env = {**os.environ, "PYTHONPATH": str(Path(__file__).parents[2])}
    done = subprocess.run(
        [sys.executable, "-c", ELSEWHERE, cdp_url, url, prelude], env=env, capture_output=True, timeout=60
    )
    assert done.returncode == 0, done.stderr.decode(errors="replace")
    return done.stdout.decode()


@pytest.mark.skipif(sys.platform == "win32", reason="a path that is no UTF-8 is a POSIX path's to hold")
@pytest.mark.timeout(90)
def test_a_package_path_that_is_no_utf8_neither_fails_the_import_nor_stops_pdfs(cdp_url: str, main_site: str) -> None:
    """A directory another installation puts ahead of this one in the package's path, named in no UTF-8."""
    prelude = 'import fastbrowse, os; fastbrowse.__path__.insert(0, os.fsdecode(b"/nonexistent/caf\\xe9"))'
    text = read_elsewhere(cdp_url, f"{main_site}/map.pdf", prelude)
    assert "\nFairbanks Wind" in text, text


@pytest.mark.timeout(90)
def test_an_application_that_embeds_python_is_never_run_as_its_helper(
    cdp_url: str, main_site: str, tmp_path: Path
) -> None:
    """An embedding host hands Python its arguments unparsed, and its `sys.executable` is the host: never run it."""
    ran = tmp_path / "ran"
    host = tmp_path / "host"
    host.write_text(f"#!/bin/sh\ntouch {ran}\n")
    host.chmod(0o755)
    prelude = f"sys.orig_argv = list(sys.argv); sys.executable = sys._base_executable = {str(host)!r}"
    text = read_elsewhere(cdp_url, f"{main_site}/map.pdf", prelude)
    assert "its text was not read: this program has no document reader set up" in text, text
    assert not ran.exists()
