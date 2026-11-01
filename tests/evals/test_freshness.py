"""The eval freshness tags: every case carries the build it was written against, and a stale one is named."""

import json

import pytest

from fastbrowse.evals import freshness


def test_every_case_is_tagged_at_the_build_running_now():
    record = freshness.load()
    assert freshness.check(record) == [], "a stale tag is a case no longer measured against what it claims"


def test_every_case_the_suites_run_has_an_entry():
    record = freshness.load()
    cases = record["cases"]
    assert isinstance(cases, dict)
    assert set(cases) == set(freshness.cases())


def test_a_newer_driver_makes_every_movable_case_stale():
    now = {**freshness.current(), "driver": "99.0.0"}
    findings = freshness.check(freshness.load(), now)
    assert {finding.case for finding in findings} == set(freshness.cases())
    assert {finding.field for finding in findings} == {"driver"}
    assert all(finding.movable for finding in findings)


def test_a_case_with_no_entry_is_stale_whatever_the_build():
    record = freshness.load()
    dropped = sorted(freshness.cases())[0]
    cases = record["cases"]
    assert isinstance(cases, dict)
    del cases[dropped]
    findings = freshness.check(record)
    assert [(finding.case, finding.field) for finding in findings] == [(dropped, freshness.TAG)]


def test_an_immovable_case_is_named_with_its_reason_and_never_restamped():
    record = freshness.load()
    cases = record["cases"]
    assert isinstance(cases, dict)
    case = sorted(cases)[0]
    record["immovable"] = {case: "graded against a provider reply this repository does not pin"}
    now = {**freshness.current(), "driver": "99.0.0"}

    refused = [finding for finding in freshness.check(record, now) if not finding.movable]
    assert [finding.case for finding in refused] == [case]
    assert refused[0].reason == record["immovable"][case]
    assert "immovable" in refused[0].line()

    updated, also_refused = freshness.resync(record, now)
    stamped = updated["cases"]
    assert isinstance(stamped, dict)
    assert stamped[case] == cases[case], "an immovable case keeps the build it was measured on"
    assert [finding.case for finding in also_refused] == [case]
    # Every other case moved, so the next check reports the one limit rather than a wall of stale tags.
    assert {finding.case for finding in freshness.check(updated, now) if finding.movable} == set()


def test_an_immovable_case_with_no_entry_stays_untagged():
    record = freshness.load()
    cases = record["cases"]
    assert isinstance(cases, dict)
    case = sorted(cases)[0]
    del cases[case]
    record["immovable"] = {case: "no build measured it"}
    updated, refused = freshness.resync(record)
    stamped = updated["cases"]
    assert isinstance(stamped, dict)
    assert case not in stamped
    assert [finding.case for finding in refused] == [case]


def _book(monkeypatch, record: dict[str, object]) -> list[dict[str, object]]:
    """Point the command at `record`, and return the list of what it saves."""
    saved: list[dict[str, object]] = []
    monkeypatch.setattr(freshness, "load", lambda: record)
    monkeypatch.setattr(freshness, "save", saved.append)
    return saved


def test_the_immovable_flag_records_the_reason(monkeypatch):
    saved = _book(monkeypatch, {"cases": {}})
    case = sorted(freshness.cases())[0]
    assert freshness.main(["--immovable", case, "provider reply is not pinned"]) == 0
    assert saved[0]["immovable"] == {case: "provider reply is not pinned"}


def test_the_immovable_flag_refuses_an_unknown_case(monkeypatch):
    saved = _book(monkeypatch, {"cases": {}})
    with pytest.raises(SystemExit):
        freshness.main(["--immovable", "invented-case", "no reason"])
    assert saved == []


def test_the_resync_flag_says_it_relabels_and_counts_only_what_moved(monkeypatch, capsys):
    record = freshness.load()
    case = sorted(freshness.cases())[0]
    record["immovable"] = {case: "provider reply is not pinned"}
    saved = _book(monkeypatch, record)
    assert freshness.main(["--resync"]) == 0
    assert saved[0]["immovable"] == record["immovable"]
    assert f"re-labelled {len(freshness.cases()) - 1} cases" in (out := capsys.readouterr().out)
    assert "nothing was re-measured" in out


def test_resync_drops_a_case_the_suites_no_longer_run():
    record = freshness.load()
    cases = record["cases"]
    assert isinstance(cases, dict)
    cases["retired-case"] = {"suite": "local", "driver": "0.0.1", "judge": "jev-0"}
    updated, _ = freshness.resync(record)
    stamped = updated["cases"]
    assert isinstance(stamped, dict)
    assert "retired-case" not in stamped


def test_the_pinned_file_round_trips_through_save(tmp_path):
    path = tmp_path / "freshness.json"
    record = freshness.load()
    freshness.save(record, path)
    assert json.loads(path.read_text(encoding="utf-8")) == record
    assert freshness.check(freshness.load(path)) == []
