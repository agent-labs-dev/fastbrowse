import json
from pathlib import Path

import pytest
from pydantic import ValidationError

from fastbrowse.evals.api_cost import ApiCostEstimate, recover_api_cost, token_cost_range
from fastbrowse.evals.evidence import campaign


def events(path: Path, inputs: int = 200_000) -> Path:
    path.write_text(
        json.dumps(
            {
                "type": "turn.completed",
                "usage": {
                    "input_tokens": inputs,
                    "cached_input_tokens": 150_000,
                    "cache_write_input_tokens": 0,
                    "output_tokens": 1000,
                    "reasoning_output_tokens": 200,
                },
            }
        )
        + "\n"
    )
    return path


def test_cache_and_reasoning_are_not_charged_twice(tmp_path: Path) -> None:
    estimate = recover_api_cost(events(tmp_path / "events.jsonl"), model="gpt-6-astra", service_tier="default")
    assert estimate is not None
    assert estimate.dollars_low == estimate.dollars_high == 0.7
    low, high = token_cost_range(400_000, 150_000, 0, 1000)
    assert low == 2.7 and high == 5.375


def test_missing_ambiguous_and_unsupported_usage_stays_unknown(tmp_path: Path) -> None:
    path = events(tmp_path / "events.jsonl")
    assert recover_api_cost(path, model="other", service_tier="default") is None
    assert recover_api_cost(path, model="gpt-6-astra", service_tier="fast") is None
    path.write_text(path.read_text() * 2)
    assert recover_api_cost(path, model="gpt-6-astra", service_tier="default") is None
    path.write_text('{"type":"turn.failed"}\n')
    assert recover_api_cost(path, model="gpt-6-astra", service_tier="default") is None


def test_estimated_cost_never_fills_recorded_dollars_or_changes_grade(tmp_path: Path) -> None:
    estimate = recover_api_cost(events(tmp_path / "events.jsonl"), model="gpt-6-astra", service_tier="default")
    assert estimate is not None
    folder = tmp_path / "docs/validation"
    folder.mkdir(parents=True)
    ledger = folder / "example.jsonl"
    ledger.write_text(
        json.dumps(
            {
                "task": "example",
                "arm": "cua-codex",
                "status": "complete",
                "passed": False,
                "dollars": None,
                "unknown_cost": True,
                "model": "gpt-6-astra",
                "estimated_api_cost": estimate.model_dump(mode="json"),
            }
        )
        + "\n"
    )
    result = campaign(ledger, tmp_path).attempts[0]
    assert result.dollars is None and result.unknown_cost and result.passed is False
    assert result.estimated_api_cost == estimate
    data = estimate.model_dump()
    data["dollars_low"] = 0.0
    with pytest.raises(ValidationError):
        ApiCostEstimate.model_validate(data)


def test_legacy_projection_omits_the_optional_estimate(tmp_path: Path) -> None:
    folder = tmp_path / "docs/validation"
    folder.mkdir(parents=True)
    ledger = folder / "example.jsonl"
    ledger.write_text('{"task":"example","arm":"fastbrowse"}\n')
    assert "estimated_api_cost" not in campaign(ledger, tmp_path).model_dump_json()


def test_corrupt_usage_does_not_block_other_run_estimates(tmp_path: Path) -> None:
    good = events(tmp_path / "good.jsonl")
    bad = events(tmp_path / "bad.jsonl")
    bad.write_text(bad.read_text() + '{"type":')
    assert recover_api_cost(bad, model="gpt-6-astra", service_tier="default") is None
    assert recover_api_cost(good, model="gpt-6-astra", service_tier="default") is not None
    row = json.loads(good.read_text())
    row["usage"]["cached_input_tokens"] = 300_000
    bad.write_text(json.dumps(row))
    assert recover_api_cost(bad, model="gpt-6-astra", service_tier="default") is None
