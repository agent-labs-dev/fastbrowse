import json
from pathlib import Path

import pytest

from fastbrowse.evals.runner import _FixtureBudget, main
from fastbrowse.models import CostBasis, CostBreakdown, CostComponent, CostLine


def cost(dollars: float | None) -> CostBreakdown:
    return CostBreakdown(
        lines=(
            CostLine(
                component=CostComponent.LLM,
                basis=CostBasis.UNKNOWN if dollars is None else CostBasis.METERED,
                dollars=dollars,
            ),
        )
    )


def test_fixture_budget_uses_full_remaining_balance_without_task_caps(tmp_path: Path) -> None:
    path = tmp_path / "budget.json"
    budget = _FixtureBudget(path, 10)
    first = budget.reserve("first", 0)
    assert first.max_dollars == 10
    assert first.max_steps is first.max_seconds is first.max_llm_calls is first.max_jev_calls is None
    assert json.loads(path.read_text())["reserved_usd"] == 10
    budget.settle(cost(0.0000049))
    assert budget.reserve("second", 0).max_dollars == 10 - 0.0000049


def test_unknown_fixture_charge_holds_unused_balance_and_stops_admission(tmp_path: Path) -> None:
    path = tmp_path / "budget.json"
    budget = _FixtureBudget(path, 10)
    budget.reserve("first", 0)
    budget.settle(CostBreakdown(lines=(*cost(2).lines, *cost(None).lines)))
    assert budget.receipt.known_usd == 2
    assert budget.receipt.unknown_reserved_usd == 8
    assert budget.receipt.remaining == 0
    assert json.loads(path.read_text())["unknown_reserved_usd"] == 8
    with pytest.raises(ValueError, match="no unreserved budget"):
        budget.reserve("second", 0)


def test_fixture_restart_cannot_reset_interrupted_reservation(tmp_path: Path) -> None:
    path = tmp_path / "budget.json"
    budget = _FixtureBudget(path, 10)
    budget.reserve("first", 0)
    with pytest.raises(FileExistsError):
        _FixtureBudget(path, 10)
    assert json.loads(path.read_text())["reserved_usd"] == 10
    with pytest.raises(ValueError, match="no unreserved budget"):
        budget.reserve("second", 0)


def test_nonfinite_fixture_cost_does_not_release_reservation(tmp_path: Path) -> None:
    budget = _FixtureBudget(tmp_path / "budget.json", 10)
    budget.reserve("first", 0)
    with pytest.raises(ValueError, match="not finite"):
        budget.settle(cost(float("inf")))
    assert budget.receipt.reserved_usd == 10


@pytest.mark.parametrize("value", ["0", "-1", "nan", "inf"])
async def test_invalid_fixture_budget_is_rejected_before_provider_setup(value: str, tmp_path: Path) -> None:
    with pytest.raises(SystemExit):
        await main(["--budget", value, "--out", str(tmp_path / "rows.jsonl")])


async def test_fixture_budget_cannot_append_to_existing_output(tmp_path: Path) -> None:
    path = tmp_path / "rows.jsonl"
    path.write_text("existing evidence\n")
    with pytest.raises(SystemExit):
        await main(["--budget", "10", "--out", str(path)])
    assert path.read_text() == "existing evidence\n"


@pytest.mark.parametrize("unknown", [False, True])
async def test_fixture_campaign_admission_uses_exact_cost_and_stops_on_unknown(
    unknown: bool, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import os
    from contextlib import nullcontext
    from unittest.mock import AsyncMock, Mock

    from fastbrowse.evals import runner

    charge = CostBreakdown(lines=(*cost(0.0000049).lines, *(cost(None).lines if unknown else ())))
    row = {
        "passed": True,
        "status": "complete",
        "seconds": 1.0,
        "dollars": None if unknown else 0.0,
        "failure": None,
        "steps": 1,
        "cost": charge.model_dump(mode="json"),
    }
    drive = AsyncMock(side_effect=[row.copy(), row.copy()])
    monkeypatch.setattr(runner, "run_task", drive)
    monkeypatch.setattr(runner, "load_settings", lambda: Mock(providers=lambda: {}, local_chrome=lambda: None))
    monkeypatch.setattr(runner, "provenance", lambda **_: {})
    monkeypatch.setattr(runner, "fixture_server", lambda: nullcontext(("https://example.test", Mock())))
    monkeypatch.setattr(runner, "local_chrome", lambda _: nullcontext(Mock()))
    output = tmp_path / "rows.jsonl"
    previous_mask = os.umask(0o077)
    try:
        result = await main(
            ["--suite", "local", "--only", runner.TASKS[0].id, "--repeat", "2", "--budget", "2", "--out", str(output)]
        )
    finally:
        os.umask(previous_mask)
    rows = [json.loads(line) for line in output.read_text().splitlines()]
    assert result == int(unknown)
    assert len(rows) == drive.await_count == (1 if unknown else 2)
    first_limits = drive.await_args_list[0].kwargs["limits"]
    assert first_limits.max_dollars == 2 and first_limits.max_steps is None
    assert rows[0]["run"]["agent_limits"] == first_limits.model_dump(mode="json")
    assert rows[0]["cost"]["lines"][0]["dollars"] == 0.0000049
    if not unknown:
        assert drive.await_args_list[1].kwargs["limits"].max_dollars == 2 - 0.0000049
    receipt = json.loads(output.with_suffix(".budget.json").read_text())
    assert receipt["known_usd"] == 0.0000049 * len(rows)
    assert receipt["reserved_usd"] == 0
    assert receipt["unknown_reserved_usd"] == (2 - 0.0000049 if unknown else 0)


def test_unknown_fixture_cost_blocks_fractional_remaining_residue(tmp_path: Path) -> None:
    budget = _FixtureBudget(tmp_path / "budget.json", 1)
    budget.reserve("first", 0)
    budget.settle(cost(0.3))
    budget.reserve("second", 0)
    budget.settle(CostBreakdown(lines=(*cost(0.2).lines, *cost(None).lines)))
    assert budget.receipt.remaining > 0
    with pytest.raises(ValueError, match="no unreserved budget"):
        budget.reserve("third", 0)


def test_negative_fixture_cost_cannot_increase_available_funds(tmp_path: Path) -> None:
    budget = _FixtureBudget(tmp_path / "budget.json", 1)
    budget.reserve("first", 0)
    malformed = cost(0.1).lines[0].model_copy(update={"dollars": -0.1})
    with pytest.raises(ValueError, match="negative"):
        budget.settle(CostBreakdown(lines=(malformed,)))
    assert budget.receipt.reserved_usd == 1
    assert budget.receipt.known_usd == 0
