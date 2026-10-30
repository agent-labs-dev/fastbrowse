"""Compare live harness runs across configs, and show one run's steps and model calls in order.

    python scripts/experiments/compare_runs.py DIR [DIR ...]              # per-task pass table and totals
    python scripts/experiments/compare_runs.py DIR --task TASK [--repeat N]  # one run, step by step

Each DIR holds the runs.jsonl that live_harness.py wrote. Nothing is cut: answers, failures and notes print whole.
"""

import argparse
import collections
import json
import statistics
from pathlib import Path


def rows(directory):
    return [json.loads(line) for line in (Path(directory) / "runs.jsonl").open()]


def calls(row):
    return collections.Counter(e["call"] for e in row.get("events", []) if e.get("event") == "call")


def table(directories):
    runs = {Path(d).name: rows(d) for d in directories}
    tasks = sorted({r["task"] for rs in runs.values() for r in rs})
    width = max(map(len, tasks))
    print(" " * width, *(f"{name:>14}" for name in runs))
    for task in tasks:
        cells = []
        for rs in runs.values():
            mine = [r for r in rs if r["task"] == task]
            cells.append(f"{sum(bool(r.get('passed')) for r in mine)}/{len(mine)}" if mine else "-")
        print(f"{task:<{width}}", *(f"{c:>14}" for c in cells))
    print()
    for name, rs in runs.items():
        passed = sum(bool(r.get("passed")) for r in rs)
        seconds = [r["seconds"] for r in rs if "seconds" in r]
        mean = collections.Counter()
        for r in rs:
            mean.update(calls(r))
        per_run = ", ".join(f"{k} {v / len(rs):.1f}" for k, v in sorted(mean.items()))
        print(f"{name}: {passed}/{len(rs)} passed, median {statistics.median(seconds):.1f}s; calls per run: {per_run}")
    print()
    for name, rs in runs.items():
        for r in rs:
            if not r.get("passed"):
                print(f"{name} {r['task']}#{r['repeat']}: {r.get('status')}: {r.get('failure') or r.get('crash')}")


def timeline(directory, task, repeat):
    row = next(r for r in rows(directory) if r["task"] == task and r["repeat"] == repeat)
    print(f"{row['task']}#{row['repeat']} {row.get('status')} passed={row.get('passed')}: {row.get('failure')}")
    print(f"answer: {row.get('answer')}")
    events = [e for e in row.get("events", []) if e.get("event") == "call"]
    steps = (row.get("result") or {}).get("steps", [])
    for step in steps:
        print(
            f"step {step['index']}: {step['operation']} by {step.get('decided_by')} on {step.get('target')!r}"
            f" -> {step.get('outcome')} {step.get('url')}"
        )
        if step.get("note"):
            print(f"    note: {step['note']}")
        for fact in step.get("facts") or []:
            print(f"    fact: {fact}")
    print("calls:", ", ".join(f"{e['call']} {e['ended'] - e['began']:.1f}s {e.get('outcome')}" for e in events))
    print(f"observed final page: {(row.get('observed') or {}).get('url')}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("dirs", nargs="+")
    parser.add_argument("--task")
    parser.add_argument("--repeat", type=int, default=0)
    args = parser.parse_args()
    if args.task:
        timeline(args.dirs[0], args.task, args.repeat)
    else:
        table(args.dirs)


if __name__ == "__main__":
    main()
