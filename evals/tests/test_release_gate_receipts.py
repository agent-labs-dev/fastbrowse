"""The release gate accepts only the public rows the successful official producer exported."""

import importlib.util
import json
import sys
from pathlib import Path

import pytest

from fastbrowse_evals.publication import OfficialRows, official_collected

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("release_gate_receipts", ROOT / "scripts/release_gate.py")
assert spec and spec.loader
gate = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = gate
spec.loader.exec_module(gate)


def measured():
    raw = json.loads((ROOT / "docs/results/benchmark-candidate.json").read_text())
    template = raw["groups"][0]["rows"][0]
    parts = []
    for name, count in (("bu-bench", 200), ("online-mind2web", 300)):
        tasks = [f"task{i}" for i in range(count)]
        part = OfficialRows.model_validate(
            {
                "group": name,
                "arm": "fastbrowse",
                "measured_on": "2026-11-12",
                "agent_sha": raw["agent_sha"],
                "runner_sha": raw["runner_sha"],
                "dataset_sha256": "e" * 64,
                "task_ids": tasks,
                "repeats": 3,
                "upstream_commit": "c" * 40,
                "judge_model": "test/model",
                "judge_reasoning": "high",
                "score_threshold": 1,
                "judge_versions": {"test": "1"},
                "judge_pricing_note": None,
                "screenshot_timing": "test policy",
                "missing_captures": 0,
                "concurrency": 4,
                "browser": "local-chrome",
                "authorized": False,
                "langfuse_dataset": "test-dataset",
                "langfuse_run": "test-run",
                "rows": [{**template, "task": task, "repeat": repeat} for task in tasks for repeat in range(3)],
            }
        )
        parts.append(part)
        raw["groups"].append(official_collected(part).group.model_dump(mode="json"))
    return gate.official_candidate(raw, raw["campaign_id"], raw["agent_sha"]), parts


def receipts(monkeypatch, candidate, parts, **overrides):
    monkeypatch.setattr(gate, "_runs", lambda *_: [{"headSha": candidate.runner_sha, "databaseId": 1}])

    def gh(*args):
        if args[0] == "api":
            return json.dumps(
                {
                    "path": ".github/workflows/official-benchmarks.yml",
                    "event": "workflow_dispatch",
                    "conclusion": "success",
                    "status": "completed",
                    **overrides,
                }
            )
        directory = Path(args[args.index("-D") + 1])
        for part in parts:
            (directory / f"{part.group}.rows.json").write_text(part.model_dump_json())
        return ""

    monkeypatch.setattr(gate, "_gh", gh)


def test_matching_canonical_official_artifacts_prove_both_suites(monkeypatch):
    candidate, parts = measured()
    receipts(monkeypatch, candidate, parts)
    assert len(gate.official_receipts("o/r", candidate)) == 2


@pytest.mark.parametrize(
    "change", [{"path": "other.yml"}, {"event": "push"}, {"conclusion": "failure"}, {"status": "in_progress"}]
)
def test_wrong_producer_metadata_cannot_supply_receipts(monkeypatch, change):
    candidate, parts = measured()
    receipts(monkeypatch, candidate, parts, **change)
    with pytest.raises(gate.GateError, match="matching public receipts"):
        gate.official_receipts("o/r", candidate)


@pytest.mark.parametrize(
    "field,value",
    [("agent_sha", "d" * 40), ("runner_sha", "d" * 40), ("dataset_sha256", "d" * 64), ("upstream_commit", "d" * 40)],
)
def test_unrelated_source_cannot_supply_matching_receipts(monkeypatch, field, value):
    candidate, parts = measured()
    parts[0] = parts[0].model_copy(update={field: value})
    receipts(monkeypatch, candidate, parts)
    with pytest.raises(gate.GateError, match="matching public receipts"):
        gate.official_receipts("o/r", candidate)


def test_same_count_with_other_task_ids_cannot_match_original_artifact(monkeypatch):
    candidate, parts = measured()
    group = candidate.groups[-2]
    group.task_ids[0] = "foreign"
    for row in group.rows:
        if row.task == "task0":
            row.task = "foreign"
    receipts(monkeypatch, candidate, parts)
    with pytest.raises(gate.GateError, match="matching public receipts"):
        gate.official_receipts("o/r", candidate)


def test_split_official_runs_keep_explicit_harness_pin_independent_of_workflow_head(monkeypatch):
    candidate, parts = measured()
    monkeypatch.setattr(
        gate,
        "_runs",
        lambda *_: [
            {"databaseId": 1, "headSha": "different-head"},
            {"databaseId": 2, "headSha": "other-head"},
        ],
    )

    def gh(*args):
        if args[0] == "api":
            return json.dumps(
                {
                    "path": ".github/workflows/official-benchmarks.yml",
                    "event": "workflow_dispatch",
                    "conclusion": "success",
                    "status": "completed",
                }
            )
        part = parts[int(args[2]) - 1]
        directory = Path(args[args.index("-D") + 1])
        (directory / f"{part.group}.rows.json").write_text(part.model_dump_json())
        return ""

    monkeypatch.setattr(gate, "_gh", gh)
    assert gate.official_receipts("o/r", candidate) == [
        "bu-bench: verified official producer run 1",
        "online-mind2web: verified official producer run 2",
    ]
