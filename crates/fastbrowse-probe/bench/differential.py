"""Run Python's CdpPage and the Rust browser crate side by side on every fixture page and action sequence, compare
what each step produced, and time both.

    cargo build -p fastbrowse-probe --release
    uv run python crates/fastbrowse-probe/bench/differential.py [runs] [--only <page>]

bench/scripts.json lists the pages and steps. Each arm launches its own headless Chrome and runs every page in a
fresh session, as the browser tests do; the arms alternate, run by run. Observations, captures and action results
are compared as JSON after taking out what differs by construction: `captured_at`, Chrome's target and session ids
(renamed by order of first appearance) and the order of a control's `operations` (a frozenset in Python, so its
JSON order changes with the process's hash seed). Everything else must match byte for byte.
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

PROBE = ROOT / "target" / "release" / "fastbrowse-probe"
REFERENCE = Path(__file__).with_name("python_reference.py")
SCRIPT = Path(__file__).with_name("scripts.json")
CHROME_ID = re.compile(r"\b[0-9A-F]{32}\b")


def run(command: list[str]) -> dict:
    done = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, env=os.environ)
    if done.returncode:
        raise SystemExit(f"{command[0]} failed:\n{done.stderr[-4000:]}")
    return json.loads(done.stdout)


def normalized(page: dict) -> list[str]:
    """Each step's output as comparable JSON text."""
    names: dict[str, str] = {}
    origins: dict[str, str] = {}
    entries: dict[str, str] = {}

    def strip(value: object) -> object:
        if isinstance(value, dict):
            out = {k: strip(v) for k, v in value.items() if k != "captured_at"}
            # performance.timeOrigin: when the document began, which no two runs share.
            if isinstance(out.get("document_key"), str) and out["document_key"]:
                out["document_key"] = origins.setdefault(out["document_key"], f"<time-origin-{len(origins)}>")
            # A history entry's id is Chrome's browser-wide counter, which other documents (an iframe, a popup) move on
            # their own schedule; only which entries are the same one carries meaning.
            if isinstance(out.get("history_entry"), str):
                tab, _, entry = out["history_entry"].rpartition(":")
                out["history_entry"] = f"{tab}:" + entries.setdefault(entry, f"<entry-{len(entries)}>")
            if isinstance(out.get("operations"), list):
                out["operations"] = sorted(out["operations"])
            return out
        if isinstance(value, list):
            return [strip(v) for v in value]
        if isinstance(value, str):
            return CHROME_ID.sub(lambda m: names.setdefault(m.group(0), f"<chrome-id-{len(names)}>"), value)
        return value

    return [json.dumps(strip(step["out"]), ensure_ascii=False) for step in page.get("steps", [])]


def first_difference(a: object, b: object, path: str = "$") -> str:
    if type(a) is not type(b):
        return f"{path}: {json.dumps(a)[:200]} != {json.dumps(b)[:200]}"
    if isinstance(a, dict):
        for key in list(dict.fromkeys([*a, *b])):
            if key not in a or key not in b:
                return f"{path}.{key}: present only in {'python' if key not in a else 'rust'}"
            if a[key] != b[key]:
                return first_difference(a[key], b[key], f"{path}.{key}")
        if list(a) != list(b):
            return f"{path}: key order {list(a)} != {list(b)}"
    if isinstance(a, list):
        if len(a) != len(b):
            return f"{path}: {len(a)} items != {len(b)} items"
        for i, (x, y) in enumerate(zip(a, b, strict=True)):
            if x != y:
                return first_difference(x, y, f"{path}[{i}]")
    return f"{path}: {json.dumps(a)[:300]} != {json.dumps(b)[:300]}"


def category(op: str) -> str:
    return op if op in ("navigate", "observe", "capture") else "act"


def main(runs: int, only: str | None) -> None:
    urls = serve_all()
    base = urls["main"].removesuffix("/index.html")
    extra = ["--only", only] if only else []
    arms = {
        "rust": [str(PROBE), str(SCRIPT), base, *extra],
        "python": ["uv", "run", "--no-sync", "python", str(REFERENCE), str(SCRIPT), base, *extra],
    }
    results: dict[str, list[dict]] = {arm: [] for arm in arms}
    wall: dict[str, list[float]] = {arm: [] for arm in arms}
    for _ in range(runs):
        for arm, command in arms.items():
            started = time.perf_counter()
            results[arm].append(run(command))
            wall[arm].append(time.perf_counter() - started)

    mismatches = 0
    compared = 0
    unstable: set[str] = set()
    for n in range(runs):
        rust_pages = {p["name"]: p for p in results["rust"][n]["pages"]}
        for page in results["python"][n]["pages"]:
            name = page["name"]
            python_steps, rust_steps = normalized(page), normalized(rust_pages[name])
            for i, (p, r) in enumerate(zip(python_steps, rust_steps, strict=False)):
                compared += 1
                if p != r:
                    mismatches += 1
                    op = page["steps"][i]["op"]
                    print(f"run {n} {name} step {i} ({op}): {first_difference(json.loads(p), json.loads(r))}")
            if len(python_steps) != len(rust_steps):
                mismatches += 1
                print(f"run {n} {name}: python ran {len(python_steps)} steps, rust {len(rust_steps)}")
    # A step whose Python output differs between runs is the page's own nondeterminism, not the port's.
    for arm in arms:
        for name in {p["name"] for p in results[arm][0]["pages"]}:
            outputs = [normalized(next(p for p in r["pages"] if p["name"] == name)) for r in results[arm]]
            if any(o != outputs[0] for o in outputs):
                unstable.add(f"{arm}:{name}")
    print(f"\n{compared} steps compared over {runs} runs, {mismatches} mismatches")
    if unstable:
        print("differs between runs of one arm:", ", ".join(sorted(unstable)))

    print("\nmedian ms per step kind (all pages, all runs)")
    print(f"{'kind':10}{'rust':>10}{'python':>10}{'speedup':>10}{'steps':>8}")
    for kind in ("navigate", "observe", "capture", "act"):
        times = {
            arm: [s["ms"] for r in results[arm] for p in r["pages"] for s in p["steps"] if category(s["op"]) == kind]
            for arm in arms
        }
        if times["rust"] and times["python"]:
            rust, python = statistics.median(times["rust"]), statistics.median(times["python"])
            print(f"{kind:10}{rust:>10.1f}{python:>10.1f}{python / rust:>9.2f}x{len(times['rust']):>8}")
    print("\ntotal ms per page (median over runs)")
    for page in results["python"][0]["pages"]:
        name = page["name"]
        totals = {
            arm: statistics.median(
                sum(s["ms"] for s in next(p for p in r["pages"] if p["name"] == name)["steps"]) for r in results[arm]
            )
            for arm in arms
        }
        print(f"  {name:20}{totals['rust']:>10.0f}{totals['python']:>10.0f}{totals['python'] / totals['rust']:>8.2f}x")
    launch = {arm: statistics.median(r["launch_ms"] for r in results[arm]) for arm in arms}
    print(f"\nChrome launch to DevTools, median ms: rust {launch['rust']:.0f}, python {launch['python']:.0f}")
    print(f"process wall time, median s: rust {statistics.median(wall['rust']):.1f}, python {statistics.median(wall['python']):.1f}")


_args = sys.argv[1:]
_only = _args[_args.index("--only") + 1] if "--only" in _args else None
_runs = next((int(a) for a in _args if a.isdigit()), 3)
main(_runs, _only)
