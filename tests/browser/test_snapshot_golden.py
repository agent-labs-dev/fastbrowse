"""snapshot.js output on fixed pages, against the JSON it produced when last reviewed.

A speedup to snapshot.js has to leave every byte the snapshot reports alone, and these files are where a
change in what it reports shows up as a diff. After an intended change, rewrite them and review the diff:

    FASTBROWSE_UPDATE_GOLDEN=1 uv run pytest tests/browser/test_snapshot_golden.py -q
"""

from __future__ import annotations

import json
import os
from collections.abc import Iterator

import pytest

from benchmarks.fixtures import GOLDEN, PAGES, fingerprint, golden_view, open_page, serve, snapshot
from fastbrowse.browser.page import CdpPage


@pytest.fixture(scope="module")
def origin() -> Iterator[str]:
    with serve() as origin:
        yield origin


@pytest.mark.parametrize("name", list(PAGES))
async def test_snapshot_matches_golden(page: CdpPage, origin: str, name: str) -> None:
    await open_page(page, origin, name, PAGES[name][0])
    view = golden_view(await snapshot(page), await fingerprint(page), origin)
    path = GOLDEN / f"{name}.json"
    if os.environ.get("FASTBROWSE_UPDATE_GOLDEN") == "1":
        path.write_text(json.dumps(view, indent=1, ensure_ascii=False, sort_keys=True) + "\n", encoding="utf-8")
    assert view == json.loads(path.read_text(encoding="utf-8"))
