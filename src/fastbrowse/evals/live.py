"""Head-to-head on live sites: fastbrowse, jev-ultrafast and hosted Browser Use, same prompts and limits.

    uv run --extra browser-use python -m fastbrowse.evals.live [--only TASK_ID ...] [--category CATEGORY ...]
        [--arms fast ultrafast hosted] [--bitwarden] [--repeat N] [--record DIR]
        [--out artifacts/evals/live.jsonl]

Needs BROWSER_USE_API_KEY (every arm), and the Jev and LLM keys in fastbrowse.clients.environment (fast and
ultrafast arms). Each run prints a WATCH line with the URL where its browser can be watched live.

The fast and ultrafast arms each drive a fresh Browser Use Cloud browser; hosted Browser Use brings its own.
jev-ultrafast runs as its published package in an environment of its own (see scripts/ultrafast_arm.py).

Tasks and their grading live in fastbrowse.evals.live_tasks. With --bitwarden, the fast arm reads each login
task's credentials from its vault item (created by scripts/eval_vault.py) instead of the task. With --record,
each run is saved as DIR/<arm>/<task>-<n>.mp4, n counting up from 1 past any video already there.

Each task runs only on the arms it grades on equal terms (LiveTask.arms). Answer tasks compare fastbrowse with hosted
Browser Use; jev-ultrafast returns no answer, only DONE or BLOCKED. Navigation tasks, graded on the page the
run ended on, compare fastbrowse with jev-ultrafast; the hosted SDK does not say where its browser ended.
fastbrowse must also end with the task's expected status, and jev-ultrafast with DONE.
"""

import argparse
import asyncio
import json
import logging
import os
import statistics
import sys
import tempfile
import time
from collections.abc import Generator
from contextlib import contextmanager
from pathlib import Path
from typing import Any, cast
from unittest import mock

import httpx
from pydantic import BaseModel

import fastbrowse.run
from fastbrowse.adapters.bitwarden import bitwarden_login
from fastbrowse.adapters.browser_use_cloud import BrowserUseCloudBrowser
from fastbrowse.agent import Agent
from fastbrowse.browser import CdpPage
from fastbrowse.browser.recording import Recording
from fastbrowse.clients.environment import load_settings
from fastbrowse.evals.live_tasks import TASKS, Category, LiveTask, Outcome
from fastbrowse.evals.more_tasks import DEV, HELDOUT
from fastbrowse.models import Authorization, BrowserEvent, Limits, RunResult, StepEvent
from fastbrowse.page import Observation
from fastbrowse.run import run_task
from fastbrowse.safety import ScopedSecrets, origin_of
from fastbrowse.telemetry import TRACE

ARMS = ("fast", "ultrafast", "hosted")
MAX_STEPS, MAX_DOLLARS, MAX_SECONDS = 30, 0.25, 300
LIMITS = Limits(max_steps=MAX_STEPS, max_dollars=MAX_DOLLARS, max_seconds=MAX_SECONDS)
"""Every arm's bound. Hosted Browser Use takes the dollar cap and is stopped at the time limit, but has no step
cap to set."""

ULTRAFAST = "jev-ultrafast @ git+https://github.com/browser-use/jev-ultrafast@1231850a0bf1a0c0341fe408ef1668dbbfdfac46"
ULTRAFAST_RUNNER = Path(__file__).resolve().parents[3] / "scripts" / "ultrafast_arm.py"
ULTRAFAST_COMMAND = ("uv", "run", "--no-project", "--quiet", "--python", "3.14", "--with", ULTRAFAST, "python")
ULTRAFAST_TEXT_MODEL = "inception/mercury-2.5"
"""jev-ultrafast's own configuration (.env.example): its text helper on OpenRouter, reasoning off."""
_KILL_GRACE_SECONDS = 60
"""The runner stops itself at the time limit; past this much longer it is killed."""


def _watch(arm: str, task: LiveTask, live_url: str | None) -> None:
    if live_url:
        print(f"WATCH {arm:9} {task.id:18} {live_url}", flush=True)


def _secrets(task: LiveTask, bitwarden: bool) -> ScopedSecrets | None:
    origin = origin_of(task.start)
    if bitwarden and task.bitwarden_item is not None:
        return ScopedSecrets(bitwarden_login(task.bitwarden_item, origin), origin)
    return ScopedSecrets(task.secrets, origin) if task.secrets else None


def video_path(folder: Path, arm: str, task: LiveTask) -> Path:
    """The first free DIR/<arm>/<task>-<n>.mp4, absolute because the ultrafast runner has its own working directory.

    Counting past existing files means repeated invocations never overwrite a video.
    """
    n = 1
    while (path := folder.resolve() / arm / f"{task.id}-{n}.mp4").exists():
        n += 1
    path.parent.mkdir(parents=True, exist_ok=True)
    return path


class _TimedRecording(Recording):
    """A fastbrowse recording that notes when the run ended, so the result card is not timed as the run."""

    ended: float | None = None

    async def show_result(self, task: str, result: RunResult) -> None:
        _TimedRecording.ended = time.monotonic()
        await super().show_result(task, result)


# A popup left open marks the rest of the page aria-hidden, which the agent rightly ignores and a grader
# reading the form must not; so the grader's look lifts the marks, observes as the agent would, and restores them.
_UNHIDE = """(() => {
  const marked = [...document.querySelectorAll('[aria-hidden="true"],[inert]')];
  window.__fastbrowseHidden = marked.map(e => [e, e.getAttribute('aria-hidden'), e.hasAttribute('inert')]);
  marked.forEach(e => { e.removeAttribute('aria-hidden'); e.removeAttribute('inert'); });
})()"""
_RESTORE = """(() => {
  for (const [e, hidden, inert] of window.__fastbrowseHidden || []) {
    if (hidden !== null) e.setAttribute('aria-hidden', hidden);
    if (inert) e.setAttribute('inert', '');
  }
  delete window.__fastbrowseHidden;
})()"""


class _GradedPage(CdpPage):
    async def observe_all(self) -> Observation:
        """Every control on the page, including those a popup has hidden from the agent."""
        session_id = self._session.active_session_id
        await self._evaluate(session_id, _UNHIDE)
        try:
            return await self.observe()
        finally:
            await self._evaluate(session_id, _RESTORE)


class _ObservedAgent(Agent):
    """fastbrowse's agent, with the page it ended on observed once more after the run, for graders that read the
    page itself (what a form ended up holding) rather than anything the agent reported."""

    controls: tuple[tuple[str, str | None], ...] | None = None

    async def run(self, *args: Any, **kwargs: Any) -> RunResult:
        result = await super().run(*args, **kwargs)
        try:
            observation = await self._page.observe()
            hidden = await self._page.observe_all() if isinstance(self._page, _GradedPage) else None
        except Exception:  # a page that cannot be observed leaves nothing to grade, which the grader reports
            return result
        controls = (*observation.controls, *(hidden.controls if hidden is not None else ()))
        _ObservedAgent.controls = tuple((c.label, c.value) for c in controls)
        return result


class _Collect(logging.Handler):
    def __init__(self) -> None:
        super().__init__(logging.DEBUG)
        self.events: list[object] = []

    def emit(self, record: logging.LogRecord) -> None:
        self.events.append(getattr(record, "trace", record.getMessage()))


@contextmanager
def _traced() -> Generator[list[object]]:
    """The agent's trace events for one run; tasks run one at a time, so one handler at a time sees them."""
    handler = _Collect()
    previous = TRACE.level
    TRACE.addHandler(handler)
    TRACE.setLevel(logging.DEBUG)
    try:
        yield handler.events
    finally:
        TRACE.removeHandler(handler)
        TRACE.setLevel(previous)


async def fast_arm(
    task: LiveTask, http: httpx.AsyncClient, downloads: Path, *, bitwarden: bool, record: Path | None
) -> tuple[Outcome, RunResult, float | None]:
    _TimedRecording.ended = None
    _ObservedAgent.controls = None
    with (
        mock.patch.object(fastbrowse.run, "Recording", _TimedRecording),
        mock.patch.object(fastbrowse.run, "Agent", _ObservedAgent),
        mock.patch.object(fastbrowse.run, "CdpPage", _GradedPage),
    ):
        result = await run_task(
            task.task,
            start=task.start,
            browser_api_key=load_settings().browser_key(),
            output_schema=task.output_schema,
            secrets=_secrets(task, bitwarden),
            limits=LIMITS,
            authorization=Authorization(irreversible_actions=task.authorize),
            downloads=downloads,
            http=http,
            on_event=lambda event: _on_fast_event(task, event),
            record=record,
        )
    quotes = tuple((e.url, e.quote) for e in result.evidence)
    outcome = Outcome(result.answer, result.data, result.final_url or task.start, quotes, _ObservedAgent.controls)
    return outcome, result, _TimedRecording.ended


async def _on_fast_event(task: LiveTask, event: StepEvent | BrowserEvent) -> None:
    if isinstance(event, BrowserEvent):
        _watch("fast", task, event.live_url)


async def prepare_ultrafast() -> None:
    """Install jev-ultrafast's environment once, so no run is timed installing it."""
    process = await asyncio.create_subprocess_exec(*ULTRAFAST_COMMAND, "-c", "import jev_ultrafast")
    if await process.wait() != 0:
        raise RuntimeError(f"could not install {ULTRAFAST}")


def _ultrafast_env(cdp_ws: str, runtime: str) -> dict[str, str]:
    settings = load_settings()
    env = dict(os.environ)
    # Its text helper is an OpenRouter model; Jev comes from TypeSafe directly or through the gateway.
    env |= {
        "TEXT_MODEL_API_KEY": settings.openrouter_key(),
        "TEXT_MODEL_BASE_URL": "https://openrouter.ai/api/v1",
        "TEXT_MODEL": ULTRAFAST_TEXT_MODEL,
        "TEXT_MODEL_REASONING": "none",
        "BU_CDP_WS": cdp_ws,
        "BU_NAME": "fastbrowse-eval",
        "BH_RUNTIME_DIR": runtime,
        "BH_TELEMETRY": "0",
        "BH_UPDATE_CHECK": "0",
        "BH_TAB_MARKER": "0",
    }
    for name, key in (
        ("TYPESAFE_API_KEY", settings.typesafe_api_key),
        ("AI_GATEWAY_API_KEY", settings.ai_gateway_api_key),
    ):
        if key is not None:
            env[name] = key.get_secret_value()
    return env


async def ultrafast_arm(
    task: LiveTask, http: httpx.AsyncClient, *, record: Path | None
) -> tuple[Outcome, dict[str, object]]:
    started = time.monotonic()
    cloud = BrowserUseCloudBrowser(load_settings().browser_key(), http=http)
    report: dict[str, object] = {}
    async with cloud:
        booted = time.monotonic() - started
        _watch("ultrafast", task, cloud.connection.live_url)
        request = {
            "start": task.start,
            "goal": task.task,
            "cdp_ws": cloud.connection.cdp_url,
            "max_steps": MAX_STEPS,
            "max_dollars": MAX_DOLLARS,
            "max_seconds": MAX_SECONDS,
            "record": None if record is None else str(record),
        }
        with tempfile.TemporaryDirectory(prefix="bh-") as runtime:
            process = await asyncio.create_subprocess_exec(
                *ULTRAFAST_COMMAND,
                str(ULTRAFAST_RUNNER),
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                env=_ultrafast_env(cloud.connection.cdp_url, runtime),
                cwd=runtime,
            )
            try:
                stdout, _ = await asyncio.wait_for(
                    process.communicate(json.dumps(request).encode()), MAX_SECONDS + _KILL_GRACE_SECONDS
                )
            except TimeoutError:
                process.kill()
                await process.wait()
                raise
        lines = stdout.decode().strip().splitlines()
        if process.returncode != 0 or not lines:
            raise RuntimeError(f"the jev-ultrafast runner exited {process.returncode} without a result")
        report = json.loads(lines[-1])
    browser = sum(line.dollars or 0 for line in cloud.cost)
    report["seconds"] = round(booted + float(cast(float, report["seconds"])), 2)
    report["cost_by_component"] = {
        "jev": report["jev_dollars"],
        "llm": report["text_dollars"],
        "browser": round(browser, 5),
    }
    dollars = float(cast(float, report["jev_dollars"])) + float(cast(float, report["text_dollars"])) + browser
    report["dollars"] = None if report["unmetered_requests"] else round(dollars, 5)
    controls = report.get("controls")
    observed = (
        None if controls is None else tuple((str(label), value) for label, value in cast(list[list[Any]], controls))
    )
    return Outcome(None, None, cast(str | None, report["final_url"]), controls=observed), report


async def hosted_arm(
    task: LiveTask, http: httpx.AsyncClient, *, record: Path | None
) -> tuple[Outcome, dict[str, object]]:
    from browser_use_sdk.v3 import AsyncBrowserUse  # pyright: ignore[reportMissingTypeStubs] - optional extra

    client = AsyncBrowserUse(api_key=load_settings().browser_key())
    run = client.run(
        f"Start at {task.start}. {task.task}",
        output_schema=task.output_schema,
        max_cost_usd=MAX_DOLLARS,
        proxy_country_code="us",
        sensitive_data=dict(task.secrets) or None,
        enable_recording=record is not None,
    )
    started = time.monotonic()
    finishing = asyncio.ensure_future(run)
    # The session id appears once the SDK has created the session, which is when its live URL exists.
    while run.session_id is None and not finishing.done():
        await asyncio.wait({finishing}, timeout=0.2)
    if run.session_id is not None:
        _watch("hosted", task, (await client.sessions.get(run.session_id)).live_url)
    remaining = MAX_SECONDS - (time.monotonic() - started)
    # wait, not wait_for: wait_for re-raised the SDK's schema error here, before the session's cost was read, and
    # six capped structured sessions were recorded with no cost or status.
    done, _ = await asyncio.wait({finishing}, timeout=max(remaining, 1))
    timed_out = not done
    if timed_out and run.session_id is not None:
        await client.sessions.stop(run.session_id)
    await asyncio.gather(finishing, return_exceptions=True)
    seconds = time.monotonic() - started
    if (error := finishing.exception()) is None:
        result = finishing.result()
        session, output = result.session, result.output
    elif run.session_id is not None:
        # The SDK raises on output that fails the task's schema; the session still holds that output and its cost.
        session = await client.sessions.get(run.session_id)
        output = session.output
    else:
        raise error
    if isinstance(output, BaseModel):
        outcome = Outcome(output.model_dump_json(), output.model_dump(), None)
    elif isinstance(output, dict):
        outcome = Outcome(json.dumps(output), cast(dict[str, object], output), None)
    else:
        outcome = Outcome(str(output) if output else None, None, None)
    cost = session.total_cost_usd
    report: dict[str, object] = {
        "status": "timed_out" if timed_out else session.status.value,
        "seconds": round(seconds, 2),
        "dollars": None if cost is None else float(cost),
        "model": str(session.model.value if hasattr(session.model, "value") else session.model),
        "steps": session.step_count,
        "session_id": str(session.id),
    }
    if record is not None:
        urls = await client.sessions.wait_for_recording(session.id, timeout=60)
        if urls:
            response = await http.get(urls[0], follow_redirects=True)
            if response.is_success:
                await asyncio.to_thread(record.write_bytes, response.content)
    return outcome, report


def _video(record: Path | None) -> str | None:
    return str(record) if record is not None and record.exists() else None


async def run_arm(
    arm: str,
    task: LiveTask,
    http: httpx.AsyncClient,
    downloads: Path,
    *,
    bitwarden: bool,
    record: Path | None,
) -> dict[str, object]:
    truth = await task.truth(http)
    started = time.monotonic()
    row: dict[str, object] = {"arm": arm, "task": task.id, "category": task.category.value, "at": time.time()}
    try:
        if arm == "fast":
            with _traced() as events:
                outcome, result, ended = await fast_arm(task, http, downloads, bitwarden=bitwarden, record=record)
            status = result.status.value
            cost = result.cost
            row |= {
                "error": result.error,
                "trace": [f"{s.operation.value} {s.target or ''} -> {s.outcome.value}" for s in result.steps],
                # Enough to say why a run failed without running it again: every step as the agent judged it, and
                # each read, done check, verification, claim check and recovery in order.
                "step_log": [s.model_dump(mode="json") for s in result.steps],
                "events": events,
                "steps": len(result.steps),
                "unknown_cost": cost.has_unknown,
                "seconds_by_call": cost.seconds_by_call(),
                "cost_by_component": {
                    c: round(sum(line.dollars or 0 for line in cost.lines if line.component == c), 5)
                    for c in {line.component.value for line in cost.lines}
                },
            }
            # An unknown line makes the known total a floor, not a cost.
            dollars: float | None = None if cost.has_unknown else cost.known_dollars
            seconds = (ended or time.monotonic()) - started
        elif arm == "ultrafast":
            outcome, report = await ultrafast_arm(task, http, record=record)
            status = str(report.pop("status"))
            dollars = cast(float | None, report.pop("dollars"))
            seconds = float(cast(float, report.pop("seconds")))
            row |= report
        else:
            outcome, report = await hosted_arm(task, http, record=record)
            status = str(report.pop("status"))
            dollars = cast(float | None, report.pop("dollars"))
            seconds = float(cast(float, report.pop("seconds")))
            row |= report
    except Exception as exc:  # a crashed arm is a failed task, recorded rather than aborting the comparison
        return row | {
            "passed": False,
            "correct": False,
            "failure": f"{type(exc).__name__}: {exc}",
            "seconds": round(time.monotonic() - started, 1),
            "dollars": None,
            "video": _video(record),
        }
    failure = task.check(outcome, truth)
    # Right and proven are graded apart: a correct answer the agent could not back with quotes is a
    # different defect from a wrong one, and one pass/fail column hid which the suite was showing.
    correct = failure is None
    expected = {"fast": task.expect.value, "ultrafast": "done"}.get(arm)
    if expected is not None and failure is None and status != expected:
        failure = f"status {status}, expected {expected}"
    return row | {
        "correct": correct,
        "passed": failure is None,
        "failure": failure,
        "status": status,
        "seconds": round(seconds, 1),
        "dollars": None if dollars is None else round(dollars, 5),
        "answer": outcome.answer,
        "data": outcome.data,
        "final_url": outcome.final_url,
        "video": _video(record),
    }


SUITES: dict[str, tuple[LiveTask, ...]] = {"core": TASKS, "dev": DEV, "heldout": HELDOUT}
"""`core` is the published suite; `dev` and `heldout` are the split in `more_tasks`."""


def summarize(rows: list[dict[str, object]], arms: list[str]) -> None:
    for arm in arms:
        arm_rows = [r for r in rows if r["arm"] == arm]
        if not arm_rows:
            continue
        passed = sum(bool(r["passed"]) for r in arm_rows)
        correct = sum(bool(r.get("correct")) for r in arm_rows)
        priced = [float(d) for r in arm_rows if isinstance(d := r.get("dollars"), int | float)]
        seconds = [float(s) for r in arm_rows if isinstance(s := r.get("seconds"), int | float)]
        unknown = len(arm_rows) - len(priced)
        print(
            f"{arm}: {passed}/{len(arm_rows)} passed, {correct} correct, median {statistics.median(seconds):.1f}s, "
            f"${sum(priced):.4f}" + (f" ({unknown} runs of unknown cost)" if unknown else "")
        )
        calls: dict[str, float] = {}
        for r in arm_rows:
            for label, spent in cast(dict[str, float], r.get("seconds_by_call", {})).items():
                calls[label] = calls.get(label, 0.0) + spent
        for label, spent in sorted(calls.items(), key=lambda item: -item[1]):
            print(f"  {label:18} {spent / len(arm_rows):5.1f}s a task")


async def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--only", nargs="*", default=[])
    parser.add_argument("--suite", nargs="*", default=["core"], choices=list(SUITES), help="task sets to run")
    parser.add_argument("--category", nargs="*", default=[], choices=[c.value for c in Category])
    parser.add_argument("--bitwarden", action="store_true", help="login credentials from the vault items")
    parser.add_argument("--arms", nargs="*", default=list(ARMS), choices=ARMS)
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--record", type=Path, metavar="DIR", help="save each run as DIR/<arm>/<task>-<n>.mp4")
    parser.add_argument("--out", type=Path, default=Path("artifacts/evals/live.jsonl"))
    args = parser.parse_args(argv)
    tasks = [
        t
        for suite in args.suite
        for t in SUITES[suite]
        if (not args.only or t.id in args.only) and (not args.category or t.category.value in args.category)
    ]
    if "ultrafast" in args.arms:
        await prepare_ultrafast()
    args.out.parent.mkdir(parents=True, exist_ok=True)
    rows: list[dict[str, object]] = []
    with tempfile.TemporaryDirectory() as downloads, args.out.open("a", encoding="utf-8") as out:
        async with httpx.AsyncClient(timeout=60) as http:
            for _ in range(args.repeat):
                for task in tasks:
                    for arm in args.arms:
                        if arm not in task.arms:
                            continue
                        record = None if args.record is None else video_path(args.record, arm, task)
                        row = await run_arm(arm, task, http, Path(downloads), bitwarden=args.bitwarden, record=record)
                        rows.append(row)
                        out.write(json.dumps(row, default=str) + "\n")
                        out.flush()
                        mark = "PASS" if row["passed"] else "FAIL"
                        print(
                            f"{mark} {arm:9} {task.id:20} {row.get('seconds')!s:>6}s ${row.get('dollars')!s:<8}",
                            row["failure"] or "",
                            flush=True,
                        )
    summarize(rows, args.arms)
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main(sys.argv[1:])))
