import asyncio
import runpy
import sys
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock

import httpx
import pytest
from mcp import ClientSession
from mcp.client.streamable_http import streamable_http_client
from mcp.types import CallToolResult, TextContent, Tool

from fastbrowse.evals import live, versions
from fastbrowse.evals.live_tasks import TASKS, Outcome

WORKER = runpy.run_path(str(live.CUA_RUNNER))


async def test_gateway_keeps_driver_transport_and_caps_calls(tmp_path: Path) -> None:
    response = CallToolResult(
        content=[TextContent(type="text", text="snapshot")], structuredContent={"refs": [{"ref": "p1:1"}]}
    )
    client = SimpleNamespace(
        list_tools=AsyncMock(
            return_value=SimpleNamespace(
                tools=[
                    Tool(name="get_browser_state", inputSchema={"type": "object"}),
                    Tool(name="launch_app", inputSchema={"type": "object"}),
                ]
            )
        ),
        call_tool=AsyncMock(return_value=response),
    )
    config = {"session": "owned", "target_id": "owned-target", "max_steps": 1, "calls": str(tmp_path / "calls.jsonl")}
    async with (
        WORKER["gateway"](client, config) as endpoint,
        streamable_http_client(endpoint) as (reader, writer, _),
        ClientSession(reader, writer) as model,
    ):
        await model.initialize()
        assert [tool.name for tool in (await model.list_tools()).tools] == ["get_browser_state"]
        result = await model.call_tool("get_browser_state", {"session": "other", "target_id": "other", "pid": 42})
        assert result.structuredContent == response.structuredContent
        client.call_tool.assert_awaited_once_with(
            "get_browser_state", {"session": "owned", "target_id": "owned-target"}
        )
        exhausted = await model.call_tool("get_browser_state", {})
        assert isinstance(exhausted.content[0], TextContent)
        assert "budget exhausted" in exhausted.content[0].text
        assert client.call_tool.await_count == 1
    assert len((tmp_path / "calls.jsonl").read_text().splitlines()) == 1


async def test_cua_done_is_graded_against_the_same_answer(monkeypatch: pytest.MonkeyPatch) -> None:
    task = next(task for task in TASKS if task.id == "arxiv-title")

    async def report(*_: object, **__: object) -> tuple[Outcome, live.ArmReport]:
        return Outcome("wrong title", None, task.start), live.ArmReport(status="done", seconds=1, dollars=None)

    monkeypatch.setattr(live, "cua_arm", report)
    async with httpx.AsyncClient() as http:
        row = await live.run_arm(
            "cua-codex", task, "Attention Is All You Need", http, Path(), bitwarden=False, record=None
        )
    assert not row.passed
    assert row.dollars is None


async def test_cancellation_gives_worker_time_to_clean_up(tmp_path: Path) -> None:
    worker = tmp_path / "worker.py"
    ready, cleaned = tmp_path / "ready", tmp_path / "cleaned"
    worker.write_text(
        "import pathlib, signal, time\n"
        f"def stop(*args):\n pathlib.Path({str(cleaned)!r}).touch()\n raise SystemExit(0)\n"
        "signal.signal(signal.SIGTERM, stop)\n"
        f"pathlib.Path({str(ready)!r}).touch()\n"
        "time.sleep(60)\n"
    )
    running = asyncio.create_task(live._invoke((sys.executable,), worker, {}, {}, str(tmp_path), graceful=True))
    try:
        async with asyncio.timeout(5):
            for _ in range(500):
                if ready.exists():
                    break
                await asyncio.sleep(0.01)
            assert ready.exists()
        running.cancel()
        with pytest.raises(asyncio.CancelledError):
            await running
        assert cleaned.exists()
    finally:
        running.cancel()
        await asyncio.gather(running, return_exceptions=True)


def test_cua_selection_excludes_credentials_and_safe_stop_tasks() -> None:
    assert live.ARMS["cua-codex"].default is False
    assert live.ARMS["cua-codex"].tier == "local"
    for suite in live.SUITES.values():
        for task in suite:
            if task.secrets or task.bitwarden_item or task.expect.value != "complete":
                assert not live.eligible("cua-codex", task)


def test_unmetered_spend_is_unknown_in_comparison_tables() -> None:
    stats = versions._arm_stats([{"seconds": 1, "dollars": None, "passed": True, "correct": True}])
    assert stats["total cost"] == "unknown"


def test_existing_structured_tasks_get_strict_schemas_without_changing_the_tasks() -> None:
    for task in TASKS:
        if task.output_schema is None:
            continue
        original = task.output_schema.model_json_schema()
        strict = WORKER["strict_schema"](original)
        assert strict["additionalProperties"] is False
        assert strict["required"] == list(original["properties"])
        assert "additionalProperties" not in original
    nested = WORKER["strict_schema"](
        {"type": "array", "items": {"type": "object", "properties": {"value": {"type": "string"}}}}
    )
    assert nested["items"]["additionalProperties"] is False


async def test_timeout_keeps_the_attempt_artifact(monkeypatch: pytest.MonkeyPatch) -> None:
    async def hanging(*_: object, **__: object) -> None:
        live._attempt_artifact.set("artifacts/evals/cua/owned-attempt")
        await asyncio.Event().wait()

    monkeypatch.setattr(live, "cua_arm", hanging)
    monkeypatch.setattr(live, "STUCK_SECONDS", 0.01)
    task = next(task for task in TASKS if task.id == "arxiv-title")
    async with httpx.AsyncClient() as http:
        row = await live.run_arm("cua-codex", task, "title", http, Path(), bitwarden=False, record=None)
    assert row.artifact == "artifacts/evals/cua/owned-attempt"
    assert row.unknown_cost is True
    assert not row.passed
