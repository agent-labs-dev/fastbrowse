"""snapshot.js reuses what it reads within one call; a guard read in a later call sees the page as it is then."""

from __future__ import annotations

import json
from collections.abc import Iterator

import pytest

from benchmarks.fixtures import open_page, serve, snapshot
from fastbrowse.browser.page import CdpPage


@pytest.fixture(scope="module")
def origin() -> Iterator[str]:
    with serve() as origin:
        yield origin


@pytest.mark.parametrize("change", ["label", "card"])
async def test_a_guard_after_the_snapshot_reads_the_changed_page(page: CdpPage, origin: str, change: str) -> None:
    await open_page(page, origin, "cards", 4)
    result = await snapshot(page)
    target = next(c for c in result["controls"] if c["label"] == "Details" and c.get("context") == "Product 1")
    # The label is read through the reused label, and the card's text through the reused innerText.
    edit = (
        "e.textContent = 'More'"
        if change == "label"
        else "e.closest('.card').querySelector('.price').textContent = '$1.00'"
    )
    changed = await page._evaluate(
        page._session.active_session_id,
        f"(() => {{ const r = window.__fastbrowse; const e = r.nodes.get({target['id']}); {edit}; "
        f"return r.guardKey(e) !== r.observed.get({target['id']}); }})()",
    )
    assert changed, json.dumps(target)
