"""Run the fixture eval suites with real Jev and LLM clients against sites served in a headless Chrome.

    uv run python -m fastbrowse.evals.runner [--suite local mock] [--only TASK_ID ...] [--repeat N] [--out FILE]

Two suites run here. `local` serves static pages and grades what the site recorded. `mock` serves a stateful site
with a sign-in, a session and a second step, and grades the site's own state: which account signed in, what was
posted, whether an order was placed or a password changed.

Each mock task gets a site of its own, so one task's session, basket or order can never decide another's grade.

Needs OPENROUTER_API_KEY or AI_GATEWAY_API_KEY; see fastbrowse.clients.environment for provider routing.
"""

import argparse
import asyncio
import json
import math
import os
import sys
import tempfile
import time
from collections import Counter
from pathlib import Path
from typing import cast

import httpx
from pydantic import Field

from fastbrowse.adapters.local_chrome import local_chrome
from fastbrowse.agent import Agent
from fastbrowse.artifacts import DirectorySink
from fastbrowse.browser import BrowserSession, CdpPage
from fastbrowse.clients.environment import Settings, load_settings
from fastbrowse.config import Config
from fastbrowse.evals.local import Recorder, fixture_server
from fastbrowse.evals.mock import mock_site
from fastbrowse.evals.mock_tasks import TASKS as MOCK_TASKS
from fastbrowse.evals.mock_tasks import MockTask
from fastbrowse.evals.status import normalize
from fastbrowse.evals.tasks import TASKS, LocalTask
from fastbrowse.evals.versions import load_lock, provenance, suite_version, task_version
from fastbrowse.models import Attachment, BrowserConnection, CostBreakdown, Frozen, Limits, RunResult
from fastbrowse.safety import ScopedSecrets, origin_of
from fastbrowse.telemetry import traced, transient_seconds

MOCK_LIMITS = Limits(max_steps=40)
"""A mock task signs in, walks to a page and acts, which is more steps than the local suite's single form needs."""

LOCAL_LIMITS = Limits(max_steps=25)
"""A local fixture is one page and one form, so it needs far fewer steps than a mock task does."""

assert MOCK_LIMITS.max_steps is not None
assert LOCAL_LIMITS.max_steps is not None

HEADROOM = 0.6
"""The share of a step budget a task may use before the runner says so. A task at this share is one bad run from
its cap, and reaching the cap fails a run that was doing the right thing slowly."""


class _BudgetReceipt(Frozen):
    approved_usd: float = Field(gt=0, allow_inf_nan=False)
    known_usd: float = Field(default=0, ge=0, allow_inf_nan=False)
    unknown_reserved_usd: float = Field(default=0, ge=0, allow_inf_nan=False)
    reserved_usd: float = Field(default=0, ge=0, allow_inf_nan=False)
    unknown_cost: bool = False
    task: str | None = None
    repeat: int | None = None

    @property
    def remaining(self) -> float:
        return self.approved_usd - self.known_usd - self.unknown_reserved_usd - self.reserved_usd


class _FixtureBudget:
    def __init__(self, path: Path, approved_usd: float) -> None:
        self.path = path
        self.receipt = _BudgetReceipt(approved_usd=approved_usd)
        # A previous process can still own unpriced requests; restarting must not erase its reservation.
        with path.open("x", encoding="utf-8") as out:
            out.write(self.receipt.model_dump_json() + "\n")
        path.chmod(0o600)

    def _save(self) -> None:
        with tempfile.NamedTemporaryFile(mode="w", dir=self.path.parent, delete=False, encoding="utf-8") as out:
            out.write(self.receipt.model_dump_json() + "\n")
            temporary = Path(out.name)
        temporary.replace(self.path)

    def reserve(self, task: str, repeat: int) -> Limits:
        remaining = self.receipt.remaining
        if (
            remaining <= 0
            or self.receipt.reserved_usd
            or self.receipt.unknown_reserved_usd
            or self.receipt.unknown_cost
        ):
            raise ValueError("Fixture campaign has no unreserved budget")
        self.receipt = self.receipt.model_copy(update={"reserved_usd": remaining, "task": task, "repeat": repeat})
        self._save()
        return Limits(max_dollars=remaining)

    def settle(self, cost: CostBreakdown) -> None:
        reserved = self.receipt.reserved_usd
        if reserved <= 0:
            raise ValueError("Fixture run has no budget reservation")
        if any(line.dollars is not None and line.dollars < 0 for line in cost.lines):
            raise ValueError("Fixture cost is negative; its reservation remains held")
        known = cost.known_dollars
        if not math.isfinite(known) or any(
            line.dollars is not None and not math.isfinite(line.dollars) for line in cost.lines
        ):
            raise ValueError("Fixture cost is not finite; its reservation remains held")
        unknown_cost = any(line.dollars is None for line in cost.lines)
        unknown = max(0.0, reserved - known) if unknown_cost else 0.0
        self.receipt = _BudgetReceipt.model_validate(
            self.receipt.model_dump()
            | {
                "known_usd": self.receipt.known_usd + known,
                "unknown_reserved_usd": self.receipt.unknown_reserved_usd + unknown,
                "unknown_cost": self.receipt.unknown_cost or unknown_cost,
                "reserved_usd": 0.0,
            }
        )
        self._save()


def fixture_config() -> Config:
    """Fixtures own their scripted consent steps, so automatic refusal must not remove their controls."""
    return Config(refuse_cookie_banners=False)


def _row(
    result: RunResult, *, task_id: str, failure: str | None, seconds: float, lost: float, limit: int | None
) -> dict[str, object]:
    """The fields every suite's row carries, so one report can read both."""
    return {
        "correct": failure is None,
        "normalized_status": normalize(result.status),
        "task": task_id,
        "passed": failure is None,
        "would_fire": dict(Counter(tripwire.value for tripwire in result.would_fire)),
        "failure": failure,
        "status": result.status.value,
        "seconds": round(seconds, 1),
        "transient_seconds": round(lost, 2),
        "dollars": None if result.cost.has_unknown else round(result.cost.known_dollars, 5),
        "unknown_cost": result.cost.has_unknown,
        "cost": result.cost.model_dump(mode="json"),
        "seconds_by_call": result.cost.seconds_by_call(),
        "steps": len(result.steps),
        "step_limit": limit,
        "answer": result.answer,
        "data": result.data,
        "error": result.error,
        "trace": [f"{s.operation.value} {s.target or ''} -> {s.outcome.value}" for s in result.steps],
    }


async def _drive(
    task: LocalTask | MockTask,
    start: str,
    connection: BrowserConnection,
    http: httpx.AsyncClient,
    sink: DirectorySink,
    settings: Settings,
    secrets: ScopedSecrets | None,
    limits: Limits,
    attachments: tuple[Attachment, ...] = (),
) -> tuple[RunResult, float, float]:
    """One run of one task, returning its result and how long it took, wall and transient."""
    config = fixture_config()
    jev, llm = settings.jev(http), settings.llm(http)
    started = time.monotonic()
    with traced() as events:
        async with BrowserSession(connection, sink, refuse_cookie_banners=config.refuse_cookie_banners) as session:
            page = CdpPage(session, config)
            result = await Agent(page, jev, llm, config=config, secrets=secrets).run(
                task.task,
                start=start,
                inputs=task.inputs,
                attachments=attachments,
                output_schema=task.output_schema,
                limits=limits,
                authorization=task.authorization,
            )
    ended = time.monotonic()
    return result, ended - started, transient_seconds(events, started, ended)


async def run_task(
    task: LocalTask,
    base_url: str,
    recorder: Recorder,
    connection: BrowserConnection,
    http: httpx.AsyncClient,
    sink: DirectorySink,
    settings: Settings,
    *,
    limits: Limits = LOCAL_LIMITS,
) -> dict[str, object]:
    recorder.clear()
    result, seconds, lost = await _drive(task, base_url + task.start, connection, http, sink, settings, None, limits)
    failure = task.check(result, recorder.snapshot())
    return {"arm": "fastbrowse", "category": "fixture", "suite": "local"} | _row(
        result, task_id=task.id, failure=failure, seconds=seconds, lost=lost, limit=limits.max_steps
    )


async def run_mock_task(
    task: MockTask,
    connection: BrowserConnection,
    http: httpx.AsyncClient,
    sink: DirectorySink,
    settings: Settings,
    *,
    limits: Limits = MOCK_LIMITS,
) -> dict[str, object]:
    """A site of this task's own, so nothing it does can be read by, or decided by, another task's run."""
    with mock_site() as (base_url, site):
        secrets = ScopedSecrets(task.secrets, origin_of(base_url)) if task.secrets else None
        result, seconds, lost = await _drive(
            task,
            base_url + task.start,
            connection,
            http,
            sink,
            settings,
            secrets,
            limits,
            task.attachments,
        )
        failure = task.check(result, site)
        extra = {
            "signed_in": list(site.sign_ins),
            "posts": [path for path, _ in site.posts],
            "orders_placed": len(site.orders_placed),
        }
        return {"arm": "fastbrowse", "category": "mock", "suite": "mock", "expected_status": task.expect.value} | (
            _row(
                result,
                task_id=task.id,
                failure=failure,
                seconds=seconds,
                lost=lost,
                limit=limits.max_steps,
            )
            | extra
        )


def _tasks(only: list[str], suites: list[str]) -> tuple[list[tuple[str, LocalTask | MockTask]], list[str]]:
    chosen: list[tuple[str, LocalTask | MockTask]] = [("local", t) for t in TASKS] + [("mock", t) for t in MOCK_TASKS]
    chosen = [(suite, task) for suite, task in chosen if suite in suites]
    if only:
        chosen = [(suite, t) for suite, t in chosen if t.id in only]
    missing = sorted(set(only) - {t.id for _, t in chosen})
    return chosen, missing


async def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--suite", nargs="+", choices=["local", "mock"], default=["local", "mock"])
    parser.add_argument("--only", nargs="*", default=[])
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--out", type=Path, default=Path("artifacts/evals/local.jsonl"))
    parser.add_argument("--budget", type=float, help="approved total model dollars for the fixture campaign")
    args = parser.parse_args(argv)
    chosen, missing = _tasks(args.only, args.suite)
    if missing:
        parser.error(f"--only names no task: {', '.join(missing)}")
    if args.repeat < 1:
        parser.error("--repeat must be positive")
    if args.budget is not None and (not math.isfinite(args.budget) or args.budget <= 0):
        parser.error("--budget must be finite and positive")
    if args.budget is not None and args.out.exists():
        parser.error("budgeted fixture output already exists; reconcile its receipts before another campaign")
    settings = load_settings()
    lock = load_lock()
    stamps = {
        name: suite_version((t.id for t in tasks), lock) for name, tasks in (("local", TASKS), ("mock", MOCK_TASKS))
    }
    run = provenance(providers=settings.providers(), argv=list(argv))
    args.out.parent.mkdir(parents=True, exist_ok=True)
    if args.budget is not None:
        os.umask(0o077)
    campaign = _FixtureBudget(args.out.with_suffix(".budget.json"), args.budget) if args.budget is not None else None
    rows: list[dict[str, object]] = []
    with (
        fixture_server() as (base_url, recorder),
        local_chrome(settings.local_chrome()) as connection,
        tempfile.TemporaryDirectory() as downloads,
        args.out.open("a", encoding="utf-8") as out,
    ):
        async with httpx.AsyncClient(timeout=60) as http:
            for repeat in range(args.repeat):
                for suite, task in chosen:
                    if campaign is not None and (
                        campaign.receipt.remaining <= 0
                        or campaign.receipt.unknown_reserved_usd
                        or campaign.receipt.unknown_cost
                    ):
                        print("Fixture campaign budget is committed; no further run admitted", flush=True)
                        return 1
                    limits = (
                        campaign.reserve(task.id, repeat)
                        if campaign is not None
                        else (LOCAL_LIMITS if suite == "local" else MOCK_LIMITS)
                    )
                    sink = DirectorySink(Path(downloads))
                    if suite == "local":
                        assert isinstance(task, LocalTask)
                        row = await run_task(task, base_url, recorder, connection, http, sink, settings, limits=limits)
                    else:
                        assert isinstance(task, MockTask)
                        row = await run_mock_task(task, connection, http, sink, settings, limits=limits)
                    row |= {
                        "run": run
                        | {
                            "agent_limits": limits.model_dump(mode="json"),
                            "budget_usd": args.budget,
                            "budget_policy": "remaining-campaign-v1" if campaign is not None else "fixture-default-v1",
                        },
                        "suite_version": stamps[suite],
                        "task_version": task_version(task.id, lock),
                        "repeat": repeat,
                    }
                    rows.append(row)
                    out.write(json.dumps(row) + "\n")
                    out.flush()
                    if campaign is not None:
                        campaign.settle(CostBreakdown.model_validate(row["cost"]))
                    mark = "PASS" if row["passed"] else "FAIL"
                    print(
                        f"{mark} {suite:5} {task.id:26} {row['status']:20} {row['seconds']:>6}s ${row['dollars']!s:<8}",
                        row["failure"] or "",
                        flush=True,
                    )
                    used = cast("int", row["steps"])
                    budget = limits.max_steps
                    if budget is not None and used > HEADROOM * budget:
                        print(
                            f"WARN  {suite:5} {task.id:26} {used} of {budget} steps used "
                            f"({used / budget:.0%} of the budget)"
                        )
    passed = sum(bool(r["passed"]) for r in rows)
    print(f"{passed}/{len(rows)} passed")
    return 0 if passed == len(rows) else 1


if __name__ == "__main__":
    sys.exit(asyncio.run(main(sys.argv[1:])))
