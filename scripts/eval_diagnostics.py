"""Maintainer-supplied diagnostics beside, never instead of, the full fixture gate."""

import argparse
import asyncio
import hashlib
import importlib
import json
import math
import os
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path
from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, Field

ModelSetting = Literal[
    "FASTBROWSE_DECISION_SOURCE",
    "FASTBROWSE_DECISION_MODEL",
    "FASTBROWSE_LLM_PROVIDER",
    "FASTBROWSE_LLM_MODEL",
    "FASTBROWSE_LLM_REASONING",
]


class Probe(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True)

    task: str = Field(min_length=1)
    start: str | None = None
    fetch: bool = False
    output_schema: dict[str, Any] | None = None
    allowed_origins: list[str] | None = None
    settings: dict[ModelSetting, str] = Field(default_factory=dict)


def positive(value: str) -> float:
    dollars = float(value)
    if not math.isfinite(dollars) or dollars <= 0:
        raise ValueError("diagnostic budget must be finite and positive")
    return dollars


def configuration(env: dict[str, str]) -> tuple[Probe | None, list[str]]:
    raw = env.get("EVAL_PROBE", "")
    probe = Probe.model_validate_json(raw) if raw else None
    tasks = json.loads(env.get("EVAL_FILL_TASKS", "") or "[]")
    if not isinstance(tasks, list) or any(not isinstance(task, str) or not task for task in tasks):
        raise ValueError("fill_tasks must be a JSON array of task IDs")
    if len(set(tasks)) != len(tasks):
        raise ValueError("fill_tasks must contain unique task IDs")
    if (probe or tasks) and env.get("GITHUB_EVENT_NAME") != "workflow_dispatch":
        raise ValueError("diagnostics require workflow_dispatch")
    for enabled, name in ((probe is not None, "EVAL_PROBE_BUDGET"), (bool(tasks), "EVAL_FILL_BUDGET")):
        if enabled:
            positive(env.get(name, ""))
        elif env.get(name):
            raise ValueError(f"{name} supplied without its diagnostic")
    if probe and probe.output_schema is not None and probe.output_schema.get("type") != "object":
        raise ValueError("output_schema must have an object root; the native run validates the full schema")
    if tasks:
        from fastbrowse.evals.bench import _select

        _select(tasks)
    if probe or tasks:
        certificate = env.get("EVAL_DIAGNOSTIC_CERTIFICATE", "")
        with tempfile.NamedTemporaryFile(mode="w", suffix=".pem") as recipient:
            recipient.write(certificate)
            recipient.flush()
            subprocess.run(
                ["openssl", "cms", "-encrypt", "-aes-256-cbc", "-binary", "-outform", "DER", "-recip", recipient.name],
                input=b"",
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=True,
            )
    return probe, tasks


def write(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2, default=str) + "\n", encoding="utf-8")


def identity() -> dict[str, object]:
    from fastbrowse import build

    native = importlib.import_module("fastbrowse._native")
    assert native.__file__ is not None
    path = Path(native.__file__)
    facts = build.identity()
    expected = os.environ["GITHUB_SHA"]
    if facts["git_sha"] != expected or facts["git_dirty"] is not False:
        raise ValueError("diagnostics require the exact clean workflow checkout")
    return {**facts, "native_path": str(path), "native_sha256": hashlib.sha256(path.read_bytes()).hexdigest()}


def run_settings(probe: Probe) -> dict[str, str]:
    """Match fastbrowse_evals.backend.run_settings without installing the campaign package."""
    from fastbrowse.evals.harness import environment
    from fastbrowse.evals.judge import jev_source

    env = environment()
    chosen = {
        name: value
        for name, value in env.items()
        if name in ("OPENROUTER_API_KEY", "TYPESAFE_API_KEY", "EXA_API_KEY") or name.startswith("FASTBROWSE_")
    }
    chosen.pop("FASTBROWSE_JEV_SOURCE", None)
    return chosen | {"FASTBROWSE_DECISION_SOURCE": jev_source(env)} | probe.settings


def output_model(schema: dict[str, Any] | None) -> type[BaseModel] | None:
    if schema is None:
        return None

    class Output(BaseModel):
        @classmethod
        def model_json_schema(cls, *args: Any, **kwargs: Any) -> dict[str, Any]:
            assert schema is not None
            return schema

    return Output


async def validate_probe(probe: Probe) -> None:
    import httpx

    from fastbrowse import run_task
    from fastbrowse.models import LocalChrome

    reached = False

    class Validated(RuntimeError):
        pass

    async def stop() -> None:
        nonlocal reached
        reached = True
        raise Validated()

    async def refuse_http(_: httpx.Request) -> httpx.Response:
        await stop()
        raise AssertionError("unreachable")

    # The public run validates arguments before browser access or HTTP. Neither can leave this preflight.
    async with httpx.AsyncClient(transport=httpx.MockTransport(refuse_http)) as http:
        try:
            await run_task(
                probe.task,
                start=probe.start,
                fetch=probe.fetch,
                chrome=None if probe.fetch else LocalChrome(),
                allowed_origins=probe.allowed_origins,
                output_schema=output_model(probe.output_schema),
                settings={
                    "OPENROUTER_API_KEY": "validation-only",
                    "TYPESAFE_API_KEY": "validation-only",
                    "FASTBROWSE_DECISION_SOURCE": "openrouter",
                    **probe.settings,
                },
                check_access=stop,
                http=http,
            )
        except Exception:
            if not reached:
                raise
    if not reached:
        raise ValueError("native preflight stopped before argument validation could be confirmed")


async def run_probe(probe: Probe, dollars: float, out: Path) -> int:
    from fastbrowse import run_task, traced
    from fastbrowse.models import Limits, LocalChrome, Status

    output = output_model(probe.output_schema)

    receipt: dict[str, object] = {
        "identity": identity(),
        "approved_budget_usd": dollars,
        "request": probe.model_dump(),
        "unresolved_budget_hold_usd": dollars,
    }
    write(out / "probe.receipt.json", receipt)
    with traced() as events:
        try:
            result = await run_task(
                probe.task,
                start=probe.start,
                fetch=probe.fetch,
                chrome=None if probe.fetch else LocalChrome(),
                output_schema=output,
                allowed_origins=probe.allowed_origins,
                settings=run_settings(probe),
                limits=Limits(max_dollars=dollars),
            )
            write(out / "probe.result.json", result.model_dump(mode="json"))
            receipt["cost"] = result.cost.model_dump(mode="json")
            receipt["status"] = result.status
            if not result.cost.has_unknown:
                receipt.pop("unresolved_budget_hold_usd", None)
            return (
                0
                if result.status == Status.COMPLETE
                and not result.cost.has_unknown
                and result.cost.known_dollars <= dollars
                else 1
            )
        except Exception as error:
            receipt["error"] = f"{type(error).__name__}: {error}"
            receipt["unresolved_budget_hold_usd"] = dollars
            raise
        finally:
            write(out / "probe.receipt.json", receipt)
            with (out / "probe.trace.jsonl").open("w", encoding="utf-8") as stream:
                for event in events:
                    stream.write(json.dumps(event, default=str) + "\n")


def encrypt() -> int:
    out = Path("artifacts/evals/diagnostics")
    if not out.exists():
        return 0
    certificate = out / "recipient.pem"
    certificate.write_text(os.environ["EVAL_DIAGNOSTIC_CERTIFICATE"], encoding="utf-8")
    archive = out / "raw.tar.gz"
    with tarfile.open(archive, "w:gz") as tar:
        for path in sorted(out.iterdir()):
            if path != archive:
                tar.add(path, arcname=f"diagnostics/{path.name}")
        tar.add("bench/cassettes", arcname="cassettes")
    subprocess.run(
        [
            "openssl",
            "cms",
            "-encrypt",
            "-aes-256-cbc",
            "-binary",
            "-outform",
            "DER",
            "-in",
            str(archive),
            "-out",
            "artifacts/evals/diagnostics.cms",
            "-recip",
            str(certificate),
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=True,
    )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("validate", "probe", "fill", "encrypt"))
    args = parser.parse_args()
    if args.mode == "encrypt":
        return encrypt()
    probe, tasks = configuration(dict(os.environ))
    if args.mode == "validate":
        if probe:
            asyncio.run(validate_probe(probe))
        return 0
    out = Path("artifacts/evals/diagnostics")
    out.mkdir(parents=True, exist_ok=True, mode=0o700)
    if args.mode == "probe":
        return asyncio.run(run_probe(probe, positive(os.environ["EVAL_PROBE_BUDGET"]), out)) if probe else 0
    if not tasks:
        return 0
    dollars = positive(os.environ["EVAL_FILL_BUDGET"])
    receipt = {
        "identity": identity(),
        "approved_budget_usd": dollars,
        "tasks": tasks,
        "budget_semantics": "existing bench fill checks between tasks, not a per-call ceiling",
        "unresolved_budget_hold_usd": dollars,
    }
    write(out / "fill.receipt.json", receipt)
    command = [
        sys.executable,
        "-m",
        "fastbrowse.evals.bench",
        "fill",
        "--budget",
        str(dollars),
        "--cassettes",
        str(Path("bench/cassettes").resolve()),
        "--out",
        str(out / "fill.json"),
        "--only",
        *tasks,
    ]
    with (out / "fill.log").open("w", encoding="utf-8") as log:
        code = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=False).returncode
    report_path = out / "fill.json"
    if not report_path.exists():
        return 1
    report = json.loads(report_path.read_text(encoding="utf-8"))
    entries = report["tasks"]
    known = sum(entry.get("recorded_cost_usd") or 0 for entry in entries)
    unknown = any(entry.get("unpriced_exchanges", 0) for entry in entries)
    receipt["recorded_known_cost_usd"] = known
    receipt["has_unpriced_exchanges"] = unknown
    complete = {entry["task"] for entry in entries} == set(tasks)
    converged = all(not entry["misses"] and entry["result"]["grade"]["passed"] for entry in entries)
    receipt["complete"] = complete
    receipt["converged"] = converged
    write(out / "fill.receipt.json", receipt)
    return 0 if code == 0 and complete and converged and not unknown and known <= dollars else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception:
        # Provider errors can contain page content, which belongs only in the encrypted receipt.
        print("Diagnostic failed; inspect the encrypted receipt or dispatch input validation.", file=sys.stderr)
        raise SystemExit(1) from None
