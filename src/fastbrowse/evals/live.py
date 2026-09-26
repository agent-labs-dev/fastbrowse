"""Head-to-head on live sites: fastbrowse, jev-ultrafast and the Browser Use agent, same prompts.

    uv run --extra browser-use python -m fastbrowse.evals.live [--only TASK_ID ...] [--category CATEGORY ...]
        [--arms fastbrowse jev-ultrafast browser-use] [--bitwarden] [--repeat N] [--record DIR]
        [--out artifacts/evals/live.jsonl]

Needs BROWSER_USE_API_KEY (every arm), and the Jev and LLM keys in fastbrowse.clients.environment (fastbrowse and
jev-ultrafast arms). Each run prints a WATCH line with the URL where its browser can be watched live.

The fastbrowse and jev-ultrafast arms each drive a fresh Browser Use Cloud browser;
the Browser Use agent brings its own.
jev-ultrafast runs as its published package in an environment of its own (see scripts/ultrafast_arm.py).

Tasks and their grading live in fastbrowse.evals.live_tasks. With --bitwarden, the fastbrowse arm reads each login
task's credentials from its vault item (created by scripts/eval_vault.py) instead of the task. With --record,
each run is saved as DIR/<arm>/<task>-<n>.mp4, n counting up from 1 past any video already there.

Each task runs only on the arms it grades on equal terms (LiveTask.arms). Answer tasks compare fastbrowse with hosted
Browser Use; jev-ultrafast returns no answer, only DONE or BLOCKED. Navigation tasks, graded on the page the
run ended on, compare fastbrowse with jev-ultrafast; the hosted SDK does not say where its browser ended.
fastbrowse must also end with the task's expected status, and jev-ultrafast with DONE.
"""

import argparse
import asyncio
import itertools
import json
import logging
import os
import statistics
import sys
import tempfile
import time
from collections import Counter
from collections.abc import Awaitable, Callable, Mapping
from contextvars import ContextVar
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any, Literal
from unittest import mock

import httpx
from pydantic import BaseModel, ConfigDict, JsonValue

import fastbrowse.run
from fastbrowse.adapters.bitwarden import bitwarden_login
from fastbrowse.adapters.browser_use_cloud import BrowserUseCloudBrowser
from fastbrowse.agent import Agent
from fastbrowse.clients.environment import load_settings
from fastbrowse.clients.validation import RETRYABLE_STATUS, TRANSIENT_TRANSPORT
from fastbrowse.evals.live_tasks import TASKS, Category, LiveTask, Outcome, prompt
from fastbrowse.evals.more_tasks import DEV, HELDOUT, STRETCH_DEV, STRETCH_HELDOUT
from fastbrowse.evals.observe import GradedPage as _GradedPage
from fastbrowse.evals.observe import observe_browser
from fastbrowse.evals.status import Ending, normalize, status_matches
from fastbrowse.evals.versions import load_lock, provenance, suite_version, task_version
from fastbrowse.jev import JEV_DOLLARS_PER_INPUT_TOKEN
from fastbrowse.models import Authorization, BrowserEvent, Limits, RunResult, Status, StepEvent, Unavailable
from fastbrowse.run import run_task
from fastbrowse.safety import ScopedSecrets, origin_of
from fastbrowse.telemetry import traced, transient_seconds

MAX_STEPS = 50
LIMITS = Limits(max_steps=MAX_STEPS)
"""The existing harness has a step cap only; the benchmark time and dollar caps need separate metering work."""

ULTRAFAST = "jev-ultrafast @ git+https://github.com/browser-use/jev-ultrafast@1231850a0bf1a0c0341fe408ef1668dbbfdfac46"
ULTRAFAST_RUNNER = Path(__file__).resolve().parents[3] / "scripts" / "ultrafast_arm.py"
ULTRAFAST_COMMAND = ("uv", "run", "--no-project", "--quiet", "--python", "3.14", "--with", ULTRAFAST, "python")
ULTRAFAST_TEXT_MODEL = "inception/mercury-2.5"
"""jev-ultrafast's own configuration (.env.example): its text helper on OpenRouter, reasoning off."""
OUTAGE_RETRIES = 5
"""Runs of a row a provider outage ended, after the first, waiting 1, 2, 4, 8 then 10 minutes: about 25 minutes, past
the 503 spells seen so far. A row still unavailable then is recorded, and left out of every published figure."""


def _watch(arm: str, task: LiveTask, live_url: str | None) -> None:
    if live_url:
        print(f"WATCH {arm:13} {task.id:18} {live_url}", flush=True)


def _secrets(task: LiveTask, bitwarden: bool) -> ScopedSecrets | None:
    origin = origin_of(task.start)
    if bitwarden and task.bitwarden_item is not None:
        return ScopedSecrets(bitwarden_login(task.bitwarden_item, origin), origin)
    return ScopedSecrets(task.secrets, origin) if task.secrets else None


def video_path(folder: Path, arm: str, task: LiveTask) -> Path:
    """The first free DIR/<arm>/<task>-<n>.mp4, absolute because the ultrafast runner has its own working directory.

    Counting past existing files means repeated invocations never overwrite a video. The name is claimed by creating
    it empty: every repeat is allocated before any run writes, so an existence check alone gave them all one name.
    """
    (folder.resolve() / arm).mkdir(parents=True, exist_ok=True)
    n = 1
    while True:
        path = folder.resolve() / arm / f"{task.id}-{n}.mp4"
        try:
            path.touch(exist_ok=False)
        except FileExistsError:
            n += 1
        else:
            return path


@dataclass
class _Observed:
    """What the harness sees of one fastbrowse run beyond its result. One per run: eight runs overlap, and state
    kept on the classes let a run starting reset the page another had just observed, which then graded as
    "no final page to grade"."""

    final_url: str | None = None
    controls: tuple[tuple[str, str | None], ...] | None = None
    observe_error: str | None = None
    """Why the page could not be observed after the run; a grader then reports it had no page."""
    ended: float | None = None
    """When the run ended, so the recording's result card is not timed as the run."""


_observed: ContextVar[_Observed] = ContextVar("live_observed")


class _ObservedAgent(Agent):
    """fastbrowse's agent, with the page it ended on observed once more after the run, for graders that read the
    page itself (what a form ended up holding) rather than anything the agent reported."""

    async def run(self, *args: Any, **kwargs: Any) -> RunResult:
        result = await super().run(*args, **kwargs)
        _observed.get().ended = time.monotonic()
        try:
            observation = (
                await self._page.observe_all() if isinstance(self._page, _GradedPage) else await self._page.observe()
            )
        except Exception as exc:  # a page that cannot be observed leaves nothing to grade, which the grader reports
            _observed.get().observe_error = f"{type(exc).__name__}: {exc}"
            return result
        _observed.get().final_url = observation.url
        _observed.get().controls = tuple((c.label, c.value) for c in observation.controls)
        return result


_running: ContextVar[str] = ContextVar("live_running", default="-")
"""`<arm> <task>` for the run a log record came from: eight runs overlap, and a bare retry warning names none."""


class _NameRun(logging.Filter):
    def filter(self, record: logging.LogRecord) -> bool:
        record.run = _running.get()
        return True


class ArmReport(BaseModel):
    """What an arm says about one run beyond its outcome; a field belongs to the arms named beside it."""

    status: str
    task_successful: bool | None = None
    seconds: float
    """Time to the answer, including browser setup; the hosted arm ends at its agent's `done` (`session_seconds`)."""
    transient_seconds: float = 0.0
    """Provider outage waits fastbrowse measured inside the run; every published time leaves them out."""
    dollars: float | None
    """None when some of the run's spend could not be priced: a known total would then be only a floor."""
    error: str | None = None
    steps: int | None = None
    trace: list[str] = []
    cost_by_component: dict[str, float] = {}
    # fastbrowse
    would_fire: dict[str, int] = {}
    citations: list[JsonValue] = []
    step_log: list[JsonValue] = []
    events: list[object] = []
    unknown_cost: bool | None = None
    observe_error: str | None = None
    seconds_by_call: dict[str, float] = {}
    # jev-ultrafast
    actions: int | None = None
    unmetered_requests: int | None = None
    text_model: str | None = None
    # the Browser Use agent
    model: str | None = None
    session_id: str | None = None
    answered: bool | None = None
    """Its agent called `done` with a result that was not an error: its own completion, as fastbrowse's is."""
    session_seconds: float | None = None
    """Until the API reported the session stopped, which can be well after the answer; `seconds` ends at the answer."""


class EvalRow(ArmReport):
    """One line of the results file: the arm's report, graded against the task's truth."""

    arm: str
    task: str
    category: str
    at: float
    concurrency: int | None = None
    status: str | None = None  # a crashed arm reports nothing, unless a provider was unavailable
    retries: int = 0
    """Runs discarded before this one because a provider stayed unavailable: they say nothing about the agent."""
    normalized_status: Ending = Ending.ERROR
    correct: bool
    passed: bool
    failure: str | None
    answer: str | None = None
    data: object = None
    final_url: str | None = None
    video: str | None = None
    suite: str | None = None
    suite_version: str | None = None
    task_version: int | None = None
    """Rows compare only at equal task versions; see fastbrowse.evals.versions."""
    run: dict[str, JsonValue] = {}
    """The invocation's provenance, the same on every row it wrote: build, commit, providers, run id."""


class _UltrafastReport(BaseModel):
    """The jev-ultrafast runner's result line (`scripts/ultrafast_arm.py`)."""

    status: str
    error: str | None
    seconds: float
    steps: int
    actions: int
    trace: list[str]
    jev_dollars: float
    text_dollars: float
    unmetered_requests: int
    text_model: str | None


async def fast_arm(
    task: LiveTask,
    http: httpx.AsyncClient,
    downloads: Path,
    *,
    bitwarden: bool,
    record: Path | None,
) -> tuple[Outcome, RunResult, _Observed]:
    seen = _Observed()
    token = _observed.set(seen)
    try:
        result = await run_task(
            prompt(task),
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
    finally:
        _observed.reset(token)
    quotes = tuple((e.url, e.quote) for e in result.evidence)
    outcome = Outcome(result.answer, result.data, seen.final_url, quotes, seen.controls)
    return outcome, result, seen


async def _on_fast_event(task: LiveTask, event: StepEvent | BrowserEvent) -> None:
    if isinstance(event, BrowserEvent):
        _watch("fastbrowse", task, event.live_url)


async def prepare_ultrafast() -> None:
    """Install jev-ultrafast's environment once, so no run is timed installing it."""
    with tempfile.TemporaryDirectory(prefix="ultra-prepare-") as runtime:
        process = await asyncio.create_subprocess_exec(
            *ULTRAFAST_COMMAND, "-c", "import jev_ultrafast", env=arm_environment("jev-ultrafast", runtime), cwd=runtime
        )
        if await process.wait() != 0:
            raise RuntimeError(f"could not install {ULTRAFAST}")


def _ultrafast_env(cdp_ws: str, runtime: str) -> dict[str, str]:
    settings = load_settings()
    env = arm_environment("jev-ultrafast", runtime)
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


async def ultrafast_arm(task: LiveTask, http: httpx.AsyncClient, *, record: Path | None) -> tuple[Outcome, ArmReport]:
    started = time.monotonic()
    cloud = BrowserUseCloudBrowser(load_settings().browser_key(), http=http)
    async with cloud:
        booted = time.monotonic() - started
        _watch("jev-ultrafast", task, cloud.connection.live_url)
        request = {
            "start": task.start,
            "goal": prompt(task),
            "cdp_ws": cloud.connection.cdp_url,
            "max_steps": MAX_STEPS,
            "record": None if record is None else str(record),
            "jev_dollars_per_input_token": JEV_DOLLARS_PER_INPUT_TOKEN,
        }
        with tempfile.TemporaryDirectory(prefix="bh-") as runtime:
            ran = _UltrafastReport.model_validate_json(
                await _invoke(
                    ULTRAFAST_COMMAND,
                    ULTRAFAST_RUNNER,
                    request,
                    _ultrafast_env(cloud.connection.cdp_url, runtime),
                    runtime,
                )
            )
        final = await observe_browser(cloud.connection.cdp_url)
    browser = sum(line.dollars or 0 for line in cloud.cost)
    dollars = ran.jev_dollars + ran.text_dollars + browser
    report = ArmReport(
        status=ran.status,
        seconds=round(booted + ran.seconds, 2),
        dollars=None if ran.unmetered_requests else round(dollars, 5),
        error=ran.error,
        steps=ran.steps,
        trace=ran.trace,
        cost_by_component={"jev": ran.jev_dollars, "llm": ran.text_dollars, "browser": round(browser, 5)},
        actions=ran.actions,
        unmetered_requests=ran.unmetered_requests,
        text_model=ran.text_model,
        observe_error=final.error,
    )
    return Outcome(None, None, final.url, controls=final.controls), report


async def hosted_arm(task: LiveTask, http: httpx.AsyncClient, *, record: Path | None) -> tuple[Outcome, ArmReport]:
    from browser_use_sdk.v3 import BrowserUseError  # an optional extra

    try:
        return await _hosted_run(task, http, record=record)
    # The SDK's message quotes the response body, which can echo the key: report the status or type alone.
    except BrowserUseError as error:
        failed = Unavailable if error.status_code in RETRYABLE_STATUS else RuntimeError
        raise failed(f"Browser Use API returned HTTP {error.status_code}") from None
    except httpx.HTTPError as error:
        failed = Unavailable if isinstance(error, TRANSIENT_TRANSPORT) else RuntimeError
        raise failed(f"Browser Use API request failed ({type(error).__name__})") from None


async def _down(task: LiveTask, http: httpx.AsyncClient) -> str | None:
    """Why the task's site is down, or None. A site serving errors (Heroku's Application Error is a 503) fails
    every arm alike and measures none, so a run it failed is an outage to wait out, not the agent's failure."""
    try:
        response = await http.get(task.start, follow_redirects=True, timeout=20)
    except httpx.TransportError as error:
        return f"site unavailable: {task.start} unreachable ({type(error).__name__})"
    if response.status_code >= 500:
        return f"site unavailable: {task.start} answered HTTP {response.status_code}"
    return None


async def hosted_answer(client: Any, session_id: str, output: object) -> datetime | None:
    """When the hosted agent answered, or None if it never did: its last `done` result that was not an error. An
    agent that answers without calling `done` (0.5.7's github-license, among others) replies in a message, which
    the session keeps as its output; that last reply is its answer."""
    done: datetime | None = None
    reply: datetime | None = None
    called_done, after = False, None
    for _ in range(100):
        page = await client.sessions.messages(session_id, limit=100, **({"after": after} if after else {}))
        for message in page.messages:
            if message.type == "completion_result":
                called_done = True
                if not json.loads(message.data or "{}").get("is_error"):
                    done = max(done or message.created_at, message.created_at)
            elif message.type == "assistant_message":
                reply = max(reply or message.created_at, message.created_at)
        if not page.messages or not getattr(page, "has_more", False):
            break
        after = str(page.messages[-1].id)
    return done if called_done else reply if output else None


def answer_seconds(created_after: float, session: Any, answered: datetime | None, wall: float) -> float:
    """Time to the hosted agent's answer: the client's wait for the session to exist, then the server's own clock
    from its creation to the answer. The session reports stopped as much as two minutes after that, on Browser Use's
    side, while fastbrowse's clock stops when its run returns its answer."""
    if answered is None:
        return wall
    return min(wall, created_after + max(0.0, (answered - session.created_at).total_seconds()))


async def _hosted_run(task: LiveTask, http: httpx.AsyncClient, *, record: Path | None) -> tuple[Outcome, ArmReport]:
    from browser_use_sdk.v3 import AsyncBrowserUse, BrowserUseError  # an optional extra

    client = AsyncBrowserUse(api_key=load_settings().browser_key())
    run = client.run(
        prompt(task),
        output_schema=task.output_schema,
        proxy_country_code="us",
        sensitive_data=dict(task.secrets) or None,
        enable_recording=record is not None,
    )
    started = time.monotonic()
    finishing = asyncio.ensure_future(run)
    # The session id appears once the SDK has created the session, which is when its live URL exists.
    while run.session_id is None and not finishing.done():
        await asyncio.wait({finishing}, timeout=0.2)
    created_after = time.monotonic() - started
    if run.session_id is not None:
        _watch("browser-use", task, (await client.sessions.get(run.session_id)).live_url)
    # gather, not await: the SDK raises on output that fails the task's schema, before the session's cost is read.
    await asyncio.gather(finishing, return_exceptions=True)
    seconds = time.monotonic() - started
    if (error := finishing.exception()) is None:
        result = finishing.result()
        session, output = result.session, result.output
    elif run.session_id is not None and not isinstance(error, BrowserUseError | httpx.HTTPError):
        # The SDK raises on output that fails the task's schema; the session still holds that output and its cost.
        # An API or transport failure is raised instead: the session it interrupted says nothing of the agent.
        session = await client.sessions.get(run.session_id)
        output = session.output
    else:
        raise error
    if session.status.value == "error":
        # Browser Use's own infrastructure ending the session ("Task ended unexpectedly."), not its agent giving up
        # or answering wrong: it appeared in none of 0.5.7's sessions and in 13 of one day's 114.
        raise Unavailable("Browser Use ended the session in error")
    answered = await hosted_answer(client, str(session.id), output)
    if isinstance(output, BaseModel):
        outcome = Outcome(output.model_dump_json(), output.model_dump(), None, unobservable=True)
    elif isinstance(output, dict):
        outcome = Outcome(json.dumps(output), output, None, unobservable=True)
    else:
        outcome = Outcome(str(output) if output else None, None, None, unobservable=True)
    cost = session.total_cost_usd
    report = ArmReport(
        status=session.status.value,
        task_successful=getattr(session, "is_task_successful", None),
        answered=answered is not None,
        seconds=round(answer_seconds(created_after, session, answered, seconds), 2),
        session_seconds=round(seconds, 2),
        dollars=None if cost is None else float(cost),
        model=str(session.model.value if hasattr(session.model, "value") else session.model),
        steps=session.step_count,
        session_id=str(session.id),
    )
    if record is not None:
        urls = await client.sessions.wait_for_recording(session.id, timeout=60)
        if urls:
            response = await http.get(urls[0], follow_redirects=True)
            if response.is_success:
                await asyncio.to_thread(record.write_bytes, response.content)
    return outcome, report


def _video(record: Path | None) -> str | None:
    # A claimed name stays empty when the run recorded nothing.
    return str(record) if record is not None and record.exists() and record.stat().st_size else None


async def _github_token(request: httpx.Request) -> None:
    # An answer key is fetched again on every retry, and a Jev outage retries rows for hours, so the anonymous
    # 60 an hour ran out and failed github-license on a 403. A token raises that to 5000.
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    if token and request.url.host == "api.github.com":
        request.headers["Authorization"] = f"Bearer {token}"


async def _truth(task: LiveTask, http: httpx.AsyncClient) -> object:
    # Answer keys come from public APIs that rate-limit, so a transient failure is waited out, never a crashed eval.
    for retries in itertools.count():
        try:
            return await task.truth(http)
        except httpx.HTTPError as exc:
            status = exc.response.status_code if isinstance(exc, httpx.HTTPStatusError) else None
            if status not in RETRYABLE_STATUS and not isinstance(exc, TRANSIENT_TRANSPORT):
                raise
            wait = min(30 * (retries + 1), 300)
            print(f"RETRY truth         {task.id:20} in {wait}s: {type(exc).__name__} {status or ''}", flush=True)
            await asyncio.sleep(wait)


def _crashed(
    arm: str, task: LiveTask, failure: str, *, at: float, seconds: float, status: str | None, record: Path | None
) -> EvalRow:
    return EvalRow(
        arm=arm,
        task=task.id,
        category=task.category.value,
        at=at,
        status=status,
        normalized_status=normalize(status),
        correct=False,
        passed=False,
        failure=failure,
        seconds=round(seconds, 1),
        dollars=None,
        video=_video(record),
    )


def _slow_jev(events: list[object]) -> float:
    """The slowest Jev call past `JEV_SLOW_SECONDS` in a run's trace, or 0."""
    return max(
        (float(e["seconds"]) for e in events if isinstance(e, dict) and e.get("event") == "request_slow"), default=0.0
    )


async def run_arm(
    arm: str,
    task: LiveTask,
    truth: object,
    http: httpx.AsyncClient,
    downloads: Path,
    *,
    bitwarden: bool,
    record: Path | None,
) -> EvalRow:
    _running.set(f"{arm} {task.id}")
    started = time.monotonic()
    at = time.time()
    try:
        outcome, report = await ARMS[arm].runner(
            task, http, downloads, bitwarden=bitwarden, record=record, started=started
        )
    except Exception as exc:  # a crashed arm is a failed task, recorded rather than aborting the comparison
        unavailable = isinstance(exc, (Unavailable, *TRANSIENT_TRANSPORT))
        return _crashed(
            arm,
            task,
            f"{type(exc).__name__}: {exc}",
            at=at,
            seconds=time.monotonic() - started,
            status=Status.UNAVAILABLE.value if unavailable else None,
            record=record,
        )
    try:
        failure = task.check(outcome, truth)
    except Exception as exc:
        # One task's grader must not discard every other run in the suite: gather propagates, and a 114-run
        # pass is an hour and real money. A grader that raises is that row's failure and nobody else's.
        failure = f"check raised {type(exc).__name__}: {exc}"
    # Right and proven are graded apart: a correct answer the agent could not back with quotes is a
    # different defect from a wrong one, and one pass/fail column hid which the suite was showing.
    correct = failure is None
    if failure is None and not status_matches(arm, report.status, task.expect, bool(report.answered)):
        failure = f"status {report.status}, expected {task.expect.value}"
    ending = normalize(report.status, hosted=arm == "browser-use", answered=bool(report.answered))
    if slow := _slow_jev(report.events):
        # Jev answers in under a second; a slower call is its outage, and the attempt is run again, never scored.
        ending, failure = Ending.UNAVAILABLE, f"Jev unavailable: a call took {slow:.1f}s"
    return EvalRow.model_validate(
        report.model_dump()
        | {
            "arm": arm,
            "normalized_status": ending,
            "task": task.id,
            "category": task.category.value,
            "at": at,
            "correct": correct,
            "passed": failure is None,
            "failure": failure,
            "seconds": round(report.seconds, 1),
            "dollars": None if report.dollars is None else round(report.dollars, 5),
            "answer": outcome.answer,
            "data": outcome.data,
            "final_url": outcome.final_url,
            "video": _video(record),
        }
    )


async def _fast_report(
    task: LiveTask, http: httpx.AsyncClient, downloads: Path, *, bitwarden: bool, record: Path | None, started: float
) -> tuple[Outcome, ArmReport]:
    with traced() as events:
        outcome, result, seen = await fast_arm(task, http, downloads, bitwarden=bitwarden, record=record)
    cost = result.cost
    settings = load_settings()
    ended = seen.ended or time.monotonic()
    lost = transient_seconds(events, started, ended)
    return outcome, ArmReport(
        status=result.status.value,
        seconds=ended - started,
        transient_seconds=round(lost, 2),
        # An unknown line makes the known total a floor, not a cost.
        dollars=None if cost.has_unknown else cost.known_dollars,
        # A shadow tripwire only earns arming on evidence from LIVE sites: the local fixtures never
        # grind and never spin, so a zero there says nothing about the rate that matters.
        would_fire=dict(Counter(tripwire.value for tripwire in result.would_fire)),
        error=result.error,
        citations=[citation.model_dump(mode="json") for citation in result.citations],
        trace=[f"{s.operation.value} {s.target or ''} -> {s.outcome.value}" for s in result.steps],
        # Enough to say why a run failed without running it again: every step as the agent judged it, and
        # each read, done check, verification, claim check and recovery in order.
        step_log=[s.model_dump(mode="json") for s in result.steps],
        events=events,
        steps=len(result.steps),
        unknown_cost=cost.has_unknown,
        observe_error=seen.observe_error,
        seconds_by_call=cost.seconds_by_call(),
        model=f"jev {settings.jev_route()[0]}",
        text_model=", ".join(sorted(set(settings.models().values()))),
        cost_by_component={
            c: round(sum(line.dollars or 0 for line in cost.lines if line.component == c), 5)
            for c in {line.component.value for line in cost.lines}
        },
    )


class ArmSpec(BaseModel):
    model_config = ConfigDict(arbitrary_types_allowed=True, frozen=True)

    runner: Callable[..., Awaitable[tuple[Outcome, ArmReport]]]
    pin: str
    env_allowlist: tuple[str, ...]
    tier: Literal["A", "hosted"]
    default: bool = True
    prepare: Callable[[], Awaitable[None]] | None = None


_RUNTIME_ENV = ("PATH", "LANG", "LC_ALL", "SSL_CERT_FILE", "SSL_CERT_DIR")
OSS_PIN = "browser-use==0.13.10"
OSS_COMMAND = ("uv", "run", "--no-project", "--quiet", "--with", OSS_PIN, "python")
OSS_RUNNER = ULTRAFAST_RUNNER.with_name("browser_use_oss_arm.py")
OSS_MODEL = "google/gemini-3.8-flash"


def arm_environment(name: str, runtime: str, source: Mapping[str, str] | None = None) -> dict[str, str]:
    source = os.environ if source is None else source
    # HOME is a fresh directory, so uv's cache and managed Pythons are pointed at where they really are: without
    # them each run would resolve cold and download its interpreter again.
    cache = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache"))
    data = Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local" / "share"))
    return {key: source[key] for key in ARMS[name].env_allowlist if key in source} | {
        "HOME": runtime,
        "UV_CACHE_DIR": str(Path(os.environ.get("UV_CACHE_DIR", cache / "uv")).resolve()),
        "UV_PYTHON_INSTALL_DIR": str(Path(os.environ.get("UV_PYTHON_INSTALL_DIR", data / "uv" / "python")).resolve()),
        "ANONYMIZED_TELEMETRY": "false",
    }


async def _invoke(
    command: tuple[str, ...], script: Path, request: Mapping[str, object], env: dict[str, str], cwd: str
) -> bytes:
    process = await asyncio.create_subprocess_exec(
        *command, str(script), stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, env=env, cwd=cwd
    )
    try:
        stdout, _ = await process.communicate(json.dumps(request).encode())
    except BaseException:
        if process.returncode is None:
            process.kill()
        await process.wait()
        raise
    lines = stdout.splitlines()
    if process.returncode != 0 or not lines:
        raise RuntimeError(f"{script.name} exited {process.returncode} without a result")
    return lines[-1]


class _OssReport(BaseModel):
    status: str
    answer: str | None = None
    data: JsonValue = None
    steps: int
    seconds: float
    dollars: float | None = None
    error: str | None = None


async def oss_arm(task: LiveTask, http: httpx.AsyncClient, *, record: Path | None) -> tuple[Outcome, ArmReport]:
    if record is not None:
        raise ValueError("browser-use-oss recording is not supported by this adapter")
    started = time.monotonic()
    cloud = BrowserUseCloudBrowser(load_settings().browser_key(), http=http)
    async with cloud:
        booted = time.monotonic() - started
        _watch("browser-use-oss", task, cloud.connection.live_url)
        request: dict[str, object] = {
            "start": task.start,
            "goal": prompt(task),
            "cdp_ws": cloud.connection.cdp_url,
            "max_steps": MAX_STEPS,
            "record": None,
            "model": OSS_MODEL,
            "secrets": dict(task.secrets),
            "output_schema": task.output_schema.model_json_schema() if task.output_schema else None,
        }
        with tempfile.TemporaryDirectory(prefix="bu-oss-") as runtime:
            env = arm_environment("browser-use-oss", runtime)
            env["OPENROUTER_API_KEY"] = load_settings().openrouter_key()
            ran = _OssReport.model_validate_json(await _invoke(OSS_COMMAND, OSS_RUNNER, request, env, runtime))
        final = await observe_browser(cloud.connection.cdp_url)
    browser = sum(line.dollars or 0 for line in cloud.cost)
    return Outcome(ran.answer, ran.data, final.url, controls=final.controls), ArmReport(
        status=ran.status,
        # Timed inside the runner as jev-ultrafast is, so neither arm is charged for resolving and importing itself.
        seconds=round(booted + ran.seconds, 2),
        dollars=None if ran.dollars is None else ran.dollars + browser,
        steps=ran.steps,
        error=ran.error,
        model=OSS_MODEL,
        observe_error=final.error,
        cost_by_component={"browser": browser} | ({} if ran.dollars is None else {"llm": ran.dollars}),
    )


async def prepare_oss() -> None:
    with tempfile.TemporaryDirectory(prefix="bu-prepare-") as runtime:
        process = await asyncio.create_subprocess_exec(
            *OSS_COMMAND, "-c", "import browser_use", env=arm_environment("browser-use-oss", runtime), cwd=runtime
        )
        if await process.wait() != 0:
            raise RuntimeError(f"could not install {OSS_PIN}")


async def _run_fast(
    task: LiveTask, http: httpx.AsyncClient, downloads: Path, **kwargs: Any
) -> tuple[Outcome, ArmReport]:
    return await _fast_report(task, http, downloads, **kwargs)


async def _run_ultra(task: LiveTask, http: httpx.AsyncClient, _: Path, **kwargs: Any) -> tuple[Outcome, ArmReport]:
    return await ultrafast_arm(task, http, record=kwargs["record"])


async def _run_hosted(task: LiveTask, http: httpx.AsyncClient, _: Path, **kwargs: Any) -> tuple[Outcome, ArmReport]:
    return await hosted_arm(task, http, record=kwargs["record"])


async def _run_oss(task: LiveTask, http: httpx.AsyncClient, _: Path, **kwargs: Any) -> tuple[Outcome, ArmReport]:
    return await oss_arm(task, http, record=kwargs["record"])


async def _prepare_ultra() -> None:
    await prepare_ultrafast()


ARMS: dict[str, ArmSpec] = {
    "fastbrowse": ArmSpec(runner=_run_fast, pin="run.fastbrowse_version + run.git_sha", env_allowlist=(), tier="A"),
    "jev-ultrafast": ArmSpec(
        runner=_run_ultra, pin=ULTRAFAST, env_allowlist=_RUNTIME_ENV, tier="A", prepare=_prepare_ultra
    ),
    "browser-use": ArmSpec(
        runner=_run_hosted,
        pin="browser-use-sdk==3.11.3; hosted model reported per row",
        env_allowlist=(),
        tier="hosted",
    ),
    "browser-use-oss": ArmSpec(
        runner=_run_oss, pin=OSS_PIN, env_allowlist=_RUNTIME_ENV, tier="A", default=False, prepare=prepare_oss
    ),
}


def eligible(arm: str, task: LiveTask) -> bool:
    return arm in task.arms or (arm == "browser-use-oss" and task.expect == Status.COMPLETE)


SUITES: dict[str, tuple[LiveTask, ...]] = {
    "core": TASKS,
    "dev": DEV,
    "heldout": HELDOUT,
    "stretch-dev": STRETCH_DEV,
    "stretch-heldout": STRETCH_HELDOUT,
}
"""`core` is the published suite; the others are the splits in `more_tasks`."""


def summarize(rows: list[EvalRow], arms: list[str]) -> None:
    for arm in arms:
        ran = [r for r in rows if r.arm == arm]
        # Outage rows say nothing about the agent: left out here as in every published figure.
        arm_rows = [r for r in ran if r.normalized_status != Ending.UNAVAILABLE]
        if not arm_rows:
            if ran:
                print(f"{arm}: all {len(ran)} runs ended by a provider outage")
            continue
        passed = sum(r.passed for r in arm_rows)
        correct = sum(r.correct for r in arm_rows)
        priced = [r.dollars for r in arm_rows if r.dollars is not None]
        seconds = [max(0.0, r.seconds - r.transient_seconds) for r in arm_rows]
        unknown = len(arm_rows) - len(priced)
        print(
            f"{arm}: {passed}/{len(arm_rows)} passed, {correct} correct, median {statistics.median(seconds):.1f}s, "
            f"${sum(priced):.4f}" + (f" ({unknown} runs of unknown cost)" if unknown else "")
        )
        if excluded := len(ran) - len(arm_rows):
            print(f"  {excluded} runs ended by a provider outage, excluded")
        if lost := sum(r.transient_seconds for r in arm_rows):
            print(f"  {'transient':18} {lost / len(arm_rows):5.1f}s a task, left out of the median")
        calls: dict[str, float] = {}
        for r in arm_rows:
            for label, spent in r.seconds_by_call.items():
                calls[label] = calls.get(label, 0.0) + spent
        for label, spent in sorted(calls.items(), key=lambda item: -item[1]):
            print(f"  {label:18} {spent / len(arm_rows):5.1f}s a task")
        # RUNS affected, not fires, and only among the passing ones - that is the false-positive rate the
        # arming decision turns on. Summing fires reads like a rate and is not one: four repetitions inside
        # a single grinding run reported as "4 on 19" invites the reading "4 runs of 19", which is 5%
        # misread as 21%. A tripwire firing on a run that failed anyway costs nothing and is excluded.
        shadow: Counter[str] = Counter()
        for r in arm_rows:
            if r.passed:
                shadow.update(set(r.would_fire))
        for tripwire, runs in shadow.most_common():
            print(f"  would-fire {tripwire:18} {runs}/{passed} passing runs")
        for r in arm_rows:
            if not r.passed:
                print(f"  FAIL {r.task:22} {_cause(r)}")


def _cause(row: EvalRow) -> str:
    """Why a run failed, leading with how the run itself ended: a grader's "answer lacks X: None" only restates
    that a run which died on a provider outage had no answer."""
    if row.error and row.status is not None:
        return f"{row.status}: {row.error}" + (f" | graded: {row.failure}" if row.failure else "")
    return row.failure or f"status {row.status}"


async def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--only", nargs="*", default=[], help="task ids to run; an id the selection lacks is an error")
    parser.add_argument(
        "--suite", nargs="*", choices=list(SUITES), help="task sets to run (default core, or every suite with --only)"
    )
    parser.add_argument("--category", nargs="*", default=[], choices=[c.value for c in Category])
    parser.add_argument("--bitwarden", action="store_true", help="login credentials from the vault items")
    parser.add_argument(
        "--arms", nargs="*", default=[name for name, spec in ARMS.items() if spec.default], choices=list(ARMS)
    )
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--record", type=Path, metavar="DIR", help="save each run as DIR/<arm>/<task>-<n>.mp4")
    parser.add_argument("--out", type=Path, default=Path("artifacts/evals/live.jsonl"))
    parser.add_argument(
        "--concurrency",
        type=int,
        default=8,
        help="runs in flight at once (default 8). A run waits on pages rather than on this machine, so eight at a "
        "time timed the same as one at a time; compare arms only under the same setting.",
    )
    args = parser.parse_args(argv)
    if args.record is not None and "browser-use-oss" in args.arms:
        parser.error("browser-use-oss does not support --record")
    tasks = [
        t
        for suite in args.suite or (list(SUITES) if args.only else ["core"])
        for t in SUITES[suite]
        if (not args.only or t.id in args.only) and (not args.category or t.category.value in args.category)
    ]
    # A rerun meant to confirm one row must not "pass" by quietly running none of it.
    if missing := sorted(set(args.only) - {t.id for t in tasks}):
        parser.error(f"--only names tasks outside the selected suites and categories: {', '.join(missing)}")
    lock = load_lock()
    suite_of = {t.id: name for name, suite_tasks in SUITES.items() for t in suite_tasks}
    suite_versions = {name: suite_version((t.id for t in suite_tasks), lock) for name, suite_tasks in SUITES.items()}
    handler = logging.StreamHandler(sys.stdout)
    handler.addFilter(_NameRun())
    # Traces are collected per run at DEBUG and propagate here; only warnings belong on the console.
    handler.setLevel(logging.WARNING)
    handler.setFormatter(logging.Formatter("%(levelname)-7s %(run)s %(name)s: %(message)s"))
    logging.basicConfig(level=logging.WARNING, handlers=[handler])
    providers = load_settings().providers() if "fastbrowse" in args.arms else None
    if providers is not None:
        print(f"PROVIDERS {providers}", flush=True)
    run = provenance(
        providers=providers,
        jev_ultrafast=ULTRAFAST if "jev-ultrafast" in args.arms else None,
        arms={name: {"pin": ARMS[name].pin, "tier": ARMS[name].tier} for name in args.arms},
        argv=list(argv),
        concurrency=args.concurrency,
        max_steps=MAX_STEPS,
    )
    dirty = " (uncommitted changes)" if run["git_dirty"] else ""
    print(f"RUN {run['run_id']} fastbrowse {run['fastbrowse_version']} at {run['git_sha']}{dirty}", flush=True)
    for name in args.arms:
        if prepare := ARMS[name].prepare:
            await prepare()
    args.out.parent.mkdir(parents=True, exist_ok=True)
    rows: list[EvalRow] = []
    # Patched once for the whole run: overlapping runs each patching and restoring would interleave, and a run ending
    # would restore the originals under a run still going.
    patches = (
        mock.patch.object(fastbrowse.run, "Agent", _ObservedAgent),
        mock.patch.object(fastbrowse.run, "CdpPage", _GradedPage),
    )
    gate = asyncio.Semaphore(args.concurrency)
    with (
        patches[0],
        patches[1],
        tempfile.TemporaryDirectory() as downloads,
        args.out.open("a", encoding="utf-8") as out,
    ):
        async with httpx.AsyncClient(timeout=60, event_hooks={"request": [_github_token]}) as http:

            async def one(arm: str, task: LiveTask, record: Path | None) -> EvalRow:
                # An outage is waited out and the row run again; bounded, so a dead provider cannot hold a run forever.
                for retries in itertools.count():
                    # The answer key is read once the row holds its slot: read while it queued, a live key (the
                    # top story, the newest release) could move on before the run began.
                    async with gate:
                        try:
                            truth = await _truth(task, http)
                        except Exception as exc:
                            # An answer key that will not come back (a 403 from a rate-limited API, a body missing
                            # the field it is read from) fails this task alone: gather would discard every run.
                            failure = f"truth raised {type(exc).__name__}: {exc}"
                            row = _crashed(arm, task, failure, at=time.time(), seconds=0.0, status=None, record=None)
                            break
                        row = await run_arm(
                            arm, task, truth, http, Path(downloads), bitwarden=args.bitwarden, record=record
                        )
                    if (
                        not row.passed
                        and row.normalized_status != Ending.UNAVAILABLE
                        and (down := await _down(task, http))
                    ):
                        row = row.model_copy(update={"normalized_status": Ending.UNAVAILABLE, "failure": down})
                    if row.normalized_status != Ending.UNAVAILABLE or retries >= OUTAGE_RETRIES:
                        break
                    wait = min(60 * 2**retries, 600)
                    print(f"RETRY {arm:13} {task.id:20} in {wait}s: {_cause(row)}", flush=True)
                    await asyncio.sleep(wait)
                row = row.model_copy(
                    update={
                        "concurrency": args.concurrency,
                        "retries": retries,
                        "suite": suite_of[task.id],
                        "suite_version": suite_versions[suite_of[task.id]],
                        "task_version": task_version(task.id, lock),
                        "run": run,
                    }
                )
                # Trace records hold whatever a component logged, so anything JSON cannot hold is written as text.
                out.write(row.model_dump_json(fallback=str) + "\n")
                out.flush()
                mark = "PASS" if row.passed else "FAIL"
                print(
                    f"{mark} {arm:13} {task.id:20} {row.seconds!s:>6}s ${row.dollars!s:<8}",
                    "" if row.passed else _cause(row),
                    flush=True,
                )
                return row

            planned = [
                (arm, task, None if args.record is None else video_path(args.record, arm, task))
                for _ in range(args.repeat)
                for task in tasks
                for arm in args.arms
                if eligible(arm, task)
            ]
            rows = list(await asyncio.gather(*(one(arm, task, record) for arm, task, record in planned)))
    summarize(rows, args.arms)
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main(sys.argv[1:])))
