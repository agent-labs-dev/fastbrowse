import pytest
from pydantic import ValidationError

from fastbrowse.evals.catalog import CATALOG, Catalog


def test_catalog_rejects_a_grader_identity_not_in_the_version_lock() -> None:
    data = CATALOG.model_dump(mode="json")
    data["suites"]["core"][0]["source_fingerprint"] = "0" * 16
    with pytest.raises(ValidationError, match="task-version lock"):
        Catalog.model_validate(data)


def test_catalog_rejects_unknown_arms() -> None:
    data = CATALOG.model_dump(mode="json")
    data["suites"]["core"][0]["arms"] = ["invented-agent"]
    with pytest.raises(ValidationError, match="arm"):
        Catalog.model_validate(data)


def test_all_task_versions_include_shared_grading_changes() -> None:
    from fastbrowse.evals import versions

    suites, fixtures = versions.all_tasks()
    tasks = [*(task for suite in suites.values() for task in suite), *fixtures]
    assert versions.mismatches(tasks) == []


CREDENTIAL_TASKS = {
    "saucedemo-cart",
    "expandtesting-login",
    "practice-login",
    "saucedemo-locked-out",
    "saucedemo-checkout",
    "saucedemo-pause",
}
BROWSER_USE_1 = ("browser-use-1-luna",)


def test_the_browser_use_1_arms_follow_the_credentials_policy_of_browser_use_oss() -> None:
    for task in (t for tasks in CATALOG.suites.values() for t in tasks):
        for arm in BROWSER_USE_1:
            if task.id in CREDENTIAL_TASKS:
                assert arm not in task.arms
                assert task.excluded[arm] == task.excluded["browser-use-oss"]
            else:
                assert (arm in task.arms) == ("browser-use-oss" in task.arms)
                assert arm not in task.excluded


def test_the_heldout_suite_runs_the_arms_every_other_suite_runs() -> None:
    heldout = CATALOG.suites["heldout"]
    assert len(heldout) >= 6
    assert {task.arms for task in heldout} == {CATALOG.suites["dev"][0].arms}
    assert not any(task.excluded for task in heldout)
