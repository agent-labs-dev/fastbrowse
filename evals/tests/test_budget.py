"""The campaign budget gates new runs only, and keeps unresolved spend after interruption."""

import asyncio
from pathlib import Path

import pytest

from fastbrowse_evals.budget import BudgetStop, CampaignBudget


async def test_concurrent_reservation_waits_for_known_charge_and_survives_reload(tmp_path: Path):
    path = tmp_path / "spend.jsonl"
    budget = CampaignBudget(1, 1, path)
    await budget.reserve("first")
    waiting = asyncio.create_task(budget.reserve("second"))
    await asyncio.sleep(0)
    assert not waiting.done()
    await budget.settle("first", 0)
    await waiting
    await budget.settle("second", 0.1)
    budget.close()
    restored = CampaignBudget(1, 1, path)
    assert restored.committed == pytest.approx(0.1)
    with pytest.raises(BudgetStop):
        await restored.reserve("third")


async def test_unknown_charge_keeps_its_reservation_and_never_starts_a_duplicate(tmp_path: Path):
    budget = CampaignBudget(1, 1, tmp_path / "spend.jsonl")
    await budget.reserve("first")
    await budget.settle("first", None)
    with pytest.raises(BudgetStop):
        await budget.reserve("second")
    with pytest.raises(ValueError, match="reserve twice"):
        await budget.reserve("first")


def test_campaign_lock_refuses_another_owner(tmp_path: Path):
    path = tmp_path / "spend.jsonl"
    with CampaignBudget(1, 1, path), pytest.raises(ValueError, match="another process"):
        CampaignBudget(1, 1, path)
    with CampaignBudget(1, 1, path):
        pass


async def test_waiting_duplicate_is_rechecked_after_another_waiter_reserves(tmp_path):
    with CampaignBudget(2, 1, tmp_path / "race.jsonl") as budget:
        await budget.reserve("first")
        await budget.reserve("second")
        waiting = [asyncio.create_task(budget.reserve("same")) for _ in range(2)]
        await asyncio.sleep(0)
        await budget.settle("first", 0)
        await asyncio.sleep(0)
        assert sum(task.done() for task in waiting) == 2
        outcomes = await asyncio.gather(*waiting, return_exceptions=True)
        assert sum(isinstance(result, ValueError) for result in outcomes) == 1
        assert budget.committed == 2 and budget.running == 2
        await budget.settle("same", None)
        await budget.settle("second", None)


async def test_cancelled_reservation_survives_resume_and_changed_limits_refused(tmp_path):
    path = tmp_path / "cancel.jsonl"
    with CampaignBudget(2, 1, path) as budget:
        await budget.reserve("interrupted")
    with CampaignBudget(2, 1, path) as restored:
        assert restored.committed == 1
        assert await restored.reserve("next") == 1
        await restored.settle("next", 0.5)
        with pytest.raises(BudgetStop):
            await restored.reserve("full")
    for limit, reservation in ((3, 1), (2, 0.5)):
        with pytest.raises(ValueError, match="original limits"):
            CampaignBudget(limit, reservation, path)


async def test_overrun_is_recorded_and_never_stops_dispatch(tmp_path):
    """A run dearer than its hold limited nothing: the charge is kept and dispatch continues while a hold fits."""
    path = tmp_path / "overrun.jsonl"
    with CampaignBudget(10, 1, path) as budget:
        await budget.reserve("first")
        await budget.settle("first", 3)  # above its 1 dollar hold: recorded, no exception
        assert budget.committed == 3 and budget.running == 0
        for n in range(7):
            assert await budget.reserve(f"next-{n}") == 1
        assert budget.committed == 10
        waiting = asyncio.create_task(budget.reserve("over"))
        await asyncio.sleep(0)
        assert not waiting.done()  # waits for runs in flight rather than stopping
        await asyncio.gather(*(budget.settle(f"next-{n}", 0.5) for n in range(7)))
        assert await waiting == 1
        assert budget.committed == pytest.approx(7.5)
        await budget.settle("over", None)
    rows = [line for line in path.read_text().splitlines()]
    assert all('"policy": "start-gate-v1"' in row for row in rows)


async def test_gate_refuses_only_when_nothing_running_can_free_room(tmp_path):
    with CampaignBudget(1.5, 1, tmp_path / "gate.jsonl") as budget:
        await budget.reserve("first")
        waiting = asyncio.create_task(budget.reserve("second"))
        await asyncio.sleep(0)
        assert not waiting.done()
        await budget.settle("first", 2)
        with pytest.raises(BudgetStop):
            await waiting


def test_start_gate_requires_positive_hold(tmp_path):
    for limit, reservation in ((0, 1), (1, 0), (float("inf"), 1), (1, float("nan"))):
        with pytest.raises(ValueError, match="positive finite"):
            CampaignBudget(limit, reservation, tmp_path / "bad.jsonl")


def test_legacy_policy_ledger_cannot_resume(tmp_path):
    path = tmp_path / "legacy.jsonl"
    path.write_text('{"id": "a", "limit": 2, "reservation": 1, "committed": 1, "remaining": true}\n')
    with pytest.raises(ValueError, match="policy"):
        CampaignBudget(2, 1, path)
