"""Time fastbrowse-probe against the Python reference, interleaved, and check their observations agree.

    cargo build --profile release-small
    uv run python crates/fastbrowse-probe/bench/run.py [runs]

Pages: tests/browser/sites/main with its cross-origin iframe (served by bench/serve.py), Hacker News, and the
dialog fixtures in bench/sites, which are checked once each rather than timed. One untimed run of each timed arm
comes first, so neither pays Chrome's first read from a cold disk.

Frame target ids differ on every run, so each is replaced by its order of first appearance before comparing.
"""

import json
import os
import re
import statistics
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from serve import ROOT, serve_all

PROBE = ROOT / "target" / "release-small" / "fastbrowse-probe"
REFERENCE = Path(__file__).with_name("python_reference.py")
PHASES = ("imported", "connected", "tab", "navigated", "snapshot", "exited")
ALERT = "document.getElementById('alert').click()"
CONFIRM = "document.getElementById('confirm').click()"


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


def summary(result: dict) -> list[dict]:
    frames: dict[str, str] = {}

    def frame(name: str | None) -> str | None:
        if name is None:
            return None
        target, _, rest = name.partition("/")
        return frames.setdefault(target, f"frame{len(frames)}") + (f"/{rest}" if rest else "")

    def step(observation: dict) -> dict:
        controls = [
            (
                f"{frame(c['id'].split(':')[0]) or 'main'}:{c['id'].split(':')[1]}"
                if not c["id"].startswith("main:")
                else c["id"],
                c["role"],
                c["label"],
                frame(c.get("frame_id")),
            )
            for c in observation["controls"]
        ]
        dialog = observation.get("dialog")
        return {
            "controls": controls,
            "dialog": dialog and (dialog["kind"], dialog["message"]),
            "inaccessible_frames": observation["inaccessible_frames"],
        }

    return [step(s) for s in result["steps"]]


def compare(rust: dict, python: dict) -> None:
    for n, (r, p) in enumerate(zip(summary(rust), summary(python), strict=True)):
        same_order = r["controls"] == p["controls"]
        same_set = sorted(r["controls"], key=repr) == sorted(p["controls"], key=repr)
        frames = len({c[3] for c in r["controls"] if c[3]})
        print(
            f"  step {n}: controls rust {len(r['controls'])} python {len(p['controls'])}, frame ids {frames}, "
            f"(id, role, label, frame) identical in order: {same_order}, as sets: {same_set}, "
            f"dialog {r['dialog']} vs {p['dialog']}, inaccessible {r['inaccessible_frames']} vs "
            f"{p['inaccessible_frames']}"
        )
        if not same_set:
            print("    only rust:", sorted(set(r["controls"]) - set(p["controls"]))[:5])
            print("    only python:", sorted(set(p["controls"]) - set(r["controls"]))[:5])


def main(runs: int) -> None:
    arms = {
        "rust": lambda url, *extra: [str(PROBE), url, *extra],
        "python": lambda url, *extra: ["uv", "run", "--no-sync", "python", str(REFERENCE), url, *extra],
    }
    urls = serve_all()
    for url in (urls["main"], "https://news.ycombinator.com/"):
        print(f"\n{url}")
        for command in arms.values():
            run(command(url))
        timings: dict[str, list[dict[str, float]]] = {arm: [] for arm in arms}
        results: dict[str, dict] = {}
        for _ in range(runs):
            for arm, command in arms.items():
                marks, results[arm] = run(command(url))
                timings[arm].append(marks)
        print(f"{'arm':8}" + "".join(f"{p:>12}" for p in PHASES) + "   (median ms from spawn)")
        for arm, rows in timings.items():
            print(f"{arm:8}" + "".join(_median(rows, p) for p in PHASES))
        compare(results["rust"], results["python"])
    for name, extra in (
        ("dialog", ("--click", ALERT, "--accept-dialog")),
        ("dialog", ("--click", CONFIRM, "--accept-dialog")),
        ("alert-on-load", ()),
    ):
        print(f"\n{name} {' '.join(extra)}")
        compare(run(arms["rust"](urls[name], *extra))[1], run(arms["python"](urls[name], *extra))[1])


main(int(sys.argv[1]) if len(sys.argv) > 1 else 5)
