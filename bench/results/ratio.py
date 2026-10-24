"""Tables of Python against Rust from `fastbrowse.evals.bench replay` run files.

    uv run python bench/results/ratio.py <python runs...> -- <rust runs...>

Each metric is the median across runs per task, as `bench report` takes it; the aggregate rows are the median and
the sum of those per-task medians.
"""

import json
import statistics
import sys
from pathlib import Path
from typing import Any

METRICS = ("wall_s", "cpu_s", "startup_s", "rss_bytes", "steps", "model_requests")


def per_task(paths: list[str]) -> dict[str, dict[str, Any]]:
    entries: dict[str, list[dict[str, Any]]] = {}
    for path in paths:
        for entry in json.loads(Path(path).read_text(encoding="utf-8"))["tasks"]:
            entries.setdefault(entry["task"], []).append(entry)
    table: dict[str, dict[str, Any]] = {}
    for task, runs in entries.items():
        row: dict[str, Any] = {m: statistics.median(run["metrics"][m] for run in runs) for m in METRICS}
        row["passed"] = sum(run["result"]["grade"]["passed"] for run in runs)
        row["runs"] = len(runs)
        table[task] = row
    return table


def fmt(value: float, metric: str) -> str:
    if metric == "rss_bytes":
        return f"{value / 2**20:.1f}"
    if metric == "startup_s":
        return f"{value:.3f}"
    if metric in ("steps", "model_requests"):
        return f"{value:g}"
    return f"{value:.2f}"


def ratio(a: float, b: float) -> str:
    return f"{a / b:.1f}x" if b else "-"


def main(argv: list[str]) -> None:
    cut = argv.index("--")
    py, rs = per_task(argv[:cut]), per_task(argv[cut + 1 :])
    print("| metric | Python | Rust | Python / Rust |")
    print("|---|---:|---:|---:|")
    labels = (("wall_s", "wall s"), ("cpu_s", "agent CPU s"), ("startup_s", "startup s"), ("rss_bytes", "peak RSS MiB"))
    for metric, label in labels:
        pm = statistics.median(row[metric] for row in py.values())
        rm = statistics.median(row[metric] for row in rs.values())
        print(f"| {label}, median of tasks | {fmt(pm, metric)} | {fmt(rm, metric)} | {ratio(pm, rm)} |")
        combine, name = (max, "max") if metric == "rss_bytes" else (sum, "total")
        pt = combine(row[metric] for row in py.values())
        rt = combine(row[metric] for row in rs.values())
        print(f"| {label}, {name} | {fmt(pt, metric)} | {fmt(rt, metric)} | {ratio(pt, rt)} |")
    for metric, label in (("steps", "steps"), ("model_requests", "model requests")):
        pt = sum(row[metric] for row in py.values())
        rt = sum(row[metric] for row in rs.values())
        print(f"| {label}, total | {pt:g} | {rt:g} | - |")
    print()
    print(
        "| task | pass Py | pass Rs | wall Py | wall Rs | CPU Py | CPU Rs | CPU | startup Py | startup Rs "
        "| RSS Py | RSS Rs |"
    )
    print("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
    for task, p in py.items():
        r = rs[task]
        print(
            f"| {task} | {p['passed']}/{p['runs']} | {r['passed']}/{r['runs']} "
            f"| {fmt(p['wall_s'], 'wall_s')} | {fmt(r['wall_s'], 'wall_s')} "
            f"| {fmt(p['cpu_s'], 'cpu_s')} | {fmt(r['cpu_s'], 'cpu_s')} | {ratio(p['cpu_s'], r['cpu_s'])} "
            f"| {fmt(p['startup_s'], 'startup_s')} | {fmt(r['startup_s'], 'startup_s')} "
            f"| {fmt(p['rss_bytes'], 'rss_bytes')} | {fmt(r['rss_bytes'], 'rss_bytes')} |"
        )


if __name__ == "__main__":
    main(sys.argv[1:])
