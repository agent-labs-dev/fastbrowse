"""Smoke flow for the shape Slack's admin export takes, at the browser layer.

Drop into `tests/browser/` and run `uv run pytest tests/browser/test_slack_export_flow.py -q`.
It needs Chrome, like the rest of that directory, and no network beyond 127.0.0.1.

The fixture reproduces the four things a Slack workspace export actually demands of the agent, each of which
has broken a generic form-driving loop somewhere before:

1. a two-step sign-in (identifier, then password) with a sixth-digit one-time code on its own screen,
2. a settings page whose controls are a <select>, a checkbox group and a button inside a wizard step,
3. an export that is *queued*, so success is a sentence on the page and not a new URL,
4. an archive that arrives as an attachment from another host, with a signed query string.

Assertions are about what fastbrowse observed and saved, never about pixels.
"""

from __future__ import annotations

import threading
from collections.abc import AsyncGenerator, Iterator
from contextlib import asynccontextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from fastbrowse.adapters.local_chrome import find_chrome, free_port, local_chrome
from fastbrowse.browser.page import CdpPage
from fastbrowse.browser.session import BrowserSession
from fastbrowse.clients.environment import load_settings
from fastbrowse.config import Config
from fastbrowse.models import Artifact, ArtifactKind, Operation, StepOutcome
from fastbrowse.page import Action
from tests.browser.test_browser import find

SIGN_IN = """<!doctype html><title>Sign in to Acme</title>
<h1>Sign in to Acme</h1>
<form method="get" action="/signin/password">
  <label>Email address <input name="email" type="email" autocomplete="username" required></label>
  <button type="submit">Continue</button>
</form>"""

PASSWORD = """<!doctype html><title>Sign in to Acme</title>
<h1>Sign in to Acme</h1>
<form method="get" action="/signin/code">
  <label>Password <input name="password" type="password" autocomplete="current-password" required></label>
  <button type="submit">Sign in</button>
</form>"""

CODE = """<!doctype html><title>Two-factor authentication</title>
<h1>Two-factor authentication</h1>
<form method="get" action="/admin/settings">
  <label>6-digit code <input name="code" inputmode="numeric" autocomplete="one-time-code" required></label>
  <button type="submit">Verify</button>
</form>"""

ADMIN = """<!doctype html><title>Acme - Import and export data</title>
<h1>Acme</h1><h2>Import and export data</h2>
<form method="post" action="/admin/export">
  <label>Export date range
    <select name="range">
      <option value="all">All time</option>
      <option value="year">Last year</option>
    </select>
  </label>
  <fieldset><legend>Include</legend>
    <label><input type="checkbox" name="include" value="dms"> Direct messages</label>
    <label><input type="checkbox" name="include" value="files"> File links</label>
  </fieldset>
  <button type="submit" name="action" value="start" aria-label="Start Export">Start Export</button>
</form>"""

QUEUED = """<!doctype html><title>Acme - Import and export data</title>
<h1>Acme</h1><h2>Import and export data</h2>
<p>Your export is being prepared. We'll send you an email once your export file is ready.</p>
<a href="/admin/export/42">Ready for download</a>"""


def legacy_page(host: str) -> str:
    """The archive page: its link is on another host and carries a signed query string, as a file host serves it."""
    return f"""<!doctype html><title>Acme - Export archive</title>
<h1>Export archive</h1>
<a id="ready" href="http://{host}/files-pri/T1-F1/acme_slack_export.zip?sig=deadbeef">Ready for download</a>"""


def _site(archive_host: str) -> type[BaseHTTPRequestHandler]:
    pages = {
        "/signin": SIGN_IN,
        "/signin/password": PASSWORD,
        "/signin/code": CODE,
        "/admin/settings": ADMIN,
        "/admin/export": QUEUED,
        "/admin/export/42": legacy_page(archive_host),
    }
    archive = b"PK\x03\x04" + b"slack export fixture" * 4096

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format: str, *args: object) -> None:
            pass

        def do_POST(self) -> None:
            length = int(self.headers.get("content-length", "0"))
            self.rfile.read(length)
            self._send(200, "text/html", QUEUED)

        def do_GET(self) -> None:
            path = self.path.split("?", 1)[0]
            if path.endswith("acme_slack_export.zip"):
                # Chrome hands this to the download manager rather than rendering it.
                self.send_response(200)
                self.send_header("Content-Type", "application/zip")
                self.send_header("Content-Disposition", 'attachment; filename="acme_slack_export.zip"')
                self.send_header("Content-Length", str(len(archive)))
                self.end_headers()
                self.wfile.write(archive)
                return
            page = pages.get(path)
            if page is None:
                self.send_response(404)
                self.end_headers()
                return
            self._send(200, "text/html", page)

        def _send(self, status: int, mime: str, body: str) -> None:
            raw = body.encode()
            self.send_response(status)
            self.send_header("Content-Type", mime)
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

    return Handler


@pytest.fixture(scope="module")
def slack_site() -> Iterator[str]:
    port = free_port()
    server = ThreadingHTTPServer(("127.0.0.1", port), _site(f"localhost:{port}"))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{port}"
    finally:
        server.shutdown()


class MemorySink:
    def __init__(self) -> None:
        self.saved: list[tuple[str, str, int]] = []

    async def put(self, kind: ArtifactKind, name: str, mime_type: str, content: bytes) -> Artifact:
        self.saved.append((name, mime_type, len(content)))
        return Artifact(
            kind=kind, name=name, mime_type=mime_type, size_bytes=len(content), sha256="0" * 64, uri=f"memory://{name}"
        )


@asynccontextmanager
async def _page(config: Config) -> AsyncGenerator[CdpPage]:
    if find_chrome(load_settings().chrome) is None:
        pytest.skip("Chrome is not installed")
    with local_chrome(load_settings().local_chrome()) as connection:
        async with BrowserSession(connection, MemorySink(), max_download_bytes=config.max_download_bytes) as session:
            yield CdpPage(session, config)


async def act(page: CdpPage, operation: Operation, label: str, text: str | None = None) -> StepOutcome:
    observation = await page.observe()
    control = find(observation, label)
    return (await page.act(Action(operation=operation, target_id=control.id, text=text), observation)).outcome


async def _sign_in_and_export(page: CdpPage, site: str) -> None:
    await page.navigate(f"{site}/signin")

    assert await act(page, Operation.FILL, "Email address", "owner@acme.test") is StepOutcome.EXECUTED
    assert await act(page, Operation.CLICK, "Continue") is StepOutcome.EXECUTED
    assert find(await page.observe(), "Password") is not None

    assert await act(page, Operation.FILL, "Password", "hunter2") is StepOutcome.EXECUTED
    assert await act(page, Operation.CLICK, "Sign in") is StepOutcome.EXECUTED
    assert find(await page.observe(), "6-digit code") is not None

    # The one-time code is only correct for its 30 seconds, so it is supplied here rather than at run start.
    assert await act(page, Operation.FILL, "6-digit code", "123456") is StepOutcome.EXECUTED
    assert await act(page, Operation.CLICK, "Verify") is StepOutcome.EXECUTED

    observation = await page.observe()
    assert "/admin/settings" in observation.url
    # A <select> whose option text differs from its value is the one form control a model most often gets wrong.
    assert await act(page, Operation.SELECT, "Export date range", "Last year") is StepOutcome.EXECUTED
    assert await act(page, Operation.CLICK, "Direct messages") is StepOutcome.EXECUTED
    assert await act(page, Operation.CLICK, "Start Export") is StepOutcome.EXECUTED

    queued = await page.observe()
    assert "being prepared" in queued.viewport_text

    # The emailed link lands on a page whose link is on another host; the download is still the run's artifact.
    await page.navigate(f"{site}/admin/export/42")
    assert await act(page, Operation.CLICK, "Ready for download") is StepOutcome.EXECUTED


async def test_export_flow_reaches_the_archive(slack_site: str) -> None:
    async with _page(Config()) as page:
        await _sign_in_and_export(page, slack_site)

        artifacts = page.artifacts
        assert [a.name for a in artifacts] == ["acme_slack_export.zip"]
        assert artifacts[0].mime_type == "application/zip"
        assert artifacts[0].size_bytes > 60_000


async def test_an_archive_over_the_ceiling_is_reported_not_silently_dropped(slack_site: str) -> None:
    """The regression this file exists for: a refused download used to leave no trace at all.

    The ceiling is the caller's, through `Config`, and the run is told the file was offered and refused, so it can
    report the archive as not downloaded instead of treating the click as a success.
    """
    async with _page(Config(max_download_bytes=1024)) as page:
        await page.navigate(f"{slack_site}/admin/export/42")
        assert await act(page, Operation.CLICK, "Ready for download") is StepOutcome.EXECUTED

        refusals = page._session.downloads_refused  # type: ignore[attr-defined]
        assert [r.name for r in refusals] == ["acme_slack_export.zip"]
        assert refusals[0].size_bytes > refusals[0].limit_bytes
        observation = await page.observe()
        assert "Download not kept" in observation.viewport_text
        assert page.artifacts == ()


async def test_the_signed_query_never_reaches_the_kept_name(slack_site: str) -> None:
    """The archive is served from another host with a signed query: the name keeps the path, not the credential."""
    async with _page(Config()) as page:
        await page.navigate(f"{slack_site}/admin/export/42")
        assert await act(page, Operation.CLICK, "Ready for download") is StepOutcome.EXECUTED
        names = [artifact.name for artifact in page.artifacts]
    assert names == ["acme_slack_export.zip"]
    assert not any("sig=" in name for name in names)
