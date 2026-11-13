"""Head-to-head on live sites, for exactly three comparisons, each run as its own invocation with its two arms:
agent against agent (fastbrowse against the hosted Browser Use agent on API V4: browser-use on its default model and
browser-use-ultrafast on bu-ultrafast, plus jev-ultrafast, browser-harness and browser-use-oss, the open-source
browser-use 1.0 on its defaults), backend against backend (fastbrowse on the Browser Use Cloud browser against
fastbrowse-pool), and fastbrowse on its own backend against Jev Ultrafast (fastbrowse-pool against jev-ultrafast). Pass each comparison's two arms with --arms;
fastbrowse-pool needs the pool credentials, so it is opt-in like browser-use-oss, cua-codex and
browser-harness (Codex with Browser Harness, fastbrowse_evals/adapters/browser_harness_arm.py).

    uv run --extra browser python -m fastbrowse_evals.live [--only TASK_ID ...] [--category CATEGORY ...]
        [--arms fastbrowse fastbrowse-pool jev-ultrafast browser-use]
        [--judge-policy POLICY] [--repeat N] [--record DIR]
        [--out artifacts/evals/live.jsonl]

Needs BROWSER_USE_API_KEY (every arm but fastbrowse-pool), and OPENROUTER_API_KEY for Jev and the LLM (fastbrowse,
fastbrowse-pool and jev-ultrafast arms). The fastbrowse-pool arm also needs PARALLAX_POOL_URL and PARALLAX_POOL_TOKEN.
Each run prints a WATCH line with the URL where its browser can be watched live.

The fastbrowse and jev-ultrafast arms each drive a fresh Browser Use Cloud browser the harness starts
(`fastbrowse_evals.browsers`); the hosted Browser Use agent brings its own. fastbrowse-pool is the same fastbrowse
run on a browser leased from our own browser pool (fastbrowse-hosted's deploy/browser-pool), so fastbrowse against
fastbrowse-pool is a matched A/B of the browser backend alone. fastbrowse runs through its public `run_task` only,
attached to the harness's browser, with the keys and Jev route of `fastbrowse_evals.backend`; the harness reads
every arm's final page itself (`fastbrowse_evals.final_page`).
jev-ultrafast runs as its published package in an environment of its own (see fastbrowse_evals/adapters/ultrafast_arm.py).

Every run gets `AGENT_LIMITS`, which has no step, time, dollar or model-call limit. Without `--request-profiles`,
the approved `--budget` only decides whether a new physical run may start (`fastbrowse_evals.budget`); it is
never handed to a running agent. With `--request-profiles`, a shared wallet reserves a source-bound conservative
request cost before each agent or judge dispatch and stops admission when the reservation cannot fit. This
mode requires local Chrome and its results do not qualify for publication. The harness treats a provider browser
session that does not open or close (`attempt_clock`) as an outage. Where a provider applies a limit of its own
unless told otherwise, the arm overrides it explicitly (`HOSTED_UNCAPPED_USD`, `CLOUD_BROWSER_MINUTES`), and what no
caller can override is recorded with every row (`PROVIDER_LIMITS`).

Tasks and their grading live in fastbrowse_evals.live_tasks. A login task's credentials reach fastbrowse as
`run_task` secrets scoped to the task's start origin, and the hosted agent as V4 secret bindings allowed only on the
start origin's host. With --record,
each run is saved as DIR/<arm>/<task>-<n>.mp4, n counting up from 1 past any video already there.

Each task runs on every arm that can be graded on it on equal terms (`eligible`), and the catalog states why any
head-to-head arm skips one (`excluded_because`). The hosted Browser Use run keeps its browser alive after its agent
finishes (`HOSTED_KEEP_ALIVE_SECONDS`), so the harness reads the page it ended on through that browser, grades it on
navigation tasks too, then stops it; its recorded usage is read after stopping and retained as provisional. It skips only the task
that passes by stopping before an unauthorized step, since V4 takes no authorization setting and no pause.
jev-ultrafast returns no answer, only DONE or BLOCKED, so it runs only the navigation tasks, which are graded on the
page a run ends on. fastbrowse must also end with the task's expected status, and jev-ultrafast with DONE.
fastbrowse-pool runs every task fastbrowse runs.

Every run is graded by `deepseek/deepseek-v4.1-flash` through OpenRouter alone (`JUDGE_POLICY`, the only policy
publication accepts); `--judge-policy browser-internal-jev-v4` grades Jev first with a verified fallback. The earlier
`browser-internal-opus-v*` policies name Opus, which is no longer an eval judge; they remain only so their recorded
receipts stay readable.
"""

import argparse
import asyncio
import inspect
import itertools
import json
import logging
import math
import os
import re
import signal
import statistics
import sys
import tempfile
import time
import uuid
from collections.abc import AsyncIterator, Awaitable, Callable, Mapping, Sequence
from contextlib import asynccontextmanager, nullcontext, suppress
from contextvars import ContextVar
from dataclasses import dataclass
from datetime import UTC, datetime
from decimal import Decimal
from importlib.metadata import version as package_version
from pathlib import Path
from typing import Any, Literal, TypedDict
from urllib.parse import quote, urlsplit, urlunsplit

import httpx
from fastbrowse import run_task, traced
from fastbrowse.evals.benchmarks import FASTBROWSE_ARMS
from fastbrowse.evals.harness import providers, transient_seconds
from fastbrowse.evals.status import Ending, normalize
from fastbrowse.models import (
    Authorization,
    BrowserEvent,
    CostBreakdown,
    CostComponent,
    CostLine,
    Limits,
    RunResult,
    Status,
    StepEvent,
    Unavailable,
)
from fastbrowse.safety import ScopedSecrets, origin_of
from fastbrowse.telemetry import collecting
from parallax.judge import JudgeConfigError
from parallax.judging import Judgment
from pydantic import BaseModel, ConfigDict, Field, JsonValue, TypeAdapter, ValidationError
from pydantic_core import to_jsonable_python

from fastbrowse_evals import arm_pins, backend, hosted_api
from fastbrowse_evals.attempt_clock import (
    SESSION_OPEN_SECONDS,
    TIMING_BASIS,
    AttemptClock,
    elapsed,
    session_answered,
    session_ready,
    setup_marks,
    setup_seconds,
)
from fastbrowse_evals.backend import (
    JEV_DOLLARS_PER_INPUT_TOKEN,
    RETRYABLE_STATUS,
    TRANSIENT_TRANSPORT,
    NotStarted,
    refusal,
)
from fastbrowse_evals.browsers import (
    CLOUD_BROWSER_MINUTES,
    Browser,
    cloud_browser,
    devtools_websocket,
    local_browser,
    metered_cost,
    stop_cloud_browser,
)
from fastbrowse_evals.budget import POLICY, BudgetStop, CampaignBudget
from fastbrowse_evals.evidence import once, parts, parts_v5, run_evidence, shared_step
from fastbrowse_evals.final_page import EVIDENCE, VIEWPORT, FinalPage, FrameViewport, observe_browser, prepared_tab
from fastbrowse_evals.judging import (
    EVIDENCE_QUESTIONS,
    JUDGE_MODEL,
    OPUS_JUDGE,
    QUESTIONS,
    SITE_FAILURE_QUESTIONS,
    browser_verdict,
    judge_browser,
    judge_browser_parts,
    judge_receipt,
    require_judges,
)
from fastbrowse_evals.live_tasks import TASKS, Category, LiveTask, Outcome, PageEvidence, prompt
from fastbrowse_evals.more_tasks import DEV, STRETCH_DEV
from fastbrowse_evals.process import CommandError, run_command
from fastbrowse_evals.request_session import (
    RequestJudgeClient,
    RequestSession,
    RequestSessionSpec,
    runtime_identity,
)
from fastbrowse_evals.spend import SpendStop
from fastbrowse_evals.sweep_tasks import SWEEP
from fastbrowse_evals.versions import load_lock, local_browser_identity, provenance, suite_version, task_version

AGENT_LIMITS = Limits()
"""What every arm's agent is allowed: everything. No step, time, dollar, model-call or decision limit, for every arm,
budgeted or not, in the internal and official runners alike. A cap ends hard tasks early and scores the cap rather
than the agent, so the benchmark never sets one; the approved campaign budget gates only the start of new physical runs (`fastbrowse_evals.budget`)."""
_local_browser: ContextVar[Path | None] = ContextVar("internal_local_browser", default=None)
_ultra_billing: ContextVar[dict[str, Any] | None] = ContextVar("ultrafast_billing", default=None)

HOSTED_UNCAPPED_USD = "1000"
"""`max_cost_usd` for every hosted Browser Use run. Browser Use applies a budget of its own when the field is omitted
(V4: "New API sessions default to $25", its Create Run reference), which would cap the hosted arms where no other
arm is capped. 1000 dollars is far above any run seen and above any approved campaign
budget, so it overrides the provider default without ever being reached; it is never derived from the campaign."""
PROVIDER_LIMITS = (
    "Browser Use enforces a 4 hour maximum session lifetime (SessionTimeoutLimitExceededError) on its hosted agent "
    "and its cloud browsers; the hosted SDK also stops polling a session after the same 4 hours",
)
"""Bounds imposed by a provider that no request can lift, recorded with every row rather than hidden."""

SDK_VERSION = package_version("browser-use-sdk")
ULTRAFAST_COMMIT = arm_pins.pinned("browser-use/jev-ultrafast")
ULTRAFAST = f"jev-ultrafast @ git+https://github.com/browser-use/jev-ultrafast@{ULTRAFAST_COMMIT}"
ULTRAFAST_PIN = f"jev-ultrafast commit {ULTRAFAST_COMMIT}"
"""The arm's pin as the catalog and a published source record it. The install spec above holds a URL, which public
text may not name."""
ULTRAFAST_RUNNER = Path(__file__).with_name("adapters") / "ultrafast_arm.py"
ULTRAFAST_COMMAND = ("uv", "run", "--no-project", "--quiet", "--python", "3.14", "--with", ULTRAFAST, "python")
ULTRAFAST_TEXT_MODEL = "google/gemini-3.1-flash-lite"
"""jev-ultrafast's text helper on OpenRouter, reasoning off as its .env.example sets it. Its configured model,
inception/mercury-2.5, has no provider on OpenRouter any more (every request answers 404); this is the model its
docs/performance.md probed beside Mercury on the same fields and found correct."""
OUTAGE_RETRIES = 5
"""Runs of a row a provider outage, or an interruption the judge found (`judged_unavailable`), ended, after the
first, waiting 1, 2, 4, 8 then 10 minutes: about 25 minutes, past the 503 spells seen so far. A row still unavailable then is recorded, and left out of every published figure."""
BROWSER_TRANSPORT = "browser_transport"
"""`failure_class` of an attempt a browser's own transport ended (an IPC or CDP reply timing out) on either arm. That it
clears on a repeat is unproven, so the row is retried at most `TRANSPORT_RETRIES` times, every attempt kept."""
TRANSPORT_RETRIES = 1
TRANSPORT_ERROR = re.compile(r"failed \((?:TimeoutError|ConnectionClosed\w*)\)")
"""How fastbrowse's browser layer spells a command whose reply timed out or whose socket closed, and nothing else: a
task's own timeout is not one, and neither is any other error."""
HOSTED_MODELS: dict[str, str | None] = {"browser-use": None, "browser-use-ultrafast": "bu-ultrafast"}
"""The V4 model each hosted Browser Use arm asks for. None omits `model` and `modelParams`, which is Browser Use's
default (GPT-6 Luna, high reasoning, docs.browser-use.com/cloud/agent/models); `bu-ultrafast` is its low-latency
preset, which takes no `modelParams`. Each row records the model the run reports."""
HOSTED_KEEP_ALIVE_SECONDS = 900
"""V4 `keepAlive` for every hosted run: how long its browser stays open after its agent finishes, so the harness can
read the page it ended on before stopping it. V4 takes integer seconds from 120 to 14400 and otherwise stops an idle
browser after 120, which a slow read could outlast; the harness stops the browser once it has read it, so the kept
time billed is that read alone."""
HOSTED_POLL_SECONDS = 1.0
# Browser Use's SDK (v4 runs.wait_for_completion) retries a new run's 404 for five seconds.
HOSTED_VISIBILITY_SECONDS = 5.0
HOSTED_VISIBILITY_RETRY_SECONDS = 0.25
"""How often a running hosted run's status and events are read; its seconds come from the server's event times."""
HOSTED_TERMINAL = frozenset({"completed", "failed", "cancelled"})
BROWSER_READY = {"browser.ready", "browser.reattached"}
HOSTED_READY_POLL_SECONDS = 0.5
"""How often a hosted session is asked whether its browser has started; the `seconds` come from the server's clock."""
SITE_PROBE_SECONDS = 15
"""How often the harness fetches each task site's start page while a run goes on."""
SITE_STALL_SECONDS = 10
"""A start page fetch slower than this is the site stalling: every site in the suites answers in under two seconds
between stalls."""


def _watch(arm: str, task: LiveTask, live_url: str | None) -> None:
    if live_url:
        print(f"WATCH {arm:13} {task.id:18} {live_url}", flush=True)


def _secrets(task: LiveTask) -> ScopedSecrets | None:
    """The task's login, typed only on its start origin: fastbrowse checks the origin before it resolves a value."""
    return ScopedSecrets(task.secrets, origin_of(task.start)) if task.secrets else None


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


def requirements(check: Callable[..., object]) -> dict[str, JsonValue]:
    """A check's source and the values it closes over. Most checks are built by a factory (`_has("Marilyn Monroe")`,
    `_pairs(...)`, `_path_is("/project/httpx")`) whose source names its expected values only as variables, and their
    tasks' truth is None, so the source alone hides every expected value from the judge. Nested checks (`_all`,
    `_arrived`) are expanded the same way."""

    def bound(value: object) -> JsonValue:
        if inspect.isfunction(value):
            return requirements(value)
        if isinstance(value, tuple | list):
            return [bound(item) for item in value]
        return json_value(value)

    return {
        "source": inspect.getsource(check),
        "bound": {name: bound(value) for name, value in inspect.getclosurevars(check).nonlocals.items()},
    }


OPUS_V1_POLICY = "browser-internal-opus-v1"
"""anthropic/claude-opus-5.5 through OpenRouter grades every row alone (`judging.OPUS_JUDGE`), and Jev is never asked.
As in v3 of the Jev policy, a state over the judge's window is never sent and a run a site's bot check stopped before
it answered is UNKNOWN without a judge. Selected only with --judge-policy; no published figure accepts it."""
OPUS_V2_POLICY = "browser-internal-opus-v2"
"""v1 with `judging.SITE_FAILURE_QUESTIONS`, so an error page, a failed results load or an outage the site served, not
caused by the agent, is an environment interruption the judge leaves ungraded (`judged_unavailable`) rather than an
agent failure; a site's bot check is judged by the same question for every arm, never settled by an arm's own wording;
and the judged report leaves out the agent's own model traffic (`shown_to`). Selected only with --judge-policy; no
published figure accepts it."""
OPUS_V3_POLICY = "browser-internal-opus-v3"
"""v2's questions over a state that holds the whole final page, on every task and for every arm (v2 saw its first 300
characters, on navigation tasks for some arms), with each arm's report reduced to `blind_report`. It still shows
fastbrowse's `quotes` and step `facts`, which name that arm. Selected only with --judge-policy; no published figure
accepts it."""
OPUS_V4_POLICY = "browser-internal-opus-v4"
"""v3 with every arm's evidence in one shared form (`shared_step`), so no field exists for only one arm. A quote stays
as evidence on the step that read it. A v3 row can be regraded under it from its recorded state; an earlier row holds
no more of the final page, so it cannot. Selected only with --judge-policy; no published figure accepts it."""
JEV_JUDGE_POLICY = "browser-internal-jev-v4"
"""v4 uses OpenRouter System One and its noul/refused wire contract. Jev settles only strictly beyond its
thresholds, states over its input budget skip it, safety tasks
(`fallback_only`) go straight to the pinned fallback, a state over the fallback's window is never sent, and a run a
site's bot check stopped before it answered is UNKNOWN without a judge (`bot_check_block`). Selected only with
--judge-policy; no published figure accepts it."""
OPUS_V5_POLICY = "browser-internal-opus-v5"
"""Every page the run saw, each action, typed values, and citations with their quotes, in one form for every arm
(`evidence.run_evidence`), so no run is graded on its answer alone: one with no record at all is UNKNOWN without a judge
(`NO_EVIDENCE`). The source text of a task's check is gone from the state. A state too long for one request is divided
by `evidence.parts_v5`, which sends a long page's whole controls with every slice of its text: one Hacker News thread
took 16 requests and $23 to judge (#1075). Selected only with --judge-policy; no published figure accepts it."""
JUDGE_POLICY = "browser-internal-deepseek-v6"
"""The default, and the only policy a published figure accepts, graded by `judging.JUDGE_MODEL` alone: v5's evidence
with each page listed once
(`evidence.once`), its controls beside its text, and a state too long for one request divided by `evidence.parts`, which
sends every byte of every page once. Nothing the judge read under v5 is left out. A state that fits one request differs
from v5's only where v5 listed a page twice. Any v5 row can be regraded under it from its recorded state."""
EVIDENCE_POLICIES = (OPUS_V5_POLICY, JUDGE_POLICY)
"""The policies that show the judge `evidence.run_evidence` in place of the arm's report."""
POLICY_QUESTIONS = {
    OPUS_V1_POLICY: QUESTIONS,
    OPUS_V2_POLICY: SITE_FAILURE_QUESTIONS,
    OPUS_V3_POLICY: SITE_FAILURE_QUESTIONS,
    OPUS_V4_POLICY: SITE_FAILURE_QUESTIONS,
    OPUS_V5_POLICY: EVIDENCE_QUESTIONS,
    JUDGE_POLICY: EVIDENCE_QUESTIONS,
    JEV_JUDGE_POLICY: QUESTIONS,
}
JUDGE_POLICIES = tuple(POLICY_QUESTIONS)
POLICY_JUDGE = {policy: OPUS_JUDGE for policy in POLICY_QUESTIONS} | {JUDGE_POLICY: JUDGE_MODEL}
"""The one model each policy that never asks Jev is graded by; the Jev policy's fallback is any verified model."""
BOT_CHECK_POLICIES = (OPUS_V1_POLICY, JEV_JUDGE_POLICY)
"""The earlier policies, which keep a bot check fastbrowse named UNKNOWN without a judge (`bot_check_block`)."""
_judge_policy: ContextVar[str] = ContextVar("judge_policy", default=JUDGE_POLICY)


def fallback_only(task: LiveTask) -> str | None:
    """Why `task` skips Jev, or None. A safety or authorization task passes by pausing or refusing; Jev returned
    exactly p=0.10, a FAIL, on six correct saucedemo-pause runs (2026-10-16), so these always go to the fallback."""
    if task.category is Category.SAFETY or task.expect is not Status.COMPLETE:
        return f"safety task ({task.category.value}, expected ending {task.expect.value}): pinned fallback only"
    return None


BOT_CHECK = "blocked by site bot check"
BOT_CHECK_STOP = "bot check at "
"""How fastbrowse's `blocked` stop names a bot check (crates/fastbrowse-agent/src/agent/driving.rs): `Status.BLOCKED`
is its typed "a bot check stands in the way", and this prefix tells it apart from the same status given for an HTTP
403."""


def bot_check_block(outcome: Outcome, report: "ArmReport") -> bool:
    """A site's bot check stopped the run before it answered: an external block, the same verdict every time. On
    2026-10-16 the judge put 13 such pool rows at UNKNOWN and 2 at FAIL, because a captured challenge page read as an
    observation; the policy between the two is open, so the block is kept UNKNOWN deterministically."""
    return (
        report.status == Status.BLOCKED.value
        and (report.error or "").startswith(BOT_CHECK_STOP)
        and outcome.answer is None
        and outcome.data is None
    )


FINAL_CAPTURE_FIELDS = {"final_page_capture", "final_page_capture_viewport", "final_page_capture_withheld"}


def judge_state(task: LiveTask, truth: object, outcome: Outcome, report: "ArmReport") -> dict[str, JsonValue]:
    """Everything the judge is shown of one run: the task, its intended outcome and the arm's observations."""
    return {
        "task": task.task,
        "authorization": {"irreversible_actions": task.authorize},
        "expected_ending": task.expect.value,
        "requirements": requirements(task.check),
        "intended_outcome": json_value(truth),
        "answer": json_value(outcome.answer),
        "data": json_value(outcome.data),
        "final_url": json_value(outcome.final_url),
        "final_page": json_value(outcome.evidence),
        # The final page's controls (a cart's contents, a date field), the quotes located verbatim in page captures
        # (`shown_to` moves those onto their steps), and whether the arm can say where its browser ended at all.
        "final_controls": json_value(outcome.controls),
        "quotes": json_value(outcome.quotes),
        "final_page_unobservable": outcome.unobservable,
        **({"final_page_capture": report.final_page_capture} if report.final_page_capture else {}),
        **(
            {"final_page_capture_viewport": json_value(report.final_page_capture_viewport)}
            if report.final_page_capture_viewport
            else {}
        ),
        **(
            {"final_page_capture_withheld": report.final_page_capture_withheld}
            if report.final_page_capture_withheld
            else {}
        ),
        "report": json_value(report.model_dump(mode="json", exclude=FINAL_CAPTURE_FIELDS)),
    }


MODEL_TRAFFIC = "http"
"""The trace event fastbrowse writes for each model request, with its full request and response bodies
(fastbrowse-clients `HttpEvent`): the agent's own model traffic, kept in the row for trace analysis."""
BLIND_REPORT = ("error", "observe_error", "failure_class", "steps", "trace")
"""What a blind judge reads of an arm's report beside its ending and steps. Anything else there says who ran (model,
session, provenance, helpers, artifact paths), what the run cost or how long it took, or how the agent reached its
choices (trace events, decision internals), none of which is evidence of the task."""
BLIND_STEP = frozenset(
    {
        "index",
        "operation",
        "outcome",
        "url",
        "target",
        "choice",
        "command",
        "exit_code",
        "output",
        "note",
        "facts",
        "file",
        "page_changed",
        "observation",
        "observation_same_as_step",
    }
)
"""What each arm's step log says the run did and saw; anything else on a step is that arm's own bookkeeping."""


def blind_report(report: Mapping[str, JsonValue], ending: Ending) -> dict[str, JsonValue]:
    """`report` as a judge blind to the arm reads it, by one rule for every arm: its ending in the vocabulary every arm
    shares, instead of the word only one arm uses, and its steps. An arm logs its steps in `step_log` or, jev-ultrafast
    alone, `decision_log`. The ledger row keeps the whole report."""
    logged = report.get("step_log") or report.get("decision_log")
    steps = logged if isinstance(logged, list) else []
    return {
        "ending": ending.value,
        **{field: report.get(field) for field in BLIND_REPORT},
        "step_log": [
            {k: v for k, v in step.items() if k in BLIND_STEP} if isinstance(step, dict) else step for step in steps
        ],
    }


NO_EVIDENCE = "the run left no record of a page or an action, so its answer cannot be checked"


def no_evidence(state: Mapping[str, JsonValue]) -> bool:
    """Whether `state` holds only the run's answer: a judge would have nothing to check it against."""
    return not (
        state.get("pages") or state.get("timeline") or state.get("final_page") or state.get("final_page_capture")
    )


def shown_to(policy: str, state: dict[str, JsonValue], ending: Ending) -> dict[str, JsonValue]:
    """`state` as `policy`'s judge is shown it; `ending` is how the run ended in every arm's shared vocabulary. Under
    `JUDGE_POLICY` it is `OPUS_V5_POLICY`'s state with each page listed once (`evidence.once`). Under
    `OPUS_V4_POLICY` every arm's evidence takes one form (`shared_step`), from a run's state or a state recorded under
    `OPUS_V3_POLICY` alike, with `quotes` empty for every arm because each quote stays on its step. Under
    `OPUS_V3_POLICY` the report is `blind_report`, with the expected ending in the same vocabulary. Under
    `OPUS_V2_POLICY` the report leaves out the agent's own model traffic: those request and response
    bodies are not task evidence, no vendor arm exposes them, and they took one row's state from 19 KB to 1.8 MB, past
    the judge's window. The ledger row keeps everything, and earlier policies are shown the state as they always were."""
    if policy not in EVIDENCE_POLICIES and FINAL_CAPTURE_FIELDS.intersection(state):
        state = {k: v for k, v in state.items() if k not in FINAL_CAPTURE_FIELDS}
    report = state.get("report")
    if policy in EVIDENCE_POLICIES:
        if "pages" not in state and isinstance(report, dict):
            hidden = ("requirements", "report", "quotes")
            kept = {k: v for k, v in state.items() if k not in hidden}
            outcome = {field: report.get(field) for field in ("error", "observe_error", "failure_class", "steps")}
            state = {
                **kept,
                "expected_ending": normalize(str(state["expected_ending"])).value,
                "report": {"ending": ending.value, **outcome},
                **run_evidence(report, state.get("quotes")),
            }
        return once(state) if policy == JUDGE_POLICY else state
    if policy in (OPUS_V4_POLICY, OPUS_V3_POLICY) and isinstance(report, dict) and "ending" not in report:
        expected = normalize(str(state["expected_ending"])).value
        state = {**state, "expected_ending": expected, "report": blind_report(report, ending)}
        report = state["report"]
    if policy == OPUS_V4_POLICY and isinstance(report, dict):
        steps = report.get("step_log")
        shared = [shared_step(step) for step in steps] if isinstance(steps, list) else []
        return {**state, "quotes": None, "report": {**report, "step_log": shared}}
    if policy != OPUS_V2_POLICY or not isinstance(report, dict):
        return state
    events = report.get("events")
    if not isinstance(events, list):
        return state
    kept = [e for e in events if not (isinstance(e, dict) and e.get("event") == MODEL_TRAFFIC)]
    return {**state, "report": {**report, "events": kept}}


class _JudgeRoute(TypedDict):
    policy: str
    questions: Mapping[str, str]
    fallback_only: str | None
    unjudgeable: str | None
    use_jev: bool


async def grade_state(
    task: LiveTask,
    state: dict[str, JsonValue],
    *,
    policy: str,
    unjudgeable: str | None,
    ending: Ending,
    client: httpx.AsyncClient | None = None,
) -> tuple[bool | None, str, dict[str, JsonValue]]:
    """Judge `state` under `policy`, preserving abstentions and provider receipts. The receipt keeps the exact state
    judged, so a later policy can grade the row again without running it (`regrade`)."""
    state = shown_to(policy, state, ending)
    if policy in EVIDENCE_POLICIES:
        scrubbed = _scrubbed(state, task.secrets)
        assert isinstance(scrubbed, dict)
        state = scrubbed
        unjudgeable = unjudgeable or (NO_EVIDENCE if no_evidence(state) else None)
    divided = parts(state) if policy == JUDGE_POLICY else parts_v5(state) if policy == OPUS_V5_POLICY else [state]
    route: _JudgeRoute = {
        "policy": policy,
        "questions": POLICY_QUESTIONS[policy],
        "fallback_only": fallback_only(task) if policy == JEV_JUDGE_POLICY else None,
        "unjudgeable": unjudgeable,
        "use_jev": policy == JEV_JUDGE_POLICY,
    }
    judged = (
        judge_browser(state, **route, **({} if client is None else {"client": client}))
        if len(divided) == 1
        else judge_browser_parts(state, divided, **route, **({} if client is None else {"client": client}))
    )
    result, identity = await judged
    passed, reason = browser_verdict(result)
    return passed, reason, {**judge_receipt(result, identity), "state": state}


def ending_of(arm: str, report: "ArmReport") -> Ending:
    """How a run ended in the vocabulary every arm shares. `normalize` reads the hosted agent's statuses in API V3's
    words, where a finished session is `stopped`; V4 names a finished run `completed`. `normalize` is part of every
    task's fingerprint, so V4's word is read here rather than there, leaving every task's version as it was."""
    hosted = arm in HOSTED_MODELS
    status = "stopped" if hosted and report.status == "completed" else report.status
    return normalize(status, hosted=hosted, answered=bool(report.answered))


async def semantic_grade(
    task: LiveTask,
    truth: object,
    outcome: Outcome,
    report: "ArmReport",
    ending: Ending,
    *,
    client: httpx.AsyncClient | None = None,
) -> tuple[bool | None, str, dict[str, JsonValue]]:
    """Judge complete observations under the invocation's policy."""
    policy = _judge_policy.get()
    return await grade_state(
        task,
        judge_state(task, truth, outcome, report),
        policy=policy,
        unjudgeable=BOT_CHECK if policy in BOT_CHECK_POLICIES and bot_check_block(outcome, report) else None,
        ending=ending,
        client=client,
    )


def judged_unavailable(receipt: Mapping[str, JsonValue] | None) -> bool:
    """Whether the semantic judge found the run's evidence unavailable (`evidence_available` false): the policy's
    question admits only an infrastructure interruption, missing observer data or unavailable state there (under
    every policy from `OPUS_V2_POLICY` on, a failure the site served that the agent did not cause too), never the
    agent's own mistake. Such a run measured no agent, so it is an outage to wait out and run again, as one the harness
    recognised itself would be. So does a run that left no record at all (`NO_EVIDENCE`), which no judge is asked about.
    A judge that abstained (null), or any other harness rule such as a bot check, is not a finding of unavailability."""
    if receipt is None:
        return False
    decision = Judgment.model_validate(receipt["judgment"]).decisions.get("evidence_available")
    return decision is not None and (decision.passed is False or decision.reason == NO_EVIDENCE)


@dataclass
class _Observed:
    """What the harness sees of one fastbrowse run beyond its result, read from the browser after the run."""

    final: FinalPage
    began: float
    """When the agent's clock started, with any browser it opened mid-run taken out."""
    ended: float
    """When the run returned, so neither the final reading nor the recording's result card is timed as the run."""
    browser: tuple[CostLine, ...] = ()
    """The cost of the cloud browser the harness started for the run; empty for the pool or a local browser."""
    live_url: str | None = None
    browser_session_id: str | None = None
    open_seconds: float | None = None
    close_seconds: float | None = None


_running: ContextVar[str] = ContextVar("live_running", default="-")
"""`<arm> <task>` for the run a log record came from: eight runs overlap, and a bare retry warning names none."""
_attempt_artifact: ContextVar[str | None] = ContextVar("live_attempt_artifact", default=None)


def json_value(value: object) -> JsonValue:
    return TypeAdapter(JsonValue).validate_python(to_jsonable_python(value, fallback=str))


class _NameRun(logging.Filter):
    def filter(self, record: logging.LogRecord) -> bool:
        record.run = _running.get()
        return True


class Efficiency(TypedDict):
    """The stable per-row measures `efficiency` returns, typed so they splat into `ArmReport`."""

    input_tokens: int
    output_tokens: int
    llm_calls: int | None
    llm_calls_basis: Literal["physical_requests"]
    jev_calls: int
    tokens_per_second: float | None


class ProvisionalHostedCost(BaseModel):
    """Session usage at one query time; the provider exposes no billing-finalized signal."""

    observed_at: datetime
    session_id: str
    run_id: str
    browser_session_id: str | None
    total_cost_usd: str
    llm_cost_usd: str
    search_cost_usd: str
    browser_cost_usd: str
    proxy_cost_usd: str


class ArmReport(BaseModel):
    """What an arm says about one run beyond its outcome; a field belongs to the arms named beside it."""

    status: str
    task_successful: bool | None = None
    seconds: float
    """From the browser session being ready to the final answer (`attempt_clock`); the hosted arm ends at its agent's
    `done`."""
    timing_basis: str | None = TIMING_BASIS
    """What `seconds` measures; `SESSION_INCLUDED` when an arm could not keep its session out of it."""
    session_open_seconds: float | None = None
    """Opening the browser session, outside `seconds`; None when the arm was handed its browser or could not tell."""
    session_close_seconds: float | None = None
    """Closing the browser session, outside `seconds`; a cloud browser's excludes its proxy cost settling."""
    browser_seconds: float | None = None
    agent_seconds: float | None = None
    """Inside `seconds`, the time spent in browser round trips (acting and observing the page), for an arm whose step
    timing exposes it; None for any other. Measured, not taken off the headline."""
    timing_note: str | None = None
    """What of a run's browser setup stayed in `seconds`, for an arm that cannot separate it."""
    transient_seconds: float = 0.0
    """Provider outage waits fastbrowse measured inside the run; included in the full wall time."""
    dollars: float | None
    """None when some of the run's spend could not be priced: a known total would then be only a floor."""
    error: str | None = None
    steps: int | None = None
    failure_class: Literal["browser_transport", "site_http"] | None = None
    """`browser_transport` when the browser's own transport ended the attempt; see BROWSER_TRANSPORT."""
    trace: list[str] = []
    cost_by_component: dict[str, float] = {}
    provisional_hosted_cost: ProvisionalHostedCost | None = Field(
        default=None, exclude_if=lambda receipt: receipt is None
    )
    # fastbrowse
    citations: list[JsonValue] = []
    step_log: list[JsonValue] = []
    events: list[object] = []
    unknown_cost: bool | None = None
    observe_error: str | None = None
    final_page_capture: str | None = None
    final_page_capture_viewport: FrameViewport | None = None
    final_page_capture_withheld: str | None = None
    seconds_by_call: dict[str, float] = {}
    # The stable efficiency measures: tokens the model calls sent and how long those calls' own seconds were.
    # Wall clock and cloud load move together, so neither can compare two builds; this can.
    input_tokens: int | None = None
    output_tokens: int | None = None
    llm_calls: int | None = None
    llm_calls_basis: Literal["physical_requests", "cost_lines"] | None = Field(
        default=None, exclude_if=lambda basis: basis is None
    )
    jev_calls: int | None = None
    tokens_per_second: float | None = None
    # jev-ultrafast
    actions: int | None = None
    decisions: int | None = None
    """Model choices made, of which `actions` were executed: a stale page discards a choice before it acts."""
    step_cap: int | None = None
    decision_cap: int | None = None
    step_cap_unit: Literal["actions", "steps", "tool_calls"] | None = None
    """The unit `step_cap` counts in: this arm's actions, fastbrowse's steps."""
    decision_log: list[JsonValue] = []
    stale: list[JsonValue] = []
    """Each choice upstream discarded as stale, with its exact reason."""
    last_exception: str | None = None
    last_offered: JsonValue = None
    final_page_url: str | None = None
    artifact: str | None = None
    """Per-step offered controls and the full action history, written beside the video when recording."""
    text_helpers: list[str] = []
    provenance: dict[str, JsonValue] = {}
    unmetered_requests: int | None = None
    text_model: str | None = None
    # the Browser Use agent
    model: str | None = None
    session_id: str | None = None
    answered: bool | None = None
    """Its agent called `done` with a result that was not an error: its own completion, as fastbrowse's is."""
    session_seconds: float | None = None
    """The whole session, from its create to the API reporting it stopped, which can be well after the answer."""
    # fastbrowse-pool
    lease_seconds: float | None = None
    """Getting the pool browser: its lease and WebSocket address, without waiting out a full pool. The arm's
    `session_open_seconds`."""
    pool_full_seconds: float | None = None
    """Waiting out a full pool before the lease: an artifact of the pool's size, outside every other figure."""
    lease_extend_failures: int | None = None
    """Lease extensions the pool refused or never answered; each is logged, and none stops the run."""


class HostedUnavailable(Unavailable):
    """A hosted outage whose settled provider receipt must survive the interruption."""

    def __init__(self, report: ArmReport) -> None:
        super().__init__("Browser Use could not dispatch the run" + (f": {report.error}" if report.error else ""))
        self.report = report


class EvalRow(ArmReport):
    """One line of the results file: the arm's report, graded against the task's truth."""

    timing_basis: str | None = None
    """None on a row written before `seconds` had a recorded basis, which no comparison reads."""
    arm: str
    task: str
    category: str
    at: float
    concurrency: int | None = None
    status: str | None = None
    """The arm's own word for how its run ended, in that arm's vocabulary; None when a crashed arm reported nothing.
    It never decides a result: `passed` does, and `normalized_status` compares endings across arms."""
    repeat: int | None = None
    """Which of the invocation's `--repeat` passes this is: every arm's attempt at a task in one pass is paired."""
    retries: int = 0
    """Earlier outage attempts, retained in the attempt ledger and excluded from agent scores."""
    started: bool = True
    """False when the provider refused the request that starts the run (`NotStarted`): nothing ran or was billed, so
    the row's charge is zero and its budget hold is released."""
    normalized_status: Ending = Ending.ERROR
    """`status` in the vocabulary every arm shares. Only `unavailable` changes how a row counts (an outage)."""
    execution_admission_stopped: bool = False
    execution_admission_reason: str | None = None
    semantic_judge: dict[str, JsonValue] | None = None
    judge_dollars: float | None = None
    known_judge_dollars: float = 0.0
    unpriced_judge_attempts: int = 0
    correct: bool | None
    passed: bool | None
    failure: str | None
    answer: str | None = None
    data: object = None
    final_url: str | None = None
    final_page: PageEvidence | None = None
    final_controls: list[tuple[str, str | None]] | None = None
    """The label and value of every control on the final page, as the judges were given them (`final_controls`)."""
    site_probe: str | None = None
    video: str | None = None
    suite: str | None = None
    suite_version: str | None = None
    task_version: int | None = None
    """Rows compare only at equal task versions; see fastbrowse.evals.versions."""
    run: dict[str, JsonValue] = {}
    """The invocation's provenance, the same on every row it wrote: build, commit, providers, run id."""
    regraded: dict[str, JsonValue] | None = None
    """Set only by Parallax 1's `parallax browser regrade`, which this harness does not carry, and kept so its ledgers
    still read: the ledger and line this row was judged again from, the policy and
    verdict it had there, and whether the judged state was the one recorded or one rebuilt from the row."""


_completed_attempt: ContextVar[EvalRow | None] = ContextVar("live_completed_attempt", default=None)


class _UltrafastReport(BaseModel):
    """The jev-ultrafast runner's result line (`fastbrowse_evals/adapters/ultrafast_arm.py`)."""

    status: str
    error: str | None
    seconds: float
    steps: int
    actions: int
    trace: list[str]
    setup_seconds: float = 0.0
    """Connecting and opening the first page, before the runner's `seconds` began."""
    failure_class: Literal["browser_transport", "site_http"] | None = None
    decisions: int | None = None
    step_cap: int | None = None
    decision_cap: int | None = None
    step_cap_unit: Literal["actions", "steps"] | None = None
    decision_log: list[JsonValue] = []
    stale: list[JsonValue] = []
    last_exception: str | None = None
    last_offered: JsonValue = None
    final_page_url: str | None = None
    final_page: FinalPage = FinalPage(error="the runner sent no final page")
    """Read inside the runner before its browser daemon restarts, which resets the emulated viewport."""
    artifact: str | None = None
    text_helpers: list[str] = []
    provenance: dict[str, JsonValue] = {}
    jev_dollars: float
    text_dollars: float
    unmetered_requests: int
    text_model: str | None


def _page_of(final: FinalPage) -> tuple[PageEvidence | None, bool]:
    """`Outcome`'s evidence and unobservable: every arm, on every task, carries the final page it could read and says
    when it could not, so no arm is graded on more of a page than another."""
    return final.evidence, final.error is not None


@asynccontextmanager
async def _fast_browser(
    http: httpx.AsyncClient, task: LiveTask, cdp_url: str | None, local: Path | None
) -> AsyncIterator[Browser]:
    """The browser a fastbrowse run attaches to; the one thing the fastbrowse arms differ in. A pool browser is handed
    over by its WebSocket address; otherwise the --local-browser Chrome, else a Browser Use Cloud browser."""
    if cdp_url is not None:
        yield Browser(cdp_url)
    elif local is not None:
        async with local_browser(local) as browser:
            yield browser
    else:
        async with cloud_browser(http, screen=_screen(task)) as browser:
            yield browser


@asynccontextmanager
async def _fast_tab(
    http: httpx.AsyncClient, task: LiveTask, cdp_url: str | None, arm: str, local: Path | None
) -> AsyncIterator[Browser]:
    async with _fast_browser(http, task, cdp_url, local) as browser:
        _watch(arm, task, browser.live_url)
        pinned = task.category is Category.NAVIGATE and not browser.screen_sized
        async with prepared_tab(browser.cdp_url, viewport=pinned):
            yield browser


_OPENS_ITS_OWN_BROWSER = "open_browser" in inspect.signature(run_task).parameters
"""Whether the installed fastbrowse opens its browser lazily, through the harness's callback. A baseline release older
than that is measured as it shipped (`fast_arm`)."""


async def fast_arm(
    task: LiveTask,
    http: httpx.AsyncClient,
    downloads: Path,
    *,
    record: Path | None,
    cdp_url: str | None = None,
    arm: str = "fastbrowse",
    request_session: RequestSession | None = None,
) -> tuple[Outcome, RunResult, _Observed]:
    """One fastbrowse run through its public `run_task`. Every arm built on fastbrowse calls this, so prompt, start,
    schema, secrets, settings, limits, authorization, downloads, events, recording and evidence are the same by
    construction; only the browser differs (`_fast_browser`). The run reads over HTTP first, as fastbrowse does by
    default, so the browser opens only if the run asks for one: a run that never does has no session to exclude and
    no final page to read, and one that does excludes that open. The harness owns that browser and fastbrowse attaches
    to its tab in front, so after the run the harness reads the page it ended on (`final_page.observe_browser`), as it
    does for every other arm. A navigation task runs at the competing arm's viewport: a cloud browser starts at that
    size, and any other browser's tab is held there for the whole run."""
    # Read here: fastbrowse opens the browser from a callback, outside this run's context.
    tab = _fast_tab(http, task, cdp_url, arm, _local_browser.get())
    browser: Browser | None = None
    asked, page_ready = setup_marks()

    async def open_browser() -> str:
        nonlocal browser
        asked()
        browser = await tab.__aenter__()
        return browser.cdp_url

    began = session_ready()
    settings = (
        backend.run_settings() if request_session is None else backend.run_settings(request_session.agent_settings())
    )
    try:
        if _OPENS_ITS_OWN_BROWSER:
            browser_options: dict[str, object] = {"open_browser": open_browser, "page_ready": page_ready}
        else:
            # A release before lazy browsers (0.9.x) drives a browser from its first step, as it shipped. It gets the
            # same harness-owned browser up front and takes over the tab the harness prepared.
            browser_options = {"cdp_url": await open_browser(), "attach": True}
            await page_ready()
        result = await run_task(
            prompt(task),
            start=task.start,
            # The two shapes suit different fastbrowse releases, so neither can be written as fixed keywords.
            **browser_options,  # ty: ignore[invalid-argument-type]
            settings=settings,
            output_schema=task.output_schema,
            secrets=_secrets(task),
            limits=AGENT_LIMITS,
            authorization=Authorization(irreversible_actions=task.authorize),
            downloads=downloads,
            http=http,
            on_event=lambda event: _on_fast_event(arm, task, event),
            record=record,
        )
        ended = session_answered(run_ended_at(collecting()))
        opening = setup_seconds()
        if browser is None:
            final = FinalPage(url=result.final_url, error="the run opened no browser")
        else:
            final = await observe_browser(
                browser.cdp_url, url=result.final_url, secrets=task.secrets if not task.bitwarden_item else None
            )
    finally:
        if browser is not None:
            await tab.__aexit__(None, None, None)
    seen = _Observed(
        final=final,
        began=began + opening,
        ended=ended,
        browser=() if browser is None else browser.cost,
        live_url=None if browser is None else browser.live_url,
        browser_session_id=None if browser is None else browser.session_id,
        open_seconds=None if browser is None else round(opening, 2),
        close_seconds=None if browser is None else browser.close_seconds,
    )
    quotes = tuple((e.url, e.quote) for e in result.evidence)
    outcome = Outcome(result.answer, result.data, final.url, quotes, final.controls, *_page_of(final))
    return outcome, result, seen


def run_ended_at(events: list[object] | None) -> float | None:
    """When the agent's run ended with its answer, from its `run` phase on `time.monotonic()`. What `run_task` does
    after that is teardown: encoding a recording, showing the result card and closing the session, which can take
    minutes on a loaded machine and are not the agent's time. `None` when the trace has no run phase."""
    ends = [
        event["ended"]
        for event in events or ()
        if isinstance(event, dict) and event.get("event") == "phase" and event.get("phase") == "run"
    ]
    return max(ends) if ends else None


async def _on_fast_event(arm: str, task: LiveTask, event: StepEvent | BrowserEvent) -> None:
    if isinstance(event, BrowserEvent):
        _watch(arm, task, event.live_url)


async def prepare_ultrafast() -> None:
    """Install jev-ultrafast's environment once, so no run is timed installing it."""
    with tempfile.TemporaryDirectory(prefix="ultra-prepare-") as runtime:
        await run_command(
            (*ULTRAFAST_COMMAND, "-c", "import jev_ultrafast"),
            b"",
            seconds=None,
            environment=arm_environment("jev-ultrafast", runtime),
            cwd=runtime,
        )


def _ultrafast_env(cdp_ws: str, runtime: str) -> dict[str, str]:
    env = arm_environment("jev-ultrafast", runtime)
    # The isolated runner cannot import fastbrowse, so it is handed the Jev route the fastbrowse arms are given.
    key_name, base_url, model, key = backend.jev_route()
    env[key_name] = key
    return env | {
        "TEXT_MODEL_API_KEY": backend.openrouter_key(),
        "TEXT_MODEL_BASE_URL": "https://openrouter.ai/api/v1",
        "TEXT_MODEL": ULTRAFAST_TEXT_MODEL,
        "TEXT_MODEL_REASONING": "none",
        "BU_CDP_WS": cdp_ws,
        "BU_NAME": "fastbrowse-eval",
        "BH_RUNTIME_DIR": runtime,
        "BH_TELEMETRY": "0",
        "BH_UPDATE_CHECK": "0",
        "BH_TAB_MARKER": "0",
        "FASTBROWSE_JEV_BASE_URL": base_url,
        "FASTBROWSE_JEV_MODEL": model,
    }


def _browser_dollars(cost: Sequence[CostLine]) -> tuple[float, bool]:
    """A cloud browser's known dollars, and whether any of its lines was unpriced."""
    return sum(line.dollars or 0 for line in cost), any(line.dollars is None for line in cost)


def _known_components(lines: Sequence[CostLine]) -> dict[str, float]:
    """Dollars by component; a component with an unpriced line has no figure, which a zero would misstate."""
    known = {line.component for line in lines} - {line.component for line in lines if line.dollars is None}
    return {c.value: sum(line.dollars or 0 for line in lines if line.component == c) for c in known}


async def ultrafast_arm(task: LiveTask, http: httpx.AsyncClient, *, record: Path | None) -> tuple[Outcome, ArmReport]:
    async with cloud_browser(http, screen=_screen(task)) as cloud:
        session_ready()
        _watch("jev-ultrafast", task, cloud.live_url)
        request = {
            "start": task.start,
            "goal": prompt(task),
            "cdp_ws": cloud.cdp_url,
            # Uncapped: the runner stops on no action or decision count, and its meter reserves nothing.
            "max_steps": None,
            "record": None if record is None else str(record),
            "jev_dollars_per_input_token": JEV_DOLLARS_PER_INPUT_TOKEN,
            "evidence_script": EVIDENCE,
            "max_dollars": None,
            "max_seconds": None,
            **(_ultra_billing.get() or {}),
        }
        with tempfile.TemporaryDirectory(prefix="bh-") as runtime:
            ran = _UltrafastReport.model_validate_json(
                await _invoke(
                    ULTRAFAST_COMMAND, ULTRAFAST_RUNNER, request, _ultrafast_env(cloud.cdp_url, runtime), runtime
                )
            )
        session_answered()
        # The runner read the evidence at its own viewport before its daemon reset it; the controls are read here, by
        # the same observer every arm's final page goes through.
        final = ran.final_page
        if final.error is None:
            final = await observe_browser(
                cloud.cdp_url,
                url=final.url,
                evidence=final.evidence,
                secrets=task.secrets if not task.bitwarden_item else None,
            )
    browser, unpriced = _browser_dollars(cloud.cost)
    dollars = ran.jev_dollars + ran.text_dollars + browser
    unknown = bool(ran.unmetered_requests) or unpriced
    report = ArmReport(
        status=ran.status,
        seconds=round(ran.seconds, 2),
        session_open_seconds=round((cloud.open_seconds or 0.0) + ran.setup_seconds, 2),
        session_close_seconds=cloud.close_seconds,
        dollars=None if unknown else dollars,
        unknown_cost=unknown,
        error=ran.error,
        steps=ran.steps,
        trace=ran.trace,
        cost_by_component=({} if ran.unmetered_requests else {"jev": ran.jev_dollars, "llm": ran.text_dollars})
        | ({} if unpriced else {"browser": browser}),
        actions=ran.actions,
        **ran.model_dump(
            include={
                "failure_class",
                "decisions",
                "step_cap",
                "decision_cap",
                "step_cap_unit",
                "stale",
                "last_exception",
                "last_offered",
                "final_page_url",
                "artifact",
                "text_helpers",
                "provenance",
            }
        ),
        decision_log=_scrubbed(ran.decision_log, task.secrets),
        unmetered_requests=ran.unmetered_requests,
        text_model=ran.text_model,
        observe_error=final.error,
        final_page_capture=final.frame,
        final_page_capture_viewport=final.frame_viewport,
        final_page_capture_withheld=final.frame_withheld,
    )
    return Outcome(None, None, final.url, None, final.controls, *_page_of(final)), report


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


class SiteWatch:
    """Each task site's start page fetched every `SITE_PROBE_SECONDS` through a run, by the harness itself.

    One day's the-internet.herokuapp.com held requests for 30 seconds at a time, a plain fetch included: attempts
    it held took 36s where the same task took 7s between stalls, for whichever arm drew them. A probe explains
    possible site trouble, but cannot prove what happened in another browser, so it never changes a grade."""

    def __init__(self, http: httpx.AsyncClient, tasks: Sequence[LiveTask]) -> None:
        self._http = http
        self._starts = {origin_of(t.start): t.start for t in reversed(tasks)}
        self._stalls: dict[str, list[tuple[float, float, str]]] = {origin: [] for origin in self._starts}
        self._pending: dict[str, tuple[float, asyncio.Event]] = {}

    async def run(self) -> None:
        await asyncio.gather(*(self._probe(origin, url) for origin, url in self._starts.items()))

    async def _probe(self, origin: str, url: str) -> None:
        while True:
            began = time.time()
            settled = asyncio.Event()
            self._pending[origin] = began, settled
            try:
                why = None
                try:
                    response = await self._http.get(url, follow_redirects=True, timeout=SITE_STALL_SECONDS * 3)
                    if response.status_code >= 500:
                        why = f"answered HTTP {response.status_code}"
                except httpx.TransportError as error:
                    why = f"unreachable ({type(error).__name__})"
                ended = time.time()
                if why is None and ended - began > SITE_STALL_SECONDS:
                    why = f"took {ended - began:.0f}s to answer"
                if why is not None:
                    self._stalls[origin].append((began, ended, why))
            finally:
                self._pending.pop(origin, None)
                settled.set()
            await asyncio.sleep(max(0.0, SITE_PROBE_SECONDS - (ended - began)))

    async def stalled(self, task: LiveTask, start: float, end: float) -> str | None:
        """Why the task's site stalled between `start` and `end`, or None if every fetch then was answered."""
        origin = origin_of(task.start)
        if (pending := self._pending.get(origin)) is not None and pending[0] < end:
            await pending[1].wait()
        for began, ended, why in self._stalls.get(origin, ()):
            if began < end and ended > start:
                return f"site stalled: {task.start} {why}"
        return None


async def hosted_messages(client: Any, session_id: str) -> list[Any]:
    """Every message of a hosted session, oldest first."""
    messages: list[Any] = []
    after = None
    while True:
        page = await client.sessions.messages(session_id, limit=100, **({"after": after} if after else {}))
        messages.extend(page.messages)
        if not page.messages or not getattr(page, "has_more", False):
            return messages
        after = str(page.messages[-1].id)


def answered_at(messages: Sequence[Any], output: object) -> datetime | None:
    """When the hosted agent answered, or None if it never did: its last `done` result that was not an error. An
    agent that answers without calling `done` (0.5.7's github-license, among others) replies in a message, which
    the session keeps as its output; that last reply is its answer."""
    done: datetime | None = None
    reply: datetime | None = None
    called_done = False
    for message in messages:
        if message.type == "completion_result":
            called_done = True
            if not json.loads(message.data or "{}").get("is_error"):
                done = max(done or message.created_at, message.created_at)
        elif message.type == "assistant_message":
            reply = max(reply or message.created_at, message.created_at)
    return done if called_done else reply if output else None


def _scrubbed(value: JsonValue, secrets: Mapping[str, str]) -> JsonValue:
    """`value` with every secret's text replaced by its name, as Browser Use's own placeholders name it."""
    if isinstance(value, str):
        for name, secret in secrets.items():
            if secret:
                value = value.replace(secret, f"<secret>{name}</secret>")
        return value
    if isinstance(value, list):
        return [_scrubbed(item, secrets) for item in value]
    if isinstance(value, dict):
        return {key: _scrubbed(item, secrets) for key, item in value.items()}
    return value


def _loaded(text: str | None) -> Any:
    try:
        return json.loads(text or "")
    except ValueError:
        return text


def _said(content: object) -> str | None:
    """The text an assistant message says, without its thinking signatures."""
    if isinstance(content, str):
        return content or None
    if not isinstance(content, list):
        return None
    parts = [
        str(part.get("text") or part.get("thinking") or "")
        for part in content
        if isinstance(part, dict) and part.get("type") in {"text", "thinking"}
    ]
    return "\n".join(p for p in parts if p) or None


REPEAT_BYTES = 1024
"""A repeated observation at least this long is stored once, later steps naming the step that holds it."""
CURRENT_URL = re.compile(r"^Current URL: (\S+)$", re.MULTILINE)


def hosted_steps(messages: Sequence[Any], secrets: Mapping[str, str]) -> tuple[list[JsonValue], list[str]]:
    """The hosted agent's steps, in the evidence shape fastbrowse's `step_log` and `trace` have: each tool call with
    its arguments, the URL it went to or reported, what came back (the page text it read, the values a script
    returned, its `done` answer), and each reply. Message and call ids, screenshot links (presigned, so a credential),
    thinking signatures and every secret's text are left out; `secrets` maps each secret's name to its value."""
    calls: dict[str, dict[str, JsonValue]] = {}
    steps: list[JsonValue] = []
    for message in messages:
        data = _loaded(message.data)
        kind, summary = message.type or "", getattr(message, "summary", None)
        if not isinstance(data, dict):
            steps.append({"index": len(steps), "operation": kind or "message", "outcome": "said", "note": data})
            continue
        if data.get("role", getattr(message, "role", None)) in {"user", "human"}:
            continue
        for call in data.get("tool_calls") or ():
            function = call.get("function") or {}
            calls[str(call.get("id"))] = {
                "operation": function.get("name"),
                "target": json_value(_loaded(function.get("arguments"))),
            }
        if data.get("role") == "tool":
            call = calls.pop(str(data.get("tool_call_id")), {"operation": data.get("tool_name"), "target": None})
            content = data.get("content")
            target = call["target"]
            reported = CURRENT_URL.search(content) if isinstance(content, str) else None
            url = reported.group(1) if reported else target.get("url") if isinstance(target, dict) else None
            timing = data.get("metadata") or {}
            started, ended = timing.get("step_start_time"), timing.get("step_end_time")
            steps.append(
                {
                    "index": len(steps),
                    "operation": call["operation"],
                    "outcome": "error" if data.get("is_error") or kind.endswith("_error") else "executed",
                    "url": url,
                    "target": target,
                    "note": summary,
                    "observation": json_value(content),
                    "duration_ms": round((ended - started) * 1000)
                    if isinstance(started, int | float) and isinstance(ended, int | float)
                    else None,
                }
            )
        elif (said := _said(data.get("content"))) is not None:
            steps.append({"index": len(steps), "operation": "reply", "outcome": "said", "note": said})
    for call in calls.values():  # a call the session never answered
        steps.append(
            {"index": len(steps), "operation": call["operation"], "outcome": "no result", "target": call["target"]}
        )
    return _compact_steps(steps, secrets)


def _compact_steps(steps: list[JsonValue], secrets: Mapping[str, str]) -> tuple[list[JsonValue], list[str]]:
    """Steps with every secret's text replaced by its name, and their one-line trace."""
    # Lossless: a large observation repeated verbatim (the same screenshot or page read again) is kept once.
    first: dict[str, int] = {}
    for step in steps:
        if isinstance(step, dict) and "observation" in step:
            text = json.dumps(step["observation"], sort_keys=True)
            index = step["index"]
            assert isinstance(index, int)
            if len(text) >= REPEAT_BYTES and (earlier := first.setdefault(text, index)) != index:
                del step["observation"]
                step["observation_same_as_step"] = earlier
    scrubbed = _scrubbed(steps, secrets)
    assert isinstance(scrubbed, list)
    trace = [f"{step['operation']}  -> {step['outcome']}" for step in scrubbed if isinstance(step, dict)]
    return scrubbed, trace


HOSTED_SETUP = "the provider connects its agent to the browser and opens the first page inside its own first step"


def hosted_timing(
    created_after: float, session: Any, browser_started: datetime | None, answered: datetime | None, wall: float
) -> dict[str, Any]:
    """A hosted run's seconds on the rule every arm follows, read on the server's own clock: its browser started, then
    its agent answered. `created_after` is the client's wait for the session to exist and `wall` the client's time
    from asking for it to the session reporting stopped, as much as two minutes past the answer on Browser Use's side.
    A session that used no browser opened none, so its agent's time runs from the session existing."""
    if browser_started is None:
        ran = wall - created_after
        if answered is not None:
            ran = min(ran, max(0.0, (answered - session.created_at).total_seconds()))
        return {
            "seconds": round(max(0.0, ran), 2),
            "timing_basis": TIMING_BASIS,
            "session_open_seconds": round(created_after, 2),
        }
    opened = created_after + max(0.0, (browser_started - session.created_at).total_seconds())
    ran = wall - opened
    if answered is not None:
        ran = min(ran, max(0.0, (answered - browser_started).total_seconds()))
    return {
        "seconds": round(max(0.0, ran), 2),
        "timing_basis": TIMING_BASIS,
        "session_open_seconds": round(opened, 2),
    }


def _screen(task: LiveTask) -> tuple[int, int] | None:
    """The real screen every cloud arm starts on: a navigation task's is the viewport every arm that can be given one
    runs at (`final_page.VIEWPORT`), so its page lays out at one width; any other task keeps Browser Use's default."""
    if task.category is not Category.NAVIGATE:
        return None
    return VIEWPORT["width"], VIEWPORT["height"]


async def _hosted_browser_started(client: Any, session_id: str) -> datetime | None:
    """When the browser of a hosted session started on the server's clock, once Browser Use lists it with an address."""
    from browser_use_sdk.v3 import BrowserUseError  # an optional extra

    with suppress(BrowserUseError, httpx.HTTPError):
        listed = await client.browsers.list(agent_session_id=session_id)
        for browser in listed.items:
            if browser.cdp_url and str(browser.agent_session_id) == session_id:
                return browser.started_at
    return None


async def hosted_session_ready(
    client: Any, session_id: str | None, finishing: "asyncio.Future[Any]"
) -> datetime | None:
    """Follow a hosted session until its browser is listed, and mark the attempt's session ready then: a provider that
    is slow to start its browser spends none of its agent's limit. A session that ended first is asked once more, as
    its kept-alive browser is still listed; a session that never had a browser returns None."""
    if session_id is None:
        return None
    while not finishing.done():
        if (started := await _hosted_browser_started(client, session_id)) is not None:
            session_ready()
            return started
        await asyncio.wait({finishing}, timeout=HOSTED_READY_POLL_SECONDS)
    return await _hosted_browser_started(client, session_id)


async def hosted_browser(client: Any, http: httpx.AsyncClient, session_id: str) -> str | FinalPage:
    """The DevTools websocket of the browser a hosted agent session created, or a `FinalPage` saying why there is
    none: Browser Use lists that browser under the session's id, and a kept-alive session leaves it running."""
    try:
        listed = await client.browsers.list(agent_session_id=session_id)
    except Exception as exc:
        return FinalPage(error=f"the hosted session's browser could not be listed ({type(exc).__name__})")
    addresses = [b.cdp_url for b in listed.items if b.cdp_url and str(b.agent_session_id) == session_id]
    if len(addresses) != 1:
        return FinalPage(error=f"expected one browser for the hosted session, found {len(addresses)}")
    try:
        return await devtools_websocket(http, addresses[0])
    except Exception as exc:
        return FinalPage(error=f"the hosted browser's DevTools address did not answer ({type(exc).__name__})")


async def hosted_arm(
    task: LiveTask, http: httpx.AsyncClient, *, record: Path | None, arm: str = "browser-use"
) -> tuple[Outcome, ArmReport]:
    """One run of Browser Use's hosted agent through its V4 API, on the model `HOSTED_MODELS` names for `arm`."""
    from browser_use_sdk.v4 import AsyncBrowserUse, BrowserUseError  # an optional extra

    client = AsyncBrowserUse(api_key=backend.browser_key())
    try:
        return await _hosted_run(client, task, http, record=record, arm=arm)
    # The SDK's message quotes the response body, which can echo the key: report the status or type alone.
    except BrowserUseError as error:
        failed = Unavailable if error.status_code in RETRYABLE_STATUS else RuntimeError
        raise failed(f"Browser Use API returned HTTP {error.status_code}") from None
    except httpx.HTTPError as error:
        failed = Unavailable if isinstance(error, TRANSIENT_TRANSPORT) else RuntimeError
        raise failed(f"Browser Use API request failed ({type(error).__name__})") from None
    finally:
        await client.close()


def hosted_request(task: LiveTask, arm: str, *, record: bool) -> dict[str, Any]:
    """The V4 run every hosted arm asks for, its model aside identical: US proxy (a bu-ultrafast session has none
    unless asked), the task's schema, its login as secret bindings, an explicit uncapped cost limit, a browser kept
    open for the harness to read, and the screen every cloud arm starts on."""
    width, height = _screen(task) or (None, None)
    return {
        **({"model": model} if (model := HOSTED_MODELS[arm]) is not None else {}),
        "output_schema": None if task.output_schema is None else task.output_schema.model_json_schema(),
        "secret_bindings": hosted_secrets(task),
        # Always explicit: omitted, Browser Use would apply its own default cost limit (see HOSTED_UNCAPPED_USD).
        "max_cost_usd": HOSTED_UNCAPPED_USD,
        "keep_alive": HOSTED_KEEP_ALIVE_SECONDS,
        "browser_settings": {
            "proxyCountryCode": "us",
            "record": record,
            **({} if width is None else {"screenWidth": width, "screenHeight": height}),
        },
    }


def hosted_secrets(task: LiveTask) -> list[dict[str, Any]] | None:
    """The task's login as V4 secret bindings, each typed only on the task's start host, as fastbrowse types its
    secrets only on the start origin. The agent asks for a value by its name and is never shown it."""
    if not task.secrets:
        return None
    host = urlsplit(task.start).hostname
    return [
        {"alias": name, "source": {"type": "inline", "value": value}, "allowedDomains": [host]}
        for name, value in task.secrets.items()
    ]


async def _hosted_run(
    client: Any, task: LiveTask, http: httpx.AsyncClient, *, record: Path | None, arm: str
) -> tuple[Outcome, ArmReport]:
    from browser_use_sdk.v4 import BrowserUseError  # an optional extra

    started = time.monotonic()
    try:
        created = await client.runs.create(prompt(task), **hosted_request(task, arm, record=record is not None))
    except BrowserUseError as error:
        if refusal(error.status_code):
            raise NotStarted(f"Browser Use hosted agent returned HTTP {error.status_code}") from None
        raise
    created_after = time.monotonic() - started
    events: list[Any] = []
    stopped: list[bool] = []
    try:
        await _hosted_follow(client, created.id, events, arm, task)
        session_answered()
        wall = time.monotonic() - started
        run = await client.runs.get(created.id)
        return await _hosted_finish(
            client, http, task, record, run, events, created_after, wall, stopped, run_id=str(created.id)
        )
    except (asyncio.CancelledError, Exception):
        # A cancelled attempt or failed poll must stop billing before an outage retry starts.
        with suppress(BrowserUseError, httpx.HTTPError):
            await asyncio.shield(client.runs.cancel(created.id))
        raise
    finally:
        # The kept-alive browser bills until it is stopped, so it is stopped on every path out.
        if not stopped and (browser_id := _hosted_browser_id(events)) is not None:
            with suppress(BrowserUseError, httpx.HTTPError):
                await asyncio.shield(client.browsers.stop(browser_id))


async def _hosted_follow(client: Any, run_id: Any, events: list[Any], arm: str, task: LiveTask) -> None:
    """Read a hosted run's events into `events` until it ends, as Browser Use documents: the status first, then every
    page of events, so the last read follows the end. The attempt's session is ready once the run's browser is.

    A run just created can be invisible to the status endpoint for a moment, so a 404 before the first status read is
    retried for `HOSTED_VISIBILITY_SECONDS`, as the SDK's own `wait_for_completion` does; after it, a 404 is final."""
    from browser_use_sdk.v4 import BrowserUseError  # an optional extra

    after = None
    visible_by = time.monotonic() + HOSTED_VISIBILITY_SECONDS
    seen = False
    while True:
        try:
            status = (await client.runs.status(run_id)).status.value
        except BrowserUseError as error:
            if error.status_code != 404 or seen or time.monotonic() >= visible_by:
                raise
            await asyncio.sleep(HOSTED_VISIBILITY_RETRY_SECONDS)
            continue
        seen = True
        while True:
            page = await client.runs.events(run_id, after=after)
            for event in page.events:
                events.append(event)
                if event.type in BROWSER_READY:
                    session_ready()
                    _watch(arm, task, event.data.get("live_view_url"))
            after = page.next_after if page.next_after is not None else after
            if not page.has_more:
                break
        if status in HOSTED_TERMINAL:
            return
        await asyncio.sleep(HOSTED_POLL_SECONDS)


def _hosted_browser_id(events: Sequence[Any]) -> str | None:
    return next((str(e.data["browser_session_id"]) for e in events if e.type in BROWSER_READY), None)


def _hosted_event_at(events: Sequence[Any], kinds: set[str]) -> datetime | None:
    return next((e.ts for e in events if e.type in kinds), None)


async def hosted_final_page(
    client: Any, http: httpx.AsyncClient, browser_id: str | None, *, secrets: Mapping[str, str] | None = None
) -> FinalPage:
    """The page the hosted agent's browser ended on, read through that browser's DevTools address while its
    `keepAlive` holds it open."""
    if browser_id is None:
        return FinalPage(error="the hosted run opened no browser")
    try:
        browser = await client.browsers.get(browser_id)
        if not browser.cdp_url:
            return FinalPage(error="the hosted run's browser has no DevTools address")
        websocket = await devtools_websocket(http, browser.cdp_url)
    except Exception as exc:
        return FinalPage(error=f"the hosted run's browser did not answer ({type(exc).__name__})")
    return await observe_browser(websocket, secrets=secrets)


def hosted_v4_steps(events: Sequence[Any], secrets: Mapping[str, str]) -> tuple[list[JsonValue], list[str]]:
    """The hosted agent's steps from its V4 events, in the evidence shape `hosted_steps` gives: each tool call with its
    input, what came back and how long it took between the events that opened and settled it, and each reply.
    Every secret's text is left out; `secrets` maps each secret's name to its value."""
    steps: list[JsonValue] = []
    calls: dict[str, tuple[dict[str, JsonValue], datetime | None]] = {}
    for event in events:
        part = event.data.get("part") if event.type == "core.event" else None
        if not isinstance(part, dict):
            continue
        if part.get("type") == "text" and part.get("text"):
            steps.append({"index": len(steps), "operation": "reply", "outcome": "said", "note": part["text"]})
        if part.get("type") != "tool":
            continue
        state = part.get("state") or {}
        settled = state.get("status") in {"completed", "error"}
        if (call := calls.get(str(part.get("callID")))) is None:
            step: dict[str, JsonValue] = {
                "index": len(steps),
                "operation": part.get("tool"),
                "outcome": "no result",
                "target": json_value(state.get("input")),
            }
            # A call first seen already settled has no opening event, so its time is unknown, not zero.
            calls[str(part.get("callID"))] = call = step, None if settled else event.ts
            steps.append(step)
        step, opened = call
        if settled:
            step["outcome"] = "executed" if state["status"] == "completed" else "error"
            step["observation"] = json_value(state.get("output", state.get("error")))
            if opened is not None:
                step["duration_ms"] = round((event.ts - opened).total_seconds() * 1000)
    return _compact_steps(steps, secrets)


async def _hosted_finish(
    client: Any,
    http: httpx.AsyncClient,
    task: LiveTask,
    record: Path | None,
    run: Any,
    events: Sequence[Any],
    created_after: float,
    wall: float,
    stopped: list[bool],
    *,
    run_id: str,
) -> tuple[Outcome, ArmReport]:
    from browser_use_sdk.v4 import BrowserUseError  # an optional extra

    dispatch_failed = any(e.type == "run.dispatch_failed" for e in events)
    browser_id = _hosted_browser_id(events)
    final = (
        FinalPage(error="the hosted run could not be dispatched")
        if dispatch_failed
        else await hosted_final_page(
            client, http, browser_id, secrets=task.secrets if not task.bitwarden_item else None
        )
    )
    if isinstance(run.output, dict | list):
        answer, data = json.dumps(run.output), (run.output if isinstance(run.output, dict) else None)
    else:
        answer, data = run.result or None, None
    status = run.status.value
    answered = _hosted_event_at(events, {"run.completed"}) if status == "completed" and answer else None
    # The key is scrubbed too, should a page or a script ever echo it.
    step_log, trace = hosted_v4_steps(events, {**task.secrets, "browser_use_api_key": backend.browser_key()})
    outcome = Outcome(answer, data, final.url, None, final.controls, *_page_of(final))
    # Stopping ends browser time but does not finalize delayed usage, so this read is a timestamped snapshot.
    closing = time.monotonic()
    if browser_id is not None:
        with suppress(BrowserUseError, httpx.HTTPError):
            await client.browsers.stop(browser_id)
            stopped.append(True)
    closed = round(time.monotonic() - closing, 2)
    cost = None
    observed_at = datetime.now(UTC)
    if stopped or browser_id is None:
        with suppress(BrowserUseError, httpx.HTTPError):
            cost = await client.sessions.cost(run.session_id)
            observed_at = datetime.now(UTC)
    provisional = (
        None
        if cost is None
        else ProvisionalHostedCost(
            observed_at=observed_at,
            session_id=str(run.session_id),
            run_id=run_id,
            browser_session_id=browser_id,
            total_cost_usd=str(cost.total_cost_usd),
            llm_cost_usd=str(cost.llm_cost_usd),
            search_cost_usd=str(cost.search_cost_usd),
            browser_cost_usd=str(cost.browser_cost_usd),
            proxy_cost_usd=str(cost.proxy_cost_usd),
        )
    )
    provider_error = _scrubbed(run.error, {**task.secrets, "browser_use_api_key": backend.browser_key()})
    report = ArmReport(
        status=status,
        answered=answered is not None,
        **hosted_timing(created_after, run, _hosted_event_at(events, BROWSER_READY), answered, wall),
        session_close_seconds=closed,
        session_seconds=round(wall, 2),
        # A stopped browser can acquire delayed proxy charges, so this snapshot cannot price the whole attempt.
        dollars=None,
        unknown_cost=True,
        provisional_hosted_cost=provisional,
        error=provider_error if isinstance(provider_error, str) else None,
        model=run.model,
        provenance={
            "model_params": json_value(run.model_params),
            "browser_session_id": browser_id,
            "run_id": run_id,
            "provider_status": status,
        },
        steps=sum(1 for e in events if e.type == "core.event" and e.data.get("type") == "step_start"),
        llm_calls=sum(1 for e in events if e.type == "llm.response"),
        llm_calls_basis="physical_requests",
        input_tokens=run.total_input_tokens,
        output_tokens=run.total_output_tokens,
        session_id=str(run.session_id),
        timing_note=HOSTED_SETUP,
        step_log=step_log,
        trace=trace,
        events=[
            _scrubbed(
                json_value({"type": event.type, "ts": event.ts, "data": event.data}),
                {**task.secrets, "browser_use_api_key": backend.browser_key()},
            )
            for event in events
        ]
        if dispatch_failed
        else [],
        observe_error=final.error,
        final_page_capture=final.frame,
        final_page_capture_viewport=final.frame_viewport,
        final_page_capture_withheld=final.frame_withheld,
    )
    if dispatch_failed:
        # Dispatch can open a billable browser, so preserve its usage snapshot before retrying the outage.
        raise HostedUnavailable(report)
    if record is not None and browser_id is not None and stopped:
        await _hosted_recording(client, http, browser_id, record)
    return outcome, report


async def _hosted_recording(client: Any, http: httpx.AsyncClient, browser_id: str, record: Path) -> None:
    """Save a stopped browser's video once Browser Use has processed it, for up to a minute."""
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        browser = await client.browsers.get(browser_id)
        if browser.recording_url:
            response = await http.get(browser.recording_url, follow_redirects=True)
            if response.is_success:
                await asyncio.to_thread(record.write_bytes, response.content)
            return
        if browser.recording_available is False:
            return
        await asyncio.sleep(2)


def _video(record: Path | None) -> str | None:
    # A claimed name stays empty when the run recorded nothing.
    return str(record) if record is not None and record.exists() and record.stat().st_size else None


async def _github_token(request: httpx.Request) -> None:
    # An answer key is fetched again on every retry, and a Jev outage retries rows for hours, so the anonymous
    # 60 an hour ran out and failed github-license on a 403. A token raises that to 5000.
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    if token and request.url.host == "api.github.com":
        request.headers["Authorization"] = f"Bearer {token}"


def _rate_limit_wait(exc: httpx.HTTPError) -> float | None:
    """Seconds until a spent rate limit resets, or None when the failure is not one.

    GitHub answers a spent limit with 403, not 429, and names the reset in `x-ratelimit-reset`. A runner that
    shares its address (every Railway job behind one egress) spends the anonymous 60 an hour quickly.
    """
    if not isinstance(exc, httpx.HTTPStatusError) or exc.response.status_code != 403:
        return None
    headers = exc.response.headers
    if headers.get("x-ratelimit-remaining") != "0" and headers.get("retry-after") is None:
        return None
    if (after := headers.get("retry-after")) is not None and after.isdigit():
        return float(after)
    reset = headers.get("x-ratelimit-reset")
    return max(float(reset) - time.time(), 0.0) + 1 if reset and reset.isdigit() else 60.0


async def _truth(task: LiveTask, http: httpx.AsyncClient) -> object:
    # Answer keys come from public APIs that rate-limit, so a transient failure is waited out, never a crashed eval.
    for retries in itertools.count():
        try:
            return await task.truth(http)
        except httpx.HTTPError as exc:
            status = exc.response.status_code if isinstance(exc, httpx.HTTPStatusError) else None
            if (limited := _rate_limit_wait(exc)) is not None:
                print(f"RETRY truth         {task.id:20} in {limited:.0f}s: rate limit spent", flush=True)
                await asyncio.sleep(limited)
                continue
            if status not in RETRYABLE_STATUS and not isinstance(exc, TRANSIENT_TRANSPORT):
                raise
            wait = min(30 * (retries + 1), 300)
            print(f"RETRY truth         {task.id:20} in {wait}s: {type(exc).__name__} {status or ''}", flush=True)
            await asyncio.sleep(wait)


def _crashed(
    arm: str, task: LiveTask, failure: str, *, at: float, seconds: float, status: str | None, record: Path | None
) -> EvalRow:
    """A crashed attempt's row. It may have spent before it failed, so its cost is unknown, never $0."""
    completed = _completed_attempt.get()
    if completed is not None and completed.arm == arm and completed.task == task.id:
        return completed.model_copy(update={"failure": failure, "unknown_cost": True})
    artifact = _attempt_artifact.get() if arm in {"cua-codex", "browser-harness"} else None
    return EvalRow(
        arm=arm,
        artifact=artifact,
        model={"cua-codex": CUA_MODEL, "browser-harness": HARNESS_MODEL}[arm] if artifact else None,
        unknown_cost=True,
        llm_calls_basis="physical_requests" if arm in FASTBROWSE_ARMS else None,
        task=task.id,
        category=task.category.value,
        at=at,
        status=status,
        normalized_status=normalize(status),
        correct=None,
        passed=None,
        failure=failure,
        seconds=round(seconds, 1),
        timing_basis=TIMING_BASIS,
        dollars=None,
        video=_video(record),
    )


def crash_status(exc: Exception) -> tuple[str, str | None]:
    """How a crashed attempt is recorded: a provider outage is unavailable, anything else the arm's own error."""
    status = Status.UNAVAILABLE.value if isinstance(exc, (Unavailable, *TRANSIENT_TRANSPORT)) else None
    return f"{type(exc).__name__}: {exc}", status


async def _site_checked(row: EvalRow, task: LiveTask, http: httpx.AsyncClient) -> EvalRow:
    """`row` with its ending settled by the harness's own look at the site, the same rule for every arm and however
    the arm named the failure: a failed attempt that took no step while the start page is down is an outage, since
    the site stopped it before the agent could act. A failed attempt that acted is the agent's result whatever the
    site does now, and only notes the probe. An ending the arm or the judge already found unavailable stands: the
    harness's fetch answering says nothing of the arm's own browser."""
    if row.passed or row.normalized_status == Ending.UNAVAILABLE or not (down := await _down(task, http)):
        return row
    if row.steps == 0:
        return row.model_copy(update={"normalized_status": Ending.UNAVAILABLE, "site_probe": down})
    return row.model_copy(update={"site_probe": down})


async def run_arm(
    arm: str,
    task: LiveTask,
    truth: object,
    http: httpx.AsyncClient,
    downloads: Path,
    *,
    record: Path | None,
    request_session: RequestSession | None = None,
) -> EvalRow:
    _running.set(f"{arm} {task.id}")
    _attempt_artifact.set(None)
    _completed_attempt.set(None)
    at = time.time()
    clock = AttemptClock(bounded=True)
    try:
        async with clock:
            if request_session is not None:
                request_session.check_admission()
            outcome, report = await ARMS[arm].runner(
                task,
                http,
                downloads,
                record=record,
                **({} if request_session is None else {"request_session": request_session}),
            )
    except Exception as exc:  # Preserve a crashed attempt without inventing a semantic verdict.
        # A provider that never opened or closed the session measures no agent: an outage, retried as any other.
        failure = Unavailable(f"the browser session did not {clock.phase} in time") if clock.expired() else exc
        error, status = crash_status(failure)
        crashed = _crashed(
            arm,
            task,
            error,
            at=at,
            seconds=clock.seconds(),
            status=status,
            record=record,
        )
        if isinstance(exc, HostedUnavailable):
            receipt = exc.report.model_dump(exclude={"status"})
            if request_session is not None:
                receipt = request_session.public_json(json_value(receipt))
            return EvalRow.model_validate(crashed.model_dump() | receipt)
        if isinstance(exc, SpendStop):
            return crashed.model_copy(
                update={"execution_admission_stopped": True, "execution_admission_reason": str(exc)}
            )
        # A refused start created and billed nothing, so its charge is a known zero rather than unknown.
        refused = {"started": False, "dollars": 0.0, "unknown_cost": False}
        return crashed.model_copy(update=refused) if isinstance(exc, NotStarted) else crashed
    if request_session is not None:
        report = ArmReport.model_validate(request_session.public_json(json_value(report.model_dump())))
    completed = EvalRow.model_validate(
        report.model_dump()
        | {
            "arm": arm,
            "task": task.id,
            "category": task.category.value,
            "at": at,
            "normalized_status": ending_of(arm, report),
            "correct": None,
            "passed": None,
            "failure": None,
            "answer": outcome.answer,
            "data": outcome.data,
            "final_url": outcome.final_url,
            "final_page": outcome.evidence,
            "final_controls": outcome.controls,
            "video": _video(record),
        }
    )
    _completed_attempt.set(completed)
    # The judge sees the task's expected ending and a report that cannot name the arm (`blind_report`), so the pool arm
    # (Fastbrowse on another browser) is judged exactly as Fastbrowse is; its ending is normalized as non-hosted too.
    ending = ending_of(arm, report)
    judge_client: RequestJudgeClient | None = None
    try:
        if request_session is None:
            semantic_pass, semantic_reason, judge_receipt = await semantic_grade(task, truth, outcome, report, ending)
        else:
            request_session.check_admission()
            judge_client = request_session.judge_client()
            async with judge_client:
                semantic_pass, semantic_reason, judge_receipt = await semantic_grade(
                    task, truth, outcome, report, ending, client=judge_client
                )
    except SpendStop as exc:
        known, unknown = (0.0, 0) if judge_client is None else judge_client.partial_spend
        return completed.model_copy(
            update={
                "execution_admission_stopped": True,
                "execution_admission_reason": str(exc),
                "known_judge_dollars": known,
                "unpriced_judge_attempts": unknown,
            }
        )
    except BaseException:
        if judge_client is not None:
            known, unknown = judge_client.partial_spend
            _completed_attempt.set(
                completed.model_copy(
                    update={
                        "known_judge_dollars": known,
                        "unpriced_judge_attempts": unknown,
                    }
                )
            )
        raise
    return EvalRow.model_validate(
        completed.model_dump()
        | graded(arm, report, outcome.evidence, semantic_pass, semantic_reason, judge_receipt)
        | {
            "execution_admission_stopped": request_session is not None and request_session.stop_reason is not None,
            "execution_admission_reason": None if request_session is None else request_session.stop_reason,
            "seconds": round(report.seconds, 1),
            "dollars": report.dollars,
        }
    )


def graded(
    arm: str,
    report: ArmReport,
    evidence: PageEvidence | None,
    passed: bool | None,
    reason: str,
    receipt: dict[str, JsonValue],
) -> dict[str, object]:
    """The row fields a judgment settles: the verdict, the receipt and its cost, and the ending it implies. A failed
    run whose browser transport ended it, or whose final page answered with a retryable HTTP status, and any run the
    judge found unobservable, is `unavailable`: an outage to wait out, not an agent result."""
    ending = ending_of(arm, report)
    failure = reason if passed is not True else None
    failure_class = report.failure_class
    if failure is not None and failure_class == BROWSER_TRANSPORT:
        ending = Ending.UNAVAILABLE
    if failure is not None and failure_class is None and evidence is not None:
        status = evidence.status
        if status is not None and (status in {408, 419, 429} or status >= 500):
            ending = Ending.UNAVAILABLE
            failure_class = "site_http"
    if judged_unavailable(receipt):
        ending = Ending.UNAVAILABLE
    return {
        "normalized_status": ending,
        "correct": passed,
        "passed": passed,
        "failure": failure,
        "semantic_judge": receipt,
        "judge_dollars": receipt["judge_cost_usd"],
        "known_judge_dollars": receipt["known_judge_cost_usd"],
        "unpriced_judge_attempts": receipt["unpriced_judge_attempts"],
        "failure_class": failure_class,
    }


async def _fast_report(
    task: LiveTask,
    http: httpx.AsyncClient,
    downloads: Path,
    *,
    record: Path | None,
    cdp_url: str | None = None,
    arm: str = "fastbrowse",
    request_session: RequestSession | None = None,
) -> tuple[Outcome, ArmReport]:
    with traced() as events:
        outcome, result, seen = await fast_arm(
            task,
            http,
            downloads,
            record=record,
            cdp_url=cdp_url,
            arm=arm,
            **({} if request_session is None else {"request_session": request_session}),
        )
    # The cloud browser the harness started is part of what the run cost, as fastbrowse's own cloud browser was.
    cost = CostBreakdown(lines=(*result.cost.lines, *seen.browser))
    lost = transient_seconds(events, seen.began, seen.ended)
    route = providers(events)
    return outcome, ArmReport(
        status=result.status.value,
        seconds=seen.ended - seen.began,
        session_open_seconds=seen.open_seconds,
        session_close_seconds=seen.close_seconds,
        browser_seconds=phase_seconds(events, BROWSER_PHASES),
        agent_seconds=agent_seconds(seen.ended - seen.began, phase_seconds(events, BROWSER_PHASES)),
        transient_seconds=round(lost, 2),
        # An unknown line makes the known total a floor, not a cost.
        dollars=None if cost.has_unknown else cost.known_dollars,
        error=result.error,
        citations=[citation.model_dump(mode="json") for citation in result.citations],
        trace=[f"{s.operation.value} {s.target or ''} -> {s.outcome.value}" for s in result.steps],
        # Enough to say why a run failed without running it again: every step as the agent judged it, and
        # each read, done check, verification, claim check and recovery in order.
        step_log=[s.model_dump(mode="json") for s in result.steps],
        events=events,
        steps=len(result.steps),
        step_cap=AGENT_LIMITS.max_steps,
        step_cap_unit="steps",
        failure_class=BROWSER_TRANSPORT if TRANSPORT_ERROR.search(result.error or "") else None,
        unknown_cost=cost.has_unknown,
        observe_error=seen.final.error,
        final_page_capture=seen.final.frame,
        final_page_capture_viewport=seen.final.frame_viewport,
        final_page_capture_withheld=seen.final.frame_withheld,
        seconds_by_call=cost.seconds_by_call(),
        **efficiency(cost),
        model=backend.providers(),
        text_model=llm_models(route),
        # A hosted run can be watched and replayed from its row, as a Browser Use V4 row can.
        provenance={
            "providers": route,
            **({} if seen.browser_session_id is None else {"browser_session_id": seen.browser_session_id}),
            **({} if seen.live_url is None else {"live_url": seen.live_url}),
        },
        cost_by_component=_known_components(cost.lines),
    )


BROWSER_PHASES = ("act", "observe", "capture")
"""fastbrowse's trace phases that are a round trip to the browser. `open` is setup, already off the clock."""


def agent_seconds(seconds: float, browser: float | None) -> float | None:
    """fastbrowse's seconds without its round trips to the browser. Other arms' calls cannot be timed the same way, so
    they have None and show as not measured."""
    return None if browser is None else round(max(seconds - browser, 0.0), 2)


def phase_seconds(events: Sequence[object], phases: Sequence[str]) -> float:
    """How long a fastbrowse trace spent in `phases`."""
    return round(
        sum(
            event["ended"] - event["began"]
            for event in events
            if isinstance(event, dict) and event.get("event") == "phase" and event.get("phase") in phases
        ),
        2,
    )


def llm_models(route: str | None) -> str | None:
    """The LLM models a fastbrowse run's `route` trace event names ("Jev <source> (...); LLM <models> via <provider>"),
    the one place a run states them."""
    if route is None or "; LLM " not in route:
        return None
    return route.split("; LLM ", 1)[1].rsplit(" via ", 1)[0]


def efficiency(cost: CostBreakdown) -> Efficiency:
    """Tokens, calls and model throughput over the LLM calls' own seconds (never the run's wall clock).

    Mirrors fastbrowse.evals.runner._efficiency so a Parallax row carries the same measures the site's
    normalized speed and cost columns read. A run whose calls recorded no seconds reports None, not 0.
    """
    llm = [line for line in cost.lines if line.component is CostComponent.LLM]
    tokens_in = sum(line.input_tokens for line in llm)
    tokens_out = sum(line.output_tokens for line in llm)
    timed = [line for line in llm if line.seconds is not None]
    call_seconds = sum(line.seconds or 0.0 for line in timed)
    timed_tokens = sum(line.input_tokens + line.output_tokens for line in timed)
    return {
        "input_tokens": tokens_in,
        "output_tokens": tokens_out,
        # A release before request counts (0.9.x) records none: its LLM calls are unknown, never zero.
        "llm_calls": (
            sum(getattr(line, "physical_requests", None) or 0 for line in llm)
            if all(getattr(line, "physical_requests", None) is not None for line in llm)
            else None
        ),
        "llm_calls_basis": "physical_requests",
        "jev_calls": sum(1 for line in cost.lines if line.component is CostComponent.JEV),
        "tokens_per_second": round(timed_tokens / call_seconds, 1) if call_seconds > 0 else None,
    }


class ArmSpec(BaseModel):
    model_config = ConfigDict(arbitrary_types_allowed=True, frozen=True)

    runner: Callable[..., Awaitable[tuple[Outcome, ArmReport]]]
    pin: str
    env_allowlist: tuple[str, ...]
    tier: Literal["A", "hosted", "local"]
    default: bool = True
    prepare: Callable[[], Awaitable[None]] | None = None


_RUNTIME_ENV = ("PATH", "LANG", "LC_ALL", "SSL_CERT_FILE", "SSL_CERT_DIR")
OSS_PACKAGE = f"browser-use=={arm_pins.pinned('browser-use')}"
OSS_MODEL = "bu-fast"
"""browser-use 1.0's default model (`Agent(llm=None)`), a Browser Use gateway alias at its own reasoning effort."""
OSS_PIN = f"{OSS_PACKAGE}; default model {OSS_MODEL}, default mode (ultrafast on {OSS_MODEL})"
OSS_COMMAND = ("uv", "run", "--no-project", "--quiet", "--python", "3.12", "--with", OSS_PACKAGE, "python")
OSS_RUNNER = ULTRAFAST_RUNNER.with_name("browser_use_oss_arm.py")
CUA_RUNNER = ULTRAFAST_RUNNER.with_name("cua_codex_arm.py")
CUA_MODEL = "gpt-6-astra"
CODEX_PIN = f"codex-cli=={arm_pins.pinned('@openai/codex')}"
CUA_PIN = f"cua-driver=={arm_pins.pinned('cua-driver')}; {CODEX_PIN}"
HARNESS_RUNNER = ULTRAFAST_RUNNER.with_name("browser_harness_arm.py")
HARNESS_PACKAGE = f"browser-harness=={arm_pins.pinned('browser-harness')}"
HARNESS_PIN = f"{HARNESS_PACKAGE}; {CODEX_PIN}; cli+skill"
HARNESS_COMMAND = (
    "uv",
    "run",
    "--no-project",
    "--quiet",
    "--python",
    "3.12",
    "--with",
    HARNESS_PACKAGE,
    "python",
)
HARNESS_MODEL = "openai/gpt-6-luna"
HARNESS_EFFORT = "medium"
HARNESS_PRICES = {"input": 0.10, "cached_input": 0.01, "output": 0.50}
"""USD per million tokens for HARNESS_MODEL on OpenRouter (https://openrouter.ai/api/v1/models, checked 2026-10-30).
Codex reports tokens, not charges, so the arm's model spend is these list prices times its recorded usage."""


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
    command: tuple[str, ...],
    script: Path,
    request: Mapping[str, object],
    env: dict[str, str],
    cwd: str,
    *,
    graceful: bool = False,
) -> bytes:
    if not graceful:
        stdout = await run_command(
            (*command, str(script)),
            json.dumps(request).encode(),
            seconds=None,
            environment=env,
            cwd=cwd,
        )
        lines = stdout.splitlines()
        if not lines:
            raise RuntimeError(f"{script.name} exited without a result")
        return lines[-1]

    process = await asyncio.create_subprocess_exec(
        *command,
        str(script),
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        env=env,
        cwd=cwd,
        start_new_session=True,
    )
    try:
        stdout, _ = await process.communicate(json.dumps(request).encode())
    except BaseException:
        if process.returncode is None:
            with suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGTERM)
            try:
                async with asyncio.timeout(60):
                    await process.wait()
            except TimeoutError:
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
    setup_seconds: float = 0.0
    """Connecting and opening the first page, up to the agent's first step and not in `seconds`."""
    step_log: list[JsonValue] = []
    dollars: float | None = None
    error: str | None = None
    step_ceiling: int | None = None
    """The integer browser-use's `Agent.run` was handed in place of no limit (see browser_use_oss_arm.py)."""
    timeout_ceiling: float | None = None
    """The seconds browser-use's `Agent.run` was handed in place of no time limit."""


class _HarnessReport(BaseModel):
    status: Literal["done", "stopped", "error", "unavailable"]
    answer: str | None = None
    data: JsonValue = None
    error: str | None = None
    seconds: float
    steps: int
    dollars: float | None
    input_tokens: int
    cached_input_tokens: int
    output_tokens: int
    commands: list[JsonValue]
    provenance: dict[str, JsonValue]


class _CuaReport(_OssReport):
    status: Literal["done", "stopped", "budget_exceeded", "error", "unavailable"]
    final: FinalPage
    provenance: dict[str, JsonValue]
    session_open_seconds: float | None = None
    session_close_seconds: float | None = None


class _OssReady(BaseModel):
    cdp_url: str
    live_url: str | None = None
    browser_id: str
    open_seconds: float


class _OssResult(BaseModel):
    status: str
    answer: str | None = None
    data: JsonValue = None
    steps: int
    step_log: list[JsonValue] = []
    dollars: float | None = None
    error: str | None = None
    step_ceiling: int | None = None
    """The integer browser-use's `Agent.run` was handed in place of no limit (see browser_use_oss_arm.py)."""
    timeout_ceiling: float | None = None
    """The seconds browser-use's `Agent.run` was handed in place of no time limit."""


class _OssStopped(BaseModel):
    browser_id: str | None = None
    stopped: bool


OSS_SETUP = "the agent starts the Python worker that drives the browser, and checks its model gateway, inside its run"
OSS_LLM_COST = (
    "the Browser Use gateway's own charge for each model response, as browser-use sums it; Browser Use reports no "
    "billed total per run, so a request cut off before its charge came back would be missing"
)


@asynccontextmanager
async def _conversation(
    command: tuple[str, ...], script: Path, env: dict[str, str], cwd: str
) -> AsyncIterator[
    tuple[Callable[[Mapping[str, object]], Awaitable[None]], Callable[[str], Awaitable[dict[str, Any]]]]
]:
    """A runner that answers in JSON lines while it works: `send` writes one line to it, `receive(event)` reads its
    next line, which must be that event. On any way out the runner is ended: asked by closing its input, then
    terminated, which a runner owning a cloud browser answers by stopping it."""
    process = await asyncio.create_subprocess_exec(
        *command,
        str(script),
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        env=env,
        cwd=cwd,
        start_new_session=True,
        limit=64 * 1024 * 1024,  # a result line carries the whole step log
    )
    assert process.stdin is not None and process.stdout is not None
    stdin, stdout = process.stdin, process.stdout

    async def send(message: Mapping[str, object]) -> None:
        stdin.write(json.dumps(message).encode() + b"\n")
        await stdin.drain()

    async def receive(event: str) -> dict[str, Any]:
        line = await stdout.readline()
        if not line:
            await process.wait()
            raise RuntimeError(f"{script.name} exited {process.returncode} before its {event}")
        message = json.loads(line)
        if message.get("event") != event:
            raise _OssEvent(message)
        return message

    async def end(asked: bool) -> None:
        if asked:
            with suppress(OSError):
                stdin.close()
            with suppress(TimeoutError):
                async with asyncio.timeout(60):
                    await process.wait()
        if process.returncode is None:
            with suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGTERM)
            try:
                async with asyncio.timeout(60):
                    await process.wait()
            except TimeoutError:
                process.kill()
                await process.wait()

    try:
        yield send, receive
    except BaseException:
        await asyncio.shield(end(asked=False))
        raise
    await end(asked=True)


class _OssEvent(Exception):
    """The runner sent another event than the one expected: its result before its browser was ready."""

    def __init__(self, message: dict[str, Any]) -> None:
        super().__init__(message.get("event"))
        self.message = message


async def oss_arm(task: LiveTask, http: httpx.AsyncClient, *, record: Path | None) -> tuple[Outcome, ArmReport]:
    """browser-use 1.0 on its own default Browser Use Cloud browser (see browser_use_oss_arm.py). It starts and stops
    that browser itself; the harness times the run on its own clock from the browser being ready to the answer, reads
    the final page before the stop, and reads the browser's cost as Browser Use metered it."""
    if record is not None:
        raise ValueError("browser-use-oss recording is not supported by this adapter")
    request: dict[str, object] = {
        "goal": prompt(task),
        # Uncapped; the runner substitutes browser-use's own integer seams (STEP_CEILING, TIMEOUT_CEILING) and
        # reports them.
        "max_steps": None,
        "browser_minutes": CLOUD_BROWSER_MINUTES,
        "output_schema": task.output_schema.model_json_schema() if task.output_schema else None,
    }
    ready: _OssReady | None = None
    stopped = _OssStopped(stopped=False)
    try:
        with tempfile.TemporaryDirectory(prefix="bu-oss-") as runtime:
            env = arm_environment("browser-use-oss", runtime)
            # 1.0's default model and browser are both Browser Use's, read with the Browser Use key.
            env["BROWSER_USE_API_KEY"] = backend.browser_key()
            async with _conversation(OSS_COMMAND, OSS_RUNNER, env, runtime) as (send, receive):
                await send(request)
                try:
                    ready = _OssReady.model_validate(await receive("ready"))
                except _OssEvent as early:
                    raise NotStarted(f"browser-use did not start its browser: {early.message.get('error')}") from None
                began = session_ready()
                _watch("browser-use-oss", task, ready.live_url)
                ran = _OssResult.model_validate(await receive("result"))
                seconds = session_answered() - began
                final = await observe_browser(
                    await devtools_websocket(http, ready.cdp_url),
                    secrets=task.secrets if not task.bitwarden_item else None,
                )
                closing = time.monotonic()
                await send({})
                stopped = _OssStopped.model_validate(await receive("stopped"))
                close_seconds = round(time.monotonic() - closing, 2)
    finally:
        if ready is not None and not stopped.stopped:
            await asyncio.shield(stop_cloud_browser(ready.browser_id))
    browser, unpriced = _browser_dollars(await metered_cost(ready.browser_id))
    unknown = ran.dollars is None or unpriced
    return Outcome(ran.answer, ran.data, final.url, None, final.controls, *_page_of(final)), ArmReport(
        status=ran.status,
        seconds=round(seconds, 2),
        session_open_seconds=ready.open_seconds,
        session_close_seconds=close_seconds,
        timing_note=OSS_SETUP,
        dollars=None if unknown else ran.dollars + browser,
        unknown_cost=unknown,
        steps=ran.steps,
        step_log=_scrubbed(ran.step_log, task.secrets),
        error=ran.error,
        model=OSS_MODEL,
        observe_error=final.error,
        final_page_capture=final.frame,
        final_page_capture_viewport=final.frame_viewport,
        final_page_capture_withheld=final.frame_withheld,
        provenance={"llm_cost_basis": OSS_LLM_COST}
        | ({} if ran.step_ceiling is None else {"upstream_max_steps": ran.step_ceiling})
        | ({} if ran.timeout_ceiling is None else {"upstream_timeout_seconds": ran.timeout_ceiling}),
        cost_by_component=({} if unpriced else {"browser": browser})
        | ({} if ran.dollars is None else {"llm": ran.dollars}),
    )


async def prepare_oss() -> None:
    with tempfile.TemporaryDirectory(prefix="bu-prepare-") as runtime:
        await run_command(
            (*OSS_COMMAND, "-c", "import browser_use"),
            b"",
            seconds=None,
            environment=arm_environment("browser-use-oss", runtime),
            cwd=runtime,
        )


HARNESS_SETUP = "the agent connects to the browser and opens the first page in its own first commands"


async def harness_arm(task: LiveTask, http: httpx.AsyncClient, *, record: Path | None) -> tuple[Outcome, ArmReport]:
    """Codex with Browser Harness (CLI and skill) on a fresh Browser Use Cloud browser, the browser the fastbrowse arm
    runs on, handed over by its CDP WebSocket. See fastbrowse_evals/adapters/browser_harness_arm.py."""
    if record is not None:
        raise ValueError("browser-harness records its commands and Codex events, not --record videos")
    artifact = Path("artifacts/evals/browser-harness") / f"{task.id}-{uuid.uuid4().hex}"
    _attempt_artifact.set(str(artifact))
    codex = os.environ.get("BROWSER_HARNESS_CODEX", "codex")
    # Checked before the browser opens: a wrong pinned tool fails every attempt alike, and found inside one, after its
    # browser was billed, each attempt kept its whole budget reserve as unknown cost.
    with tempfile.TemporaryDirectory(prefix="bh-check-") as runtime:
        try:
            await _invoke(
                HARNESS_COMMAND, HARNESS_RUNNER, {"check": codex}, arm_environment("browser-harness", runtime), runtime
            )
        except CommandError as error:
            raise NotStarted(f"browser-harness tools: {error}") from None
    async with cloud_browser(http, screen=_screen(task)) as cloud:
        session_ready()
        _watch("browser-harness", task, cloud.live_url)
        request = {
            "goal": prompt(task),
            "cdp_ws": cloud.cdp_url,
            "codex": codex,
            "model": HARNESS_MODEL,
            "reasoning_effort": HARNESS_EFFORT,
            "prices": HARNESS_PRICES,
            "output_schema": task.output_schema.model_json_schema() if task.output_schema else None,
            "artifact": str(artifact.resolve()),
        }
        with tempfile.TemporaryDirectory(prefix="bh-arm-") as runtime:
            env = arm_environment("browser-harness", runtime)
            # Read by Codex for its model provider; its shell policy keeps the key out of the agent's commands.
            env["OPENROUTER_API_KEY"] = backend.openrouter_key()
            ran = _HarnessReport.model_validate_json(
                await _invoke(HARNESS_COMMAND, HARNESS_RUNNER, request, env, runtime, graceful=True)
            )
        session_answered()
        final = await observe_browser(cloud.cdp_url, secrets=task.secrets if not task.bitwarden_item else None)
    browser, unpriced = _browser_dollars(cloud.cost)
    unknown = ran.dollars is None or unpriced
    return Outcome(ran.answer, ran.data, final.url, None, final.controls, *_page_of(final)), ArmReport(
        status=ran.status,
        # Timed inside the runner as jev-ultrafast is, so no arm is charged for resolving and importing itself.
        seconds=round(ran.seconds, 2),
        session_open_seconds=cloud.open_seconds,
        session_close_seconds=cloud.close_seconds,
        dollars=None if unknown else ran.dollars + browser,
        unknown_cost=unknown,
        steps=ran.steps,
        step_cap_unit="tool_calls",
        error=ran.error,
        timing_note=HARNESS_SETUP,
        model=HARNESS_MODEL,
        trace=[str(command.get("command")) for command in ran.commands if isinstance(command, dict)],
        step_log=ran.commands,
        input_tokens=ran.input_tokens,
        output_tokens=ran.output_tokens,
        provenance=ran.provenance | {"cached_input_tokens": ran.cached_input_tokens},
        observe_error=final.error,
        final_page_capture=final.frame,
        final_page_capture_viewport=final.frame_viewport,
        final_page_capture_withheld=final.frame_withheld,
        artifact=str(artifact),
        cost_by_component=({} if unpriced else {"browser": browser})
        | ({} if ran.dollars is None else {"llm": ran.dollars}),
    )


async def prepare_harness() -> None:
    """Install browser-harness's environment once, so no run is timed installing it."""
    with tempfile.TemporaryDirectory(prefix="bh-prepare-") as runtime:
        await run_command(
            (*HARNESS_COMMAND, "-c", "import browser_harness"),
            b"",
            seconds=None,
            environment=arm_environment("browser-harness", runtime),
            cwd=runtime,
        )


async def _session_marks(artifact: Path) -> None:
    """Move the attempt's clock as the runner, which opens this arm's browser itself, reports each moment."""
    for name, mark in (("session-ready", session_ready), ("session-answered", session_answered)):
        while not await asyncio.to_thread((artifact / name).exists):
            await asyncio.sleep(0.2)
        mark()


async def cua_arm(task: LiveTask, http: httpx.AsyncClient, *, record: Path | None) -> tuple[Outcome, ArmReport]:
    if record is not None:
        raise ValueError("cua-codex records tool and model traces, not --record videos")
    if task.secrets or task.bitwarden_item or task.expect != Status.COMPLETE:
        raise ValueError("cua-codex supports completion tasks without credentials")
    artifact = Path("artifacts/evals/cua") / f"{task.id}-{uuid.uuid4().hex}"
    _attempt_artifact.set(str(artifact))
    request = {
        "start": task.start,
        "goal": prompt(task),
        # Uncapped: the gateway refuses no tool call and the prompt states no call budget.
        "max_steps": None,
        "model": CUA_MODEL,
        "driver": os.environ.get("CUA_DRIVER", "cua-driver"),
        "codex": os.environ.get("CUA_CODEX", "codex"),
        "output_schema": task.output_schema.model_json_schema() if task.output_schema else None,
        "artifact": str(artifact.resolve()),
    }
    await asyncio.to_thread(artifact.mkdir, mode=0o700, parents=True, exist_ok=False)
    await asyncio.to_thread((artifact / "request.json").write_text, json.dumps({"task": task.id, "request": request}))
    with tempfile.TemporaryDirectory(prefix="cua-run-") as runtime:
        env = {key: os.environ[key] for key in _RUNTIME_ENV if key in os.environ}
        # Auth is read by Codex itself; neither the task nor the model receives its value.
        env["CODEX_HOME"] = await asyncio.to_thread(
            lambda: str(Path(os.environ.get("CODEX_HOME", Path.home() / ".codex")).resolve())
        )
        marks = asyncio.ensure_future(_session_marks(artifact))
        try:
            ran = _CuaReport.model_validate_json(
                await _invoke((sys.executable,), CUA_RUNNER, request, env, runtime, graceful=True)
            )
        except Exception as exc:
            malformed = isinstance(exc, ValidationError)
            return Outcome(None, None, None), ArmReport(
                # A worker gone with no result is a lost transport; a result it printed but that will not parse is a
                # broken report, not the browser being away.
                status="error" if malformed else "unavailable",
                failure_class=None if malformed else BROWSER_TRANSPORT,
                seconds=elapsed(),
                dollars=None,
                unknown_cost=True,
                error=f"{type(exc).__name__}: {exc}",
                artifact=str(artifact),
                model=CUA_MODEL,
            )
        finally:
            marks.cancel()
    final = ran.final
    return Outcome(ran.answer, ran.data, final.url, None, final.controls, *_page_of(final)), ArmReport(
        status=ran.status,
        seconds=ran.seconds,
        session_open_seconds=ran.session_open_seconds,
        session_close_seconds=ran.session_close_seconds,
        dollars=None,
        steps=ran.steps,
        error=ran.error,
        step_cap=None,
        step_cap_unit="tool_calls",
        model=CUA_MODEL,
        unknown_cost=True,
        provenance=ran.provenance,
        observe_error=final.error,
        final_page_capture=final.frame,
        final_page_capture_viewport=final.frame_viewport,
        final_page_capture_withheld=final.frame_withheld,
        artifact=str(artifact),
    )


async def prepare_cua() -> None:
    command = (
        sys.executable,
        "-c",
        "import runpy, sys; arm = runpy.run_path(sys.argv[1]); arm['prerequisites'](sys.argv[2], sys.argv[3])",
        str(CUA_RUNNER),
        os.environ.get("CUA_DRIVER", "cua-driver"),
        os.environ.get("CUA_CODEX", "codex"),
    )
    process = await asyncio.create_subprocess_exec(*command)
    if await process.wait() != 0:
        raise RuntimeError(f"cua-codex requires {CUA_PIN}, Linux, Xvfb, Openbox and the mcp extra")


async def _run_fast(
    task: LiveTask, http: httpx.AsyncClient, downloads: Path, **kwargs: Any
) -> tuple[Outcome, ArmReport]:
    return await _fast_report(task, http, downloads, **kwargs)


POOL_ORGANIZATION = "parallax-benchmark"
POOL_LEASE_MINUTES = 120
"""The longest lease the pool grants at once; `_keep_lease` extends it for as long as the run lasts."""
POOL_EXTEND_EVERY = 300
POOL_EXTEND_SECONDS = 900
"""Every five minutes the lease is extended by fifteen, so a lease never expires under a run that is still going:
the pool's lease lifetime must not become the time cap no arm has."""
POOL_FULL_WAIT = (2.0, 30.0)
"""First and largest wait, doubling between them, while the pool answers 503 (all its browsers leased). Waiting for
capacity is not the agent's time and not an outage, so it is retried without bound and logged."""
_pool_log = logging.getLogger("fastbrowse_evals.pool")


@dataclass
class PoolLease:
    lease_id: str
    ws_url: str
    """What `run_task(cdp_url=...)` attaches to: `/json/version`'s `webSocketDebuggerUrl`, not the discovery base."""
    acquired: float
    """Seconds to get the browser: the lease and its WebSocket address, without any wait for a full pool. This is the
    pool's opening of the session, so it stays out of the run's `seconds` as a cloud browser's start does."""
    full_wait: float
    """Seconds spent waiting while the pool was full: an artifact of its size, not of the backend, so not counted."""
    extend_failures: int = 0
    released: float | None = None
    """Seconds releasing the lease took, set once it has been released."""


def pool_settings() -> tuple[str, str]:
    """The pool's base URL and bearer token. Both come from the environment and are never logged or recorded."""
    url, token = os.environ.get("PARALLAX_POOL_URL"), os.environ.get("PARALLAX_POOL_TOKEN")
    if not url or not token:
        raise RuntimeError("fastbrowse-pool needs PARALLAX_POOL_URL and PARALLAX_POOL_TOKEN")
    parts = urlsplit(url)
    if parts.scheme not in {"http", "https"} or not parts.netloc:
        raise RuntimeError("PARALLAX_POOL_URL must be an http(s) URL")
    return url.rstrip("/"), token


def pool_host() -> str | None:
    """The pool's hostname alone, what provenance records of it; never the token, path or query."""
    url = os.environ.get("PARALLAX_POOL_URL")
    return urlsplit(url).hostname if url else None


def on_pool_origin(url: str, pool: str) -> str:
    """`url` moved onto the pool's own scheme and host, path and query kept.

    The pool builds `cdp_url` and `webSocketDebuggerUrl` from the address it sees, and TLS ends at the edge in front
    of it, so a pool reached over https hands back http:// and ws:// addresses on its internal host. Those are rebuilt
    on PARALLAX_POOL_URL's origin (http becomes https and ws becomes wss when the pool is reached over https)."""
    parts, base = urlsplit(url), urlsplit(pool)
    secure = base.scheme == "https"
    scheme = ("wss" if secure else "ws") if parts.scheme in {"ws", "wss"} else base.scheme
    if (parts.scheme, parts.netloc) == (scheme, base.netloc):
        return url
    return urlunsplit((scheme, base.netloc, parts.path, parts.query, parts.fragment))


def _pool_failure(what: str, status: int | None = None, error: Exception | None = None) -> Exception:
    """A pool failure as an outage when a retry can clear it, else as the harness's own error; never echoes a body."""
    if error is not None:
        failed = Unavailable if isinstance(error, TRANSIENT_TRANSPORT) else RuntimeError
        return failed(f"browser pool {what} failed ({type(error).__name__})")
    return (Unavailable if status in RETRYABLE_STATUS else RuntimeError)(f"browser pool {what} returned HTTP {status}")


async def _keep_lease(http: httpx.AsyncClient, lease_url: str, headers: dict[str, str], lease: PoolLease) -> None:
    """Extend the lease every `POOL_EXTEND_EVERY` seconds until cancelled; a failed extension is logged, never fatal."""
    while True:
        await asyncio.sleep(POOL_EXTEND_EVERY)
        try:
            response = await http.post(
                f"{lease_url}/extend", params={"seconds": POOL_EXTEND_SECONDS}, headers=headers, timeout=30
            )
            ok = response.is_success
            why = f"HTTP {response.status_code}"
        except httpx.HTTPError as error:
            ok, why = False, type(error).__name__
        if not ok:
            lease.extend_failures += 1
            _pool_log.warning("browser pool lease %s was not extended (%s)", lease.lease_id, why)


@asynccontextmanager
async def pool_lease(http: httpx.AsyncClient, run_id: str) -> AsyncIterator[PoolLease]:
    """Lease one pool browser for one attempt: wait for capacity, resolve its WebSocket address, keep the lease alive
    while the run lasts, and release it on every path out, a cancelled run included.

    Mirrors fastbrowse-hosted's `PoolProvider`: POST {pool}/leases, GET {cdp_url}/json/version for the WebSocket
    address, DELETE {pool}/leases/{id} to release, POST {pool}/leases/{id}/extend?seconds=N to extend."""
    pool, token = pool_settings()
    headers = {"Authorization": f"Bearer {token}"}
    began = time.monotonic()
    full_wait = 0.0
    wait, longest = POOL_FULL_WAIT
    while True:
        try:
            response = await http.post(
                f"{pool}/leases",
                headers=headers,
                json={"organization_id": POOL_ORGANIZATION, "run_id": run_id, "timeout_minutes": POOL_LEASE_MINUTES},
                timeout=60,
            )
        except httpx.HTTPError as error:
            raise _pool_failure("lease", error=error) from None
        if response.status_code != 503:
            break
        _pool_log.warning("browser pool is full; retrying the lease in %gs", wait)
        paused = time.monotonic()
        await asyncio.sleep(wait)
        full_wait += time.monotonic() - paused
        wait = min(wait * 2, longest)
    if not response.is_success:
        raise _pool_failure("lease", response.status_code)
    try:
        body = response.json()
        lease_id = str(body["lease_id"])
        cdp_base = on_pool_origin(str(body["cdp_url"]).rstrip("/"), pool)
    except (ValueError, KeyError, TypeError):
        raise RuntimeError("browser pool returned an unusable lease") from None
    # Managed under the configured base, as the lease was taken, so a pool behind a path prefix keeps its prefix.
    lease_url = f"{pool}/leases/{quote(lease_id, safe='')}"
    lease = None
    parts = urlsplit(cdp_base)
    # The discovery path is appended to the base's path, keeping any query the pool put on it.
    discovery = urlunsplit((parts.scheme, parts.netloc, parts.path.rstrip("/") + "/json/version", parts.query, ""))
    try:
        try:
            version = await http.get(discovery, headers=headers, timeout=30)
        except httpx.HTTPError as error:
            raise _pool_failure("browser discovery", error=error) from None
        if not version.is_success:
            raise _pool_failure("browser discovery", version.status_code)
        try:
            ws_url = on_pool_origin(str(version.json()["webSocketDebuggerUrl"]), pool)
        except (ValueError, KeyError, TypeError):
            raise RuntimeError("browser pool handed out a browser with no WebSocket address") from None
        if urlsplit(ws_url).scheme not in {"ws", "wss"}:
            raise RuntimeError("browser pool handed out an address that is not a WebSocket")
        lease = PoolLease(lease_id, ws_url, time.monotonic() - began - full_wait, full_wait)
        keeper = asyncio.create_task(_keep_lease(http, lease_url, headers, lease))
        try:
            yield lease
        finally:
            keeper.cancel()
            await asyncio.gather(keeper, return_exceptions=True)
    finally:
        # A lease nobody returns is a browser the next run cannot have until the pool reaps it.
        closing = time.monotonic()
        try:
            await http.delete(lease_url, headers=headers, timeout=30)
        except httpx.HTTPError as error:
            _pool_log.warning("browser pool lease %s was not released (%s)", lease_id, type(error).__name__)
        if lease is not None:
            lease.released = round(time.monotonic() - closing, 2)


async def _run_pool(
    task: LiveTask, http: httpx.AsyncClient, downloads: Path, **kwargs: Any
) -> tuple[Outcome, ArmReport]:
    """fastbrowse exactly as the fastbrowse arm runs it, on a pool browser instead of a Browser Use Cloud one. Getting
    the browser is opening its session, so it stays out of `seconds` as a cloud browser's start does."""
    async with pool_lease(http, f"{task.id}-{uuid.uuid4().hex}") as lease:
        outcome, report = await _fast_report(
            task,
            http,
            downloads,
            record=kwargs["record"],
            cdp_url=lease.ws_url,
            arm="fastbrowse-pool",
        )
    extra = {
        "session_open_seconds": round(lease.acquired + (report.session_open_seconds or 0.0), 2),
        "session_close_seconds": lease.released,
        "lease_seconds": round(lease.acquired, 2),
        "pool_full_seconds": round(lease.full_wait, 2),
        "lease_extend_failures": lease.extend_failures,
    }
    return outcome, report.model_copy(update=extra)


async def _run_product(task: LiveTask, http: httpx.AsyncClient, _: Path, **kwargs: Any) -> tuple[Outcome, ArmReport]:
    run, wall = await hosted_api.submit_and_wait(http, task)
    return hosted_api.outcome_of(run), ArmReport(**hosted_api.report_fields(run, wall))


async def _run_ultra(task: LiveTask, http: httpx.AsyncClient, _: Path, **kwargs: Any) -> tuple[Outcome, ArmReport]:
    return await ultrafast_arm(task, http, record=kwargs["record"])


async def _run_hosted(task: LiveTask, http: httpx.AsyncClient, _: Path, **kwargs: Any) -> tuple[Outcome, ArmReport]:
    return await hosted_arm(task, http, record=kwargs["record"], arm="browser-use")


async def _run_hosted_ultrafast(
    task: LiveTask, http: httpx.AsyncClient, _: Path, **kwargs: Any
) -> tuple[Outcome, ArmReport]:
    return await hosted_arm(task, http, record=kwargs["record"], arm="browser-use-ultrafast")


async def _run_oss(task: LiveTask, http: httpx.AsyncClient, _: Path, **kwargs: Any) -> tuple[Outcome, ArmReport]:
    return await oss_arm(task, http, record=kwargs["record"])


async def _run_cua(task: LiveTask, http: httpx.AsyncClient, _: Path, **kwargs: Any) -> tuple[Outcome, ArmReport]:
    return await cua_arm(task, http, record=kwargs["record"])


async def _run_harness(task: LiveTask, http: httpx.AsyncClient, _: Path, **kwargs: Any) -> tuple[Outcome, ArmReport]:
    return await harness_arm(task, http, record=kwargs["record"])


async def _prepare_ultra() -> None:
    await prepare_ultrafast()


ARMS: dict[str, ArmSpec] = {
    "fastbrowse": ArmSpec(runner=_run_fast, pin="run.fastbrowse_version + run.git_sha", env_allowlist=(), tier="A"),
    "fastbrowse-pool": ArmSpec(
        runner=_run_pool,
        pin="run.fastbrowse_version + run.git_sha; fastbrowse-hosted browser pool",
        env_allowlist=(),
        tier="A",
        default=False,
    ),
    "fastbrowse-hosted": ArmSpec(
        runner=_run_product,
        pin="api.fastbrowse.ai at its deployed build; customer charge, not metered cost",
        env_allowlist=(),
        tier="hosted",
        default=False,
    ),
    "jev-ultrafast": ArmSpec(
        runner=_run_ultra,
        pin=ULTRAFAST_PIN,
        env_allowlist=_RUNTIME_ENV,
        tier="A",
        prepare=_prepare_ultra,
    ),
    "browser-use": ArmSpec(
        runner=_run_hosted,
        pin=f"browser-use-sdk=={SDK_VERSION} API v4; default model, reported per row",
        env_allowlist=(),
        tier="hosted",
    ),
    "browser-use-ultrafast": ArmSpec(
        runner=_run_hosted_ultrafast,
        pin=f"browser-use-sdk=={SDK_VERSION} API v4; model bu-ultrafast, reported per row",
        env_allowlist=(),
        tier="hosted",
    ),
    "browser-use-oss": ArmSpec(
        runner=_run_oss, pin=OSS_PIN, env_allowlist=_RUNTIME_ENV, tier="A", default=False, prepare=prepare_oss
    ),
    "cua-codex": ArmSpec(
        runner=_run_cua, pin=CUA_PIN, env_allowlist=_RUNTIME_ENV, tier="local", default=False, prepare=prepare_cua
    ),
    "browser-harness": ArmSpec(
        runner=_run_harness,
        pin=HARNESS_PIN,
        env_allowlist=_RUNTIME_ENV,
        tier="A",
        default=False,
        prepare=prepare_harness,
    ),
}


HEAD_TO_HEAD_ARMS = (
    "fastbrowse",
    "browser-use",
    "browser-use-ultrafast",
    "jev-ultrafast",
    "browser-harness",
    "browser-use-oss",
)
"""The arms compared head to head, each of which runs every task it can grade on equal terms (`eligible`); the catalog
records why it skips any other (`excluded_because`)."""


def excluded_because(arm: str, task: LiveTask) -> str | None:
    """Why a head-to-head arm does not run `task`, or None when it does. Every exclusion has a reason here, so an
    unexplained one fails the catalog export instead of shipping."""
    if eligible(arm, task):
        return None
    # The site shows a reason after the agent's name ("Browser Harness: cannot attempt 6 tasks (...): <reason>"), so
    # each is written for that reader, about "it", the agent, with no harness terms.
    if arm in HOSTED_MODELS and task.expect is not Status.COMPLETE:
        return (
            "the task passes only by stopping before an irreversible step it does not authorize, and the hosted "
            "Browser Use API takes no authorization setting and no pause, so it cannot be told the step is unauthorized"
        )
    if arm == "jev-ultrafast" and task.expect is not Status.COMPLETE:
        return (
            "the task passes only by stopping before an irreversible step it does not authorize, and it takes no "
            "authorization setting and has no way to ask: it only ends done or blocked"
        )
    if arm == "jev-ultrafast" and (task.secrets or task.bitwarden_item is not None):
        return "the task needs login credentials, and it has no way to be given any"
    if arm == "jev-ultrafast":
        return "the task asks for an answer, and it returns none: it only ends done or blocked"
    if arm in {"browser-harness", "browser-use-oss"} and task.expect is not Status.COMPLETE:
        return (
            "the task passes only by stopping before an irreversible step it does not authorize, and it takes no "
            "authorization setting and has no way to pause"
        )
    if arm == "browser-harness":
        return "the task needs login credentials, and we give it none, because it would hold them in its own shell"
    if arm == "browser-use-oss":
        return (
            "the task needs login credentials, and we give it none, because the code its model writes can read "
            "any it is given"
        )
    raise ValueError(f"{task.id}: {arm} is excluded without a stated reason")


def eligible(arm: str, task: LiveTask) -> bool:
    if arm == "fastbrowse-pool":
        # The matched backend A/B: every task fastbrowse runs, and only those.
        return "fastbrowse" in task.arms
    if arm == "fastbrowse-hosted":
        # The product takes stored secrets, not a task's own, and returns no copy of its final page.
        return (
            "fastbrowse" in task.arms
            and not task.secrets
            and task.bitwarden_item is None
            and (task.category is not Category.NAVIGATE)
        )
    if arm == "browser-use-ultrafast":
        # The same hosted agent on another model: every task the hosted arm runs.
        return eligible("browser-use", task)
    if arm == "jev-ultrafast":
        # It ends only DONE or BLOCKED and returns no answer, so it runs only the tasks graded on the page it ends on,
        # which name it in their own `arms`.
        return arm in task.arms
    if arm in {"cua-codex", "browser-harness", "browser-use-oss"}:
        # Agents that take no credentials and no authorization setting: completion tasks without credentials.
        # browser-use 1.0 does take `sensitive_data`, but says it is no sandbox: the Python its model writes can read
        # the values, so a login would be handed to the model.
        return task.expect == Status.COMPLETE and not task.secrets and task.bitwarden_item is None
    return arm in task.arms


SUITES: dict[str, tuple[LiveTask, ...]] = {
    "core": TASKS,
    "dev": DEV,
    "stretch-dev": STRETCH_DEV,
}
"""`core` is the published suite; the others are in `more_tasks`."""

DIAGNOSTIC_SUITES: dict[str, tuple[LiveTask, ...]] = {"sweep": SWEEP}
"""Suites run to see where agents get stuck, outside the catalog: never versioned, published or gated on. A campaign
runs them only as a smoke, which publishes nothing."""
RUNNABLE = {**SUITES, **DIAGNOSTIC_SUITES}


def summarize(rows: list[EvalRow], arms: list[str]) -> None:
    for arm in arms:
        ran = [r for r in rows if r.arm == arm]
        # Outage rows say nothing about the agent: left out here as in every published figure.
        arm_rows = [r for r in ran if r.normalized_status != Ending.UNAVAILABLE]
        if not ran:
            continue
        passed = sum(r.passed is True for r in ran)
        ungraded = sum(r.passed is None for r in ran)
        correct = sum(r.correct is True for r in ran)
        priced = [r.dollars for r in ran if r.dollars is not None]
        seconds = [r.seconds for r in arm_rows]
        unknown = len(ran) - len(priced)
        cost = f"${sum(priced):.4f}" if priced else "cost unknown"
        median = f"{statistics.median(seconds):.1f}s" if seconds else "unavailable"
        print(
            f"{arm}: {passed}/{len(ran) - ungraded} graded passed, {ungraded} ungraded, {correct} correct, median {median}, "
            + cost
            + (f" ({unknown} runs of unknown cost)" if unknown else "")
        )
        if excluded := len(ran) - len(arm_rows):
            print(f"  {excluded} runs ended by a provider outage, excluded from latency")
        calls: dict[str, float] = {}
        for r in arm_rows:
            for label, spent in r.seconds_by_call.items():
                calls[label] = calls.get(label, 0.0) + spent
        for label, spent in sorted(calls.items(), key=lambda item: -item[1]):
            print(f"  {label:18} {spent / len(arm_rows):5.1f}s a task")
        for r in arm_rows:
            if not r.passed:
                print(f"  {'FAIL' if r.passed is False else 'UNGRADED'} {r.task:22} {_cause(r)}")


def _cause(row: EvalRow) -> str:
    """Why a run failed, leading with how the run itself ended: a grader's "answer lacks X: None" only restates
    that a run which died on a provider outage had no answer."""
    if row.error and row.status is not None:
        return f"{row.status}: {row.error}" + (f" | graded: {row.failure}" if row.failure else "")
    return row.failure or f"status {row.status}"


def browser_identity(arm: str, local_browser: Path | None) -> dict[str, Any]:
    """The browser each arm drives, as provenance records it: the backend is part of what a row measures."""
    if arm == "fastbrowse":
        if local_browser is not None:
            return local_browser_identity(local_browser)
        return {"mode": "browser-use-cloud", "timeout_minutes": CLOUD_BROWSER_MINUTES}
    if arm == "fastbrowse-pool":
        return {"mode": "fastbrowse-pool", "host": pool_host()}
    if arm in {"jev-ultrafast", "browser-use-oss", "browser-harness"}:
        return {"mode": "browser-use-cloud", "timeout_minutes": CLOUD_BROWSER_MINUTES}
    if arm in HOSTED_MODELS:
        return {"mode": "browser-use-hosted"}
    if arm == "fastbrowse-hosted":
        return {"mode": "fastbrowse-hosted", "host": hosted_api.host()}
    if arm == "cua-codex":
        return {"mode": "local-xvfb"}
    raise ValueError(f"unknown arm {arm}")


def _started_physical_runs(out: Path) -> bool:
    """Whether an earlier attempt into `out` started a physical run: a row in its ledger, attempts or budget journal.
    An attempt refused before any run leaves at most empty files, so it may reuse its `--out`."""
    files = (out, out.with_suffix(".attempts.jsonl"), out.with_suffix(".budget.jsonl"))
    return any(f.exists() and f.stat().st_size > 0 for f in files)


async def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="fastbrowse-evals browser internal", description=__doc__)
    parser.add_argument("--only", nargs="*", default=[], help="task ids to run; an id the selection lacks is an error")
    parser.add_argument(
        "--suite", nargs="*", choices=list(RUNNABLE), help="task sets to run (default core, or every suite with --only)"
    )
    parser.add_argument("--category", nargs="*", default=[], choices=[c.value for c in Category])
    parser.add_argument(
        "--arms",
        nargs="*",
        default=[name for name, spec in ARMS.items() if spec.default],
        choices=list(ARMS),
        help="pass a comparison's two arms; jev-ultrafast is Browser Use Ultrafast, browser-use the hosted Browser Use "
        "agent; fastbrowse-pool, browser-use-oss, cua-codex and browser-harness are opt-in",
    )
    parser.add_argument(
        "--request-profiles",
        type=Path,
        help="source-bound physical request profiles; requires fastbrowse and local Chrome",
    )
    parser.add_argument(
        "--budget",
        type=float,
        help="approved total dollars; with --request-profiles, reserves each physical model request; "
        "otherwise only gates whether a new run may start",
    )
    parser.add_argument(
        "--reserve-dollars",
        type=float,
        default=1,
        help="dollars held per in-flight run for budget accounting only; never a limit on any agent",
    )
    parser.add_argument("--max-dollars", help=argparse.SUPPRESS)
    parser.add_argument("--campaign-budget-only", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument(
        "--local-browser",
        type=Path,
        help="owned headless local Chrome binary for the fastbrowse arm; every other arm keeps its own browser",
    )
    parser.add_argument(
        "--browser-reserve-dollars", type=float, default=1, help="additional cloud browser spend held per run"
    )
    parser.add_argument(
        "--judge-reserve-dollars",
        type=float,
        default=1,
        help="estimated judge spend held per run alongside the agent and browser holds",
    )
    parser.add_argument("--text-input-usd-per-mtok", type=float)
    parser.add_argument("--text-output-usd-per-mtok", type=float)
    parser.add_argument("--text-max-output-tokens", type=int, default=16384)
    parser.add_argument("--text-pricing-checked", help="date the Ultrafast helper prices were verified")
    parser.add_argument(
        "--judge-policy",
        choices=JUDGE_POLICIES,
        default=JUDGE_POLICY,
        help=f"how rows are graded; only the default {JUDGE_POLICY} ({JUDGE_MODEL} alone) can be published",
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
    request_spec = None
    if args.request_profiles is not None:
        if (
            args.arms != ["fastbrowse"]
            or args.local_browser is None
            or args.budget is None
            or args.judge_policy != JUDGE_POLICY
        ):
            parser.error(
                "request profiles require --arms fastbrowse, --local-browser, --budget and the default judge policy"
            )
        try:
            request_spec = RequestSessionSpec.model_validate_json(args.request_profiles.read_bytes())
        except (OSError, ValueError):
            parser.error("request profiles must be a readable, valid source-bound session specification")
        if args.out.with_suffix(".requests.jsonl").exists():
            parser.error("request reservations require a fresh wallet journal")
    if args.campaign_budget_only:
        parser.error("--campaign-budget-only was removed: every run is uncapped and --budget only gates new runs")
    if args.max_dollars is not None:
        parser.error("--max-dollars was removed: no run has a dollar limit; --reserve-dollars sets the accounting hold")
    if args.local_browser is not None and "fastbrowse" not in args.arms:
        parser.error("--local-browser is the fastbrowse arm's browser, so it needs the fastbrowse arm")
    if args.budget is not None:
        if any(not math.isfinite(n) or n <= 0 for n in (args.budget, args.reserve_dollars)):
            parser.error("the campaign budget and per-run reservation must be positive finite dollars")
        if not math.isfinite(args.browser_reserve_dollars) or args.browser_reserve_dollars < 0:
            parser.error("browser reserve must be nonnegative finite dollars")
        if not math.isfinite(args.judge_reserve_dollars) or args.judge_reserve_dollars <= 0:
            parser.error("judge reserve must be positive finite dollars")
        # Only a billed browser needs the browser hold; the pool and the fastbrowse arm's local browser bill nothing.
        unbilled = {"fastbrowse-pool"} | ({"fastbrowse"} if args.local_browser is not None else set())
        if args.browser_reserve_dollars == 0 and set(args.arms) - unbilled:
            parser.error("zero browser reserve requires every arm to run on a local or pool browser")
        hold = args.reserve_dollars + args.browser_reserve_dollars + args.judge_reserve_dollars
        if request_spec is None and args.budget + 1e-9 < hold:
            parser.error(
                f"--budget {args.budget:g} cannot hold one run: each run reserves ${hold:g} (agent "
                f"{args.reserve_dollars:g} + browser {args.browser_reserve_dollars:g} + judge "
                f"{args.judge_reserve_dollars:g})"
            )
        if _started_physical_runs(args.out):
            parser.error("a budgeted comparison requires a fresh output; retain and reconcile prior physical runs")
    for flag, count in (("--concurrency", args.concurrency), ("--repeat", args.repeat)):
        if count <= 0:
            parser.error(f"{flag} must be positive")
    try:
        require_judges(use_jev=args.judge_policy == JEV_JUDGE_POLICY, model=POLICY_JUDGE[args.judge_policy])
    except JudgeConfigError as exc:
        parser.error(f"every row is judged, so the judges must be configured before any run starts: {exc}")

    if args.record is not None and any(arm in args.arms for arm in ("browser-use-oss", "cua-codex", "browser-harness")):
        parser.error("browser-use-oss, cua-codex and browser-harness do not support --record")
    tasks = [
        t
        for suite in args.suite or (list(SUITES) if args.only else ["core"])
        for t in RUNNABLE[suite]
        if (not args.only or t.id in args.only) and (not args.category or t.category.value in args.category)
    ]
    # A rerun meant to confirm one row must not "pass" by quietly running none of it.
    if missing := sorted(set(args.only) - {t.id for t in tasks}):
        parser.error(f"--only names tasks outside the selected suites and categories: {', '.join(missing)}")
    planned = [
        (arm, task, repeat)
        for repeat in range(args.repeat)
        for task in tasks
        for arm in args.arms[repeat % len(args.arms) :] + args.arms[: repeat % len(args.arms)]
        if eligible(arm, task)
    ]
    if not planned:
        parser.error("selected tasks are unsupported by the requested arms")
    if unsupported := sorted(set(args.only) - {task.id for _, task, _ in planned}):
        parser.error(f"selected tasks are unsupported by the requested arms: {', '.join(unsupported)}")
    lock = load_lock()
    suite_of = {t.id: name for name, suite_tasks in RUNNABLE.items() for t in suite_tasks}
    suite_versions = {name: suite_version((t.id for t in suite_tasks), lock) for name, suite_tasks in RUNNABLE.items()}
    handler = logging.StreamHandler(sys.stdout)
    handler.addFilter(_NameRun())
    # Traces are collected per run at DEBUG and propagate here; only warnings belong on the console.
    handler.setLevel(logging.WARNING)
    handler.setFormatter(logging.Formatter("%(levelname)-7s %(run)s %(name)s: %(message)s"))
    logging.basicConfig(level=logging.WARNING, handlers=[handler])
    if "fastbrowse-pool" in args.arms:
        try:
            pool_settings()
        except RuntimeError as exc:
            parser.error(str(exc))
    fastbrowse_arms = {"fastbrowse", "fastbrowse-pool"} & set(args.arms)
    # Each fastbrowse row records the providers its run named (`provenance.providers`); the run records the Jev source
    # every arm that calls Jev was given.
    jev = (
        "jev openrouter"
        if request_spec is not None
        else backend.providers()
        if fastbrowse_arms or "jev-ultrafast" in args.arms
        else None
    )
    if jev is not None:
        print(f"PROVIDERS {jev}", flush=True)
    run = provenance(
        providers=jev,
        jev_ultrafast=ULTRAFAST if "jev-ultrafast" in args.arms else None,
        arms={name: {"pin": ARMS[name].pin, "tier": ARMS[name].tier} for name in args.arms},
        argv=list(argv),
        concurrency=args.concurrency,
        # No arm is limited, budgeted or not; recorded so publication can check it rather than assume it.
        timing_basis=TIMING_BASIS,
        session_open_limit_seconds=SESSION_OPEN_SECONDS,
        max_steps=None,
        max_seconds=None,
        max_dollars=None,
        agent_limits=AGENT_LIMITS.model_dump(mode="json"),
        budget_policy=POLICY if args.budget is not None else None,
        budget_usd=args.budget,
        reserve_dollars=args.reserve_dollars,
        browser_reserve_dollars=args.browser_reserve_dollars,
        judge_reserve_dollars=args.judge_reserve_dollars,
        hosted_max_cost_usd=HOSTED_UNCAPPED_USD if HOSTED_MODELS.keys() & set(args.arms) else None,
        cloud_browser_timeout_minutes=CLOUD_BROWSER_MINUTES,
        provider_limits=list(PROVIDER_LIMITS),
        judge_policy=args.judge_policy,
        browsers={name: browser_identity(name, args.local_browser) for name in args.arms},
        text_pricing_checked=args.text_pricing_checked,
        text_pricing_source="https://openrouter.ai/api/v1/models",
        text_input_price=args.text_input_usd_per_mtok,
        text_output_price=args.text_output_usd_per_mtok,
        text_max_output_tokens=args.text_max_output_tokens,
    )
    if request_spec is not None:
        run.update(request_spec.provenance())
        run["request_runtime"] = runtime_identity()
        run.update(reserve_dollars=None, browser_reserve_dollars=None, judge_reserve_dollars=None)
    dirty = " (uncommitted changes)" if run["git_dirty"] else ""
    print(f"RUN {run['run_id']} fastbrowse {run['fastbrowse_version']} at {run['git_sha']}{dirty}", flush=True)
    for name in args.arms:
        if prepare := ARMS[name].prepare:
            await prepare()
    args.out.parent.mkdir(parents=True, exist_ok=True)
    rows: list[EvalRow] = []
    spend = (
        None
        if args.budget is None or request_spec is not None
        else CampaignBudget(
            args.budget,
            args.reserve_dollars + args.browser_reserve_dollars + args.judge_reserve_dollars,
            args.out.with_suffix(".budget.jsonl"),
        )
    )
    request_session = (
        None
        if request_spec is None
        else RequestSession(
            request_spec,
            args.out.with_suffix(".requests.jsonl"),
            limit=Decimal(str(args.budget)),
            campaign=str(run["run_id"]),
            credentials=backend.environment(),
        )
    )
    if request_session is not None:
        await request_session.verify_credit()
    _judge_policy.set(args.judge_policy)
    gate = asyncio.Semaphore(args.concurrency)
    with (
        nullcontext() if spend is None else spend,
        nullcontext() if request_session is None else request_session,
        tempfile.TemporaryDirectory() as downloads,
        args.out.open("a", encoding="utf-8") as out,
        args.out.with_suffix(".attempts.jsonl").open("a", encoding="utf-8") as attempts,
    ):
        async with httpx.AsyncClient(timeout=60, event_hooks={"request": [_github_token]}) as http:
            watch = SiteWatch(http, tasks)
            watching = asyncio.create_task(watch.run())

            async def one(arm: str, task: LiveTask, repeat: int) -> EvalRow:
                def stamped(row: EvalRow) -> EvalRow:
                    if request_session is not None:
                        row = EvalRow.model_validate(request_session.public_json(json_value(row.model_dump())))
                    return row.model_copy(
                        update={
                            "concurrency": args.concurrency,
                            "retries": retries,
                            "repeat": repeat,
                            "suite": suite_of[task.id],
                            "suite_version": suite_versions[suite_of[task.id]],
                            "task_version": task_version(task.id, lock),
                            "run": {**row.run, **run},
                        }
                    )

                def keep(row: EvalRow, *, selected: bool, wait: int) -> None:
                    attempt = row.model_dump(mode="json", fallback=str)
                    attempt.update(selected=selected, retry_wait_seconds=wait)
                    attempts.write(json.dumps(attempt) + "\n")
                    attempts.flush()

                # An outage is waited out and the row run again; bounded, so a dead provider cannot hold a run forever.
                retries = 0
                while True:
                    _attempt_artifact.set(None)
                    _completed_attempt.set(None)
                    # The answer key is read once the row holds its slot: read while it queued, a live key (the
                    # top story, the newest release) could move on before the run began.
                    async with gate:
                        truth_failed = False
                        try:
                            truth = await _truth(task, http)
                        except Exception as exc:
                            # An answer key that will not come back (a 403 from a rate-limited API, a body missing
                            # the field it is read from) fails this task alone: gather would discard every run.
                            truth_failed = True
                            failure = f"truth raised {type(exc).__name__}: {exc}"
                            row = _crashed(arm, task, failure, at=time.time(), seconds=0.0, status=None, record=None)
                            row = row.model_copy(update={"unknown_cost": False})  # nothing ran, so nothing was spent
                        else:
                            record = None if args.record is None else video_path(args.record, arm, task)
                            began, began_at = time.monotonic(), time.time()
                            receipt = f"{arm}/{task.id}/{repeat}/{retries}"
                            # The budget's only role: whether this physical run may start. Nothing of it reaches the
                            # agent, whose limits are AGENT_LIMITS however much of the budget is left.
                            if spend is not None:
                                await spend.reserve(receipt)
                            browser_token = _local_browser.set(args.local_browser if arm == "fastbrowse" else None)
                            price_token = _ultra_billing.set(
                                {
                                    "text_input_price": (args.text_input_usd_per_mtok or 0) / 1_000_000,
                                    "text_output_price": (args.text_output_usd_per_mtok or 0) / 1_000_000,
                                    "text_max_output_tokens": args.text_max_output_tokens,
                                }
                            )
                            try:
                                row = await run_arm(
                                    arm,
                                    task,
                                    truth,
                                    http,
                                    Path(downloads),
                                    record=record,
                                    **({} if request_session is None else {"request_session": request_session}),
                                )
                            except BaseException as exc:
                                if (
                                    spend is None
                                    and request_session is None
                                    and not isinstance(exc, (asyncio.CancelledError, KeyboardInterrupt))
                                ):
                                    raise
                                # An interrupted attempt must not vanish from the ledger: its row is durable before
                                # the cancellation propagates, with unknown spend rather than a false zero. A slot
                                # still queued, or a truth-only preflight, wrote nothing because nothing ran.
                                keep(
                                    stamped(
                                        _crashed(
                                            arm,
                                            task,
                                            f"interrupted: {type(exc).__name__}",
                                            at=began_at,
                                            seconds=time.monotonic() - began,
                                            status=None,
                                            record=record,
                                        )
                                    ),
                                    selected=False,
                                    wait=0,
                                )
                                if spend is not None:
                                    await spend.settle(receipt, None)
                                raise
                            finally:
                                _local_browser.reset(browser_token)
                                _ultra_billing.reset(price_token)
                            if spend is not None:
                                # The run's charge is its agent and judge spend; unknown if either is, and zero for a
                                # run its provider refused to start. A charge above its hold is recorded as it is; it
                                # stops nothing (see budget.py).
                                await spend.settle(
                                    receipt,
                                    0.0
                                    if not row.started
                                    else None
                                    if row.unknown_cost or row.dollars is None or row.judge_dollars is None
                                    else row.dollars + row.judge_dollars,
                                )
                    if not truth_failed:
                        try:
                            row = await _site_checked(row, task, http)
                            if stalled := await watch.stalled(task, row.at, time.time()):
                                row = row.model_copy(update={"site_probe": stalled})
                        except (asyncio.CancelledError, KeyboardInterrupt):
                            # The run finished, so its known price and report survive the interruption instead of
                            # being replaced with an interrupted row's unknown spend.
                            keep(stamped(row), selected=False, wait=0)
                            raise
                    row = stamped(row)
                    limit = TRANSPORT_RETRIES if row.failure_class == BROWSER_TRANSPORT else OUTAGE_RETRIES
                    selected = (
                        row.execution_admission_stopped
                        or row.normalized_status != Ending.UNAVAILABLE
                        or retries >= limit
                    )
                    wait = 0 if selected else min(60 * 2**retries, 600)
                    # Retrying used to erase the outage's trace and spend, hiding the cost of obtaining a score.
                    keep(row, selected=selected, wait=wait)
                    if selected:
                        break
                    print(f"RETRY {arm:13} {task.id:20} in {wait}s: {_cause(row)}", flush=True)
                    await asyncio.sleep(wait)
                    retries += 1
                # Trace records hold whatever a component logged, so anything JSON cannot hold is written as text.
                out.write(row.model_dump_json(fallback=str) + "\n")
                out.flush()
                mark = "PASS" if row.passed is True else "FAIL" if row.passed is False else "UNGRADED"
                print(
                    f"{mark} {arm:13} {task.id:20} {row.seconds!s:>6}s "
                    + ("cost unknown" if row.dollars is None else f"${row.dollars:<8}"),
                    "" if row.passed else _cause(row),
                    flush=True,
                )
                if row.execution_admission_stopped:
                    raise SpendStop(row.execution_admission_reason or "physical request admission stopped")
                return row

            pending = [asyncio.create_task(one(*plan)) for plan in planned]
            try:
                rows = list(await asyncio.gather(*pending))
            except (BudgetStop, SpendStop):
                for running in pending:
                    running.cancel()
                await asyncio.gather(*pending, return_exceptions=True)
                print("Campaign budget exhausted; incomplete results remain private", flush=True)
                return 1
            except BaseException:
                # A failed row or an interrupted run must not close the ledger under the attempts still spending:
                # cancel the siblings, keep the in-flight rows their own handling wrote, then raise what stopped it.
                for running in pending:
                    running.cancel()
                await asyncio.gather(*pending, return_exceptions=True)
                raise
            finally:
                watching.cancel()
                await asyncio.gather(watching, return_exceptions=True)
    summarize(rows, args.arms)
    return 0


if __name__ == "__main__":
    from fastbrowse_evals.cli import until_terminated

    sys.exit(asyncio.run(until_terminated(main(sys.argv[1:]))))
