"""One headless Chrome and one fixture server for the whole benchmark session.

CodSpeed's `benchmark` fixture times a synchronous call, so the browser runs on an event loop this module
owns and each measured call drives that loop to completion.
"""

from __future__ import annotations

import asyncio
from collections.abc import Iterator

import pytest

from benchmarks.fixtures import serve
from fastbrowse.adapters.local_chrome import local_chrome
from fastbrowse.browser.page import CdpPage
from fastbrowse.browser.session import BrowserSession
from fastbrowse.clients.environment import load_settings
from fastbrowse.config import Config
from tests.browser.conftest import RecordingArtifactSink


@pytest.fixture(scope="session")
def loop() -> Iterator[asyncio.AbstractEventLoop]:
    loop = asyncio.new_event_loop()
    yield loop
    loop.close()


@pytest.fixture(scope="session")
def page(loop: asyncio.AbstractEventLoop) -> Iterator[CdpPage]:
    with local_chrome(load_settings().local_chrome()) as connection:
        session = BrowserSession(connection, RecordingArtifactSink())
        loop.run_until_complete(session.__aenter__())
        try:
            yield CdpPage(session, Config())
        finally:
            loop.run_until_complete(session.__aexit__(None, None, None))


@pytest.fixture(scope="session")
def origin() -> Iterator[str]:
    with serve() as origin:
        yield origin
