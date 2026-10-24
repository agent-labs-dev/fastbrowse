"""snapshot.js is compiled once per document, and a document that never installed it still gets observed."""

from __future__ import annotations

import time

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


async def test_settle_waits_for_a_shadow_root_added_while_it_reads_the_clock(page: CdpPage, main_site: str) -> None:
    await page.navigate(f"{main_site}/")
    # The host lands in the light DOM once; everything after happens inside its shadow root, which the document's
    # observer cannot see until a full sample starts watching it.
    await page._evaluate(
        page._session.active_session_id,
        "(() => { const host = document.createElement('div'); document.body.append(host); "
        "const root = host.attachShadow({mode: 'open'}); let n = 0; "
        "const tick = () => { root.textContent = String(n++); if (n < 16) setTimeout(tick, 50); }; tick(); })()",
    )
    started = time.monotonic()
    stable, _ = await page._settled_fingerprint(3.0)
    assert stable
    # 16 changes 50ms apart end about 750ms in; settling needs 200ms of quiet after the last.
    assert time.monotonic() - started >= 0.8
