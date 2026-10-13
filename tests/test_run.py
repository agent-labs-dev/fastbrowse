"""run_task decisions that do not need a browser: the browser, the agent and the recording are stand-ins."""

from contextlib import asynccontextmanager
from pathlib import Path
from typing import Any, ClassVar

import httpx
import pytest
import websockets
from cdp_use.client import CDPClient
from websockets.asyncio.server import ServerConnection

from fastbrowse import run as run_module
from fastbrowse.agent import Agent
from fastbrowse.browser import session as browser_session
from fastbrowse.models import BrowserConnection, CostBreakdown, RunResult, Status
from tests.test_policy import ScriptedJev
from tests.test_retrieval import ScriptedLLM


class FakeSession:
    def __init__(self, connection: BrowserConnection, *_args: Any, **_kwargs: Any) -> None:
        self.connection = connection

    async def __aenter__(self) -> "FakeSession":
        return self

    async def __aexit__(self, *_exc: object) -> None:
        return None


class FakeRecording:
    cards: ClassVar[list[str]] = []
    outputs: tuple[Path, ...] = ()

    def __init__(self, *_args: Any) -> None:
        pass

    async def __aenter__(self) -> "FakeRecording":
        return self

    async def __aexit__(self, *_exc: object) -> None:
        return None

    async def show_result(self, task: str, _result: RunResult) -> None:
        self.cards.append(task)


@pytest.mark.parametrize(("attach", "cards"), [(True, 0), (False, 1)])
async def test_the_result_card_never_navigates_an_attached_window(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, attach: bool, cards: int
) -> None:
    FakeRecording.cards = []

    @asynccontextmanager
    async def browser(*_args: Any, **_kwargs: Any):
        yield BrowserConnection(cdp_url="ws://localhost:9222", remote=False, attach=attach)

    async def finish(_self: Agent, task: str, **_kwargs: Any) -> RunResult:
        return RunResult(
            status=Status.COMPLETE, answer=task, data=None, evidence=(), steps=(), cost=CostBreakdown(lines=()),
            artifacts=(),
        )  # fmt: skip

    monkeypatch.setattr(run_module, "_browser", browser)
    monkeypatch.setattr(run_module, "BrowserSession", FakeSession)
    monkeypatch.setattr(run_module, "Recording", FakeRecording)
    monkeypatch.setattr(run_module, "CdpPage", lambda *_a, **_k: object())
    monkeypatch.setattr(Agent, "run", finish)
    result = await run_module.run_task(
        "read the page",
        cdp_url="ws://localhost:9222",
        record=tmp_path / "r.webm",
        jev=ScriptedJev({}),
        llm=ScriptedLLM([]),
    )
    assert result.status is Status.COMPLETE
    assert len(FakeRecording.cards) == cards


async def test_a_handshake_timeout_names_the_connection_without_blaming_approval(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def timeout(self: CDPClient) -> None:
        raise TimeoutError

    monkeypatch.setattr(CDPClient, "start", timeout)
    with pytest.raises(browser_session.BrowserUnresponsive) as raised:
        await browser_session._BrowserClient("ws://127.0.0.1:9222/x").start()
    message = str(raised.value)
    assert "TimeoutError" in message and "may be waiting for you to approve remote debugging" in message


async def test_a_browser_that_accepts_the_connection_but_never_answers_is_named_unavailable(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(browser_session, "CDP_ALIVE_SECONDS", 0.05)

    async def silent(ws: ServerConnection) -> None:
        await ws.wait_closed()

    async with websockets.serve(silent, "127.0.0.1", 0) as server:
        port = next(iter(server.sockets)).getsockname()[1]
        result = await run_module.run_task(
            "read the page",
            cdp_url=f"ws://127.0.0.1:{port}/devtools/browser/x",
            attach=True,
            jev=ScriptedJev({}),
            llm=ScriptedLLM([]),
        )
    assert result.status is Status.UNAVAILABLE
    assert result.error is not None
    assert "accepted the DevTools connection but has not answered" in result.error
    assert "approve remote debugging" in result.error


async def test_a_devtools_port_without_its_discovery_route_names_the_websocket_to_pass() -> None:
    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(404))) as http:
        result = await run_module.run_task(
            "read the page", cdp_port=9222, http=http, jev=ScriptedJev({}), llm=ScriptedLLM([])
        )
    assert result.status is Status.ERROR
    assert result.error is not None
    assert "answered 404" in result.error and "DevToolsActivePort" in result.error
    assert "ws://127.0.0.1:<port><path>" in result.error


@pytest.mark.parametrize("status", [401, 403, 500])
async def test_a_devtools_port_refusing_discovery_names_the_status_without_the_chrome_inspect_advice(
    status: int,
) -> None:
    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(status))) as http:
        result = await run_module.run_task(
            "read the page", cdp_port=9222, http=http, jev=ScriptedJev({}), llm=ScriptedLLM([])
        )
    assert result.status is Status.ERROR
    assert result.error is not None
    assert f"answered {status}" in result.error
    assert "chrome://inspect" not in result.error and "DevToolsActivePort" not in result.error
