"""Structural tests for the capability audit suite.

These check the suite itself rather than the product: that its case list is coherent, that its exit-code
contract covers every status, and that a tier-0 run produces rows in the documented shape. Tier 0 calls no
model and spends nothing. The paid tiers run only when `FASTBROWSE_AUDIT_SPEND` is set, so the unit suite
never spends money by accident.
"""

import json
import os
from pathlib import Path

import pytest

from fastbrowse.audit.cases import CASES
from fastbrowse.audit.probes import status_exit_contract
from fastbrowse.audit.runner import ROWS, main
from fastbrowse.models import Status


def test_case_ids_are_unique() -> None:
    ids = [case.id for case in CASES]
    assert len(ids) == len(set(ids)), "two audit cases share an id"


def test_every_case_targets_a_real_tier() -> None:
    assert {case.tier for case in CASES} <= {0, 1, 2, 3}


def test_every_status_is_covered_by_the_exit_code_contract() -> None:
    payload = status_exit_contract()
    assert payload["total"], "a Status member has no exit code"
    assert set(payload["codes"]) == {status.value for status in Status}
    assert payload["complete_zero"]
    assert payload["others_one"]
    assert payload["stable"]


def test_a_spending_tier_is_refused_without_the_spend_flag() -> None:
    with pytest.raises(SystemExit):
        main(["--tier", "1"])


@pytest.mark.skipif(
    not os.environ.get("FASTBROWSE_AUDIT_SPEND"),
    reason="paid tiers run only when FASTBROWSE_AUDIT_SPEND is set",
)
def test_the_spending_tiers_run_only_when_explicitly_requested() -> None:
    # With the flag set, the selection must include at least one case that calls a model.
    assert any(case.spends for case in CASES if case.tier > 0)


def test_tier_0_runs_green_with_rows_in_the_documented_shape(tmp_path: Path) -> None:
    out = tmp_path / "report.json"
    rollup = tmp_path / "rollup.md"
    assert main(["--tier", "0", "--out", str(out), "--rollup", str(rollup)]) == 0
    rows = json.loads(out.read_text(encoding="utf-8"))
    assert rows, "tier 0 produced no rows"
    assert rollup.exists()
    for row in rows:
        assert set(ROWS) <= set(row), f"{row.get('id')} row is missing a documented field"
        assert all(item["ok"] for item in row["assertions"]), f"{row.get('id')} failed a check"
