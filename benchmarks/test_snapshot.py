"""snapshot.js on dense pages, timed as `observe` pays for it: one CDP evaluate, from sending the script
to holding its result in Python.

    uv run --group bench pytest benchmarks --codspeed
"""

from __future__ import annotations

import asyncio

import pytest

from benchmarks.fixtures import PAGES, fingerprint, open_page, snapshot
from fastbrowse.browser.page import CdpPage


@pytest.mark.parametrize("name", list(PAGES))
def test_snapshot(benchmark, loop: asyncio.AbstractEventLoop, page: CdpPage, origin: str, name: str) -> None:
    loop.run_until_complete(open_page(page, origin, name, PAGES[name][1]))
    result = benchmark(lambda: loop.run_until_complete(snapshot(page)))
    assert result["controls"]


# The settle loop samples the fingerprint every 100ms while it waits for a page to go quiet.
@pytest.mark.parametrize("name", list(PAGES))
def test_fingerprint(benchmark, loop: asyncio.AbstractEventLoop, page: CdpPage, origin: str, name: str) -> None:
    loop.run_until_complete(open_page(page, origin, name, PAGES[name][1]))
    assert benchmark(lambda: loop.run_until_complete(fingerprint(page)))
