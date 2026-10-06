import json
from pathlib import Path

import pytest
from pydantic import ValidationError

from fastbrowse.evals.evidence import Evidence, campaign, export

ROOT = Path(__file__).parents[2]


def test_public_evidence_is_regenerated_from_sources() -> None:
    committed = Evidence.model_validate_json((ROOT / "docs/results/evidence.json").read_text())
    assert committed == export(ROOT)


def test_native_grades_are_preserved_independently_of_completion() -> None:
    native = next(c for c in export(ROOT).campaigns if c.id == "2026-10-05-native-corpus-attempts")
    selected = [a for a in native.attempts if a.selected]
    fastbrowse = [a for a in selected if a.arm == "fastbrowse"]
    browser_use = [a for a in selected if a.arm == "browser-use"]
    assert native.scheduled_slots == 72
    assert len(native.attempts) == 73
    assert (len(fastbrowse), sum(a.passed is True for a in fastbrowse)) == (36, 23)
    assert (sum(a.passed is not None for a in browser_use), sum(a.passed is True for a in browser_use)) == (35, 33)
    assert sum(a.completed and a.passed is True for a in fastbrowse) == 19
    assert sum(a.completed for a in browser_use) == 35
    assert any(a.passed is True and not a.completed for a in fastbrowse)
    assert any(a.status == "stopped" and a.completed for a in browser_use)


def test_projection_retains_failed_retry_and_omits_page_content(tmp_path: Path) -> None:
    directory = tmp_path / "docs/validation"
    directory.mkdir(parents=True)
    path = directory / "example.jsonl"
    row = {
        "task": "example",
        "arm": "fastbrowse",
        "suite": "dev",
        "repeat": 0,
        "status": "complete",
        "passed": True,
        "dollars": 0.1,
        "seconds": 1.0,
        "answer": "secret-marker",
        "final_url": "https://private.invalid/?secret-marker",
        "final_page": {"text": "secret-marker"},
        "run": {"run_id": "chosen"},
    }
    path.write_text(json.dumps(row) + "\n")
    ledger = path.with_suffix(".attempts.jsonl")
    failed = row | {"passed": False, "status": "error", "selected": False, "unknown_cost": True, "dollars": 9.0}
    ledger.write_text(json.dumps(failed) + "\n" + json.dumps(row | {"selected": True}) + "\n")
    result = campaign(path, tmp_path)
    assert len(result.attempts) == 2
    assert result.scheduled_slots is None
    assert result.attempts[0].passed is False
    assert result.attempts[0].dollars is None
    assert result.attempts[0].unknown_cost is True
    assert result.attempts[1].selected is True
    assert "secret-marker" not in result.model_dump_json()
    assert "answer" not in result.model_dump_json()
    assert len(result.sources) == 2


def test_unstarted_final_budget_refusals_do_not_fill_coverage() -> None:
    heldout = next(c for c in export(ROOT).campaigns if c.id == "native-final-heldout")
    assert heldout.scheduled_slots == 27
    assert len(heldout.attempts) == 33
    assert sum(a.started for a in heldout.attempts) == 25
    assert sum(a.selected for a in heldout.attempts) == 25
    assert sum(a.passed is True for a in heldout.attempts if a.selected) == 21
    assert sum(a.unknown_cost for a in heldout.attempts) == 8


@pytest.mark.parametrize("field,value", [("answer", "private"), ("dollars", float("nan")), ("id", "duplicate")])
def test_public_schema_rejects_content_and_invalid_measures(field: str, value: object) -> None:
    data = export(ROOT).model_dump()
    data["campaigns"][0]["attempts"][0][field] = value
    with pytest.raises(ValidationError):
        Evidence.model_validate(data)


def test_archived_selected_outcomes_reconcile_with_original_receipt() -> None:
    archive = Evidence.model_validate_json((ROOT / "docs/results/evidence-archive.json").read_text())
    receipt_path = ROOT / "docs/validation/2026-10-06-native-diagnostic-receipt.json"
    receipt = {Path(row["file"]).stem: row for row in json.loads(receipt_path.read_text())["runs"]}
    for entry in archive.campaigns:
        rows = [a for a in entry.attempts if a.selected]
        expected = receipt[entry.id]
        assert len(rows) == expected["attempts"]
        assert sum(a.passed is True for a in rows) == expected["passed"]
        assert sum(a.unknown_cost for a in rows) == expected["unknown_cost"]
        assert sum(a.dollars for a in rows if a.dollars is not None) == pytest.approx(expected["dollars"])


def test_legacy_trials_without_repeat_ids_remain_distinct() -> None:
    legacy = next(c for c in export(ROOT).campaigns if c.id == "0.5.6")
    rows = [a for a in legacy.attempts if a.task == legacy.attempts[0].task and a.arm == legacy.attempts[0].arm]
    assert len({a.repeat for a in rows}) == len(rows)
    assert legacy.scheduled_slots is None
