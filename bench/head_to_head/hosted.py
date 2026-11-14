#!/usr/bin/env python3
"""Browser Use's hosted agent on the head-to-head's tasks, one run at a time, in the same campaign files as run.py.

    python3 bench/head_to_head/hosted.py run --campaign NAME [--repeats 3] [--only TASK ...] [--arms ARM ...]
    python3 bench/head_to_head/hosted.py settle --campaign NAME

The agent and its browser run on Browser Use's cloud, so only the clock and the network path start on this machine.
README.md has what that changes. Needs BROWSER_USE_API_KEY. `settle` rereads each session's bill once it has settled,
which the docs put at up to about 90 seconds after a run. A failed run is kept as a result: nothing here retries one.
"""

# ruff: noqa: E501

import argparse
import json
import os
import shutil
import sys
import time
import urllib.error
import urllib.request
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).parent))
from run import RUN_SECONDS
from tasks import TASKS, Task, goal, grade

HERE = Path(__file__).parent
API = "https://api.browser-use.com/api/v4"
POLL_SECONDS = 0.25
FINISHED = ("completed", "failed", "cancelled")

ARMS = {"browser-use-cloud": None, "bu-ultrafast": "bu-ultrafast", "bu-fast": "bu-fast"}
"""Arm to the `model` field. `None` leaves it out, which is the API's default: GPT-6 Luna at high reasoning. The presets
take no `modelParams` or `mode`, so nothing else varies between the arms."""


def _call(method: str, path: str, body: dict[str, Any] | None = None, *, tries: int = 4) -> Any:
    request = urllib.request.Request(
        path if path.startswith("http") else API + path,
        method=method,
        data=None if body is None else json.dumps(body).encode(),
        headers={
            "X-Browser-Use-API-Key": os.environ["BROWSER_USE_API_KEY"],
            "Content-Type": "application/json",
            "User-Agent": "fastbrowse-head-to-head",
        },
    )
    for attempt in range(tries):
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            if (error.code < 500 and error.code != 429) or attempt == tries - 1:
                raise RuntimeError(f"{method} {path}: {error.code} {error.read().decode()[:300]}") from error
        except OSError:
            if attempt == tries - 1:
                raise
        time.sleep(1 + attempt)
    raise AssertionError("unreachable")


def _prompt(task: Task) -> str:
    """The shared prompt. The API's secret bindings need a vault, so the checkout's two public credentials, printed on
    the site's own sign-in page, are in the words."""
    _, prompt = goal(task)
    if task.secrets:
        prompt += f" Username: {task.secrets['username']}. Password: {task.secrets['password']}."
    return prompt


def _events(run_id: str) -> list[dict[str, Any]]:
    events: list[dict[str, Any]] = []
    after = 0
    while True:
        page = _call("GET", f"/runs/{run_id}/events?limit=200&after={after}&include_output=true")
        events += page["events"]
        if not page.get("hasMore"):
            return events
        after = page["nextAfter"]


def _seconds(start: str, end: str) -> float:
    return round((datetime.fromisoformat(end) - datetime.fromisoformat(start)).total_seconds(), 3)


def _read_events(events: list[dict[str, Any]]) -> dict[str, Any]:
    """Counts from the run's event stream. The API has no step field; these are the types the stream carries."""
    steps, tools, model_seconds, opened = 0, {}, 0.0, None
    ready = created = completed = None
    calls = 0
    for event in events:
        kind, data = event["type"], event["data"]
        if kind == "run.created":
            created = event["ts"]
        elif kind in ("browser.ready", "browser.reattached") and ready is None:
            ready = event["ts"]
        elif kind in ("run.completed", "run.failed", "run.cancelled"):
            completed = event["ts"]
        elif kind == "llm.request":
            opened = event["ts"]
        elif kind == "llm.response":
            calls += 1
            if opened:
                model_seconds += _seconds(opened, event["ts"])
                opened = None
        elif kind == "core.event":
            part = data.get("part") or {}
            if data.get("type") == "step_start":
                steps += 1
            elif part.get("type") == "tool":
                tools.setdefault(part.get("callID"), part.get("tool"))
    return {
        "steps": steps,
        "tool_calls": list(tools.values()),
        "model_calls": calls,
        "llm_calls": calls,
        "jev_calls": 0,
        "model_seconds": round(model_seconds, 3),
        "browser_ready_seconds": _seconds(created, ready) if created and ready else None,
        "server_seconds": _seconds(created, completed) if created and completed else None,
    }


def _browser_id(events: list[dict[str, Any]]) -> str | None:
    return next(
        (
            e["data"]["browser_session_id"]
            for e in events
            if e["type"] in ("browser.ready", "browser.released") and e["data"].get("browser_session_id")
        ),
        None,
    )


def _final_url(browser_id: str | None) -> str | None:
    """The page the browser is on, read over CDP before it stops: the same fact fastbrowse and the open-source arm report
    about their own browser, without trusting the agent's words."""
    if not browser_id:
        return None
    try:
        cdp = _call("GET", f"/browsers/{browser_id}").get("cdpUrl") or ""
        origin = cdp.replace("wss://", "https://").replace("ws://", "http://").split("/devtools")[0].rstrip("/")
        request = urllib.request.Request(f"{origin}/json/list", headers={"User-Agent": "fastbrowse-head-to-head"})
        with urllib.request.urlopen(request, timeout=15) as response:
            pages = [target["url"] for target in json.load(response) if target.get("type") == "page"]
        return pages[0] if pages else None
    except (OSError, RuntimeError, ValueError):
        return None


def one_run(task: Task, arm: str, repeat: int, raw: Path) -> dict[str, Any]:
    run_id = f"{task.id}.{arm}.{repeat}"
    run_dir = raw / run_id
    if run_dir.exists():
        shutil.rmtree(run_dir)
    run_dir.mkdir(parents=True)
    # No managed proxy on any hosted arm: the local arms have none, and the docs and schema disagree on the default.
    body: dict[str, Any] = {"task": _prompt(task), "browserSettings": {"proxyCountryCode": None}}
    if ARMS[arm]:
        body["model"] = ARMS[arm]
    started_at = datetime.now(UTC).isoformat(timespec="seconds")
    row: dict[str, Any] = {"ending": "no result", "ended_normally": False, "timed_out": False, "exit_code": None}
    started = time.monotonic()
    current: dict[str, Any] = {}
    try:
        created = _call("POST", "/runs", body)
        hosted_id = created["id"]
        row |= {"hosted_run_id": hosted_id, "session_id": created["sessionId"], "preset": ARMS[arm] or "default"}
        while True:
            current = _call("GET", f"/runs/{hosted_id}")
            if current["status"] in FINISHED:
                break
            if time.monotonic() - started > RUN_SECONDS:
                row |= {"timed_out": True, "ending": "killed"}
                _call("POST", f"/runs/{hosted_id}/cancel", {}, tries=1)
                break
            time.sleep(POLL_SECONDS)
        wall = time.monotonic() - started
        events = _events(hosted_id)
        (run_dir / "run.json").write_text(json.dumps(current, indent=1), encoding="utf-8")
        (run_dir / "events.json").write_text(json.dumps(events), encoding="utf-8")
        browser_id = _browser_id(events)
        row |= _read_events(events) | {
            "wall_seconds": round(wall, 3),
            "final_url": _final_url(browser_id),
            "browser_id": browser_id,
        }
        if browser_id:
            _call("PATCH", f"/browsers/{browser_id}", {"action": "stop"})
        if not row["timed_out"]:
            row |= {"ending": current["status"], "ended_normally": current["status"] == "completed"}
        row |= {
            "model": current.get("model"),
            "answer": current.get("result"),
            "error": current.get("error"),
            "input_tokens": current.get("totalInputTokens"),
            "output_tokens": current.get("totalOutputTokens"),
            "llm_input_tokens": current.get("totalInputTokens"),
            "llm_output_tokens": current.get("totalOutputTokens"),
            "dollars": float(current["totalCostUsd"]) if current.get("totalCostUsd") else None,
            "unknown_cost": not current.get("totalCostUsd"),
        }
    except (OSError, RuntimeError, ValueError, KeyError) as error:
        row |= {"error": f"{type(error).__name__}: {error}"[:500], "wall_seconds": round(time.monotonic() - started, 3)}
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
        "load_1m_before": None,
        "load_1m_after": None,
        "cpu_seconds": None,
        "peak_rss_mib": None,
        "passed": failure is None,
        "failure": failure,
        **row,
    }


def campaign(name: str, repeats: int, only: list[str] | None, arms: list[str], budget: float) -> None:
    out = HERE / "results" / name
    (out / "raw").mkdir(parents=True, exist_ok=True)
    runs = out / "runs.jsonl"
    existing = [json.loads(line) for line in runs.read_text().splitlines()] if runs.exists() else []
    done = {row["run_id"] for row in existing}
    spent = sum(row.get("dollars") or 0 for row in existing if row["arm"] in ARMS)
    tasks = [task for task in TASKS if not only or task.id in only]
    for repeat in range(1, repeats + 1):
        for index, task in enumerate(tasks):
            shift = (index + repeat) % len(arms)
            for arm in arms[shift:] + arms[:shift]:
                if f"{task.id}.{arm}.{repeat}" in done:
                    continue
                if spent >= budget:
                    raise SystemExit(f"stopped at the ${budget:.2f} budget, with ${spent:.2f} spent")
                row = one_run(task, arm, repeat, out / "raw")
                spent += row.get("dollars") or 0
                with runs.open("a", encoding="utf-8") as file:
                    file.write(json.dumps(row) + "\n")
                print(
                    f"{row['started_at']} {row['run_id']}: {'PASS' if row['passed'] else 'FAIL'} {row['wall_seconds']}s model={row.get('model_seconds')}s steps={row.get('steps')} ${row.get('dollars')} {row['failure'] or ''}",
                    flush=True,
                )


def settle(name: str) -> None:
    """Replace each hosted run's cost with its session's settled bill, split so model charges stay comparable."""
    runs = HERE / "results" / name / "runs.jsonl"
    rows = [json.loads(line) for line in runs.read_text().splitlines()]
    for row in rows:
        if row["arm"] in ARMS and row.get("session_id"):
            bill = _call("GET", f"/sessions/{row['session_id']}/cost")
            row |= {
                "dollars": round(float(bill["llmCostUsd"]) + float(bill["searchCostUsd"]), 6),
                "unknown_cost": False,
                "browser_dollars": float(bill["browserCostUsd"]),
                "proxy_dollars": float(bill["proxyCostUsd"]),
                "session_dollars": float(bill["totalCostUsd"]),
            }
    runs.write_text("".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser("run")
    run.add_argument("--campaign", required=True)
    run.add_argument("--repeats", type=int, default=3)
    run.add_argument("--only", nargs="*")
    run.add_argument("--arms", nargs="*", choices=list(ARMS), default=list(ARMS))
    run.add_argument(
        "--budget", type=float, default=15.0, help="stop before a new run once this many dollars are spent"
    )
    closing = commands.add_parser("settle")
    closing.add_argument("--campaign", required=True)
    args = parser.parse_args()
    if not os.environ.get("BROWSER_USE_API_KEY"):
        raise SystemExit("BROWSER_USE_API_KEY is not set")
    if args.command == "run":
        campaign(args.campaign, args.repeats, args.only, args.arms, args.budget)
    else:
        settle(args.campaign)


if __name__ == "__main__":
    main()
