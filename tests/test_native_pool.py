"""A caller's own `httpx.AsyncClient` with a connection limit outlives a run that sent it more requests than that.

The core sends a pass's Jev batches together and drops the rest when the run is cancelled. Cancelling them inside
httpcore 1.0.9's pool stranded connections, so every later request on the client raised PoolTimeout, and the queue
past the limit held the caller's event loop for seconds (#1374). These runs reach a local System One server through
the caller's client, with no mock between the core and the pool.
"""

import asyncio
import json
import time
from collections.abc import Generator
from contextlib import contextmanager
from pathlib import Path

import httpcore
import httpx
import pytest
from pydantic import TypeAdapter

from fastbrowse import run_task
from fastbrowse.config import Config, TokenBudget
from fastbrowse.jev import Question
from fastbrowse.telemetry import traced
from tests.scripted import ScriptedJev, ScriptedLLM, steps

QUESTION: TypeAdapter[Question] = TypeAdapter(Question)
# Every passage of the page names an item, so every question the read asks about one carries this.
PASSAGE = "Item number"


def page(items: int) -> str:
    rows = "".join(f"<p>Item number {n} costs {n * 3} dollars and ships in {n} days.</p>" for n in range(items))
    return f"<html><head><title>Items</title></head><body><main><h1>Items</h1>{rows}</main></body></html>"


class Jev:
    """A System One host that answers as the scripted Jev does, but holds every question about a passage of the page
    until `answer_held` is set, as a provider that has slowed down does."""

    def __init__(self) -> None:
        self.scripted = ScriptedJev(steps(""))
        self.answer_held = asyncio.Event()
        self.held = 0
        self.unanswered = 0
        self.all_answered = asyncio.Event()

    async def serve(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            while head := await reader.readuntil(b"\r\n\r\n"):
                lines = head.decode().split("\r\n")
                length = next(
                    (int(line.split(":")[1]) for line in lines if line.lower().startswith("content-length")), 0
                )
                body = await reader.readexactly(length)
                if lines[0].startswith("GET /ping"):
                    answer = b"{}"
                else:
                    request = json.loads(body)
                    if PASSAGE in json.dumps(request["questions"]):
                        self.held += 1
                        self.unanswered += 1
                        await self.answer_held.wait()
                        self.unanswered -= 1
                        if not self.unanswered:
                            self.all_answered.set()
                    answer = await self.answer(request)
                writer.write(b"HTTP/1.1 200 OK\r\nContent-Length: %d\r\n\r\n%s" % (len(answer), answer))
                await writer.drain()
        except (asyncio.IncompleteReadError, ConnectionError):
            pass
        finally:
            writer.close()

    async def answer(self, request: dict) -> bytes:
        # The wire nests a yes/no question's two descriptions under `criteria`; the Python model holds them flat.
        asked = {
            key: QUESTION.validate_python(
                question | question.pop("criteria", {}) if question["type"] == "noul" else question
            )
            for key, question in request["questions"].items()
        }
        answers = {}
        for key, answer in (await self.scripted.evaluate(request["state"], asked)).answers.items():
            wire = answer.model_dump(mode="json")
            if wire["type"] == "noul":
                wire["noul"] = wire.pop("probability")
            answers[key] = wire
        return json.dumps({"answers": answers, "usage": {"input_tokens": 1}}).encode()


def state(info: str) -> str:
    """The state in httpcore's description of a connection: "CONNECTING" before it has one, else the HTTP/1.1
    connection's, as in "'http://127.0.0.1:80', HTTP/1.1, IDLE, Request Count: 1"."""
    return info.split(", ")[2] if ", " in info else info


@contextmanager
def watched(loop_lag: list[float]) -> Generator[None]:
    """Record in `loop_lag` the longest the event loop went without running a 10 ms timer, beyond its 10 ms."""

    async def watch() -> None:
        while True:
            before = time.perf_counter()
            await asyncio.sleep(0.01)
            loop_lag[0] = max(loop_lag[0], time.perf_counter() - before - 0.01)

    watcher = asyncio.get_running_loop().create_task(watch())
    try:
        yield
    finally:
        watcher.cancel()


async def cancelled_read(connections: int, items: int, tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """Cancel a run once its read of a page of `items` rows holds every connection of the caller's pool, let the
    provider answer what it held, and return how many requests the run dropped, the states the pool's connections are
    left in, the status of a later request on the client, and the event loop's longest stall."""
    # A checkout's `.env` would hand the run a real key.
    monkeypatch.chdir(tmp_path)
    jev = Jev()
    server = await asyncio.start_server(jev.serve, "127.0.0.1", 0)
    port = server.sockets[0].getsockname()[1]
    html = page(items)
    transport = httpx.AsyncHTTPTransport(limits=httpx.Limits(max_connections=connections))
    client = httpx.AsyncClient(
        transport=transport,
        timeout=httpx.Timeout(30, pool=10),
        mounts={"https://shop.example": httpx.MockTransport(lambda _: httpx.Response(200, html=html))},
    )
    loop_lag = [0.0]
    async with server, client:
        with traced() as events, watched(loop_lag):
            run = asyncio.create_task(
                run_task(
                    "List every item's price",
                    start="https://shop.example/items",
                    fetch=True,
                    http=client,
                    llm=ScriptedLLM("https://shop.example/items"),
                    settings={
                        "FASTBROWSE_DECISION_SOURCE": "custom",
                        "FASTBROWSE_DECISION_BASE_URL": f"http://127.0.0.1:{port}",
                        "FASTBROWSE_DECISION_MODEL": "jev-test",
                    },
                    # A request per passage, so the read sends many more batches than the pool has connections.
                    config=Config(tokens=TokenBudget(batch_tokens=1)),
                )
            )
            async with asyncio.timeout(60):
                while jev.held < connections:
                    assert not run.done(), run.result()
                    await asyncio.sleep(0.05)
            # The rest of the pass is sent meanwhile; the run is then cancelled, as a caller does when it loses a lease.
            await asyncio.sleep(1)
            run.cancel()
            with pytest.raises(asyncio.CancelledError):
                await run
            jev.answer_held.set()
            async with asyncio.timeout(10):
                await jev.all_answered.wait()
            # The answers then reach the client, which closes or keeps each connection.
            await asyncio.sleep(0.5)
            dropped = sum(1 for event in events if isinstance(event, dict) and event.get("error") == "cancelled")
            pool = transport._pool
            assert isinstance(pool, httpcore.AsyncConnectionPool)
            states = {state(connection.info()) for connection in pool.connections}
            response = await client.get(f"http://127.0.0.1:{port}/ping", timeout=httpx.Timeout(5, pool=2))
    return dropped, states, response.status_code, loop_lag[0]


async def test_a_cancelled_run_leaves_the_callers_pool_serving(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    dropped, states, status, _ = await cancelled_read(4, 3000, tmp_path, monkeypatch)
    # The run had more requests out than the pool has connections, so some waited for one when it was cancelled.
    assert dropped > 4
    # A connection CONNECTING or NEW with no request, or ACTIVE with nobody reading it, holds its slot for good.
    assert states <= {"IDLE", "CLOSED"}
    assert status == 200


async def test_the_callers_loop_runs_while_a_long_read_waits_for_connections(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    dropped, states, status, loop_lag = await cancelled_read(16, 30000, tmp_path, monkeypatch)
    assert dropped > 16
    assert states <= {"IDLE", "CLOSED"}
    assert status == 200
    # The bridge does a fixed amount of work on the loop per request, a few milliseconds at most here. A second leaves
    # room for a loaded CI machine; the thousand requests this read queued in httpcore held the loop five to seven.
    assert loop_lag < 1.0
