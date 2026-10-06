"""Cancelling a run over the serve protocol, as a client sees it.

`run_task` is replaced by a stand-in that holds a browser open until it is cancelled. What is asserted is the
order of the messages the server wrote and whether that browser was closed by then.
"""

import asyncio
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any

from fastbrowse.models import BrowserEvent, CostBreakdown, RunResult, Status
from tests.serve_client import FakeClient, serving
from tests.test_serve_run import Recorder, _run, _settings
from tests.test_serve_secrets import PASSWORD, SHOP, Typist

REPO = Path(__file__).resolve().parent.parent
CANCELLED = -32002


class Browsing:
    """A run that says its browser is open and then works until it is cancelled or told to finish."""

    def __init__(self, *, slow_close: bool = False) -> None:
        self.started: list[str] = []
        self.closed: list[str] = []
        self.finish = asyncio.Event()
        # A browser that takes a while to close, and closes only when the test lets it.
        self.closing = asyncio.Event()
        self.let_close = asyncio.Event()
        if not slow_close:
            self.let_close.set()

    async def __call__(self, task: str, **arguments: Any) -> RunResult:
        self.started.append(task)
        try:
            await arguments["on_event"](BrowserEvent(live_url=None))
            await self.finish.wait()
        finally:
            self.closing.set()
            await self.let_close.wait()
            self.closed.append(task)
        return RunResult(
            status=Status.COMPLETE,
            answer=task,
            data=None,
            evidence=(),
            steps=(),
            cost=CostBreakdown(lines=()),
            artifacts=(),
        )


async def _under_way(client: FakeClient, run_id: str, **fields: Any) -> int:
    """Start a run and wait for its first event, which is the proof that its browser is open."""
    request_id = await client.call("run", _run(run_id, **fields))
    assert (await client.receive())["params"]["run_id"] == run_id
    return request_id


async def test_cancelling_the_active_run_ends_it_with_the_cancelled_error() -> None:
    browsing = Browsing()
    async with serving(runner=browsing, settings=_settings) as client:
        run = await _under_way(client, "run-1")

        cancel = await client.request("run/cancel", {"run_id": "run-1"})
        reply = await client.reply(run)

    assert cancel == {"jsonrpc": "2.0", "id": cancel["id"], "result": None}
    assert reply["error"]["code"] == CANCELLED
    assert "run-1" in reply["error"]["message"]
    assert "result" not in reply
    assert browsing.closed == ["Add the kettle to the cart"]


async def test_the_browser_is_closed_before_the_cancelled_reply_is_written() -> None:
    browsing = Browsing(slow_close=True)
    async with serving(runner=browsing, settings=_settings) as client:
        run = await _under_way(client, "run-1")
        await client.result("run/cancel", {"run_id": "run-1"})
        async with asyncio.timeout(5):
            await browsing.closing.wait()
        # Still closing, and a handshake goes through, so the server is not merely slow to write.
        await client.result("initialize", {"protocol_version": 1})
        assert client.inbox == [] and client.silent

        browsing.let_close.set()
        reply = await client.reply(run)

    assert reply["error"]["code"] == CANCELLED
    assert browsing.closed == ["Add the kettle to the cart"]


async def test_a_cancel_sent_straight_after_the_run_still_cancels_it() -> None:
    browsing = Browsing()
    async with serving(runner=browsing, settings=_settings) as client:
        # Both lines are waiting before the server reads either, as when an AbortSignal fires at once.
        run = await client.call("run", _run("run-1"))
        cancel = await client.call("run/cancel", {"run_id": "run-1"})

        reply = await client.reply(run)

    assert reply["error"]["code"] == CANCELLED
    assert browsing.started == browsing.closed
    assert {"jsonrpc": "2.0", "id": cancel, "result": None} in client.inbox


async def test_cancelling_an_unknown_run_is_answered_and_leaves_the_active_run_alone() -> None:
    browsing = Browsing()
    async with serving(runner=browsing, settings=_settings) as client:
        run = await _under_way(client, "run-1")

        cancel = await client.request("run/cancel", {"run_id": "run-0"})
        assert cancel["result"] is None and "error" not in cancel
        assert browsing.closed == []

        browsing.finish.set()
        reply = await client.reply(run)

    assert reply["result"]["status"] == "complete"


async def test_cancelling_a_run_that_has_finished_is_answered_and_leaves_the_next_run_alone() -> None:
    browsing = Browsing()
    browsing.finish.set()
    async with serving(runner=browsing, settings=_settings) as client:
        await client.result("run", _run("run-1"))
        browsing.finish.clear()
        run = await _under_way(client, "run-2", task="Then check out")

        cancel = await client.request("run/cancel", {"run_id": "run-1"})
        assert cancel["result"] is None and "error" not in cancel

        browsing.finish.set()
        reply = await client.reply(run)

    assert reply["result"]["answer"] == "Then check out"


async def test_cancelling_twice_does_not_interrupt_the_browser_closing() -> None:
    browsing = Browsing(slow_close=True)
    async with serving(runner=browsing, settings=_settings) as client:
        run = await _under_way(client, "run-1")
        await client.result("run/cancel", {"run_id": "run-1"})
        async with asyncio.timeout(5):
            await browsing.closing.wait()

        await client.result("run/cancel", {"run_id": "run-1"})
        assert browsing.closed == []

        browsing.let_close.set()
        reply = await client.reply(run)

    assert reply["error"]["code"] == CANCELLED
    assert browsing.closed == ["Add the kettle to the cart"]


async def test_a_run_after_a_cancelled_one_succeeds() -> None:
    browsing = Browsing()
    async with serving(runner=browsing, settings=_settings) as client:
        cancelled = await _under_way(client, "run-1")
        await client.result("run/cancel", {"run_id": "run-1"})
        assert (await client.reply(cancelled))["error"]["code"] == CANCELLED

        browsing.finish.set()
        result = await client.result("run", _run("run-2", task="Then check out"))

    assert result["status"] == "complete"
    assert browsing.started == ["Add the kettle to the cart", "Then check out"]


async def test_a_cancel_without_a_run_id_is_refused_and_cancels_nothing() -> None:
    hold = asyncio.Event()
    async with serving(runner=Recorder(hold=hold), settings=_settings) as client:
        run = await client.call("run", _run("run-1"))

        refused = await client.request("run/cancel", {})
        assert refused["error"]["code"] == -32602
        assert "run_id" in refused["error"]["message"]

        hold.set()
        reply = await client.reply(run)

    assert reply["result"]["status"] == "complete"


async def test_shutdown_during_a_run_closes_its_browser_answers_it_cancelled_and_exits_zero() -> None:
    browsing = Browsing(slow_close=True)
    async with serving(runner=browsing, settings=_settings) as client:
        run = await _under_way(client, "run-1")

        shutdown = await client.request("shutdown")
        async with asyncio.timeout(5):
            await browsing.closing.wait()
        assert client.silent
        browsing.let_close.set()
        reply = await client.reply(run)

        assert await client.exit_code() == 0

    assert shutdown["result"] is None
    assert reply["error"]["code"] == CANCELLED
    assert browsing.closed == ["Add the kettle to the cart"]


async def test_end_of_input_during_a_run_does_what_shutdown_does() -> None:
    browsing = Browsing(slow_close=True)
    async with serving(runner=browsing, settings=_settings) as client:
        run = await _under_way(client, "run-1")

        await client.close_input()
        async with asyncio.timeout(5):
            await browsing.closing.wait()
        assert client.silent
        browsing.let_close.set()
        reply = await client.reply(run)

        assert await client.exit_code() == 0

    assert reply["error"]["code"] == CANCELLED
    assert browsing.closed == ["Add the kettle to the cart"]


async def test_shutdown_straight_after_the_run_still_cancels_it() -> None:
    browsing = Browsing()
    async with serving(runner=browsing, settings=_settings) as client:
        run = await client.call("run", _run("run-1"))
        await client.call("shutdown")

        reply = await client.reply(run)
        assert await client.exit_code() == 0

    assert reply["error"]["code"] == CANCELLED
    assert browsing.started == browsing.closed


def _command(*arguments: str) -> list[str]:
    return [sys.executable, "-c", "from fastbrowse.cli import main; main()", "serve", *arguments]


def _served(marker: Path) -> subprocess.Popen[bytes]:
    """The command on a run that never finishes, and that writes `marker` when its browser has closed."""
    process = subprocess.Popen(
        _command("--stdio", "--run-task", "tests.serve_scripts:browse_until_cancelled"),
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        env=os.environ | {"PYTHONPATH": str(REPO), "OPENROUTER_API_KEY": "unused", "AI_GATEWAY_API_KEY": "unused"},
    )
    assert process.stdin is not None and process.stdout is not None
    params = {"run_id": "run-1", "task": str(marker), "local": True, "chrome": {"binary": sys.executable}}
    process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 1, "method": "run", "params": params}).encode() + b"\n")
    process.stdin.flush()
    assert json.loads(process.stdout.readline())["method"] == "run/event"
    return process


def test_the_command_exits_zero_with_its_browser_closed_when_its_input_ends_during_a_run(tmp_path: Path) -> None:
    marker = tmp_path / "closed"
    with _served(marker) as process:
        assert process.stdin is not None and process.stdout is not None
        assert not marker.exists()

        process.stdin.close()
        rest = process.stdout.read()
        assert process.wait(timeout=30) == 0

    (reply,) = map(json.loads, rest.splitlines())
    assert (reply["id"], reply["error"]["code"]) == (1, CANCELLED)
    assert marker.read_text() == "closed"


def test_the_command_exits_zero_with_its_browser_closed_on_shutdown_during_a_run(tmp_path: Path) -> None:
    marker = tmp_path / "closed"
    with _served(marker) as process:
        assert process.stdin is not None and process.stdout is not None

        process.stdin.write(b'{"jsonrpc":"2.0","id":2,"method":"shutdown"}\n')
        process.stdin.flush()
        rest = process.stdout.read()
        # The input is still open, as it is when the SDK's `close()` waits for the exit.
        assert process.wait(timeout=30) == 0

    replies = {reply["id"]: reply for reply in map(json.loads, rest.splitlines())}
    assert replies[2]["result"] is None
    assert replies[1]["error"]["code"] == CANCELLED
    assert marker.read_text() == "closed"


async def test_cancelling_a_run_that_is_waiting_on_a_secret_ends_it_and_the_late_value_goes_nowhere() -> None:
    typist = Typist(("SHOP_PASSWORD", SHOP))
    async with serving(runner=typist, settings=_settings) as client:
        run = await _under_way(client, "run-1", secrets=[PASSWORD])
        question = await client.receive()
        assert question["method"] == "secrets/resolve"

        await client.result("run/cancel", {"run_id": "run-1"})
        reply = await client.reply(run)
        await client.send({"jsonrpc": "2.0", "id": question["id"], "result": "hunter2-correct-horse"})
        handshake = await client.result("initialize", {"protocol_version": 1})

    assert reply["error"]["code"] == CANCELLED
    assert typist.resolved == []
    assert handshake["protocol_version"] == 1
    assert client.inbox == []
