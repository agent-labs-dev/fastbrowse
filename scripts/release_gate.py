"""Gate a release on its fixture evals and its benchmark campaign.

The release workflow runs this before it publishes anything. A release passes when:

- a dispatched Evals run on a commit with the tag's tree succeeded, so every fixture task passed every repeat;
- for a minor release, or a patch whose tag message has the line `Benchmarks: run`, the runs of the benchmark
  campaign `release-<version>` together measured every task at least three times, and against the campaign named
  in `docs/results/release-baseline.json` no task's pass rate fell, and the median run time and the mean cost per
  run are both lower.

Fixtures are cheap, so they must pass on the exact tree. Live tasks are not, so a fix late in a release reruns only
the tasks it affects (the campaign's `only` input): each task is scored on the newest campaign run that measured it
at least three times. The gate prints the run and the commit behind every task, and every row's trace is in
Langfuse, so what each task was measured on stays visible.

    uv run python scripts/release_gate.py v0.10.0            # what the release workflow runs
    uv run python scripts/release_gate.py --ref <sha>        # before tagging: would this commit pass?
"""

import argparse
import json
import math
import os
import statistics
import subprocess
import sys
import tempfile
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Any

BASELINE = Path(__file__).resolve().parent.parent / "docs" / "results" / "release-baseline.json"
ARM = "fastbrowse"


class GateError(RuntimeError):
    """The release does not have the evals it needs."""


@dataclass(frozen=True)
class Task:
    suite: str
    task: str
    version: int


def rows_by_task(rows: list[dict[str, Any]]) -> dict[Task, list[dict[str, Any]]]:
    found: dict[Task, list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        if row.get("arm") == ARM:
            found[Task(row["suite"], row["task"], int(row["task_version"]))].append(row)
    return found


def _rate(rows: list[dict[str, Any]]) -> float:
    return sum(bool(row.get("passed")) for row in rows) / len(rows)


def compare(candidate: list[dict[str, Any]], baseline: list[dict[str, Any]]) -> tuple[list[str], list[str]]:
    """The reasons the candidate fails against the baseline, and notes on what could not be compared.

    A task whose version changed since the baseline measures something else, so it is noted rather than compared.
    A baseline task the candidate did not run fails: a release measures every task.
    """
    failures: list[str] = []
    notes: list[str] = []
    ours, theirs = rows_by_task(candidate), rows_by_task(baseline)
    by_name = {(task.suite, task.task): task for task in ours}
    for task, before in sorted(theirs.items(), key=lambda item: (item[0].suite, item[0].task)):
        now = ours.get(task)
        if now is None:
            moved = by_name.get((task.suite, task.task))
            if moved is None:
                failures.append(f"{task.suite}/{task.task}: not run")
            else:
                notes.append(f"{task.suite}/{task.task}: version {task.version} -> {moved.version}, not compared")
            continue
        if _rate(now) < _rate(before):
            failures.append(
                f"{task.suite}/{task.task}: passed {sum(bool(r.get('passed')) for r in now)}/{len(now)}, "
                f"the baseline {sum(bool(r.get('passed')) for r in before)}/{len(before)}"
            )
    mine = [row for rows in ours.values() for row in rows]
    base = [row for rows in theirs.values() for row in rows]
    for label, field, measure in (
        ("median seconds", "seconds", statistics.median),
        ("mean $", "dollars", statistics.fmean),
    ):
        if field == "seconds" and len({row.get("timing_basis") for row in [*mine, *base]}) > 1:
            failures.append("median seconds cannot compare different timing bases")
            continue
        incomplete = False
        for side, rows in (("candidate", mine), ("baseline", base)):
            # An absent receipt is not zero spend or zero time, and dropping it would change the measured population.
            if not rows or any(
                not isinstance(value := row.get(field), int | float)
                or isinstance(value, bool)
                or not math.isfinite(value)
                or value < 0
                or (field == "dollars" and row.get("unknown_cost") is True)
                for row in rows
            ):
                failures.append(f"{side} {field}: missing, unknown or invalid measurements")
                incomplete = True
        if incomplete:
            continue
        now = measure([float(row[field]) for row in mine])
        before = measure([float(row[field]) for row in base])
        if not now < before:
            failures.append(f"{label} {now:.4g} is not below the baseline's {before:.4g}")
        else:
            notes.append(f"{label} {now:.4g}, the baseline {before:.4g}")
    return failures, notes


def _gh(*args: str) -> str:
    return subprocess.run(["gh", *args], check=True, capture_output=True, text=True).stdout


def _tree(repo: str, ref: str) -> str:
    return _gh("api", f"repos/{repo}/commits/{ref}", "--jq", ".commit.tree.sha").strip()


def _runs(repo: str, workflow: str, *extra: str) -> list[dict[str, Any]]:
    fields = "databaseId,headSha,displayTitle,conclusion,status"
    return json.loads(_gh("run", "list", "-R", repo, "--workflow", workflow, "-L", "100", "--json", fields, *extra))


def fixture_run(repo: str, tree: str) -> int:
    """The newest successful dispatched Evals run on a commit with `tree`."""
    for run in _runs(repo, "evals.yml", "--event", "workflow_dispatch", "--status", "success"):
        if _tree(repo, run["headSha"]) == tree:
            return int(run["databaseId"])
    raise GateError("no successful dispatched Evals run on this tree: `gh workflow run evals.yml --ref <sha>`")


def ledger_rows(repo: str, run_id: int) -> list[dict[str, Any]]:
    """Every row of every `ledger-*` artifact of a campaign run."""
    with tempfile.TemporaryDirectory() as scratch:
        try:
            _gh("run", "download", str(run_id), "-R", repo, "-D", scratch, "-p", "ledger-*")
        except subprocess.CalledProcessError:
            return []
        return [
            json.loads(line)
            for path in sorted(Path(scratch).glob("ledger-*/*.jsonl"))
            if not path.name.endswith((".attempts.jsonl", ".budget.jsonl"))
            for line in path.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]


MIN_RUNS = 3
"""A task's pass rate rests on at least this many runs, as a published comparison does."""


def campaign_rows(repo: str, campaign: str) -> tuple[list[dict[str, Any]], list[str]]:
    """Each task's rows from the newest finished run of `campaign` that measured it `MIN_RUNS` times or more, and a
    line per run used naming the commits it measured and how many tasks it supplied. An older run never supplies a
    task a newer one measured, even at another task version."""
    chosen: dict[tuple[str, str], list[dict[str, Any]]] = {}
    used: list[str] = []
    for run in _runs(repo, "benchmark-campaign.yml", "--status", "completed"):
        if run["displayTitle"].split()[-1:] != [campaign]:
            continue
        fresh = {
            (task.suite, task.task): rows
            for task, rows in rows_by_task(ledger_rows(repo, int(run["databaseId"]))).items()
            if (task.suite, task.task) not in chosen and len(rows) >= MIN_RUNS
        }
        if fresh:
            chosen.update(fresh)
            shas = sorted({row["run"]["git_sha"][:8] for rows in fresh.values() for row in rows})
            used.append(f"run {run['databaseId']} on {', '.join(shas)}: {len(fresh)} tasks")
    if not chosen:
        raise GateError(f"no finished campaign {campaign} measured any task {MIN_RUNS} times")
    return [row for rows in chosen.values() for row in rows], used


def measures_itself(repo: str, tag: str, version: str) -> bool:
    """A minor release does; a patch does when its annotated tag message has the line `Benchmarks: run`."""
    if version.split(".")[-1] == "0":
        return True
    ref = json.loads(_gh("api", f"repos/{repo}/git/ref/tags/{tag}"))["object"]
    if ref["type"] != "tag":
        return False
    message = json.loads(_gh("api", f"repos/{repo}/git/tags/{ref['sha']}"))["message"]
    return "Benchmarks: run" in message.splitlines()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("tag", nargs="?", help="the release tag, v<x.y.z>")
    parser.add_argument("--ref", help="check a commit before it is tagged, as a minor release")
    parser.add_argument("--version", help="the version the campaign is named for (default: the tag's)")
    parser.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", "agent-labs-dev/fastbrowse"))
    args = parser.parse_args(argv)
    if bool(args.tag) == bool(args.ref):
        parser.error("give a tag or --ref")
    ref = args.tag or args.ref
    version = args.version or (args.tag or "").removeprefix("v")
    if not version:
        parser.error("--ref needs --version")
    tree = _tree(args.repo, ref)
    try:
        print(f"fixtures: Evals run {fixture_run(args.repo, tree)} passed on tree {tree}")
        if args.tag and not measures_itself(args.repo, args.tag, version):
            print(f"{args.tag} is a patch release without 'Benchmarks: run' in its tag message: no campaign needed")
            return 0
        baseline = json.loads(BASELINE.read_text(encoding="utf-8"))
        candidate, used = campaign_rows(args.repo, f"release-{version}")
        failures, notes = compare(candidate, ledger_rows(args.repo, int(baseline["campaign_run"])))
    except GateError as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    print(f"campaign release-{version} against {baseline['version']} (run {baseline['campaign_run']}):")
    for line in used:
        print(f"  {line}")
    for note in notes:
        print(f"  {note}")
    for failure in failures:
        print(f"::error::{failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
