"""Tables for docs/evals.md from the live head-to-head's rows.

    uv run python scripts/h2h_report.py artifacts/evals/h2h.jsonl [--since UNIX_TIME] [--videos]

Prints a table for the answer tasks, the navigation tasks, and each category (pass rate, correct answers,
median and mean time, cost per task, wasted actions, for each arm), and with --videos one line per recorded run.
A run whose cost is unknown is left out of the cost column and counted beside it, never guessed.

Passing is not the bar: a run that clicks a dead button three times and asks for help twice before it finishes
passes, and is still slow and costly. Wasted actions count, from each run's own step trace, the steps that did
not move it forward: a request for help, an action that did not execute or left the page unchanged, and an
action repeated on the same target. Hosted Browser Use reports no step trace, so it has no count.
"""

import argparse
import json
import statistics
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import cast

from fastbrowse.evals.live_tasks import TASKS

ARMS = {"fast": "fastbrowse", "ultrafast": "jev-ultrafast", "hosted": "Browser Use (hosted)"}
CATEGORIES = ("lookup", "login", "checkout", "safety", "widget", "navigate")


def load(path: Path, since: float) -> list[dict[str, object]]:
    """Rows from `since` on, for the arms each task grades on equal terms (earlier runs tried every arm everywhere)."""
    arms = {task.id: task.arms for task in TASKS}
    rows = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
    return [r for r in rows if float(r.get("at", 0) or 0) >= since and r["arm"] in arms.get(str(r["task"]), ())]


_PASSIVE = {"read", "scroll", "done", "wait"}


def _steps(row: Mapping[str, object]) -> list[tuple[str, str, str]] | None:
    """(operation, the action as a repeat is judged, outcome) per step; None for an arm that reports no steps."""
    log = row.get("step_log")
    if isinstance(log, list):
        # The same action from another page is a new action: the second of two searches fills the same box.
        steps = cast(list[dict[str, object]], log)
        return [
            (str(s["operation"]), f"{s['operation']} {s.get('target')} @ {s.get('url')}", str(s["outcome"]))
            for s in steps
        ]
    trace = row.get("trace")
    if not isinstance(trace, list):
        return None
    # Rows from before step_log hold only "operation target -> outcome", so a repeat there ignores the page.
    parsed = [str(step).rpartition(" -> ") for step in cast(list[object], trace)]
    return [(action.split(" ", 1)[0], action, outcome.strip()) for action, _, outcome in parsed]


def wasted(row: Mapping[str, object]) -> int | None:
    """Steps that did not move a run forward: help requests, actions that failed or changed nothing, repeats."""
    if (steps := _steps(row)) is None:
        return None
    seen: set[str] = set()
    count = 0
    for operation, action, outcome in steps:
        if operation == "escalate" or outcome not in {"executed", "changed"}:
            count += 1
        elif operation not in _PASSIVE:
            count += action in seen
            seen.add(action)
    return count


def _number(value: object) -> float | None:
    return float(value) if isinstance(value, int | float) else None


def table(rows: Sequence[dict[str, object]]) -> str:
    lines = [
        "| | passed | correct answer | median time | mean time | cost per task | wasted actions per run |",
        "|:--|:--|:--|:--|:--|:--|:--|",
    ]
    for arm, name in ARMS.items():
        runs = [r for r in rows if r["arm"] == arm]
        if not runs:
            continue
        seconds = [s for r in runs if (s := _number(r.get("seconds"))) is not None]
        dollars = [d for r in runs if (d := _number(r.get("dollars"))) is not None]
        unknown = len(runs) - len(dollars)
        cost = f"${statistics.mean(dollars):.4f}" if dollars else "unknown"
        if unknown and dollars:
            cost += f" ({unknown} unknown)"
        counts = [w for r in runs if (w := wasted(r)) is not None]
        waste = f"{statistics.mean(counts):.1f}" if counts else "not reported"
        lines.append(
            f"| {name} | {sum(bool(r['passed']) for r in runs)}/{len(runs)} "
            f"| {sum(bool(r.get('correct')) for r in runs)}/{len(runs)} "
            f"| {statistics.median(seconds):.1f}s | {statistics.mean(seconds):.1f}s | {cost} | {waste} |"
        )
    return "\n".join(lines)


def videos(rows: Sequence[dict[str, object]]) -> str:
    lines = ["| video | arm | task | result | time | wasted actions |", "|:--|:--|:--|:--|:--|:--|"]
    for r in sorted(rows, key=lambda r: (str(r["task"]), str(r["arm"]), str(r.get("video")))):
        if r.get("video"):
            mark = "pass" if r["passed"] else "fail"
            waste = "-" if (count := wasted(r)) is None else count
            lines.append(
                f"| `{Path(str(r['video'])).name}` | {r['arm']} | {r['task']} | {mark} | {r['seconds']}s | {waste} |"
            )
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("rows", type=Path)
    parser.add_argument("--since", type=float, default=0.0, help="only rows from this Unix time on")
    parser.add_argument("--videos", action="store_true")
    args = parser.parse_args()
    rows = load(args.rows, args.since)
    # Arms meet only on the tasks both can be graded on, so the headline tables never mix task sets.
    answers = [r for r in rows if r["category"] != "navigate" and r["task"] != "saucedemo-pause"]
    print("### Answer tasks\n\n" + table(answers))
    print("\n### Navigation tasks\n\n" + table([r for r in rows if r["category"] == "navigate"]))
    for category in CATEGORIES:
        subset = [r for r in rows if r["category"] == category]
        if subset:
            tasks = ", ".join(sorted({f"`{r['task']}`" for r in subset}))
            print(f"\n### {category}\n\n{tasks}\n\n{table(subset)}")
    if args.videos:
        print("\n### Videos\n\n" + videos(rows))


if __name__ == "__main__":
    main()
