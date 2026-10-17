"""Publication refuses incomplete or mismatched evidence before owner approval."""

import copy
import hashlib
import json
from pathlib import Path

import pytest
from pydantic import ValidationError

from fastbrowse.evals.benchmarks import Candidate, check_catalog, regressions

CATALOG = Path(__file__).parents[1] / "src/fastbrowse/evals/benchmark-catalog.json"


@pytest.fixture
def candidate() -> dict:
    raw = CATALOG.read_bytes()
    digest = hashlib.sha256(raw).hexdigest()
    catalog = json.loads(raw)
    tasks = [task for suite in catalog["suites"].values() for task in suite]
    groups = []
    for name, metric, arms, ids, repeats in (
        (
            "internal-hosted",
            "task-success",
            ["fastbrowse", "browser-use"],
            sorted(t["id"] for t in tasks if "browser-use" in t["arms"]),
            3,
        ),
        (
            "internal-ultrafast",
            "task-success",
            ["fastbrowse", "jev-ultrafast"],
            sorted(t["id"] for t in tasks if "jev-ultrafast" in t["arms"]),
            3,
        ),
        ("bu-bench", "weighted-rubric", ["fastbrowse"], [f"bu-{i}" for i in range(200)], 1),
        ("online-mind2web", "full-task-success", ["fastbrowse"], [f"mind-{i}" for i in range(300)], 1),
    ):
        groups.append(
            {
                "id": name,
                "agent_sha": "a" * 40,
                "runner_sha": "b" * 40,
                "metric": metric,
                "source_sha256": digest,
                "task_ids": ids,
                "repeats": repeats,
                "limits": ["Matched recorded limits"],
                "rows": [
                    {
                        "task": task,
                        "repeat": repeat,
                        "arm": arm,
                        "status": "complete",
                        "completed": True,
                        "score": 1.0,
                        "seconds": 10.0,
                        "dollars": 0.01,
                        "physical_runs": 1,
                    }
                    for task in ids
                    for repeat in range(repeats)
                    for arm in arms
                ],
            }
        )
    return {
        "schema_version": 1,
        "campaign_id": "test-campaign",
        "agent_sha": "a" * 40,
        "runner_sha": "b" * 40,
        "catalog_sha256": digest,
        "fixture": {"head_sha": "a" * 40, "run_id": 123, "passed": 108, "total": 108},
        "langfuse_verified": True,
        "groups": groups,
    }


def test_complete_candidate_matches_the_catalog(candidate):
    check_catalog(Candidate.model_validate(candidate), CATALOG)


def test_official_suite_can_publish_without_new_comparator_runs(candidate):
    candidate.update(schema_version=2, groups=[candidate["groups"][2]])
    check_catalog(Candidate.model_validate(candidate), CATALOG)


def test_internal_fastbrowse_group_covers_the_full_catalog_without_comparators(candidate):
    raw = CATALOG.read_bytes()
    catalog = json.loads(raw)
    tasks = [task for suite in catalog["suites"].values() for task in suite]
    ids = sorted(task["id"] for task in tasks if "fastbrowse" in task["arms"])
    group = {
        "id": "internal-fastbrowse",
        "agent_sha": "a" * 40,
        "runner_sha": "b" * 40,
        "metric": "task-success",
        "source_sha256": hashlib.sha256(raw).hexdigest(),
        "task_ids": ids,
        "repeats": 3,
        "limits": ["Matched recorded limits"],
        "rows": [
            {
                "task": task,
                "repeat": repeat,
                "arm": "fastbrowse",
                "status": "complete",
                "completed": True,
                "score": 1.0,
                "seconds": 10.0,
                "dollars": 0.01,
                "physical_runs": 1,
            }
            for task in ids
            for repeat in range(3)
        ],
    }
    candidate.update(schema_version=3, groups=[group])
    check_catalog(Candidate.model_validate(candidate), CATALOG)
    candidate["schema_version"] = 2
    with pytest.raises(ValidationError, match="four original"):
        Candidate.model_validate(candidate)


def test_internal_fastbrowse_group_rejects_a_missing_catalog_slot(candidate):
    raw = CATALOG.read_bytes()
    catalog = json.loads(raw)
    ids = sorted(task["id"] for suite in catalog["suites"].values() for task in suite if "fastbrowse" in task["arms"])
    group = {
        "id": "internal-fastbrowse",
        "agent_sha": "a" * 40,
        "runner_sha": "b" * 40,
        "metric": "task-success",
        "source_sha256": hashlib.sha256(raw).hexdigest(),
        "task_ids": ids[:-1],
        "repeats": 3,
        "limits": ["Matched recorded limits"],
        "rows": [],
    }
    candidate.update(schema_version=3, groups=[group])
    with pytest.raises(ValidationError):
        Candidate.model_validate(candidate)


def test_figures_approved_before_a_task_was_retired_still_read() -> None:
    """The published baseline ran 54 tasks before stretch-wizard-correction was retired; the gate reads it back."""
    published = Candidate.model_validate_json((CATALOG.parents[3] / "docs/results/benchmarks.json").read_bytes())
    assert {len(g.task_ids) for g in published.groups if g.id == "internal-fastbrowse"} == {54}


def test_a_new_candidate_cannot_keep_a_retired_task(candidate):
    group = candidate["groups"][0]
    group["task_ids"].append("stretch-wizard-correction")
    for repeat in range(3):
        for arm in ("fastbrowse", "browser-use"):
            group["rows"].append(
                {**group["rows"][0], "task": "stretch-wizard-correction", "repeat": repeat, "arm": arm}
            )
    with pytest.raises(ValueError, match="exact matched catalog"):
        check_catalog(Candidate.model_validate(candidate), CATALOG)


def test_legacy_schema_still_requires_all_four_groups(candidate):
    candidate["groups"] = [candidate["groups"][2]]
    with pytest.raises(ValidationError, match="all four"):
        Candidate.model_validate(candidate)


def test_legacy_complete_candidate_without_group_provenance_remains_valid(candidate):
    for group in candidate["groups"]:
        del group["agent_sha"], group["runner_sha"]
    Candidate.model_validate(candidate)


def test_copied_groups_cannot_be_attributed_to_a_new_build(candidate):
    candidate.update(schema_version=2, agent_sha="c" * 40)
    candidate["fixture"]["head_sha"] = "c" * 40
    with pytest.raises(ValidationError, match="measured agent and runner"):
        Candidate.model_validate(candidate)


def test_independent_groups_require_explicit_provenance(candidate):
    candidate["schema_version"] = 2
    del candidate["groups"][0]["agent_sha"]
    with pytest.raises(ValidationError, match="measured agent and runner"):
        Candidate.model_validate(candidate)


@pytest.mark.parametrize("groups", [[], [2, 2]])
def test_independent_publication_requires_unique_complete_groups(candidate, groups):
    candidate.update(schema_version=2, groups=[candidate["groups"][i] for i in groups])
    with pytest.raises(ValidationError):
        Candidate.model_validate(candidate)


def test_published_groups_cannot_be_removed(candidate):
    before = Candidate.model_validate(candidate)
    candidate.update(schema_version=2, groups=[candidate["groups"][2]])
    with pytest.raises(ValueError, match="cannot disappear"):
        regressions(Candidate.model_validate(candidate), before)


def test_new_complete_suite_can_be_added_to_an_approved_campaign(candidate):
    candidate["schema_version"] = 2
    before = Candidate.model_validate({**candidate, "groups": [candidate["groups"][2]]})
    regressions(Candidate.model_validate(candidate), before)


def test_legacy_schema_cannot_bypass_an_approved_group_provenance_check(candidate):
    before = Candidate.model_validate({**candidate, "schema_version": 2})
    for group in candidate["groups"]:
        del group["agent_sha"], group["runner_sha"]
    with pytest.raises(ValueError, match="cannot downgrade"):
        regressions(Candidate.model_validate(candidate), before)


@pytest.mark.parametrize(
    "change",
    [
        lambda c: c["groups"][0]["rows"].pop(),
        lambda c: c["groups"][0]["rows"].append(c["groups"][0]["rows"][0]),
        lambda c: c["groups"][0]["rows"][0].update(dollars=None),
        lambda c: c["groups"][0]["rows"][0].update(score=None),
        lambda c: c["groups"][0]["rows"][0].update(answer="private page content"),
        lambda c: c["groups"][0]["rows"][0].update(seconds=float("nan")),
        lambda c: c["groups"][0]["rows"][0].update(score=0.5),
        lambda c: c["fixture"].update(head_sha="c" * 40),
        lambda c: c["fixture"].update(passed=107),
        lambda c: c.update(langfuse_verified=False),
    ],
)
def test_incomplete_or_unsafe_candidate_is_refused(candidate, change):
    change(candidate)
    with pytest.raises(ValidationError):
        Candidate.model_validate(candidate)


def test_same_count_with_an_unmatched_catalog_task_is_refused(candidate):
    group = candidate["groups"][0]
    old = group["task_ids"][0]
    group["task_ids"][0] = "invented-task"
    for row in group["rows"]:
        if row["task"] == old:
            row["task"] = "invented-task"
    with pytest.raises(ValueError, match="exact matched catalog"):
        check_catalog(Candidate.model_validate(candidate), CATALOG)


@pytest.mark.parametrize(("field", "value"), [("score", 0.0), ("seconds", 15.0), ("dollars", 0.02)])
def test_matched_regressions_block_publication(candidate, field, value):
    before = Candidate.model_validate(copy.deepcopy(candidate))
    task = candidate["groups"][0]["task_ids"][0]
    for row in candidate["groups"][0]["rows"]:
        if row["task"] == task and row["arm"] == "fastbrowse":
            row[field] = value
    with pytest.raises(ValueError, match="regression"):
        regressions(Candidate.model_validate(candidate), before)


def test_changed_protocol_cannot_silently_skip_regressions(candidate):
    before = Candidate.model_validate(copy.deepcopy(candidate))
    candidate["groups"][0]["limits"] = ["Different limits"]
    with pytest.raises(ValueError, match="protocol changed"):
        regressions(Candidate.model_validate(candidate), before)


def test_fastbrowse_completion_cannot_disagree_with_status(candidate):
    candidate["groups"][0]["rows"][0]["completed"] = False
    with pytest.raises(ValidationError, match="completion"):
        Candidate.model_validate(candidate)


def test_source_grade_remains_independent_of_completion(candidate):
    row = candidate["groups"][2]["rows"][0]
    row.update(status="unverified", completed=False, score=1.0)
    Candidate.model_validate(candidate)


def test_fixture_claims_are_checked_against_the_actual_artifact(candidate, monkeypatch):
    from fastbrowse.evals import benchmarks
    from fastbrowse.evals.runner import _tasks

    monkeypatch.setattr(
        benchmarks,
        "github_run",
        lambda _: {
            "head_sha": "a" * 40,
            "conclusion": "success",
            "status": "completed",
            "path": ".github/workflows/evals.yml",
            "repository": {"full_name": "agent-labs-dev/fastbrowse"},
            "head_branch": "main",
            "event": "workflow_dispatch",
        },
    )
    tasks, _ = _tasks([], ["local", "mock"])
    rows = [
        {"task": task.id, "repeat": repeat, "passed": True, "run": {"git_sha": "a" * 40, "git_dirty": False}}
        for _, task in tasks
        for repeat in range(3)
    ]

    def download(args, **_):
        target = Path(args[args.index("--dir") + 1]) / "nightly.jsonl"
        target.write_text("\n".join(json.dumps(r) for r in rows))

    monkeypatch.setattr(benchmarks.subprocess, "run", download)
    parsed = Candidate.model_validate(candidate)
    benchmarks.check_fixture(parsed)
    rows[0]["passed"] = False
    with pytest.raises(ValueError, match="all fixture rows"):
        benchmarks.check_fixture(parsed)
    rows[0]["passed"] = True
    rows.pop()
    with pytest.raises(ValueError, match="artifact coverage"):
        benchmarks.check_fixture(parsed)


# A deliberate protocol change: the owner approves publishing changed groups as a fresh baseline, never compared
# with the figures they replace, which stay published under their campaign's name.

HEAD = "c" * 40


def _bytes(candidate: dict) -> bytes:
    return (json.dumps(candidate, indent=2) + "\n").encode()


def _changed(candidate: dict) -> dict:
    """The same campaign measured under new limits for internal-hosted, a new protocol for that group alone."""
    changed = copy.deepcopy(candidate)
    changed["campaign_id"] = "release-0.5.22-new"
    changed["groups"][0]["limits"] = ["No arm capped"]
    return changed


def _approval(content: bytes, record=None) -> dict:
    approval = {
        "candidate_sha256": hashlib.sha256(content).hexdigest(),
        "workflow_run": 9,
        "head_sha": HEAD,
        "approved_by": "cjber",
        "approved_at": "2026-10-17T12:00:00Z",
    }
    if record is not None:
        approval["protocol_change"] = record.model_dump(mode="json")
    return approval


def test_a_changed_protocol_is_refused_without_the_owner_flag(candidate):
    from fastbrowse.evals.benchmarks import Approval, check_protocol_change

    base = Candidate.model_validate(candidate)
    changed = Candidate.model_validate(_changed(candidate))
    with pytest.raises(ValueError, match="protocol changed: internal-hosted"):
        regressions(changed, base)
    # An approval without the owner's record exempts nothing, so the comparison still refuses the change.
    unflagged = Approval.model_validate(_approval(b"x"))
    assert check_protocol_change(changed, base, _bytes(candidate), b"{}", unflagged) == set()


def test_a_protocol_change_skips_only_the_changed_groups(candidate):
    from fastbrowse.evals.benchmarks import protocol_change

    base = Candidate.model_validate(candidate)
    changed = _changed(candidate)
    record = protocol_change(Candidate.model_validate(changed), base, _bytes(candidate), "Uncapped protocol (#356)")
    assert [g.id for g in record.groups] == ["internal-hosted"]
    assert record.archived_figures == "docs/results/benchmarks.test-campaign.json"
    regressions(Candidate.model_validate(changed), base, changed={"internal-hosted"})
    # internal-ultrafast kept its protocol, so it is still compared with its published figures.
    task = changed["groups"][1]["task_ids"][0]
    for row in changed["groups"][1]["rows"]:
        if row["task"] == task and row["arm"] == "fastbrowse":
            row["score"] = 0.0
    with pytest.raises(ValueError, match="source-score regression: internal-ultrafast"):
        regressions(Candidate.model_validate(changed), base, changed={"internal-hosted"})


def test_a_protocol_change_cannot_drop_a_published_group(candidate):
    base = Candidate.model_validate(candidate)
    changed = _changed(candidate)
    changed.update(schema_version=2, groups=changed["groups"][:3])
    with pytest.raises(ValueError, match="cannot disappear"):
        regressions(Candidate.model_validate(changed), base, changed={"internal-hosted", "online-mind2web"})


def test_a_protocol_change_needs_a_changed_group(candidate):
    from fastbrowse.evals.benchmarks import protocol_change

    with pytest.raises(ValueError, match="no published group changed"):
        protocol_change(Candidate.model_validate(candidate), Candidate.model_validate(candidate), b"{}", "Note")


def test_the_receipt_must_name_exactly_the_changed_groups(candidate, tmp_path, monkeypatch):
    from fastbrowse.evals.benchmarks import Approval, check_protocol_change, protocol_change

    monkeypatch.chdir(tmp_path)
    (tmp_path / "docs/results").mkdir(parents=True)
    old, old_approval = _bytes(candidate), b'{"approval": "old"}\n'
    base = Candidate.model_validate(candidate)
    changed = Candidate.model_validate(_changed(candidate))
    record = protocol_change(changed, base, old, "Uncapped protocol (#356)")
    Path(record.archived_figures).write_bytes(old)
    Path(record.archived_approval).write_bytes(old_approval)
    assert check_protocol_change(changed, base, old, old_approval, Approval.model_validate(_approval(b"", record))) == {
        "internal-hosted"
    }
    wider = record.model_copy(
        update={"groups": [*record.groups, record.groups[0].model_copy(update={"id": "bu-bench"})]}
    )
    narrower = record.model_copy(update={"groups": []})
    stale = record.model_copy(update={"previous_candidate_sha256": "0" * 64})
    for wrong in (wider, narrower, stale):
        approval = Approval.model_validate(_approval(b"", wrong)) if wrong.groups else None
        if approval is None:
            with pytest.raises(ValidationError):
                Approval.model_validate(_approval(b"", wrong))
            continue
        with pytest.raises(ValueError, match="exactly the changed groups"):
            check_protocol_change(changed, base, old, old_approval, approval)
    Path(record.archived_approval).unlink()
    with pytest.raises(ValueError, match="stay published"):
        check_protocol_change(changed, base, old, old_approval, Approval.model_validate(_approval(b"", record)))


def test_install_archives_the_superseded_figures(candidate, tmp_path, monkeypatch):
    from fastbrowse.evals.benchmarks import install, protocol_change

    monkeypatch.chdir(tmp_path)
    results = tmp_path / "docs/results"
    results.mkdir(parents=True)
    old, old_approval = _bytes(candidate), b'{"approval": "old"}\n'
    (results / "benchmarks.json").write_bytes(old)
    (results / "benchmark-approval.json").write_bytes(old_approval)
    new = _bytes(_changed(candidate))
    staged = results / "benchmark-candidate.json"
    staged.write_bytes(new)
    record = protocol_change(Candidate.model_validate_json(new), Candidate.model_validate(candidate), old, "Note")
    approval = tmp_path / "benchmark-approval.json"
    approval.write_text(json.dumps(_approval(new, record)))
    install(staged, approval)
    assert (results / "benchmarks.test-campaign.json").read_bytes() == old
    assert (results / "benchmark-approval.test-campaign.json").read_bytes() == old_approval
    assert (results / "benchmarks.json").read_bytes() == new
    assert (results / "benchmark-approval.json").read_bytes() == approval.read_bytes()
    with pytest.raises(ValueError, match="not the ones this protocol change supersedes"):
        install(staged, approval)


def _repository(tmp_path, monkeypatch, base: dict[str, bytes]) -> None:
    """A checkout at `tmp_path` whose base ref holds `base`, with the network-bound checks stubbed."""
    from fastbrowse.evals import benchmarks

    monkeypatch.chdir(tmp_path)
    (tmp_path / "docs/results").mkdir(parents=True)
    (tmp_path / "catalog.json").write_bytes(CATALOG.read_bytes())
    for path, content in base.items():
        (tmp_path / path).write_bytes(content)

    def git_file(ref, path):
        if ref == HEAD and path == "docs/results/benchmark-candidate.json":
            return (tmp_path / path).read_bytes()
        return base.get(path) if ref == "base" else None

    monkeypatch.setattr(benchmarks, "git_file", git_file)
    monkeypatch.setattr(benchmarks, "git_names", lambda ref, d: [Path(p).name for p in base if p.startswith(str(d))])
    monkeypatch.setattr(benchmarks, "check_catalog", lambda *_: None)
    monkeypatch.setattr(benchmarks, "check_fixture", lambda *_: None)
    run = {
        "head_sha": HEAD,
        "head_branch": "main",
        "actor": {"login": "cjber"},
        "triggering_actor": {"login": "cjber"},
        "event": "workflow_dispatch",
        "path": ".github/workflows/publish-evals.yml",
        "status": "completed",
        "conclusion": "success",
    }
    monkeypatch.setattr(benchmarks, "github_run", lambda _: run)


def _publish(tmp_path, candidate: dict, record) -> None:
    new = _bytes(_changed(candidate))
    for name in ("benchmark-candidate.json", "benchmarks.json"):
        (tmp_path / "docs/results" / name).write_bytes(new)
    (tmp_path / "docs/results/benchmark-approval.json").write_text(json.dumps(_approval(new, record)))


def test_check_diff_publishes_an_approved_protocol_change_and_keeps_the_archive(candidate, tmp_path, monkeypatch):
    from fastbrowse.evals.benchmarks import check_diff, protocol_change

    old = _bytes(candidate)
    old_approval = json.dumps(_approval(old)).encode()
    base = {"docs/results/benchmarks.json": old, "docs/results/benchmark-approval.json": old_approval}
    _repository(tmp_path, monkeypatch, base)
    changed = Candidate.model_validate(_changed(candidate))
    record = protocol_change(changed, Candidate.model_validate(candidate), old, "Uncapped protocol (#356)")

    _publish(tmp_path, candidate, None)
    with pytest.raises(ValueError, match="protocol changed: internal-hosted"):
        check_diff("base", Path("catalog.json"))

    _publish(tmp_path, candidate, record)
    with pytest.raises(ValueError, match="stay published"):
        check_diff("base", Path("catalog.json"))

    Path(record.archived_figures).write_bytes(old)
    Path(record.archived_approval).write_bytes(old_approval)
    check_diff("base", Path("catalog.json"))

    wider = record.model_copy(
        update={"groups": [*record.groups, record.groups[0].model_copy(update={"id": "bu-bench"})]}
    )
    _publish(tmp_path, candidate, wider)
    with pytest.raises(ValueError, match="exactly the changed groups"):
        check_diff("base", Path("catalog.json"))


def test_check_diff_refuses_a_rewritten_archive(candidate, tmp_path, monkeypatch):
    from fastbrowse.evals.benchmarks import check_diff

    archived = {"docs/results/benchmarks.release-0.5.20.json": _bytes(candidate)}
    _repository(tmp_path, monkeypatch, archived)
    check_diff("base", Path("catalog.json"))
    (tmp_path / "docs/results/benchmarks.release-0.5.20.json").write_text("{}")
    with pytest.raises(ValueError, match="archived benchmark figures cannot change"):
        check_diff("base", Path("catalog.json"))
    (tmp_path / "docs/results/benchmarks.release-0.5.20.json").unlink()
    with pytest.raises(ValueError, match="archived benchmark figures cannot change"):
        check_diff("base", Path("catalog.json"))


@pytest.mark.parametrize("flag", [False, True])
def test_the_approval_command_accepts_a_protocol_change_only_when_flagged(candidate, tmp_path, monkeypatch, flag):
    import sys

    from fastbrowse.evals import benchmarks

    monkeypatch.setattr(benchmarks, "check_catalog", lambda *_: None)
    baseline, staged, record = tmp_path / "benchmarks.json", tmp_path / "candidate.json", tmp_path / "record.json"
    baseline.write_bytes(_bytes(candidate))
    staged.write_bytes(_bytes(_changed(candidate)))
    argv = ["benchmarks", "--candidate", str(staged), "--baseline", str(baseline)]
    if flag:
        argv += ["--protocol-change", "--protocol-note", "Uncapped protocol (#356)", "--protocol-record", str(record)]
    monkeypatch.setattr(sys, "argv", argv)
    if not flag:
        with pytest.raises(ValueError, match="protocol changed: internal-hosted"):
            benchmarks.main()
        return
    benchmarks.main()
    assert [g["id"] for g in json.loads(record.read_text())["groups"]] == ["internal-hosted"]


# A retirement: the catalog before it carried the task in its suite and listed nothing retired; the shipped catalog
# lists it. The published group ran it; a new group cannot, and is compared on the tasks they share.
RETIRED = "stretch-wizard-correction"
RETIRED_ENTRY = {
    "category": "checkout",
    "expect": "complete",
    "source_fingerprint": "2dc76be2b1f49207",
    "task_version": 8,
}
GROUP_ARMS = {
    "internal-fastbrowse": ("fastbrowse",),
    "internal-hosted": ("fastbrowse", "browser-use"),
    "internal-backend": ("fastbrowse", "fastbrowse-pool"),
}


def _dump(catalog: dict) -> bytes:
    return (json.dumps(catalog, indent=2) + "\n").encode()


def _before(catalog: dict) -> dict:
    before = copy.deepcopy(catalog)
    before.pop("retired")
    arms = ["fastbrowse", "browser-use", "fastbrowse-pool", "cua-codex"]
    before["suites"]["stretch-dev"].insert(4, {"id": RETIRED, **RETIRED_ENTRY, "arms": arms})
    return before


def _group(name: str, catalog: bytes) -> dict:
    arms = GROUP_ARMS[name]
    ids = sorted(
        t["id"] for suite in json.loads(catalog)["suites"].values() for t in suite if set(arms) <= set(t["arms"])
    )
    row = {"status": "complete", "completed": True, "score": 1.0, "seconds": 10.0, "dollars": 0.01, "physical_runs": 1}
    return {
        "id": name,
        "agent_sha": "a" * 40,
        "runner_sha": "b" * 40,
        "metric": "task-success",
        "source_sha256": hashlib.sha256(catalog).hexdigest(),
        "task_ids": ids,
        "repeats": 3,
        "limits": ["Matched recorded limits"],
        "rows": [{"task": t, "repeat": r, "arm": a, **row} for t in ids for r in range(3) for a in arms],
    }


def _published(candidate: dict, name: str, catalog: bytes) -> Candidate:
    return Candidate.model_validate({**candidate, "schema_version": 4, "groups": [_group(name, catalog)]})


def _transition(candidate: dict, monkeypatch, name: str, before: dict, after: dict, ids: list[str] | None = None):
    from fastbrowse.evals import benchmarks

    old, new = _dump(before), _dump(after)
    monkeypatch.setattr(
        benchmarks, "catalog_at", lambda digest: old if digest == hashlib.sha256(old).hexdigest() else None
    )
    group = _group(name, new)
    if ids is not None:
        arms = GROUP_ARMS[name]
        template = group["rows"][0]
        group["task_ids"] = ids
        group["rows"] = [{**template, "task": t, "repeat": r, "arm": a} for t in ids for r in range(3) for a in arms]
    published = _published(candidate, name, old)
    regressions(Candidate.model_validate({**candidate, "schema_version": 4, "groups": [group]}), published, new)


@pytest.mark.parametrize("name", sorted(GROUP_ARMS))
def test_a_group_may_drop_exactly_the_listed_retired_tasks(candidate, monkeypatch, name):
    catalog = json.loads(CATALOG.read_bytes())
    assert [r["id"] for r in catalog["retired"]] == [RETIRED]
    _transition(candidate, monkeypatch, name, _before(catalog), catalog)


@pytest.mark.parametrize("name", sorted(GROUP_ARMS))
def test_the_published_group_ran_the_retired_task(candidate, name):
    published = _published(candidate, name, _dump(_before(json.loads(CATALOG.read_bytes()))))
    assert RETIRED in published.groups[0].task_ids


@pytest.mark.parametrize("name", sorted(GROUP_ARMS))
def test_an_unlisted_removal_is_a_protocol_change(candidate, monkeypatch, name):
    catalog = json.loads(CATALOG.read_bytes())
    unlisted = {**catalog, "retired": []}
    with pytest.raises(ValueError, match="protocol changed"):
        _transition(candidate, monkeypatch, name, _before(catalog), unlisted)


@pytest.mark.parametrize("name", sorted(GROUP_ARMS))
def test_a_retirement_with_a_changed_task_version_is_a_protocol_change(candidate, monkeypatch, name):
    catalog = json.loads(CATALOG.read_bytes())
    bumped = copy.deepcopy(catalog)
    bumped["suites"]["core"][0]["task_version"] += 1
    with pytest.raises(ValueError, match="protocol changed"):
        _transition(candidate, monkeypatch, name, _before(catalog), bumped)


@pytest.mark.parametrize("name", sorted(GROUP_ARMS))
def test_a_retirement_with_an_added_task_is_a_protocol_change(candidate, monkeypatch, name):
    catalog = json.loads(CATALOG.read_bytes())
    added = copy.deepcopy(catalog)
    new = {**RETIRED_ENTRY, "id": "stretch-new-task", "arms": ["fastbrowse", "browser-use", "fastbrowse-pool"]}
    added["suites"]["stretch-dev"].append(new)
    ids = sorted({*_group(name, _dump(catalog))["task_ids"], "stretch-new-task"})
    with pytest.raises(ValueError, match="protocol changed"):
        _transition(candidate, monkeypatch, name, _before(catalog), added, ids)


@pytest.mark.parametrize("name", sorted(GROUP_ARMS))
def test_a_retirement_without_the_catalog_or_with_new_limits_is_a_protocol_change(candidate, monkeypatch, name):
    catalog = json.loads(CATALOG.read_bytes())
    with pytest.raises(ValueError, match="protocol changed"):
        _transition(candidate, monkeypatch, name, _before(catalog), {**catalog, "arms": {}})
    before = _published(candidate, name, _dump(_before(catalog)))
    after = Candidate.model_validate({**candidate, "schema_version": 4, "groups": [_group(name, _dump(catalog))]})
    with pytest.raises(ValueError, match="protocol changed"):
        regressions(after, before)


def test_the_shipped_catalog_retires_exactly_from_the_one_before() -> None:
    from fastbrowse.evals.benchmarks import retired_between

    catalog = json.loads(CATALOG.read_bytes())
    assert retired_between(_dump(_before(catalog)), CATALOG.read_bytes()) == {RETIRED}
    assert retired_between(CATALOG.read_bytes(), CATALOG.read_bytes()) == set()


@pytest.mark.parametrize("name", sorted(GROUP_ARMS))
def test_a_source_digest_change_that_retires_nothing_is_a_protocol_change(candidate, monkeypatch, name):
    catalog = json.loads(CATALOG.read_bytes())
    moved = copy.deepcopy(catalog)
    moved["task_sources"] = {source: "0" * 64 for source in moved["task_sources"]}
    with pytest.raises(ValueError, match="protocol changed"):
        _transition(candidate, monkeypatch, name, catalog, moved)


def test_internal_fastbrowse_both_retired_and_uncapped_publishes_only_as_a_protocol_change(
    candidate, tmp_path, monkeypatch
):
    """The first uncapped internal-fastbrowse figures: the catalog retired a task since the published figures and the
    limits changed too. The retirement alone would be compared on the shared tasks; with the new protocol it is a
    change only the owner's flag publishes, recorded as changed and never compared with the old figures."""
    import sys

    from fastbrowse.evals import benchmarks

    catalog = json.loads(CATALOG.read_bytes())
    old_catalog = _dump(_before(catalog))
    monkeypatch.setattr(
        benchmarks, "catalog_at", lambda d: old_catalog if d == hashlib.sha256(old_catalog).hexdigest() else None
    )
    published = _published(candidate, "internal-fastbrowse", old_catalog)
    group = _group("internal-fastbrowse", CATALOG.read_bytes())
    retired_only = Candidate.model_validate({**candidate, "schema_version": 4, "groups": [group]})
    regressions(retired_only, published, CATALOG.read_bytes())
    assert benchmarks.unapproved_changes(retired_only, published, CATALOG.read_bytes()) == set()

    group["limits"] = ["No arm capped"]
    uncapped = {**candidate, "schema_version": 4, "groups": [group]}
    parsed = Candidate.model_validate(uncapped)
    assert benchmarks.unapproved_changes(parsed, published, CATALOG.read_bytes()) == {"internal-fastbrowse"}
    with pytest.raises(ValueError, match="protocol changed: internal-fastbrowse"):
        regressions(parsed, published, CATALOG.read_bytes())

    baseline, staged, record = tmp_path / "benchmarks.json", tmp_path / "candidate.json", tmp_path / "record.json"
    baseline.write_text(published.model_dump_json())
    staged.write_bytes(_bytes(uncapped))
    argv = ["benchmarks", "--candidate", str(staged), "--baseline", str(baseline), "--catalog", str(CATALOG)]
    monkeypatch.setattr(sys, "argv", argv)
    with pytest.raises(ValueError, match="protocol changed: internal-fastbrowse"):
        benchmarks.main()
    flags = ["--protocol-change", "--protocol-note", "Uncapped protocol (#356); stretch-wizard-correction retired"]
    monkeypatch.setattr(sys, "argv", [*argv, *flags, "--protocol-record", str(record)])
    benchmarks.main()
    [change] = json.loads(record.read_text())["groups"]
    assert change["id"] == "internal-fastbrowse" and change["limits"] == ["No arm capped"]
    assert RETIRED in change["previous_task_ids"] and RETIRED not in change["task_ids"]
    assert len(change["previous_task_ids"]) == 54 and len(change["task_ids"]) == 53
