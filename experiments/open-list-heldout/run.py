# ruff: noqa: E501
"""Run the held-out tasks on the named builds, alternating builds so neither gets a time-of-day advantage.

usage: run.py OUT_DIR REPEATS LIMIT_DOLLARS CONCURRENCY build=/path/to/venv [build=/path/to/venv ...]

Needs OPENROUTER_API_KEY and BROWSER_USE_API_KEY in the environment. Stops starting runs once the tracked spend plus
the reservation for runs in flight reaches the limit. A finished row is never rerun, so a resumed campaign keeps its
ledger.
"""

import json
import pathlib
import subprocess
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor

HERE = pathlib.Path(__file__).parent
RESERVE = 0.60  # held for each run in flight and charged to one that is killed without a report
WATCHDOG = 1200

out, repeats, limit, concurrency = pathlib.Path(sys.argv[1]), int(sys.argv[2]), float(sys.argv[3]), int(sys.argv[4])
builds = dict(arg.split("=", 1) for arg in sys.argv[5:])
tasks = json.loads((HERE / "tasks.json").read_text())
out.mkdir(parents=True, exist_ok=True)
ledger = out / "runs.jsonl"
lock = threading.Lock()
rows = [json.loads(line) for line in ledger.read_text().splitlines()] if ledger.exists() else []
done = {(r["build"], r["task"], r["repeat"]) for r in rows}
spent = sum(r["dollars"] for r in rows)
flying = 0


def dollars(report: dict | None) -> tuple[float, float]:
    """Model dollars and browser plus proxy dollars from a run's own cost lines."""
    if report is None:
        return RESERVE, 0.0
    lines = report["cost"]["lines"]
    model = sum(x["dollars"] or 0 for x in lines if x["component"] not in ("browser", "proxy"))
    browser = sum(x["dollars"] or 0 for x in lines if x["component"] in ("browser", "proxy"))
    return model, browser


def one(build: str, task: dict, repeat: int) -> None:
    global spent, flying
    with lock:
        if spent + (flying + 1) * RESERVE > limit:
            print(f"limit: ${spent:.3f} spent, skipping {build} {task['id']} {repeat}", flush=True)
            return
        flying += 1
    stem = out / f"{build}.{task['id']}.{repeat}"
    command = [
        f"{builds[build]}/bin/fastbrowse",
        task["task"],
        "--start",
        task["start"],
        "--cloud",
        "--json",
        "--trace",
        f"{stem}.trace",
    ]
    started = time.monotonic()
    report, code = None, -1
    try:
        done_run = subprocess.run(
            command, capture_output=True, text=True, timeout=WATCHDOG, stdin=subprocess.DEVNULL, cwd=out
        )
        code = done_run.returncode
        pathlib.Path(f"{stem}.json").write_text(done_run.stdout)
        pathlib.Path(f"{stem}.err").write_text(done_run.stderr)
        report = json.loads(done_run.stdout)
    except subprocess.TimeoutExpired:
        code = -2
    except json.JSONDecodeError:
        code = -3
    model, browser = dollars(report)
    row = {
        "build": build,
        "task": task["id"],
        "repeat": repeat,
        "exit": code,
        "status": (report or {}).get("status"),
        "answer": (report or {}).get("answer"),
        "steps": len((report or {}).get("steps") or []),
        "seconds": round(time.monotonic() - started, 1),
        "model_dollars": round(model, 5),
        "browser_dollars": round(browser, 5),
        "dollars": round(model + browser, 5),
    }
    with lock:
        flying -= 1
        spent += row["dollars"]
        with ledger.open("a") as handle:
            handle.write(json.dumps(row) + "\n")
        print(
            f"{build} {task['id']} {repeat} {row['status']} steps={row['steps']} s={row['seconds']} ${row['dollars']} total=${spent:.3f} :: {(row['answer'] or '')[:90]!r}",
            flush=True,
        )


order = []
for repeat in range(repeats):
    for index, task in enumerate(tasks):
        names = list(builds)
        shift = (index + repeat) % len(names)
        order += [(b, task, repeat) for b in names[shift:] + names[:shift] if (b, task["id"], repeat) not in done]
with ThreadPoolExecutor(concurrency) as pool:
    list(pool.map(lambda job: one(*job), order))
print(f"finished: ${spent:.3f} tracked", flush=True)
