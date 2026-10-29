"""In-process probes the audit cases drive as a command.

Each probe prints exactly one JSON object to stdout and nothing else, so the runner can assert on it. A
probe that fails prints `{"status": "probe_error", "error": ...}` rather than raising, so a broken probe
shows up as a failed case with a reason instead of a silent crash.

    python -m fastbrowse.audit.probes <name>
"""

import asyncio
import json
import os
import subprocess
import sys
from typing import Any

from pydantic import ValidationError

from fastbrowse.audit.cases import CLI_FLAGS
from fastbrowse.models import (
    CostBreakdown,
    Limits,
    LocalChrome,
    RunResult,
    Status,
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
            # `--json` adds the build that produced the result, which is not a `RunResult` field.
            fields = json.loads(run.stdout)
            fields.pop("build", None)
            result = RunResult.model_validate(fields)
            statuses.append(result.status.value)
            cost += result.cost.known_dollars
        except (json.JSONDecodeError, ValidationError):
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

    from fastbrowse.evals.live_tasks import Release
    from fastbrowse.run import run_task

    result = asyncio.run(
        run_task(
            "What is the current release version of httpx? Return the package name and version.",
            start="https://pypi.org/project/httpx/",
            chrome=LocalChrome(),
            limits=Limits(max_dollars=float(os.environ["FB_AUDIT_MAX_DOLLARS"])),
            output_schema=Release,
        )
    )
    valid = False
    if result.data is not None:
        try:
            Release.model_validate(result.data)
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


PROBES: dict[str, Any] = {
    "cli_surface": cli_surface,
    "mcp_ceilings": mcp_ceilings,
    "repeat_status": repeat_status,
    "structured_output": structured_output,
    "embed_run_task": embed_run_task,
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
