"""`fastbrowse.native`: `run_task` on the Rust core in this process, when FASTBROWSE_RUST_CORE=1.

The options are checked here without the module. The runs need `fastbrowse_native` built with `run_task`
(`just rust` installs it) and skip without it.
"""

import asyncio
import base64
import importlib
import inspect
import json
from collections.abc import Mapping
from pathlib import Path
from typing import Any, cast

import httpx
import pytest
from pydantic import BaseModel, JsonValue, ValidationError

from fastbrowse import native, run_task
from fastbrowse.clients.environment import ConfigurationError
from fastbrowse.clients.openai_compatible import _ends_mid_json
from fastbrowse.config import Config
from fastbrowse.jev import ChoiceAnswer, ChoiceQuestion, Evaluation, Question
from fastbrowse.llm import Generation
from fastbrowse.models import (
    Attachment,
    BrowserEvent,
    BudgetStop,
    CostBasis,
    CostComponent,
    CostLine,
    Limits,
    LLMPurpose,
    LocalChrome,
    Status,
    StepEvent,
)
from fastbrowse.safety import ScopedSecrets
from fastbrowse.scripted import ScriptedJev, ScriptedLLM, steps
from fastbrowse.telemetry import BudgetExceeded

try:
    fastbrowse_native = importlib.import_module("fastbrowse_native")

    NATIVE = hasattr(fastbrowse_native, "run_task")
except ImportError:
    NATIVE = False
needs_native = pytest.mark.skipif(not NATIVE, reason="fastbrowse_native with run_task is not installed")


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


def test_arguments_become_the_options_the_core_reads() -> None:
    secrets = ScopedSecrets({"password": "hunter2"}, "https://shop.example")
    written = native.options(
        arguments(
            start="https://pypi.org/",
            chrome=LocalChrome(headed=True),
            output_schema=Release,
            inputs={"name": "httpx"},
            attachments=(Attachment(name="a.txt", mime_type="text/plain", content=b"hello"),),
            secrets=secrets,
            until=_until,
            downloads=Path("/tmp/downloads"),
            allowed_origins=("https://pypi.org",),
            viewport=(1280, 800),
            config=Config(max_pages=3),
            fetch=False,
        )
    )
    options = json.loads(written)
    assert options["start"] == "https://pypi.org/"
    assert options["chrome"] == {"binary": None, "headed": True, "profile": None}
    assert options["output_schema"] == Release.model_json_schema()
    assert base64.b64decode(options["attachments"][0]["content"]) == b"hello"
    assert options["secrets"] == [{"name": "password", "origins": ["https://shop.example"]}]
    assert (options["allowed_origins"], options["viewport"]) == (["https://pypi.org"], [1280, 800])
    assert options["config"]["max_pages"] == 3
    assert options["downloads"] == "/tmp/downloads"
    assert "hunter2" not in written


def test_every_run_task_argument_crosses() -> None:
    """An argument added to `run_task` and left out of the options and the callbacks would be dropped silently."""
    callbacks = {"on_event", "on_frame", "until", "check_access", "jev", "llm", "http"}
    names = set(inspect.signature(run_task).parameters) - {"task"}
    assert names - callbacks == set(json.loads(native.options(arguments())))


@pytest.fixture
def rust_core(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setenv("FASTBROWSE_RUST_CORE", "1")
    # A checkout's `.env` would hand the run a real key.
    monkeypatch.chdir(tmp_path)
    for name in ("OPENROUTER_API_KEY", "AI_GATEWAY_API_KEY", "TYPESAFE_API_KEY", "BROWSER_USE_API_KEY"):
        monkeypatch.delenv(name, raising=False)


@needs_native
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


@needs_native
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


@needs_native
@pytest.mark.usefixtures("rust_core")
async def test_an_exception_a_callback_raises_comes_out_of_run_task(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("OPENROUTER_API_KEY", "unused")

    class Refused(Exception):
        pass

    async def on_event(_: StepEvent | BrowserEvent) -> None:
        raise Refused("not this browser")

    with pytest.raises(Refused, match="not this browser"):
        await run_task("t", cdp_url="ws://127.0.0.1:9/devtools/browser/x", on_event=on_event)


PAGE = "<html><head><title>Releases</title></head><body><main><h1>httpx</h1>" + "<p>Version 0.28.1 is out.</p>" * 20
PAGE += "</main></body></html>"


class Counted:
    """The scripted model clients and an HTTP client on a fixed page, counting what the run asked each of them."""

    def __init__(self) -> None:
        self.jev = ScriptedJev(steps(""))
        self.llm = ScriptedLLM()
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
            return httpx.Response(200, text=PAGE, headers={"content-type": "text/html; charset=utf-8"})

        self.http = httpx.AsyncClient(transport=httpx.MockTransport(page))


@needs_native
@pytest.mark.usefixtures("rust_core")
async def test_the_callers_own_clients_serve_the_rust_run() -> None:
    counted = Counted()
    async with counted.http:
        result = await run_task(
            "Read the release",
            start="https://pypi.example/httpx",
            fetch=True,
            jev=counted.jev,
            llm=counted.llm,
            http=counted.http,
            config=Config(max_pages=3),
        )
    assert result.status is Status.COMPLETE, result.error
    assert counted.requests == ["https://pypi.example/httpx"]
    assert counted.questions > 0
    assert LLMPurpose.PLAN in counted.purposes
    assert result.cost.known_dollars == 0.0


@needs_native
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
            start="https://pypi.example/httpx",
            fetch=True,
            jev=counted.jev,
            llm=counted.llm,
            http=counted.http,
            limits=Limits(max_llm_calls=1),
        )
    assert reserved == [False]
    assert result.status is Status.BUDGET_EXCEEDED
    assert result.budget == BudgetStop(resource="llm_calls", limit=1)


@needs_native
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


@needs_native
@pytest.mark.usefixtures("rust_core")
async def test_an_llm_client_that_raises_fails_as_that_client_and_the_run_reports_it() -> None:
    counted = Counted()

    async def broken(*_: Any, **__: Any) -> Any:
        raise KeyError("not a model error")

    counted.llm.generate = broken  # ty: ignore[invalid-assignment]
    async with counted.http:
        result = await run_task(
            "Read the release",
            start="https://pypi.example/httpx",
            fetch=True,
            jev=counted.jev,
            llm=counted.llm,
            http=counted.http,
        )
    assert (result.status, result.error) == (Status.ERROR, "'not a model error'")


@needs_native
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


UNKNOWN = CostLine(component=CostComponent.LLM, basis=CostBasis.UNKNOWN, purpose=LLMPurpose.PLAN)


@needs_native
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
            start="https://pypi.example/httpx",
            fetch=True,
            jev=counted.jev,
            llm=counted.llm,
            http=counted.http,
        )
    assert UNKNOWN in result.cost.lines


def reading(counted: Counted) -> None:
    """Jev holds the page as evidence for the plan's one requirement, so the run asks the LLM to read it, inline."""
    evaluate = counted.jev.evaluate

    async def evidence(state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        evaluation = await evaluate(state, questions)
        picks = {"read_assessment": "evidence", "req_1": "synthesis"}
        answers = dict(evaluation.answers)
        for key, choice in picks.items():
            if key in answers:
                options = list(cast(ChoiceQuestion, questions[key]).criteria)
                probabilities = {option: float(option == choice) for option in options}
                answers[key] = ChoiceAnswer(choice=choice, probabilities=probabilities, confidence=1.0)
        return evaluation.model_copy(update={"answers": answers})

    counted.jev.evaluate = evidence  # ty: ignore[invalid-assignment]


@needs_native
@pytest.mark.usefixtures("rust_core")
async def test_a_cancelled_llm_call_records_its_receipt_before_run_task_raises() -> None:
    counted = Counted()
    reading(counted)
    reading_started = asyncio.Event()
    cleanup: list[str] = []

    async def hanging(purpose: LLMPurpose, messages: Any, schema: type[BaseModel], *, ledger: Any, **_: Any) -> Any:
        if purpose is LLMPurpose.PLAN:
            plan = schema.model_validate({"requirements": [], "answer_expected": True})
            return Generation(data=plan, cost=CostLine(component=CostComponent.LLM, basis=CostBasis.ESTIMATED))
        ledger.reserve(CostComponent.LLM)
        reading_started.set()
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError:
            # A request's charge is known only once it is cancelled, after a moment on the loop.
            await asyncio.sleep(0.05)
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
                start="https://pypi.example/httpx",
                fetch=True,
                jev=counted.jev,
                llm=counted.llm,
                http=counted.http,
            )
        )
        await asyncio.wait_for(reading_started.wait(), 10)
        run.cancel()
        with pytest.raises(asyncio.CancelledError):
            await run
        assert cleanup == ["recorded"]


@needs_native
@pytest.mark.usefixtures("rust_core")
async def test_a_response_cut_off_mid_json_fails_validation_as_pydantic_reports_it() -> None:
    counted = Counted()
    errors: list[ValidationError] = []
    generate = counted.llm.generate

    async def truncated(purpose: LLMPurpose, messages: Any, schema: type[BaseModel], **kwargs: Any) -> Any:
        try:
            schema.model_validate_json('{"requirements": [')
        except ValidationError as exc:
            errors.append(exc)
        return await generate(purpose, messages, schema, **kwargs)

    counted.llm.generate = truncated  # ty: ignore[invalid-assignment]
    async with counted.http:
        await run_task(
            "Read the release",
            start="https://pypi.example/httpx",
            fetch=True,
            jev=counted.jev,
            llm=counted.llm,
            http=counted.http,
        )
    assert errors
    assert _ends_mid_json(errors[0])
