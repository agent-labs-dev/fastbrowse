"""Serve runs to another process over JSON-RPC on stdio.

    fastbrowse serve --stdio

The fourth entry point, beside the CLI, the MCP server and `run_task`. It is what the JavaScript SDK starts
and talks to. The messages are in `protocol.py`.
"""

import argparse
import asyncio
import contextlib
import json
import logging
import os
import sys
import threading
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from importlib.metadata import version
from typing import Any, Protocol

from pydantic import BaseModel, ValidationError

from fastbrowse.protocol import (
    PROTOCOL_VERSION,
    Error,
    ErrorCode,
    ErrorResponse,
    InitializeParams,
    InitializeResult,
    Method,
    NoParams,
    Request,
    RequestId,
    Response,
)

log = logging.getLogger(__name__)


class Transport(Protocol):
    """Lines in and lines out. `receive` returns one line, or None once the input has ended."""

    async def receive(self) -> bytes | None: ...

    async def send(self, line: bytes) -> None: ...


class StdioTransport:
    """The process's own stdin and stdout, as lines of any length.

    asyncio's stream reader refuses a line over 64 KiB, and a `run` carrying attachments is longer than that.
    Its pipe support also differs between Windows and everywhere else. A thread doing blocking reads has
    neither problem. It reads the descriptor directly, since a thread parked inside `sys.stdin`'s buffer holds
    a lock the interpreter wants at exit.
    """

    def __init__(self, read_fd: int, write_fd: int) -> None:
        self._read_fd = read_fd
        self._write_fd = write_fd
        self._lines: asyncio.Queue[bytes | None] = asyncio.Queue()
        self._reader: threading.Thread | None = None
        # One message at a time, so two writers cannot interleave the halves of their lines.
        self._writing = asyncio.Lock()

    async def receive(self) -> bytes | None:
        if self._reader is None:
            # A daemon, because after `shutdown` it is still blocked on input that may never come.
            self._reader = threading.Thread(
                target=self._pump, args=(asyncio.get_running_loop(),), name="fastbrowse-serve-stdin", daemon=True
            )
            self._reader.start()
        return await self._lines.get()

    def _pump(self, loop: asyncio.AbstractEventLoop) -> None:
        def deliver(line: bytes | None) -> None:
            # The loop is closed once the server has returned, and what arrives after that has no reader.
            with contextlib.suppress(RuntimeError):
                loop.call_soon_threadsafe(self._lines.put_nowait, line)

        pending = bytearray()
        while chunk := os.read(self._read_fd, 1 << 16):
            pending += chunk
            *lines, rest = bytes(pending).split(b"\n")
            pending = bytearray(rest)
            for line in lines:
                deliver(line)
        if pending:
            deliver(bytes(pending))
        deliver(None)

    async def send(self, line: bytes) -> None:
        async with self._writing:
            await asyncio.to_thread(self._write_all, line)

    def _write_all(self, line: bytes) -> None:
        view = memoryview(line)
        while view:
            view = view[os.write(self._write_fd, view) :]


def stdio_transport() -> StdioTransport:
    """Take stdin and stdout for the protocol, and leave descriptor 1 pointing at stderr.

    Anything else in the process that writes to stdout, a `print` in a dependency or a C extension writing to
    descriptor 1, would otherwise put a line in the stream that is not a message. After this it lands on stderr
    with the logs, and so does the output of a Chrome this process starts.
    """
    sys.stdout.flush()
    write_fd = os.dup(1)
    os.dup2(2, 1)
    sys.stdout = sys.stderr
    if sys.platform == "win32":
        import msvcrt

        # The C runtime's text mode would turn every "\n" written into "\r\n", and end input at a Ctrl-Z byte.
        msvcrt.setmode(0, os.O_BINARY)
        msvcrt.setmode(write_fd, os.O_BINARY)
    return StdioTransport(0, write_fd)


class Refused(Exception):
    """A request that gets an error reply, raised wherever the reason is found."""

    def __init__(self, code: ErrorCode, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


@dataclass(frozen=True)
class _Handler[P: BaseModel]:
    params: type[P]
    call: Callable[[P], Awaitable[BaseModel | None]]


def _problems(exc: ValidationError) -> str:
    """What is wrong with a message, by field. The offending value is left out: a `run` carries attachments."""
    return "; ".join(
        f"{'.'.join(map(str, error['loc'])) or 'params'}: {error['msg']}" for error in exc.errors(include_input=False)
    )


def _id_of(payload: Any) -> RequestId | None:
    """The id to answer a request that could not be read as one, when it at least had a usable id."""
    found = payload.get("id") if isinstance(payload, dict) else None
    return found if isinstance(found, int | str) and not isinstance(found, bool) else None


class Server:
    def __init__(self, transport: Transport) -> None:
        self._transport = transport
        self._closing = False
        self._handlers: dict[Method, _Handler[Any]] = {
            Method.INITIALIZE: _Handler(InitializeParams, self._initialize),
            Method.SHUTDOWN: _Handler(NoParams, self._shutdown),
        }

    async def serve(self) -> int:
        """Answer requests until the client is done, and return the process's exit code.

        The client is done when it says `shutdown` or when the input ends. A parent that was killed says
        nothing, and its end of the pipe closing is the only notice there is.
        """
        while not self._closing and (line := await self._transport.receive()) is not None:
            await self._receive(line)
        return 0

    async def _receive(self, line: bytes) -> None:
        try:
            payload = json.loads(line)
        except ValueError as exc:
            await self._write(ErrorResponse(id=None, error=Error(code=ErrorCode.PARSE_ERROR, message=str(exc))))
            return
        try:
            request = Request.model_validate(payload)
        except ValidationError as exc:
            error = Error(code=ErrorCode.INVALID_REQUEST, message=_problems(exc))
            await self._write(ErrorResponse(id=_id_of(payload), error=error))
            return
        try:
            result = await self._dispatch(request)
        except Refused as exc:
            # Named in the log as well as the reply, since a notification that was refused has no reply.
            log.warning("serve: %s", exc.message)
            if request.id is not None:
                await self._write(ErrorResponse(id=request.id, error=Error(code=exc.code, message=exc.message)))
            return
        if request.id is not None:
            await self._write(Response(id=request.id, result=result))

    async def _dispatch(self, request: Request) -> BaseModel | None:
        try:
            handler = self._handlers[Method(request.method)]
        except ValueError:
            raise Refused(ErrorCode.METHOD_NOT_FOUND, f"unknown method {request.method!r}") from None
        try:
            params = handler.params.model_validate({} if request.params is None else request.params)
        except ValidationError as exc:
            raise Refused(ErrorCode.INVALID_PARAMS, f"{request.method}: {_problems(exc)}") from None
        return await handler.call(params)

    async def _initialize(self, params: InitializeParams) -> InitializeResult:
        return InitializeResult(protocol_version=PROTOCOL_VERSION, fastbrowse_version=version("fastbrowse"))

    async def _shutdown(self, params: NoParams) -> None:
        self._closing = True

    async def _write(self, message: BaseModel) -> None:
        await self._transport.send(message.model_dump_json().encode() + b"\n")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        prog="fastbrowse serve", description="Serve browser runs to another process over JSON-RPC."
    )
    # The only transport there is. It is spelled out so that adding another does not change what this one means.
    parser.add_argument("--stdio", action="store_true", required=True, help="speak on stdin and stdout")
    parser.parse_args(argv)
    transport = stdio_transport()
    logging.basicConfig(stream=sys.stderr, level=logging.WARNING, format="fastbrowse: %(levelname)s %(message)s")
    return asyncio.run(Server(transport).serve())
