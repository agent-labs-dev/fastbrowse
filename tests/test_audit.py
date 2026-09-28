"""Structural tests for the capability audit suite.

These check the suite itself rather than the product: that its case list is coherent, that its exit-code
contract covers every status, and that a tier-0 run produces rows in the documented shape. Tier 0 calls no
model and spends nothing. The paid tiers run only when `FASTBROWSE_AUDIT_SPEND` is set, so the unit suite
never spends money by accident.
"""

import json
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
    assert payload["distinct_failures"]
    assert payload["stable"]


def test_a_spending_tier_is_refused_without_the_spend_flag() -> None:
    with pytest.raises(SystemExit):
        main(["--tier", "1"])


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


def test_paid_cases_keep_exported_model_credentials(monkeypatch: pytest.MonkeyPatch) -> None:
    from fastbrowse.audit.runner import _environment

    monkeypatch.setenv("OPENROUTER_API_KEY", "paid-test-key")
    paid = next(c for c in CASES if c.id == "T1.2")
    refusal = next(c for c in CASES if c.id == "T0.2")
    assert _environment(paid)["OPENROUTER_API_KEY"] == "paid-test-key"
    assert "OPENROUTER_API_KEY" not in _environment(refusal)


def test_missing_index_is_failed_evidence_instead_of_a_crash() -> None:
    from fastbrowse.audit.runner import _dig

    assert _dig({"citations": []}, "citations[0].quote") is None


def test_case_budget_reaches_the_subprocess(monkeypatch: pytest.MonkeyPatch) -> None:
    import subprocess

    from fastbrowse.audit import runner

    calls = []

    def capture(argv: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
        calls.append((argv, kwargs))
        return subprocess.CompletedProcess(argv, 0, stdout="{}", stderr="")

    monkeypatch.setattr(runner.subprocess, "run", capture)
    case = next(c for c in CASES if c.id == "T1.2")
    runner._execute(case, {"cli": "fastbrowse", "base_url": "http://fixture"}, "/tmp", 0.012)
    argv, kwargs = calls[0]
    assert argv[argv.index("--max-dollars") + 1] == "0.012"
    assert kwargs["env"]["FB_AUDIT_MAX_DOLLARS"] == "0.012"


def test_mcp_concurrency_probe_observes_one_active_run() -> None:
    from fastbrowse.audit.probes import mcp_concurrency

    result = mcp_concurrency()
    assert result["both_finished"]
    assert result["peak_concurrent"] == 1
