"""Reproducible local timings and settling regressions, without model API calls."""

from __future__ import annotations

import asyncio
import hashlib
import importlib
import threading
import time
from collections import Counter
from collections.abc import Iterator
from contextlib import suppress
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

import pytest
from cdp_use.cdp.fetch.events import RequestPausedEvent

from fastbrowse.browser.page import CdpPage
from fastbrowse.browser.session import BrowserSession
from fastbrowse.config import Config
from fastbrowse.models import Operation, StepOutcome
from fastbrowse.page import Action
from tests.browser.conftest import RecordingArtifactSink
from tests.browser.test_browser import eval_value, find, observe_until, wait_until
from tests.browser.test_lifecycle import CONNECTION, CdpTransport

page_module = importlib.import_module("fastbrowse.browser.page")


@pytest.fixture(scope="module")
def settling_site() -> Iterator[str]:
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format: str, *args: object) -> None:
            pass

        def do_GET(self) -> None:
            if self.path == "/slow.svg":
                time.sleep(1.2)
                body = b'<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"/>'
                mime = "image/svg+xml"
            elif self.path == "/ready":
                body = b"""<!doctype html><title>Settling fixture</title>
                <button id="state">Stage 0</button><img src="/slow.svg">
                <script>
                for (const [i, delay] of [100, 280, 460].entries()) {
                  setTimeout(() => {
                    document.querySelector('button').textContent = 'Stage ' + (i + 1);
                  }, delay);
                }
                </script>"""
                mime = "text/html"
            else:
                body = b'<!doctype html><title>Start</title><a href="/ready">Navigate</a>'
                mime = "text/html"
            self.send_response(200)
            self.send_header("Content-Type", mime)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            # The next navigation can discard the slow image before its response arrives.
            with suppress(BrokenPipeError, ConnectionResetError):
                self.wfile.write(body)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


async def test_navigation_pauses_only_documents(
    page: CdpPage, browser_session: BrowserSession, main_site: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    calls: Counter[str] = Counter()
    paused: list[tuple[str, str]] = []
    send = browser_session.client.send_raw

    async def counted(method: str, params: Any = None, session_id: str | None = None) -> dict[str, Any]:
        calls[method] += 1
        return await send(method, params, session_id)

    def on_paused(event: RequestPausedEvent, session_id: str | None) -> None:
        paused.append((event["resourceType"], event["request"]["url"]))
        browser_session._on_request_paused(event, session_id)

    monkeypatch.setattr(browser_session.client, "send_raw", counted)
    browser_session.client.register.Fetch.requestPaused(on_paused)
    await page.navigate(main_site)
    await wait_until(lambda: bool(browser_session.frame_sessions()))
    await asyncio.sleep(0.25)
    assert len(paused) == calls["Fetch.continueRequest"] == 2
    assert all(kind == "Document" for kind, _ in paused)


async def test_download_resource_types(page: CdpPage, browser_session: BrowserSession, main_site: str) -> None:
    paused: list[tuple[str, str]] = []

    def on_paused(event: RequestPausedEvent, session_id: str | None) -> None:
        paused.append((event["resourceType"], event["request"]["url"]))
        browser_session._on_request_paused(event, session_id)

    browser_session.client.register.Fetch.requestPaused(on_paused)
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.querySelector('#download-link').download = 'named.bin'",
    )
    obs = await page.observe()
    await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "Download attachment").id), obs)
    await wait_until(lambda: bool(browser_session.artifacts))
    assert (
        browser_session.artifacts[0].sha256
        == hashlib.sha256(b"fastbrowse fixture attachment bytes for download checksum test").hexdigest()
    )
    assert ("Document", f"{main_site}/download") in paused
    assert all(kind == "Document" for kind, _ in paused)


async def test_settling_waits_out_staged_hydration(
    page: CdpPage, browser_session: BrowserSession, settling_site: str
) -> None:
    await browser_session.client.send.Target.activateTarget(params={"targetId": browser_session.active_target_id})
    await page.navigate(settling_site)
    obs = await page.observe()
    result = await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "Navigate").id), obs)
    assert result.outcome is StepOutcome.EXECUTED and result.page_changed
    assert find(await page.observe(), "Stage 3")
    # The slow image is still loading: settling no longer waits for readyState complete.
    assert await eval_value(browser_session, browser_session.active_session_id, "document.readyState") == "interactive"


@pytest.mark.parametrize("nested", ["document", "shadow", "iframe"])
async def test_settling_tracks_same_length_mutations(
    page: CdpPage, browser_session: BrowserSession, settling_site: str, nested: str
) -> None:
    await browser_session.client.send.Target.activateTarget(params={"targetId": browser_session.active_target_id})
    await page.navigate(settling_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.body.innerHTML = '<button>Change</button><div></div><iframe></iframe>'; "
        "const root = "
        + {
            "document": "document.querySelector('div')",
            "shadow": "document.querySelector('div').attachShadow({mode: 'open'})",
            "iframe": "document.querySelector('iframe').contentDocument.body",
        }[nested]
        + "; root.innerHTML = '<p>State 0</p>'; "
        "document.querySelector('button').onclick = () => { "
        "for (const [i, delay] of [100, 280, 460].entries()) "
        "setTimeout(() => { root.querySelector('p').textContent = 'State ' + (i + 1); }, delay); }; "
        "window.fixtureRoot = root;",
    )
    obs = await page.observe()
    result = await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "Change").id), obs)
    assert result.outcome is StepOutcome.EXECUTED
    assert result.page_changed
    assert (
        await eval_value(browser_session, browser_session.active_session_id, "window.fixtureRoot.textContent")
        == "State 3"
    )


async def test_observe_still_reads_hydration_after_settling(
    page: CdpPage, browser_session: BrowserSession, settling_site: str
) -> None:
    await page.navigate(settling_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.body.innerHTML = '<button>Hydrate later</button>'; "
        "document.querySelector('button').onclick = () => setTimeout(() => { "
        "document.querySelector('button').outerHTML = '<button>Hydrated now</button>'; }, 700);",
    )
    before = await page.observe()
    target = find(before, "Hydrate later")
    await page.act(Action(operation=Operation.CLICK, target_id=target.id), before)
    after = await observe_until(page, "Hydrated now")
    assert after.page_key != before.page_key
    assert find(after, "Hydrated now").id != target.id
    assert (await page.act(Action(operation=Operation.CLICK, target_id=target.id), before)).outcome is StepOutcome.STALE


async def test_navigation_after_observe_rejects_old_control(page: CdpPage, settling_site: str) -> None:
    await page.navigate(settling_site)
    obs = await page.observe()
    target = find(obs, "Navigate")
    await page.navigate(f"{settling_site}/?new-document")
    result = await page.act(Action(operation=Operation.CLICK, target_id=target.id), obs)
    assert result.outcome is StepOutcome.STALE and not result.page_changed


async def test_hidden_tab_requires_dom_quiet(
    page: CdpPage, browser_session: BrowserSession, settling_site: str
) -> None:
    await page.navigate(settling_site)
    await page.observe()
    other = await browser_session.client.send.Target.createTarget(params={"url": "about:blank", "background": False})
    try:
        assert await eval_value(browser_session, browser_session.active_session_id, "document.hidden")
        await eval_value(
            browser_session, browser_session.active_session_id, "document.body.setAttribute('data-state', 'new')"
        )
        stable, fingerprint = await page._settled_fingerprint(1)
        assert not stable and fingerprint is not None
        await asyncio.sleep(0.25)
        stable, current = await page._settled_fingerprint(1)
        assert stable and current == fingerprint
    finally:
        await browser_session.client.send.Target.closeTarget(params={"targetId": other["targetId"]})


async def test_continuous_mutations_are_bounded(
    page: CdpPage, browser_session: BrowserSession, settling_site: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(page_module, "_SETTLE_SECONDS", 0.5)
    await browser_session.client.send.Target.activateTarget(params={"targetId": browser_session.active_target_id})
    await page.navigate(settling_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.body.innerHTML = '<button>Animate</button>'; "
        "document.querySelector('button').onclick = () => { "
        "window.animation = setInterval(() => { document.body.dataset.tick = performance.now(); }, 30); };",
    )
    obs = await page.observe()
    start = time.monotonic()
    try:
        result = await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "Animate").id), obs)
        assert result.outcome is StepOutcome.EXECUTED and result.page_changed
        assert time.monotonic() - start < 1.5
    finally:
        await eval_value(browser_session, browser_session.active_session_id, "clearInterval(window.animation)")


async def test_cancelled_settling_drains_renderer_and_dialog_waits(monkeypatch: pytest.MonkeyPatch) -> None:
    transport = CdpTransport(monkeypatch)
    started = transport.blocked["Runtime.evaluate"] = asyncio.Event()
    dialog_finished = asyncio.Event()

    async def wait_for_dialog() -> None:
        try:
            await asyncio.Future[None]()
        finally:
            dialog_finished.set()

    async with BrowserSession(CONNECTION, RecordingArtifactSink()) as session:
        monkeypatch.setattr(session, "wait_for_dialog", wait_for_dialog)
        page = CdpPage(session, Config())
        task = asyncio.create_task(page._changed_since("before"))
        try:
            await asyncio.wait_for(started.wait(), timeout=2)
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task
            assert "Runtime.evaluate" in transport.finished
            assert dialog_finished.is_set()
        finally:
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)
