"""The whole `observe` a step makes on a dense page, snapshot.js inside it, on the Rust core.

uv run --group bench pytest benchmarks --codspeed
"""

from __future__ import annotations

import asyncio

import pytest

from benchmarks.fixtures import PAGES
from fastbrowse.native import NativePage


# What a step pays to read the page: the loading wait, the history entry, every frame's snapshot and the core's
# validation of it, with no settle before it.
@pytest.mark.parametrize("name", list(PAGES))
def test_observe(benchmark, loop: asyncio.AbstractEventLoop, page: NativePage, origin: str, name: str) -> None:
    loop.run_until_complete(page.navigate(f"{origin}/{name}.html?n={PAGES[name][1]}"))
    assert benchmark(lambda: loop.run_until_complete(page.observe())).controls
