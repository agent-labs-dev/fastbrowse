"""Head-to-head on live sites: fastbrowse on a Browser Use Cloud browser vs hosted Browser Use, same prompts.

    uv run --extra browser-use python -m fastbrowse.evals.live [--only TASK_ID ...] [--category CATEGORY ...]
        [--arms fast hosted] [--bitwarden] [--repeat N] [--out artifacts/evals/live.jsonl]

Needs BROWSER_USE_API_KEY (both arms), and the Jev and LLM keys in fastbrowse.clients.environment (fast arm).
Each run prints a WATCH line with the URL where its browser can be watched live.

Tasks and their grading live in fastbrowse.evals.live_tasks. With --bitwarden, the fast arm reads each login
task's credentials from its vault item (created by scripts/eval_vault.py) instead of the task.

Both arms are graded on their answer. The fast arm is also graded on the page it ended on and on its final
status; the hosted SDK exposes neither, so its tasks rest on the answer alone.
"""

import argparse
import asyncio
import json
import sys
import tempfile
import time
from pathlib import Path
from typing import cast

import httpx
from pydantic import BaseModel

from fastbrowse.adapters.bitwarden import bitwarden_login
from fastbrowse.clients.environment import load_settings
from fastbrowse.evals.live_tasks import TASKS, Category, LiveTask, Outcome
from fastbrowse.models import Authorization, BrowserEvent, CostBreakdown, Limits, RunResult, StepEvent
from fastbrowse.run import run_task
from fastbrowse.safety import ScopedSecrets, origin_of

HOSTED_MAX_DOLLARS = 0.50


def _watch(arm: str, task: LiveTask, live_url: str | None) -> None:
    if live_url:
        print(f"WATCH {arm:6} {task.id:18} {live_url}", flush=True)


def _secrets(task: LiveTask, bitwarden: bool) -> ScopedSecrets | None:
    origin = origin_of(task.start)
    if bitwarden and task.bitwarden_item is not None:
        return ScopedSecrets(bitwarden_login(task.bitwarden_item, origin), origin)
    return ScopedSecrets(task.secrets, origin) if task.secrets else None


async def fast_arm(
    task: LiveTask, http: httpx.AsyncClient, downloads: Path, *, bitwarden: bool
) -> tuple[Outcome, RunResult, CostBreakdown]:
    result = await run_task(
        task.task,
        start=task.start,
        browser_api_key=load_settings().browser_key(),
        output_schema=task.output_schema,
        secrets=_secrets(task, bitwarden),
        limits=Limits(max_steps=30, max_dollars=0.25, max_seconds=300),
        authorization=Authorization(irreversible_actions=task.authorize),
        downloads=downloads,
        http=http,
        on_event=lambda event: _on_fast_event(task, event),
    )
    quotes = tuple((e.url, e.quote) for e in result.evidence)
    return Outcome(result.answer, result.data, result.final_url or task.start, quotes), result, result.cost


async def _on_fast_event(task: LiveTask, event: StepEvent | BrowserEvent) -> None:
    if isinstance(event, BrowserEvent):
        _watch("fast", task, event.live_url)


async def hosted_arm(task: LiveTask) -> tuple[Outcome, str, float | None]:
    from browser_use_sdk.v3 import AsyncBrowserUse  # pyright: ignore[reportMissingTypeStubs] - optional extra

    client = AsyncBrowserUse(api_key=load_settings().browser_key())
    run = client.run(
        f"Start at {task.start}. {task.task}",
        output_schema=task.output_schema,
        max_cost_usd=HOSTED_MAX_DOLLARS,
        proxy_country_code="us",
        sensitive_data=dict(task.secrets) or None,
    )
    finishing = asyncio.ensure_future(run)
    # The session id appears once the SDK has created the session, which is when its live URL exists.
    while run.session_id is None and not finishing.done():
        await asyncio.wait({finishing}, timeout=0.2)
    if run.session_id is not None:
        _watch("hosted", task, (await client.sessions.get(run.session_id)).live_url)
    result = await finishing
    session = result.session
    output = result.output
    if isinstance(output, BaseModel):
        outcome = Outcome(output.model_dump_json(), output.model_dump(), None)
    else:
        outcome = Outcome(str(output) if output else None, None, None)
    status = session.status.value
    cost = session.total_cost_usd
    return outcome, status, None if cost is None else float(cost)


async def run_arm(
    arm: str, task: LiveTask, http: httpx.AsyncClient, downloads: Path, *, bitwarden: bool
) -> dict[str, object]:
    truth = await task.truth(http)
    started = time.monotonic()
    row: dict[str, object] = {"arm": arm, "task": task.id, "category": task.category.value}
    try:
        if arm == "fast":
            outcome, result, cost = await fast_arm(task, http, downloads, bitwarden=bitwarden)
            status = result.status.value
            row["error"] = result.error
            row["trace"] = [f"{s.operation.value} {s.target or ''} -> {s.outcome.value}" for s in result.steps]
            dollars: float | None = cost.known_dollars
            row["unknown_cost"] = cost.has_unknown
            row["seconds_by_call"] = cost.seconds_by_call()
            row["cost_by_component"] = {
                c: round(sum(line.dollars or 0 for line in cost.lines if line.component == c), 5)
                for c in {line.component.value for line in cost.lines}
            }
        else:
            outcome, status, dollars = await hosted_arm(task)
    except Exception as exc:  # a crashed arm is a failed task, recorded rather than aborting the comparison
        row |= {
            "passed": False,
            "failure": f"{type(exc).__name__}: {exc}",
            "seconds": round(time.monotonic() - started, 1),
        }
        return row
    failure = task.check(outcome, truth)
    # Right and proven are graded apart: a correct answer the agent could not back with quotes is a
    # different defect from a wrong one, and one pass/fail column hid which the suite was showing.
    correct = failure is None
    if arm == "fast" and failure is None and status != task.expect.value:
        failure = f"status {status}, expected {task.expect.value}"
    return row | {
        "correct": correct,
        "passed": failure is None,
        "failure": failure,
        "status": status,
        "seconds": round(time.monotonic() - started, 1),
        "dollars": None if dollars is None else round(dollars, 5),
        "answer": outcome.answer,
        "data": outcome.data,
        "final_url": outcome.final_url,
    }


async def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--only", nargs="*", default=[])
    parser.add_argument("--category", nargs="*", default=[], choices=[c.value for c in Category])
    parser.add_argument("--bitwarden", action="store_true", help="login credentials from the vault items")
    parser.add_argument("--arms", nargs="*", default=["fast", "hosted"], choices=["fast", "hosted"])
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--out", type=Path, default=Path("artifacts/evals/live.jsonl"))
    args = parser.parse_args(argv)
    tasks = [
        t
        for t in TASKS
        if (not args.only or t.id in args.only) and (not args.category or t.category.value in args.category)
    ]
    args.out.parent.mkdir(parents=True, exist_ok=True)
    rows: list[dict[str, object]] = []
    with tempfile.TemporaryDirectory() as downloads, args.out.open("a") as out:
        async with httpx.AsyncClient(timeout=60) as http:
            for _ in range(args.repeat):
                for task in tasks:
                    for arm in args.arms:
                        if arm == "hosted" and task.fast_only:
                            continue
                        row = await run_arm(arm, task, http, Path(downloads), bitwarden=args.bitwarden)
                        rows.append(row)
                        out.write(json.dumps(row, default=str) + "\n")
                        out.flush()
                        mark = "PASS" if row["passed"] else "FAIL"
                        print(
                            f"{mark} {arm:6} {task.id:20} {row.get('seconds')!s:>6}s ${row.get('dollars')!s:<8}",
                            row["failure"] or "",
                            flush=True,
                        )
    for arm in args.arms:
        arm_rows = [r for r in rows if r["arm"] == arm]
        passed = sum(bool(r["passed"]) for r in arm_rows)
        dollars = sum(float(d) for r in arm_rows if isinstance(d := r.get("dollars"), int | float))
        seconds = sum(float(s) for r in arm_rows if isinstance(s := r.get("seconds"), int | float))
        correct = sum(bool(r.get("correct")) for r in arm_rows)
        print(f"{arm}: {passed}/{len(arm_rows)} passed, {correct} correct, ${dollars:.4f}, {seconds:.0f}s")
        calls: dict[str, float] = {}
        for r in arm_rows:
            for label, spent in cast(dict[str, float], r.get("seconds_by_call", {})).items():
                calls[label] = calls.get(label, 0.0) + spent
        for label, spent in sorted(calls.items(), key=lambda item: -item[1]):
            print(f"  {label:18} {spent / len(arm_rows):5.1f}s a task")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main(sys.argv[1:])))
