import runpy
from pathlib import Path

import pytest

from fastbrowse_evals import catalog, live
from fastbrowse_evals.live_tasks import TASKS

WORKER = runpy.run_path(str(live.HARNESS_RUNNER))
EVENTS = Path(__file__).parent / "fixtures" / "codex_browser_harness_events.jsonl"
"""Codex 0.163.0's --json stream from a live hn-top attempt (2026-10-30): four browser-harness commands, one turn."""


def test_recorded_codex_turn_is_priced_from_its_token_usage() -> None:
    tokens = WORKER["usage"](EVENTS)
    assert tokens == {"input_tokens": 63015, "cached_input_tokens": 49417, "output_tokens": 559, "turns": 1}
    # 13,598 uncached at $0.10, 49,417 cached at $0.01 and 559 out at $0.50 per million.
    assert WORKER["dollars"](tokens, live.HARNESS_PRICES) == pytest.approx(0.00213347)


def test_an_attempt_with_no_completed_turn_has_unknown_spend(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    events.write_text('{"type":"turn.failed","error":{"message":"Selected model is at capacity."}}\n')
    assert WORKER["dollars"](WORKER["usage"](events), live.HARNESS_PRICES) is None


def test_browser_harness_runs_the_cua_codex_task_set() -> None:
    assert live.ARMS["browser-harness"].default is False
    assert "browser-harness" in catalog.DERIVED_ARMS
    for task in TASKS:
        assert live.eligible("browser-harness", task) == live.eligible("cua-codex", task)
