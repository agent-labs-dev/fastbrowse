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
