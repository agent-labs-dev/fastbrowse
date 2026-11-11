"""`fastbrowse.native`: `run_task` on the Rust core in this process.

The runs need `fastbrowse._native`, which `uv sync` builds.
"""

import asyncio
import base64
import gzip
import importlib
import inspect
import json
import os
import subprocess
import sys
import time
from collections.abc import Mapping
from pathlib import Path
from typing import Any

import httpx
import pytest
from pydantic import BaseModel, JsonValue, ValidationError

from fastbrowse import native, run_task
from fastbrowse.config import Config
from fastbrowse.evals.harness import providers, transient_seconds
from fastbrowse.jev import Evaluation, NoulAnswer, Question
from fastbrowse.models import (
    Attachment,
    BrowserEvent,
    BudgetStop,
    ConfigurationError,
    CostBasis,
    CostComponent,
    CostLine,
    Limits,
    LLMPurpose,
    LocalChrome,
    RunCancelled,
    SecretRef,
    Status,
    StepEvent,
)
from fastbrowse.telemetry import BudgetExceeded, traced
from tests.scripted import ScriptedJev, ScriptedLLM, steps


class Release(BaseModel):
    package: str
    version: str


def arguments(**changes: Any) -> dict[str, Any]:
    """`run_task`'s arguments at their defaults, as `locals()` holds them at its first line."""
    defaults = {
        name: parameter.default
        for name, parameter in inspect.signature(run_task).parameters.items()
        if parameter.default is not inspect.Parameter.empty
    }
    return defaults | changes


async def _until(_: str) -> bool:
    return True


class Secrets:
    """A `SecretResolver` holding each value for one origin."""

    def __init__(self, values: Mapping[str, str], origin: str) -> None:
        self._values = values
        self._origin = origin

    def available(self) -> tuple[SecretRef, ...]:
        return tuple(SecretRef(name=name, origins=(self._origin,)) for name in self._values)

    async def resolve(self, name: str, origin: str) -> str | None:
        return self._values.get(name) if origin == self._origin else None


def _ends_mid_json(error: ValidationError) -> bool:
    return any(
        detail["type"] == "json_invalid" and str(detail.get("ctx", {}).get("error", "")).startswith("EOF")
        for detail in error.errors(include_url=False)
    )


def test_arguments_become_the_run_task_the_core_reads() -> None:
    written = native.run_task_json(
        "Find the release",
        arguments(
            start="https://pypi.org/",
            chrome=LocalChrome(headed=True),
            output_schema=Release,
            inputs={"name": "httpx"},
            attachments=(Attachment(name="a.txt", mime_type="text/plain", content=b"hello"),),
            secrets=Secrets({"password": "hunter2"}, "https://shop.example"),
            until=_until,
            downloads=Path("/tmp/downloads"),
            allowed_origins=("https://pypi.org",),
            viewport=(1280, 800),
            config=Config(step_frames=False),
            fetch=False,
        ),
    )
    task = json.loads(written)
    assert (task["task"], task["start"]) == ("Find the release", "https://pypi.org/")
    assert task["chrome"] == {"binary": None, "headed": True, "profile": None}
    assert task["output_schema"] == Release.model_json_schema()
    assert base64.b64decode(task["attachments"][0]["content"]) == b"hello"
    assert (task["allowed_origins"], task["viewport"]) == (["https://pypi.org"], [1280, 800])
    assert task["downloads"] == "/tmp/downloads"
    assert "secrets" not in task and "config" not in task and "hunter2" not in written


def test_every_run_task_argument_crosses() -> None:
    """An argument added to `run_task` and left out of the run and the host would be dropped silently."""
    host = {
        "browser_api_key",
        "secrets",
        "config",
        "settings",
        "on_event",
        "on_frame",
        "until",
        "check_access",
        "open_browser",
        "page_ready",
        "jev",
        "llm",
        "http",
    }
    names = set(inspect.signature(run_task).parameters)
    assert names - host == set(json.loads(native.run_task_json("t", arguments())))


@pytest.fixture
def rust_core(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    # A checkout's `.env` would hand the run a real key.
    monkeypatch.chdir(tmp_path)
    for name in ("OPENROUTER_API_KEY", "AI_GATEWAY_API_KEY", "TYPESAFE_API_KEY", "BROWSER_USE_API_KEY"):
        monkeypatch.delenv(name, raising=False)


@pytest.mark.usefixtures("rust_core")
async def test_a_missing_key_raises_configuration_error() -> None:
    with pytest.raises(ConfigurationError, match="OPENROUTER_API_KEY"):
        await run_task("t", cdp_url="ws://127.0.0.1:9/devtools/browser/x")


class Silent:
    """A DevTools address that accepts the connection and never answers, so a run waits there until stopped."""

    def __init__(self) -> None:
        self.closed = asyncio.Event()
        self.connected = asyncio.Event()

    async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        self.connected.set()
        await reader.read()
        self.closed.set()
        writer.close()


@pytest.mark.usefixtures("rust_core")
async def test_cancelling_the_awaiting_task_stops_the_rust_run_and_closes_its_connection(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("OPENROUTER_API_KEY", "unused")
    silent = Silent()
    server = await asyncio.start_server(silent.handle, "127.0.0.1", 0)
    port = server.sockets[0].getsockname()[1]
    events: list[StepEvent | BrowserEvent] = []

    async def on_event(event: StepEvent | BrowserEvent) -> None:
        events.append(event)

    async with server:
        run = asyncio.create_task(run_task("t", cdp_url=f"ws://127.0.0.1:{port}/devtools/browser/x", on_event=on_event))
        await asyncio.wait_for(silent.connected.wait(), 30)
        run.cancel()
        with pytest.raises(asyncio.CancelledError):
            await run
        await asyncio.wait_for(silent.closed.wait(), 10)
    assert [event.type for event in events] == ["browser"]


@pytest.mark.usefixtures("rust_core")
async def test_an_exception_a_callback_raises_comes_out_of_run_task(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("OPENROUTER_API_KEY", "unused")

    class Refused(Exception):
        pass

    async def on_event(_: StepEvent | BrowserEvent) -> None:
        raise Refused("not this browser")

    with pytest.raises(Refused, match="not this browser"):
        await run_task("t", cdp_url="ws://127.0.0.1:9/devtools/browser/x", on_event=on_event)


@pytest.mark.usefixtures("rust_core")
@pytest.mark.parametrize(("option", "value"), [("viewport", (800, 600)), ("cloud_timeout_minutes", 30)])
async def test_fetch_refuses_an_option_that_shapes_a_browser(option: str, value: object) -> None:
    with pytest.raises(ValueError, match=f"cannot be combined with {option}$"):
        await run_task("t", start="https://example.com", fetch=True, **{option: value})  # ty: ignore[invalid-argument-type]


PAGE = "<html><head><title>Releases</title></head><body><main><h1>httpx</h1>" + "<p>Version 0.28.1 is out.</p>" * 20
PAGE += "</main></body></html>"


class Counted:
    """The scripted model clients and an HTTP client on a fixed page, counting what the run asked each of them."""

    def __init__(self) -> None:
        self.jev = ScriptedJev(steps(""))
        self.llm = ScriptedLLM("https://pypi.example/httpx")
        self.requests: list[str] = []
        self.purposes: list[LLMPurpose] = []
        evaluate, generate = self.jev.evaluate, self.llm.generate
        self.questions = 0

        async def counted_evaluate(state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
            self.questions += len(questions)
            return await evaluate(state, questions)

        async def counted_generate(purpose: LLMPurpose, *args: Any, **kwargs: Any) -> Any:
            self.purposes.append(purpose)
            return await generate(purpose, *args, **kwargs)

        self.jev.evaluate = counted_evaluate  # ty: ignore[invalid-assignment]
        self.llm.generate = counted_generate  # ty: ignore[invalid-assignment]

        def page(request: httpx.Request) -> httpx.Response:
            self.requests.append(str(request.url))
            # Compressed, as a server answers the gzip the reader asks for: httpx decodes it before the core reads it.
            headers = {"content-type": "text/html; charset=utf-8", "content-encoding": "gzip"}
            return httpx.Response(200, content=gzip.compress(PAGE.encode()), headers=headers)

        self.http = httpx.AsyncClient(transport=httpx.MockTransport(page))


@pytest.mark.usefixtures("rust_core")
async def test_a_callback_failure_keeps_the_paid_runs_receipt() -> None:
    counted = Counted()
    evaluate = counted.jev.evaluate

    async def paid(state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        result = await evaluate(state, questions)
        return result.model_copy(
            update={"cost": CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0.01)}
        )

    counted.jev.evaluate = paid  # ty: ignore[invalid-assignment]
    failure = RuntimeError("the caller stopped reading steps")

    async def until(_: str) -> bool:
        raise failure

    async with counted.http:
        with pytest.raises(RuntimeError, match="the caller stopped reading steps") as caught:
            await run_task(
                "Read the release",
                fetch=True,
                jev=counted.jev,
                llm=counted.llm,
                http=counted.http,
                until=until,
                config=Config(step_frames=False),
            )
    assert caught.value is failure
    receipt = getattr(caught.value, "receipt", None)
    assert receipt is not None
    assert receipt.cost.known_dollars > 0


async def test_a_callback_failure_keeps_a_simultaneously_completed_receipt(monkeypatch: pytest.MonkeyPatch) -> None:
    failure = RuntimeError("callback failed at completion")

    class Refused(Exception):
        pass

    class CompletedNative:
        def __init__(self, callback: Any) -> None:
            self.callback = callback
            self.finished = asyncio.Future()
            self.finished.set_result(
                json.dumps(
                    {
                        "steps": [],
                        "cost": {"lines": [{"component": "llm", "basis": "metered", "dollars": 0.01}]},
                        "recordings": ["finished.mp4"],
                    }
                )
            )

        def __await__(self):
            async def refused():
                try:
                    await self.callback("answer")
                except RuntimeError:
                    raise Refused("callback", "") from None

            return refused().__await__()

    class FakeCore:
        RunRefused = Refused

        @staticmethod
        def run_task(*args: Any, **kwargs: Any) -> CompletedNative:
            return CompletedNative(kwargs["until"])

    monkeypatch.setattr(native.importlib, "import_module", lambda _: FakeCore)

    async def until(_: str) -> bool:
        raise failure

    with pytest.raises(RuntimeError) as caught:
        await native.run("task", arguments(until=until))
    assert caught.value is failure
    receipt = getattr(failure, "receipt", None)
    assert receipt is not None
    assert receipt.cost.known_dollars == 0.01
    assert receipt.recordings == (Path("finished.mp4"),)


@pytest.mark.usefixtures("rust_core")
async def test_the_callers_own_clients_serve_the_rust_run() -> None:
    counted = Counted()
    async with counted.http:
        result = await run_task(
            "Read the release",
            fetch=True,
            jev=counted.jev,
            llm=counted.llm,
            http=counted.http,
            config=Config(step_frames=False),
        )
    assert result.status is Status.COMPLETE, result.error
    assert counted.requests == ["https://pypi.example/httpx"]
    assert counted.questions > 0
    assert LLMPurpose.START in counted.purposes
    assert result.cost.known_dollars == 0.0


@pytest.mark.usefixtures("rust_core")
async def test_a_reservation_the_rust_ledger_refuses_ends_the_run_budget_exceeded() -> None:
    counted = Counted()
    generate = counted.llm.generate
    reserved: list[bool] = []

    async def greedy(purpose: LLMPurpose, *args: Any, ledger: Any = None, **kwargs: Any) -> Any:
        # Two calls at once, as a client that sends a pair would reserve them, against a limit of one.
        try:
            ledger.reserve(CostComponent.LLM, calls=2)
        except BudgetExceeded:
            reserved.append(False)
            raise
        return await generate(purpose, *args, ledger=ledger, **kwargs)

    counted.llm.generate = greedy  # ty: ignore[invalid-assignment]
    async with counted.http:
        result = await run_task(
            "Read the release",
            fetch=True,
            jev=counted.jev,
            llm=counted.llm,
            http=counted.http,
            limits=Limits(max_llm_calls=1),
        )
    # The start page and the known pages are asked together, and each reservation is refused.
    assert reserved and not any(reserved)
    assert result.status is Status.BUDGET_EXCEEDED
    assert result.budget == BudgetStop(resource="llm_calls", limit=1)


@pytest.mark.usefixtures("rust_core")
async def test_check_access_refusing_ends_the_run_naming_only_the_exception_type() -> None:
    counted = Counted()

    async def check_access() -> None:
        raise PermissionError("revoked for jane@example.com")

    async with counted.http:
        result = await run_task(
            "Read the release",
            start="https://pypi.example/httpx",
            fetch=True,
            jev=counted.jev,
            llm=counted.llm,
            http=counted.http,
            check_access=check_access,
        )
    assert (result.status, result.error) == (Status.ERROR, "BrowserError")
    assert counted.requests == []


@pytest.mark.usefixtures("rust_core")
async def test_an_llm_client_that_raises_fails_as_that_client_and_the_run_reports_it() -> None:
    counted = Counted()

    async def broken(*_: Any, **__: Any) -> Any:
        raise KeyError("not a model error")

    counted.llm.generate = broken  # ty: ignore[invalid-assignment]
    async with counted.http:
        result = await run_task(
            "Read the release",
            fetch=True,
            jev=counted.jev,
            llm=counted.llm,
            http=counted.http,
        )
    assert (result.status, result.error) == (Status.ERROR, "'not a model error'")


@pytest.mark.usefixtures("rust_core")
async def test_a_redirect_the_callers_client_would_follow_is_checked_against_the_allowed_origins() -> None:
    counted = Counted()
    asked: list[str] = []

    def redirecting(request: httpx.Request) -> httpx.Response:
        asked.append(str(request.url))
        if request.url.host == "pypi.example":
            return httpx.Response(302, headers={"location": "https://evil.example/"})
        return httpx.Response(200, text=PAGE, headers={"content-type": "text/html; charset=utf-8"})

    async with httpx.AsyncClient(transport=httpx.MockTransport(redirecting), follow_redirects=True) as http:
        result = await run_task(
            "Read the release",
            start="https://pypi.example/httpx",
            fetch=True,
            jev=counted.jev,
            llm=counted.llm,
            http=http,
            allowed_origins=["https://pypi.example"],
        )
    assert asked == ["https://pypi.example/httpx"]
    assert result.status is not Status.COMPLETE


UNKNOWN = CostLine(component=CostComponent.LLM, basis=CostBasis.UNKNOWN, purpose=LLMPurpose.START)


@pytest.mark.usefixtures("rust_core")
async def test_receipts_a_client_adds_to_ledger_lines_count_in_the_result() -> None:
    counted = Counted()
    generate = counted.llm.generate

    async def unpriced(purpose: LLMPurpose, *args: Any, ledger: Any = None, **kwargs: Any) -> Any:
        ledger.lines.extend([UNKNOWN])
        return await generate(purpose, *args, ledger=ledger, **kwargs)

    counted.llm.generate = unpriced  # ty: ignore[invalid-assignment]
    async with counted.http:
        result = await run_task(
            "Read the release",
            fetch=True,
            jev=counted.jev,
            llm=counted.llm,
            http=counted.http,
        )
    assert UNKNOWN in result.cost.lines


@pytest.mark.usefixtures("rust_core")
async def test_a_cancelled_llm_call_records_its_receipt_before_run_task_raises() -> None:
    counted = Counted()
    reading_started = asyncio.Event()
    cleanup: list[str] = []

    generate = counted.llm.generate

    async def hanging(purpose: LLMPurpose, messages: Any, schema: type[BaseModel], *, ledger: Any, **_: Any) -> Any:
        if purpose is not LLMPurpose.START:
            return await generate(purpose, messages, schema, ledger=ledger)
        ledger.reserve(CostComponent.LLM)
        reading_started.set()
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError:
            # A request's charge is known only once it is cancelled, which may take a while: the run waits, as
            # The core joins a cancelled client, for as long as it takes.
            await asyncio.sleep(2.1)
            try:
                ledger.lines.extend([UNKNOWN])
                cleanup.append("recorded")
            except RuntimeError as exc:
                cleanup.append(str(exc))
            raise

    counted.llm.generate = hanging  # ty: ignore[invalid-assignment]
    async with counted.http:
        run = asyncio.create_task(
            run_task(
                "Read the version",
                fetch=True,
                jev=counted.jev,
                llm=counted.llm,
                http=counted.http,
            )
        )
        await asyncio.wait_for(reading_started.wait(), 10)
        run.cancel()
        with pytest.raises(RunCancelled) as stopped:
            await run
        assert cleanup == ["recorded"]
    # The charge the dropped call recorded reaches the caller once, and unpriced, never as $0.
    assert stopped.value.receipt.cost.lines.count(UNKNOWN) == 1
    started = [line for line in stopped.value.receipt.cost.lines if line.purpose is LLMPurpose.START]
    assert started and all(line.dollars is None for line in started)


@pytest.mark.usefixtures("rust_core")
@pytest.mark.parametrize("cancel_before_result", [True, False])
async def test_cancellation_before_success_delivery_keeps_the_completed_receipt(
    monkeypatch, cancel_before_result
) -> None:
    counted = Counted()
    loop = asyncio.get_running_loop()
    schedule = loop.call_soon_threadsafe
    intercepted = []
    execution = None

    def cancel_before_delivery(callback, *args, **kwargs):
        # Native run settlement is a builtin closure; host coroutine scheduling uses Python callbacks.
        if inspect.isbuiltin(callback) and not args:

            def deliver():
                assert execution is not None
                intercepted.append(True)
                if cancel_before_result:
                    execution.cancel()
                    loop.call_soon(execution.cancel)
                callback()
                if not cancel_before_result:
                    execution.cancel()

            return schedule(deliver, **kwargs)
        return schedule(callback, *args, **kwargs)

    monkeypatch.setattr(loop, "call_soon_threadsafe", cancel_before_delivery)
    async with counted.http:
        execution = asyncio.create_task(
            run_task("Read the version", fetch=True, jev=counted.jev, llm=counted.llm, http=counted.http)
        )
        with pytest.raises(RunCancelled) as stopped:
            await asyncio.wait_for(execution, 10)
    assert intercepted == [True]
    assert stopped.value.receipt.cost.lines
    assert stopped.value.receipt.cost.known_dollars == 0.0


@pytest.mark.parametrize("cancel_receipt", [False, True])
async def test_repeated_cancellation_drains_a_pending_native_receipt(
    monkeypatch: pytest.MonkeyPatch, cancel_receipt: bool
) -> None:
    started = asyncio.Event()
    receipt = json.dumps(
        {
            "steps": [],
            "cost": {"lines": [{"component": "llm", "basis": "metered", "dollars": 0.01}]},
            "recordings": ["finished.mp4"],
        }
    )

    class PendingNative:
        finished = asyncio.Future()

        def __await__(self):
            async def pending():
                started.set()
                await asyncio.Future()

            return pending().__await__()

        def trace_events(self) -> str:
            return "[]"

    class FakeCore:
        RunRefused = type("RunRefused", (Exception,), {})

        @staticmethod
        def monotonic() -> float:
            return 0.0

        @staticmethod
        def run_task(*args: Any, **kwargs: Any) -> PendingNative:
            return PendingNative()

    monkeypatch.setattr(native.importlib, "import_module", lambda _: FakeCore)
    execution = asyncio.create_task(native.run("task", arguments()))
    await started.wait()
    execution.cancel()
    await asyncio.sleep(0)
    if cancel_receipt:
        PendingNative.finished.cancel()
        with pytest.raises(asyncio.CancelledError):
            await execution
        return
    execution.cancel()
    await asyncio.sleep(0)
    assert not execution.done()
    PendingNative.finished.set_result(receipt)
    with pytest.raises(RunCancelled) as stopped:
        await execution
    assert stopped.value.receipt.cost.known_dollars == 0.01
    assert stopped.value.receipt.recordings == (Path("finished.mp4"),)


@pytest.mark.usefixtures("rust_core")
async def test_a_response_cut_off_mid_json_fails_validation_as_pydantic_reports_it() -> None:
    counted = Counted()
    errors: list[ValidationError] = []
    generate = counted.llm.generate

    async def truncated(purpose: LLMPurpose, messages: Any, schema: type[BaseModel], **kwargs: Any) -> Any:
        try:
            schema.model_validate_json('{"url": [')
        except ValidationError as exc:
            errors.append(exc)
        return await generate(purpose, messages, schema, **kwargs)

    counted.llm.generate = truncated  # ty: ignore[invalid-assignment]
    async with counted.http:
        await run_task(
            "Read the release",
            fetch=True,
            jev=counted.jev,
            llm=counted.llm,
            http=counted.http,
        )
    assert errors
    assert _ends_mid_json(errors[0])


@pytest.mark.usefixtures("rust_core")
async def test_cancelling_a_run_stops_the_model_call_it_began_before_run_task_raises() -> None:
    counted = Counted()
    asking = asyncio.Event()
    stopped: list[str] = []

    generate = counted.llm.generate

    async def hanging(purpose: LLMPurpose, *args: Any, ledger: Any, **kwargs: Any) -> Any:
        if purpose is not LLMPurpose.START:
            return await generate(purpose, *args, ledger=ledger, **kwargs)
        ledger.reserve(CostComponent.LLM)
        asking.set()
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError:
            stopped.append(purpose.value)
            raise

    counted.llm.generate = hanging  # ty: ignore[invalid-assignment]
    async with counted.http:
        run = asyncio.create_task(
            run_task(
                "Read the version",
                fetch=True,
                jev=counted.jev,
                llm=counted.llm,
                http=counted.http,
            )
        )
        await asyncio.wait_for(asking.wait(), 10)
        run.cancel()
        with pytest.raises(asyncio.CancelledError):
            await run
        # The core cancels the call it began and joins it before the cancellation reaches the caller.
        assert stopped == ["start"]


@pytest.mark.parametrize("command", ["fastbrowse", "fastbrowse_mcp"])
def test_the_commands_run_the_rust_cli(command: str) -> None:
    program = command.replace("_", "-")
    script = f"import sys; sys.argv = [{program!r}, '--help']; from fastbrowse import commands; commands.{command}()"
    ran = subprocess.run([sys.executable, "-c", script], capture_output=True, text=True, timeout=30)
    assert ran.returncode == 0, ran.stderr
    assert ran.stdout.startswith(f"usage: {program}")


async def test_a_transient_model_failure_reaches_traced_on_its_clock(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """The fixture runner leaves out time lost to provider retries, read from `traced()`."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("OPENROUTER_API_KEY", "unused")
    asked: list[str] = []

    def handle(request: httpx.Request) -> httpx.Response:
        if request.url.host == "pypi.example":
            return httpx.Response(200, text=PAGE, headers={"content-type": "text/html; charset=utf-8"})
        asked.append(request.url.path)
        if asked.count("/api/v1/chat/completions") == 1:
            return httpx.Response(503, json={"error": "busy"})
        # The known pages are asked beside the start page, each for its own schema.
        known = "urls" in request.content.decode()
        proposal = json.dumps({"urls": []} if known else {"url": "https://pypi.example/httpx"})
        choice = {"index": 0, "message": {"role": "assistant", "content": proposal}, "finish_reason": "stop"}
        usage = {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        return httpx.Response(200, json={"id": "x", "model": "m", "choices": [choice], "usage": usage})

    started = time.monotonic()
    async with httpx.AsyncClient(transport=httpx.MockTransport(handle)) as http:
        with traced() as events:
            result = await run_task(
                "Read the release",
                fetch=True,
                jev=ScriptedJev(steps("")),
                http=http,
            )
    ended = time.monotonic()
    assert result.status is Status.COMPLETE, result.error
    # The first retry waits half a second, and the time lost lies inside the run on this process's clock.
    assert 0.5 <= transient_seconds(events, started, ended) < ended - started


async def test_a_run_names_the_providers_it_calls_in_its_trace(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    """The fixture runner records the route each run took from the run's own trace."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("OPENROUTER_API_KEY", "unused")
    # A gateway key in the environment would add the backup route this run is described without.
    monkeypatch.delenv("AI_GATEWAY_API_KEY", raising=False)
    for name in [
        name for name in os.environ if name.startswith(("FASTBROWSE_LLM", "FASTBROWSE_DECISION", "FASTBROWSE_JEV"))
    ]:
        monkeypatch.delenv(name)
    refused = httpx.MockTransport(lambda _: httpx.Response(401, json={"error": "no"}))
    async with httpx.AsyncClient(transport=refused) as http:
        with traced() as events:
            result = await run_task("Read the release", start="https://pypi.example/httpx", fetch=True, http=http)
        with traced() as own:
            await run_task(
                "Read the release", start="https://pypi.example/httpx", fetch=True, http=http, jev=ScriptedJev([])
            )
    assert result.status is not Status.COMPLETE
    assert providers(events) == (
        "Jev openrouter (no backup: an outage past its retries ends the run); LLM openai/gpt-6-luna via OpenRouter"
    )
    # A caller's own clients are theirs to describe.
    assert providers(own) is None


@pytest.mark.usefixtures("rust_core")
async def test_settings_passed_in_are_read_instead_of_the_environment(monkeypatch: pytest.MonkeyPatch) -> None:
    """A key the process holds reaches no provider when the caller hands the run its own settings."""
    monkeypatch.setenv("OPENROUTER_API_KEY", "from-the-environment")
    with pytest.raises(ConfigurationError, match="OPENROUTER_API_KEY"):
        await run_task("t", start="https://pypi.example/httpx", fetch=True, settings={})
    seen: list[str] = []

    def refuse(request: httpx.Request) -> httpx.Response:
        seen.append(request.headers.get("authorization", ""))
        return httpx.Response(401, json={"error": "no"})

    async with httpx.AsyncClient(transport=httpx.MockTransport(refuse)) as http:
        with traced() as events:
            await run_task(
                "Read the release",
                start="https://pypi.example/httpx",
                fetch=True,
                http=http,
                settings={"AI_GATEWAY_API_KEY": "given"},
            )
    assert "Vercel AI Gateway" in (providers(events) or "")
    assert seen and all("from-the-environment" not in header for header in seen)


def test_the_helper_script_reads_a_pdf_on_the_base_interpreter_without_site() -> None:
    """Run as the package registers it: on the interpreter itself, not a venv's link, without the venv's packages."""
    core: Any = importlib.import_module("fastbrowse._native")
    helper = [os.path.realpath(sys.executable), "-I", "-S", str(Path(core.__file__).with_name("_helper.py"))]
    pdf = Path(__file__).parents[1] / "crates/fastbrowse-browser/tests/fixtures/wind.pdf"
    marker = ["__fastbrowse-helper", "pdf-text", "1"]
    done = subprocess.run([*helper, *marker], input=pdf.read_bytes(), capture_output=True, timeout=60, check=True)
    magic = b"\0fastbrowse-helper-reply\0"
    reply = done.stdout[done.stdout.index(magic) + len(magic) :]
    assert reply[0] == 0
    assert reply[9 : 9 + int.from_bytes(reply[1:9], "little")].decode().strip() == "Fairbanks Wind"


@pytest.mark.usefixtures("rust_core")
async def test_a_missing_browser_key_after_a_paid_fetch_keeps_the_configuration_receipt() -> None:
    class PaidBotCheck(ScriptedJev):
        async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
            evaluation = await super().evaluate(state, questions)
            answers = dict(evaluation.answers)
            if "bot_check" in answers:
                answers["bot_check"] = NoulAnswer(probability=1.0)
            return evaluation.model_copy(
                update={
                    "answers": answers,
                    "cost": CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0.01),
                }
            )

    async with httpx.AsyncClient(
        transport=httpx.MockTransport(
            lambda _: httpx.Response(
                200,
                text="<html><body>Checking access</body></html>",
                headers={"content-type": "text/html"},
            )
        )
    ) as http:
        with pytest.raises(ConfigurationError, match="BROWSER_USE_API_KEY") as caught:
            await run_task(
                "Read the release",
                start="https://docs.example.test/value",
                settings={},
                jev=PaidBotCheck([]),
                llm=ScriptedLLM("https://docs.example.test/value"),
                http=http,
            )
    receipt = caught.value.receipt
    assert receipt is not None
    assert receipt.cost.known_dollars > 0
    assert receipt.steps == ()


def test_an_observation_with_an_open_modal_validates() -> None:
    # Rust writes `modal` only while one is open; the Python model must take what Rust writes.
    from fastbrowse.page import Observation

    page = {
        "url": "https://shop.test/",
        "title": "Shop",
        "page_key": "k",
        "captured_at": 0,
        "viewport_text": "",
        "controls": [],
        "tabs": [],
    }
    assert Observation.model_validate(page).modal is None
    assert Observation.model_validate({**page, "modal": "Stops"}).modal == "Stops"
