"""Run the local eval tasks with real Jev and LLM clients against fixture sites in a headless Chrome.

    uv run python -m fastbrowse.evals.runner [--only TASK_ID ...] [--repeat N] [--out results.jsonl]

Needs Jev and LLM keys; see fastbrowse.clients.environment.
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
from fastbrowse.evals.status import normalize
from fastbrowse.evals.tasks import TASKS, LocalTask
from fastbrowse.evals.versions import load_lock, provenance, suite_version, task_version
from fastbrowse.models import BrowserConnection, Limits
from fastbrowse.telemetry import traced, transient_seconds


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
    config = Config()
    jev, llm = settings.jev(http), settings.llm(http)
    started = time.monotonic()
    with traced() as events:
        async with BrowserSession(connection, sink) as session:
            page = CdpPage(session, config)
            result = await Agent(page, jev, llm, config=config).run(
                task.task,
                start=base_url + task.start,
                inputs=task.inputs,
                output_schema=task.output_schema,
                limits=Limits(max_steps=25),
                authorization=task.authorization,
            )
    ended = time.monotonic()
    lost = transient_seconds(events, started, ended)
    failure = task.check(result, recorder.snapshot())
    return {
        "arm": "fastbrowse",
        "category": "fixture",
        "correct": failure is None,
        "normalized_status": normalize(result.status),
        "task": task.id,
        "passed": failure is None,
        "would_fire": dict(Counter(tripwire.value for tripwire in result.would_fire)),
        "failure": failure,
        "status": result.status.value,
        "seconds": round(ended - started, 1),
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


async def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--only", nargs="*", default=[])
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--out", type=Path, default=Path("artifacts/evals/local.jsonl"))
    args = parser.parse_args(argv)
    tasks = [t for t in TASKS if not args.only or t.id in args.only]
    if missing := sorted(set(args.only) - {t.id for t in tasks}):
        parser.error(f"--only names no local task: {', '.join(missing)}")
    settings = load_settings()
    lock = load_lock()
    stamp = {
        "suite": "local",
        "suite_version": suite_version((t.id for t in TASKS), lock),
        "run": provenance(providers=settings.providers(), argv=list(argv)),
    }
    args.out.parent.mkdir(parents=True, exist_ok=True)
    rows: list[dict[str, object]] = []
    with (
        fixture_server() as (base_url, recorder),
        local_chrome(settings.local_chrome()) as connection,
        tempfile.TemporaryDirectory() as downloads,
        args.out.open("a", encoding="utf-8") as out,
    ):
        async with httpx.AsyncClient(timeout=60) as http:
            for _ in range(args.repeat):
                for task in tasks:
                    row = await run_task(
                        task, base_url, recorder, connection, http, DirectorySink(Path(downloads)), settings
                    )
                    row |= stamp | {"task_version": task_version(task.id, lock)}
                    rows.append(row)
                    out.write(json.dumps(row) + "\n")
                    mark = "PASS" if row["passed"] else "FAIL"
                    summary = f"{mark} {task.id:28} {row['status']:20} {row['seconds']:>6}s ${row['dollars']:<8}"
                    print(summary, row["failure"] or "")
    passed = sum(bool(r["passed"]) for r in rows)
    print(f"{passed}/{len(rows)} passed")
    return 0 if passed == len(rows) else 1


if __name__ == "__main__":
    sys.exit(asyncio.run(main(sys.argv[1:])))
