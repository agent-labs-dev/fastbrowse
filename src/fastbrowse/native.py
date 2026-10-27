"""`run_task` on the Rust core (crates/fastbrowse-py), in this process, when `FASTBROWSE_RUST_CORE=1`.

Every argument crosses. Data goes as JSON. The caller's own objects (its Jev and LLM clients, its HTTP client, its
access check and its secrets) stay here and the core calls them back: each is wrapped below into a function that
takes and answers JSON, so the core sees its own types and the caller's objects see theirs. The result comes back
as the `RunResult` `serve` writes.
"""

import asyncio
import base64
import contextlib
import copy
import importlib
import json
import signal
import sys
import time
from collections.abc import AsyncGenerator, Awaitable, Callable, Iterable
from contextlib import asynccontextmanager
from typing import Any, ClassVar, NoReturn

import httpx
from pydantic import BaseModel, ConfigDict, JsonValue, TypeAdapter, ValidationError
from pydantic_core import InitErrorDetails, PydanticCustomError

from fastbrowse.browser.page import _ScriptError
from fastbrowse.browser.recording import RecordingError
from fastbrowse.browser.session import BrowserUnresponsive, OriginNotAllowed, PageUnresponsive
from fastbrowse.clients.environment import ConfigurationError
from fastbrowse.jev import (
    JevClient,
    JevError,
    JevInputTooLarge,
    JevRetriesExhausted,
    JevTransportFailed,
    Question,
)
from fastbrowse.llm import LLMClient, LLMError, LLMRetriesExhausted, Message
from fastbrowse.models import (
    Artifact,
    ArtifactKind,
    ArtifactSink,
    BrowserEvent,
    BudgetStop,
    CostBreakdown,
    CostComponent,
    CostLine,
    EventHandler,
    FrameHandler,
    Limits,
    LLMPurpose,
    RunResult,
    SecretResolver,
    StepEvent,
    UntilCheck,
)
from fastbrowse.page import (
    Action,
    ActResult,
    BrowserError,
    BrowserUnavailable,
    Capture,
    ContextLost,
    NavigationTimeout,
    Observation,
    ScreenshotsUnavailable,
    SiteUnreachable,
)
from fastbrowse.telemetry import BudgetExceeded, collecting

_EVENT: TypeAdapter[StepEvent | BrowserEvent] = TypeAdapter(StepEvent | BrowserEvent)
_QUESTIONS: TypeAdapter[dict[str, Question]] = TypeAdapter(dict[str, Question])
_JSON: TypeAdapter[JsonValue] = TypeAdapter(JsonValue)

_PLAIN = (
    "start",
    "browser_api_key",
    "cloud_profile",
    "cdp_url",
    "cdp_port",
    "attach",
    "target_match",
    "foreground",
    "proxy_country",
    "cloud_allow_resizing",
    "cloud_timeout_minutes",
    "cursor",
    "fetch",
)
"""`run_task` arguments that are JSON already."""


def _dumped(value: BaseModel | None) -> Any:
    return None if value is None else value.model_dump(mode="json")


def options(arguments: dict[str, Any]) -> str:
    """`run_task`'s data arguments, as the JSON `fastbrowse._native.run_task` reads."""
    output_schema: type[BaseModel] | None = arguments["output_schema"]
    secrets: SecretResolver | None = arguments["secrets"]
    origins, viewport, inputs = arguments["allowed_origins"], arguments["viewport"], arguments["inputs"]
    return json.dumps(
        {
            **{name: arguments[name] for name in _PLAIN},
            "allowed_origins": None if origins is None else list(origins),
            "viewport": None if viewport is None else list(viewport),
            "inputs": None if inputs is None else dict(inputs),
            "chrome": _dumped(arguments["chrome"]),
            "cloud_extensions": list(arguments["cloud_extensions"]),
            "output_schema": None if output_schema is None else output_schema.model_json_schema(),
            "attachments": [
                {"name": a.name, "mime_type": a.mime_type, "content": base64.b64encode(a.content).decode("ascii")}
                for a in arguments["attachments"]
            ],
            "limits": _dumped(arguments["limits"]),
            "authorization": _dumped(arguments["authorization"]),
            "secrets": None if secrets is None else [ref.model_dump(mode="json") for ref in secrets.available()],
            "downloads": None if arguments["downloads"] is None else str(arguments["downloads"]),
            "record": None if arguments["record"] is None else str(arguments["record"]),
            "config": _dumped(arguments["config"]),
        }
    )


class _Raised:
    """The first exception a callback raised. When it stopped the run, `run_task` on Python lets it through, so this
    one does too."""

    def __init__(self) -> None:
        self.first: BaseException | None = None

    def wrap[**P, R](self, call: Callable[P, Awaitable[R]]) -> Callable[P, Awaitable[R]]:
        async def called(*args: P.args, **kwargs: P.kwargs) -> R:
            try:
                return await call(*args, **kwargs)
            except Exception as exc:
                if self.first is None:
                    self.first = exc
                raise

        return called


def _event(payload: str) -> StepEvent | BrowserEvent:
    event = json.loads(payload)
    # JSON carries the step's PNG as base64, which the model's bytes field would read as text.
    if isinstance(event.get("frame"), str):
        event["frame"] = base64.b64decode(event["frame"])
    return _EVENT.validate_python(event)


def _fault(kind: str, exc: BaseException, **details: Any) -> str:
    return json.dumps({"error": {"kind": kind, "message": str(exc), **details}})


def _budget(exc: BudgetExceeded) -> str:
    return _fault("budget", exc, budget=exc.budget.model_dump(mode="json"))


def _jev(client: JevClient) -> Callable[[str, str], Awaitable[str]]:
    async def evaluate(state: str, questions: str) -> str:
        try:
            evaluation = await client.evaluate(json.loads(state), _QUESTIONS.validate_json(questions))
        except BudgetExceeded as exc:
            return _budget(exc)
        except JevInputTooLarge as exc:
            return _fault("input_too_large", exc)
        except JevTransportFailed as exc:
            return _fault("transport_failed", exc)
        except JevRetriesExhausted as exc:
            return _fault(
                "retries_exhausted",
                exc,
                seconds=exc.seconds,
                unaccounted_requests=exc.unaccounted_requests,
                requests=exc.requests,
                answered=[answered.model_dump(mode="json") for answered in exc.answered],
                last=exc.last,
            )
        except JevError as exc:
            return _fault("failed", exc)
        return json.dumps({"ok": evaluation.model_dump(mode="json")})

    return evaluate


class _Ledger:
    """The run's ledger, which the Rust core keeps, for the length of one LLM call."""

    def __init__(self, context: Any) -> None:
        self._context = context

    def _op(self, op: str, **request: Any) -> Any:
        answer = json.loads(self._context.op(json.dumps({"op": op, **request})))
        if isinstance(answer, dict) and "error" in answer:
            raise RuntimeError(answer["error"])
        if isinstance(answer, dict) and "budget" in answer:
            refused = answer["budget"]
            raise BudgetExceeded(refused["reason"], budget=BudgetStop.model_validate(refused["budget"]))
        return answer

    @property
    def limits(self) -> Limits:
        return Limits.model_validate(self._op("limits"))

    @property
    def lines(self) -> list[CostLine]:
        return _Lines(self, (CostLine.model_validate(line) for line in self._op("lines")))

    @property
    def capped(self) -> bool:
        return bool(self._op("capped"))

    def reserve(self, component: CostComponent, estimate_dollars: float = 0.0, calls: int = 1) -> None:
        self._op("reserve", component=CostComponent(component).value, estimate_dollars=estimate_dollars, calls=calls)

    def check(self, extra_dollars: float = 0.0) -> None:
        self._op("check", extra_dollars=extra_dollars)

    def check_spend(self, extra_dollars: float = 0.0) -> None:
        self._op("check_spend", extra_dollars=extra_dollars)

    def record(self, *lines: CostLine) -> None:
        self._op("record", lines=[line.model_dump(mode="json") for line in lines])

    def breakdown(self) -> CostBreakdown:
        return CostBreakdown.model_validate(self._op("breakdown"))


class _Lines(list[CostLine]):
    """`ledger.lines` read from the Rust ledger. Extending it keeps the receipts there, without the check `record`
    makes, as a client that was cancelled keeps its unknown charge without raising a budget failure instead."""

    def __init__(self, ledger: _Ledger, lines: Iterable[CostLine]) -> None:
        super().__init__(lines)
        self._ledger = ledger

    def append(self, line: CostLine) -> None:
        self.extend((line,))

    def extend(self, lines: Iterable[CostLine]) -> None:
        added = list(lines)
        self._ledger._op("append", lines=[line.model_dump(mode="json") for line in added])
        super().extend(added)


def _response_model(name: str, schema: dict[str, Any], context: Any) -> type[BaseModel]:
    """The response model a call asks for, as the class a client expects: its name, its JSON Schema, and validation
    by the Rust core, which raises pydantic's `ValidationError` so a client's repair loop works as before."""

    def validated(text: str) -> dict[str, Any]:
        # Text that is not JSON fails as pydantic fails it, `json_invalid` with where it ended, which is how a client
        # tells a response cut off at its token cap from one that is wrong.
        _JSON.validate_json(text)
        answer = json.loads(context.op(json.dumps({"op": "validate", "text": text})))
        if "error" in answer:
            error = PydanticCustomError("response_invalid", "{reason}", {"reason": answer["error"]})
            raise ValidationError.from_exception_data(name, [InitErrorDetails(type=error, loc=(), input=text)])
        return answer["ok"]

    class Response(BaseModel):
        model_config: ClassVar[ConfigDict] = ConfigDict(extra="allow")

        @classmethod
        def model_json_schema(cls, *args: Any, **kwargs: Any) -> dict[str, Any]:
            return copy.deepcopy(schema)

        @classmethod
        def model_validate_json(cls, json_data: str | bytes | bytearray, *args: Any, **kwargs: Any) -> "Response":
            text = json_data if isinstance(json_data, str) else bytes(json_data).decode()
            return cls.model_construct(**validated(text))

        @classmethod
        def model_validate(cls, obj: Any, *args: Any, **kwargs: Any) -> "Response":
            if isinstance(obj, BaseModel):
                obj = obj.model_dump(mode="json")
            return cls.model_construct(**validated(json.dumps(obj)))

    Response.__name__ = Response.__qualname__ = name
    return Response


def _messages(payload: str) -> list[Message]:
    return [
        Message(
            role=message["role"],
            content=message["content"],
            images=tuple(base64.b64decode(image) for image in message["images"]),
        )
        for message in json.loads(payload)
    ]


def _llm(client: LLMClient) -> Callable[..., Awaitable[str]]:
    async def generate(
        purpose: str, messages: str, name: str, schema: str, max_output_tokens: int, context: Any
    ) -> str:
        ledger: Any = _Ledger(context) if context.ledger else None
        model = _response_model(name, json.loads(schema), context)
        try:
            generation = await client.generate(
                LLMPurpose(purpose), _messages(messages), model, max_output_tokens=max_output_tokens, ledger=ledger
            )
        except BudgetExceeded as exc:
            return _budget(exc)
        except LLMRetriesExhausted as exc:
            return _fault("retries_exhausted", exc)
        except LLMError as exc:
            return _fault("failed", exc)
        finally:
            # A cancelled call waits for this before it lets the ledger go, so a cancelled request's receipt lands.
            with contextlib.suppress(RuntimeError):
                context.op('{"op": "done"}')
        data = generation.data
        return json.dumps(
            {
                "ok": {
                    "data": data.model_dump(mode="json") if isinstance(data, BaseModel) else data,
                    "cost": generation.cost.model_dump(mode="json"),
                }
            }
        )

    return generate


type _Sent = tuple[str, int, list[tuple[str, str]], bytes] | tuple[str, str]


def _http(client: httpx.AsyncClient) -> Callable[..., Awaitable[_Sent]]:
    async def send(method: str, url: str, headers: list[tuple[str, str]], body: bytes, max_bytes: int | None) -> _Sent:
        try:
            # The core times each request itself and cancels this call when it runs out.
            request = client.build_request(method, url, headers=headers, content=body or None, timeout=None)
            # The core follows redirects itself, checking each against the run's allowed origins.
            response = await client.send(request, stream=True, follow_redirects=False)
            try:
                if max_bytes is None:
                    content = await response.aread()
                else:
                    # One byte past the bound shows the core it was passed, without the rest landing in memory.
                    read = bytearray()
                    async for chunk in response.aiter_bytes():
                        read.extend(chunk)
                        if len(read) > max_bytes:
                            break
                    content = bytes(read[: max_bytes + 1])
            finally:
                await response.aclose()
        except httpx.TimeoutException as exc:
            return ("timeout", type(exc).__name__)
        except (httpx.NetworkError, httpx.RemoteProtocolError, httpx.ProxyError, httpx.DecodingError) as exc:
            return ("transient", type(exc).__name__)
        except httpx.HTTPError as exc:
            return ("fatal", type(exc).__name__)
        return ("ok", response.status_code, list(response.headers.multi_items()), content)

    return send


def _check(check_access: Callable[[], Awaitable[None]]) -> Callable[[], Awaitable[str | None]]:
    async def checked() -> str | None:
        try:
            await check_access()
        except Exception as exc:
            # The run names only the exception's type, never its text.
            return type(exc).__name__
        return None

    return checked


async def run(task: str, arguments: dict[str, Any]) -> RunResult:
    """Run `task` with `run_task`'s `arguments` on the Rust core, and return its result as Python's returns one."""
    # The Rust core, built from crates/fastbrowse-py into this package; compiled, so the checker has no types for it.
    core: Any = importlib.import_module("fastbrowse._native")

    raised = _Raised()
    on_event: EventHandler | None = arguments["on_event"]
    on_frame: FrameHandler | None = arguments["on_frame"]
    until: UntilCheck | None = arguments["until"]
    secrets: SecretResolver | None = arguments["secrets"]
    check_access: Callable[[], Awaitable[None]] | None = arguments["check_access"]
    jev: JevClient | None = arguments["jev"]
    llm: LLMClient | None = arguments["llm"]
    http: httpx.AsyncClient | None = arguments["http"]

    async def event(payload: str) -> None:
        if on_event is not None:
            await on_event(_event(payload))

    # A `traced()` block collecting this run's events gets the core's too, as it gets the Python core's.
    events = collecting()
    native = core.run_task(
        task,
        options(arguments),
        on_event=None if on_event is None else raised.wrap(event),
        on_frame=None if on_frame is None else raised.wrap(on_frame),
        until=None if until is None else raised.wrap(until),
        resolve_secret=None if secrets is None else raised.wrap(secrets.resolve),
        check_access=None if check_access is None else _check(check_access),
        jev=None if jev is None else _jev(jev),
        llm=None if llm is None else _llm(llm),
        http=None if http is None else _http(http),
        trace=events is not None,
    )
    try:
        payload = await native
    except core.RunRefused as exc:
        kind, message = exc.args
        if kind == "configuration":
            raise ConfigurationError(message) from None
        if kind == "recording":
            raise RecordingError(message) from None
        if kind == "cancelled":
            raise asyncio.CancelledError(message) from None
        if kind == "callback" and raised.first is not None:
            raise raised.first from None
        raise ValueError(message) from None
    except asyncio.CancelledError:
        # Python's `run_task` closes its browser before the cancellation reaches its caller, and so does this.
        await asyncio.shield(native.finished)
        raise
    finally:
        if events is not None:
            events.extend(_on_this_clock(json.loads(native.trace_events()), time.monotonic() - core.monotonic()))
    return RunResult.model_validate_json(payload)


def _on_this_clock(events: list[dict[str, Any]], offset: float) -> list[dict[str, Any]]:
    """The core's trace events with their times moved onto `time.monotonic()`, which Python's are read on."""
    return [
        event | {name: event[name] + offset for name in ("began", "ended") if isinstance(event.get(name), int | float)}
        for event in events
    ]


_PAGE_ERRORS: dict[str, type[BrowserError]] = {
    error.__name__: error
    for error in (
        BrowserError,
        ContextLost,
        BrowserUnavailable,
        ScreenshotsUnavailable,
        NavigationTimeout,
        SiteUnreachable,
        BrowserUnresponsive,
        PageUnresponsive,
        _ScriptError,
    )
}


def _page_error(failed: Exception) -> BrowserError:
    kind, message = failed.args
    if kind == OriginNotAllowed.__name__:
        return OriginNotAllowed()
    return _PAGE_ERRORS.get(kind, BrowserError)(message)


class NativePage:
    """`CdpPage`'s methods, on a page the Rust core drives."""

    def __init__(self, page: Any, failed: type[Exception]) -> None:
        self._page = page
        self._failed = failed

    async def _call[T](self, awaiting: Awaitable[T]) -> T:
        try:
            return await awaiting
        except self._failed as exc:
            raise _page_error(exc) from None

    async def observe(self) -> Observation:
        return Observation.model_validate_json(await self._call(self._page.observe()))

    async def capture(self) -> Capture:
        return Capture.model_validate_json(await self._call(self._page.capture()))

    async def act(self, action: Action, observation: Observation) -> ActResult:
        acted = self._page.act(action.model_dump_json(), observation.model_dump_json())
        return ActResult.model_validate_json(await self._call(acted))

    async def navigate(self, url: str, load_timeout_seconds: float = 15.0, *, back_to: str | None = None) -> None:
        await self._call(self._page.navigate(url, load_timeout_seconds, back_to))

    async def screenshot(self) -> bytes:
        return await self._call(self._page.screenshot())

    async def origin(self) -> str:
        return await self._call(self._page.origin())

    async def address(self) -> str:
        return await self._call(self._page.address())

    async def document_changed(self, observation: Observation) -> bool:
        return await self._call(self._page.document_changed(observation.model_dump_json()))

    async def response_status(self) -> int | None:
        return await self._call(self._page.response_status())

    @property
    def artifacts(self) -> tuple[Artifact, ...]:
        return tuple(Artifact.model_validate(artifact) for artifact in json.loads(self._page.artifacts()))

    def withhold_frames(self, withheld: bool) -> None:
        self._page.withhold_frames(withheld)


def _sink(sink: ArtifactSink) -> Callable[[str, str, str, bytes], Awaitable[str]]:
    async def put(kind: str, name: str, mime_type: str, content: bytes) -> str:
        return (await sink.put(ArtifactKind(kind), name, mime_type, content)).model_dump_json()

    return put


@asynccontextmanager
async def connect_cdp(arguments: dict[str, Any]) -> AsyncGenerator[NativePage]:
    """`connect_cdp` with its `arguments` on the Rust core."""
    core: Any = importlib.import_module("fastbrowse._native")
    origins, downloads = arguments["allowed_origins"], arguments["downloads"]
    options = {
        **{name: arguments[name] for name in ("port", "cdp_url", "host", "target_match", "attach", "foreground")},
        "allowed_origins": None if origins is None else list(origins),
        "config": _dumped(arguments["config"]),
        "downloads": None if downloads is None else str(downloads),
    }
    check_access, sink = arguments["check_access"], arguments["artifact_sink"]
    try:
        page = await core.connect_cdp(
            json.dumps(options),
            check_access=None if check_access is None else _check(check_access),
            artifact_sink=None if sink is None else _sink(sink),
        )
    except core.PageFailed as exc:
        raise _page_error(exc) from None
    try:
        yield NativePage(page, core.PageFailed)
    finally:
        await page.close()


def command(program: str) -> NoReturn:
    """Run the Rust core's `program` command in this process with this process's arguments, then exit with its code."""
    core: Any = importlib.import_module("fastbrowse._native")
    # Ctrl-C ends the command as it ends the binary; Python's own handler would wait for the core to return first.
    signal.signal(signal.SIGINT, signal.SIG_DFL)
    sys.stdout.flush()
    sys.exit(core.cli_main(program, sys.argv[1:]))
