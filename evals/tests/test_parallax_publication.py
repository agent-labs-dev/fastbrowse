import json
from datetime import UTC, datetime
from pathlib import Path

import pytest
from fastbrowse.evals.benchmarks import check_catalog
from fastbrowse.evals.catalog import CATALOG
from fastbrowse.evals.catalog import PATH as CATALOG_PATH
from parallax.models import (
    Area,
    CostSource,
    HarnessKind,
    Isolation,
    Maturity,
    RunManifest,
    Split,
    TrajectoryStatus,
    TrialResult,
    Usage,
    Verdict,
    WebIsolation,
)
from parallax.provenance import Provenance, write
from parallax.task import source_tasks

from fastbrowse_evals.arms import binding, invocations
from fastbrowse_evals.publication import (
    VERIFICATION_MARKER,
    candidate_from_run,
    candidate_from_runs,
    main,
    public_arm,
    trial_result_to_row,
)
from fastbrowse_evals.versions import task_version

AGENT_SHA = "a" * 40
RUNNER_SHA = "b" * 40


def result(
    task_id: str,
    repeat: int,
    *,
    attempt: int = 0,
    task_hash: str = "c" * 64,
    task_version: int = 1,
    details: dict[str, object] | None = None,
) -> TrialResult:
    return TrialResult(
        key=f"{task_id}-{repeat}",
        task_id=task_id,
        task_version=task_version,
        task_hash=task_hash,
        harness="fastbrowse",
        harness_version="test",
        harness_kind=HarnessKind.OPEN,
        config_hash="d" * 64,
        model="test/model",
        actual_models=("test/model",),
        repeat=repeat,
        verdict=Verdict.PASS,
        xfail=False,
        score=1.0,
        invalid_reason=None,
        na_reason=None,
        status=TrajectoryStatus.COMPLETED,
        checks=(),
        usage=(
            Usage(
                model="test/model",
                component="llm",
                input=10,
                output=20,
                requests=2,
                cost_usd=0.01,
                source=CostSource.HARNESS,
            ),
        ),
        cost_usd=0.01,
        cost_source=CostSource.HARNESS,
        boot_ms=100,
        ttft_ms=200,
        wall_ms=1_500,
        agent_ms=1_000,
        latency_split=None,
        judge=("test/judge",),
        judge_same_family=True,
        web_isolation=WebIsolation.NONE,
        maturity=Maturity.CANDIDATE,
        split=Split.TUNE,
        area=Area.BROWSER,
        lite=False,
        dir=None,
        attempt=attempt,
        timing_basis="session-ready-to-answer",
        timing_breakdown={"open": 250.0, "agent": 1_000.0, "close": 250.0},
        cost_by_component={"agent": 0.01},
        details=details or {"status": "complete"},
        judge_identity="browser-evidence-v1",
    )


def test_trial_result_to_row_keeps_typed_measurements_and_retries() -> None:
    first = result("pypi-version", 0)
    retry = result("pypi-version", 0, attempt=1)

    row = trial_result_to_row(first, attempts=(first, retry))

    assert row.task == "pypi-version"
    assert row.arm == "fastbrowse"
    assert row.completed is True
    assert row.seconds == 2.0
    assert row.dollars == 0.02
    assert row.physical_runs == 2
    assert row.input_tokens == 20
    assert row.output_tokens == 40
    assert row.timing_basis == "session-ready-to-answer"
    assert row.session_open_seconds == 0.5
    assert row.session_close_seconds == 0.5


def test_trial_result_to_row_maps_jev_and_llm_usage_separately() -> None:
    trial = result("pypi-version", 0).model_copy(
        update={
            "usage": (
                Usage(
                    model="compose",
                    component="llm",
                    input=10,
                    output=20,
                    requests=3,
                    cost_usd=0.01,
                    source=CostSource.HARNESS,
                ),
                Usage(
                    model="action",
                    component="jev",
                    input=4,
                    output=8,
                    requests=2,
                    cost_usd=0.002,
                    source=CostSource.HARNESS,
                ),
            )
        }
    )

    row = trial_result_to_row(trial)

    assert row.llm_calls == 3
    assert row.jev_calls == 2


def test_public_arm_maps_parallax_competitor_identity() -> None:
    cloud = result("pypi-version", 0).model_copy(update={"harness": "browser-use-cloud"})
    ultrafast = cloud.model_copy(update={"model": "bu-ultrafast"})
    diagnostic = cloud.model_copy(update={"harness": "cua-codex"})

    assert public_arm(cloud) == "browser-use"
    assert public_arm(ultrafast) == "browser-use-ultrafast"
    assert public_arm(diagnostic) is None


def test_arm_table_binds_public_ultrafast_to_its_harness_and_model() -> None:
    ultrafast = binding("browser-use-ultrafast")

    assert (ultrafast.harness, ultrafast.model) == ("browser-use-cloud", "bu-ultrafast")
    assert invocations(["browser-use-ultrafast"]) == (ultrafast,)


def test_status_mapping_keeps_hosted_terminal_outcomes_out_of_failure_statuses() -> None:
    for status in ("stuck", "needs_login"):
        terminal = result("pypi-version", 0, details={"status": status})
        row = trial_result_to_row(terminal)
        assert row.status == status
        assert row.completed is False
    stuck = result("pypi-version", 0, details={"status": "stuck"})
    invalid = stuck.model_copy(update={"verdict": Verdict.INVALID, "invalid_reason": "environment interruption: site"})
    with pytest.raises(ValueError, match="invalid has no public row"):
        trial_result_to_row(invalid)
    ungraded = stuck.model_copy(update={"verdict": Verdict.UNGRADED})
    with pytest.raises(ValueError, match="ungraded has no public row"):
        trial_result_to_row(ungraded, official=True)
    with pytest.raises(ValueError, match="not a valid Status"):
        trial_result_to_row(result("pypi-version", 0, details={"status": "unknown"}))


def test_missing_task_version_is_not_defaulted() -> None:
    with pytest.raises(KeyError, match="no pinned version"):
        task_version("missing-task", {})


def _support(run: Path, manifest: RunManifest) -> None:
    write(
        run,
        Provenance(
            parallax_head="p" * 40,
            task_hashes=dict(manifest.tasks),
            judge_policy=manifest.grading_policy,
            extra={
                "fastbrowse.agent_sha": AGENT_SHA,
                "fastbrowse.runner_sha": RUNNER_SHA,
                "fastbrowse.group": "internal-fastbrowse",
            },
        ),
    )
    (run / VERIFICATION_MARKER).write_text(json.dumps({"verified": True, "run_id": manifest.run_id}) + "\n")


def test_synthetic_parallax_run_projects_to_a_candidate(tmp_path: Path) -> None:
    tasks = [task for suite in CATALOG.suites.values() for task in suite]
    source_task_map = {task.id: task for task in source_tasks(source_names=("fastbrowse",))}
    run = tmp_path / "run"
    run.mkdir()
    manifest = RunManifest(
        run_id="synthetic-run",
        created_at=datetime(2026, 11, 13, tzinfo=UTC),
        parallax_version="2.0.0.dev22",
        harnesses=(),
        models=("test/model",),
        tasks=tuple((task.id, source_task_map[task.id].hash) for task in tasks),
        repeats=3,
        judge=("test/judge",),
        isolation=Isolation.HOST,
        selection="fastbrowse",
        grading_policy="browser-evidence-v1",
    )
    (run / "manifest.json").write_text(manifest.model_dump_json() + "\n")
    (run / "results.jsonl").write_text(
        "".join(
            result(
                task.id,
                repeat,
                task_hash=source_task_map[task.id].hash,
                task_version=source_task_map[task.id].meta.version,
            ).model_dump_json()
            + "\n"
            for task in tasks
            for repeat in range(3)
        )
    )
    _support(run, manifest)

    candidate = candidate_from_run(
        run,
        campaign_id="synthetic-campaign",
    )

    assert len(candidate.groups) == 1
    assert len(candidate.groups[0].rows) == 53 * 3
    assert len(candidate.groups[0].sources) == 1
    assert candidate.groups[0].sources[0].run_id == "synthetic-run"
    assert candidate.groups[0].sources[0].tasks == sorted(task.id for task in tasks)
    check_catalog(candidate, CATALOG_PATH)

    duplicate = candidate_from_runs([run, run], campaign_id="synthetic-campaign")
    assert len(duplicate.groups[0].rows) == 53 * 3

    out = tmp_path / "candidate.json"
    assert (
        main(
            [
                str(run),
                "--campaign-id",
                "synthetic-campaign",
                "--out",
                str(out),
            ]
        )
        == 0
    )
    assert out.read_text().startswith('{\n  "schema_version": 7')


def test_official_run_uses_upstream_score_and_execution_completion(tmp_path: Path) -> None:
    task_ids = [f"bu-bench-task-{index:03d}" for index in range(200)]
    run = tmp_path / "official"
    run.mkdir()
    manifest = RunManifest(
        run_id="official-run",
        created_at=datetime(2026, 11, 13, tzinfo=UTC),
        parallax_version="2.0.0.dev22",
        harnesses=(),
        models=("test/model",),
        tasks=tuple((task_id, "c" * 64) for task_id in task_ids),
        repeats=1,
        judge=("test/judge",),
        isolation=Isolation.HOST,
        selection="bu-bench",
        grading_policy="bu-bench-upstream-v1",
    )
    (run / "manifest.json").write_text(manifest.model_dump_json() + "\n")
    (run / "results.jsonl").write_text(
        "".join(
            result(task_id, 0)
            .model_copy(
                update={
                    "task_id": task_id,
                    "score": 0.75,
                    "verdict": Verdict.FAIL,
                    "judge": ("test/judge",),
                    "judge_identity": "bu-bench-upstream-v1:bu-bench:judge-pinned:model-test/model",
                }
            )
            .model_dump_json()
            + "\n"
            for task_id in task_ids
        )
    )
    _support(run, manifest)

    candidate = candidate_from_run(run, campaign_id="official-campaign")

    assert len(candidate.groups) == 1
    group = candidate.groups[0]
    assert group.id == "bu-bench"
    assert group.metric == "weighted-rubric"
    assert len(group.rows) == 200
    assert group.rows[0].score == 0.75
    assert group.rows[0].completed is True
    assert group.rows[0].status == "complete"
    check_catalog(candidate, CATALOG_PATH)


def test_official_groups_use_their_own_selected_task_ids(tmp_path: Path) -> None:
    def write_official_run(source: str, policy: str, count: int) -> Path:
        task_ids = [f"{source}-task-{index:03d}" for index in range(count)]
        run = tmp_path / source
        run.mkdir()
        manifest = RunManifest(
            run_id=f"{source}-run",
            created_at=datetime(2026, 11, 13, tzinfo=UTC),
            parallax_version="2.0.0.dev22",
            harnesses=(),
            models=("test/model",),
            tasks=tuple((task_id, "c" * 64) for task_id in task_ids),
            repeats=1,
            judge=("test/judge",),
            isolation=Isolation.HOST,
            selection=source,
            grading_policy=policy,
        )
        (run / "manifest.json").write_text(manifest.model_dump_json() + "\n")
        rows = []
        for task_id in task_ids:
            rows.append(
                result(task_id, 0)
                .model_copy(
                    update={
                        "task_id": task_id,
                        "score": 0.75 if policy == "bu-bench-upstream-v1" else 1.0,
                        "judge_identity": f"{policy}:judge-pinned:model-test/judge",
                    }
                )
                .model_dump_json()
            )
        (run / "results.jsonl").write_text("\n".join(rows) + "\n")
        _support(run, manifest)
        return run

    bu_run = write_official_run("bu-bench", "bu-bench-upstream-v1", 200)
    mind2web_run = write_official_run("online-mind2web", "online-mind2web-upstream-v1", 300)

    candidate = candidate_from_runs([bu_run, mind2web_run], campaign_id="official-campaign")

    groups = {group.id: group for group in candidate.groups}
    assert len(groups["bu-bench"].task_ids) == 200
    assert len(groups["online-mind2web"].task_ids) == 300
    assert set(groups["bu-bench"].task_ids).isdisjoint(groups["online-mind2web"].task_ids)
