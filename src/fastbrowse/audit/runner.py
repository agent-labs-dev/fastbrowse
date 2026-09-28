"""Run the audit suite's cases by tier and write one report plus a Markdown roll-up.

    uv run python -m fastbrowse.audit --tier 0
    uv run python -m fastbrowse.audit --tier 1 2 --spend --max-dollars 0.05

Tier 0 spends nothing, so it is the default and is safe to run anywhere. Tiers 1 to 3 call paid models, so
they are refused unless `--spend` is given.

Every case yields one JSON row shaped as `{id, tier, command, exit_code, status, duration_s, cost_usd,
assertions, evidence, budget_usd, timeout_s}`. The assertions array holds one entry per check, each naming
what it expected and what it saw. The evidence array lists the files written for the case: its captured
output, plus any file or recording it produced.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

from fastbrowse import __file__ as _fastbrowse_file
from fastbrowse.audit.cases import CASES, Assertion, AuditCase
from fastbrowse.audit.live import fixture_server

# Keys the refusal cases must not see, whether they come from the environment or from a `.env` the case
# would otherwise inherit. A case that needs one sets it back explicitly.
_SCRUBBED = frozenset(
    {
        "TYPESAFE_API_KEY",
        "AI_GATEWAY_API_KEY",
        "OPENROUTER_API_KEY",
        "BROWSER_USE_API_KEY",
        "FASTBROWSE_MCP_TOKEN",
        "FASTBROWSE_HEADED",
        "FASTBROWSE_PROFILE",
        "FASTBROWSE_JEV_SOURCE",
        "FASTBROWSE_JEV_BASE_URL",
        "FASTBROWSE_JEV_MODEL",
        "FASTBROWSE_LLM_MODEL",
    }
)

_REPO = Path(_fastbrowse_file).resolve().parents[2]
_SRC = Path(_fastbrowse_file).resolve().parents[1]

ROWS: tuple[str, ...] = (
    "id",
    "tier",
    "command",
    "exit_code",
    "status",
    "duration_s",
    "cost_usd",
    "assertions",
    "evidence",
    "budget_usd",
    "timeout_s",
)


def _script(name: str) -> str:
    """The console script, preferring the one beside this interpreter so a source run finds its own venv."""
    found = shutil.which(name)
    if found:
        return found
    candidate = Path(sys.executable).with_name(name)
    return str(candidate)


def _dig(data: Any, pointer: str) -> Any:
    """Read a dotted pointer out of `data`, where a segment may carry a `[index]` suffix."""
    if not pointer:
        return data
    node = data
    for segment in pointer.split("."):
        name, _, index = segment.partition("[")
        if name:
            if not isinstance(node, Mapping) or name not in node:
                return None
            node = node[name]
        if index:
            position = index.rstrip("]")
            if not isinstance(node, Sequence) or isinstance(node, (str, bytes)) or not position.isdigit():
                return None
            node = node[int(position)]
    return node


def _actual(source: str, pointer: str, ctx: Mapping[str, Any]) -> Any:
    match source:
        case "exit_code":
            return ctx.get("exit_code")
        case "stdout":
            return ctx.get("stdout", "")
        case "stderr":
            return ctx.get("stderr", "")
        case "json":
            return _dig(ctx.get("json"), pointer)
        case "recorder":
            return _dig(ctx.get("recorder"), pointer)
        case "file":
            return ctx.get("file", {}).get(pointer)
        case _:
            return None


def _evaluate(assertion: Assertion, ctx: Mapping[str, Any]) -> dict[str, Any]:
    value = assertion.value
    actual = _actual(assertion.source, assertion.pointer, ctx)
    op = assertion.op
    ok: bool
    match op:
        case "eq":
            ok = actual == value
        case "ne":
            ok = actual != value
        case "contains":
            ok = isinstance(actual, str) and str(value) in actual
        case "not_contains":
            ok = isinstance(actual, str) and str(value) not in actual
        case "present":
            ok = actual is not None and actual != "" and actual != [] and actual != {}
        case "absent":
            ok = actual is None
        case "is_true":
            ok = actual is True
        case "is_false":
            ok = actual is False
        case "ge":
            ok = isinstance(actual, (int, float)) and actual >= value  # type: ignore[operator]
        case "le":
            ok = isinstance(actual, (int, float)) and actual <= value  # type: ignore[operator]
        case "matches":
            import re

            ok = isinstance(actual, str) and re.search(str(value), actual) is not None
        case "list_eq":
            ok = actual == value
        case "list_contains":
            ok = isinstance(actual, list) and value in actual
        case "list_excludes":
            ok = isinstance(actual, list) and value not in actual
        case "count_eq":
            ok = isinstance(actual, (list, Mapping)) and len(actual) == value
        case "count_ge":
            ok = isinstance(actual, (list, Mapping)) and isinstance(value, int) and len(actual) >= value
        case "one_of":
            ok = isinstance(actual, str) and isinstance(value, list) and actual in value
        case "keys_present":
            keys = value if isinstance(value, Sequence) and not isinstance(value, (str, bytes)) else ()
            ok = isinstance(actual, Mapping) and bool(keys) and all(key in actual for key in keys)
        case "exists":
            ok = actual is True
        case "nonempty":
            ok = actual is True
        case _:
            ok = False
    return {"name": assertion.name, "expected": value, "actual": actual, "ok": ok}


def _file_state(path: str) -> dict[str, bool]:
    target = Path(path)
    if not target.exists():
        return {"exists": False, "nonempty": False}
    if target.is_dir():
        return {"exists": True, "nonempty": any(target.iterdir())}
    return {"exists": True, "nonempty": target.stat().st_size > 0}


def _substitute(argv: Sequence[str], mapping: Mapping[str, str]) -> list[str]:
    return [token.format(**mapping) if "{" in token else token for token in argv]


def _environment(case: AuditCase) -> dict[str, str]:
    env = {key: value for key, value in os.environ.items() if key not in _SCRUBBED}
    env["PYTHONPATH"] = str(_SRC) + os.pathsep + env.get("PYTHONPATH", "")
    env["FB_AUDIT_CLI"] = _script("fastbrowse")
    env["FB_AUDIT_MCP"] = _script("fastbrowse-mcp")
    for name, value in case.env:
        if value is None:
            env.pop(name, None)
        else:
            env[name] = value
    return env


def _cost(payload: Any) -> float:
    if isinstance(payload, Mapping):
        if isinstance(payload.get("cost_usd"), (int, float)):
            return float(payload["cost_usd"])
        cost = payload.get("cost")
        if isinstance(cost, Mapping):
            lines = cost.get("lines", [])
            return round(sum(line["dollars"] for line in lines if isinstance(line, Mapping) and line.get("dollars")), 6)
    return 0.0


def _observed_status(exit_code: int | None, payload: Any) -> str:
    if isinstance(payload, Mapping) and isinstance(payload.get("status"), str):
        return str(payload["status"])
    if exit_code == 0:
        return "ok"
    return "refused"


def run_case(case: AuditCase, *, workdir: Path, max_dollars: float, spend: bool) -> dict[str, Any]:
    scratch = tempfile.mkdtemp(prefix=f"fb-audit-{case.id}-")
    downloads = Path(scratch) / "downloads"
    downloads.mkdir(exist_ok=True)
    record = Path(scratch) / "run.mp4"
    mapping = {
        "python": sys.executable,
        "cli": _script("fastbrowse"),
        "mcp": _script("fastbrowse-mcp"),
        "base_url": "",
        "downloads": str(downloads),
        "record": str(record),
    }
    cwd = scratch if case.foreign_cwd else str(_REPO)
    evidence_dir = workdir / "evidence"
    evidence_dir.mkdir(parents=True, exist_ok=True)

    started = time.monotonic()
    recorder: Mapping[str, Any] = {}
    if case.fixture:
        with fixture_server() as (base_url, record_state):
            mapping["base_url"] = base_url
            rows = _execute(case, mapping, cwd, downloads, record, evidence_dir)
            recorder = record_state()
    else:
        rows = _execute(case, mapping, cwd, downloads, record, evidence_dir)
    duration = round(time.monotonic() - started, 3)

    ctx: dict[str, Any] = {
        "exit_code": rows["exit_code"],
        "stdout": rows["stdout"],
        "stderr": rows["stderr"],
        "json": rows["json"],
        "recorder": recorder,
        "file": {
            "{downloads}": _file_state(str(downloads))["nonempty"],
            "{record}": _file_state(str(record))["exists"],
        },
    }

    assertions: list[dict[str, Any]] = []
    if case.expect_exit is not None:
        assertions.append(
            {
                "name": "exit code",
                "expected": case.expect_exit,
                "actual": rows["exit_code"],
                "ok": rows["exit_code"] == case.expect_exit,
            }
        )
    status = _observed_status(rows["exit_code"], rows["json"])
    if case.expect_status is not None:
        assertions.append(
            {"name": "status", "expected": case.expect_status, "actual": status, "ok": status == case.expect_status}
        )
    for assertion in case.assertions:
        assertions.append(_evaluate(assertion, ctx))

    evidence: list[str] = [str(_write_evidence(case, rows, evidence_dir))]
    if "{downloads}" in " ".join(case.argv):
        evidence.append(str(downloads))
    if "{record}" in " ".join(case.argv):
        evidence.append(str(record))

    return {
        "id": case.id,
        "tier": case.tier,
        "command": " ".join(_substitute(case.argv, mapping)),
        "exit_code": rows["exit_code"],
        "status": status,
        "duration_s": duration,
        "cost_usd": _cost(rows["json"]),
        "assertions": assertions,
        "evidence": evidence,
        "budget_usd": min(case.budget_usd, max_dollars) if case.spends else 0.0,
        "timeout_s": case.timeout_s,
    }


def _execute(
    case: AuditCase,
    mapping: Mapping[str, str],
    cwd: str,
    downloads: Path,
    record: Path,
    evidence_dir: Path,
) -> dict[str, Any]:
    argv = _substitute(case.argv, mapping)
    try:
        completed = subprocess.run(
            argv,
            env=_environment(case),
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=case.timeout_s,
            check=False,
        )
        stdout, stderr, code = completed.stdout, completed.stderr, completed.returncode
    except subprocess.TimeoutExpired as expired:
        stdout = _text(expired.stdout)
        stderr = _text(expired.stderr)
        code = None
    payload: Any = None
    for candidate in (stdout,):
        stripped = candidate.strip()
        if stripped.startswith("{"):
            try:
                payload = json.loads(stripped)
            except json.JSONDecodeError:
                payload = None
    return {"exit_code": code, "stdout": stdout, "stderr": stderr, "json": payload}


def _text(value: bytes | str | None) -> str:
    if value is None:
        return ""
    return value.decode(errors="replace") if isinstance(value, bytes) else value


def _write_evidence(case: AuditCase, rows: Mapping[str, Any], evidence_dir: Path) -> Path:
    path = evidence_dir / f"{case.id}.json"
    payload = {
        "id": case.id,
        "purpose": case.purpose,
        "exit_code": rows["exit_code"],
        "stdout": rows["stdout"],
        "stderr": rows["stderr"],
        "json": rows["json"],
    }
    path.write_text(json.dumps(payload, indent=2), encoding="utf-8")
    return path


def _select(cases: Sequence[AuditCase], tiers: Sequence[int], only: Sequence[str]) -> list[AuditCase]:
    chosen = [case for case in cases if case.tier in tiers]
    if only:
        wanted = set(only)
        chosen = [case for case in chosen if case.id in wanted]
    return chosen


def _markdown(rows: Sequence[Mapping[str, Any]]) -> str:
    lines = [
        "# Fastbrowse capability audit roll-up",
        "",
        "| Id | Tier | Status | Exit | Time | Cost | Result |",
        "|:--|:--|:--|:--|:--|:--|:--|",
    ]
    for row in rows:
        failed = [item["name"] for item in row["assertions"] if not item["ok"]]
        verdict = "pass" if not failed else "FAIL: " + ", ".join(failed)
        lines.append(
            f"| {row['id']} | {row['tier']} | {row['status']} | {row['exit_code']} | {row['duration_s']}s | "
            f"${row['cost_usd']} | {verdict} |"
        )
    passed = sum(1 for row in rows if all(item["ok"] for item in row["assertions"]))
    lines += ["", f"{passed}/{len(rows)} cases passed."]
    return "\n".join(lines) + "\n"


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="fastbrowse.audit", description="Run the capability audit suite.")
    parser.add_argument("--tier", type=int, action="append", default=None, help="tier to run; repeatable (default 0)")
    parser.add_argument("--only", action="append", default=[], help="case id to run; repeatable")
    parser.add_argument("--spend", action="store_true", help="allow tiers that call paid models")
    parser.add_argument("--max-dollars", type=float, default=0.25, help="per-case ceiling for a spending tier")
    parser.add_argument("--out", type=Path, default=Path("artifacts/audit/report.json"))
    parser.add_argument("--rollup", type=Path, default=Path("artifacts/audit/rollup.md"))
    args = parser.parse_args(argv)

    tiers = args.tier if args.tier else [0]
    if any(tier < 0 or tier > 3 for tier in tiers):
        parser.error("--tier must be between 0 and 3")
    chosen = _select(CASES, tiers, args.only)
    if not chosen:
        parser.error("no case matched the selected tiers and ids")
    if any(case.spends for case in chosen) and not args.spend:
        spending = sorted({case.id for case in chosen if case.spends})
        parser.error(f"tiers that call paid models need --spend: {', '.join(spending)}")
    over_budget = sorted({case.id for case in chosen if case.spends and case.budget_usd > args.max_dollars})
    if over_budget:
        parser.error(f"a case's declared budget exceeds --max-dollars={args.max_dollars}: {', '.join(over_budget)}")

    args.out.parent.mkdir(parents=True, exist_ok=True)
    workdir = args.out.parent
    rows: list[dict[str, Any]] = []
    for case in chosen:
        row = run_case(case, workdir=workdir, max_dollars=args.max_dollars, spend=args.spend)
        rows.append(row)
        failed = [item["name"] for item in row["assertions"] if not item["ok"]]
        mark = "PASS" if not failed else "FAIL"
        print(f"{mark} {case.id:6} tier {case.tier} {row['status']:20} {row['duration_s']:>6}s ${row['cost_usd']}")
        if failed:
            print(f"      failed: {', '.join(failed)}")

    args.out.write_text(json.dumps(rows, indent=2), encoding="utf-8")
    args.rollup.write_text(_markdown(rows), encoding="utf-8")
    passed = sum(1 for row in rows if all(item["ok"] for item in row["assertions"]))
    print(f"{passed}/{len(rows)} cases passed; wrote {args.out} and {args.rollup}")
    return 0 if passed == len(rows) else 1
