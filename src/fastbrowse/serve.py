"""Serve runs to another process over JSON-RPC on stdio.

    fastbrowse serve --stdio

The fourth entry point, beside the CLI, the MCP server and `run_task`. It is what the JavaScript SDK starts
and talks to. The messages are in `protocol.py`.
"""

import argparse
import asyncio
import contextlib
import importlib
import json
import logging
import os
import sys
import threading
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from importlib.metadata import version
from typing import Any, Protocol

import httpx
from pydantic import BaseModel, ValidationError

from fastbrowse import options
from fastbrowse.adapters.local_chrome import find_chrome
from fastbrowse.clients.environment import ConfigurationError, Settings, load_settings
from fastbrowse.models import Attachment, BrowserEvent, LocalChrome, RunResult, StepEvent
from fastbrowse.protocol import (
    PROTOCOL_VERSION,
    Error,
    ErrorCode,
    ErrorResponse,
    InitializeParams,
    InitializeResult,
    Method,
    NoParams,
    Notification,
    Request,
    RequestId,
    Response,
    RunEvent,
    RunParams,
    ServerMethod,
)
from fastbrowse.run import run_task

log = logging.getLogger(__name__)

type Runner = Callable[..., Awaitable[RunResult]]
"""`run_task`, or what stands in for it."""


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
    background: bool = False
    """Answered from a task of its own, so the server keeps reading while the answer is worked out."""


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
    def __init__(
        self, transport: Transport, *, runner: Runner = run_task, settings: Callable[[], Settings] = load_settings
    ) -> None:
        self._transport = transport
        self._runner = runner
        self._settings = settings
        self._closing = False
        # The id of the run in progress. One at a time: a caller that wants more starts more processes.
        self._active: str | None = None
        self._answering: set[asyncio.Task[None]] = set()
        self._handlers: dict[Method, _Handler[Any]] = {
            Method.INITIALIZE: _Handler(InitializeParams, self._initialize),
            Method.RUN: _Handler(RunParams, self._run, background=True),
            Method.SHUTDOWN: _Handler(NoParams, self._shutdown),
        }

    async def serve(self) -> int:
        """Answer requests until the client is done, and return the process's exit code.

        The client is done when it says `shutdown` or when the input ends. A parent that was killed says
        nothing, and its end of the pipe closing is the only notice there is.
        """
        try:
            while not self._closing and (line := await self._transport.receive()) is not None:
                await self._receive(line)
        finally:
            # A run outlives the loop that started it, and cancelling it is what closes its browser.
            for task in self._answering:
                task.cancel()
            await asyncio.gather(*self._answering, return_exceptions=True)
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
            handler, params = self._handler(request)
        except Refused as exc:
            await self._refuse(request, exc)
            return
        answering = self._answer(request, handler.call(params))
        if handler.background:
            task = asyncio.create_task(answering)
            self._answering.add(task)
            task.add_done_callback(self._answering.discard)
        else:
            await answering

    def _handler(self, request: Request) -> tuple[_Handler[Any], BaseModel]:
        try:
            handler = self._handlers[Method(request.method)]
        except ValueError:
            raise Refused(ErrorCode.METHOD_NOT_FOUND, f"unknown method {request.method!r}") from None
        try:
            return handler, handler.params.model_validate({} if request.params is None else request.params)
        except ValidationError as exc:
            raise Refused(ErrorCode.INVALID_PARAMS, f"{request.method}: {_problems(exc)}") from None

    async def _answer(self, request: Request, call: Awaitable[BaseModel | None]) -> None:
        try:
            result = await call
        except Refused as exc:
            await self._refuse(request, exc)
        except Exception as exc:
            # A fault in a handler is one request's failure. The client is waiting on a reply, and the requests
            # after this one have nothing to do with it.
            log.exception("serve: %s failed", request.method)
            await self._reply(request, Error(code=ErrorCode.INTERNAL_ERROR, message=f"{type(exc).__name__}: {exc}"))
        else:
            if request.id is not None:
                await self._write(Response(id=request.id, result=result))

    async def _refuse(self, request: Request, refusal: Refused) -> None:
        # Named in the log as well as the reply, since a notification that was refused has no reply.
        log.warning("serve: %s", refusal.message)
        await self._reply(request, Error(code=refusal.code, message=refusal.message))

    async def _reply(self, request: Request, error: Error) -> None:
        if request.id is not None:
            await self._write(ErrorResponse(id=request.id, error=error))

    async def _initialize(self, params: InitializeParams) -> InitializeResult:
        return InitializeResult(protocol_version=PROTOCOL_VERSION, fastbrowse_version=version("fastbrowse"))

    async def _shutdown(self, params: NoParams) -> None:
        self._closing = True

    async def _run(self, params: RunParams) -> RunResult:
        if self._active is not None:
            raise Refused(ErrorCode.BUSY, f"run {self._active!r} is still active, and a server runs one at a time")
        self._active = params.run_id

        async def on_event(event: StepEvent | BrowserEvent) -> None:
            # The run waits for this before it goes on, which puts the events on the wire in order and ahead of
            # the reply.
            await self._notify(ServerMethod.RUN_EVENT, RunEvent(run_id=params.run_id, event=event))

        try:
            return await self._runner(params.task, **await self._arguments(params), on_event=on_event)
        except ConfigurationError as exc:
            raise Refused(ErrorCode.CONFIGURATION, str(exc)) from None
        finally:
            self._active = None

    async def _arguments(self, params: RunParams) -> dict[str, Any]:
        """A `run` request as `run_task`'s arguments, or the reason it cannot become a run.

        What the environment can get wrong is found here, before a browser opens: `run_task` reads its model
        keys only when it builds the clients, and a missing Chrome leaves it as a bare `RuntimeError`. The key
        checks hold for a stand-in runner too, so the server refuses the same requests whatever runs them.
        """
        try:
            proxy_country = None if params.proxy_country is None else options.country_code(params.proxy_country)
        except argparse.ArgumentTypeError as exc:
            raise Refused(ErrorCode.INVALID_PARAMS, f"run: proxy_country: {exc}") from None
        options.recording(params.record)
        settings = self._settings()
        asked = params.chrome or LocalChrome()
        chrome = options.chrome(settings, asked.headed, asked.profile, asked.binary)
        handed_over = options.handed_over(
            params.cdp_url,
            params.cdp_port,
            attach=params.attach,
            target_match=params.target_match,
            local=params.local,
            chrome=chrome,
            cloud_profile=params.cloud_profile,
            proxy_country=proxy_country,
        )
        on_cloud = not handed_over and options.cloud(params.local, chrome, params.cloud_profile, proxy_country)
        if not handed_over and not on_cloud and find_chrome(chrome.binary) is None:
            raise ConfigurationError(
                "Chrome was not found: install it, or name it in FASTBROWSE_CHROME or in `chrome.binary`"
            )
        browser_api_key = options.browser_key(settings, on_cloud)
        settings.openrouter_key()
        async with httpx.AsyncClient() as http:
            settings.jev(http)
        return {
            "start": params.start,
            "browser_api_key": browser_api_key,
            "chrome": chrome,
            "cloud_profile": params.cloud_profile,
            "cdp_url": params.cdp_url,
            "cdp_port": params.cdp_port,
            "attach": params.attach,
            "target_match": params.target_match,
            "proxy_country": "us" if proxy_country is None else proxy_country,
            "viewport": params.viewport,
            "cloud_allow_resizing": params.cloud_allow_resizing,
            "inputs": params.inputs,
            "attachments": tuple(
                Attachment(name=attachment.name, mime_type=attachment.mime_type, content=attachment.content)
                for attachment in params.attachments
            ),
            "limits": params.limits,
            "authorization": params.authorization,
            "downloads": params.downloads,
            "record": params.record,
        }

    async def _notify(self, method: ServerMethod, params: BaseModel) -> None:
        await self._write(Notification(method=method, params=params))

    async def _write(self, message: BaseModel) -> None:
        await self._transport.send(message.model_dump_json().encode() + b"\n")


def _callable(name: str) -> Runner:
    """`module:attribute` as the object it names, written the way an entry point in package metadata is."""
    module, _, attribute = name.partition(":")
    try:
        found: Any = importlib.import_module(module)
        for part in attribute.split("."):
            found = getattr(found, part)
    except (ImportError, AttributeError, ValueError) as exc:
        raise argparse.ArgumentTypeError(f"{name!r} names nothing to call: {exc}") from None
    if not callable(found):
        raise argparse.ArgumentTypeError(f"{name!r} is not callable")
    return found


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        prog="fastbrowse serve", description="Serve browser runs to another process over JSON-RPC."
    )
    # The only transport there is. It is spelled out so that adding another does not change what this one means.
    parser.add_argument("--stdio", action="store_true", required=True, help="speak on stdin and stdout")
    # Tests that start this command need a run with no model behind it. It is left out of the help because it is
    # no part of what the command offers, and it gives nothing to someone who already chooses the arguments.
    parser.add_argument("--run-task", type=_callable, default=run_task, help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    transport = stdio_transport()
    logging.basicConfig(stream=sys.stderr, level=logging.WARNING, format="fastbrowse: %(levelname)s %(message)s")
    return asyncio.run(Server(transport, runner=args.run_task).serve())
