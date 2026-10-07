"""A fixture eval run's per-task pass rate, which is the suite's stability budget.

    uv run python scripts/eval_stability.py artifacts/evals/nightly.jsonl [--target 1.0] [--suite local mock --repeat 3]
    uv run python scripts/eval_stability.py --check-providers

A suite that passes once is not a suite that passes. One task that passes three times in five moves a published
figure on its own, and the run it happens to be in decides whether anyone sees it. This reads the rows a run
wrote, prints each task's rate over its repeats, and exits non-zero when any task is below the target, so a flaky
task fails the job rather than quietly feeding a headline number.

Given the suites and the repeat count the run was asked for, it also refuses rows that do not cover them: a runner
that died part way leaves only the tasks it reached, and those can all have passed.

`--check-providers` answers whether the environment can call the models at all, so a scheduled job fails once,
before a task starts, naming the setting to add. OpenRouter or the gateway can serve the LLM.
"""

import argparse
import json
import sys
from collections import Counter
from collections.abc import Sequence
from pathlib import Path

from fastbrowse.clients.environment import ConfigurationError, JevSource, Settings, load_settings


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


_JEV_SECRET = {
    JevSource.OPENROUTER: "OPENROUTER_API_KEY",
    JevSource.TYPESAFE: "TYPESAFE_API_KEY",
    JevSource.GATEWAY: "AI_GATEWAY_API_KEY",
}


def provider_gaps(settings: Settings) -> list[str]:
    """The model settings a fixture run needs but the environment does not supply, each naming the setting.

    It asks `jev_route` for the first route rather than repeating the choice, so a route the environment
    changes is the route this checks.
    """
    gaps: dict[str, str] = {}
    if not settings.openrouter_api_key and not settings.ai_gateway_api_key:
        gaps["OPENROUTER_API_KEY or AI_GATEWAY_API_KEY"] = "the LLM needs a configured provider"
    source, _backup = settings.jev_route()
    keyed = {
        JevSource.OPENROUTER: settings.openrouter_api_key,
        JevSource.TYPESAFE: settings.typesafe_api_key,
        JevSource.GATEWAY: settings.ai_gateway_api_key,
    }
    if not keyed[source]:
        gaps.setdefault(_JEV_SECRET[source], f"Jev starts on the {source} route")
    return [f"{setting}: {reason}" for setting, reason in gaps.items()]


def uncovered(measured: Sequence[tuple[str, str, int, int]], suites: Sequence[str], repeat: int) -> list[str]:
    """Tasks of `suites` with fewer than `repeat` recorded runs, each as `suite task runs/repeat`."""
    from fastbrowse.evals.mock_tasks import TASKS as MOCK_TASKS
    from fastbrowse.evals.tasks import TASKS

    expected = {"local": TASKS, "mock": MOCK_TASKS}
    runs = {(suite, task): count for suite, task, _, count in measured}
    return [
        f"{suite} {task.id} {runs.get((suite, task.id), 0)}/{repeat}"
        for suite in suites
        for task in expected[suite]
        if runs.get((suite, task.id), 0) < repeat
    ]


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="check a fixture eval run against a per-task pass target")
    parser.add_argument("rows", nargs="?", type=Path, help="the JSONL file the runner wrote")
    parser.add_argument("--target", type=float, default=1.0, help="the pass rate every task must reach")
    parser.add_argument("--suite", nargs="+", choices=["local", "mock"], default=[], help="suites the run covers")
    parser.add_argument("--repeat", type=int, default=1, help="runs each task of those suites must have")
    parser.add_argument(
        "--check-providers", action="store_true", help="check that a fixture run can call its models, then exit"
    )
    args = parser.parse_args(argv)
    if args.check_providers:
        try:
            settings = load_settings()
        except ConfigurationError as error:
            print(f"invalid settings: {error}")
            return 1
        gaps = provider_gaps(settings)
        if gaps:
            print("the fixture suites cannot call their models:")
            for gap in gaps:
                print(f"- {gap}")
            return 1
        print(f"model providers ready: {settings.providers()}")
        return 0
    if args.rows is None:
        parser.error("rows is required unless --check-providers is given")
    # A missing or empty file is the runner's failure, not a wrong invocation, so it reads as one and does not
    # hide behind an argparse usage error.
    if not args.rows.exists():
        print(f"no eval rows: {args.rows} does not exist, so the runner recorded nothing")
        return 1
    recorded = rows(args.rows)
    measured = rates(recorded)
    if not measured:
        print(f"no eval rows: {args.rows} is empty, so the runner executed no task")
        return 1
    width = max(len(task) for _, task, _, _ in measured)
    below = [entry for entry in measured if entry[2] / entry[3] < args.target]
    for suite, task, passed, runs in measured:
        mark = "ok   " if passed / runs >= args.target else "FLAKY"
        print(f"{mark} {suite:5} {task:{width}} {passed}/{runs}")
    summary = f"{len(measured) - len(below)}/{len(measured)} tasks at {args.target:.0%} or better"
    print(f"{summary} over {len(recorded)} runs")
    if below:
        print("below target: " + ", ".join(task for _, task, _, _ in below))
    missing = uncovered(measured, args.suite, args.repeat)
    if missing:
        print("incomplete run: " + ", ".join(missing))
    return 1 if below or missing else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
