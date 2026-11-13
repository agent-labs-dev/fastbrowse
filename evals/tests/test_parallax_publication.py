from datetime import UTC, datetime
from pathlib import Path

from fastbrowse.evals.benchmarks import check_catalog
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

from fastbrowse_evals.publication import candidate_from_run, candidate_from_runs, main, public_arm, trial_result_to_row

AGENT_SHA = "a" * 40
RUNNER_SHA = "b" * 40


def result(task_id: str, repeat: int, *, attempt: int = 0) -> TrialResult:
    return TrialResult(
        key=f"{task_id}-{repeat}",
        task_id=task_id,
        task_version=1,
        task_hash="c" * 64,
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
        judge=("browser-evidence-v1",),
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
        details={"status": "done"},
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


def test_public_arm_maps_parallax_competitor_identity() -> None:
    cloud = result("pypi-version", 0).model_copy(update={"harness": "browser-use-cloud"})
    ultrafast = cloud.model_copy(update={"model": "bu-ultrafast"})
    diagnostic = cloud.model_copy(update={"harness": "cua-codex"})

    assert public_arm(cloud) == "browser-use"
    assert public_arm(ultrafast) == "browser-use-ultrafast"
    assert public_arm(diagnostic) is None


def test_synthetic_parallax_run_projects_to_a_candidate(tmp_path: Path) -> None:
    import fastbrowse.evals.catalog as catalog

    tasks = [task for suite in catalog.CATALOG.suites.values() for task in suite]
    run = tmp_path / "run"
    run.mkdir()
    manifest = RunManifest(
        run_id="synthetic-run",
        created_at=datetime(2026, 11, 13, tzinfo=UTC),
        parallax_version="2.0.0.dev20",
        harnesses=(),
        models=("test/model",),
        tasks=tuple((task.id, "c" * 64) for task in tasks),
        repeats=3,
        judge=("browser-evidence-v1",),
        isolation=Isolation.HOST,
        selection="fastbrowse",
        grading_policy="browser-evidence-v1",
    )
    (run / "manifest.json").write_text(manifest.model_dump_json() + "\n")
    (run / "results.jsonl").write_text(
        "".join(result(task.id, repeat).model_dump_json() + "\n" for task in tasks for repeat in range(3))
    )

    candidate = candidate_from_run(
        run,
        campaign_id="synthetic-campaign",
        agent_sha=AGENT_SHA,
        runner_sha=RUNNER_SHA,
    )

    assert len(candidate.groups) == 1
    assert len(candidate.groups[0].rows) == 53 * 3
    assert candidate.groups[0].sources == []
    check_catalog(candidate, CATALOG_PATH)

    duplicate = candidate_from_runs(
        [run, run], campaign_id="synthetic-campaign", agent_sha=AGENT_SHA, runner_sha=RUNNER_SHA
    )
    assert len(duplicate.groups[0].rows) == 53 * 3

    out = tmp_path / "candidate.json"
    assert (
        main(
            [
                str(run),
                "--campaign-id",
                "synthetic-campaign",
                "--agent-sha",
                AGENT_SHA,
                "--runner-sha",
                RUNNER_SHA,
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
        parallax_version="2.0.0.dev20",
        harnesses=(),
        models=("test/model",),
        tasks=tuple((task_id, "c" * 64) for task_id in task_ids),
        repeats=1,
        judge=("bu-bench-upstream-v1",),
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
                    "judge": ("bu-bench-upstream-v1",),
                    "judge_identity": "bu-bench-upstream-v1:bu-bench:judge-pinned:model-test/model",
                }
            )
            .model_dump_json()
            + "\n"
            for task_id in task_ids
        )
    )

    candidate = candidate_from_run(run, campaign_id="official-campaign", agent_sha=AGENT_SHA, runner_sha=RUNNER_SHA)

    assert len(candidate.groups) == 1
    group = candidate.groups[0]
    assert group.id == "bu-bench"
    assert group.metric == "weighted-rubric"
    assert len(group.rows) == 200
    assert group.rows[0].score == 0.75
    assert group.rows[0].completed is True
    assert group.rows[0].status == "complete"
    check_catalog(candidate, CATALOG_PATH)
