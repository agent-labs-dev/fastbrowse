"""The MCP server's gates and the shape of what it returns, driven through a real MCP client session.

`run_task` is replaced by a recorder, so these check what the server passes to a run and what it makes of the
result, with no browser and no model keys.
"""

import argparse
import asyncio
import sys
from collections.abc import Mapping
from datetime import UTC, date, datetime
from pathlib import Path
from typing import Any

import httpx
import pytest
from mcp.server.fastmcp.exceptions import ToolError
from mcp.shared.memory import create_connected_server_and_client_session
from mcp.types import CallToolResult, LoggingMessageNotificationParams, TextContent
from pydantic import SecretStr

from fastbrowse import mcp_server
from fastbrowse.clients.environment import ConfigurationError, Settings
from fastbrowse.mcp_server import (
    BearerAuth,
    DeclaredSecret,
    OutputField,
    ServerConfig,
    bounded,
    build_server,
    configure,
    is_loopback,
    output_model,
    parse,
)
from fastbrowse.models import (
    Artifact,
    ArtifactKind,
    BrowserEvent,
    CostBasis,
    CostBreakdown,
    CostComponent,
    CostLine,
    Decider,
    EventHandler,
    Evidence,
    Limits,
    Operation,
    RunResult,
    SecretResolver,
    Status,
    StepEvent,
    StepOutcome,
    StepResult,
)

START = "https://shop.example.com/cart"


def _step(index: int) -> StepResult:
    return StepResult(
        index=index,
        operation=Operation.CLICK,
        decided_by=Decider.JEV,
        outcome=StepOutcome.EXECUTED,
        url=START,
        target="Add to cart",
        duration_ms=5,
    )


def _result(status: Status = Status.COMPLETE) -> RunResult:
    return RunResult(
        status=status,
        answer="The backpack costs $29.99.",
        data={"price": 29.99},
        evidence=(
            Evidence(
                source_id="s1",
                url=START,
                frame_id=None,
                captured_at=datetime(2026, 9, 19, tzinfo=UTC),
                capture_sha256="0" * 64,
                start=0,
                end=6,
                quote="$29.99",
            ),
        ),
        steps=(_step(0), _step(1)),
        cost=CostBreakdown(
            lines=(
                CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0.004),
                CostLine(component=CostComponent.BROWSER, basis=CostBasis.UNKNOWN),
            )
        ),
        artifacts=(
            Artifact(
                kind=ArtifactKind.DOWNLOAD,
                name="receipt.pdf",
                mime_type="application/pdf",
                size_bytes=10,
                sha256="1" * 64,
                uri="file:///downloads/receipt.pdf",
            ),
        ),
        final_url=START,
    )


class Recorder:
    """Stands in for `run_task`: keeps what it was called with and plays the events a run would send."""

    def __init__(self, result: RunResult | None = None) -> None:
        self.result = result or _result()
        self.calls: list[dict[str, Any]] = []

    async def __call__(self, task: str, **kwargs: Any) -> RunResult:
        self.calls.append({"task": task, **kwargs})
        on_event: EventHandler = kwargs["on_event"]
        await on_event(BrowserEvent(live_url="https://live.example.com/session"))
        for step in self.result.steps:
            await on_event(StepEvent(step=step))
        return self.result

    @property
    def last(self) -> dict[str, Any]:
        return self.calls[-1]


def _text(result: CallToolResult) -> str:
    content = result.content[0]
    assert isinstance(content, TextContent)
    return content.text


async def _call(
    config: ServerConfig, recorder: Recorder, arguments: Mapping[str, Any]
) -> tuple[CallToolResult, list[str]]:
    server = build_server(config, runner=recorder)
    reported: list[str] = []
    logs: list[str] = []

    async def on_progress(progress: float, total: float | None, message: str | None) -> None:
        reported.append(f"{progress:g}/{total:g} {message}")

    async def on_log(params: LoggingMessageNotificationParams) -> None:
        logs.append(str(params.data))

    async with create_connected_server_and_client_session(server, logging_callback=on_log) as client:
        result = await client.call_tool("browse", dict(arguments), progress_callback=on_progress)
    return result, reported + logs


async def test_a_run_returns_structured_output_with_citations_and_reports_each_step() -> None:
    recorder = Recorder()
    config = ServerConfig(downloads=Path("downloads"))
    result, notices = await _call(config, recorder, {"task": "What does the backpack cost?", "start": START})
    assert not result.isError
    structured = result.structuredContent
    assert structured is not None
    assert structured["status"] == "complete"
    assert structured["citations"] == [{"quote": "$29.99", "url": START}]
    assert structured["steps"] == 2
    assert structured["dollars"] == 0.004
    assert structured["cost_complete"] is False
    assert structured["live_url"] == "https://live.example.com/session"
    assert structured["downloads"][0]["name"] == "receipt.pdf"
    assert structured["next_step"] is None
    assert isinstance(result.content[0], TextContent)
    assert "Add to cart" in " ".join(notices)
    assert "1/60 click Add to cart -> executed" in notices
    assert any("watch live" in notice for notice in notices)


async def test_downloads_are_not_listed_when_the_server_keeps_none() -> None:
    result, _ = await _call(ServerConfig(), Recorder(), {"task": "t", "start": START})
    assert result.structuredContent is not None
    assert result.structuredContent["downloads"] == []


async def test_authorize_is_refused_unless_the_operator_allowed_it() -> None:
    recorder = Recorder()
    result, _ = await _call(ServerConfig(), recorder, {"task": "Buy it", "start": START, "authorize": True})
    assert result.isError
    assert "--allow-authorize" in _text(result)
    assert recorder.calls == []

    await _call(ServerConfig(allow_authorize=True), recorder, {"task": "Buy it", "start": START, "authorize": True})
    assert recorder.last["authorization"].irreversible_actions is True


async def test_needs_confirmation_says_what_the_caller_can_do() -> None:
    recorder = Recorder(_result(Status.NEEDS_CONFIRMATION))
    result, _ = await _call(ServerConfig(), recorder, {"task": "Buy it", "start": START})
    assert result.structuredContent is not None
    assert "--allow-authorize" in result.structuredContent["next_step"]


async def test_a_call_cannot_raise_the_servers_limits() -> None:
    recorder = Recorder()
    ceilings = Limits(max_steps=20, max_dollars=0.5, max_seconds=120)
    config = ServerConfig(ceilings=ceilings)
    arguments = {"task": "t", "start": START, "max_steps": 500, "max_dollars": 9.0, "max_seconds": 10}
    await _call(config, recorder, arguments)
    limits: Limits = recorder.last["limits"]
    assert (limits.max_steps, limits.max_dollars, limits.max_seconds) == (20, 0.5, 10)


def test_limits_left_unset_fall_back_to_the_ceilings() -> None:
    ceilings = Limits(max_steps=20, max_dollars=None, max_seconds=120)
    limits = bounded(ceilings, None, 0.2, None)
    assert (limits.max_steps, limits.max_dollars, limits.max_seconds) == (20, 0.2, 120)


async def test_secrets_reach_a_run_only_on_their_own_origin() -> None:
    secret = DeclaredSecret(name="password", value="hunter2", origin="https://shop.example.com")
    config = ServerConfig(secrets=(secret,))
    recorder = Recorder()

    await _call(config, recorder, {"task": "Sign in", "start": START})
    resolver: SecretResolver = recorder.last["secrets"]
    assert [ref.name for ref in resolver.available()] == ["password"]
    assert await resolver.resolve("password", "https://shop.example.com") == "hunter2"
    assert await resolver.resolve("password", "https://evil.example.com") is None

    await _call(config, recorder, {"task": "Sign in", "start": "https://other.example.com/"})
    assert recorder.last["secrets"] is None


async def test_secret_values_never_appear_in_the_tool_listing() -> None:
    secret = DeclaredSecret(name="password", value="hunter2", origin="https://shop.example.com")
    server = build_server(ServerConfig(secrets=(secret,)), runner=Recorder())
    async with create_connected_server_and_client_session(server) as client:
        tools = (await client.list_tools()).tools
    listing = tools[0].model_dump_json()
    assert "password on https://shop.example.com" in listing
    assert "hunter2" not in listing


async def test_a_bitwarden_item_must_be_one_the_server_offers() -> None:
    recorder = Recorder()
    result, _ = await _call(
        ServerConfig(bitwarden=("Shop",)), recorder, {"task": "t", "start": START, "bitwarden": "Bank"}
    )
    assert result.isError
    assert "not an item this server offers" in _text(result)
    assert recorder.calls == []


@pytest.mark.parametrize("start", ["file:///etc/passwd", "javascript:alert(1)", "shop.example.com", "https://"])
async def test_start_must_be_a_web_url(start: str) -> None:
    recorder = Recorder()
    result, _ = await _call(ServerConfig(), recorder, {"task": "t", "start": start})
    assert result.isError
    assert recorder.calls == []


async def test_fields_become_the_runs_output_schema() -> None:
    recorder = Recorder()
    fields = {"price": {"type": "number", "description": "In USD"}, "listed": {"type": "date"}}
    await _call(ServerConfig(), recorder, {"task": "t", "start": START, "fields": fields})
    schema = recorder.last["output_schema"]
    assert schema.model_fields["price"].annotation is float
    assert schema.model_fields["price"].description == "In USD"
    assert schema.model_fields["listed"].annotation is date


@pytest.mark.parametrize("name", ["_private", "model_config", "class", "two words", "json"])
def test_field_names_that_cannot_be_a_model_field_are_refused(name: str) -> None:
    with pytest.raises(ToolError):
        output_model({name: OutputField(type="string")})


def test_too_many_fields_are_refused() -> None:
    with pytest.raises(ToolError):
        output_model({f"f{i}": OutputField(type="string") for i in range(21)})


async def test_a_configuration_error_in_a_run_is_a_tool_error() -> None:
    async def missing_key(task: str, **_: Any) -> RunResult:
        raise ConfigurationError("set OPENROUTER_API_KEY for the LLM")

    server = build_server(ServerConfig(), runner=missing_key)
    async with create_connected_server_and_client_session(server) as client:
        result = await client.call_tool("browse", {"task": "t", "start": START})
    assert result.isError
    assert "OPENROUTER_API_KEY" in _text(result)


def _settings(**overrides: Any) -> Settings:
    keys: dict[str, Any] = {
        "openrouter_api_key": SecretStr("or"),
        "ai_gateway_api_key": SecretStr("gw"),
        "typesafe_api_key": None,
        "browser_use_api_key": None,
        "profile": None,
        "headed": False,
        # Any binary that exists stands in for Chrome, which configure only looks for.
        "chrome": sys.executable,
    }
    return Settings.model_construct(**(keys | overrides))


async def test_configure_reads_secrets_and_ceilings_from_flags() -> None:
    args = parse(
        ["--secret", "password=SHOP_PASSWORD@https://Shop.example.com", "--max-dollars", "0.25", "--allow-authorize"]
    )
    config = await configure(args, _settings(), {"SHOP_PASSWORD": "hunter2"})
    assert config.secrets == (DeclaredSecret("password", "hunter2", "https://shop.example.com"),)
    assert config.ceilings.max_dollars == 0.25
    assert config.allow_authorize


@pytest.mark.parametrize(
    ("argv", "environ", "settings", "message"),
    [
        (["--secret", "password=UNSET@https://a.example"], {}, {}, "unset variables: UNSET"),
        (["--profile", "p", "--max-concurrent", "2"], {}, {}, "one Chrome at a time"),
        (["--cloud", "--headed"], {}, {}, "not --cloud"),
        (["--cloud"], {}, {}, "BROWSER_USE_API_KEY"),
        ([], {}, {"chrome": "no-such-chrome"}, "Chrome was not found"),
        ([], {}, {"openrouter_api_key": None}, "OPENROUTER_API_KEY"),
        ([], {}, {"ai_gateway_api_key": None}, "AI_GATEWAY_API_KEY"),
        (["--max-steps", "0"], {}, {}, "--max-steps"),
        (["--max-dollars", "-1"], {}, {}, "--max-dollars"),
    ],
)
async def test_configure_refuses_a_setup_that_cannot_work(
    argv: list[str], environ: dict[str, str], settings: dict[str, Any], message: str
) -> None:
    with pytest.raises(ConfigurationError, match=message):
        await configure(parse(argv), _settings(**settings), environ)


@pytest.mark.parametrize(
    "value", ["password=VAR", "password@https://a.example", "=VAR@https://a.example", "p=V@https://a.example/login"]
)
def test_a_secret_flag_must_name_an_origin(value: str) -> None:
    with pytest.raises(SystemExit):
        parse(["--secret", value])


def test_a_secret_origin_is_parsed_as_an_origin() -> None:
    args: argparse.Namespace = parse(["--secret", "p=V@https://a.example:8443"])
    assert args.secret == [("p", "V", "https://a.example:8443")]


@pytest.mark.parametrize(
    ("host", "loopback"), [("127.0.0.1", True), ("::1", True), ("localhost", True), ("0.0.0.0", False)]
)
def test_loopback_hosts(host: str, loopback: bool) -> None:
    assert is_loopback(host) is loopback


async def test_http_needs_the_bearer_token_except_for_the_health_check() -> None:
    server = build_server(ServerConfig(), runner=Recorder())
    app = BearerAuth(server.streamable_http_app(), "s3cret")
    transport = httpx.ASGITransport(app=app)
    client = httpx.AsyncClient(transport=transport, base_url="http://127.0.0.1:8000")
    # ASGITransport sends no lifespan events, so the session manager the app would start is started here.
    async with server.session_manager.run(), client:
        assert (await client.get("/healthz")).status_code == 200
        refused = await client.post("/mcp", json={})
        assert refused.status_code == 401
        assert refused.headers["www-authenticate"] == "Bearer"
        wrong = await client.post("/mcp", json={}, headers={"Authorization": "Bearer nope"})
        assert wrong.status_code == 401
        # Past the gate, the MCP app answers (406 here: the probe does not accept an event stream).
        allowed = await client.post("/mcp", json={}, headers={"Authorization": "Bearer s3cret"})
        assert allowed.status_code != 401


async def test_a_run_that_overruns_its_deadline_is_abandoned(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(mcp_server, "_OVERRUN_GRACE_SECONDS", 0.0)

    async def hangs(task: str, **_: Any) -> RunResult:
        await asyncio.sleep(30)
        raise AssertionError("not reached")

    server = build_server(ServerConfig(), runner=hangs)
    async with create_connected_server_and_client_session(server) as client:
        result = await client.call_tool("browse", {"task": "t", "start": START, "max_seconds": 0.05})
    assert result.isError
    assert "overran max_seconds=0.05" in _text(result)


async def test_a_call_waiting_for_a_slot_does_not_spend_its_own_time_limit() -> None:
    """The deadline is fixed when it is built, so building it before the wait would expire in the queue."""
    started = asyncio.Event()
    release = asyncio.Event()

    async def first(task: str, **_: Any) -> RunResult:
        started.set()
        await release.wait()
        return _result()

    async def second(task: str, **_: Any) -> RunResult:
        return _result()

    async def runner(task: str, **kwargs: Any) -> RunResult:
        return await (first if task == "first" else second)(task, **kwargs)

    server = build_server(ServerConfig(max_concurrent=1), runner=runner)
    async with create_connected_server_and_client_session(server) as client:
        queued = asyncio.create_task(client.call_tool("browse", {"task": "first", "start": START}))
        # The waiting call's whole limit passes while the slot is held.
        waiting = asyncio.create_task(
            client.call_tool("browse", {"task": "second", "start": START, "max_seconds": 0.2})
        )
        await started.wait()
        await asyncio.sleep(0.5)
        release.set()
        result = await waiting
        queued.cancel()
    assert not result.isError, _text(result)
