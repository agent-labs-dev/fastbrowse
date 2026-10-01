"""A fixture eval run's per-task pass rate, which is the suite's stability budget.

    uv run python scripts/eval_stability.py artifacts/evals/nightly.jsonl [--target 1.0]

A suite that passes once is not a suite that passes. One task that passes three times in five moves a published
figure on its own, and the run it happens to be in decides whether anyone sees it. This reads the rows a run
wrote, prints each task's rate over its repeats, and exits non-zero when any task is below the target, so a flaky
task fails the job rather than quietly feeding a headline number.
"""

import argparse
import json
import sys
from collections import Counter
from collections.abc import Sequence
from pathlib import Path


def rows(path: Path) -> list[dict[str, object]]:
    """Every row a runner appended to `path`."""
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def rates(rows: Sequence[dict[str, object]]) -> list[tuple[str, str, int, int]]:
    """(suite, task, passes, runs) per task, ordered by suite and then task."""
    seen: Counter[tuple[str, str]] = Counter()
    passed: Counter[tuple[str, str]] = Counter()
    for row in rows:
        key = (str(row.get("suite", "?")), str(row.get("task", "?")))
        seen[key] += 1
        passed[key] += 1 if row.get("passed") else 0
    return [(suite, task, passed[(suite, task)], seen[(suite, task)]) for suite, task in sorted(seen)]


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="check a fixture eval run against a per-task pass target")
    parser.add_argument("rows", type=Path, help="the JSONL file the runner wrote")
    parser.add_argument("--target", type=float, default=1.0, help="the pass rate every task must reach")
    args = parser.parse_args(argv)
    if not args.rows.exists():
        parser.error(f"{args.rows} does not exist")
    recorded = rows(args.rows)
    measured = rates(recorded)
    if not measured:
        parser.error(f"{args.rows} holds no rows")
    width = max(len(task) for _, task, _, _ in measured)
    below = [entry for entry in measured if entry[2] / entry[3] < args.target]
    for suite, task, passed, runs in measured:
        mark = "ok   " if passed / runs >= args.target else "FLAKY"
        print(f"{mark} {suite:5} {task:{width}} {passed}/{runs}")
    summary = f"{len(measured) - len(below)}/{len(measured)} tasks at {args.target:.0%} or better"
    print(f"{summary} over {len(recorded)} runs")
    if below:
        print("below target: " + ", ".join(task for _, task, _, _ in below))
    return 1 if below else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
