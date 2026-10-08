"""Safety boundaries exercised through real Chrome, with only model responses scripted."""

import asyncio
import contextlib
import json
import threading
from collections.abc import AsyncIterator, Iterator
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, quote

import pytest
from pydantic import JsonValue

from fastbrowse.adapters.local_chrome import free_port
from fastbrowse.agent import Agent
from fastbrowse.browser import BrowserSession, CdpPage
from fastbrowse.browser.recording import Recording, RecordingError
from fastbrowse.browser.session import OriginNotAllowed
from fastbrowse.config import Config, Thresholds
from fastbrowse.models import (
    Attachment,
    Authorization,
    BrowserConnection,
    Limits,
    Operation,
    SecretRef,
    Status,
    StepOutcome,
)
from fastbrowse.origins import OriginGrant
from fastbrowse.page import Action, BrowserError, ScreenshotsUnavailable
from tests.browser.conftest import RecordingArtifactSink
from tests.browser.test_browser import eval_value, find, observe_until, wait_until
from tests.test_policy import ScriptedJev
from tests.test_retrieval import ScriptedLLM

PLAN: JsonValue = {"requirements": [], "answer_expected": False}
# The scripted Jev agrees with every yes/no question, so both wall questions are put out of reach: these
# fixture pages are neither a sign-in nor a bot check, and `tests/browser/test_walls.py` covers those.
CONFIG = Config(thresholds=Thresholds(login_required_above=1, bot_check_above=1))


class Secrets:
    def __init__(self, origin: str) -> None:
        self.origin = origin
        self.resolved: list[str] = []

    def available(self) -> tuple[SecretRef, ...]:
        return (SecretRef(name="token", origins=(self.origin,)),)

    async def resolve(self, name: str, origin: str) -> str:
        self.resolved.append(origin)
        return "top-secret-value"


@pytest.mark.parametrize("label", ["Username", "Frame field"])
async def test_window_origin_cannot_spoof_secret_scope(
    page: CdpPage, browser_session: BrowserSession, main_site: str, iframe_site: str, label: str
) -> None:
    await page.navigate(main_site)
    target = find(await observe_until(page, label), label)
    session_id = (
        browser_session.frame_sessions()[target.frame_id] if target.frame_id else browser_session.active_session_id
    )
    assert (
        await eval_value(browser_session, session_id, "window.origin = 'https://bank.example'")
        == "https://bank.example"
    )
    obs = await page.observe()
    target = find(obs, label)
    assert target.frame_origin == (iframe_site if target.frame_id else main_site)

    secrets = Secrets("https://bank.example")
    result = await Agent(
        page,
        ScriptedJev({"operation": "fill", "fill_target": target.id, "pick": "secret:token"}),
        ScriptedLLM([PLAN]),
        config=CONFIG,
        secrets=secrets,
    ).run("Fill the token")
    assert result.status is Status.NEEDS_INPUT and secrets.resolved == []

    obs = await page.observe()
    result = await page.act(
        Action(
            operation=Operation.FILL,
            target_id=target.id,
            text="do-not-leak",
            secret=True,
            secret_origin="https://bank.example",
        ),
        obs,
    )
    assert result.outcome is StepOutcome.FAILED
    assert find(await page.observe(), label).value == ""


async def test_secret_resolution_uses_receiving_origin(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(main_site)
    observed = await observe_until(page, "Frame field")
    target = find(observed, "Frame field")
    secrets = Secrets(main_site)
    agent = Agent(
        page,
        ScriptedJev({"operation": "fill", "fill_target": target.id, "pick": "secret:token"}),
        ScriptedLLM([PLAN]),
        config=CONFIG,
        secrets=secrets,
    )
    result = await agent.run("Fill the frame field with the token")
    assert result.status is Status.NEEDS_INPUT and secrets.resolved == []
    assert find(await page.observe(), "Frame field").value == ""


async def test_page_rechecks_secret_origin_before_insertion(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(main_site)
    obs = await observe_until(page, "Frame field")
    target = find(obs, "Frame field")
    result = await page.act(
        Action(operation=Operation.FILL, target_id=target.id, text="do-not-leak", secret=True, secret_origin=main_site),
        obs,
    )
    assert result.outcome is StepOutcome.FAILED
    assert find(await page.observe(), "Frame field").value == ""


@pytest.mark.parametrize("authorize", [False, True])
async def test_enter_uses_form_semantics_and_focuses_verified_target(
    page: CdpPage, browser_session: BrowserSession, main_site: str, authorize: bool
) -> None:
    await page.navigate(f"{main_site}/safety.html")
    obs = await page.observe()
    field = find(obs, "Account name")
    assert field.submit_semantics and "Delete account" in field.submit_semantics
    await eval_value(browser_session, browser_session.active_session_id, "document.getElementById('other').focus()")
    jev = ScriptedJev({"operation": "enter", "enter_target": field.id})
    result = await Agent(page, jev, ScriptedLLM([PLAN]), config=CONFIG).run(
        "Delete the account", authorization=Authorization(irreversible_actions=authorize), limits=Limits(max_steps=1)
    )
    assert any("irreversible" in request for request in jev.requests) == (not authorize)
    submitted = await eval_value(browser_session, browser_session.active_session_id, "document.body.dataset.submitted")
    if authorize:
        assert submitted == "delete" and result.steps[0].outcome is StepOutcome.EXECUTED
    else:
        assert submitted is None and result.status is Status.NEEDS_CONFIRMATION


@pytest.mark.parametrize("choice,authorize", [("accept", False), ("accept", True), ("dismiss", False)])
async def test_dialog_acceptance_has_its_own_gate(
    page: CdpPage, browser_session: BrowserSession, main_site: str, choice: str, authorize: bool
) -> None:
    await page.navigate(f"{main_site}/safety.html")
    obs = await page.observe()
    await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "Continue").id), obs)
    jev = ScriptedJev({"operation": "dialog", "pick": choice})
    result = await Agent(page, jev, ScriptedLLM([PLAN]), config=CONFIG).run(
        "Delete the account", authorization=Authorization(irreversible_actions=authorize), limits=Limits(max_steps=1)
    )
    if choice == "accept" and not authorize:
        assert result.status is Status.NEEDS_CONFIRMATION
        assert browser_session.pending_dialog() is not None
        await browser_session.handle_dialog(False)
    else:
        assert result.steps[0].outcome is StepOutcome.EXECUTED
        assert (
            await eval_value(browser_session, browser_session.active_session_id, "document.body.dataset.accepted")
            == str(choice == "accept").lower()
        )
    assert any("irreversible" in request for request in jev.requests) == (choice == "accept" and not authorize)


@pytest.mark.parametrize("operation", [Operation.SELECT, Operation.UPLOAD])
async def test_covered_controls_are_not_modified(
    page: CdpPage, browser_session: BrowserSession, main_site: str, operation: Operation
) -> None:
    await page.navigate(f"{main_site}/safety.html")
    await eval_value(
        browser_session, browser_session.active_session_id, "document.getElementById('cover').style.display = 'block'"
    )
    obs = await page.observe()
    target = find(obs, "Destination" if operation is Operation.SELECT else "Attachment")
    result = await page.act(
        Action(
            operation=operation,
            target_id=target.id,
            text="Second",
            files=(Attachment(name="data.txt", mime_type="text/plain", content=b"data"),),
        ),
        obs,
    )
    assert result.outcome is StepOutcome.COVERED
    assert await eval_value(
        browser_session,
        browser_session.active_session_id,
        "[document.querySelector('select').value, document.querySelector('[type=file]').files.length]",
    ) == ["First", 0]


async def test_iframe_focus_hit_testing_and_capture_scope(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(f"{main_site}/safety.html")

    async def inner_loaded() -> bool:
        return bool(
            await eval_value(
                browser_session,
                browser_session.active_session_id,
                "document.getElementById('same').contentDocument?.querySelector('input') !== null",
            )
        )

    await wait_until(inner_loaded)
    obs = await page.observe()
    field = find(obs, "Inner field")
    assert field.frame_id is not None
    assert (
        await page.act(Action(operation=Operation.FILL, target_id=field.id, text="landed"), obs)
    ).outcome is StepOutcome.EXECUTED
    assert find(await page.observe(), "Inner field").value == "landed"
    obs = await page.observe()
    button = find(obs, "Inner button")
    assert (await page.act(Action(operation=Operation.CLICK, target_id=button.id), obs)).outcome is StepOutcome.EXECUTED
    assert find(await page.observe(), "Inner clicked")
    capture = await page.capture()
    assert "Same-origin evidence" in capture.text and "Shadow evidence" in capture.text
    assert capture.inaccessible_frames == 0
    assert all(b.frame_id == field.frame_id for b in capture.blocks if "evidence" in capture.text[b.start : b.end])
    assert any("shadow:" in b.source_id for b in capture.blocks if "Shadow evidence" in capture.text[b.start : b.end])

    await page.navigate(main_site)
    obs = await observe_until(page, "Frame field")
    field = find(obs, "Frame field")
    assert (
        await page.act(Action(operation=Operation.FILL, target_id=field.id, text="frame value"), obs)
    ).outcome is StepOutcome.EXECUTED
    assert find(await page.observe(), "Frame field").value == "frame value"
    obs = await page.observe()
    opened = await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "Open popup").id), obs)
    assert opened.outcome is StepOutcome.EXECUTED, opened
    await wait_until(lambda: any("popup.html" in t.url for t in browser_session.tabs()))
    popup = next(t for t in browser_session.tabs() if "popup.html" in t.url)
    await browser_session.switch_tab(popup.id)
    assert browser_session.frame_sessions() == {}
    assert all("Frame field" not in c.label for c in (await page.observe()).controls)
    assert "Inside the frame" not in (await page.capture()).text


async def test_resolved_secret_is_masked_in_model_metadata_but_raw_url_is_preserved(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    from collections.abc import Mapping

    from fastbrowse.jev import Evaluation, Question
    from fastbrowse.models import Frozen

    class RecordingJev(ScriptedJev):
        def __init__(self) -> None:
            super().__init__({})
            self.states: list[JsonValue] = []

        async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
            self.states.append(state)
            return await super().evaluate(state, questions)

    class Output(Frozen):
        count: int

    await page.navigate(f"{main_site}/safety.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.getElementById('other').addEventListener('input', e => { "
        "document.title = e.target.value; history.replaceState({}, '', '#' + e.target.value); });",
    )
    obs = await page.observe()
    field = find(obs, "Other field")
    jev = RecordingJev()
    jev.pick = {"operation": "fill", "fill_target": field.id, "pick": "secret:token"}
    url_plan: JsonValue = {"requirements": [], "answer_expected": False, "run_reports": ["final_url"]}
    llm = ScriptedLLM([PLAN, url_plan, {"complete": True, "missing": []}, PLAN])
    agent = Agent(page, jev, llm, config=CONFIG, secrets=Secrets(main_site))
    first = await agent.run("Fill the token", limits=Limits(max_steps=1))
    assert first.steps[0].outcome is StepOutcome.EXECUTED
    assert "top-secret-value" in (await page.observe()).url

    seen_urls: list[str] = []

    async def until(url: str) -> bool:
        seen_urls.append(url)
        return True

    jev.pick = {"operation": "done"}
    second = await agent.run("Report the current URL", output_schema=Output, until=until)
    assert second.status is Status.COMPLETE
    assert seen_urls == [f"{main_site}/safety.html#top-secret-value"]
    obs = await page.observe()
    await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "Echo prompt").id), obs)
    jev.pick = {"operation": "dialog", "pick": "dismiss"}
    third = await agent.run("Dismiss the prompt", limits=Limits(max_steps=1))
    assert third.steps[0].outcome is StepOutcome.EXECUTED
    assert all("top-secret-value" not in str(state) for state in jev.states)
    assert all("top-secret-value" not in message.content for _, messages in llm.calls for message in messages)
    assert any("••••" in message.content for _, messages in llm.calls for message in messages)


async def test_fill_does_not_claim_execution_when_page_rejects_value(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(f"{main_site}/safety.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.getElementById('other').addEventListener('input', e => { e.target.value = ''; });",
    )
    obs = await page.observe()
    result = await page.act(
        Action(operation=Operation.FILL, target_id=find(obs, "Other field").id, text="rejected"), obs
    )
    assert result.outcome is StepOutcome.FAILED


# -- allowed_origins: documents outside the grant never load, are never read, and never leak ----------------

SECRET_TEXT = "FOREIGN-SECRET-TEXT"
SECRET_QUERY = "token=hunter2"


class OriginSites:
    """A granted site and a foreign one that counts every request it gets: the foreign log is the arbiter."""

    def __init__(self, granted: str, foreign_port: int, hits: list[str]) -> None:
        self.granted, self.foreign_port, self.hits = granted, foreign_port, hits

    def foreign(self, host: str = "127.0.0.1") -> str:
        return f"http://{host}:{self.foreign_port}"


@pytest.fixture
def sites() -> Iterator[OriginSites]:
    hits: list[str] = []
    foreign_port = free_port()

    class Foreign(BaseHTTPRequestHandler):
        def log_message(self, format: str, *args: object) -> None:
            pass

        def do_GET(self) -> None:
            hits.append(self.path)
            body = f"<title>Foreign title</title><h1>{SECRET_TEXT}</h1>".encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    class Granted(BaseHTTPRequestHandler):
        def log_message(self, format: str, *args: object) -> None:
            pass

        def do_GET(self) -> None:
            path, _, query = self.path.partition("?")
            to = parse_qs(query).get("to", [""])[0]
            if path == "/sw.js":
                # Answers /swtarget itself, so without a bypass the request never reaches the network or Fetch.
                script = "\n".join(
                    [
                        "self.addEventListener('install', () => self.skipWaiting());",
                        "self.addEventListener('activate', e => e.waitUntil(clients.claim()));",
                        "self.addEventListener('fetch', e => { const u = new URL(e.request.url);",
                        "  if (u.pathname === '/swtarget')",
                        "    e.respondWith(Response.redirect(u.searchParams.get('to'), 302)); });",
                    ]
                ).encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/javascript")
                self.send_header("Content-Length", str(len(script)))
                self.end_headers()
                self.wfile.write(script)
                return
            if path == "/redirect":
                self.send_response(302)
                self.send_header("Location", to)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            page = {
                "/": "<title>Granted home</title><h1>Granted home</h1><input type=file aria-label=Resume>",
                "/sw": "<h1>Granted worker</h1><script>navigator.serviceWorker.register('/sw.js')</script>",
                "/swtarget": "<title>Granted real page</title><h1>Granted real page</h1>",
                "/ok": "<title>Granted popup</title><h1>Granted popup</h1>",
                "/refresh": f"<meta http-equiv=refresh content='0;url={to}'><h1>Granted refresh</h1>",
                "/script": f"<h1>Granted script</h1><script>location = {json.dumps(to)}</script>",
                "/frame": f"<title>Granted frame page</title><h1>Granted frame page</h1><iframe src='{to}'></iframe>",
            }.get(path, "")
            body = page.encode()
            self.send_response(200 if page else 404)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    servers = [
        ThreadingHTTPServer(("127.0.0.1", foreign_port), Foreign),
        ThreadingHTTPServer(("127.0.0.1", 0), Granted),
    ]
    for server in servers:
        threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        yield OriginSites(f"http://127.0.0.1:{servers[1].server_port}", foreign_port, hits)
    finally:
        for server in servers:
            server.shutdown()


@contextlib.asynccontextmanager
async def scoped(
    connection: BrowserConnection, origin: str, **changes: object
) -> AsyncIterator[tuple[BrowserSession, CdpPage]]:
    scoped_connection = connection.model_copy(update={"allowed_origins": (origin,), **changes})
    async with BrowserSession(scoped_connection, RecordingArtifactSink()) as session:
        yield session, CdpPage(session, Config())


async def until_refused(session: BrowserSession, seconds: float = 5.0) -> None:
    """Wait for the browser's report of a foreign document to reach the guard."""
    async with asyncio.timeout(seconds):
        while True:
            try:
                await session.assert_clear()
            except OriginNotAllowed:
                return
            await asyncio.sleep(0.05)


async def settle() -> None:
    await asyncio.sleep(1.0)  # long enough for a request that was going to leave to have left


@pytest.mark.parametrize("route", ["direct", "redirect", "refresh", "script"])
async def test_navigation_and_redirects_never_reach_a_foreign_origin(
    chrome_connection: BrowserConnection, sites: OriginSites, route: str
) -> None:
    foreign = f"{sites.foreign()}/page?{SECRET_QUERY}"
    async with scoped(chrome_connection, sites.granted) as (session, page):
        if route == "direct":
            start = foreign
        else:
            start = f"{sites.granted}/{'redirect' if route == 'redirect' else route}?to={quote(foreign)}"
        if route in {"direct", "redirect"}:
            with pytest.raises(OriginNotAllowed) as refused:
                await page.navigate(start)
            assert SECRET_QUERY not in str(refused.value) and str(sites.foreign_port) not in str(refused.value)
        else:
            await page.navigate(start)
        await settle()
        assert sites.hits == []
        if route in {"direct", "redirect"}:
            with pytest.raises(OriginNotAllowed):
                await page.capture()
            with pytest.raises(OriginNotAllowed):
                await page.observe()
        else:
            assert SECRET_TEXT not in (await page.capture()).text
            assert SECRET_TEXT not in (await page.observe()).viewport_text
        assert SECRET_QUERY not in "".join(f"{t.url}{t.title}" for t in session.tabs())


@pytest.mark.parametrize("host", ["127.0.0.1", "localhost"], ids=["same-process", "out-of-process"])
async def test_frames_of_a_foreign_origin_never_load(
    chrome_connection: BrowserConnection, sites: OriginSites, host: str
) -> None:
    async with scoped(chrome_connection, sites.granted) as (_session, page):
        await page.navigate(f"{sites.granted}/frame?to={quote(sites.foreign(host) + '/page')}")
        await settle()
        assert sites.hits == []
        assert SECRET_TEXT not in (await page.capture()).text
        with pytest.raises(ScreenshotsUnavailable):
            await page.screenshot()


@pytest.mark.parametrize("opened", ["foreign", "redirect", "granted"])
async def test_popups_are_adopted_only_inside_the_grant(
    chrome_connection: BrowserConnection, sites: OriginSites, opened: str
) -> None:
    foreign = f"{sites.foreign()}/page?{SECRET_QUERY}"
    url = {
        "foreign": foreign,
        "redirect": f"{sites.granted}/redirect?to={quote(foreign)}",
        "granted": f"{sites.granted}/ok",
    }[opened]
    async with scoped(chrome_connection, sites.granted) as (session, page):
        await page.navigate(f"{sites.granted}/")
        await session.client.send.Runtime.evaluate(
            params={"expression": f"window.open({json.dumps(url)})", "userGesture": True},
            session_id=session.active_session_id,
        )
        await settle()
        assert sites.hits == []
        urls = [t.url for t in session.tabs()]
        assert (f"{sites.granted}/ok" in urls) is (opened == "granted")
        assert SECRET_QUERY not in "".join(urls)


async def test_an_attached_window_keeps_its_foreign_frames_and_other_windows_out(
    chrome_connection: BrowserConnection, browser_session: BrowserSession, sites: OriginSites
) -> None:
    # The window is the person's, already open with a foreign OOPIF in it before the run attaches.
    owner = CdpPage(browser_session, Config())
    await owner.navigate(f"{sites.granted}/frame?to={quote(sites.foreign('localhost') + '/page')}")
    await wait_until(lambda: bool(sites.hits))
    before = await eval_value(browser_session, browser_session.active_session_id, "location.href")
    async with scoped(chrome_connection, sites.granted, attach=True, target_match="Granted frame") as (_session, page):
        assert SECRET_TEXT not in (await page.capture()).text
        assert SECRET_TEXT not in (await page.observe()).viewport_text
        with pytest.raises(ScreenshotsUnavailable):
            await page.screenshot()
    # A window that is not the grant's is neither matched nor named, and nothing navigates the person's window.
    await owner.navigate(f"{sites.foreign()}/page")
    with pytest.raises(BrowserError, match="no page matching") as missing:
        async with scoped(chrome_connection, sites.granted, attach=True, target_match="/page"):
            pass
    assert "Foreign title" not in str(missing.value) and str(sites.foreign_port) not in str(missing.value)
    await owner.navigate(before)


async def test_a_document_the_network_never_saw_is_not_read_or_controlled(
    chrome_connection: BrowserConnection, sites: OriginSites
) -> None:
    async with scoped(chrome_connection, sites.granted) as (session, page):
        await page.navigate(f"{sites.granted}/")
        observed = await page.observe()
        # A data: document is not a request, so Fetch cannot refuse it; every read and mutation must.
        await session.client.send.Page.navigate(
            params={"url": f"data:text/html,<h1>{SECRET_TEXT}</h1>"}, session_id=session.active_session_id
        )
        await until_refused(session)
        assert SECRET_TEXT not in "".join(f"{t.url}{t.title}" for t in session.tabs())
        for read in (page.observe, page.capture, page.screenshot, page.address, page.origin):
            with pytest.raises(OriginNotAllowed):
                await read()
        with pytest.raises(OriginNotAllowed):
            await page.act(Action(operation=Operation.SCROLL), observed)


async def test_an_upload_script_checks_the_origin_it_runs_in(
    chrome_connection: BrowserConnection, sites: OriginSites
) -> None:
    async with scoped(chrome_connection, sites.granted) as (session, page):
        await page.navigate(f"{sites.granted}/")
        control = find(await page.observe(), "Resume")
        assert page._last is not None
        target = page._last.controls[control.id]
        files = (Attachment(name="cv.txt", mime_type="text/plain", content=b"cv"),)
        assert (await page._upload(target, files, (1.0, 1.0)))[0] is StepOutcome.EXECUTED
        # The document changed between the check in `act` and the write: the grant no longer names this origin.
        session._grant = OriginGrant([sites.foreign()])
        assert (await page._upload(target, files, (1.0, 1.0)))[0] is StepOutcome.STALE


async def test_a_service_worker_cannot_answer_a_navigation_past_the_gate(
    chrome_connection: BrowserConnection, sites: OriginSites
) -> None:
    foreign = f"{sites.foreign()}/page?{SECRET_QUERY}"
    async with scoped(chrome_connection, sites.granted) as (session, page):
        await page.navigate(f"{sites.granted}/sw")
        # The worker is registered by the page itself, from the granted origin, which is allowed.
        await session.client.send.Runtime.evaluate(
            params={"expression": "navigator.serviceWorker.ready.then(() => true)", "awaitPromise": True},
            session_id=session.active_session_id,
        )
        # The page is controlled by the worker, which would redirect this navigation to a foreign origin. Bypassed,
        # the request reaches the server, which has a page there, and the worker never answers it.
        await page.navigate(f"{sites.granted}/swtarget?to={quote(foreign)}")
        await settle()
        assert session.denials == 0 and sites.hits == []
        assert (await page.address()).startswith(f"{sites.granted}/swtarget")
        assert SECRET_TEXT not in (await page.capture()).text


async def test_a_scoped_run_delivers_no_live_frames(chrome_connection: BrowserConnection, sites: OriginSites) -> None:
    frames: list[bytes] = []

    async def receive(frame: bytes) -> None:
        frames.append(frame)

    connection = chrome_connection.model_copy(update={"allowed_origins": (sites.granted,)})
    async with BrowserSession(connection, RecordingArtifactSink(), on_frame=receive) as session:
        page = CdpPage(session, Config())
        await page.navigate(f"{sites.granted}/")
        await settle()
        assert session.frames_withheld and frames == []


async def test_a_scoped_session_refuses_to_record(chrome_connection: BrowserConnection, sites: OriginSites) -> None:
    async with scoped(chrome_connection, sites.granted) as (session, _page):
        with pytest.raises(RecordingError, match="allowed_origins"):
            async with Recording(session, Path("unused.mp4")):
                pass


@pytest.mark.parametrize("scope", [False, True], ids=["unscoped", "scoped"])
async def test_check_access_runs_before_every_browser_touch(
    chrome_connection: BrowserConnection, sites: OriginSites, scope: bool
) -> None:
    calls = 0
    allowed = True

    async def check() -> None:
        nonlocal calls
        calls += 1
        if not allowed:
            raise RuntimeError("secret detail the caller put in its error")

    connection = chrome_connection.model_copy(update={"allowed_origins": (sites.granted,) if scope else None})
    async with BrowserSession(connection, RecordingArtifactSink(), check_access=check) as session:
        page = CdpPage(session, Config())
        await page.navigate(f"{sites.granted}/")
        observed = await page.observe()
        touches = (
            page.observe,
            page.capture,
            page.screenshot,
            page.address,
            page.origin,
            page.response_status,
            lambda: page.navigate(f"{sites.granted}/ok"),
            lambda: page.act(Action(operation=Operation.SCROLL), observed),
            lambda: page.document_changed(observed),
            lambda: page.redrawn(observed, 0.1),
        )
        allowed = False
        for touch in touches:
            before = calls
            with pytest.raises(BrowserError, match="caller supplied") as refused:
                await touch()
            assert calls == before + 1 and "secret detail" not in str(refused.value)


async def test_a_blank_popup_inherits_no_foreign_document_grant(
    chrome_connection: BrowserConnection, browser_session: BrowserSession, sites: OriginSites
) -> None:
    owner = CdpPage(browser_session, Config())
    await owner.navigate(f"{sites.foreign()}/page")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "(() => { const popup = window.open('about:blank'); "
        "popup.document.write('<title>Inherited foreign document</title>outside-grant-secret'); return true; })()",
    )
    await settle()
    with pytest.raises(BrowserError):
        async with scoped(chrome_connection, sites.granted, attach=True, target_match="Inherited foreign document"):
            pytest.fail("A blank popup must not inherit a grant from its foreign opener")


@pytest.mark.parametrize("source", ["blank", "srcdoc", "blob"])
async def test_opaque_child_documents_never_enter_a_scoped_read(
    chrome_connection: BrowserConnection, browser_session: BrowserSession, sites: OriginSites, source: str
) -> None:
    owner = CdpPage(browser_session, Config())
    await owner.navigate(f"{sites.granted}/ok")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "(kind => { const frame = document.createElement('iframe'); "
        f"const html = '<p>{SECRET_TEXT}</p><button>Opaque child action</button>'; "
        "if (kind === 'srcdoc') frame.srcdoc = html; "
        "if (kind === 'blob') frame.src = URL.createObjectURL(new Blob([html], {type:'text/html'})); "
        "document.body.append(frame); if (kind === 'blank') frame.contentDocument.write(html); return true; })("
        + json.dumps(source)
        + ")",
    )
    await settle()
    assert SECRET_TEXT in (await owner.capture()).text
    async with scoped(chrome_connection, sites.granted, attach=True, target_match="/ok") as (_session, page):
        assert SECRET_TEXT not in (await page.capture()).text
        observed = await page.observe()
        assert SECRET_TEXT not in observed.viewport_text
        assert all(control.label != "Opaque child action" for control in observed.controls)


async def test_access_is_checked_before_connecting_to_a_browser() -> None:
    async def refused() -> None:
        raise PermissionError("private caller detail")

    from fastbrowse.run import connect_cdp

    with pytest.raises(BrowserError, match="caller supplied") as failure:
        async with connect_cdp(cdp_url="ws://127.0.0.1:1", check_access=refused):
            pytest.fail("Revoked callers must not enter a browser session")
    assert "private caller detail" not in str(failure.value)


async def test_scoped_screenshots_are_refused_even_on_a_clean_page(
    chrome_connection: BrowserConnection, sites: OriginSites
) -> None:
    async with scoped(chrome_connection, sites.granted) as (_session, page):
        await page.navigate(f"{sites.granted}/ok")
        with pytest.raises(BrowserError, match="screenshots are unavailable"):
            await page.screenshot()
