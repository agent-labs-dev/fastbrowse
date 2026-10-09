import json
from pathlib import Path

import pytest
from pydantic import ValidationError

from fastbrowse.evals.evidence import Evidence, campaign, export
from fastbrowse.evals.storage import Manifest

ROOT = Path(__file__).parents[2]


def test_public_manifest_names_verified_content() -> None:
    manifest = Manifest.model_validate_json((ROOT / "docs/results/evidence.manifest.json").read_text())
    assert manifest.campaigns
    assert all(entry.trace_id == entry.sha256[:32] for entry in manifest.campaigns)


def test_native_grades_are_preserved_independently_of_completion(tmp_path: Path) -> None:
    directory = tmp_path / "docs/validation"
    directory.mkdir(parents=True)
    path = directory / "native.jsonl"
    data = {
        "task": "example",
        "source": "native",
        "arm": "fastbrowse",
        "graded": True,
        "grade": {"passed": True},
        "raw_status": "unverified",
        "completed": False,
        "dollars": 0.1,
        "seconds": 1.0,
    }
    other = data | {"arm": "browser-use", "raw_status": "stopped", "completed": True}
    path.write_text("\n".join(json.dumps({"attempt": row}) for row in (data, other)) + "\n")
    result = campaign(path, tmp_path)
    assert result.attempts[0].passed is True and result.attempts[0].completed is False
    assert result.attempts[1].passed is True and result.attempts[1].completed is True


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


def test_unstarted_budget_refusal_remains_visible_without_filling_coverage(tmp_path: Path) -> None:
    directory = tmp_path / "docs/validation"
    directory.mkdir(parents=True)
    path = directory / "refused.jsonl"
    row = {"attempt": {"task": "example", "arm": "fastbrowse", "budget_refused": True, "started": False}}
    path.write_text(json.dumps(row) + "\n")
    result = campaign(path, tmp_path)
    assert len(result.attempts) == 1
    assert result.attempts[0].started is False
    assert result.attempts[0].passed is None
    assert result.attempts[0].unknown_cost is True


@pytest.mark.parametrize("field,value", [("answer", "private"), ("dollars", float("nan")), ("id", "duplicate")])
def test_public_schema_rejects_content_and_invalid_measures(tmp_path: Path, field: str, value: object) -> None:
    directory = tmp_path / "docs/validation"
    directory.mkdir(parents=True)
    (directory / "example.jsonl").write_text(json.dumps({"task": "example", "arm": "fastbrowse"}) + "\n")
    data = export(tmp_path).model_dump()
    data["campaigns"][0]["attempts"][0][field] = value
    with pytest.raises(ValidationError):
        Evidence.model_validate(data)


def test_legacy_trials_without_repeat_ids_remain_distinct(tmp_path: Path) -> None:
    directory = tmp_path / "docs/validation"
    directory.mkdir(parents=True)
    path = directory / "legacy.jsonl"
    row = {"task": "example", "arm": "fastbrowse", "passed": True, "status": "complete"}
    path.write_text((json.dumps(row) + "\n") * 3)
    result = campaign(path, tmp_path)
    assert [attempt.repeat for attempt in result.attempts] == [0, 1, 2]
    assert result.scheduled_slots is None


def test_started_attempt_without_final_status_is_interrupted(tmp_path: Path) -> None:
    directory = tmp_path / "docs/validation"
    directory.mkdir(parents=True)
    path = directory / "interrupted.jsonl"
    path.write_text(
        json.dumps(
            {"task": "example", "arm": "fastbrowse", "started": True, "status": None, "seconds": 9.6, "dollars": None}
        )
        + "\n"
    )
    attempt = campaign(path, tmp_path).attempts[0]
    assert attempt.started is True
    assert attempt.status == "interrupted"
    assert attempt.seconds == 9.6
    assert attempt.unknown_cost is True


@pytest.mark.parametrize("suffix", [".budget.jsonl", ".budget.json"])
def test_remaining_campaign_projection_binds_the_exact_budget_journal(tmp_path: Path, suffix: str) -> None:
    import hashlib

    directory = tmp_path / "docs/validation"
    directory.mkdir(parents=True)
    path = directory / "campaign.jsonl"
    path.write_text(
        json.dumps({"task": "example", "arm": "fastbrowse", "run": {"budget_policy": "remaining-campaign-v1"}}) + "\n"
    )
    with pytest.raises(FileNotFoundError):
        campaign(path, tmp_path)
    journal = path.with_suffix(suffix)
    journal.write_text('{"committed": 0.1}\n')
    result = campaign(path, tmp_path)
    assert result.sources[-1].path == "docs/validation/campaign" + suffix
    assert result.sources[-1].sha256 == hashlib.sha256(journal.read_bytes()).hexdigest()
