"""A fake client for `fastbrowse serve`, on in-memory streams.

It speaks to the server the way the JavaScript SDK does, in lines of JSON, so a test asserts what a client on
the other side of the pipe would see and never looks inside the server.
"""

import asyncio
import json
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from typing import Any

from fastbrowse.serve import Server

# A reply that has not come after this long never will, and the suite's own timeout is two minutes away.
PATIENCE = 5.0


class MemoryTransport:
    """Both directions of the pipe as queues of lines. None on the inbound one is end of input."""

    def __init__(self) -> None:
        self.to_server: asyncio.Queue[bytes | None] = asyncio.Queue()
        self.to_client: asyncio.Queue[bytes] = asyncio.Queue()

    async def receive(self) -> bytes | None:
        return await self.to_server.get()

    async def send(self, line: bytes) -> None:
        await self.to_client.put(line)


class FakeClient:
    def __init__(self, transport: MemoryTransport, serving: asyncio.Task[int]) -> None:
        self._transport = transport
        self._serving = serving
        self._ids = 0
        # What the server sent that was not the reply being waited for, in the order it arrived:
        # notifications and the server's own requests.
        self.inbox: list[dict[str, Any]] = []

    async def send_line(self, line: bytes) -> None:
        """One raw line, for what a well-behaved client would never send."""
        await self._transport.to_server.put(line)

    async def send(self, message: dict[str, Any]) -> None:
        await self.send_line(json.dumps(message).encode() + b"\n")

    async def receive_line(self) -> bytes:
        async with asyncio.timeout(PATIENCE):
            return await self._transport.to_client.get()

    async def receive(self) -> dict[str, Any]:
        """The next message from the server, whatever it is."""
        return self.parse(await self.receive_line())

    @staticmethod
    def parse(line: bytes) -> dict[str, Any]:
        assert line.endswith(b"\n") and b"\n" not in line[:-1], f"not one line: {line!r}"
        return json.loads(line)

    async def respond(self, request: dict[str, Any], result: Any) -> None:
        """Answer one of the server's own requests."""
        await self.send({"jsonrpc": "2.0", "id": request["id"], "result": result})

    async def refuse(self, request: dict[str, Any], code: int, message: str) -> None:
        """Answer one of the server's own requests with an error, as a client whose callback threw does."""
        await self.send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": code, "message": message}})

    async def call(self, method: str, params: dict[str, Any] | None = None) -> int:
        """Send a request and return its id without waiting, for a test that has more to say before the reply."""
        self._ids += 1
        message: dict[str, Any] = {"jsonrpc": "2.0", "id": self._ids, "method": method}
        if params is not None:
            message["params"] = params
        await self.send(message)
        return self._ids

    async def reply(self, request_id: int) -> dict[str, Any]:
        """The response to one request. Anything that arrives ahead of it is kept in `inbox`."""
        while True:
            message = await self.receive()
            if "method" not in message and message.get("id") == request_id:
                return message
            self.inbox.append(message)

    async def request(self, method: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
        return await self.reply(await self.call(method, params))

    async def result(self, method: str, params: dict[str, Any] | None = None) -> Any:
        response = await self.request(method, params)
        assert "error" not in response, response
        return response["result"]

    async def close_input(self) -> None:
        """End of input, which is what the server sees when its parent closes the pipe or dies."""
        await self._transport.to_server.put(None)

    async def exit_code(self) -> int:
        """Wait for the server to stop, and return what the process would exit with."""
        async with asyncio.timeout(PATIENCE):
            return await self._serving

    @property
    def silent(self) -> bool:
        """Whether the server has written nothing that is still unread."""
        return self._transport.to_client.empty()


@asynccontextmanager
async def serving(**server_options: Any) -> AsyncIterator[FakeClient]:
    """A running server and a client connected to it. `server_options` go to `Server` as keywords."""
    transport = MemoryTransport()
    task = asyncio.create_task(Server(transport, **server_options).serve())
    try:
        yield FakeClient(transport, task)
    finally:
        if not task.done():
            task.cancel()
        await asyncio.gather(task, return_exceptions=True)
