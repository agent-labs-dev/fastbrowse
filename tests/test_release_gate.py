"""`scripts/release_gate.py`: a release passes only when no task got worse and runs got faster and cheaper."""

import importlib.util
import sys
from pathlib import Path

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


def test_a_task_left_out_or_run_fewer_times_fails_but_a_new_version_is_only_noted() -> None:
    candidate = [row("a", True, 8, 0.01), row("b", True, 8, 0.01, version=2), row("b", True, 8, 0.01, version=2)]
    failures, notes = gate.compare(candidate, [*BASELINE, row("c", True)])
    assert failures == ["core/a: 1 runs, the baseline has 2", "core/c: not run"]
    assert "core/b: version 1 -> 2, not compared" in notes


def test_other_arms_are_not_the_release() -> None:
    candidate = [row(task, True, 8, 0.01) for task in ("a", "a", "b", "b")] + [
        row("a", False, 99, 9, arm="browser-use")
    ]
    failures, _ = gate.compare(candidate, BASELINE)
    assert failures == []
