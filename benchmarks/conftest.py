"""One headless Chrome, one page on it and one fixture server for the whole benchmark session.

CodSpeed's `benchmark` fixture times a synchronous call, so the page runs on an event loop this module owns and each
measured call drives that loop to completion.
"""

from __future__ import annotations

import asyncio
from collections.abc import Iterator

import pytest

from benchmarks.fixtures import serve
from fastbrowse import connect_cdp
from fastbrowse.evals.harness import chrome
from fastbrowse.native import NativePage


@pytest.fixture(scope="session")
def loop() -> Iterator[asyncio.AbstractEventLoop]:
    loop = asyncio.new_event_loop()
    yield loop
    loop.close()


@pytest.fixture(scope="session")
def page(loop: asyncio.AbstractEventLoop) -> Iterator[NativePage]:
    with chrome() as cdp_url:
        opened = connect_cdp(cdp_url=cdp_url, attach=False)
        yield loop.run_until_complete(opened.__aenter__())
        loop.run_until_complete(opened.__aexit__(None, None, None))


@pytest.fixture(scope="session")
def origin() -> Iterator[str]:
    with serve() as origin:
        yield origin
