"""Failure and cancellation paths that need deterministic transport responses."""

import asyncio
import importlib
import json
import re
import ssl
import subprocess
import sys
import threading
import time
from collections.abc import Generator
from contextlib import contextmanager
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock, Mock

import httpx
import pytest
from cdp_use.client import CDPClient
from pydantic import ValidationError
from websockets.exceptions import InvalidMessage

from fastbrowse.adapters.browser_use_cloud import BrowserUseCloudBrowser, BrowserUseCloudError
from fastbrowse.adapters.local_chrome import async_local_chrome, find_chrome, local_chrome
from fastbrowse.agent import Agent
from fastbrowse.browser import BrowserSession, CdpPage
from fastbrowse.browser import session as browser_session
from fastbrowse.config import Config
from fastbrowse.models import (
    BrowserConnection,
    CostBreakdown,
    CostLine,
    LocalChrome,
    RunResult,
    Status,
    StepOutcome,
    Unavailable,
)
from fastbrowse.page import BrowserError, NavigationTimeout
from fastbrowse.run import _browser, run_task
from tests.browser.conftest import RecordingArtifactSink
from tests.test_policy import ScriptedJev
from tests.test_retrieval import ScriptedLLM

CONNECTION = BrowserConnection(cdp_url="ws://localhost:9222", remote=False)
chrome_adapter = importlib.import_module("fastbrowse.adapters.local_chrome")
page_module = importlib.import_module("fastbrowse.browser.page")


class CdpTransport:
    def __init__(self, monkeypatch: pytest.MonkeyPatch) -> None:
        self.calls: list[str] = []
        self.requests: list[tuple[str, Any, str | None]] = []
        self.failures: dict[str, BaseException] = {}
        self.blocked: dict[str, asyncio.Event] = {}
        self.delays: dict[str, float] = {}
        self.finished: set[str] = set()
        self.results: dict[str, list[dict[str, Any]]] = {}
        monkeypatch.setattr(CDPClient, "start", AsyncMock())
        monkeypatch.setattr(CDPClient, "stop", AsyncMock(side_effect=lambda: self.calls.append("stop")))
        monkeypatch.setattr(CDPClient, "send_raw", AsyncMock(side_effect=self.send))

    async def send(self, method: str, params: Any = None, session_id: str | None = None) -> dict[str, Any]:
        self.calls.append(method)
        self.requests.append((method, params, session_id))
        if method in self.failures:
            raise self.failures[method]
        if queued := self.results.get(method):
            return queued.pop(0)
        await asyncio.sleep(self.delays.get(method, 0))
        if started := self.blocked.get(method):
            started.set()
            try:
                await asyncio.Future[None]()
            finally:
                await asyncio.sleep(0)
                self.finished.add(method)
        return {
            "Target.createTarget": {"targetId": "owned"},
            "Target.attachToTarget": {"sessionId": "session"},
            "Runtime.evaluate": {"result": {"value": "loading"}},
        }.get(method, {})


@pytest.mark.parametrize(
    "method", ["Target.setDiscoverTargets", "Target.attachToTarget", "Runtime.enable", "Fetch.enable"]
)
@pytest.mark.parametrize("cancelled", [False, True])
async def test_session_setup_rolls_back(monkeypatch: pytest.MonkeyPatch, method: str, cancelled: bool) -> None:
    transport = CdpTransport(monkeypatch)
    session = BrowserSession(CONNECTION, RecordingArtifactSink())
    if cancelled:
        started = transport.blocked[method] = asyncio.Event()
        task = asyncio.create_task(session.__aenter__())
        try:
            await started.wait()
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task
            assert method in transport.finished
        finally:
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)
    else:
        transport.failures[method] = RuntimeError({"code": -32000, "message": "secret"})
        with pytest.raises(BrowserError) as raised:
            await session.__aenter__()
        assert "secret" not in str(raised.value)
    assert transport.calls[-1] == "stop"
    assert ("Target.closeTarget" in transport.calls) == (method != "Target.setDiscoverTargets")


def dropped_handshake(cause: Exception) -> InvalidMessage:
    error = InvalidMessage("did not receive a valid HTTP response")
    error.__cause__ = cause
    return error


@pytest.mark.parametrize(
    ("failure", "outage"),
    [
        (dropped_handshake(EOFError("connection closed while reading HTTP status line")), True),
        (ConnectionRefusedError(), True),
        (dropped_handshake(ValueError("unsupported protocol; expected HTTP/1.1: HTTP/1.0 401")), False),
        (ssl.SSLCertVerificationError(), False),
    ],
    ids=["dropped handshake", "refused", "malformed reply", "bad certificate"],
)
async def test_a_browser_unreachable_at_start_is_an_outage(
    monkeypatch: pytest.MonkeyPatch, failure: Exception, outage: bool
) -> None:
    CdpTransport(monkeypatch)
    monkeypatch.setattr(CDPClient, "start", AsyncMock(side_effect=failure))
    with pytest.raises(BrowserError) as raised:
        await BrowserSession(CONNECTION, RecordingArtifactSink()).__aenter__()
    assert isinstance(raised.value, Unavailable) is outage


@pytest.mark.parametrize("failure", [RuntimeError({"code": -32000, "message": "secret"}), ConnectionError("secret")])
async def test_cdp_errors_are_typed_with_safe_messages(monkeypatch: pytest.MonkeyPatch, failure: Exception) -> None:
    transport = CdpTransport(monkeypatch)
    async with BrowserSession(CONNECTION, RecordingArtifactSink()) as session:
        transport.failures["Page.navigate"] = failure
        with pytest.raises(BrowserError) as raised:
            await CdpPage(session, Config()).navigate("https://example.test/secret")
        assert raised.value.__cause__ is failure
        assert str(raised.value).startswith("Page.navigate failed (")
        assert "secret" not in str(raised.value)


LOADED = {"result": {"value": "complete"}}
SETTLED = {"result": {"value": [True, "fingerprint"]}}
"""Navigation waits for a loaded document, then for its DOM to go quiet."""


@pytest.mark.parametrize(
    ("errors", "raised"),
    [
        (["net::ERR_TUNNEL_CONNECTION_FAILED"], None),
        (["net::ERR_TUNNEL_CONNECTION_FAILED"] * 2, "Page.navigate failed (net::ERR_TUNNEL_CONNECTION_FAILED)"),
        (["secret https://example.test/secret"] * 2, "Page.navigate failed (NavigationError)"),
    ],
)
async def test_a_failed_navigation_is_tried_again_once(
    monkeypatch: pytest.MonkeyPatch, errors: list[str], raised: str | None
) -> None:
    transport = CdpTransport(monkeypatch)
    transport.results["Page.navigate"] = [{"errorText": error} for error in errors]
    transport.results["Runtime.evaluate"] = [LOADED, SETTLED]
    monkeypatch.setattr(page_module, "_NAVIGATE_RETRY_SECONDS", 0)
    async with BrowserSession(CONNECTION, RecordingArtifactSink()) as session:
        navigating = CdpPage(session, Config()).navigate("https://example.test")
        if raised is None:
            await navigating
        else:
            # A real navigation error (a net:: failure, or one the page reports with no error text at all) is
            # never mistaken for a timeout: only the readiness wait exhausting its budget classifies as one.
            with pytest.raises(BrowserError, match=re.escape(raised)) as caught:
                await navigating
            assert not isinstance(caught.value, NavigationTimeout)
    assert transport.calls.count("Page.navigate") == min(len(errors) + 1, 2)


async def test_a_page_that_never_loads_is_tried_again_once(monkeypatch: pytest.MonkeyPatch) -> None:
    transport = CdpTransport(monkeypatch)
    monkeypatch.setattr(page_module, "_NAVIGATE_RETRY_SECONDS", 0)
    async with BrowserSession(CONNECTION, RecordingArtifactSink()) as session:
        with pytest.raises(NavigationTimeout, match=re.escape("Page.navigate failed (TimeoutError)")):
            await CdpPage(session, Config()).navigate("https://example.test", load_timeout_seconds=0.1)
        transport.results["Runtime.evaluate"] = [LOADED, SETTLED]
        await CdpPage(session, Config()).navigate("https://example.test", load_timeout_seconds=0.1)
    assert transport.calls.count("Page.navigate") == 3


def _history(entries: list[str], current: int) -> dict[str, Any]:
    return {"currentIndex": current, "entries": [{"id": i, "url": url} for i, url in enumerate(entries)]}


@pytest.mark.parametrize(
    ("entries", "current"),
    [
        (["about:blank", "https://example.test/wizard"], 1),
        (["https://other.test/", "https://example.test/"], 1),
        (["https://example.test/"], 0),
    ],
    ids=["blank predecessor", "cross-origin predecessor", "empty history"],
)
async def test_can_go_back_is_false_without_a_same_origin_predecessor(
    monkeypatch: pytest.MonkeyPatch, entries: list[str], current: int
) -> None:
    transport = CdpTransport(monkeypatch)
    transport.results["Page.getNavigationHistory"] = [_history(entries, current)]
    async with BrowserSession(CONNECTION, RecordingArtifactSink()) as session:
        assert await CdpPage(session, Config())._can_go_back() is False


async def test_can_go_back_is_true_with_a_same_origin_predecessor(monkeypatch: pytest.MonkeyPatch) -> None:
    transport = CdpTransport(monkeypatch)
    transport.results["Page.getNavigationHistory"] = [
        _history(["https://example.test/start", "https://example.test/next"], 1)
    ]
    async with BrowserSession(CONNECTION, RecordingArtifactSink()) as session:
        assert await CdpPage(session, Config())._can_go_back() is True


async def test_back_refuses_to_navigate_when_the_predecessor_is_no_longer_same_origin(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # History can change between the observation that offered BACK and this dispatch, so `_back` re-checks the
    # live history rather than trusting a guard the caller computed from a stale observation.
    transport = CdpTransport(monkeypatch)
    transport.results["Page.getNavigationHistory"] = [_history(["about:blank", "https://example.test/"], 1)]
    async with BrowserSession(CONNECTION, RecordingArtifactSink()) as session:
        outcome, detail = await CdpPage(session, Config())._back()
    assert outcome is StepOutcome.FAILED
    assert detail == "no same-origin earlier history entry"
    assert "Page.navigateToHistoryEntry" not in transport.calls


async def test_back_navigates_to_a_same_origin_predecessor_and_can_be_taken_twice(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    transport = CdpTransport(monkeypatch)
    transport.results["Page.getNavigationHistory"] = [
        _history(["https://example.test/a", "https://example.test/b", "https://example.test/c"], 2),
        _history(["https://example.test/a", "https://example.test/b", "https://example.test/c"], 1),
    ]
    async with BrowserSession(CONNECTION, RecordingArtifactSink()) as session:
        page = CdpPage(session, Config())
        first = await page._back()
        second = await page._back()
    assert first == (StepOutcome.EXECUTED, None)
    assert second == (StepOutcome.EXECUTED, None)
    assert transport.calls.count("Page.navigateToHistoryEntry") == 2


async def test_background_finalizers_finish_before_socket_stops(monkeypatch: pytest.MonkeyPatch) -> None:
    transport = CdpTransport(monkeypatch)
    started = transport.blocked["Fetch.getResponseBody"] = asyncio.Event()
    async with BrowserSession(CONNECTION, RecordingArtifactSink()) as session:
        await session.client.emit_event(
            "Fetch.requestPaused",
            {
                "requestId": "download",
                "responseHeaders": [{"name": "content-disposition", "value": "attachment"}],
                "request": {"url": "https://example.test/file"},
            },
            "session",
        )
        await started.wait()
    assert "Fetch.getResponseBody" in transport.finished
    assert transport.calls.index("Fetch.continueRequest") < transport.calls.index("stop")


@pytest.mark.parametrize("method", ["Input.dispatchKeyEvent", "Page.captureScreenshot"])
async def test_cancelling_page_wait_drains_cdp_task(monkeypatch: pytest.MonkeyPatch, method: str) -> None:
    transport = CdpTransport(monkeypatch)
    started = transport.blocked[method] = asyncio.Event()
    async with BrowserSession(CONNECTION, RecordingArtifactSink()) as session:
        page = CdpPage(session, Config())
        operation = (
            page.screenshot()
            if method == "Page.captureScreenshot"
            else page._input(
                session.client.send.Input.dispatchKeyEvent(
                    params={"type": "keyDown", "key": "Enter"}, session_id=session.active_session_id
                )
            )
        )
        task = asyncio.create_task(operation)
        try:
            await started.wait()
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task
            assert method in transport.finished
        finally:
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)


async def test_initial_navigation_error_returns_error_result(monkeypatch: pytest.MonkeyPatch) -> None:
    transport = CdpTransport(monkeypatch)
    transport.failures["Page.navigate"] = ConnectionError("secret")

    @contextmanager
    def chrome(_binary: str | None) -> Generator[BrowserConnection]:
        yield CONNECTION

    monkeypatch.setattr(chrome_adapter, "local_chrome", chrome)
    result = await run_task("Read", start="https://example.test", jev=ScriptedJev({}), llm=ScriptedLLM([]))
    assert result.status is Status.ERROR
    assert result.error == "Page.navigate failed (ConnectionError)"


@pytest.mark.parametrize("failure", ["validation", "cancel", "unexpected"])
async def test_cloud_setup_failure_stops_created_browser(failure: str) -> None:
    methods: list[str] = []

    async def respond(request: httpx.Request) -> httpx.Response:
        methods.append(request.method)
        if request.method == "GET":
            if failure == "cancel":
                raise asyncio.CancelledError()
            raise RuntimeError("setup failed")
        return httpx.Response(
            200,
            json={
                "id": "created",
                "cdpUrl": 123 if failure == "validation" and request.method == "POST" else "https://cdp.test",
                "browserCost": "0.25",
                "proxyCost": "0.10",
            },
        )

    expected = {"validation": ValidationError, "cancel": asyncio.CancelledError, "unexpected": RuntimeError}[failure]
    async with httpx.AsyncClient(transport=httpx.MockTransport(respond)) as http:
        cloud = BrowserUseCloudBrowser("key", http=http)
        with pytest.raises(expected):
            await cloud.__aenter__()
        assert methods[-1] == "PATCH"
        assert [line.dollars for line in cloud.cost] == [0.25, 0.10]


@pytest.mark.parametrize("during", ["POST", "PATCH"])
async def test_cloud_cancellation_waits_for_billable_requests(during: str) -> None:
    started, release = asyncio.Event(), asyncio.Event()
    methods: list[str] = []

    async def respond(request: httpx.Request) -> httpx.Response:
        methods.append(request.method)
        if request.method == during:
            started.set()
            await release.wait()
        return httpx.Response(
            200,
            json={
                "id": "created",
                "cdpUrl": "https://cdp.test",
                "webSocketDebuggerUrl": "ws://cdp.test",
                "browserCost": "0.25",
            },
        )

    async with httpx.AsyncClient(transport=httpx.MockTransport(respond)) as http:
        cloud = BrowserUseCloudBrowser("key", http=http)

        async def use() -> None:
            async with cloud:
                pass

        task = asyncio.create_task(use())
        try:
            await started.wait()
            task.cancel()
            release.set()
            with pytest.raises(asyncio.CancelledError):
                await task
            assert methods[-1] == "PATCH"
            assert cloud.cost[0].dollars == 0.25
        finally:
            release.set()
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)


@pytest.mark.parametrize("stop_fails", [False, True])
async def test_cloud_teardown_preserves_original_error_and_cost(stop_fails: bool) -> None:
    def respond(request: httpx.Request) -> httpx.Response:
        if request.method == "PATCH" and stop_fails:
            return httpx.Response(503)
        return httpx.Response(
            200,
            json={
                "id": "created",
                "cdpUrl": "https://cdp.test",
                "webSocketDebuggerUrl": "ws://cdp.test",
                "browserCost": "0.25" if request.method == "POST" else "0.50",
            },
        )

    cost: list[CostLine] = []
    original = RuntimeError("original")
    async with httpx.AsyncClient(transport=httpx.MockTransport(respond)) as http:
        with pytest.raises(RuntimeError) as raised:
            async with _browser("key", LocalChrome(), http, cost):
                raise original
        assert raised.value is original
        assert cost[0].dollars == (0.25 if stop_fails else 0.50)
        if stop_fails:
            with pytest.raises(BrowserUseCloudError):
                async with BrowserUseCloudBrowser("key", http=http):
                    pass


@pytest.mark.parametrize("cancel_start", [False, True])
async def test_local_chrome_threads_startup_and_shutdown(monkeypatch: pytest.MonkeyPatch, cancel_start: bool) -> None:
    loop = asyncio.get_running_loop()
    started, stopped = asyncio.Event(), asyncio.Event()
    release = threading.Event()
    loop_thread = threading.get_ident()

    @contextmanager
    def chrome(_binary: str | None) -> Generator[BrowserConnection]:
        assert threading.get_ident() != loop_thread
        loop.call_soon_threadsafe(started.set)
        assert release.wait(timeout=5)
        try:
            yield CONNECTION
        finally:
            assert threading.get_ident() != loop_thread
            loop.call_soon_threadsafe(stopped.set)

    monkeypatch.setattr(chrome_adapter, "local_chrome", chrome)

    async def use() -> None:
        async with async_local_chrome(LocalChrome()):
            pass

    task = asyncio.create_task(use())
    try:
        await asyncio.wait_for(started.wait(), timeout=2)
        if cancel_start:
            task.cancel()
        release.set()
        if cancel_start:
            with pytest.raises(asyncio.CancelledError):
                await task
        else:
            await task
        assert stopped.is_set()
    finally:
        release.set()
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)


def test_chrome_is_killed_and_reaped_on_shutdown_timeout(monkeypatch: pytest.MonkeyPatch) -> None:
    process = Mock()
    process.wait.side_effect = [subprocess.TimeoutExpired("chrome", 10), 0]
    monkeypatch.setattr(chrome_adapter, "find_chrome", Mock(return_value="/chrome"))
    monkeypatch.setattr(chrome_adapter.subprocess, "Popen", Mock(return_value=process))
    monkeypatch.setattr(chrome_adapter, "_wait_for_ws", Mock(return_value=CONNECTION.cdp_url))
    with local_chrome(LocalChrome()):
        pass
    process.terminate.assert_called_once()
    process.kill.assert_called_once()
    assert process.wait.call_count == 2


@pytest.mark.skipif(sys.platform == "win32", reason="the stand-in Chrome is a shell script")
def test_a_chrome_that_exits_at_start_fails_at_once_with_its_own_error(tmp_path: Path) -> None:
    binary = tmp_path / "chrome"
    binary.write_text(
        "#!/bin/sh\necho 'Running as root without --no-sandbox is not supported' >&2\nexit 1\n", encoding="utf-8"
    )
    binary.chmod(0o755)
    started = time.monotonic()
    with (
        pytest.raises(RuntimeError, match="status 1 before DevTools started:\nRunning as root"),
        local_chrome(LocalChrome(binary=str(binary))),
    ):
        pass
    assert time.monotonic() - started < 5


@pytest.mark.parametrize(
    "binary",
    [
        "google-chrome-stable",
        "google-chrome",
        "chromium",
        "chromium-browser",
        "chrome",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/custom/chrome",
    ],
)
def test_chrome_discovery(monkeypatch: pytest.MonkeyPatch, binary: str) -> None:
    def which(name: str) -> str | None:
        return binary if name == binary else None

    monkeypatch.setattr(chrome_adapter.shutil, "which", which)
    assert find_chrome(binary if binary == "/custom/chrome" else None) == binary


def test_chrome_discovery_finds_a_windows_install_off_path(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("LOCALAPPDATA", "C:/Users/me/AppData/Local")
    installed = str(Path("C:/Users/me/AppData/Local", "Google", "Chrome", "Application", "chrome.exe"))

    def which(name: str) -> str | None:
        return name if name == installed else None

    monkeypatch.setattr(chrome_adapter.shutil, "which", which)
    assert find_chrome(None) == installed


async def test_a_cloud_profile_starts_the_browser_signed_in_as_that_profile() -> None:
    """The run inherits the profile's cookies, so nothing has to type the credentials behind them."""
    bodies: list[object] = []

    def respond(request: httpx.Request) -> httpx.Response:
        if request.method == "POST":
            bodies.append(json.loads(request.content))
        return httpx.Response(
            200,
            json={"id": "created", "cdpUrl": "https://cdp.test", "webSocketDebuggerUrl": "ws://cdp.test"},
        )

    cost: list[CostLine] = []
    async with (
        httpx.AsyncClient(transport=httpx.MockTransport(respond)) as http,
        _browser("key", LocalChrome(), http, cost, profile="profile-42") as connection,
    ):
        assert connection.remote
    assert bodies == [{"timeout": 15, "proxyCountryCode": "us", "profileId": "profile-42"}]


async def test_a_cloud_profile_without_a_cloud_browser_is_refused() -> None:
    """Local Chrome keeps its profile in a directory; a cloud profile id means nothing to it."""
    cost: list[CostLine] = []
    async with httpx.AsyncClient() as http:
        with pytest.raises(BrowserError, match="needs a cloud browser"):
            async with _browser(None, LocalChrome(), http, cost, profile="profile-42"):
                pass


async def test_a_browser_handed_over_is_attached_to_and_left_running() -> None:
    """A caller's browser outlives the run: nothing is started for it and nothing is stopped."""
    calls: list[str] = []

    def respond(request: httpx.Request) -> httpx.Response:
        calls.append(f"{request.method} {request.url.path}")
        return httpx.Response(200, json={})

    cost: list[CostLine] = []
    async with (
        httpx.AsyncClient(transport=httpx.MockTransport(respond)) as http,
        _browser(None, LocalChrome(), http, cost, cdp_url="ws://given.test/devtools") as connection,
    ):
        assert connection.cdp_url == "ws://given.test/devtools"
        assert connection.remote
    assert calls == [], "attaching to a browser must not call the cloud API"
    assert cost == []


@pytest.mark.parametrize(
    ("key", "profile", "message"),
    [("key", None, "would start a second one"), (None, "profile-42", "not to one it is handed")],
)
async def test_a_browser_handed_over_refuses_what_belongs_to_one_we_start(
    key: str | None, profile: str | None, message: str
) -> None:
    cost: list[CostLine] = []
    async with httpx.AsyncClient() as http:
        with pytest.raises(BrowserError, match=message):
            async with _browser(key, LocalChrome(), http, cost, profile=profile, cdp_url="ws://given.test/devtools"):
                pass


async def test_a_browser_handed_over_with_no_page_named_still_works_one_out_from_the_task(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The run opens a tab of its own, so an attached browser is never already on the page the task wants."""
    CdpTransport(monkeypatch)
    captured: dict[str, Any] = {}

    async def capture(_self: Agent, task: str, **kwargs: Any) -> RunResult:
        captured.update(kwargs)
        return RunResult(
            status=Status.COMPLETE,
            answer=task,
            data=None,
            evidence=(),
            steps=(),
            cost=CostBreakdown(lines=()),
            artifacts=(),
        )

    monkeypatch.setattr(Agent, "run", capture)
    await run_task(
        "What is the top story on Hacker News?",
        cdp_url="ws://given.test/devtools",
        jev=ScriptedJev({}),
        llm=ScriptedLLM([]),
    )
    assert captured["start"] is None
    assert captured["choose_start"], "with no page named the first address comes from the task"


async def test_a_command_the_browser_never_answers_is_an_outage(monkeypatch: pytest.MonkeyPatch) -> None:
    transport = CdpTransport(monkeypatch)
    monkeypatch.setattr(browser_session, "CDP_REPLY_SECONDS", 0.05)
    monkeypatch.setattr(browser_session, "CDP_ALIVE_SECONDS", 0.05)
    async with BrowserSession(CONNECTION, RecordingArtifactSink()) as session:
        transport.blocked["Page.navigate"] = asyncio.Event()
        transport.blocked["Browser.getVersion"] = asyncio.Event()
        with pytest.raises(browser_session.BrowserUnresponsive) as raised:
            await CdpPage(session, Config()).navigate("https://example.test/")
    assert isinstance(raised.value, Unavailable) and "Page.navigate got no reply" in str(raised.value)


async def test_a_slow_command_from_a_live_browser_is_waited_for(monkeypatch: pytest.MonkeyPatch) -> None:
    transport = CdpTransport(monkeypatch)
    monkeypatch.setattr(browser_session, "CDP_REPLY_SECONDS", 0.05)
    async with BrowserSession(CONNECTION, RecordingArtifactSink()) as session:
        transport.delays["Target.getTargets"] = 0.2
        await session.client.send_raw("Target.getTargets")
    assert transport.calls.count("Browser.getVersion") >= 2, "the browser was asked whether it was there"


async def test_a_reply_that_lands_during_the_liveness_probe_is_kept(monkeypatch: pytest.MonkeyPatch) -> None:
    transport = CdpTransport(monkeypatch)
    monkeypatch.setattr(browser_session, "CDP_REPLY_SECONDS", 0.05)
    monkeypatch.setattr(browser_session, "CDP_ALIVE_SECONDS", 0.1)
    async with BrowserSession(CONNECTION, RecordingArtifactSink()) as session:
        transport.delays["Target.getTargets"] = 0.08
        transport.blocked["Browser.getVersion"] = asyncio.Event()
        await session.client.send_raw("Target.getTargets")
