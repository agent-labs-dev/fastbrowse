#!/usr/bin/env python3
"""Run fastbrowse and Browser Use on the same tasks, one run at a time, on this machine. README.md has the method.

    python3 bench/head_to_head/run.py prepare
    python3 bench/head_to_head/run.py run --campaign NAME [--repeats 3] [--only TASK ...] [--arms ARM ...]
    python3 bench/head_to_head/report.py NAME

Standard library only. A finished run is one line of `runs.jsonl`, and a rerun of a campaign skips what it has, so a
campaign survives being stopped. A failed run is kept as a result: nothing here retries one.
"""

# The table rows and page markup read better whole than wrapped.
# ruff: noqa: E501

import argparse
import contextlib
import json
import os
import platform
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.request
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).parent))
from tasks import TASKS, Task, goal, grade, verify_truth

HERE = Path(__file__).parent
ROOT = HERE.parents[1]
ENV = Path(os.environ.get("HEAD_TO_HEAD_ENV", Path.home() / ".cache" / "fastbrowse-head-to-head"))
FASTBROWSE_PYTHON_DIR = ENV / "fastbrowse"
BROWSER_USE_DIR = ENV / "browser-use"
BUILD_RECORD = ENV / "fastbrowse-build.json"

BROWSER_USE_PIN = "browser-use==0.13.14"
MODEL = "google/gemini-3.8-flash"
REASONING = "low"
"""Both arms read and plan with this model at this effort, through OpenRouter. fastbrowse also calls Jev, its own
choice model, through the same key; that is the product, not a setting."""

RUN_SECONDS = 900
"""The one limit: a run still going after this is killed and counted as a timeout. Same for both arms."""

ARMS = ("fastbrowse", "browser-use", "browser-use-default-effort")
DEFAULT_ARMS = ("fastbrowse", "browser-use")
"""`browser-use-default-effort` is the same arm without the reasoning field, at the provider's default effort. It is a
sensitivity check and runs only when named."""


def _output(*command: str, cwd: Path | None = None) -> str:
    return subprocess.run(command, capture_output=True, text=True, check=True, cwd=cwd).stdout.strip()


def _chrome_binary() -> str:
    for name in (os.environ.get("FASTBROWSE_CHROME"), "google-chrome-stable", "google-chrome", "chromium"):
        if name and (found := shutil.which(name)):
            return found
    raise SystemExit("Chrome is not installed")


def prepare() -> None:
    """Build fastbrowse with the release profile from this checkout and install the pinned Browser Use, each apart."""
    ENV.mkdir(parents=True, exist_ok=True)
    commit = _output("git", "rev-parse", "HEAD", cwd=ROOT)
    with tempfile.TemporaryDirectory() as wheels:
        subprocess.run(["uv", "venv", "-q", "--clear", "--python", "3.13", str(FASTBROWSE_PYTHON_DIR)], check=True)
        python = str(FASTBROWSE_PYTHON_DIR / "bin" / "python")
        subprocess.run(
            ["uvx", "--from", "maturin>=1.16,<2", "maturin", "build", "--release", "-i", python, "-o", wheels],
            cwd=ROOT,
            check=True,
        )
        wheel = next(Path(wheels).glob("*.whl"))
        subprocess.run(["uv", "pip", "install", "-q", "--python", python, str(wheel)], check=True)
        BUILD_RECORD.write_text(json.dumps({"commit": commit, "profile": "release", "wheel": wheel.name}))
    subprocess.run(["uv", "venv", "-q", "--clear", "--python", "3.13", str(BROWSER_USE_DIR)], check=True)
    subprocess.run(
        ["uv", "pip", "install", "-q", "--python", str(BROWSER_USE_DIR / "bin" / "python"), BROWSER_USE_PIN], check=True
    )
    print(json.dumps(environment_record(), indent=2))


def environment_record() -> dict[str, Any]:
    build = json.loads(BUILD_RECORD.read_text())
    version = 'import importlib.metadata as m; print(m.version("%s"))'
    cpu = next(
        line.split(":")[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines() if "model name" in line
    )
    memory = next(
        line.split()[1] for line in Path("/proc/meminfo").read_text().splitlines() if line.startswith("MemTotal")
    )
    return {
        "machine": {
            "name": os.environ.get("HEAD_TO_HEAD_MACHINE", platform.node()),
            "hostname": platform.node(),
            "cpu": cpu,
            "vcpus": os.cpu_count(),
            "memory_gib": round(int(memory) / 1024**2, 1),
            "kernel": platform.release(),
        },
        "chrome": _output(_chrome_binary(), "--version"),
        "fastbrowse": {
            "version": _output(str(FASTBROWSE_PYTHON_DIR / "bin" / "python"), "-c", version % "fastbrowse"),
            **build,
        },
        "browser_use": {"version": _output(str(BROWSER_USE_DIR / "bin" / "python"), "-c", version % "browser-use")},
        "python": _output(str(FASTBROWSE_PYTHON_DIR / "bin" / "python"), "--version"),
        "model": MODEL,
        "reasoning": REASONING,
        "run_seconds_limit": RUN_SECONDS,
    }


class Chrome:
    """A fresh headless Chrome on a throwaway profile, the same flags for every run; the harness owns and kills it."""

    def __enter__(self) -> str:
        self.profile = tempfile.mkdtemp(prefix="h2h-chrome-")
        preferences = Path(self.profile, "Default", "Preferences")
        preferences.parent.mkdir(parents=True)
        preferences.write_text(
            json.dumps(
                {
                    "credentials_enable_service": False,
                    "profile": {"password_manager_enabled": False, "password_manager_leak_detection": False},
                }
            )
        )
        self.process = subprocess.Popen(
            [
                _chrome_binary(),
                "--headless=new",
                "--window-size=1280,900",
                "--remote-debugging-port=0",
                f"--user-data-dir={self.profile}",
                "--no-first-run",
                "--disable-popup-blocking",
                "about:blank",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        port_file = Path(self.profile, "DevToolsActivePort")
        for _ in range(300):
            if port_file.exists() and len(lines := port_file.read_text().split()) >= 2:
                with urllib.request.urlopen(f"http://127.0.0.1:{lines[0]}/json/version", timeout=5) as response:
                    return json.load(response)["webSocketDebuggerUrl"]
            time.sleep(0.1)
        raise RuntimeError("Chrome did not start")

    def __exit__(self, *_: object) -> None:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(self.process.pid, signal.SIGKILL)
        self.process.wait()
        shutil.rmtree(self.profile, ignore_errors=True)


def _clean_env(run_dir: Path) -> dict[str, str]:
    """Only what a run needs. No other provider's key, no FASTBROWSE_ setting, and a HOME of its own, so neither arm
    takes a route, a cache or a config from this machine's account or from an earlier run."""
    home = run_dir / "home"
    home.mkdir()
    return {
        "PATH": os.environ["PATH"],
        "HOME": str(home),
        "LANG": "C.UTF-8",
        "OPENROUTER_API_KEY": os.environ["OPENROUTER_API_KEY"],
    }


def _spawn(command: list[str], env: dict[str, str], run_dir: Path, stdin: str | None = None) -> dict[str, Any]:
    """Run to exit or to the limit; wall time is spawn to exit, CPU and peak memory the process's own, from wait4."""
    with open(run_dir / "stdout.txt", "wb") as out, open(run_dir / "stderr.txt", "wb") as err:
        started = time.monotonic()
        process = subprocess.Popen(
            command, stdin=subprocess.PIPE, stdout=out, stderr=err, env=env, cwd=run_dir, start_new_session=True
        )
        if process.stdin:
            process.stdin.write((stdin or "").encode())
            process.stdin.close()
        timed_out = False
        while True:
            pid, status, usage = os.wait4(process.pid, os.WNOHANG)
            if pid:
                break
            if time.monotonic() - started > RUN_SECONDS:
                timed_out = True
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(process.pid, signal.SIGKILL)
                pid, status, usage = os.wait4(process.pid, 0)
                break
            time.sleep(0.02)
        wall = time.monotonic() - started
        process.returncode = 0
    return {
        "wall_seconds": round(wall, 3),
        "exit_code": os.waitstatus_to_exitcode(status),
        "timed_out": timed_out,
        "cpu_seconds": round(usage.ru_utime + usage.ru_stime, 3),
        "peak_rss_mib": round(usage.ru_maxrss / 1024, 1),
    }


def run_fastbrowse(task: Task, run_dir: Path, cdp_url: str) -> dict[str, Any]:
    start, prompt = goal(task)
    env = _clean_env(run_dir) | {"FASTBROWSE_LLM_MODEL": MODEL, "FASTBROWSE_LLM_REASONING": REASONING}
    command = [
        str(FASTBROWSE_PYTHON_DIR / "bin" / "fastbrowse"),
        prompt,
        "--start",
        start,
        "--cdp-url",
        cdp_url,
        "--json",
        "--trace",
        str(run_dir / "trace.jsonl"),
    ]
    if task.authorize:
        command.append("--authorize")
    for name, value in task.secrets.items():
        env[f"H2H_{name.upper()}"] = value
        command += ["--secret", f"{name}=H2H_{name.upper()}"]
    process = _spawn(command, env, run_dir)
    row: dict[str, Any] = {**process, "ending": "killed" if process["timed_out"] else "no result"}
    with contextlib.suppress(json.JSONDecodeError):
        result = json.loads((run_dir / "stdout.txt").read_text())
        lines = result["cost"]["lines"]
        row |= {
            "ending": result["status"],
            "ended_normally": result["status"] == "complete",
            "answer": result.get("answer"),
            "final_url": result.get("final_url"),
            "error": result.get("error"),
            "steps": len(result["steps"]),
            "tool_calls": [step["operation"] for step in result["steps"]],
            "model_calls": sum(
                line.get("physical_requests") or 1 for line in lines if line["component"] in ("jev", "llm")
            ),
            "llm_calls": sum(line.get("physical_requests") or 1 for line in lines if line["component"] == "llm"),
            "jev_calls": sum(line.get("physical_requests") or 1 for line in lines if line["component"] == "jev"),
            "input_tokens": sum(line["input_tokens"] for line in lines),
            "output_tokens": sum(line["output_tokens"] for line in lines),
            "llm_input_tokens": sum(line["input_tokens"] for line in lines if line["component"] == "llm"),
            "llm_output_tokens": sum(line["output_tokens"] for line in lines if line["component"] == "llm"),
            "dollars": round(sum(line["dollars"] or 0 for line in lines), 6),
            "unknown_cost": any(line["dollars"] is None for line in lines),
            "dollars_by_component": {
                c: round(sum(line["dollars"] or 0 for line in lines if line["component"] == c), 6)
                for c in {line["component"] for line in lines}
            },
        }
    return row


def run_browser_use(task: Task, run_dir: Path, cdp_url: str, *, reasoning: str | None) -> dict[str, Any]:
    _, prompt = goal(task)
    env = _clean_env(run_dir) | {
        "ANONYMIZED_TELEMETRY": "false",
        "BROWSER_USE_CLOUD_SYNC": "false",
        "BROWSER_USE_CONFIG_DIR": str(run_dir / "home" / "browseruse"),
    }
    meter, result_file = run_dir / "meter.jsonl", run_dir / "result.json"
    request = {
        "goal": prompt,
        "model": MODEL,
        "reasoning": reasoning,
        "cdp_url": cdp_url,
        "secrets": task.secrets,
        "meter": str(meter),
        "result": str(result_file),
    }
    process = _spawn(
        [str(BROWSER_USE_DIR / "bin" / "python"), str(HERE / "browser_use_arm.py")], env, run_dir, json.dumps(request)
    )
    row: dict[str, Any] = {**process, "ending": "killed" if process["timed_out"] else "no result"}
    calls = [json.loads(line) for line in meter.read_text().splitlines()] if meter.exists() else []
    usages = [call["usage"] for call in calls if call["usage"]]
    row |= {
        "model_calls": len(calls),
        "llm_calls": len(calls),
        "jev_calls": 0,
        "failed_model_calls": sum(call["status"] >= 400 for call in calls),
        "input_tokens": sum(usage.get("prompt_tokens") or 0 for usage in usages),
        "output_tokens": sum(usage.get("completion_tokens") or 0 for usage in usages),
        "llm_input_tokens": sum(usage.get("prompt_tokens") or 0 for usage in usages),
        "llm_output_tokens": sum(usage.get("completion_tokens") or 0 for usage in usages),
        "reasoning_tokens": sum(
            (usage.get("completion_tokens_details") or {}).get("reasoning_tokens") or 0 for usage in usages
        ),
        "dollars": round(sum(usage.get("cost") or 0 for usage in usages), 6),
        "unknown_cost": any(usage.get("cost") is None for usage in usages),
    }
    if result_file.exists():
        result = json.loads(result_file.read_text())
        if result.get("crash"):
            row |= {"ending": "crash", "error": result["crash"]}
        else:
            row |= {
                "ending": "done"
                if result["done"] and result["successful"]
                else "done, reported unsuccessful"
                if result["done"]
                else "stopped",
                "ended_normally": bool(result["done"] and result["successful"]),
                "answer": result["answer"],
                "final_url": result["final_url"],
                "steps": result["steps"],
                "tool_calls": result["tool_calls"],
                "step_errors": result["errors"],
            }
    return row


def one_run(task: Task, arm: str, repeat: int, raw: Path) -> dict[str, Any]:
    run_id = f"{task.id}.{arm}.{repeat}"
    run_dir = raw / run_id
    if run_dir.exists():
        shutil.rmtree(run_dir)
    run_dir.mkdir(parents=True)
    load_before = os.getloadavg()[0]
    started_at = datetime.now(UTC).isoformat(timespec="seconds")
    with Chrome() as cdp_url:
        if arm == "fastbrowse":
            row = run_fastbrowse(task, run_dir, cdp_url)
        else:
            row = run_browser_use(
                task, run_dir, cdp_url, reasoning=None if arm == "browser-use-default-effort" else REASONING
            )
    shutil.rmtree(run_dir / "home", ignore_errors=True)
    failure = grade(
        task, ended=row.get("ended_normally", False), answer=row.get("answer"), final_url=row.get("final_url")
    )
    return {
        "run_id": run_id,
        "task": task.id,
        "length": task.length,
        "arm": arm,
        "repeat": repeat,
        "started_at": started_at,
        "load_1m_before": round(load_before, 2),
        "load_1m_after": round(os.getloadavg()[0], 2),
        "passed": failure is None,
        "failure": failure,
        **row,
    }


def campaign(name: str, repeats: int, only: list[str] | None, arms: list[str]) -> None:
    out = HERE / "results" / name
    (out / "raw").mkdir(parents=True, exist_ok=True)
    runs = out / "runs.jsonl"
    done = {json.loads(line)["run_id"] for line in runs.read_text().splitlines()} if runs.exists() else set()
    record = out / "environment.json"
    if not record.exists():
        record.write_text(json.dumps(environment_record() | {"truth_checked": verify_truth()}, indent=2) + "\n")
    tasks = [task for task in TASKS if not only or task.id in only]
    for repeat in range(1, repeats + 1):
        for index, task in enumerate(tasks):
            # The order swaps from task to task and from repeat to repeat, so neither arm always goes first.
            order = arms if (index + repeat) % 2 else list(reversed(arms))
            for arm in order:
                if f"{task.id}.{arm}.{repeat}" in done:
                    continue
                row = one_run(task, arm, repeat, out / "raw")
                with runs.open("a", encoding="utf-8") as file:
                    file.write(json.dumps(row) + "\n")
                print(
                    f"{row['started_at']} {row['run_id']}: {'PASS' if row['passed'] else 'FAIL'} {row['wall_seconds']}s steps={row.get('steps')} ${row.get('dollars')} {row['failure'] or ''}",
                    flush=True,
                )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("prepare")
    run = commands.add_parser("run")
    run.add_argument("--campaign", required=True)
    run.add_argument("--repeats", type=int, default=3)
    run.add_argument("--only", nargs="*")
    run.add_argument("--arms", nargs="*", choices=ARMS, default=list(DEFAULT_ARMS))
    args = parser.parse_args()
    if args.command == "prepare":
        prepare()
    else:
        if not os.environ.get("OPENROUTER_API_KEY"):
            raise SystemExit("OPENROUTER_API_KEY is not set")
        campaign(args.campaign, args.repeats, args.only, args.arms)


if __name__ == "__main__":
    main()
