"""Benchmark any implementation of `fastbrowse serve --stdio` on the fixture suites, with model traffic replayed.

    uv run python -m fastbrowse.evals.bench record  [--only TASK ...] [--budget 5]
    uv run python -m fastbrowse.evals.bench fill    [--only TASK ...] [--latency zero|recorded] [--passes 6]
    uv run python -m fastbrowse.evals.bench replay  [--latency zero|recorded] [--agent "CMD"] [--out FILE]
    uv run python -m fastbrowse.evals.bench compare A.json B.json
    uv run python -m fastbrowse.evals.bench report  RUN.json ... [--markdown FILE] [--json FILE]

The harness speaks to the agent over the protocol alone, as the JavaScript SDK does, so the same commands measure
the Python package today and a Rust binary later (`--agent "target/release/fastbrowse serve --stdio"`). For every
task it serves the fixture site on a fixed port, starts one headless Chrome and hands it over by `cdp_url`, starts
the record or replay proxy (`replay.py`), and starts one agent process. Chrome belongs to the harness, so the agent's
CPU time and peak RSS are its own.

What is measured per task: spawn to the `initialize` reply (startup), spawn to the `run` reply (wall), the agent
process's user and system CPU and peak RSS (from `wait4`, so only that process and what it waited for), and the
step count. The result is graded by the suite's own checks against what the site recorded.

A live recording is not enough for a replay at zero latency. The agent asks for its plan while the browser starts
and takes its first step without it if it has not arrived (`agent.py`, `if state.planning.done()`), so the first
Jev state carries no requirements when the plan took three seconds and carries them when it took none. `fill`
replays what the cassette has and sends only the misses upstream, pass after pass, until a run at the chosen
latency asks nothing new.
"""

import argparse
import asyncio
import base64
import contextlib
import difflib
import json
import os
import platform
import shlex
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from collections.abc import Callable, Generator
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import UTC, datetime
from importlib.metadata import PackageNotFoundError, distribution, version
from pathlib import Path
from typing import Any

from packaging.requirements import Requirement
from pydantic import ValidationError

from fastbrowse.adapters.local_chrome import find_chrome, local_chrome
from fastbrowse.clients.environment import DEFAULT_LLM, DEFAULT_REASONING, FIELD_TEXT_LLM
from fastbrowse.evals.local import fixture_server
from fastbrowse.evals.mock import mock_site
from fastbrowse.evals.mock_tasks import TASKS as MOCK_TASKS
from fastbrowse.evals.mock_tasks import MockTask
from fastbrowse.evals.replay import JEV_MOUNT, LLM_MOUNT, Cassette, Latency, Proxy, openrouter_forward
from fastbrowse.evals.runner import LOCAL_LIMITS, MOCK_LIMITS
from fastbrowse.evals.tasks import TASKS as LOCAL_TASKS
from fastbrowse.evals.tasks import LocalTask
from fastbrowse.models import LocalChrome, RunResult
from fastbrowse.protocol import PROTOCOL_VERSION
from fastbrowse.safety import origin_of

ROOT = Path(__file__).resolve().parents[3]
CASSETTES = ROOT / "bench" / "cassettes"
ARTIFACTS = ROOT / "artifacts" / "bench"

PORT_BASE = 47300
"""Task n's site is served on PORT_BASE + n. The address reaches the model's prompts, and a URL the model writes
back (a shortcut, a link it chose) has to lead to the same site when the run is replayed."""

TIMING_SENSITIVE = {
    "mock-support-portal": "the page shows its modal 1.2 s after load, and the step it lands on depends on agent speed",
}
"""Tasks whose result depends on wall time, not on the model responses. A diff in one is reported, and labelled."""

TASK_SECONDS = 900.0
SHUTDOWN_SECONDS = 30.0

AGENT_ENV_DROPPED = ("OPENROUTER_API_KEY", "AI_GATEWAY_API_KEY", "TYPESAFE_API_KEY", "BROWSER_USE_API_KEY")
"""Never handed to the agent: its model traffic goes to the proxy, and only the proxy holds a real key."""

PLACEHOLDER_KEY = "bench-placeholder-key"


def default_agent() -> str:
    """The `fastbrowse` script of the interpreter running the harness, without `uv run` in front of it."""
    return f"{shlex.quote(str(Path(sys.executable).parent / 'fastbrowse'))} serve --stdio"


def agent_environment(proxy: str) -> dict[str, str]:
    """The agent's environment: the caller's, minus every model key and fastbrowse setting, plus the proxy routes.

    The models are named explicitly because a custom LLM endpoint has no defaults, and they are the defaults a run
    keyed with OPENROUTER_API_KEY uses. The reasoning effort is set because a custom endpoint is sent one only when
    it is.
    """
    env = {
        name: value
        for name, value in os.environ.items()
        if name not in AGENT_ENV_DROPPED and not name.startswith("FASTBROWSE_")
    }
    return env | {
        "OPENROUTER_API_KEY": PLACEHOLDER_KEY,
        "FASTBROWSE_JEV_SOURCE": "openrouter",
        "FASTBROWSE_JEV_BASE_URL": proxy + JEV_MOUNT,
        "FASTBROWSE_LLM_BASE_URL": proxy + LLM_MOUNT,
        "FASTBROWSE_LLM_API_KEY": PLACEHOLDER_KEY,
        "FASTBROWSE_LLM_MODEL": DEFAULT_LLM,
        "FASTBROWSE_LLM_MODEL_FIELD_TEXT": FIELD_TEXT_LLM,
        "FASTBROWSE_LLM_MODEL_SHORTCUT": FIELD_TEXT_LLM,
        "FASTBROWSE_LLM_REASONING": DEFAULT_REASONING.value,
    }


@dataclass(frozen=True)
class BenchTask:
    suite: str
    task: LocalTask | MockTask
    port: int

    @property
    def id(self) -> str:
        return self.task.id


def corpus(port_base: int = PORT_BASE) -> list[BenchTask]:
    tasks: list[LocalTask | MockTask] = [*LOCAL_TASKS, *MOCK_TASKS]
    return [
        BenchTask("local" if isinstance(task, LocalTask) else "mock", task, port_base + index)
        for index, task in enumerate(tasks)
    ]


type Grader = Callable[[RunResult], tuple[str | None, dict[str, Any]]]


@contextmanager
def fixture_site(task: BenchTask) -> Generator[tuple[str, Grader]]:
    """The task's site on its own port, and the suite's grader bound to what that site records."""
    if isinstance(task.task, LocalTask):
        local = task.task
        with fixture_server(task.port) as (base_url, recorder):

            def grade_local(result: RunResult) -> tuple[str | None, dict[str, Any]]:
                return local.check(result, recorder.snapshot()), {}

            yield base_url, grade_local
        return
    mock = task.task
    with mock_site(task.port) as (base_url, site):

        def grade_mock(result: RunResult) -> tuple[str | None, dict[str, Any]]:
            extra = {
                "signed_in": list(site.sign_ins),
                "posts": [path for path, _ in site.posts],
                "orders_placed": len(site.orders_placed),
            }
            return mock.check(result, site), extra

        yield base_url, grade_mock


def run_params(task: BenchTask, base_url: str, cdp_url: str, run_id: str) -> dict[str, Any]:
    """The `run` request the fixture runner's arguments become, with the harness's Chrome handed over."""
    spec = task.task
    limits = LOCAL_LIMITS if task.suite == "local" else MOCK_LIMITS
    params: dict[str, Any] = {
        "run_id": run_id,
        "task": spec.task,
        "start": base_url + spec.start,
        "limits": limits.model_dump(mode="json", exclude_none=True),
        "authorization": spec.authorization.model_dump(mode="json"),
        "cdp_url": cdp_url,
    }
    if spec.inputs:
        params["inputs"] = dict(spec.inputs)
    if spec.output_schema is not None:
        params["output_schema"] = spec.output_schema.model_json_schema()
    if isinstance(spec, MockTask):
        if spec.secrets:
            origin = origin_of(base_url)
            params["secrets"] = [{"name": name, "origins": [origin]} for name in spec.secrets]
        if spec.attachments:
            params["attachments"] = [
                {"name": a.name, "mime_type": a.mime_type, "content": base64.b64encode(a.content).decode("ascii")}
                for a in spec.attachments
            ]
    return params


class AgentProcess:
    """One agent process over stdio, reaped with `wait4` so its resource use is read apart from Chrome's."""

    def __init__(self, argv: list[str], env: dict[str, str], cwd: Path, stderr: Path) -> None:
        self._stderr = stderr.open("wb")
        self.spawned = time.perf_counter()
        self.proc = subprocess.Popen(
            argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self._stderr, env=env, cwd=cwd
        )
        self._lines: asyncio.Queue[bytes | None] = asyncio.Queue()
        self._loop = asyncio.get_running_loop()
        threading.Thread(target=self._pump, name="bench-agent-stdout", daemon=True).start()
        self._ids = 0

    def _pump(self) -> None:
        assert self.proc.stdout is not None
        for line in self.proc.stdout:
            self._loop.call_soon_threadsafe(self._lines.put_nowait, line)
        self._loop.call_soon_threadsafe(self._lines.put_nowait, None)

    async def send(self, message: dict[str, Any]) -> None:
        line = json.dumps(message).encode() + b"\n"
        assert self.proc.stdin is not None
        stdin = self.proc.stdin

        def write() -> None:
            stdin.write(line)
            stdin.flush()

        with contextlib.suppress(BrokenPipeError, ValueError):
            await asyncio.to_thread(write)

    async def call(self, method: str, params: dict[str, Any] | None = None) -> int:
        self._ids += 1
        message: dict[str, Any] = {"jsonrpc": "2.0", "id": self._ids, "method": method}
        if params is not None:
            message["params"] = params
        await self.send(message)
        return self._ids

    async def receive(self) -> dict[str, Any] | None:
        line = await self._lines.get()
        return None if line is None else json.loads(line)

    def close_stdin(self) -> None:
        with contextlib.suppress(BrokenPipeError, OSError):
            assert self.proc.stdin is not None
            self.proc.stdin.close()

    def reap(self, timeout: float) -> tuple[int, Any]:
        """Wait for the process, killing it after `timeout`, and return its exit code and resource usage."""
        deadline = time.monotonic() + timeout
        while True:
            pid, status, usage = os.wait4(self.proc.pid, os.WNOHANG)
            if pid:
                break
            if time.monotonic() > deadline:
                self.proc.kill()
                _, status, usage = os.wait4(self.proc.pid, 0)
                break
            time.sleep(0.02)
        # Reaped here, so Popen must not try again.
        self.proc.returncode = os.waitstatus_to_exitcode(status)
        self._stderr.close()
        return self.proc.returncode, usage


@dataclass
class Drive:
    startup: float | None
    wall: float | None
    result: dict[str, Any] | None
    error: dict[str, Any] | None
    events: int
    secrets_resolved: int


async def drive(agent: AgentProcess, params: dict[str, Any], secrets: dict[str, str], seconds: float) -> Drive:
    """Initialize, run one task, answer the run's callbacks, and shut down."""
    startup = wall = None
    result = error = None
    events = resolved = 0
    init_id = await agent.call("initialize", {"protocol_version": PROTOCOL_VERSION})
    run_id: int | None = None
    try:
        async with asyncio.timeout(seconds):
            while True:
                message = await agent.receive()
                if message is None:
                    error = {"message": "the agent closed its output before replying"}
                    break
                method = message.get("method")
                if method is None:
                    if message.get("id") == init_id:
                        startup = time.perf_counter() - agent.spawned
                        if "error" in message:
                            error = message["error"]
                            break
                        run_id = await agent.call("run", params)
                    elif message.get("id") == run_id:
                        wall = time.perf_counter() - agent.spawned
                        result, error = message.get("result"), message.get("error")
                        break
                elif method == "run/event":
                    events += 1
                elif method == "secrets/resolve":
                    resolved += 1
                    name = message["params"]["name"]
                    await agent.send({"jsonrpc": "2.0", "id": message["id"], "result": secrets.get(name)})
                elif method == "run/until":
                    await agent.send({"jsonrpc": "2.0", "id": message["id"], "result": True})
    except TimeoutError:
        error = {"message": f"no run reply within {seconds:.0f}s"}
        await agent.send({"jsonrpc": "2.0", "method": "run/cancel", "params": {"run_id": params["run_id"]}})
    shutdown_id = await agent.call("shutdown")
    with contextlib.suppress(TimeoutError):
        async with asyncio.timeout(SHUTDOWN_SECONDS):
            while (message := await agent.receive()) is not None and message.get("id") != shutdown_id:
                pass
    agent.close_stdin()
    return Drive(startup, wall, result, error, events, resolved)


def normalized(result: RunResult | None, error: dict[str, Any] | None, grade: dict[str, Any]) -> dict[str, Any]:
    """What two runs given the same model responses must agree on. Timings and costs are left out."""
    if result is None:
        return {"protocol_error": (error or {}).get("message"), "grade": grade}
    return {
        "status": result.status.value,
        "answer": result.answer,
        "data": result.data,
        "error": result.error,
        "final_url": result.final_url,
        "citations": [{"url": c.url, "quote": c.quote, "text": c.text} for c in result.citations],
        "steps": [
            {
                "operation": s.operation.value,
                "target": s.target,
                "outcome": s.outcome.value,
                "detail": s.note,
            }
            for s in result.steps
        ],
        "grade": grade,
    }


def rusage_bytes(maxrss: int) -> int:
    """`ru_maxrss` is bytes on macOS and kibibytes on Linux."""
    return maxrss if sys.platform == "darwin" else maxrss * 1024


async def bench_task(
    task: BenchTask,
    *,
    agent_argv: list[str],
    proxy: Proxy,
    logs: Path,
    chrome: LocalChrome,
) -> dict[str, Any]:
    """One task end to end: site, Chrome, proxy, agent, grade."""
    with (
        fixture_site(task) as (base_url, grade),
        local_chrome(chrome) as connection,
        proxy as proxy_url,
        tempfile.TemporaryDirectory(prefix="fastbrowse-bench-") as cwd,
    ):
        params = run_params(task, base_url, connection.cdp_url, f"bench-{task.id}")
        secrets = dict(task.task.secrets) if isinstance(task.task, MockTask) else {}
        agent = AgentProcess(agent_argv, agent_environment(proxy_url), Path(cwd), logs / f"{task.id}.stderr.log")
        try:
            driven = await drive(agent, params, secrets, TASK_SECONDS)
        finally:
            exit_code, usage = await asyncio.to_thread(agent.reap, SHUTDOWN_SECONDS)
        result: RunResult | None = None
        failure: str | None
        extra: dict[str, Any] = {}
        if driven.result is not None:
            try:
                result = RunResult.model_validate(driven.result)
            except ValidationError as exc:
                driven.error = {"message": f"the run reply is not a RunResult: {exc.error_count()} problems"}
        if result is not None:
            try:
                failure, extra = grade(result)
            except Exception as exc:
                failure = f"check raised {type(exc).__name__}: {exc}"
        else:
            failure = f"no result: {(driven.error or {}).get('message')}"
    grade_record = {"passed": failure is None, "failure": failure} | extra
    entry = {
        "task": task.id,
        "suite": task.suite,
        "port": task.port,
        "exit_code": exit_code,
        "metrics": {
            "startup_s": None if driven.startup is None else round(driven.startup, 4),
            "wall_s": None if driven.wall is None else round(driven.wall, 4),
            "cpu_user_s": round(usage.ru_utime, 4),
            "cpu_sys_s": round(usage.ru_stime, 4),
            "cpu_s": round(usage.ru_utime + usage.ru_stime, 4),
            "rss_bytes": rusage_bytes(usage.ru_maxrss),
            "steps": None if result is None else len(result.steps),
            "events": driven.events,
            "model_requests": proxy.requests,
        },
        "cost_usd": None if result is None else round(result.cost.known_dollars, 6),
        "result": normalized(result, driven.error, grade_record)
        | {"misses": sorted(f"{m.purpose} {m.model}" for m in proxy.misses)},
        "misses": [m.to_json() for m in proxy.misses],
        "forwarded": proxy.forwarded,
        "unused_responses": proxy.unused(),
    }
    return entry


def _chrome_version(chrome: LocalChrome) -> str | None:
    binary = find_chrome(chrome.binary)
    if binary is None:
        return None
    try:
        return subprocess.run([binary, "--version"], capture_output=True, text=True, timeout=10).stdout.strip()
    except (OSError, subprocess.TimeoutExpired):
        return None


def _meta(mode: str, agent: str, latency: Latency | None, chrome: LocalChrome) -> dict[str, Any]:
    return {
        "mode": mode,
        "agent": agent,
        "latency": None if latency is None else latency.value,
        "started": datetime.now(UTC).isoformat(timespec="seconds"),
        "harness_fastbrowse": version("fastbrowse"),
        "platform": f"{platform.system()} {platform.release()} {platform.machine()}",
        "cpu_count": os.cpu_count(),
        "python": platform.python_version(),
        "chrome": _chrome_version(chrome),
    }


def _write(path: Path, run: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(run, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    temporary.replace(path)


def _select(only: list[str], port_base: int) -> list[BenchTask]:
    tasks = corpus(port_base)
    if only:
        unknown = sorted(set(only) - {t.id for t in tasks})
        if unknown:
            raise SystemExit(f"--only names no task: {', '.join(unknown)}")
        tasks = [t for t in tasks if t.id in only]
    return tasks


def _cassette_path(directory: Path, task: str) -> Path:
    return directory / f"{task}.jsonl.gz"


async def run_suite(args: argparse.Namespace) -> int:
    """`record`, `fill` or `replay` every selected task, writing the run after each one."""
    mode: str = args.command
    forward = None
    if mode in ("record", "fill"):
        key = os.environ.get("OPENROUTER_API_KEY")
        if not key:
            raise SystemExit(f"{mode} needs OPENROUTER_API_KEY in the environment; only the proxy is given it")
        forward = openrouter_forward(key)
    latency = Latency.ZERO if mode == "record" else args.latency
    chrome = LocalChrome()
    tasks = _select(args.only, args.port_base)
    run: dict[str, Any] = {"meta": _meta(mode, args.agent, None if mode == "record" else latency, chrome), "tasks": []}
    logs = args.out.with_suffix("") / "logs"
    logs.mkdir(parents=True, exist_ok=True)
    spent = 0.0
    for task in tasks:
        path = _cassette_path(args.cassettes, task.id)
        if forward is not None and spent >= args.budget:
            print(f"budget: ${spent:.4f} of ${args.budget:.2f} spent; stopping before {task.id}", flush=True)
            break
        if mode == "record":
            cassette = Cassette(
                task=task.id,
                meta={
                    "recorded_at": datetime.now(UTC).date().isoformat(),
                    "agent_fastbrowse": version("fastbrowse"),
                    "models": {"llm": DEFAULT_LLM, "llm_field_text": FIELD_TEXT_LLM, "jev": "openrouter"},
                    "port": task.port,
                },
            )
        elif path.exists():
            cassette = Cassette.load(path)
        else:
            print(f"SKIP  {task.id}: no cassette at {path}", flush=True)
            continue
        before = len(cassette.exchanges)
        # A fill pass serves what it has at once and sends each miss upstream, where it takes real time, so the
        # run can branch again after it. Each pass extends the instant path, until one sends nothing upstream.
        passes = args.passes if mode == "fill" else 1
        for done in range(1, passes + 1):
            proxy = Proxy(cassette, forward=forward, replay=mode != "record", latency=latency)
            entry = await bench_task(task, agent_argv=shlex.split(args.agent), proxy=proxy, logs=logs, chrome=chrome)
            if forward is not None and (mode == "record" or proxy.forwarded):
                cassette.save(path)
            if mode == "fill":
                entry["fill_passes"] = done
                if not proxy.forwarded:
                    break
        cost = sum(exchange.cost or 0.0 for exchange in cassette.exchanges[before:])
        spent += cost
        if forward is not None:
            entry["recorded_cost_usd"] = round(cost, 6)
        run["tasks"].append(entry)
        _write(args.out, run)
        if entry["misses"]:
            with (logs / f"{task.id}.misses.txt").open("w", encoding="utf-8") as out:
                for miss in entry["misses"]:
                    out.write(f"## {miss['purpose']} ({miss['model']}) {miss['path']}\n{miss['diff']}\n\n")
        extra = f"{len(entry['misses'])} misses"
        if forward is not None:
            extra += f", {entry['forwarded']} forwarded, ${cost:.4f} (total ${spent:.4f})"
        _print_row(entry, extra=extra)
    print(f"{mode}: {len(run['tasks'])} tasks, ${spent:.4f} spent; run written to {args.out}")
    return 0


def _print_row(entry: dict[str, Any], extra: str = "") -> None:
    metrics = entry["metrics"]
    mark = "PASS" if entry["result"]["grade"]["passed"] else "FAIL"
    wall = metrics["wall_s"]
    print(
        f"{mark}  {entry['task']:30} {entry['result'].get('status', 'no-result'):20} "
        f"wall {wall if wall is not None else '-':>8}s cpu {metrics['cpu_s']:>7}s "
        f"rss {metrics['rss_bytes'] / 2**20:6.1f} MiB steps {metrics['steps']} {extra}",
        entry["result"]["grade"]["failure"] or "",
        flush=True,
    )


# Comparison


def load_run(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def compare_runs(a: dict[str, Any], b: dict[str, Any]) -> dict[str, list[str]]:
    """Per task, the unified diff of the normalized results; a task whose results agree is absent."""
    left = {entry["task"]: entry["result"] for entry in a["tasks"]}
    right = {entry["task"]: entry["result"] for entry in b["tasks"]}
    differences: dict[str, list[str]] = {}
    for task in sorted(left.keys() | right.keys()):
        if task not in left or task not in right:
            differences[task] = [f"only in {'A' if task in left else 'B'}"]
            continue
        if left[task] == right[task]:
            continue
        before = json.dumps(left[task], indent=1, sort_keys=True, ensure_ascii=False).splitlines()
        after = json.dumps(right[task], indent=1, sort_keys=True, ensure_ascii=False).splitlines()
        differences[task] = list(difflib.unified_diff(before, after, "A", "B", n=1, lineterm=""))
    return differences


def compare(args: argparse.Namespace) -> int:
    a, b = load_run(args.a), load_run(args.b)
    differences = compare_runs(a, b)
    costs = {entry["task"]: entry.get("cost_usd") for entry in a["tasks"]}
    for entry in b["tasks"]:
        if costs.get(entry["task"]) != entry.get("cost_usd"):
            print(f"cost  {entry['task']}: {costs.get(entry['task'])} -> {entry.get('cost_usd')}")
    for task, lines in differences.items():
        note = TIMING_SENSITIVE.get(task)
        print(f"DIFF  {task}" + (f" (timing-sensitive: {note})" if note else ""))
        print("\n".join(lines))
    total = len({e["task"] for e in a["tasks"]} | {e["task"] for e in b["tasks"]})
    print(f"{total - len(differences)}/{total} tasks identical")
    return 1 if differences else 0


# Report


def _median(values: list[float]) -> float | None:
    return statistics.median(values) if values else None


def _directory_bytes(path: Path) -> int:
    if path.is_file():
        return path.stat().st_size
    return sum(p.stat().st_size for p in path.rglob("*") if p.is_file() and not p.is_symlink())


def _runtime_closure(name: str) -> list[str]:
    """The installed distributions `name` needs at runtime, extras left out, itself included."""
    seen: list[str] = []
    pending = [name]
    while pending:
        current = pending.pop()
        try:
            dist = distribution(current)
        except PackageNotFoundError:
            continue
        canonical = dist.metadata["Name"].lower().replace("_", "-")
        if canonical in seen:
            continue
        seen.append(canonical)
        for spec in dist.requires or []:
            requirement = Requirement(spec)
            if requirement.marker is not None and not requirement.marker.evaluate({"extra": ""}):
                continue
            pending.append(requirement.name)
    return seen


def python_footprint() -> dict[str, Any]:
    """Installed bytes of fastbrowse and its runtime dependencies in this environment, and the frozen binary's
    directory when `pyinstaller packaging/fastbrowse.spec` has built one."""
    packages: dict[str, int] = {}
    for name in _runtime_closure("fastbrowse"):
        dist = distribution(name)
        if name == "fastbrowse":
            source = ROOT / "src" / "fastbrowse"
            packages[name] = sum(
                p.stat().st_size for p in source.rglob("*") if p.is_file() and "__pycache__" not in p.parts
            )
            continue
        total = 0
        for file in dist.files or []:
            located = Path(str(dist.locate_file(file)))
            if located.is_file() and "__pycache__" not in located.parts:
                total += located.stat().st_size
        packages[name] = total
    frozen = ROOT / "dist" / "fastbrowse"
    return {
        "site_packages_bytes": sum(packages.values()),
        "packages": dict(sorted(packages.items(), key=lambda item: -item[1])),
        "pyinstaller_bytes": _directory_bytes(frozen) if frozen.exists() else None,
        "npm_darwin_arm64_unpacked_bytes": 52_900_000,
    }


def _stats(runs: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    """Per task, the median of each metric across runs and whether every run passed."""
    by_task: dict[str, list[dict[str, Any]]] = {}
    for run in runs:
        for entry in run["tasks"]:
            by_task.setdefault(entry["task"], []).append(entry)
    stats: dict[str, dict[str, Any]] = {}
    for task, entries in by_task.items():
        row: dict[str, Any] = {"runs": len(entries), "passed": sum(e["result"]["grade"]["passed"] for e in entries)}
        for metric in ("wall_s", "cpu_s", "startup_s", "rss_bytes", "steps"):
            values = [e["metrics"][metric] for e in entries if e["metrics"][metric] is not None]
            row[metric] = _median(values)
        row["misses"] = sum(len(e["misses"]) for e in entries)
        stats[task] = row
    return stats


def _aggregate(stats: dict[str, dict[str, Any]]) -> dict[str, Any]:
    out: dict[str, Any] = {"tasks": len(stats)}
    for metric in ("wall_s", "cpu_s", "startup_s", "rss_bytes"):
        values = [row[metric] for row in stats.values() if row[metric] is not None]
        out[f"{metric}_median"] = _median(values)
        out[f"{metric}_total"] = sum(values) if metric != "rss_bytes" else None
        out[f"{metric}_max"] = max(values) if values else None
    out["passed"] = sum(row["passed"] for row in stats.values())
    out["runs"] = sum(row["runs"] for row in stats.values())
    return out


def _stability(runs: list[dict[str, Any]]) -> dict[str, Any]:
    """Every run against the first: tasks whose normalized results differ, and replay misses."""
    pairs = []
    for index, run in enumerate(runs[1:], start=1):
        differences = compare_runs(runs[0], run)
        pairs.append({"against": index, "differing_tasks": sorted(differences)})
    return {
        "runs": len(runs),
        "pairs": pairs,
        "misses": sum(len(e["misses"]) for run in runs for e in run["tasks"]),
        "identical": all(not pair["differing_tasks"] for pair in pairs),
    }


def _fmt(value: float | None, digits: int = 2) -> str:
    return "-" if value is None else f"{value:.{digits}f}"


def _mib(value: float | None) -> str:
    return "-" if value is None else f"{value / 2**20:.1f}"


def markdown(summary: dict[str, Any]) -> str:
    lines = [f"# Benchmark: {summary['agent']}", ""]
    for name, group in summary["groups"].items():
        agg = group["aggregate"]
        lines += [
            f"## {name} ({group['stability']['runs']} runs)",
            "",
            f"Passed {agg['passed']}/{agg['runs']} task runs. Stability: "
            + (
                "every run identical to the first"
                if group["stability"]["identical"]
                else "differing tasks "
                + "; ".join(
                    f"run {p['against']}: {', '.join(p['differing_tasks']) or 'none'}"
                    for p in group["stability"]["pairs"]
                )
            )
            + f", {group['stability']['misses']} replay misses. Timing-sensitive: "
            + "; ".join(f"{task}, {why}" for task, why in TIMING_SENSITIVE.items())
            + ".",
            "",
            "| | median | total | max |",
            "|---|---:|---:|---:|",
            f"| wall s | {_fmt(agg['wall_s_median'])} | {_fmt(agg['wall_s_total'])} | {_fmt(agg['wall_s_max'])} |",
            f"| agent CPU s | {_fmt(agg['cpu_s_median'])} | {_fmt(agg['cpu_s_total'])} | {_fmt(agg['cpu_s_max'])} |",
            f"| startup s | {_fmt(agg['startup_s_median'], 3)} | {_fmt(agg['startup_s_total'])} | "
            f"{_fmt(agg['startup_s_max'], 3)} |",
            f"| peak RSS MiB | {_mib(agg['rss_bytes_median'])} | - | {_mib(agg['rss_bytes_max'])} |",
            "",
            "| task | pass | wall s | CPU s | startup s | RSS MiB | steps | misses |",
            "|---|---:|---:|---:|---:|---:|---:|---:|",
        ]
        for task, row in group["tasks"].items():
            lines.append(
                f"| {task} | {row['passed']}/{row['runs']} | {_fmt(row['wall_s'])} | {_fmt(row['cpu_s'])} | "
                f"{_fmt(row['startup_s'], 3)} | {_mib(row['rss_bytes'])} | {_fmt(row['steps'], 0)} | {row['misses']} |"
            )
        lines.append("")
    footprint = summary.get("footprint")
    if footprint:
        lines += ["## Footprint", "", "| what | MiB |", "|---|---:|"]
        lines.append(f"| site-packages, fastbrowse and runtime deps | {_mib(footprint['site_packages_bytes'])} |")
        if footprint.get("pyinstaller_bytes"):
            lines.append(f"| PyInstaller directory | {_mib(footprint['pyinstaller_bytes'])} |")
        lines.append(f"| npm package, darwin-arm64, unpacked | {_mib(footprint['npm_darwin_arm64_unpacked_bytes'])} |")
        for path, size in footprint.get("installed", {}).items():
            lines.append(f"| {path} | {_mib(size)} |")
        lines.append("")
    return "\n".join(lines)


def report(args: argparse.Namespace) -> int:
    groups: dict[str, list[dict[str, Any]]] = {}
    for path in args.runs:
        run = load_run(path)
        label = f"{run['meta']['mode']}, latency {run['meta']['latency'] or 'live'}"
        groups.setdefault(label, []).append(run)
    agents = sorted({run["meta"]["agent"] for runs in groups.values() for run in runs})
    summary: dict[str, Any] = {"agent": ", ".join(agents), "groups": {}}
    for label, runs in groups.items():
        stats = _stats(runs)
        summary["groups"][label] = {
            "aggregate": _aggregate(stats),
            "stability": _stability(runs),
            "tasks": stats,
            "meta": [run["meta"] for run in runs],
        }
    if args.footprint:
        summary["footprint"] = python_footprint()
    if args.installed:
        summary.setdefault("footprint", {"site_packages_bytes": None, "npm_darwin_arm64_unpacked_bytes": 52_900_000})
        summary["footprint"]["installed"] = {str(p): _directory_bytes(p) for p in args.installed}
    text = markdown(summary)
    if args.markdown:
        args.markdown.parent.mkdir(parents=True, exist_ok=True)
        args.markdown.write_text(text, encoding="utf-8")
    if args.json:
        _write(args.json, summary)
    print(text)
    return 0


def _stamp() -> str:
    return datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(prog="python -m fastbrowse.evals.bench", description=__doc__.split("\n\n")[0])
    commands = root.add_subparsers(dest="command", required=True)

    def common(sub: argparse.ArgumentParser, mode: str) -> None:
        sub.add_argument("--agent", default=default_agent(), help="the command that starts `serve --stdio`")
        sub.add_argument("--only", nargs="*", default=[], help="task ids; every fixture task when absent")
        sub.add_argument("--cassettes", type=Path, default=CASSETTES)
        sub.add_argument("--out", type=Path, default=ARTIFACTS / f"{mode}-{_stamp()}.json")
        sub.add_argument("--port-base", type=int, default=PORT_BASE)

    rec = commands.add_parser("record", help="run every task against the real models and keep their responses")
    common(rec, "record")
    rec.add_argument("--budget", type=float, default=5.0, help="stop before the next task once this many dollars went")
    fill = commands.add_parser("fill", help="replay, sending only the misses to the real models and keeping them")
    common(fill, "fill")
    fill.add_argument("--budget", type=float, default=5.0, help="stop before the next task once this many dollars went")
    fill.add_argument("--latency", type=Latency, choices=list(Latency), default=Latency.ZERO)
    fill.add_argument("--passes", type=int, default=6, help="most passes per task before giving up on converging")
    rep = commands.add_parser("replay", help="run every task against the recorded responses")
    common(rep, "replay")
    rep.add_argument("--latency", type=Latency, choices=list(Latency), default=Latency.ZERO)
    cmp = commands.add_parser("compare", help="diff the normalized results of two runs")
    cmp.add_argument("a", type=Path)
    cmp.add_argument("b", type=Path)
    rpt = commands.add_parser("report", help="summarize runs as markdown and JSON")
    rpt.add_argument("runs", type=Path, nargs="+")
    rpt.add_argument("--markdown", type=Path)
    rpt.add_argument("--json", type=Path)
    rpt.add_argument("--footprint", action="store_true", help="measure this environment's Python install")
    rpt.add_argument("--installed", type=Path, nargs="*", default=[], help="files or directories to size")
    return root


def main(argv: list[str]) -> int:
    args = parser().parse_args(argv)
    match args.command:
        case "record" | "fill" | "replay":
            return asyncio.run(run_suite(args))
        case "compare":
            return compare(args)
        case "report":
            return report(args)
    raise AssertionError(args.command)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
