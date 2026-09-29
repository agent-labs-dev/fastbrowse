"""In-process probes the audit cases drive as a command.

Each probe prints exactly one JSON object to stdout and nothing else, so the runner can assert on it. A
probe that fails prints `{"status": "probe_error", "error": ...}` rather than raising, so a broken probe
shows up as a failed case with a reason instead of a silent crash.

    python -m fastbrowse.audit.probes <name>
"""

import ast
import asyncio
import contextlib
import inspect
import io
import json
import os
import re
import subprocess
import sys
from typing import Any

from fastbrowse.audit.cases import CLI_FLAGS
from fastbrowse.models import (
    BrowserEvent,
    CostBreakdown,
    Decider,
    Limits,
    LocalChrome,
    Operation,
    RunResult,
    Status,
    StepEvent,
    StepOutcome,
    StepResult,
)


def _cli() -> str:
    return os.environ.get("FB_AUDIT_CLI", "fastbrowse")


def _mcp() -> str:
    return os.environ.get("FB_AUDIT_MCP", "fastbrowse-mcp")


def _run(argv: list[str], timeout: float = 30.0) -> subprocess.CompletedProcess[str]:
    return subprocess.run(argv, capture_output=True, text=True, timeout=timeout, check=False)


def _result(status: Status = Status.COMPLETE) -> RunResult:
    return RunResult(
        status=status,
        answer="done",
        data=None,
        evidence=(),
        steps=(),
        cost=CostBreakdown(lines=()),
        artifacts=(),
    )


def cli_surface() -> dict[str, Any]:
    """T0.1: --help and --version render, and help documents every flag."""
    help_run = _run([_cli(), "--help"])
    version_run = _run([_cli(), "--version"])
    missing = [flag for flag in CLI_FLAGS if flag not in help_run.stdout]
    return {
        "status": "ok",
        "help_exit": help_run.returncode,
        "version_exit": version_run.returncode,
        "missing_flags": missing,
        "version": version_run.stdout.strip(),
    }


def mcp_ceilings() -> dict[str, Any]:
    """T0.5: each MCP ceiling at zero is refused, naming the flag."""
    out: dict[str, Any] = {"status": "ok"}
    for key, flag in (
        ("max_steps", "--max-steps"),
        ("max_dollars", "--max-dollars"),
        ("max_seconds", "--max-seconds"),
        ("max_concurrent", "--max-concurrent"),
    ):
        run = _run([_mcp(), flag, "0"])
        out[f"{key}_refused"] = run.returncode != 0
        out[f"{key}_named"] = flag in run.stderr
        out[f"{key}_exit"] = run.returncode
    return out


def _exit_code_for(argv: list[str]) -> int:
    from fastbrowse import cli

    namespace = cli._parse(argv)  # the audit suite is allowed to read the private parser
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        return asyncio.run(cli.run(namespace))


def status_exit_contract() -> dict[str, Any]:
    """T0.10: each status has a stable exit code; zero is success and two is reserved for usage."""
    from fastbrowse import cli

    original = cli.run_task
    codes: dict[str, list[int]] = {}

    def fake_for(target: Status) -> Any:
        async def fake(task: str, **kwargs: Any) -> RunResult:
            return _result(target)

        return fake

    try:
        for status in Status:
            vars(cli)["run_task"] = fake_for(status)
            codes[status.value] = [_exit_code_for(["x", "--local", "--json"]) for _ in range(2)]
    finally:
        vars(cli)["run_task"] = original
    every = set(Status)
    complete_zero = codes.get("complete") == [0, 0]
    failures = [codes[status.value][0] for status in every if status is not Status.COMPLETE]
    distinct_failures = len(set(failures)) == len(failures) and all(code > 0 and code != 2 for code in failures)
    stable = all(first == second for first, second in codes.values())
    return {
        "status": "ok",
        "mapping": {name: pair[0] for name, pair in codes.items()},
        "codes": codes,
        "total": set(codes) == {status.value for status in every},
        "complete_zero": complete_zero,
        "distinct_failures": distinct_failures,
        "stable": stable,
    }


def json_stdout_discipline() -> dict[str, Any]:
    """T0.11: with --json, stdout is one JSON document and progress goes to stderr."""
    from fastbrowse import cli

    original = cli.run_task

    async def fake(task: str, **kwargs: Any) -> RunResult:
        on_event = kwargs.get("on_event")
        if on_event is not None:
            await on_event(BrowserEvent(live_url="https://live.example.com/session"))
            await on_event(
                StepEvent(
                    step=StepResult(
                        index=0,
                        operation=Operation.READ,
                        decided_by=Decider.LLM,
                        outcome=StepOutcome.EXECUTED,
                        url="https://example.com/",
                        duration_ms=1,
                    )
                )
            )
        return _result()

    vars(cli)["run_task"] = fake
    try:
        namespace = cli._parse(["x", "--local", "--json"])
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            asyncio.run(cli.run(namespace))
        stdout, stderr = out.getvalue(), err.getvalue()
    finally:
        vars(cli)["run_task"] = original
    parsed = False
    try:
        json.loads(stdout)
        parsed = True
    except json.JSONDecodeError:
        parsed = False
    return {
        "status": "ok",
        "parsed": parsed,
        "progress_on_stderr": "watch live" in stderr,
        "progress_absent_from_stdout": "watch live" not in stdout,
        "stdout_len": len(stdout),
    }


def cli_library_parity() -> dict[str, Any]:
    """T0.12: report the run_task parameters the terminal never exposes, read from the code."""
    from fastbrowse import cli
    from fastbrowse.run import run_task

    source = inspect.getsource(cli)
    tree = ast.parse(source)
    cli_keywords: set[str] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id == "run_task":
            cli_keywords |= {keyword.arg for keyword in node.keywords if keyword.arg}
    parameters = set(inspect.signature(run_task).parameters) - {"task"}
    library_only = sorted(parameters - cli_keywords)
    cli_only = sorted(cli_keywords - parameters)
    fields = set(RunResult.model_json_schema(mode="serialization")["properties"])
    human = set(re.findall(r"result\.([a-z_]+)", source)) & fields
    return {
        "status": "ok",
        "library_only": library_only,
        "cli_only": cli_only,
        "cli_keywords": sorted(cli_keywords),
        "on_event_cli_wired": "on_event" in cli_keywords,
        "runresult_fields": sorted(fields),
        "runresult_json_complete": "model_dump_json" in source,
        "runresult_fields_in_human_output": sorted(human),
        "runresult_fields_json_only": sorted(fields - human),
    }


# --------------------------------------------------------------------------- tier 2 and 3 helpers


def repeat_status() -> dict[str, Any]:
    """T2.7: the same task twice reaches the same status."""
    argv = [
        _cli(),
        "What is the capital of France?",
        "--start",
        "https://en.wikipedia.org/wiki/Paris",
        "--local",
        "--json",
    ]
    budget = float(os.environ["FB_AUDIT_MAX_DOLLARS"])
    argv += ["--max-dollars", str(budget / 2)]
    cost = 0.0
    statuses: list[str | None] = []
    for _ in range(2):
        run = _run(argv, timeout=280.0)
        try:
            result = RunResult.model_validate_json(run.stdout)
            statuses.append(result.status.value)
            cost += result.cost.known_dollars
        except (json.JSONDecodeError, KeyError):
            statuses.append(None)
    return {
        "status": "ok",
        "statuses": statuses,
        "cost_usd": cost,
        "same_status": statuses[0] is not None and statuses[0] == statuses[1],
        "both_recorded": all(item is not None for item in statuses),
    }


def structured_output() -> dict[str, Any]:
    """T2.8: a structured-output run validates data against its schema."""
    from pydantic import ValidationError

    from fastbrowse.evals.live_tasks import TASKS
    from fastbrowse.run import run_task

    task = next(task for task in TASKS if task.id == "pypi-structured")
    assert task.output_schema is not None

    result = asyncio.run(
        run_task(
            task.task,
            start=task.start,
            chrome=LocalChrome(),
            limits=Limits(max_dollars=float(os.environ["FB_AUDIT_MAX_DOLLARS"])),
            output_schema=task.output_schema,
        )
    )
    valid = False
    if result.data is not None:
        try:
            task.output_schema.model_validate(result.data)
            valid = True
        except ValidationError:
            valid = False
    return {
        "status": result.status.value,
        "data_valid": valid,
        "data": result.data,
        "cost_usd": result.cost.known_dollars,
    }


def embed_run_task() -> dict[str, Any]:
    """T3.5: run_task from Python returns a RunResult carrying evidence and citations."""
    from fastbrowse.run import run_task

    result = asyncio.run(
        run_task(
            "What is the title of the page, and quote it?",
            start="https://example.com/",
            chrome=LocalChrome(),
            limits=Limits(max_dollars=float(os.environ["FB_AUDIT_MAX_DOLLARS"])),
        )
    )
    return {
        "status": "ok",
        "is_runresult": isinstance(result, RunResult),
        "has_evidence": len(result.evidence) > 0,
        "has_citations": len(result.citations) > 0,
        "cost_usd": result.cost.known_dollars,
    }


def _tool_payload(result: Any) -> dict[str, Any]:
    structured = getattr(result, "structuredContent", None)
    if isinstance(structured, dict):
        return structured
    for item in getattr(result, "content", []):
        text = getattr(item, "text", None)
        if isinstance(text, str):
            try:
                loaded = json.loads(text)
            except json.JSONDecodeError:
                continue
            if isinstance(loaded, dict):
                return loaded
    return {}


def _fake_runner(delay: float = 0.0) -> Any:
    async def run(task: str, **kwargs: Any) -> RunResult:
        if delay:
            await asyncio.sleep(delay)
        return _result()

    return run


def mcp_stdio() -> dict[str, Any]:
    """T3.1: an MCP stdio handshake lists the browse tool with a usable description."""
    from mcp.shared.memory import create_connected_server_and_client_session

    from fastbrowse.mcp_server import ServerConfig, build_server

    async def go() -> dict[str, Any]:
        server = build_server(ServerConfig())
        async with create_connected_server_and_client_session(server) as session:
            tools = await session.list_tools()
        names = [tool.name for tool in tools.tools]
        description = next((tool.description or "" for tool in tools.tools if tool.name == "browse"), "")
        return {
            "status": "ok",
            "tools": names,
            "has_browse": "browse" in names,
            "has_description": len(description) > 20,
        }

    return asyncio.run(go())


def mcp_result() -> dict[str, Any]:
    """T3.2: an MCP call returns the BrowseResult shape, not a raw RunResult."""
    from mcp.shared.memory import create_connected_server_and_client_session

    from fastbrowse.mcp_server import ServerConfig, build_server

    browse_keys = {
        "status",
        "answer",
        "data",
        "citations",
        "next_step",
        "error",
        "final_url",
        "live_url",
        "steps",
        "dollars",
        "cost_complete",
        "downloads",
    }
    runresult_only = {"artifacts", "recordings", "would_fire", "evidence"}

    async def go() -> dict[str, Any]:
        server = build_server(ServerConfig(), runner=_fake_runner())
        async with create_connected_server_and_client_session(server) as session:
            call = await session.call_tool("browse", {"task": "x", "start": "https://example.com/"})
        payload = _tool_payload(call)
        return {
            "status": "ok",
            "payload_keys": sorted(payload),
            "browse_keys": browse_keys <= set(payload),
            "no_runresult_keys": not (runresult_only & set(payload)),
        }

    return asyncio.run(go())


def mcp_http() -> dict[str, Any]:
    """T3.3: MCP http rejects a wrong token, accepts the right one, and leaves healthz open."""
    import httpx

    from fastbrowse.mcp_server import BearerAuth, ServerConfig, build_server

    server = build_server(ServerConfig())
    app = BearerAuth(server.streamable_http_app(), "audit-secret-token")
    transport = httpx.ASGITransport(app=app)

    async def go() -> dict[str, Any]:
        async with (
            server.session_manager.run(),
            httpx.AsyncClient(transport=transport, base_url="http://127.0.0.1:8000") as client,
        ):
            health = await client.get("/healthz")
            bad = await client.post("/mcp", headers={"Authorization": "Bearer wrong"}, json={})
            good = await client.post("/mcp", headers={"Authorization": "Bearer audit-secret-token"}, json={})
        return {
            "status": "ok",
            "healthz_status": health.status_code,
            "bad_status": bad.status_code,
            "good_status": good.status_code,
            "healthz_open": health.status_code == 200,
            "bad_token_rejected": bad.status_code == 401,
            "good_token_accepted": good.status_code != 401,
        }

    return asyncio.run(go())


def mcp_concurrency() -> dict[str, Any]:
    """T3.4: with one slot, two calls finish rather than fail."""
    from mcp.shared.memory import create_connected_server_and_client_session

    from fastbrowse.mcp_server import ServerConfig, build_server

    async def go() -> dict[str, Any]:
        active = 0
        peak = 0

        async def counted(task: str, **kwargs: Any) -> RunResult:
            nonlocal active, peak
            active += 1
            peak = max(peak, active)
            try:
                await asyncio.sleep(0.2)
                return _result()
            finally:
                active -= 1

        server = build_server(ServerConfig(max_concurrent=1), runner=counted)
        async with create_connected_server_and_client_session(server) as session:
            first, second = await asyncio.gather(
                session.call_tool("browse", {"task": "a", "start": "https://example.com/"}),
                session.call_tool("browse", {"task": "b", "start": "https://example.com/"}),
            )
        return {
            "status": "ok",
            "first_ok": bool(_tool_payload(first)),
            "second_ok": bool(_tool_payload(second)),
            "both_finished": bool(_tool_payload(first)) and bool(_tool_payload(second)),
            "peak_concurrent": peak,
        }

    return asyncio.run(go())


PROBES: dict[str, Any] = {
    "cli_surface": cli_surface,
    "mcp_ceilings": mcp_ceilings,
    "status_exit_contract": status_exit_contract,
    "json_stdout_discipline": json_stdout_discipline,
    "cli_library_parity": cli_library_parity,
    "repeat_status": repeat_status,
    "structured_output": structured_output,
    "embed_run_task": embed_run_task,
    "mcp_stdio": mcp_stdio,
    "mcp_result": mcp_result,
    "mcp_http": mcp_http,
    "mcp_concurrency": mcp_concurrency,
}


def main(argv: list[str]) -> int:
    if not argv or argv[0] not in PROBES:
        print(json.dumps({"status": "probe_error", "error": f"unknown probe: {argv[0] if argv else ''}"}))
        return 1
    try:
        payload = PROBES[argv[0]]()
    except Exception as exc:  # a probe reports, it does not crash the runner
        print(json.dumps({"status": "probe_error", "error": f"{type(exc).__name__}: {exc}"}))
        return 1
    print(json.dumps(payload))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
