import json
from pathlib import Path

from fastbrowse_evals.recipes import TASKS, measure, table


def test_a_run_is_measured_from_its_result_and_trace(tmp_path: Path) -> None:
    result = {
        "status": "complete",
        "answer": "Born 10 December 1815",
        "steps": [{"decided_by": "recipe"}, {"decided_by": "jev"}, {"decided_by": "recipe"}],
        "cost": {"lines": [{"dollars": 0.01}, {"dollars": 0.002}, {"dollars": None}]},
    }
    (tmp_path / "replay.json").write_text(json.dumps(result))
    trace = [
        {"event": "call", "call": "jev"},
        {"event": "phase", "phase": "screenshot"},
        {"event": "call", "call": "jev"},
        {"event": "phase", "phase": "decide"},
    ]
    (tmp_path / "replay.jsonl").write_text("\n".join(json.dumps(event) for event in trace))
    (tmp_path / "replay.wall").write_text("12.5")
    measured = measure("t", "replay", tmp_path, "1815")
    assert (measured.calls, measured.screenshots, measured.steps, measured.from_recipe) == (2, 1, 3, 2)
    assert measured.dollars == 0.012
    assert measured.expected is True
    assert "| t | replay | complete | 12 | 0.0120 | 2 | 1 | 3 (2) | yes |" in table([measured])


def test_every_task_names_the_inputs_its_text_refers_to() -> None:
    for task in TASKS:
        assert set(task.inputs) == set(task.other_inputs)
        for name in task.inputs:
            assert f"input {name}" in task.task
