"""Gate a release on its fixture evals and its benchmark campaign.

The release workflow runs this before it publishes anything. A release passes when:

- a dispatched Evals run on a commit with the tag's tree succeeded, so every fixture task passed every repeat;
- for a minor release, or a patch whose tag message has the line `Benchmarks: run`, the runs of the benchmark
  campaign `release-<version>` together measured every task at least three times, and against the campaign named
  in `docs/results/release-baseline.json` no task's pass rate fell, and the median run time and the mean cost per
  run are both lower.
- for a release whose tag message has the line `Benchmarks: official`, both official suites ran three times on its
  build, every cost known. Their judge reads every screenshot of every run, which costs far more than the internal
  campaign, so a release claims official scores only when it says so.

Fixtures are cheap, so they must pass on the exact tree. Live tasks are not, so a fix late in a release reruns only
the tasks it affects (the campaign's `only` input): each task is scored on the newest campaign run that measured it
at least three times. The gate prints the run and the commit behind every task, and every row's trace is in
Langfuse, so what each task was measured on stays visible.

    uv run python scripts/release_gate.py v0.10.0            # what the release workflow runs
    uv run python scripts/release_gate.py --ref <sha>        # before tagging: would this commit pass?
    uv run python scripts/release_gate.py --ref <sha> --official   # and with official scores claimed
"""

from __future__ import annotations

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
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from fastbrowse.evals.benchmarks import Candidate

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


def official_candidate(raw: dict[str, Any], campaign: str, agent_sha: str) -> Candidate:
    """A release requires official coverage even when its baseline measured only internal tasks."""
    from fastbrowse.evals.benchmarks import Candidate

    candidate = Candidate.model_validate(raw)
    if candidate.campaign_id != campaign or candidate.agent_sha != agent_sha or candidate.fixture is None:
        raise GateError("the campaign candidate must identify the measured build and its exact fixture receipt")
    groups = {group.id: group for group in candidate.groups if group.id in {"bu-bench", "online-mind2web"}}
    if set(groups) != {"bu-bench", "online-mind2web"}:
        raise GateError("the release candidate must contain both official groups")
    for name, group in groups.items():
        if (
            group.repeats != MIN_RUNS
            or group.agent_sha != candidate.agent_sha
            or group.runner_sha != candidate.runner_sha
            or group.sources
            or group.references
            or group.excluded
        ):
            raise GateError(f"{name}: three repeats on the exact candidate build and runner are required")
        if any(row.dollars is None for row in group.rows) or any(slot.dollars is None for slot in group.blocked):
            raise GateError(f"{name}: official costs must be known")
        if any(slot.reason != "evidence-unavailable" for slot in group.unjudged):
            raise GateError(f"{name}: an official unjudged slot requires evidence-unavailable producer proof")
    return candidate


def official_receipts(repo: str, candidate: Candidate) -> list[str]:
    """The producer's public artifacts bind the candidate to checked task sources and upstream grades."""
    from fastbrowse_evals.publication import OfficialRows, official_collected

    expected = {group.id: group for group in candidate.groups if group.id in {"bu-bench", "online-mind2web"}}
    found: dict[str, int] = {}
    for run in _runs(repo, "official-benchmarks.yml", "--status", "success"):
        run_id = int(run["databaseId"])
        metadata = json.loads(_gh("api", f"repos/{repo}/actions/runs/{run_id}"))
        if (
            metadata["path"] != ".github/workflows/official-benchmarks.yml"
            or metadata["event"] != "workflow_dispatch"
            or metadata["conclusion"] != "success"
            or metadata["status"] != "completed"
        ):
            continue
        with tempfile.TemporaryDirectory(prefix="release-official-") as directory:
            try:
                _gh("run", "download", str(run_id), "-R", repo, "-D", directory, "-p", "official-rows-*")
            except subprocess.CalledProcessError:
                continue
            for path in sorted(Path(directory).rglob("*.rows.json")):
                part = OfficialRows.model_validate_json(path.read_bytes())
                if (
                    part.agent_sha != candidate.agent_sha
                    or part.runner_sha != candidate.runner_sha
                    or part.repeats != MIN_RUNS
                    or part.group not in expected
                ):
                    continue
                if official_collected(part).group == expected[part.group]:
                    found.setdefault(part.group, run_id)
        if found.keys() == expected.keys():
            return [f"{name}: verified official producer run {found[name]}" for name in sorted(found)]
    raise GateError("both official groups require matching public receipts from successful exact-runner workflows")


def campaign_candidate(repo: str, campaign: str, tree: str, fixture_id: int) -> tuple[Candidate, list[str]]:
    """A successful candidate stage has checked the private tracking readback before exporting public figures."""
    for run in _runs(repo, "benchmark-campaign.yml", "--status", "completed"):
        if run["displayTitle"].split()[-1:] != [campaign]:
            continue
        run_id = int(run["databaseId"])
        metadata = json.loads(_gh("api", f"repos/{repo}/actions/runs/{run_id}"))
        if metadata["path"] != ".github/workflows/benchmark-campaign.yml":
            continue
        jobs = json.loads(_gh("api", f"repos/{repo}/actions/runs/{run_id}/jobs?per_page=100"))["jobs"]
        if not any(job["name"] == "Build the candidate" and job["conclusion"] == "success" for job in jobs):
            continue
        with tempfile.TemporaryDirectory(prefix="release-candidate-") as directory:
            try:
                _gh("run", "download", str(run_id), "-R", repo, "-D", directory, "-n", "benchmark-candidate")
            except subprocess.CalledProcessError:
                continue
            paths = list(Path(directory).rglob("benchmark-candidate.json"))
            if len(paths) != 1:
                continue
            raw = json.loads(paths[0].read_text(encoding="utf-8"))
        if _tree(repo, raw["agent_sha"]) != tree:
            continue
        candidate = official_candidate(raw, campaign, raw["agent_sha"])
        assert candidate.fixture is not None
        if candidate.fixture.run_id != fixture_id:
            fixture = json.loads(_gh("api", f"repos/{repo}/actions/runs/{candidate.fixture.run_id}"))
            if (
                fixture["head_sha"] != candidate.agent_sha
                or fixture["path"] != ".github/workflows/evals.yml"
                or fixture["event"] != "workflow_dispatch"
                or fixture["conclusion"] != "success"
                or fixture["status"] != "completed"
            ):
                raise GateError("the candidate's original fixture receipt must pass on its measured build")
        return candidate, [f"official candidate: campaign run {run_id}", *official_receipts(repo, candidate)]
    raise GateError("no complete official campaign candidate on the release tree")


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


def baseline_rows(repo: str, baseline: dict[str, Any]) -> list[dict[str, Any]]:
    """The baseline campaign's rows, refused unless they still cover every task the baseline file lists: `compare`
    checks only the tasks it is given, so a run whose ledger is gone would drop its tasks from the gate unseen."""
    rows, _ = campaign_rows(repo, baseline["campaign"])
    missing = set(baseline["tasks"]) - {f"{task.suite}/{task.task}" for task in rows_by_task(rows)}
    if missing:
        names = ", ".join(sorted(missing))
        raise GateError(f"campaign {baseline['campaign']} no longer supplies {len(missing)} baseline tasks: {names}")
    return rows


def tag_lines(repo: str, tag: str) -> list[str]:
    """The lines of `tag`'s annotated message; a lightweight tag has none."""
    ref = json.loads(_gh("api", f"repos/{repo}/git/ref/tags/{tag}"))["object"]
    if ref["type"] != "tag":
        return []
    return json.loads(_gh("api", f"repos/{repo}/git/tags/{ref['sha']}"))["message"].splitlines()


def measures_itself(version: str, lines: list[str]) -> bool:
    """A minor release does; a patch does when its annotated tag message has the line `Benchmarks: run`, or claims
    official scores, which are compared beside the campaign."""
    return version.split(".")[-1] == "0" or "Benchmarks: run" in lines or "Benchmarks: official" in lines


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("tag", nargs="?", help="the release tag, v<x.y.z>")
    parser.add_argument("--ref", help="check a commit before it is tagged, as a minor release")
    parser.add_argument("--version", help="the version the campaign is named for (default: the tag's)")
    parser.add_argument("--official", action="store_true", help="with --ref, also require official coverage")
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
        fixture_id = fixture_run(args.repo, tree)
        print(f"fixtures: Evals run {fixture_id} passed on tree {tree}")
        lines = tag_lines(args.repo, args.tag) if args.tag else []
        if args.tag and not measures_itself(version, lines):
            print(f"{args.tag} is a patch release without 'Benchmarks: run' in its tag message: no campaign needed")
            return 0
        baseline = json.loads(BASELINE.read_text(encoding="utf-8"))
        if args.official or "Benchmarks: official" in lines:
            _, official_notes = campaign_candidate(args.repo, f"release-{version}", tree, fixture_id)
        else:
            official_notes = ["official benchmarks: not claimed (no 'Benchmarks: official' line)"]
        candidate, used = campaign_rows(args.repo, f"release-{version}")
        before = baseline_rows(args.repo, baseline)
        failures, notes = compare(candidate, before)
    except (GateError, ValueError, subprocess.CalledProcessError) as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    print(f"campaign release-{version} against {baseline['version']} (campaign {baseline['campaign']}):")
    for line in used:
        print(f"  {line}")
    for line in official_notes:
        print(f"  {line}")
    for note in notes:
        print(f"  {note}")
    for failure in failures:
        print(f"::error::{failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
