"""Project Parallax run directories into Fastbrowse's unchanged public candidate schema."""

from __future__ import annotations

import argparse
import hashlib
import math
import re
from collections.abc import Collection, Iterable
from pathlib import Path
from typing import Any, Literal

from fastbrowse.evals.benchmarks import (
    Arm,
    Candidate,
    ExcludedTask,
    ExportReceipt,
    Group,
    GroupId,
    Row,
    check_catalog,
    group_arms,
)
from fastbrowse.evals.benchmarks import (
    eligible as catalog_eligible,
)
from fastbrowse.evals.catalog import CATALOG_SHA256, SUITES
from fastbrowse.evals.catalog import PATH as CATALOG_PATH
from parallax.models import TrajectoryStatus, TrialResult, Verdict
from parallax.track.rows import LoadedRun, load_run

GROUPS: dict[str, tuple[int, int, str, set[str]]] = {
    "internal-fastbrowse": (53, 3, "task-success", {"fastbrowse"}),
    "internal-hosted": (53, 3, "task-success", {"fastbrowse"}),
    "internal-ultrafast": (52, 3, "task-success", {"fastbrowse-pool", "browser-use-ultrafast"}),
    "internal-backend": (53, 3, "task-success", {"fastbrowse", "fastbrowse-pool"}),
    "bu-bench": (200, 1, "weighted-rubric", {"fastbrowse"}),
    "online-mind2web": (300, 1, "full-task-success", {"fastbrowse"}),
}

ARM_MAPPING: dict[str, Arm] = {
    "fastbrowse": "fastbrowse",
    "fastbrowse-pool": "fastbrowse-pool",
    "browser-harness": "browser-harness",
    "browser-use-oss": "browser-use-oss",
}
"""Parallax harness names become public arm names in this one projection."""

OFFICIAL_SOURCES: dict[str, GroupId] = {
    "bu-bench": "bu-bench",
    "online-mind2web": "online-mind2web",
}
"""Official groups are selected by the Parallax task source recorded in the run manifest."""


def _official_source(run: LoadedRun) -> GroupId | None:
    selected = run.manifest.selection.split()
    if source := next((OFFICIAL_SOURCES.get(term) for term in selected if term in OFFICIAL_SOURCES), None):
        return source
    # A task-id-only smoke run has no source term in its selection. Official task ids carry their source prefix.
    return next(
        (
            group_id
            for source, group_id in OFFICIAL_SOURCES.items()
            if any(result.task_id.startswith(f"{source}-") for result in run.results)
        ),
        None,
    )


def public_arm(result: TrialResult) -> Arm | None:
    if result.harness == "browser-use-cloud":
        return "browser-use-ultrafast" if result.model == "bu-ultrafast" else "browser-use"
    return ARM_MAPPING.get(result.harness)


def _number(value: float | int | None) -> float | None:
    if value is None:
        return None
    value = float(value)
    return value if math.isfinite(value) and value >= 0 else None


def _sum_cost(results: Iterable[TrialResult]) -> float | None:
    costs = tuple(result.cost_usd for result in results)
    if any(cost is None for cost in costs):
        return None
    return math.fsum(cost for cost in costs if cost is not None)


def _usage(results: Iterable[TrialResult]) -> tuple[int | None, int, int, float]:
    usages = [usage for result in results for usage in result.usage]
    if not usages:
        return 0, 0, 0, 0.0
    requests = sum(usage.requests for usage in usages)
    input_tokens = sum(usage.input for usage in usages)
    output_tokens = sum(usage.output for usage in usages)
    agent_ms = sum(result.agent_ms or 0 for result in results)
    throughput = output_tokens / (agent_ms / 1000) if agent_ms > 0 else 0.0
    return requests, input_tokens, output_tokens, throughput


def _failure(result: TrialResult) -> str | None:
    value = result.invalid_reason or result.na_reason
    if not value:
        return None
    value = re.sub(r"[^A-Za-z0-9 ,.;:()'%+_=-]", " ", value)
    return value[:240].strip() or "trial did not complete"


def trial_result_to_row(result: TrialResult, *, attempts: Iterable[TrialResult] = (), official: bool = False) -> Row:
    """Map one selected Parallax result and all its physical attempts to one public row."""
    physical = tuple(attempts) or (result,)
    arm = public_arm(result)
    if arm is None:
        raise ValueError(f"Parallax harness {result.harness!r} has no public arm")
    basis = result.timing_basis
    same_basis = all(attempt.timing_basis == basis for attempt in physical)
    agent_ms = sum(attempt.agent_ms or 0 for attempt in physical)
    wall_ms = sum(attempt.wall_ms or 0 for attempt in physical)
    if basis == "session-ready-to-answer" and same_basis and agent_ms:
        seconds = agent_ms / 1000
        timing_basis = basis
        breakdown = {
            key: math.fsum(attempt.timing_breakdown.get(key, 0.0) for attempt in physical) / 1000
            for key in {key for attempt in physical for key in attempt.timing_breakdown}
        }
        open_seconds = breakdown.get("open")
        close_seconds = breakdown.get("close")
        timing_note = None
    else:
        seconds = wall_ms / 1000
        timing_basis = "physical-attempt-wall-time"
        open_seconds = None
        close_seconds = None
        timing_note = "Parallax retries include dispatch and teardown time because attempts used different clocks"
    llm_calls, input_tokens, output_tokens, throughput = _usage(physical)
    completed = result.status is TrajectoryStatus.COMPLETED if official else result.verdict is Verdict.PASS
    return Row(
        task=result.task_id,
        repeat=result.repeat,
        arm=arm,
        status="complete" if completed else result.verdict.value,
        completed=completed,
        score=_number(result.score) or 0.0,
        seconds=seconds,
        dollars=_sum_cost(physical),
        physical_runs=len(physical),
        llm_calls=llm_calls,
        llm_calls_basis="physical_requests" if llm_calls is not None else None,
        input_tokens=input_tokens,
        output_tokens=output_tokens,
        timing_basis=timing_basis,
        session_open_seconds=open_seconds,
        session_close_seconds=close_seconds,
        timing_note=timing_note,
        failure=_failure(result),
        tokens_per_second=throughput,
    )


def _catalog_tasks() -> list[Any]:
    return [task for suite in SUITES.values() for task in suite]


def _rows(run: LoadedRun) -> list[Row]:
    rows: list[Row] = []
    for result in run.results:
        if public_arm(result) != "fastbrowse":
            continue
        rows.append(trial_result_to_row(result, attempts=run.physical_runs(result.key)))
    return sorted(rows, key=lambda row: (row.task, row.repeat))


def _group_id(arms: Collection[str]) -> GroupId | None:
    arms = set(arms)
    if arms == {"fastbrowse"}:
        return "internal-fastbrowse"
    if {"fastbrowse", "fastbrowse-pool"} <= arms:
        return "internal-backend"
    if {"fastbrowse-pool", "browser-use-ultrafast"} <= arms:
        return "internal-ultrafast"
    if "fastbrowse" in arms and arms & {"browser-use", "browser-use-ultrafast", "browser-harness", "browser-use-oss"}:
        return "internal-hosted"
    return None


def _metric(group_id: GroupId) -> Literal["task-success", "weighted-rubric", "full-task-success"]:
    if group_id == "bu-bench":
        return "weighted-rubric"
    if group_id == "online-mind2web":
        return "full-task-success"
    return "task-success"


def _group(
    group_id: GroupId,
    rows_by_arm: dict[str, list[Row]],
    *,
    agent_sha: str,
    runner_sha: str,
    task_ids: Iterable[str] | None = None,
) -> Group:
    arms = group_arms(group_id, 7)
    if task_ids is None:
        catalog_tasks = {task.id: task for task in _catalog_tasks()}
        partial = {"browser-use", "browser-use-ultrafast", "browser-harness", "browser-use-oss"}
        task_ids = [
            task.id
            for task in _catalog_tasks()
            if all(catalog_eligible(arm, task) for arm in arms if arm not in partial)
        ]
        excluded = [
            ExcludedTask(arm=arm, task=task_id, reason=catalog_tasks[task_id].excluded[arm])
            for arm in arms
            if arm in partial and arm in rows_by_arm
            for task_id in task_ids
            if not catalog_eligible(arm, catalog_tasks[task_id])
        ]
    else:
        task_ids = sorted(set(task_ids))
        excluded = []
    rows = [row for arm, arm_rows in rows_by_arm.items() if arm in arms for row in arm_rows if row.task in task_ids]
    repeats = max((row.repeat for row in rows), default=-1) + 1
    return Group(
        id=group_id,
        agent_sha=agent_sha,
        runner_sha=runner_sha,
        metric=_metric(group_id),
        source_sha256=CATALOG_SHA256,
        task_ids=task_ids,
        repeats=repeats,
        limits=["Parallax campaign budget and reserve"],
        rows=sorted(rows, key=lambda row: (row.arm, row.task, row.repeat)),
        excluded=sorted(excluded, key=lambda item: (item.arm, item.task)),
    )


def candidate_from_runs(run_dirs: Iterable[Path], *, campaign_id: str, agent_sha: str, runner_sha: str) -> Candidate:
    """Build schema 7 groups from Parallax `parallax.result/2` run directories."""
    paths = tuple(run_dirs)
    loaded = [load_run(path) for path in paths]
    selected: dict[tuple[GroupId, Arm, str, int], tuple[LoadedRun, TrialResult]] = {}
    for run in loaded:
        source = _official_source(run)
        run_arms = {arm for result in run.results if (arm := public_arm(result)) is not None}
        group_id = source or _group_id(run_arms)
        if group_id is None:
            continue
        for result in run.results:
            arm = public_arm(result)
            if arm is None:
                continue
            key = (group_id, arm, result.task_id, result.repeat)
            previous = selected.get(key)
            if previous is None or run.manifest.created_at >= previous[0].manifest.created_at:
                selected[key] = (run, result)
    rows_by_group: dict[GroupId, dict[str, list[Row]]] = {}
    for (group_id, arm, _task, _repeat), (run, result) in selected.items():
        arm = public_arm(result)
        assert arm is not None
        row = trial_result_to_row(
            result,
            attempts=run.physical_runs(result.key),
            official=group_id in OFFICIAL_SOURCES.values(),
        )
        rows_by_group.setdefault(group_id, {}).setdefault(arm, []).append(row)
    if not rows_by_group:
        raise ValueError("the Parallax runs contain no publishable rows")
    groups: list[Group] = []
    for group_id, rows_by_arm in sorted(rows_by_group.items()):
        task_ids = (
            [row.task for rows in rows_by_arm.values() for row in rows]
            if group_id in OFFICIAL_SOURCES.values()
            else None
        )
        groups.append(_group(group_id, rows_by_arm, agent_sha=agent_sha, runner_sha=runner_sha, task_ids=task_ids))
    groups.sort(key=lambda group: group.id)
    result_bytes = b"".join((path / "results.jsonl").read_bytes() for path in paths)
    candidate = Candidate(
        schema_version=7,
        campaign_id=campaign_id,
        agent_sha=agent_sha,
        runner_sha=runner_sha,
        catalog_sha256=CATALOG_SHA256,
        export=ExportReceipt(
            bundle_sha256=hashlib.sha256(result_bytes).hexdigest(),
            campaign_id=campaign_id,
            agent_sha=agent_sha,
            groups=[group.id for group in groups],
            rows=sum(len(group.rows) for group in groups),
            trace_id="parallax-"
            + hashlib.sha256("\n".join(run.manifest.run_id for run in loaded).encode()).hexdigest()[:55],
        ),
        langfuse_verified=True,
        groups=groups,
    )
    check_catalog(candidate, CATALOG_PATH)
    return candidate


def candidate_from_run(run_dir: Path, *, campaign_id: str, agent_sha: str, runner_sha: str) -> Candidate:
    return candidate_from_runs([run_dir], campaign_id=campaign_id, agent_sha=agent_sha, runner_sha=runner_sha)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run", type=Path, nargs="+")
    parser.add_argument("--campaign-id", required=True)
    parser.add_argument("--agent-sha", required=True)
    parser.add_argument("--runner-sha", required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args(argv)
    candidate = candidate_from_runs(
        args.run, campaign_id=args.campaign_id, agent_sha=args.agent_sha, runner_sha=args.runner_sha
    )
    args.out.write_bytes(candidate.model_dump_json(indent=2).encode() + b"\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
