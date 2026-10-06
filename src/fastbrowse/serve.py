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
from typing import Annotated, Any, Protocol

import httpx
from pydantic import (
    BaseModel,
    StrictInt,
    StrictStr,
    TypeAdapter,
    ValidationError,
    ValidatorFunctionWrapHandler,
    WrapValidator,
    model_validator,
)

from fastbrowse import options
from fastbrowse.adapters.local_chrome import find_chrome
from fastbrowse.clients.environment import ConfigurationError, Settings, load_settings
from fastbrowse.models import (
    Attachment,
    BrowserEvent,
    LocalChrome,
    RunResult,
    SecretRef,
    StepEvent,
)
from fastbrowse.output_schema import UnsupportedSchema, output_model
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
    RunCancelParams,
    RunEvent,
    RunFrame,
    RunParams,
    RunUntilParams,
    SecretsResolveParams,
    ServerMethod,
    ServerRequest,
)
from fastbrowse.run import run_task
from fastbrowse.safety import secret_allowed

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

        # The pieces of a line still arriving. They are joined once, when its end comes: joining per chunk
        # copies a 50 MB `run` a thousand times over.
        pending: list[bytes] = []
        while chunk := os.read(self._read_fd, 1 << 16):
            *ended, rest = chunk.split(b"\n")
            for line in ended:
                deliver(b"".join((*pending, line)))
                pending.clear()
            if rest:
                pending.append(rest)
        if pending:
            deliver(b"".join(pending))
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


class ClientError(Exception):
    """The client failed one of the server's requests: an error reply, or a result of the wrong shape.

    Raised where the answer was awaited, which is inside the run's callback.
    """

    def __init__(self, method: ServerMethod, message: str, code: int | None = None) -> None:
        super().__init__(f"{method}: {message}")
        self.method = method
        self.message = message
        self.code = code
        """The code of the client's error reply. None when the client replied with a result that could not be used."""


type Ask = Callable[[str, str], Awaitable[str | None]]
"""Ask whoever holds a secret for its value, by name and origin."""


class ClientSecrets:
    """A run's `SecretResolver` for values the client holds. Each one is asked for as it is typed, and none is kept.

    The origin rule is applied here, ahead of the question: a client asked about an origin its ref does not
    cover would have to apply the rule a second time to refuse, and the two could disagree.
    """

    def __init__(self, refs: tuple[SecretRef, ...], ask: Ask) -> None:
        self._refs = refs
        self._ask = ask

    def available(self) -> tuple[SecretRef, ...]:
        return self._refs

    async def resolve(self, name: str, origin: str) -> str | None:
        ref = next((ref for ref in self._refs if ref.name == name), None)
        if ref is None or not secret_allowed(ref, origin):
            return None
        return await self._ask(name, origin)


_SECRET_VALUE: TypeAdapter[str | None] = TypeAdapter(str | None)
_UNTIL_ANSWER: TypeAdapter[bool] = TypeAdapter(bool)


def _or_none(value: Any, read: ValidatorFunctionWrapHandler) -> Any:
    try:
        return read(value)
    except ValidationError:
        return None


class _ReplyError(BaseModel):
    """The `error` of a client's reply, read for what it holds.

    `protocol.Error` is what a client should write. A reply that falls short of it still has to end the run
    waiting on it, so a member of the wrong type is read as absent and anything that is not an object as
    an error with no members.
    """

    code: Annotated[StrictInt | None, WrapValidator(_or_none)] = None
    message: Annotated[StrictStr | None, WrapValidator(_or_none)] = None

    @model_validator(mode="before")
    @classmethod
    def _of_any_shape(cls, error: Any) -> Any:
        return error if isinstance(error, dict) else {}


class _Reply(BaseModel):
    """A client's answer to one of the server's requests: a `Response` or an `ErrorResponse`, whichever came."""

    # The server's own ids are integers, so no other id answers anything.
    id: StrictInt
    result: Any = None
    # Some clients write both members and leave the unused one null.
    error: _ReplyError | None = None


@dataclass(frozen=True)
class _Question:
    """One of the server's requests that the client has not answered yet."""

    method: ServerMethod
    answer: asyncio.Future[Any]
    failure: str | None
    """What to say when the client answers with an error, in place of the message it sent."""


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
        # One run at a time: a caller that wants more starts more processes.
        self._active: str | None = None
        self._running: asyncio.Task[None] | None = None
        self._answering: set[asyncio.Task[None]] = set()
        self._last_request_id = 0
        self._pending: dict[int, _Question] = {}
        self._handlers: dict[Method, _Handler[Any]] = {
            Method.INITIALIZE: _Handler(InitializeParams, self._initialize),
            Method.RUN: _Handler(RunParams, self._run, background=True),
            Method.RUN_CANCEL: _Handler(RunCancelParams, self._run_cancel),
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
            # A run outlives the loop that started it. Cancelling it closes its browser, and the process stays
            # until that is done and the run is answered.
            self._cancel_run()
            await asyncio.gather(*self._answering, return_exceptions=True)
        return 0

    async def _receive(self, line: bytes) -> None:
        try:
            payload = json.loads(line)
        except ValueError as exc:
            await self._write(ErrorResponse(id=None, error=Error(code=ErrorCode.PARSE_ERROR, message=str(exc))))
            return
        if isinstance(payload, dict) and "method" not in payload and ("result" in payload or "error" in payload):
            self._settle(payload)
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
            # Started here and not on the loop's next turn, so a `run/cancel` or the end of input on the
            # next line finds the run under way.
            task = asyncio.Task(answering, loop=asyncio.get_running_loop(), eager_start=True)
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
        self._running = asyncio.current_task()

        async def on_event(event: StepEvent | BrowserEvent) -> None:
            # The run waits for this before it goes on, which puts the events on the wire in order and ahead of
            # the reply.
            await self._notify(ServerMethod.RUN_EVENT, RunEvent(run_id=params.run_id, event=event))

        try:
            arguments = await self._arguments(params)
            try:
                return await self._runner(params.task, **arguments, **self._callbacks(params), on_event=on_event)
            except ConfigurationError:
                raise
            except ClientError as exc:
                # `run_task` lets a callback's exception through with nothing attached, so what the run did
                # before then is in the events already sent and not in this.
                return options.error_result(str(exc))
            except Exception as exc:
                # The run was under way, and whatever ends one of those is a result with a status. The reply
                # has room for what went wrong and none for where, so the traceback goes to the log.
                log.exception("serve: run %r failed", params.run_id)
                return options.error_result(f"{type(exc).__name__}: {exc}")
        except ConfigurationError as exc:
            raise Refused(ErrorCode.CONFIGURATION, str(exc)) from None
        except asyncio.CancelledError:
            # The runner has unwound by now, and that is what closed its browser.
            raise Refused(ErrorCode.CANCELLED, f"run {params.run_id!r} was cancelled") from None
        finally:
            self._active = self._running = None

    async def _run_cancel(self, params: RunCancelParams) -> None:
        if params.run_id == self._active:
            self._cancel_run()

    def _cancel_run(self) -> None:
        # Once per run. A second cancellation would land in the runner while it is closing the browser.
        running, self._running = self._running, None
        if running is not None:
            running.cancel()

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
        try:
            output_schema = None if params.output_schema is None else output_model(params.output_schema)
        except UnsupportedSchema as exc:
            raise Refused(ErrorCode.UNSUPPORTED_SCHEMA, f"run: output_schema: {exc}") from None
        options.recording(params.record)
        settings = self._settings()
        local = params.chrome or LocalChrome()
        chrome = options.chrome(settings, local.headed, local.profile, local.binary)
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
            "proxy_country": options.proxy_country(proxy_country),
            "viewport": params.viewport,
            "cloud_allow_resizing": params.cloud_allow_resizing,
            "inputs": params.inputs,
            "attachments": tuple(
                Attachment(name=attachment.name, mime_type=attachment.mime_type, content=attachment.content)
                for attachment in params.attachments
            ),
            "limits": params.limits,
            "authorization": params.authorization,
            "output_schema": output_schema,
            "secrets": self._secrets(params),
            "downloads": params.downloads,
            "record": params.record,
        }

    def _secrets(self, params: RunParams) -> ClientSecrets | None:
        async def ask(name: str, origin: str) -> str | None:
            question = SecretsResolveParams(run_id=params.run_id, name=name, origin=origin)
            # A client's own account of why it has no value may quote the value, and the run's result is read
            # by people and models the value is kept from.
            failure = f"the client could not resolve the secret {name!r}"
            return await self._request(ServerMethod.SECRETS_RESOLVE, question, _SECRET_VALUE, failure=failure)

        return ClientSecrets(params.secrets, ask) if params.secrets else None

    def _callbacks(self, params: RunParams) -> dict[str, Any]:
        """The callbacks a run gets only when its client holds the other end of them."""

        async def on_frame(frame: bytes) -> None:
            await self._notify(ServerMethod.RUN_FRAME, RunFrame(run_id=params.run_id, frame=frame))

        async def until(url: str) -> bool:
            question = RunUntilParams(run_id=params.run_id, url=url)
            return await self._request(ServerMethod.RUN_UNTIL, question, _UNTIL_ANSWER)

        # A run with no frame handler starts no screencast, so a client that reads no frames pays for none.
        return {"on_frame": on_frame if params.frames else None, "until": until if params.until else None}

    async def _notify(self, method: ServerMethod, params: BaseModel) -> None:
        await self._write(Notification(method=method, params=params))

    async def _request[R](
        self, method: ServerMethod, params: BaseModel, result: TypeAdapter[R], *, failure: str | None = None
    ) -> R:
        """Ask the client, and wait for its answer as long as it takes.

        There is no timeout here. What is waiting is a run, and its `max_seconds` already bounds it. An error
        reply raises `ClientError`, and so does a result that is not what `result` describes. The error's text
        is the client's message, or `failure` when the client's message is not to be repeated.
        """
        self._last_request_id += 1
        request_id = self._last_request_id
        question = _Question(method, asyncio.get_running_loop().create_future(), failure)
        self._pending[request_id] = question
        try:
            await self._write(ServerRequest(id=request_id, method=method, params=params))
            answer = await question.answer
        finally:
            del self._pending[request_id]
        try:
            return result.validate_python(answer, strict=True)
        except ValidationError as exc:
            # Named by what was expected and never by what came: the answer may be a secret's value.
            problems = "; ".join(error["msg"] for error in exc.errors(include_input=False))
            raise ClientError(method, f"the result cannot be used: {problems}") from None

    def _settle(self, payload: dict[str, Any]) -> None:
        """Hand a reply from the client to the request that is waiting for it.

        Nothing of the reply is logged, and nothing is written back: its result may be a secret's value, and
        JSON-RPC has no answer to an answer.
        """
        try:
            reply = _Reply.model_validate(payload)
        except ValidationError:
            reply = None
        question = None if reply is None else self._pending.get(reply.id)
        if reply is None or question is None or question.answer.done():
            # A run that was stopped while its question was out leaves the answer with nobody to take it.
            log.debug("serve: a reply to no request")
        elif reply.error is None:
            question.answer.set_result(reply.result)
        else:
            message = question.failure or reply.error.message or "the client replied with an error"
            question.answer.set_exception(ClientError(question.method, message, reply.error.code))

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
