"""Time fastbrowse-probe against the Python reference, interleaved, and check their snapshots agree.

    cargo build --profile release-small
    python3 crates/fastbrowse-probe/bench/run.py [runs]

Serves tests/browser/sites/main over HTTP for the fixture, and also reads https://news.ycombinator.com/.
One untimed run of each comes first, so neither pays Chrome's first read from a cold disk.
"""

import functools
import http.server
import json
import os
import re
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
PROBE = ROOT / "target" / "release-small" / "fastbrowse-probe"
REFERENCE = Path(__file__).with_name("python_reference.py")
PHASES = ("imported", "connected", "tab", "navigated", "snapshot", "exited")


def serve() -> str:
    class Quiet(http.server.SimpleHTTPRequestHandler):
        def log_message(self, *_: object) -> None:
            pass

    handler = functools.partial(Quiet, directory=str(ROOT / "tests/browser/sites/main"))
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return f"http://127.0.0.1:{server.server_address[1]}/index.html"


def run(command: list[str]) -> tuple[dict[str, float], dict]:
    t0 = time.time_ns()
    done = subprocess.run(
        command, cwd=ROOT, capture_output=True, text=True, env={**os.environ, "FASTBROWSE_PROBE_T0_NS": str(t0)}
    )
    exited = (time.time_ns() - t0) / 1e6
    if done.returncode:
        raise SystemExit(f"{command[0]} failed:\n{done.stderr}")
    marks = {m[1]: float(m[2]) for m in re.finditer(r"^(\w+): ([\d.]+) ms$", done.stderr, re.M)}
    return {**marks, "exited": exited}, json.loads(done.stdout)


def _median(rows: list[dict[str, float]], phase: str) -> str:
    values = [r[phase] for r in rows if phase in r]
    return f"{statistics.median(values):>12.0f}" if values else f"{'-':>12}"


def summary(snapshot: dict) -> list[tuple]:
    return [(c["id"], c["role"], c["label"]) for c in snapshot["controls"]]


def main(runs: int) -> None:
    arms = {
        "rust": lambda url: [str(PROBE), url],
        "python": lambda url: ["uv", "run", "--no-sync", "python", str(REFERENCE), url],
    }
    for url in (serve(), "https://news.ycombinator.com/"):
        print(f"\n{url}")
        for command in arms.values():
            run(command(url))
        timings: dict[str, list[dict[str, float]]] = {arm: [] for arm in arms}
        snapshots: dict[str, dict] = {}
        for _ in range(runs):
            for arm, command in arms.items():
                marks, snapshots[arm] = run(command(url))
                timings[arm].append(marks)
        print(f"{'arm':8}" + "".join(f"{p:>12}" for p in PHASES) + "   (median ms from spawn)")
        for arm, rows in timings.items():
            print(f"{arm:8}" + "".join(_median(rows, p) for p in PHASES))
        rust, python = summary(snapshots["rust"]), summary(snapshots["python"])
        print(f"controls: rust {len(rust)}, python {len(python)}, identical (id, role, label): {rust == python}")
        if rust != python:
            for a, b in zip(rust, python, strict=False):
                if a != b:
                    print(f"  first difference: rust {a} python {b}")
                    break
        print("first 3:", rust[:3])


main(int(sys.argv[1]) if len(sys.argv) > 1 else 5)
