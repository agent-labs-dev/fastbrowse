"""snapshot.js is compiled once per document, and a document that never installed it still gets observed."""

from __future__ import annotations

from fastbrowse.browser.page import CdpPage


async def test_a_new_document_installs_snapshot_as_it_starts(page: CdpPage, main_site: str) -> None:
    await page.navigate(f"{main_site}/")
    installed = await page._evaluate(
        page._session.active_session_id, "Object.keys(globalThis.__fastbrowseScripts || {}).length"
    )
    assert installed == 1


async def test_a_document_without_snapshot_installs_it_on_the_first_call(page: CdpPage, main_site: str) -> None:
    await page.navigate(f"{main_site}/")
    # As a document older than the session would be, or one whose script removed the global.
    await page._evaluate(page._session.active_session_id, "delete globalThis.__fastbrowseScripts")
    observation = await page.observe()
    assert observation.controls
    again = await page._evaluate(page._session.active_session_id, "!!globalThis.__fastbrowseScripts")
    assert again is True
