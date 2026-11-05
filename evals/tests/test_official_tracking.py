import json
from pathlib import Path

import pytest

from fastbrowse_evals.official_tracking import project


def saved(folder: Path, *, suite: str = "bu-bench", **over: str) -> None:
    manifest = {
        **over,
        "suite": suite,
        "published": False,
        "task_ids": ["t1", "t2"],
        "upstream_commit": "a" * 40,
        "dataset_sha256": "b" * 64,
        "config_sha256": "d" * 64,
        "judge_model": "recorded-judge",
        "judge_reasoning": "xhigh",
        "score_threshold": 3,
        "run": {"run_started": "2026-10-07T09:00:00+00:00", "git_sha": "c" * 40},
    }
    (folder / "manifest.json").write_text(json.dumps(manifest))
    (folder / "attempts.jsonl").write_text(
        "\n".join(json.dumps({"task": t, "repeat": 0, "started": True}) for t in ["t1", "t2"])
    )
    (folder / "results.jsonl").write_text(
        json.dumps(
            {"task": "t1", "repeat": 0, "upstream_score": 0.75, "answer": "private task answer", "completed": True}
        )
    )


def test_partial_rubric_scores_and_interrupted_runs_survive_without_becoming_pass_rates(tmp_path: Path) -> None:
    saved(tmp_path)
    run = project(tmp_path)
    scores = {s.name: s.value for s in run.runs[0].scores}
    assert scores["weighted-rubric-score"] == 0.75
    assert scores["graded_source_runs"] == scores["ungraded_source_runs"] == 1
    assert not any(s.name == "pass_rate" for s in run.runs[0].scores)
    assert all(trace.output.verdicts == [] for trace in run.runs[0].traces)
    assert "private task answer" not in repr(run)


def test_a_run_is_tracked_as_the_arm_that_ran(tmp_path: Path) -> None:
    saved(tmp_path, arm="browser-use")
    run = project(tmp_path).runs[0]
    assert run.name.startswith("bu-bench/browser-use/") and run.row == run.harness == "browser-use"
    assert {trace.version for trace in run.traces} == {"browser-use"}


def test_a_manifest_older_than_its_arm_field_was_a_fastbrowse_run(tmp_path: Path) -> None:
    saved(tmp_path)
    assert project(tmp_path).runs[0].name.startswith("bu-bench/fastbrowse/")


def test_source_identity_prevents_pairing_different_judges(tmp_path: Path) -> None:
    saved(tmp_path)
    first = project(tmp_path)
    path = tmp_path / "manifest.json"
    manifest = json.loads(path.read_text()) | {"judge_model": "another-judge"}
    path.write_text(json.dumps(manifest))
    second = project(tmp_path)
    assert first.items[0].id != second.items[0].id


def test_result_without_a_physical_run_is_refused(tmp_path: Path) -> None:
    saved(tmp_path)
    (tmp_path / "attempts.jsonl").write_text("")
    with pytest.raises(ValueError, match="physical attempt"):
        project(tmp_path)


def test_known_agent_cost_is_preserved_without_claiming_a_settled_total(tmp_path: Path) -> None:
    saved(tmp_path)
    path = tmp_path / "results.jsonl"
    path.write_text(json.dumps(json.loads(path.read_text()) | {"agent_cost_unknown": False, "agent_dollars": 0.2}))
    run = project(tmp_path)
    first = run.runs[0].traces[0].output
    assert "agent-cost-unknown:False" in first.repeats[0].flags
    assert first.cost_usd is None
    assert {s.name: s.value for s in run.runs[0].scores}["known_agent_charges_usd"] == 0.2


def test_missing_capture_is_recorded_without_changing_the_source_grade(tmp_path: Path) -> None:
    saved(tmp_path)
    path = tmp_path / "results.jsonl"
    path.write_text(json.dumps(json.loads(path.read_text()) | {"final_capture_available": False}))
    run = project(tmp_path)
    metrics = {s.name: s.value for s in run.runs[0].traces[0].scores}
    assert metrics["runs_without_final_capture"] == 1.0
    assert metrics["weighted-rubric-score"] == 0.75
