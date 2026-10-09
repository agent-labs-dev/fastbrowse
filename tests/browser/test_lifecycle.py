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
from typing import Any, cast
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
    BudgetStop,
    CostBreakdown,
    CostLine,
    LocalChrome,
    RunResult,
    Status,
    StepOutcome,
    Unavailable,
)
from fastbrowse.page import BrowserError, NavigationTimeout, SiteUnreachable
from fastbrowse.run import _browser, connect_cdp, resolve_cdp_port, run_task
from tests.browser.conftest import RecordingArtifactSink
from tests.test_policy import ScriptedJev
from tests.test_retrieval import ScriptedLLM

# What headless local Chrome yields: no window follows the tab, so the run activates it.
CONNECTION = BrowserConnection(cdp_url="ws://localhost:9222", remote=False, foreground=True)
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


BACKGROUND = CONNECTION.model_copy(update={"foreground": False})


@pytest.mark.parametrize("attach", [False, True], ids=["its own tab", "an attached window"])
async def test_a_run_in_a_visible_window_never_activates_its_target(
    monkeypatch: pytest.MonkeyPatch, attach: bool
) -> None:
    """Chrome raises and focuses the whole window of a tab that is activated or opened in front."""
    transport = CdpTransport(monkeypatch)
    transport.results["Target.getTargets"] = [
        {"targetInfos": [{"targetId": "owned", "type": "page", "url": "https://example.com/", "title": "Open"}]}
    ]
    transport.results["Target.getTargetInfo"] = [{"targetInfo": {"url": "https://example.com/pop", "title": "Popup"}}]
    async with BrowserSession(BACKGROUND.model_copy(update={"attach": attach}), RecordingArtifactSink()) as session:
        session._popups["popup"] = ("owned", asyncio.get_running_loop().create_future())
        await session._adopt_popup("popup", "owned")
        await session.switch_tab("popup")
        await session.bring_to_front()
    assert not {"Target.activateTarget", "Page.bringToFront"} & set(transport.calls)
    created = [params for method, params, _ in transport.requests if method == "Target.createTarget"]
    assert created == ([] if attach else [{"url": "about:blank", "background": True}])
    # Both tabs render and take typing where they are instead: focus emulation, and a cast that keeps them painting.
    for method, params in (
        ("Emulation.setFocusEmulationEnabled", {"enabled": True}),
        ("Page.startScreencast", browser_session._KEEP_PAINTING),
    ):
        assert transport.requests.count((method, params, "session")) == 2


async def test_a_run_nobody_sits_in_front_of_activates_its_target(monkeypatch: pytest.MonkeyPatch) -> None:
    transport = CdpTransport(monkeypatch)
    async with BrowserSession(CONNECTION, RecordingArtifactSink()) as session:
        await session.switch_tab("owned")
    assert transport.calls.count("Target.activateTarget") == 2
    assert ("Target.createTarget", {"url": "about:blank"}, None) in transport.requests
    assert not {"Emulation.setFocusEmulationEnabled", "Page.startScreencast"} & set(transport.calls)


@pytest.mark.parametrize(
    ("chrome", "foreground", "expected"),
    [
        (LocalChrome(), False, True),
        (LocalChrome(headed=True), False, False),
        (LocalChrome(headed=True), True, True),
    ],
    ids=["headless", "a visible window", "a visible window asked to the front"],
)
async def test_local_chrome_comes_forward_only_headless_or_when_asked(
    monkeypatch: pytest.MonkeyPatch, chrome: LocalChrome, foreground: bool, expected: bool
) -> None:
    launched: list[list[str]] = []

    class Chrome:
        def __init__(self, command: list[str], **_: object) -> None:
            launched.append(command)

        def terminate(self) -> None:
            pass

        def wait(self, timeout: float | None = None) -> int:
            return 0

    monkeypatch.setattr(chrome_adapter, "find_chrome", lambda _: "chrome")
    monkeypatch.setattr(chrome_adapter.subprocess, "Popen", Chrome)
    monkeypatch.setattr(chrome_adapter, "_wait_for_ws", lambda *_: "ws://127.0.0.1:1/devtools/browser/x")
    async with httpx.AsyncClient() as http, _browser(None, chrome, http, [], foreground=foreground) as connection:
        assert connection.foreground is expected
    assert ("--headless=new" in launched[0]) is not chrome.headed


async def test_a_handed_over_browser_stays_in_the_background_unless_asked() -> None:
    async with httpx.AsyncClient() as http:
        async with _browser(None, LocalChrome(), http, [], cdp_url="ws://x") as connection:
            assert not connection.foreground
        async with _browser(None, LocalChrome(), http, [], cdp_url="ws://x", foreground=True) as connection:
            assert connection.foreground


async def test_session_setup_sends_independent_commands_at_once(monkeypatch: pytest.MonkeyPatch) -> None:
    """Each command is a cloud round trip, and discovery and foregrounding need not wait for the tab or its domains."""
    transport = CdpTransport(monkeypatch)
    transport.delays = {"Target.setDiscoverTargets": 0.3, "Target.activateTarget": 0.3, "Runtime.enable": 0.3}
    began = time.monotonic()
    async with BrowserSession(CONNECTION, RecordingArtifactSink()):
        took = time.monotonic() - began
    # In sequence the three take 0.9s.
    assert took < 0.6


async def test_popup_activation_and_preparation_overlap_and_drain_on_cancellation(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    transport = CdpTransport(monkeypatch)
    async with BrowserSession(CONNECTION, RecordingArtifactSink()) as session:
        activating = transport.blocked["Target.activateTarget"] = asyncio.Event()
        preparing = transport.blocked["Runtime.enable"] = asyncio.Event()
        adopted = asyncio.get_running_loop().create_future()
        session._popups["popup"] = ("owned", adopted)
        task = asyncio.create_task(session._adopt_popup("popup", "owned"))
        try:
            async with asyncio.timeout(2):
                await activating.wait()
                await preparing.wait()
        finally:
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)
        assert not adopted.result()
        assert {"Target.activateTarget", "Runtime.enable"} <= transport.finished


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
        (["net::ERR_TIMED_OUT"] * 3, None),
        (["net::ERR_TUNNEL_CONNECTION_FAILED"] * 4, "Page.navigate failed (net::ERR_TUNNEL_CONNECTION_FAILED)"),
        (["secret https://example.test/secret"] * 4, "Page.navigate failed (NavigationError)"),
        (["net::ERR_NAME_NOT_RESOLVED"] * 4, "Page.navigate failed (net::ERR_NAME_NOT_RESOLVED)"),
    ],
)
async def test_a_failed_navigation_is_tried_again(
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
            # The site or connection dropping it is an outage; a name that never resolved can be a typo.
            assert isinstance(caught.value, SiteUnreachable) is ("TUNNEL" in raised)
    assert transport.calls.count("Page.navigate") == min(len(errors) + 1, page_module._NAVIGATE_ATTEMPTS)


async def test_a_page_that_never_loads_is_tried_again(monkeypatch: pytest.MonkeyPatch) -> None:
    transport = CdpTransport(monkeypatch)
    monkeypatch.setattr(page_module, "_NAVIGATE_RETRY_SECONDS", 0)
    async with BrowserSession(CONNECTION, RecordingArtifactSink()) as session:
        with pytest.raises(NavigationTimeout, match=re.escape("Page.navigate failed (TimeoutError)")):
            await CdpPage(session, Config()).navigate("https://example.test", load_timeout_seconds=0.1)
        transport.results["Runtime.evaluate"] = [LOADED, SETTLED]
        await CdpPage(session, Config()).navigate("https://example.test", load_timeout_seconds=0.1)
    assert transport.calls.count("Page.navigate") == page_module._NAVIGATE_ATTEMPTS + 1


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


async def test_a_failed_history_read_offers_no_back(monkeypatch: pytest.MonkeyPatch) -> None:
    transport = CdpTransport(monkeypatch)
    transport.failures["Page.getNavigationHistory"] = RuntimeError({"code": -32000, "message": "secret"})
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


async def test_back_from_a_page_opened_in_place_of_the_start_page_opens_the_start_page(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A shortcut is loaded instead of the start page, so the tab holds no entry for BACK to return to."""
    transport = CdpTransport(monkeypatch)
    history = _history(["about:blank", "https://example.test/deep"], 1)
    transport.results["Page.getNavigationHistory"] = [history, history]
    opened: list[str] = []
    async with BrowserSession(CONNECTION, RecordingArtifactSink()) as session:
        page = CdpPage(session, Config())
        page._back_to[session.active_session_id] = "https://example.test/"

        async def navigate(url: str, *args: object, **kwargs: object) -> None:
            opened.append(url)

        monkeypatch.setattr(page, "navigate", navigate)
        assert await page._can_go_back()
        assert await page._back() == (StepOutcome.EXECUTED, None)
    assert opened == ["https://example.test/"]
    assert "Page.navigateToHistoryEntry" not in transport.calls


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
    no_shortcut = ScriptedLLM([{"url": None}, {"url": None}])
    result = await run_task("Read", start="https://example.test", jev=ScriptedJev({}), llm=no_shortcut)
    assert result.status is Status.ERROR
    assert result.error == "Page.navigate failed (ConnectionError)"


async def test_a_teardown_error_replaces_the_budget_with_its_status(monkeypatch: pytest.MonkeyPatch) -> None:
    CdpTransport(monkeypatch)
    stopped = RunResult(
        status=Status.BUDGET_EXCEEDED,
        budget=BudgetStop(resource="steps", limit=20),
        answer=None,
        data=None,
        evidence=(),
        steps=(),
        cost=CostBreakdown(),
        artifacts=(),
    )
    monkeypatch.setattr(Agent, "run", AsyncMock(return_value=stopped))
    monkeypatch.setattr(CDPClient, "stop", AsyncMock(side_effect=ConnectionError("secret")))

    @contextmanager
    def chrome(_binary: str | None) -> Generator[BrowserConnection]:
        yield CONNECTION

    monkeypatch.setattr(chrome_adapter, "local_chrome", chrome)
    result = await run_task("Read", start="https://example.test", jev=ScriptedJev({}), llm=ScriptedLLM([{"url": None}]))
    assert result.status is Status.ERROR
    assert result.budget is None


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


EXTENSION_A, EXTENSION_B = "6f1c2d3e-4a5b-4c6d-8e7f-0a1b2c3d4e5f", "11111111-2222-4333-8444-555555555555"


@pytest.mark.parametrize(
    ("extensions", "version"), [((), "v3"), ((EXTENSION_A, EXTENSION_B), "v4")], ids=["none", "some"]
)
async def test_cloud_extensions_pick_the_api_version(extensions: tuple[str, ...], version: str) -> None:
    requests: list[httpx.Request] = []

    def respond(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        return httpx.Response(
            200,
            json={
                "id": "created",
                "cdpUrl": "wss://cdp.test/browser/created" if version == "v4" else "https://cdp.test",
                "webSocketDebuggerUrl": "ws://cdp.test",
            },
        )

    cost: list[CostLine] = []
    async with (
        httpx.AsyncClient(transport=httpx.MockTransport(respond)) as http,
        _browser("key", LocalChrome(), http, cost, cloud_extensions=extensions) as connection,
    ):
        assert connection.cdp_url == ("wss://cdp.test/browser/created" if version == "v4" else "ws://cdp.test")
    cloud = [request for request in requests if request.url.host == "api.browser-use.com"]
    assert {request.url.path.split("/")[2] for request in cloud} == {version}
    body = json.loads(cloud[0].content)
    assert body.get("extensionIds") == (list(extensions) or None)
    assert [request.method for request in cloud] == ["POST", "PATCH"]
    assert [request.method for request in requests] == (
        ["POST", "PATCH"] if version == "v4" else ["POST", "GET", "PATCH"]
    )


@pytest.mark.parametrize(
    "extensions",
    [
        ("not-a-uuid",),
        ("",),
        (EXTENSION_A, EXTENSION_A),
        (EXTENSION_A, EXTENSION_A.upper()),
        (EXTENSION_A, EXTENSION_B, "22222222-2222-4222-8222-222222222222", "33333333-3333-4333-8333-333333333333"),
    ],
)
def test_cloud_extensions_are_validated_before_any_request(extensions: tuple[str, ...]) -> None:
    with pytest.raises(BrowserUseCloudError):
        BrowserUseCloudBrowser("key", http=httpx.AsyncClient(), extensions=extensions)


@pytest.mark.parametrize("browser", [{}, {"cdp_url": "ws://x"}, {"cdp_port": 9222}])
async def test_cloud_extensions_need_a_cloud_browser(browser: dict[str, Any]) -> None:
    async with httpx.AsyncClient() as http:
        with pytest.raises(BrowserError, match="cloud_extensions"):
            async with _browser(None, LocalChrome(), http, [], cloud_extensions=(EXTENSION_A,), **browser):
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


@pytest.mark.parametrize(
    ("kwargs", "expected"),
    [
        ({}, {"timeout": 15, "proxyCountryCode": "us"}),
        ({"allow_resizing": False}, {"timeout": 15, "proxyCountryCode": "us"}),
        ({"allow_resizing": True}, {"timeout": 15, "proxyCountryCode": "us", "allowResizing": True}),
    ],
)
async def test_a_cloud_browser_allows_resizing_only_when_asked(
    kwargs: dict[str, Any], expected: dict[str, Any]
) -> None:
    """The cloud ignores CDP viewport changes unless allowResizing is set, so a matched viewport needs it."""
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
        _browser("key", LocalChrome(), http, cost, **kwargs),
    ):
        pass
    assert bodies == [expected]


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


@pytest.mark.parametrize("failed", [False, True])
async def test_cancelled_cdp_reply_is_drained_without_touching_other_requests(
    caplog: pytest.LogCaptureFixture, failed: bool
) -> None:
    client = browser_session._BrowserClient("ws://localhost:9222")
    sent = asyncio.Event()

    async def send(message: str) -> None:
        sent.set()

    client.ws = Mock(send=send)
    waiting = asyncio.get_running_loop().create_future()
    client.pending_requests[999] = waiting
    request = asyncio.create_task(client.send_raw("Runtime.evaluate", {"expression": "1"}))
    await sent.wait()
    request.cancel()
    with pytest.raises(asyncio.CancelledError):
        await request
    drained = asyncio.Event()
    response = {"id": client.msg_id, "error": {"code": -32000}} if failed else {"id": client.msg_id, "result": {}}
    delivered = False

    async def recv() -> str:
        nonlocal delivered
        if not delivered:
            delivered = True
            return json.dumps(response)
        drained.set()
        await asyncio.Future()
        raise AssertionError("cancelled receiver resumed")

    client.ws.recv = recv
    handler = asyncio.create_task(client._handle_messages())
    await asyncio.wait_for(drained.wait(), timeout=1)
    handler.cancel()
    await asyncio.gather(handler, return_exceptions=True)
    assert client.pending_requests == {999: waiting}
    assert not waiting.done()
    assert "duplicate response" not in caplog.text
    assert "unexpected message" not in caplog.text
    waiting.cancel()


async def test_attach_existing_tab_and_non_destructive_teardown(monkeypatch: pytest.MonkeyPatch) -> None:
    """An attached window is driven where it is and left open on exit."""
    transport = CdpTransport(monkeypatch)
    transport.results["Target.getTargets"] = [
        {
            "targetInfos": [
                {"targetId": "devtools", "type": "page", "url": "devtools://devtools/bundled/x.html", "title": "x"},
                {"targetId": "target_tab_1", "type": "page", "url": "http://example.com/app", "title": "My App"},
            ]
        }
    ]
    conn = BrowserConnection(cdp_url="ws://localhost:9222", remote=False, attach=True)
    async with BrowserSession(conn, RecordingArtifactSink()) as session:
        assert session.active_target_id == "target_tab_1"
        assert [tab.id for tab in session.tabs()] == ["target_tab_1"]
        assert "Target.createTarget" not in transport.calls
        # Discovery replays every open window as created, so it may start only once they are known.
        assert transport.calls.index("Target.getTargets") < transport.calls.index("Target.setDiscoverTargets")
        # The window's document loaded before the session, so its scripts run in it now as well as on navigation.
        assert "Runtime.evaluate" in transport.calls
        assert not session._owned

    assert "Target.closeTarget" not in transport.calls


@pytest.mark.parametrize(("target_match", "adopted"), [(None, ("popup",)), ("App", ("popup", "window"))])
async def test_attach_adopts_its_popups_and_windows_without_opener_only_when_matched(
    monkeypatch: pytest.MonkeyPatch, target_match: str | None, adopted: tuple[str, ...]
) -> None:
    """A window with no opener is an Electron app's, or a tab its user opened by hand in a browser."""
    transport = CdpTransport(monkeypatch)
    transport.results["Target.getTargets"] = [
        {
            "targetInfos": [
                {"targetId": "app", "type": "page", "url": "app://main", "title": "App"},
                {"targetId": "other", "type": "page", "url": "app://other", "title": "Other"},
            ]
        }
    ]
    transport.results["Target.getTargetInfo"] = [
        {"targetInfo": {"url": "app://popup", "title": "Popup"}},
        {"targetInfo": {"url": "app://window", "title": "Window"}},
    ]
    conn = BrowserConnection(cdp_url="ws://localhost:9222", remote=False, attach=True, target_match=target_match)
    async with BrowserSession(conn, RecordingArtifactSink()) as session:
        # Discovery replays "other", which was open before the session and has no opener either.
        for target_id, opener in (("other", None), ("popup", "app"), ("window", None), ("elsewhere", "unrelated")):
            info: dict[str, Any] = {"targetId": target_id, "type": "page", "url": "", "title": ""}
            if opener is not None:
                info["openerId"] = opener
            session._on_target_created(cast(Any, {"targetInfo": info}), None)
        assert session.popups() == adopted
        assert all(session._popups[target_id][0] == "app" for target_id in adopted)
        await asyncio.gather(*session._background)
        assert {tab.id for tab in session.tabs()} == {"app", *adopted}
        assert not session._owned
    assert "Target.closeTarget" not in transport.calls


async def test_attach_target_matching_by_title_or_url(monkeypatch: pytest.MonkeyPatch) -> None:
    transport = CdpTransport(monkeypatch)
    pages = [
        {"targetId": "tab_1", "type": "page", "url": "http://example.com/blank", "title": "Blank Page"},
        {"targetId": "tab_2", "type": "page", "url": "http://example.com/editor", "title": "Main Editor"},
    ]
    transport.results["Target.getTargets"] = [{"targetInfos": pages}, {"targetInfos": pages}]
    conn = BrowserConnection(cdp_url="ws://localhost:9222", remote=False, attach=True, target_match="Editor")
    async with BrowserSession(conn, RecordingArtifactSink()) as session:
        assert session.active_target_id == "tab_2"

    missing = conn.model_copy(update={"target_match": "nonexistent_window"})
    with pytest.raises(browser_session.BrowserError, match="no page matching 'nonexistent_window'"):
        async with BrowserSession(missing, RecordingArtifactSink()):
            pass


async def test_connect_cdp_reads_the_port_and_attaches(monkeypatch: pytest.MonkeyPatch) -> None:
    def version(request: httpx.Request) -> httpx.Response:
        assert str(request.url) == "http://127.0.0.1:9333/json/version"
        return httpx.Response(200, json={"webSocketDebuggerUrl": "ws://127.0.0.1:9333/devtools/browser/abc"})

    async with httpx.AsyncClient(transport=httpx.MockTransport(version)) as http:
        assert await resolve_cdp_port(9333, http=http) == "ws://127.0.0.1:9333/devtools/browser/abc"

    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(200, json={}))) as http:
        with pytest.raises(browser_session.BrowserError, match="named no webSocketDebuggerUrl"):
            await resolve_cdp_port(9333, http=http)

    transport = CdpTransport(monkeypatch)
    transport.results["Target.getTargets"] = [
        {"targetInfos": [{"targetId": "app_win", "type": "page", "url": "app://main", "title": "App Window"}]}
    ]
    async with connect_cdp(cdp_url="ws://127.0.0.1:9333/devtools/browser/abc") as page:
        assert [tab.id for tab in page._session.tabs()] == ["app_win"]
    assert "Target.closeTarget" not in transport.calls
