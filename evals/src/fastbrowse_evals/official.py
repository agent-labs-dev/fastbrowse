"""Run an agent on the official suites (BU Bench, Online-Mind2Web) and grade it with their pinned upstream judges.

`--arm` picks the agent: fastbrowse (the default and the only one published) or the hosted Browser Use agent
(`OFFICIAL_ARMS`). Every arm gets the same tasks and the same upstream judge request. The judge
model is `deepseek/deepseek-v4.1-flash-1017` through `--judge-base-url` (OpenRouter unless set), keyed by OPENAI_API_KEY. A run may repeat each
task (`--repeat`); every repeat is graded, and a task's figure is the mean of its repeats. Before the upstream judge,
Parallax's own judge checks each run for a site-side interruption (`INTERRUPTION_POLICY`) and runs such an attempt
again rather than score it."""

import argparse
import asyncio
import base64
import contextlib
import fcntl
import hashlib
import json
import math
import os
import re
import signal
import subprocess
import sys
import time
from collections import deque
from collections.abc import AsyncIterator, Iterable
from contextvars import ContextVar
from dataclasses import dataclass, field
from heapq import heappop, heappush
from itertools import count
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

import httpx
from fastbrowse import traced
from fastbrowse.config import Config
from fastbrowse.evals.harness import providers
from fastbrowse.models import (
    Authorization,
    BrowserEvent,
    CostBasis,
    CostBreakdown,
    LocalChrome,
    RunResult,
    Status,
    StepEvent,
    Unavailable,
)
from fastbrowse.run import run_task
from parallax.judge import JudgeConfigError

from fastbrowse_evals import backend
from fastbrowse_evals.adapters.browser_official_bridge import BillingError, judge_charge
from fastbrowse_evals.attempt_clock import (
    SESSION_INCLUDED,
    SESSION_OPEN_SECONDS,
    TIMING_BASIS,
    AttemptClock,
    session_answered,
    session_ready,
    setup_marks,
)
from fastbrowse_evals.backend import NotStarted, refusal
from fastbrowse_evals.browsers import (
    AGENT_SCREEN,
    CLOUD_BROWSER_MINUTES,
    Browser,
    cloud_browser,
    local_browser,
)
from fastbrowse_evals.budget import POLICY
from fastbrowse_evals.judging import (
    JUDGE_MODEL,
    OPENROUTER_BASE_URL,
    SITE_FAILURE_QUESTIONS,
    judge_browser,
    judge_receipt,
    require_judges,
)
from fastbrowse_evals.live import (
    AGENT_LIMITS,
    HOSTED_UNCAPPED_USD,
    OUTAGE_RETRIES,
    PROVIDER_LIMITS,
    judged_unavailable,
    llm_models,
)
from fastbrowse_evals.registry import BENCHMARKS, Suite
from fastbrowse_evals.versions import local_browser_identity, provenance

OFFICIAL_ARMS = ("fastbrowse", "browser-use")
"""Agents the official suites can run; both official judges grade the agent's answer and its action history
(evals/BENCHMARKS.md)."""
HOSTED_RUNTIME: dict[str, dict[str, Any]] = {
    "browser-use": {"browser": {"mode": "browser-use-hosted"}, "product": "Browser Use hosted agent (SDK v3)"},
}
BRIDGE = Path(__file__).with_name("adapters") / "browser_official_bridge.py"
# The only variables a judge process inherits. Provider keys and endpoints are added per call, never copied wholesale.
ENV_ALLOWLIST = ("PATH", "LANG", "LC_ALL", "TZ", "SSL_CERT_FILE", "SSL_CERT_DIR")
BILLING_FLAGS = {
    "judge_input_usd_per_mtok": "input_usd_per_mtok",
    "judge_output_usd_per_mtok": "output_usd_per_mtok",
    "judge_max_requests": "max_requests",
    "judge_max_input_tokens": "max_input_tokens",
    "judge_max_output_tokens": "max_output_tokens",
    "judge_image_tokens": "image_tokens_per_image",
    "judge_budget": "budget_usd",
}
CONTEXT_PRICING_FLAGS = {
    "judge_context_threshold_tokens": "context_threshold_tokens",
    "judge_context_input_usd_per_mtok": "context_input_usd_per_mtok",
    "judge_context_output_usd_per_mtok": "context_output_usd_per_mtok",
}
# The BU judge accepts only "before" or "after" for a whole screenshot set, and Core captures a step frame before or
# after its action depending on the call site. So BU Bench gives no step mapping, which upstream's judge then reads
# as chronological evidence of the state each frame shows, and the evidence notes state the mixed timing.
LEGACY_BU_FRAME_TIMINGS = (
    "final frame only, after the last action; step frames are retained but not judged",
    "final frame only when available, after the last action; absent final capture disclosed to judge",
)
FRAME_TIMING = {
    "bu-bench": "step frames of unspecified timing plus a final frame after, in order without a step mapping",
    "online-mind2web": "step frames of unspecified timing (before or after their action) plus a final frame after",
}
SAFE_ID = re.compile(r"[A-Za-z0-9_-]+")
# Model families that reason before they answer. Online-Mind2Web's judge code asks a chat model for 512 tokens at
# temperature 0, which a reasoning model cannot honour, so these get the reasoning request instead.
REASONING_JUDGES = ("o1", "o3", "o4", "gpt-5", "gpt-6")


def judge_family(model: str) -> str:
    """The model id without a provider prefix, so openai/o4-mini is judged as o4-mini."""
    return model.rsplit("/", 1)[-1]


CAPTURE_RETRIES: ContextVar[int] = ContextVar("capture_retries", default=0)
"""How many more times this slot may run again for want of a final capture. A missing capture says nothing of the
agent, so by one rule for every arm and suite the attempt runs again as an interrupted one does; with none left it is
graded on what was recorded, and the group states how many runs had no capture."""


INTERRUPTION_POLICY = "browser-official-deepseek-v4"
"""Parallax's own check of every official run before the upstream judge sees it: the internal policy's evidence
question (`judging.SITE_FAILURE_QUESTIONS`) alone, asked of `judging.JUDGE_MODEL`. Upstream scoring has no ungraded
outcome, so a run the check finds interrupted by the site (an error page, a rate limit, an outage, a network failure
the agent did not cause) is never sent upstream: the attempt is run again, as an internal outage is, and the
interrupted run is kept beside it with the reason. A slot still interrupted after every retry is blocked by the site
(`blocked_by_site`): publication counts it as the agent's failed attempt, since another agent may get past what stopped
this one, unless the run ended in an error of the harness or its model provider."""
INTERRUPTION_QUESTIONS = {"evidence_available": SITE_FAILURE_QUESTIONS["evidence_available"]}


class Interrupted(RuntimeError):
    """The interruption check found the site, not the agent, kept the run from its outcome."""

    def __init__(self, reason: str, check: dict[str, Any], run: dict[str, Any]) -> None:
        super().__init__(reason)
        self.check = check
        self.run = run


async def interruption_check(
    task: dict[str, Any],
    *,
    answer: str | None,
    final_url: str | None,
    actions: list[dict[str, Any]],
    final_frame: bytes | None,
    fields: dict[str, Any],
) -> dict[str, Any]:
    """The `INTERRUPTION_POLICY` receipt for one finished run, judged on what the upstream judge would see."""
    state = {
        "task": task["confirmed_task"],
        "answer": answer,
        "final_url": final_url,
        "status": fields.get("status"),
        "error": fields.get("error"),
        "action_history": actions,
        "final_page_capture": None
        if final_frame is None
        else "data:image/png;base64," + base64.b64encode(final_frame).decode(),
    }
    result, identity = await judge_browser(
        state, policy=INTERRUPTION_POLICY, questions=INTERRUPTION_QUESTIONS, use_jev=False
    )
    return judge_receipt(result, identity)


def interruption_usd(row: dict[str, Any] | None, agent_budget: float) -> float:
    """What a slot's interrupted runs and its interruption checks committed beyond its final run: each interrupted
    run's agent charge (`agent_usd`) and every check's judge charge; an interrupted run never reaches the upstream
    judge."""
    if row is None:
        return 0.0
    earlier = sum(run_usd(run, agent_budget) for run in row.get("interruptions", []))
    return earlier + (row.get("interruption_check_usd") or 0.0)


def run_usd(run: dict[str, Any], agent_budget: float) -> float:
    """What one interrupted run committed: its agent charge and its check."""
    return agent_usd(run, agent_budget) + (run.get("interruption_check_usd") or 0.0)


def checkout(root: Path, commit: str) -> None:
    head = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=root, capture_output=True, text=True, check=True
    ).stdout.strip()
    dirty = subprocess.run(
        ["git", "status", "--porcelain"], cwd=root, capture_output=True, text=True, check=True
    ).stdout.strip()
    if len(commit) != 40 or head != commit or dirty:
        raise ValueError("a clean upstream checkout at the exact supplied commit is required")


def private_root(out: Path) -> None:
    """A run directory holds task text, screenshots and provider receipts: it must not be publishable from a repo."""
    base = out.absolute()
    while not base.exists():
        base = base.parent
    probe = subprocess.run(
        ["git", "check-ignore", "-q", "--", str(out.absolute() / "manifest.json")],
        cwd=base,
        capture_output=True,
    )
    if probe.returncode == 1:
        raise ValueError("--out is inside a Git repository and not ignored; use a path outside it or an ignored one")
    if probe.returncode not in (0, 128):
        raise ValueError("could not verify that --out is outside publication paths")


def write_private(path: Path, data: bytes | str) -> None:
    body = data.encode() if isinstance(data, str) else data
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    try:
        os.write(descriptor, body)
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


JOURNAL = "budget.jsonl"
"""The start gate's own record, in the order it acted: each attempt's hold before it started, then its actual charge.
Publication replays it, since the attempt and result files alone cannot show which holds overlapped."""


def journal_row(key: str, limit: float, reservation: float, committed: float) -> dict[str, Any]:
    """One budget journal receipt, in the shape `CampaignBudget` writes for the internal campaigns."""
    return {"id": key, "limit": limit, "reservation": reservation, "committed": committed, "policy": POLICY}


def append_row(path: Path, row: dict[str, Any]) -> None:
    """One durable line: a crash after this returns cannot lose the record."""
    descriptor = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
    try:
        os.write(descriptor, (json.dumps(row) + "\n").encode())
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def read_rows(path: Path) -> list[dict[str, Any]]:
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()] if path.exists() else []


def child_env(out: Path, *, key: str | None = None, base_url: str | None = None) -> dict[str, str]:
    """Allowlisted environment for an upstream process, with a private home so nothing lands in the real one."""
    home = out / "judge-home"
    home.mkdir(mode=0o700, exist_ok=True)
    env = {name: os.environ[name] for name in ENV_ALLOWLIST if name in os.environ}
    env.update(
        HOME=str(home),
        TMPDIR=str(home),
        XDG_CONFIG_HOME=str(home),
        BROWSER_USE_CONFIG_DIR=str(home),
        ANONYMIZED_TELEMETRY="false",
        BROWSER_USE_SETUP_LOGGING="false",
        PYTHONDONTWRITEBYTECODE="1",
        PYTHONNOUSERSITE="1",
    )
    if key is not None:
        env["OPENAI_API_KEY"] = key
    # Only an explicit, recorded --judge-base-url reaches the judge; an ambient OPENAI_BASE_URL never does.
    if base_url is not None:
        env["OPENAI_BASE_URL"] = base_url
    return env


def judge_python(args: argparse.Namespace) -> Path:
    python = Path(args.judge_python).absolute()
    folder = python.parent.resolve()
    if (
        args.upstream_root.resolve() not in (folder, *folder.parents)
        or not python.is_file()
        or not os.access(python, os.X_OK)
    ):
        raise ValueError("--judge-python must be an executable inside the upstream checkout's prepared virtualenv")
    return python


async def bridge(args: argparse.Namespace, request: dict[str, Any], name: str, timeout: float = 600) -> Any:
    """Call the pinned grader with the prepared interpreter: nothing is installed, resolved or fetched here."""
    checkout(args.upstream_root, args.upstream_commit)
    python = judge_python(args)
    key = base_url = None
    if request["operation"] == "judge":
        base_url = getattr(args, "judge_base_url", None)
        request = {**request, "base_url": base_url}
        key = os.environ.get("OPENAI_API_KEY")
        if not key:
            raise ValueError("OPENAI_API_KEY is required for the upstream judge")
    diagnostics = args.out / "diagnostics"
    diagnostics.mkdir(mode=0o700, exist_ok=True)
    stderr = os.open(diagnostics / f"{name}.stderr", os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    env = child_env(args.out, key=key, base_url=base_url)
    try:
        process = await asyncio.create_subprocess_exec(
            str(python),
            "-I",
            "-B",
            str(BRIDGE),
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=stderr,
            env=env,
            cwd=args.out / "judge-home",
            start_new_session=True,
        )
    finally:
        os.close(stderr)
    try:
        payload = json.dumps({**request, "root": str(args.upstream_root), "commit": args.upstream_commit})
        stdout, _ = await asyncio.wait_for(process.communicate(payload.encode()), timeout)
        if process.returncode:
            raise ValueError("upstream adapter failed; the run remains ungraded")
        return json.loads(stdout)
    finally:
        if process.returncode is None:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGKILL)
            await process.wait()


def check_dataset(path: Path, digest: str) -> bytes:
    body = path.read_bytes()
    if hashlib.sha256(body).hexdigest() != digest:
        raise ValueError("dataset checksum mismatch")
    return body


def start_site(task: dict[str, Any]) -> str:
    site = urlparse(str(task.get("website") or ""))
    if site.scheme not in ("http", "https") or not site.hostname:
        raise ValueError(f"{task['task_id']}: Online-Mind2Web task has no usable start website")
    return site.geturl()


async def tasks_for(args: argparse.Namespace) -> list[dict[str, Any]]:
    body = check_dataset(args.dataset, args.dataset_sha256)
    if args.suite == Suite.BU_BENCH:
        if args.dataset.resolve() != (args.upstream_root / "BU_Bench_V2.enc").resolve():
            raise ValueError("the dataset must be the pinned runner's encrypted BU_Bench_V2.enc")
        loaded = await bridge(args, {"suite": args.suite, "operation": "load"}, "load")
        # Rubrics and weights stay inside the upstream process at judging time; the agent side keeps only the goal.
        tasks = [{"task_id": t["task_id"], "confirmed_task": t["confirmed_task"]} for t in loaded]
        expected = 200
    else:
        rows = json.loads(body)
        if not isinstance(rows, list):
            raise ValueError("Online-Mind2Web must be the complete official JSON task array")
        # The whole official row travels with the task (level, reference length, ...), not a projection of it.
        tasks = [{**t, "task_id": str(t["task_id"])} for t in rows]
        for t in tasks:
            start_site(t)
            if not isinstance(t.get("confirmed_task"), str) or not t["confirmed_task"].strip():
                raise ValueError(f"{t['task_id']}: missing task instruction")
        expected = 300
    if len(tasks) != expected or len({t["task_id"] for t in tasks}) != expected:
        raise ValueError(f"expected {expected} unique official tasks")
    # Task ids name private directories; upstream text never chooses a filesystem path.
    if any(not SAFE_ID.fullmatch(str(t["task_id"])) for t in tasks):
        raise ValueError("unsafe upstream task id")
    return tasks


def billing_config(args: argparse.Namespace) -> dict[str, float] | None:
    values = {key: getattr(args, flag) for flag, key in BILLING_FLAGS.items()}
    context = {key: getattr(args, flag, None) for flag, key in CONTEXT_PRICING_FLAGS.items()}
    if all(v is None for v in (*values.values(), *context.values())):
        return None
    if any(v is None or isinstance(v, bool) or not math.isfinite(v) or v <= 0 for v in values.values()):
        raise ValueError("judge billing needs every explicit pricing and ceiling flag, each positive and finite")
    if any(v is not None for v in context.values()):
        values.update(context)
    try:
        judge_charge(values, 0, 0)
    except BillingError as exc:
        raise ValueError(str(exc)) from exc
    return values


def receipts_usd(path: Path) -> float:
    """Judge dollars committed on disk: settled charges plus every unsettled reservation at its worst case."""
    open_usd: dict[int, float] = {}
    settled = 0.0
    for row in read_rows(path):
        if row["event"] == "reserved":
            open_usd[row["n"]] = row["reserved_usd"]
        elif row["event"] == "settled":
            open_usd.pop(row["n"], None)
            settled += row["usd"]
    return settled + sum(open_usd.values())


def known_agent_usd(result: dict[str, Any] | None) -> float | None:
    """The agent dollars a result states as known; no result, an unknown basis or a non-number is None."""
    if result is None or result.get("agent_cost_unknown", True):
        return None
    agent = result.get("agent_dollars")
    return float(agent) if isinstance(agent, (int, float)) and not isinstance(agent, bool) else None


def agent_usd(result: dict[str, Any], agent_budget: float) -> float:
    """The agent dollars a result commits: the known charge, or for an unknown one its hold's agent share, and no less
    than the part it states, which is a floor."""
    known = known_agent_usd(result)
    if known is not None:
        return known
    return max(agent_budget, known_agent_usd({**result, "agent_cost_unknown": False}) or 0.0)


def attempt_usd(folder: Path, result: dict[str, Any] | None, agent_budget: float, reserved: float) -> float:
    """What an attempt is known to have committed; an attempt with no result is held at its whole reservation, and
    one its provider refused to start (`NotStarted`) committed only the judge receipts it has, which is none."""
    if result is None:
        return reserved
    earlier = interruption_usd(result, agent_budget)
    if result.get("started") is False:
        return earlier + receipts_usd(folder / "receipts.jsonl")
    return earlier + agent_usd(result, agent_budget) + receipts_usd(folder / "receipts.jsonl")


@contextlib.asynccontextmanager
async def session_browser(
    http: httpx.AsyncClient, plan: dict[str, Any], chrome: Path | None
) -> AsyncIterator[Browser | None]:
    """The browser `browser_plan` chose, opened by the harness so opening and closing it stay outside the run's time,
    as on the live arms. Only a local Chrome described by the environment is left to the run, as None: it may be headed
    or use a profile, which fastbrowse alone starts, so its opening and closing cannot be told from the run."""
    if chrome is not None:
        async with local_browser(chrome) as browser:
            yield browser
    elif "browser_api_key" in plan:
        async with cloud_browser(http, screen=AGENT_SCREEN) as browser:
            yield browser
    else:
        yield None


def agent_fields(result: Any) -> dict[str, Any]:
    return {
        "status": result.status.value,
        "completed": result.status.value == "complete",
        "agent_dollars": result.cost.known_dollars,
        # An unsettled line is a floor: the charge it stands for is not yet known, whatever the sum says.
        "agent_cost_unknown": any(line.basis in (CostBasis.UNKNOWN, CostBasis.UNSETTLED) for line in result.cost.lines),
    }


def saved_timing(folder: Path) -> dict[str, Any]:
    """What timing.json recorded, as result fields; missing, malformed or non-finite seconds stay unknown, never zero."""
    try:
        timing = json.loads((folder / "timing.json").read_text())
        seconds = timing["agent_seconds"]
    except (OSError, ValueError, KeyError, TypeError):
        return {}
    ok = isinstance(seconds, (int, float)) and not isinstance(seconds, bool) and math.isfinite(seconds) and seconds >= 0
    return {"seconds": float(seconds), "timing_basis": timing.get("timing_basis")} if ok else {}


def saved_agent(folder: Path) -> dict[str, Any]:
    """Agent facts from the validated agent.json a failed judge left behind; missing or malformed stays unknown.

    Bad timing never discards the agent's known cost: the row simply carries no seconds.
    """
    try:
        run = RunResult.model_validate_json((folder / "agent.json").read_bytes())
    except (OSError, ValueError):
        return {}
    result = {**agent_fields(run), "answer": run.answer, "final_url": run.final_url}
    return result | saved_timing(folder)


def saved_check(folder: Path) -> dict[str, Any]:
    """The interruption check a run that failed later (at the upstream judge, say) left behind, so its charge counts."""
    try:
        check = json.loads((folder / "interruption.json").read_text())
    except (OSError, ValueError):
        return {}
    return {"interruption_check": check, "interruption_check_usd": check.get("judge_cost_usd")}


def _flag(name: str, value: str | None) -> bool:
    if value is None or value.lower() in {"", "0", "false"}:
        return False
    if value.lower() in {"1", "true"}:
        return True
    raise ValueError(f"{name} must be true or false")


def browser_plan(local_browser: Path | None = None) -> tuple[dict[str, Any], dict[str, Any]]:
    """`run_task`'s browser arguments, and the key-free record of the browser and Jev source a run will use: the
    --local-browser Chrome, else a Browser Use Cloud browser when BROWSER_USE_API_KEY is set, else local Chrome as
    FASTBROWSE_CHROME, FASTBROWSE_HEADED and FASTBROWSE_PROFILE describe it. The browser is passed explicitly, so the
    record is what the run was given."""
    jev = backend.providers()
    if local_browser is not None:
        chrome = LocalChrome(binary=str(local_browser), headed=False)
        return {"chrome": chrome}, {"browser": local_browser_identity(local_browser), "providers": jev}
    env = backend.environment()
    if key := env.get("BROWSER_USE_API_KEY"):
        cloud = {"browser_api_key": key, "cloud_timeout_minutes": CLOUD_BROWSER_MINUTES}
        return cloud, {"browser": {"mode": "browser-use-cloud"}, "providers": jev}
    profile = env.get("FASTBROWSE_PROFILE")
    chrome = LocalChrome(
        binary=env.get("FASTBROWSE_CHROME"),
        headed=_flag("FASTBROWSE_HEADED", env.get("FASTBROWSE_HEADED")),
        profile=Path(profile) if profile else None,
    )
    local = {"mode": "local-chrome", "headed": chrome.headed, "binary": chrome.binary, "profile": bool(chrome.profile)}
    return {"chrome": chrome}, {"browser": local, "providers": jev}


def session_failure(clock: AttemptClock, error: Exception, folder: Path) -> Exception:
    """What an attempt the clock stopped, or whose browser the provider could not give it, comes to: a browser session
    that did not open or close measured no agent, so it is retried as an outage, its record saved as any interrupted
    run's is. The clock never stops the agent itself."""
    if isinstance(error, TimeoutError) and not clock.expired():
        return error
    reason = f"the browser session did not {clock.phase} in time" if clock.expired() else str(error)
    result = RunResult(
        status=Status.UNAVAILABLE,
        answer=None,
        data=None,
        evidence=(),
        steps=(),
        cost=CostBreakdown(lines=()),
        artifacts=(),
        error=reason,
    )
    timing = {"seconds": clock.seconds(), "timing_basis": TIMING_BASIS}
    write_private(folder / "timing.json", json.dumps({"agent_seconds": timing["seconds"], **timing}))
    write_private(folder / "agent.json", result.model_dump_json(indent=2))
    # Past opening the agent may have spent, so its charge is held rather than called zero.
    run = {**agent_fields(result), **timing, "agent_cost_unknown": clock.phase != "open"}
    return Interrupted(reason, {"judge_cost_usd": 0.0}, run)


async def execute_task(args: argparse.Namespace, task: dict[str, Any], repeat: int) -> dict[str, Any]:
    folder = args.out / f"{task['task_id']}-{repeat}"
    folder.mkdir(mode=0o700)
    write_private(folder / "task.json", json.dumps(task))
    arm = getattr(args, "arm", "fastbrowse")
    if arm != "fastbrowse":
        return await execute_hosted(args, task, repeat, folder, arm)
    screenshots: list[str] = []
    actions: list[dict[str, Any]] = []

    clock = AttemptClock(bounded=True)

    async def event(value: Any) -> None:
        if isinstance(value, BrowserEvent):
            clock.ready(time.monotonic())
        if not isinstance(value, StepEvent):
            return
        # Model reasoning notes are not tool evidence and never reach either official judge.
        step = value.step.model_dump(mode="json", exclude={"note", "confidence", "decided_by"})
        actions.append(step)
        if value.frame is not None:
            target = folder / f"step-{value.step.index}.png"
            write_private(target, value.frame)
            screenshots.append(str(target.resolve()))
        append_row(folder / "steps.jsonl", step)

    chrome = getattr(args, "local_browser", None)
    plan, runtime = browser_plan(chrome) if chrome is not None else browser_plan()
    held: Browser | None = None
    with traced() as trace:
        try:
            async with httpx.AsyncClient(timeout=60) as http, clock, session_browser(http, plan, chrome) as held:
                asked, page_ready = setup_marks()
                if held is not None:
                    session_ready()
                    asked()
                result = await run_task(
                    task["confirmed_task"],
                    **(plan if held is None else {"cdp_url": held.cdp_url}),
                    page_ready=page_ready,
                    settings=backend.run_settings(),
                    # Online-Mind2Web tasks name their start site; BU Bench gives the agent the goal alone.
                    start=start_site(task) if args.suite == Suite.MIND2WEB else None,
                    on_event=event,
                    config=Config(step_frames=True),
                    limits=AGENT_LIMITS,
                    authorization=Authorization(irreversible_actions=args.authorize),
                    downloads=folder / "downloads",
                )
                session_answered()
        except (TimeoutError, Unavailable) as error:
            raise session_failure(clock, error, folder) from None
    if held is not None:
        result = result.model_copy(update={"cost": CostBreakdown(lines=(*result.cost.lines, *held.cost))})
    timing = {
        "seconds": clock.seconds(),
        "timing_basis": SESSION_INCLUDED if held is None else TIMING_BASIS,
        "session_open_seconds": clock.open_seconds if held is None else held.open_seconds,
        "session_close_seconds": None if held is None else held.close_seconds,
    }
    write_private(folder / "timing.json", json.dumps({"agent_seconds": timing["seconds"], **timing}))
    write_private(folder / "agent.json", result.model_dump_json(indent=2))
    cloud = runtime["browser"]["mode"] == "browser-use-cloud"
    where = "Browser Use Cloud browser, its cost folded into" if cloud else "Local Chrome, no cloud browser;"
    return await grade_run(
        args,
        task,
        repeat,
        folder,
        answer=result.answer,
        final_url=result.final_url,
        actions=actions,
        screenshots=screenshots,
        final_frame=result.final_frame,
        fields={
            "arm": "fastbrowse",
            **agent_fields(result),
            "error": result.error,
            **timing,
            # Who served the run: the Jev route and the LLM models its `route` trace event names, as internal rows do.
            "model": backend.providers(),
            "text_model": llm_models(providers(trace)),
            "failure_kind": failure_kind(trace),
        },
        cost_note=f"{where} agent_dollars, Core's metered spend; judge dollars are receipts_usd.",
    )


async def grade_run(
    args: argparse.Namespace,
    task: dict[str, Any],
    repeat: int,
    folder: Path,
    *,
    answer: str | None,
    final_url: str | None,
    actions: list[dict[str, Any]],
    screenshots: list[str],
    final_frame: bytes | None,
    fields: dict[str, Any],
    cost_note: str,
    notes: tuple[str, ...] = (),
) -> dict[str, Any]:
    """Grade one finished run with the pinned upstream judge, the same request whichever arm ran it."""
    if final_frame is None and CAPTURE_RETRIES.get() > 0:
        run = {**fields, "answer": answer, "final_url": final_url}
        raise Interrupted("the run left no final page capture", {"judge_cost_usd": 0.0}, run)
    check = await interruption_check(
        task, answer=answer, final_url=final_url, actions=actions, final_frame=final_frame, fields=fields
    )
    write_private(folder / "interruption.json", json.dumps(check))
    if judged_unavailable(check):
        reason = check["judgment"]["decisions"]["evidence_available"]["reason"]
        raise Interrupted(reason, check, {**fields, "answer": answer, "final_url": final_url})
    screenshots = list(screenshots)
    target = folder / "final.png"
    if final_frame is not None:
        write_private(target, final_frame)
        screenshots.append(str(target.resolve()))
    request: dict[str, Any] = {
        "suite": args.suite,
        "operation": "judge",
        "task_id": task["task_id"],
        "judge_model": args.judge_model,
        "artifact": str((folder / "judge").resolve()),
        "receipts": str((folder / "receipts.jsonl").resolve()),
        "billing": billing_config(args),
    }
    if args.suite == Suite.BU_BENCH:
        files: dict[str, str] = {}
        binary: list[dict[str, Any]] = []
        for path in sorted((folder / "downloads").rglob("*")):
            if not path.is_file():
                continue
            name = str(path.relative_to(folder / "downloads"))
            body = path.read_bytes()
            try:
                files[name] = body.decode()
            except UnicodeDecodeError:
                binary.append({"name": name, "bytes": len(body), "sha256": hashlib.sha256(body).hexdigest()})
        write_private(folder / "binary-outputs.json", json.dumps(binary))
        evidence = [
            "Screenshots run in order; each step's frame was taken before or after its action, and the last is the "
            "final page after the last action.",
            "Frames showing a resolved secret are withheld.",
            *notes,
        ]
        if final_frame is None:
            evidence[0] = "No final page capture was available; only step frames, taken before or after their action."
        if binary:
            evidence.append(f"{len(binary)} binary output file(s) were retained but are not shown to this text judge.")
        request.update(
            reasoning=args.judge_reasoning,
            trace={
                "agent_task": task["confirmed_task"],
                "final_result": answer or "No final result",
                "agent_steps": [json.dumps(a) for a in actions],
                "screenshots_b64": [base64.b64encode(Path(frame).read_bytes()).decode() for frame in screenshots],
                "screenshot_steps": None,
                "screenshot_timing": "after",
                "output_files_text": json.dumps(files),
                "evidence_notes": evidence,
            },
        )
    else:
        request.update(
            task=task["confirmed_task"],
            reasoning=args.judge_reasoning,
            actions=[json.dumps(a) for a in actions],
            screenshots=screenshots,
            score_threshold=args.score_threshold,
            reasoning_model=judge_family(args.judge_model).startswith(REASONING_JUDGES),
        )
    grade = await bridge(args, request, f"judge-{task['task_id']}-{repeat}")
    score = grade.get("score")
    if (
        not isinstance(score, (int, float))
        or isinstance(score, bool)
        or not math.isfinite(score)
        or not 0 <= score <= 1
    ):
        raise ValueError("upstream score is missing or outside zero to one")
    # An upstream rubric score is never converted to a solved-task percentage.
    return {
        "task": task["task_id"],
        "repeat": repeat,
        "answer": answer,
        "final_url": final_url,
        "final_capture_available": final_frame is not None,
        **fields,
        "interruption_check": check,
        "interruption_check_usd": check["judge_cost_usd"],
        "upstream_score": score,
        "upstream_passed": grade.get("passed") if args.suite == Suite.MIND2WEB else grade.get("verdict"),
        "cost_note": cost_note,
        "grader": grade,
    }


def hosted_goal(args: argparse.Namespace, task: dict[str, Any]) -> str:
    """What a hosted agent is asked: the official instruction, after the start site where the suite names one, as
    every live arm is asked (`fastbrowse.evals.live_tasks.prompt`); fastbrowse is given that site as its start."""
    return (
        f"Start at {start_site(task)}. {task['confirmed_task']}"
        if args.suite == Suite.MIND2WEB
        else task["confirmed_task"]
    )


async def execute_hosted(
    args: argparse.Namespace, task: dict[str, Any], repeat: int, folder: Path, arm: str
) -> dict[str, Any]:
    """One official task on a hosted agent (`HOSTED_ARMS`), with the evidence its product exposes: its transcript as
    the action history, the screenshots its transcript carries, and the final page captured through the DevTools
    address of the browser it ended in. Neither returns downloaded files, so a BU Bench judge is told so."""
    clock = AttemptClock(bounded=True)
    try:
        async with httpx.AsyncClient(timeout=60) as http, clock:
            ran = await hosted_official(http, hosted_goal(args, task), folder)
    except (TimeoutError, Unavailable) as error:
        raise session_failure(clock, error, folder) from None
    timing = ran.pop("timing")
    actions = ran.pop("actions")
    for step in actions:
        append_row(folder / "steps.jsonl", step)
    fields = {"arm": arm, **ran.pop("fields"), **timing}
    write_private(folder / "timing.json", json.dumps({"agent_seconds": timing["seconds"], **timing}))
    write_private(
        folder / "agent.json",
        json.dumps({**fields, "answer": ran["answer"], "final_url": ran["final_url"]}, default=str),
    )
    return await grade_run(
        args,
        task,
        repeat,
        folder,
        answer=ran["answer"],
        final_url=ran["final_url"],
        actions=actions,
        screenshots=ran["screenshots"],
        final_frame=ran["final_frame"],
        fields=fields,
        cost_note=ran["cost_note"],
        notes=("This agent's downloaded or written files are not collected, so none are shown.",),
    )


async def _saved_frames(http: httpx.AsyncClient, urls: list[str], folder: Path) -> list[str]:
    """Each presigned screenshot fetched at once (they expire in minutes) and saved privately, in order."""
    saved: list[str] = []
    for n, url in enumerate(urls):
        try:
            response = await http.get(url, follow_redirects=True, timeout=60)
        except httpx.HTTPError:
            continue
        if response.is_success:
            target = folder / f"step-{n}.png"
            write_private(target, response.content)
            saved.append(str(target.resolve()))
    return saved


async def hosted_official(http: httpx.AsyncClient, goal: str, folder: Path) -> dict[str, Any]:
    """The hosted Browser Use agent on one official task, exactly as the live arm runs it: uncapped, kept alive so
    its final page can be captured, then stopped."""
    from browser_use_sdk.v3 import AsyncBrowserUse, BrowserUseError

    from fastbrowse_evals.final_page import FinalPage, capture_browser, observe_browser
    from fastbrowse_evals.live import (
        HOSTED_UNCAPPED_USD,
        answered_at,
        hosted_browser,
        hosted_messages,
        hosted_session_ready,
        hosted_steps,
        hosted_timing,
    )

    client = AsyncBrowserUse(api_key=backend.browser_key())
    began = time.monotonic()
    run = client.run(goal, proxy_country_code="us", max_cost_usd=HOSTED_UNCAPPED_USD, keep_alive=True)
    finishing = asyncio.ensure_future(run)
    while run.session_id is None and not finishing.done():
        await asyncio.wait({finishing}, timeout=0.2)
    created_after = time.monotonic() - began
    ready = asyncio.ensure_future(hosted_session_ready(client, run.session_id, finishing))
    stop_answered = False
    closed = None
    try:
        await asyncio.gather(finishing, return_exceptions=True)
        session_answered()
        wall = time.monotonic() - began
        browser_started = await ready
        if run.session_id is None:
            error = finishing.exception()
            if isinstance(error, BrowserUseError) and refusal(error.status_code):
                raise NotStarted(f"Browser Use hosted agent returned HTTP {error.status_code}")
            raise RuntimeError("the hosted agent never started a session")
        session = await client.sessions.get(run.session_id)
        messages = await hosted_messages(client, str(session.id))
        shots = [url for m in messages if (url := getattr(m, "screenshot_url", None))]
        frames = await _saved_frames(http, shots, folder)
        websocket = await hosted_browser(client, http, str(session.id))
        final = websocket if isinstance(websocket, FinalPage) else await observe_browser(websocket)
        final_frame = None if isinstance(websocket, FinalPage) else await capture_browser(websocket, url=final.url)
    finally:
        ready.cancel()
        if run.session_id is not None:
            closing = time.monotonic()
            with contextlib.suppress(Exception):
                await asyncio.shield(client.sessions.stop(run.session_id))
                stop_answered = True
                closed = round(time.monotonic() - closing, 2)
    stopped = await client.sessions.get(session.id)
    step_log, _ = hosted_steps(messages, {"browser_use_api_key": backend.browser_key()})
    output = session.output
    # The stop settles the charge; a session left running keeps accruing past this figure, so it is unknown.
    cost = stopped.total_cost_usd if stop_answered or stopped.status.value != "idle" else None
    return {
        "timing": {
            **hosted_timing(created_after, session, browser_started, answered_at(messages, output), wall),
            "session_close_seconds": closed,
        },
        "actions": [s for s in step_log if isinstance(s, dict)],
        "answer": output if isinstance(output, str) or output is None else json.dumps(output, default=str),
        "final_url": final.url,
        "screenshots": frames,
        "lost_screenshots": len(shots) - len(frames),
        "final_frame": final_frame,
        "fields": {
            "status": session.status.value,
            "completed": session.status.value in {"idle", "stopped"} and bool(output),
            "agent_dollars": None if cost is None else float(cost),
            "agent_cost_unknown": cost is None,
            "model": str(getattr(session.model, "value", session.model)),
        },
        "cost_note": "Hosted Browser Use session, agent_dollars is its metered total; judge dollars are receipts_usd.",
    }


def run_id(task: str, repeat: int, retries: int = 0) -> str:
    """One physical run's name in the budget journal: the slot's name, then each retry's own."""
    return f"{task}-{repeat}" if retries == 0 else f"{task}-{repeat}.retry-{retries}"


def retry_wait(retries: int) -> int:
    """Seconds before a slot interrupted `retries` times already may run again."""
    return min(60 * 2**retries, 600)


def blocked_by_site(result: dict[str, Any]) -> bool:
    """The site kept every attempt of this slot from an outcome (`Interrupted`). It is settled, not incomplete: it has
    no upstream grade, and publication records it as a failed row naming why."""
    return result.get("error_type") == Interrupted.__name__


def unsettled(result: dict[str, Any] | None) -> bool:
    """No result, or none a run can stand on: an ungraded slot the site did not block."""
    return result is None or (result.get("upstream_score") is None and not blocked_by_site(result))


def failure_kind(events: Iterable[object]) -> str | None:
    """What failed, by error type: the agent's `run_failed` event holds no page or task text."""
    return next((str(e["kind"]) for e in events if isinstance(e, dict) and e.get("event") == "run_failed"), None)


def result_line(row: dict[str, Any], dollars: float) -> str:
    """One finished slot for the public job log, the workflow's public row facts alone: its opaque id, the agent's
    status, score, seconds, dollars, the exception's type and the failure's kind. Never an answer, reason or message, which carry task
    text."""
    seconds = row.get("seconds")
    return (
        f"RESULT {row['task']}-{row['repeat']} status={row.get('status')} score={row.get('upstream_score')} "
        f"seconds={'None' if seconds is None else round(seconds)} dollars={dollars:.3f} cause={row.get('error_type')} "
        f"kind={row.get('failure_kind')}"
    )


@dataclass
class Slot:
    """One task and repeat, across every physical run it takes."""

    task: dict[str, Any]
    repeat: int
    row: dict[str, Any]
    interruptions: list[dict[str, Any]] = field(default_factory=list)


async def run_attempts(
    args: argparse.Namespace, pending: list[tuple[dict[str, Any], int]], committed: float, reserve: float
) -> bool:
    """Run attempts on up to args.concurrency workers; True when any failed, was ungraded or was left unstarted.

    The start gate (`start-gate-v1`): one condition guards the reservation and the reconciliation, so no worker
    begins a physical run unless its accounting hold (agent, browser and judge reserve) fits beside everything
    committed. A run that does not fit waits while another is in flight, since its reconciliation may free room; only
    an idle ledger that still cannot fit stops the run. The hold is accounting only: the agent itself runs uncapped,
    and a charge above its hold is recorded as it is without stopping dispatch.

    An interrupted run settles like any other and its slot leaves the worker until `retry_wait` has passed, so a site
    that stays down takes one worker for its attempts alone. A due retry goes first, ahead of tasks not yet begun: it
    has spent money already and a finished slot frees nothing else. No worker exits while a run is in flight or a slot
    is waiting, since either can hand it more to do.
    """
    billing = billing_config(args)
    judge_budget = billing["budget_usd"] if billing is not None else 0.0
    queue = deque(pending)
    waiting: list[tuple[float, int, Slot]] = []
    arrivals = count()
    gate = asyncio.Condition()
    ledger = {"committed": committed, "running": 0, "incomplete": False}
    clock = asyncio.get_running_loop().time

    def hold(run: str) -> None:
        ledger["committed"] += reserve
        ledger["running"] += 1
        append_row(args.out / JOURNAL, journal_row(run, args.budget, reserve, reserve))

    def settle(run: str, charge: float) -> None:
        append_row(args.out / JOURNAL, journal_row(run, args.budget, reserve, charge))
        ledger["committed"] += charge - reserve
        ledger["running"] -= 1
        gate.notify_all()

    def begin() -> Slot:
        task, repeat = queue.popleft()
        row = {
            "task": task["task_id"],
            "repeat": repeat,
            "started": True,
            "reserved_usd": reserve,
            "agent_limits": AGENT_LIMITS.model_dump(mode="json"),
        }
        append_row(args.out / "attempts.jsonl", row)
        hold(run_id(task["task_id"], repeat))
        return Slot(task, repeat, row)

    async def next_slot() -> Slot | None:
        """The slot to run now, holding its run's reservation; None when nothing is left for this worker."""
        while True:
            due = bool(waiting) and waiting[0][0] <= clock()
            if due or queue:
                if ledger["committed"] + reserve <= args.budget + 1e-9:
                    if not due:
                        return begin()
                    slot = heappop(waiting)[2]
                    hold(run_id(slot.task["task_id"], slot.repeat, len(slot.interruptions)))
                    return slot
                if ledger["running"] == 0:
                    # A waiting slot keeps what it recorded and stays without a result, as an unstarted task does.
                    ledger["incomplete"] = True
                    queue.clear()
                    waiting.clear()
                    gate.notify_all()
                    return None
            elif not waiting and ledger["running"] == 0:
                gate.notify_all()
                return None
            with contextlib.suppress(TimeoutError):
                await asyncio.wait_for(gate.wait(), None if due or not waiting else waiting[0][0] - clock())

    async def attempt(slot: Slot) -> None:
        """One physical run of `slot`: it settles here, and the slot either waits for its next run or is finished."""
        task, repeat, row, interruptions = slot.task, slot.repeat, slot.row, slot.interruptions
        retries = len(interruptions)
        folder = args.out / f"{task['task_id']}-{repeat}"
        CAPTURE_RETRIES.set(OUTAGE_RETRIES - retries)
        try:
            row.update(await execute_task(args, task, repeat))
        except Interrupted as exc:
            run = {
                **exc.run,
                "reason": str(exc),
                "interruption_check": exc.check,
                "interruption_check_usd": exc.check["judge_cost_usd"],
            }
            if retries < OUTAGE_RETRIES:
                # The interrupted run keeps its own folder beside the slot's, and the slot runs again.
                kept = folder.rename(folder.with_name(f"{folder.name}.interrupted-{retries}"))
                wait = retry_wait(retries)
                interruptions.append(
                    {**run, "folder": kept.name, "judge_usd": receipts_usd(kept / "receipts.jsonl"), "wait": wait}
                )
                row["interruptions"] = interruptions
                # A public job log: the reason quotes the site and the task, so it stays in the private rows.
                print(f"RETRY {task['task_id']}-{repeat} in {wait}s", flush=True)
                async with gate:
                    heappush(waiting, (clock() + wait, next(arrivals), slot))
                    settle(run_id(task["task_id"], repeat, retries), run_usd(interruptions[-1], reserve - judge_budget))
                return
            # Still interrupted after every retry: the site blocked this slot, on any arm and suite alike.
            row.update(run, upstream_score=None, upstream_passed=None, error_type=Interrupted.__name__)
        except Exception as exc:
            row.update(
                upstream_score=None,
                upstream_passed=None,
                error_type=type(exc).__name__,
                started=not isinstance(exc, NotStarted),
                **saved_agent(folder),
                **saved_check(folder),
            )
        row["judge_usd"] = receipts_usd(folder / "receipts.jsonl")
        append_row(args.out / "results.jsonl", row)
        async with gate:
            charge = attempt_usd(folder, {**row, "interruptions": []}, reserve - judge_budget, reserve)
            settle(run_id(task["task_id"], repeat, retries), charge)
            ledger["incomplete"] = ledger["incomplete"] or unsettled(row)
        print(result_line(row, charge), flush=True)

    async def worker() -> None:
        while True:
            async with gate:
                slot = await next_slot()
            if slot is None:
                return
            await attempt(slot)

    # A cancelled worker leaves its started receipt and reservation in place; the group cancels its siblings.
    async with asyncio.TaskGroup() as group:
        for _ in range(min(args.concurrency, len(pending))):
            group.create_task(worker())
    return bool(ledger["incomplete"])


def spaced_sample(tasks: list[dict[str, Any]], size: int) -> list[dict[str, Any]]:
    """`size` tasks spaced evenly across the suite's fixed order: no randomness and no choice by name, so the sample
    cannot be tuned. The same suite and size always give the same tasks."""
    return [tasks[i * len(tasks) // size] for i in range(size)]


def config_fingerprint(config: dict[str, Any]) -> str:
    return hashlib.sha256(json.dumps(config, sort_keys=True).encode()).hexdigest()


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="fastbrowse-evals browser official", description=__doc__)
    parser.add_argument("suite", type=Suite, choices=[Suite.BU_BENCH, Suite.MIND2WEB])
    parser.add_argument("--upstream-root", required=True, type=Path)
    parser.add_argument("--upstream-commit", required=True)
    parser.add_argument(
        "--judge-python", required=True, type=Path, help="prepared virtualenv Python inside the upstream root"
    )
    parser.add_argument("--dataset", required=True, type=Path)
    parser.add_argument("--dataset-sha256", required=True)
    parser.add_argument(
        "--judge-model", default=JUDGE_MODEL, help="model id on the judge route; a published run uses the default"
    )
    parser.add_argument("--judge-reasoning", default="medium")
    parser.add_argument(
        "--judge-base-url",
        default=OPENROUTER_BASE_URL,
        help="OpenAI-compatible endpoint the upstream judge calls; a published run uses the default, OpenRouter",
    )
    parser.add_argument("--score-threshold", type=int, default=3)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument(
        "--resume", action="store_true", help="continue a run directory without redoing any started attempt"
    )
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument(
        "--task-id",
        action="append",
        dest="task_ids",
        metavar="ID",
        help="run only this task (repeatable); every id must be in the loaded official set, and a subset never publishes",
    )
    parser.add_argument(
        "--sample",
        type=int,
        metavar="N",
        help="smoke run: N tasks spaced evenly across the suite's fixed order; a sampled run never publishes",
    )
    parser.add_argument("--concurrency", type=int, default=1, help="task workers sharing one start-gate ledger")
    # Removed: no official agent is capped. Each is refused by name rather than silently ignored.
    for removed in ("--max-steps", "--max-llm-calls", "--max-jev-calls", "--task-timeout", "--agent-budget"):
        parser.add_argument(removed, help=argparse.SUPPRESS)
    parser.add_argument("--campaign-budget-only", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument(
        "--reserve-dollars",
        type=float,
        default=1,
        help="agent dollars held per in-flight task for budget accounting only; never a limit on the agent",
    )
    parser.add_argument(
        "--local-browser", type=Path, help="headless local Chrome binary with an owned temporary profile"
    )
    parser.add_argument("--browser-reserve-dollars", type=float, default=1)
    parser.add_argument(
        "--budget", type=float, help="approved total over every attempt; it only gates whether the next task starts"
    )
    parser.add_argument("--judge-input-usd-per-mtok", type=float)
    parser.add_argument("--judge-output-usd-per-mtok", type=float)
    parser.add_argument(
        "--judge-context-threshold-tokens", type=int, help="input tokens above which a higher tier applies"
    )
    parser.add_argument(
        "--judge-context-input-usd-per-mtok", type=float, help="full-request input price above the threshold"
    )
    parser.add_argument(
        "--judge-context-output-usd-per-mtok", type=float, help="full-request output price above the threshold"
    )
    parser.add_argument("--judge-pricing-checked", help="date the prices were confirmed against the provider's pages")
    parser.add_argument("--judge-max-requests", type=int, help="provider requests per task, retries included")
    parser.add_argument("--judge-max-input-tokens", type=int, help="upper-bounded input tokens per task")
    parser.add_argument(
        "--judge-max-output-tokens", type=int, help="largest output ceiling any single request may declare"
    )
    parser.add_argument(
        "--judge-image-tokens",
        type=int,
        help="verified ceiling of input tokens one image can bill on --judge-model (see the provider's image pricing)",
    )
    parser.add_argument("--judge-budget", type=float, help="dollars per task, enforced before each provider request")
    parser.add_argument("--execute", action="store_true")
    parser.add_argument("--authorize", action="store_true")
    parser.add_argument(
        "--arm",
        choices=OFFICIAL_ARMS,
        default="fastbrowse",
        help="the agent to run; only fastbrowse's runs are published, the others are measured on the same tasks",
    )
    return parser


def attempt_state(out: Path) -> tuple[dict[tuple[str, int], dict[str, Any]], dict[tuple[str, int], dict[str, Any]]]:
    def keyed(name: str) -> dict[tuple[str, int], dict[str, Any]]:
        rows: dict[tuple[str, int], dict[str, Any]] = {}
        for r in read_rows(out / name):
            key = (r["task"], r["repeat"])
            if key in rows:
                raise ValueError(f"{name} has two rows for {key}; the run directory is corrupt")
            rows[key] = r
        return rows

    return keyed("attempts.jsonl"), keyed("results.jsonl")


async def main(argv: list[str]) -> int:
    os.umask(0o077)
    parser = build_parser()
    args = parser.parse_args(argv)
    if judge_family(args.judge_model).startswith(("o1", "o3", "o4")) and args.judge_reasoning not in {
        "low",
        "medium",
        "high",
    }:
        parser.error("the selected reasoning judge supports low, medium or high effort")
    if args.judge_base_url is not None and urlparse(args.judge_base_url).scheme != "https":
        parser.error("--judge-base-url must be an https URL")
    if args.campaign_budget_only:
        parser.error("--campaign-budget-only was removed: every run is uncapped and --budget only gates new tasks")
    for removed in ("max_steps", "max_llm_calls", "max_jev_calls", "task_timeout", "agent_budget"):
        if getattr(args, removed) is not None:
            flag = "--" + removed.replace("_", "-")
            parser.error(f"{flag} was removed: no official agent has a step, time, call or dollar limit")
    checkout(args.upstream_root, args.upstream_commit)
    if args.repeat < 1 or args.concurrency < 1 or not math.isfinite(args.reserve_dollars) or args.reserve_dollars <= 0:
        parser.error("a positive finite reservation and at least one repeat and worker are required")
    if not math.isfinite(args.browser_reserve_dollars) or args.browser_reserve_dollars < 0:
        parser.error("browser reserve must be nonnegative finite dollars")
    if args.browser_reserve_dollars == 0 and args.local_browser is None:
        parser.error("zero browser reserve requires an explicitly local browser")
    if args.arm != "fastbrowse" and args.local_browser is not None:
        parser.error("a hosted agent runs in its own provider's browser, never a local one")
    try:
        billing = billing_config(args)
        python = judge_python(args)
        runtime = (
            HOSTED_RUNTIME[args.arm]
            if args.arm != "fastbrowse"
            else browser_plan(args.local_browser)[1]
            if args.local_browser is not None
            else browser_plan()[1]
        )
    except (ValueError, RuntimeError) as exc:
        parser.error(str(exc))
    try:
        private_root(args.out)
    except ValueError as exc:
        parser.error(str(exc))
    if args.resume:
        if not args.out.is_dir():
            parser.error("--resume needs the original run directory")
    else:
        args.out.mkdir(mode=0o700, parents=True, exist_ok=False)
    reserve = args.reserve_dollars + args.browser_reserve_dollars + (billing["budget_usd"] if billing else 0.0)
    if args.execute:
        try:
            require_judges(use_jev=False)
        except JudgeConfigError as exc:
            parser.error(f"every run's interruption check needs its judge configured: {exc}")
        if billing is None or not args.judge_pricing_checked:
            parser.error("execution requires explicit judge prices, their check date and every judge ceiling")
        if args.budget is None or not math.isfinite(args.budget) or args.budget <= 0:
            parser.error("execution requires an explicitly approved total --budget")
    if args.sample is not None and (args.sample < 1 or args.task_ids):
        parser.error("--sample needs a positive size and takes no --task-id")
    tasks = await tasks_for(args)
    if args.sample is not None:
        if args.sample > len(tasks):
            parser.error(f"--sample {args.sample} exceeds the {len(tasks)} tasks of the official {args.suite} set")
        tasks = spaced_sample(tasks, args.sample)
    if args.task_ids:
        # Checked against the whole loaded set, so a mistyped id fails here instead of quietly running fewer tasks.
        if unknown := sorted(set(args.task_ids) - {t["task_id"] for t in tasks}):
            parser.error(f"--task-id names tasks outside the official {args.suite} set: {', '.join(unknown)}")
        tasks = [t for t in tasks if t["task_id"] in set(args.task_ids)]
    probe = await bridge(args, {"suite": args.suite, "operation": "probe"}, "probe")
    if probe.get("seams_ok") is not True:
        parser.error("the pinned upstream does not expose a call seam the judge meter can bound; execution is unsafe")
    config = {
        **BENCHMARKS[args.suite].model_dump(mode="json"),
        "upstream_commit": args.upstream_commit,
        "dataset_sha256": args.dataset_sha256,
        "arm": args.arm,
        "judge_model": args.judge_model,
        "judge_base_url": args.judge_base_url,
        "judge_reasoning": args.judge_reasoning,
        "judge_request_policy": "reasoning-completion-ceiling-v2",
        "interruption_policy": INTERRUPTION_POLICY,
        "budget_policy": POLICY,
        "judge_python": str(python),
        "judge_versions": probe["versions"],
        "judge_billing": billing,
        "judge_pricing_checked": args.judge_pricing_checked,
        "score_threshold": args.score_threshold,
        "repeats": args.repeat,
        "concurrency": args.concurrency,
        "agent_runtime": runtime,
        "agent_limits": AGENT_LIMITS.model_dump(mode="json"),
        "task_ids": [t["task_id"] for t in tasks],
        "sample": args.sample,
        "screenshot_timing": FRAME_TIMING[args.suite.value],
        "authorize_irreversible_actions": args.authorize,
        "max_steps": None,
        "task_timeout": None,
        "session_open_limit_seconds": SESSION_OPEN_SECONDS,
        "timing_basis": TIMING_BASIS,
        "agent_budget": None,
        "reserve_dollars": args.reserve_dollars,
        "browser_reserve_dollars": args.browser_reserve_dollars,
        "cloud_browser_timeout_minutes": CLOUD_BROWSER_MINUTES,
        "provider_limits": list(PROVIDER_LIMITS),
    }
    if args.arm == "browser-use":
        config["hosted_max_cost_usd"] = HOSTED_UNCAPPED_USD
    fingerprint = config_fingerprint(config)
    manifest_path = args.out / "manifest.json"
    if args.resume:
        if not manifest_path.exists() or json.loads(manifest_path.read_text()).get("config_sha256") != fingerprint:
            parser.error("--resume needs the original run directory and identical configuration")
        if args.execute and json.loads(manifest_path.read_text()).get("total_budget") not in (None, args.budget):
            parser.error("the approved total budget gates every start of a run; --resume cannot change --budget")
    else:
        manifest = {
            **config,
            "config_sha256": fingerprint,
            "total_budget": args.budget,
            "published": False,
            "run": provenance(benchmark_origin="upstream", benchmark_suite=args.suite.value),
            "directly_comparable": False,
            "comparison_limits": ["published comparator task/config/judge identity must be verified"],
        }
        write_private(manifest_path, json.dumps(manifest, indent=2))
    if not args.execute:
        print(f"{args.suite}: {len(tasks)} tasks on {args.arm}, preflight complete; no agent or judge calls")
        return 0
    with (args.out / "run.lock").open("a") as lock:
        try:
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            parser.error("another process is already running this output directory")
        try:
            attempts, results = attempt_state(args.out)
        except ValueError as exc:
            parser.error(str(exc))
        saved = json.loads(manifest_path.read_text())
        if saved.get("total_budget") is None:
            # A preflight manifest has no budget yet; nothing can have started, so the start gate is fixed now.
            if attempts:
                parser.error("the original manifest recorded no total budget for a started run")
            write_private(manifest_path, json.dumps({**saved, "total_budget": args.budget}, indent=2))
        committed = sum(
            attempt_usd(
                args.out / f"{k[0]}-{k[1]}",
                results.get(k),
                row["reserved_usd"] - (billing["budget_usd"] if billing else 0.0),
                row["reserved_usd"],
            )
            for k, row in attempts.items()
        )
        incomplete = any(unsettled(results.get(k)) for k in attempts)
        # A started attempt may already have spent agent money; it is never repeated.
        pending = [
            (task, repeat)
            for task in tasks
            for repeat in range(args.repeat)
            if (task["task_id"], repeat) not in attempts
        ]
        incomplete = await run_attempts(args, pending, committed, reserve) or incomplete
        print(f"{args.suite}: private results saved, publication withheld")
        return 1 if incomplete else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main(sys.argv[1:])))
