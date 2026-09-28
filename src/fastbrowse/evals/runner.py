"""Run the fixture eval suites with real Jev and LLM clients against sites served in a headless Chrome.

    uv run python -m fastbrowse.evals.runner [--suite local mock] [--only TASK_ID ...] [--repeat N] [--out FILE]

Two suites run here. `local` serves static pages and grades what the site recorded. `mock` serves a stateful site
with a sign-in, a session and a second step, and grades the site's own state: which account signed in, what was
posted, whether an order was placed or a password changed.

Each mock task gets a site of its own, so one task's session, basket or order can never decide another's grade.

Needs OPENROUTER_API_KEY for Jev and the LLM; see fastbrowse.clients.environment for optional backups.
"""

import argparse
import asyncio
import json
import sys
import tempfile
import time
from collections import Counter
from pathlib import Path

import httpx

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
from fastbrowse.models import BrowserConnection, Limits, RunResult
from fastbrowse.safety import ScopedSecrets, origin_of
from fastbrowse.telemetry import traced, transient_seconds

MOCK_LIMITS = Limits(max_steps=40)
"""A mock task signs in, walks to a page and acts, which is more steps than the local suite's single form needs."""


def _row(result: RunResult, *, task_id: str, failure: str | None, seconds: float, lost: float) -> dict[str, object]:
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
        "seconds_by_call": result.cost.seconds_by_call(),
        "steps": len(result.steps),
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
) -> tuple[RunResult, float, float]:
    """One run of one task, returning its result and how long it took, wall and transient."""
    config = Config()
    jev, llm = settings.jev(http), settings.llm(http)
    started = time.monotonic()
    with traced() as events:
        async with BrowserSession(connection, sink) as session:
            page = CdpPage(session, config)
            result = await Agent(page, jev, llm, config=config, secrets=secrets).run(
                task.task,
                start=start,
                inputs=task.inputs,
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
) -> dict[str, object]:
    recorder.clear()
    result, seconds, lost = await _drive(
        task, base_url + task.start, connection, http, sink, settings, None, Limits(max_steps=25)
    )
    failure = task.check(result, recorder.snapshot())
    return {"arm": "fastbrowse", "category": "fixture", "suite": "local"} | _row(
        result, task_id=task.id, failure=failure, seconds=seconds, lost=lost
    )


async def run_mock_task(
    task: MockTask,
    connection: BrowserConnection,
    http: httpx.AsyncClient,
    sink: DirectorySink,
    settings: Settings,
) -> dict[str, object]:
    """A site of this task's own, so nothing it does can be read by, or decided by, another task's run."""
    with mock_site() as (base_url, site):
        secrets = ScopedSecrets(task.secrets, origin_of(base_url)) if task.secrets else None
        result, seconds, lost = await _drive(
            task, base_url + task.start, connection, http, sink, settings, secrets, MOCK_LIMITS
        )
        failure = task.check(result, site)
        extra = {
            "signed_in": list(site.sign_ins),
            "posts": [path for path, _ in site.posts],
            "orders_placed": len(site.orders_placed),
        }
        return {"arm": "fastbrowse", "category": "mock", "suite": "mock", "expected_status": task.expect.value} | (
            _row(result, task_id=task.id, failure=failure, seconds=seconds, lost=lost) | extra
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
    args = parser.parse_args(argv)
    chosen, missing = _tasks(args.only, args.suite)
    if missing:
        parser.error(f"--only names no task: {', '.join(missing)}")
    if args.repeat < 1:
        parser.error("--repeat must be positive")
    settings = load_settings()
    lock = load_lock()
    stamps = {
        name: suite_version((t.id for t in tasks), lock) for name, tasks in (("local", TASKS), ("mock", MOCK_TASKS))
    }
    run = provenance(providers=settings.providers(), argv=list(argv))
    args.out.parent.mkdir(parents=True, exist_ok=True)
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
                    sink = DirectorySink(Path(downloads))
                    if suite == "local":
                        assert isinstance(task, LocalTask)
                        row = await run_task(task, base_url, recorder, connection, http, sink, settings)
                    else:
                        assert isinstance(task, MockTask)
                        row = await run_mock_task(task, connection, http, sink, settings)
                    row |= {
                        "run": run,
                        "suite_version": stamps[suite],
                        "task_version": task_version(task.id, lock),
                        "repeat": repeat,
                    }
                    rows.append(row)
                    out.write(json.dumps(row) + "\n")
                    mark = "PASS" if row["passed"] else "FAIL"
                    print(
                        f"{mark} {suite:5} {task.id:26} {row['status']:20} {row['seconds']:>6}s ${row['dollars']:<8}",
                        row["failure"] or "",
                    )
    passed = sum(bool(r["passed"]) for r in rows)
    print(f"{passed}/{len(rows)} passed")
    return 0 if passed == len(rows) else 1


if __name__ == "__main__":
    sys.exit(asyncio.run(main(sys.argv[1:])))
