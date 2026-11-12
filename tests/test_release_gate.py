"""`scripts/release_gate.py`: a release passes only when no task got worse and runs got faster and cheaper."""

import importlib.util
import sys
from pathlib import Path

import pytest

_spec = importlib.util.spec_from_file_location(
    "release_gate", Path(__file__).parent.parent / "scripts" / "release_gate.py"
)
assert _spec and _spec.loader
gate = importlib.util.module_from_spec(_spec)
sys.modules["release_gate"] = gate
_spec.loader.exec_module(gate)


def row(
    task: str, passed: bool, seconds: float = 10.0, dollars: float = 0.02, version: int = 1, arm: str = "fastbrowse"
):
    return {
        "arm": arm,
        "suite": "core",
        "task": task,
        "task_version": version,
        "passed": passed,
        "seconds": seconds,
        "dollars": dollars,
    }


BASELINE = [row("a", True), row("a", False), row("b", True), row("b", True)]


def test_a_faster_cheaper_run_with_no_task_worse_passes() -> None:
    candidate = [row("a", True, 8, 0.01), row("a", True, 8, 0.01), row("b", True, 8, 0.01), row("b", True, 8, 0.01)]
    failures, _ = gate.compare(candidate, BASELINE)
    assert failures == []


def test_a_task_that_passes_less_often_fails_the_release() -> None:
    candidate = [row("a", True, 8, 0.01), row("a", False, 8, 0.01), row("b", True, 8, 0.01), row("b", False, 8, 0.01)]
    failures, _ = gate.compare(candidate, BASELINE)
    assert failures == ["core/b: passed 1/2, the baseline 2/2"]


def test_a_run_no_faster_or_cheaper_fails_the_release() -> None:
    failures, _ = gate.compare(BASELINE, BASELINE)
    assert [failure.split()[0] for failure in failures] == ["median", "mean"]


def test_a_task_left_out_fails_but_a_new_version_is_only_noted() -> None:
    candidate = [row("a", True, 8, 0.01), row("b", True, 8, 0.01, version=2), row("b", True, 8, 0.01, version=2)]
    failures, notes = gate.compare(candidate, [*BASELINE, row("c", True)])
    assert failures == ["core/c: not run"]
    assert "core/b: version 1 -> 2, not compared" in notes


def test_each_task_is_scored_on_the_newest_campaign_run_that_measured_it_three_times(monkeypatch) -> None:
    def measured(sha: str, task: str, passed: bool, times: int = 3):
        return [{**row(task, passed), "run": {"git_sha": sha}} for _ in range(times)]

    ledgers = {
        3: measured("cccccccc", "a", True, times=1),  # a smoke: too few runs to score
        2: measured("bbbbbbbb", "a", True),  # the targeted rerun of a fix
        1: [*measured("aaaaaaaa", "a", False), *measured("aaaaaaaa", "b", True)],  # the full campaign
        0: [{**r, "task_version": 0} for r in measured("99999999", "b", False)],  # b before its task changed
    }
    titles = {4: "release-9.9.9", 3: "release-1.0.0", 2: "release-1.0.0", 1: "release-1.0.0", 0: "release-1.0.0"}
    runs = [{"databaseId": i, "displayTitle": f"Benchmark campaign {name}"} for i, name in titles.items()]
    monkeypatch.setattr(gate, "_runs", lambda *_: runs)
    monkeypatch.setattr(gate, "ledger_rows", lambda _, run_id: ledgers[run_id])
    rows, used = gate.campaign_rows("o/r", "release-1.0.0")
    assert {(r["task"], r["run"]["git_sha"], r["passed"]) for r in rows} == {
        ("a", "bbbbbbbb", True),
        ("b", "aaaaaaaa", True),
    }
    assert used == ["run 2 on bbbbbbbb: 1 tasks", "run 1 on aaaaaaaa: 1 tasks"]


def test_other_arms_are_not_the_release() -> None:
    candidate = [row(task, True, 8, 0.01) for task in ("a", "a", "b", "b")] + [
        row("a", False, 99, 9, arm="browser-use")
    ]
    failures, _ = gate.compare(candidate, BASELINE)
    assert failures == []


@pytest.mark.parametrize("side", ["candidate", "baseline"])
@pytest.mark.parametrize("receipt", [{"dollars": None}, {"dollars": 0.0, "unknown_cost": True}])
def test_an_unpriced_attempt_cannot_prove_a_cheaper_release(side, receipt) -> None:
    candidate = [row(task, True, 8, 0.01) for task in ("a", "a", "b", "b")]
    baseline = [dict(r) for r in BASELINE]
    (candidate if side == "candidate" else baseline)[0].update(receipt)
    failures, notes = gate.compare(candidate, baseline)
    assert any(side in failure and "dollars" in failure for failure in failures)
    assert not any(note.startswith("mean $") for note in notes)


def test_a_known_zero_cost_is_still_a_valid_receipt() -> None:
    candidate = [row(task, True, 8, 0) for task in ("a", "a", "b", "b")]
    failures, notes = gate.compare(candidate, BASELINE)
    assert failures == []
    assert any(note.startswith("mean $ 0,") for note in notes)


@pytest.mark.parametrize("side", ["candidate", "baseline"])
@pytest.mark.parametrize("field", ["seconds", "dollars"])
@pytest.mark.parametrize("value", [True, "0.001", float("nan"), float("inf"), -1])
def test_invalid_measurements_cannot_pass_the_release(side, field, value) -> None:
    candidate = [row(task, True, 8, 0.01) for task in ("a", "a", "b", "b")]
    baseline = [dict(r) for r in BASELINE]
    (candidate if side == "candidate" else baseline)[0][field] = value
    failures, _ = gate.compare(candidate, baseline)
    assert any(side in failure and field in failure for failure in failures)


@pytest.mark.parametrize("field", ["seconds", "dollars"])
def test_a_missing_measurement_cannot_pass_the_release(field) -> None:
    candidate = [row(task, True, 8, 0.01) for task in ("a", "a", "b", "b")]
    candidate[0].pop(field)
    failures, _ = gate.compare(candidate, BASELINE)
    assert any("candidate" in failure and field in failure for failure in failures)


def test_an_empty_baseline_cannot_prove_an_improvement() -> None:
    failures, _ = gate.compare(BASELINE, [])
    assert failures == [
        "baseline seconds: missing, unknown or invalid measurements",
        "baseline dollars: missing, unknown or invalid measurements",
    ]


def test_release_speed_never_compares_mixed_clocks():
    baseline = [{**r, "timing_basis": "session-ready-to-answer"} for r in BASELINE]
    candidate = [{**row(r["task"], True, 8, 0.01), "timing_basis": "session-ready-to-answer"} for r in BASELINE]
    candidate[0]["timing_basis"] = "physical-attempt-wall-time"
    failures, notes = gate.compare(candidate, baseline)
    assert failures == ["median seconds cannot compare different timing bases"]
    assert not any(note.startswith("median seconds") for note in notes)


def test_matched_physical_clocks_keep_all_elapsed_time():
    baseline = [{**r, "timing_basis": "physical-attempt-wall-time"} for r in BASELINE]
    candidate = [{**row(r["task"], True, 8, 0.01), "timing_basis": "physical-attempt-wall-time"} for r in BASELINE]
    candidate[0]["seconds"] = 340.3
    failures, _ = gate.compare(candidate, baseline)
    assert failures == []


def test_release_speed_requires_legacy_clock_to_match():
    candidate = [{**r, "dollars": 0.01, "timing_basis": "session-ready-to-answer"} for r in BASELINE]
    failures, _ = gate.compare(candidate, BASELINE)
    assert failures == ["median seconds cannot compare different timing bases"]
