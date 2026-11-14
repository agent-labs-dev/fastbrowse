"""Project Parallax run directories into Fastbrowse's unchanged public candidate schema."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import re
from collections.abc import Iterable
from pathlib import Path
from typing import Any, Literal, assert_never, cast

from fastbrowse.evals.benchmarks import (
    COVERAGE,
    Arm,
    Candidate,
    ExcludedTask,
    ExportReceipt,
    Group,
    GroupId,
    Row,
    Source,
    UnjudgedSlot,
    check_catalog,
    group_arms,
)
from fastbrowse.evals.catalog import CATALOG_SHA256, SUITES
from fastbrowse.evals.catalog import PATH as CATALOG_PATH
from fastbrowse.models import Status
from parallax.models import ENVIRONMENT_INTERRUPTION, TrajectoryStatus, TrialResult, Verdict
from parallax.provenance import read as read_provenance
from parallax.task import source_tasks
from parallax.track.rows import LoadedRun, load_run

from fastbrowse_evals.arms import public_arm as public_arm_for

OFFICIAL_SOURCES: dict[str, GroupId] = {
    "bu-bench": "bu-bench",
    "online-mind2web": "online-mind2web",
}
EXPECTED_POLICIES = {"bu-bench": "bu-bench-upstream-v1", "online-mind2web": "online-mind2web-upstream-v1"}
VERIFICATION_MARKER = "parallax-verify.json"


def _official_source(run: LoadedRun) -> GroupId | None:
    selected = {term.removeprefix("source=") for term in run.manifest.selection.split()}
    if source := next((OFFICIAL_SOURCES.get(term) for term in selected if term in OFFICIAL_SOURCES), None):
        return source
    return next(
        (
            group_id
            for source, group_id in OFFICIAL_SOURCES.items()
            if any(result.task_id.startswith(f"{source}-") for result in run.results)
        ),
        None,
    )


def public_arm(result: TrialResult) -> Arm | None:
    arm = public_arm_for(result.harness, result.model)
    supported = {
        "fastbrowse",
        "fastbrowse-pool",
        "browser-use",
        "browser-use-ultrafast",
        "browser-harness",
        "browser-use-oss",
    }
    return cast(Arm | None, arm if arm in supported else None)


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


def _usage(results: Iterable[TrialResult]) -> tuple[int | None, int, int, int, float]:
    usages = [usage for result in results for usage in result.usage]
    if not usages:
        return 0, 0, 0, 0, 0.0
    jev_calls = sum(usage.requests for usage in usages if usage.component == "jev")
    llm_calls = sum(usage.requests for usage in usages if usage.component == "llm")
    input_tokens = sum(usage.input for usage in usages)
    output_tokens = sum(usage.output for usage in usages)
    agent_ms = sum(result.agent_ms or 0 for result in results)
    throughput = output_tokens / (agent_ms / 1000) if agent_ms > 0 else 0.0
    return llm_calls, jev_calls, input_tokens, output_tokens, throughput


def _failure(result: TrialResult) -> str | None:
    value = result.invalid_reason or result.na_reason
    if not value:
        return None
    value = re.sub(r"[^A-Za-z0-9 ,.;:()'%+_=-]", " ", value)
    return value[:240].strip() or "trial did not complete"


def _status(result: TrialResult) -> Status:
    raw = result.details.get("status")
    if isinstance(raw, str):
        return Status(raw)
    match result.status:
        case TrajectoryStatus.COMPLETED:
            return Status.COMPLETE
        case TrajectoryStatus.OVER_BUDGET:
            return Status.BUDGET_EXCEEDED
        case TrajectoryStatus.HARNESS_ERROR | TrajectoryStatus.BOOT_ERROR:
            return Status.ERROR
        case TrajectoryStatus.TIMEOUT:
            return Status.STUCK
        case None:
            return Status.ERROR
        case other:
            assert_never(other)


def _details_sum(attempts: Iterable[TrialResult], name: str) -> float | None:
    values = [value for result in attempts if isinstance((value := result.details.get(name)), int | float)]
    return math.fsum(float(value) for value in values) if values else None


def trial_result_to_row(result: TrialResult, *, attempts: Iterable[TrialResult] = (), official: bool = False) -> Row:
    """Map one graded Parallax result and its physical attempts to one public row."""
    if result.verdict not in {Verdict.PASS, Verdict.FAIL}:
        raise ValueError(f"{result.task_id}: {result.verdict.value} has no public row")
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
    if close_seconds is None:
        pool_close = _details_sum(physical, "pool_close_seconds")
        close_seconds = pool_close
    llm_calls, jev_calls, input_tokens, output_tokens, throughput = _usage(physical)
    status = _status(result)
    if not official and result.verdict is Verdict.FAIL and status is Status.COMPLETE:
        status = Status.UNVERIFIED
    completed = status is Status.COMPLETE
    return Row(
        task=result.task_id,
        repeat=result.repeat,
        arm=arm,
        status=status.value,
        completed=completed,
        score=_number(result.score) or 0.0 if official else float(result.verdict is Verdict.PASS),
        seconds=seconds,
        dollars=_sum_cost(physical),
        physical_runs=len(physical),
        llm_calls=llm_calls,
        llm_calls_basis="physical_requests" if llm_calls is not None else None,
        jev_calls=jev_calls,
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


def _required_task_ids(group_id: GroupId) -> list[str]:
    if group_id in OFFICIAL_SOURCES.values():
        raise ValueError("official task ids come from the upstream run")
    arms = group_arms(group_id, 7)
    partial = COVERAGE[group_id].partial
    return sorted(task.id for task in _catalog_tasks() if all(arm in task.arms for arm in arms if arm not in partial))


def _group(
    group_id: GroupId,
    rows_by_arm: dict[str, list[Row]],
    unjudged_by_arm: dict[str, list[UnjudgedSlot]],
    excluded: list[ExcludedTask],
    *,
    agent_sha: str,
    runner_sha: str,
    task_ids: Iterable[str],
    sources: list[Source],
) -> Group:
    task_ids = sorted(set(task_ids))
    arms = group_arms(group_id, 7)
    rows = [row for arm, arm_rows in rows_by_arm.items() if arm in arms for row in arm_rows if row.task in task_ids]
    unjudged = [
        slot for arm, slots in unjudged_by_arm.items() if arm in arms for slot in slots if slot.task in task_ids
    ]
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
        unjudged=sorted(unjudged, key=lambda item: (item.arm, item.task, item.repeat)),
        excluded=sorted(excluded, key=lambda item: (item.arm, item.task)),
        sources=sources,
    )


def _metric(group_id: GroupId) -> Literal["task-success", "weighted-rubric", "full-task-success"]:
    if group_id == "bu-bench":
        return "weighted-rubric"
    if group_id == "online-mind2web":
        return "full-task-success"
    return "task-success"


def _sources(
    group_id: GroupId,
    selected: dict[tuple[GroupId, Arm, str, int], tuple[LoadedRun, TrialResult]],
    metadata: dict[Path, tuple[str, str, dict[str, str]]],
) -> list[Source]:
    grouped: dict[tuple[Path, Arm], list[TrialResult]] = {}
    for (result_group, arm, _task, _repeat), (run, result) in selected.items():
        if result_group != group_id or result.verdict is Verdict.NA:
            continue
        grouped.setdefault((run.path, arm), []).append(result)
    sources: list[Source] = []
    for (path, arm), results in grouped.items():
        run = next(run for run, _ in selected.values() if run.path == path)
        agent_sha, runner_sha, _ = metadata[path]
        versions = {result.harness_version for result in results if result.harness_version}
        if arm not in {"fastbrowse", "fastbrowse-pool"} and len(versions) != 1:
            raise ValueError(f"{path}: {arm} has no single recorded harness version")
        models = {model.replace("/", ":") for result in results for model in result.actual_models if model}
        sources.append(
            Source(
                run_id=run.manifest.run_id,
                arm=arm,
                agent_sha=agent_sha,
                runner_sha=runner_sha,
                measured_on=run.manifest.created_at.date().isoformat(),
                tasks=sorted({result.task_id for result in results}),
                version=None if arm in {"fastbrowse", "fastbrowse-pool"} else next(iter(versions)),
                models=sorted(models),
            )
        )
    return sorted(sources, key=lambda source: (source.measured_on, source.run_id, source.arm), reverse=True)


def _provenance(run: LoadedRun) -> tuple[str, str, dict[str, str]]:
    provenance = read_provenance(run.path)
    agent_sha = provenance.extra.get("fastbrowse.agent_sha")
    runner_sha = provenance.extra.get("fastbrowse.runner_sha")
    if not agent_sha or not runner_sha:
        raise ValueError(f"{run.path}: provenance has no fastbrowse agent and runner SHA")
    if not re.fullmatch(r"[0-9a-f]{40}", agent_sha) or not re.fullmatch(r"[0-9a-f]{40}", runner_sha):
        raise ValueError(f"{run.path}: provenance contains an invalid build SHA")
    return agent_sha, runner_sha, provenance.extra


def _verify_upload(run: LoadedRun) -> bool:
    marker = run.path / VERIFICATION_MARKER
    if not marker.is_file():
        raise ValueError(f"{run.path}: no Parallax verification result was preserved")
    data = json.loads(marker.read_text())
    if data.get("verified") is not True or data.get("run_id") != run.manifest.run_id:
        raise ValueError(f"{run.path}: verification result does not match the run")
    return True


def _validate_run(run: LoadedRun, source: GroupId | None) -> None:
    expected_policy = "browser-evidence-v1" if source is None else EXPECTED_POLICIES[source]
    policy_parts = set(run.manifest.grading_policy.split(":"))
    if expected_policy not in policy_parts:
        raise ValueError(f"{run.path}: grading policy differs from the catalog")
    provenance = read_provenance(run.path)
    if provenance.judge_policy != run.manifest.grading_policy:
        raise ValueError(f"{run.path}: provenance judge policy differs from the run manifest")
    identities = {result.judge_identity for result in run.results if result.verdict in {Verdict.PASS, Verdict.FAIL}}
    if len(identities) != 1 or None in identities:
        raise ValueError(f"{run.path}: selected rows do not share one judging identity")
    identity = next(iter(identities))
    assert identity is not None
    if identity != expected_policy and not identity.startswith(expected_policy + ":"):
        raise ValueError(f"{run.path}: judging identity differs from the catalog policy")
    for result in run.results:
        manifest_hash = dict(run.manifest.tasks).get(result.task_id)
        if manifest_hash != result.task_hash:
            raise ValueError(f"{run.path}: {result.task_id} task hash differs from the manifest")
        if source is None:
            catalog_task = next((task for task in _catalog_tasks() if task.id == result.task_id), None)
            if catalog_task is None or result.task_version != catalog_task.task_version:
                raise ValueError(f"{run.path}: {result.task_id} task version differs from the catalog")
    if source is None:
        expected = {task.id: (task.meta.version, task.hash) for task in source_tasks(source_names=("fastbrowse",))}
        for result in run.results:
            if result.task_id not in expected:
                raise ValueError(f"{run.path}: {result.task_id} is not in the fastbrowse task source")
            version, task_hash = expected[result.task_id]
            if (result.task_version, result.task_hash) != (version, task_hash):
                raise ValueError(f"{run.path}: {result.task_id} task identity differs from the source")


def candidate_from_runs(run_dirs: Iterable[Path], *, campaign_id: str) -> Candidate:
    """Build schema 7 groups from Parallax `parallax.result/2` run directories."""
    paths = tuple(run_dirs)
    loaded = [load_run(path) for path in paths]
    if not loaded:
        raise ValueError("no Parallax run directories were supplied")
    metadata: dict[Path, tuple[str, str, dict[str, str]]] = {}
    selected: dict[tuple[GroupId, Arm, str, int], tuple[LoadedRun, TrialResult]] = {}
    for run in loaded:
        source = _official_source(run)
        _validate_run(run, source)
        verified = _verify_upload(run)
        run_agent, run_runner, extra = _provenance(run)
        metadata[run.path] = (run_agent, run_runner, {**extra, "parallax.verified": str(verified).lower()})
        group_id = source or cast(GroupId | None, extra.get("fastbrowse.group"))
        if group_id not in {
            "internal-fastbrowse",
            "internal-hosted",
            "internal-ultrafast",
            "internal-backend",
            *OFFICIAL_SOURCES.values(),
        }:
            raise ValueError(f"{run.path}: provenance does not name a supported benchmark group")
        for result in run.results:
            arm = public_arm(result)
            if arm is None:
                continue
            key = (group_id, arm, result.task_id, result.repeat)
            previous = selected.get(key)
            if previous is None or run.manifest.created_at >= previous[0].manifest.created_at:
                selected[key] = (run, result)
    if not selected:
        raise ValueError("the Parallax runs contain no public arms")
    measured_agent = next(iter(metadata.values()))[0]
    measured_runner = next(iter(metadata.values()))[1]
    if any(item[0] != measured_agent or item[1] != measured_runner for item in metadata.values()):
        raise ValueError("all runs in one candidate must name the same measured build and runner")
    rows_by_group: dict[GroupId, dict[str, list[Row]]] = {}
    unjudged_by_group: dict[GroupId, dict[str, list[UnjudgedSlot]]] = {}
    excluded_by_group: dict[GroupId, list[ExcludedTask]] = {}
    task_ids_by_group: dict[GroupId, set[str]] = {}
    official_task_ids = {
        official_group: {
            selected_result.task_id
            for (selected_group, _selected_arm, _selected_task, _selected_repeat), (
                selected_run,
                selected_result,
            ) in selected.items()
            if selected_group == official_group and _official_source(selected_run) == official_group
        }
        for official_group in OFFICIAL_SOURCES.values()
    }
    for (group_id, arm, _task, _repeat), (run, result) in selected.items():
        task_ids = (
            official_task_ids[group_id] if group_id in OFFICIAL_SOURCES.values() else set(_required_task_ids(group_id))
        )
        task_ids_by_group.setdefault(group_id, set()).update(task_ids)
        if result.task_id not in task_ids:
            continue
        if result.verdict in {Verdict.PASS, Verdict.FAIL}:
            row = trial_result_to_row(
                result,
                attempts=run.physical_runs(result.key),
                official=group_id in OFFICIAL_SOURCES.values(),
            )
            rows_by_group.setdefault(group_id, {}).setdefault(arm, []).append(row)
        elif result.verdict is Verdict.NA:
            reason = _failure(result) or "Parallax marked this task not applicable"
            excluded_by_group.setdefault(group_id, []).append(ExcludedTask(arm=arm, task=result.task_id, reason=reason))
        else:
            why = (
                "evidence-unavailable"
                if result.invalid_reason and result.invalid_reason.startswith(ENVIRONMENT_INTERRUPTION)
                else "no-verdict"
            )
            unjudged_by_group.setdefault(group_id, {}).setdefault(arm, []).append(
                UnjudgedSlot(task=result.task_id, repeat=result.repeat, arm=arm, reason=why)
            )
    groups: list[Group] = []
    for group_id, rows_by_arm in sorted(rows_by_group.items()):
        group = _group(
            group_id,
            rows_by_arm,
            unjudged_by_group.get(group_id, {}),
            excluded_by_group.get(group_id, []),
            agent_sha=measured_agent,
            runner_sha=measured_runner,
            task_ids=task_ids_by_group[group_id],
            sources=_sources(group_id, selected, metadata),
        )
        groups.append(group)
    if not groups:
        raise ValueError("the Parallax runs contain no publishable rows")
    result_bytes = b"".join((path / "results.jsonl").read_bytes() for path in paths)
    verified = all(extra.get("parallax.verified") == "true" for _, _, extra in metadata.values())
    if not verified:
        raise ValueError("all Parallax run directories must carry a successful verification result")
    candidate = Candidate(
        schema_version=7,
        campaign_id=campaign_id,
        agent_sha=measured_agent,
        runner_sha=measured_runner,
        catalog_sha256=CATALOG_SHA256,
        export=ExportReceipt(
            bundle_sha256=hashlib.sha256(result_bytes).hexdigest(),
            campaign_id=campaign_id,
            agent_sha=measured_agent,
            groups=[group.id for group in groups],
            rows=sum(len(group.rows) for group in groups),
            trace_id="parallax-"
            + hashlib.sha256("\n".join(run.manifest.run_id for run in loaded).encode()).hexdigest()[:55],
        ),
        langfuse_verified=verified,
        groups=groups,
    )
    check_catalog(candidate, CATALOG_PATH)
    return candidate


def candidate_from_run(run_dir: Path, *, campaign_id: str) -> Candidate:
    return candidate_from_runs([run_dir], campaign_id=campaign_id)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run", type=Path, nargs="+")
    parser.add_argument("--campaign-id", required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args(argv)
    candidate = candidate_from_runs(args.run, campaign_id=args.campaign_id)
    args.out.write_bytes(candidate.model_dump_json(indent=2).encode() + b"\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
