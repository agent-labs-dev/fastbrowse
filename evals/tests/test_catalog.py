import pytest
from fastbrowse.evals.catalog import PATH

from fastbrowse_evals import catalog
from fastbrowse_evals.catalog import export


def test_public_catalog_is_an_exact_export_of_the_private_tasks() -> None:
    export(PATH, check=True)


def test_retired_tasks_are_listed_with_a_reason_and_run_nowhere() -> None:
    import json

    from fastbrowse_evals import live

    rendered = json.loads(catalog.render())
    assert rendered["retired"] == list(catalog.RETIRED)
    running = {t.id for suite in live.SUITES.values() for t in suite}
    assert {r["id"] for r in catalog.RETIRED} == {"stretch-wizard-correction"}
    assert all(r["id"] not in running and r["reason"] and r["suite"] in live.SUITES for r in catalog.RETIRED)


def test_every_head_to_head_exclusion_is_exported_with_its_reason() -> None:
    import dataclasses
    import json

    from fastbrowse_evals import live

    rendered = {t["id"]: t for suite in json.loads(catalog.render())["suites"].values() for t in suite}
    for task in (t for suite in live.SUITES.values() for t in suite):
        excluded = rendered[task.id]["excluded"]
        assert set(excluded) == {arm for arm in live.HEAD_TO_HEAD_ARMS if not live.eligible(arm, task)}
        assert all(excluded.values())
    skipped = {arm: {i for i, t in rendered.items() if arm in t["excluded"]} for arm in live.HEAD_TO_HEAD_ARMS}
    # The hosted agent's final page is read through its browser, so only the unauthorized-step task is left out.
    assert skipped["browser-use"] == {"saucedemo-pause"}
    assert skipped["fastbrowse"] == set()
    assert len(skipped["browser-harness"]) == len(rendered) - 47
    lookup = next(t for t in live.TASKS if t.id == "pypi-version")
    with pytest.raises(ValueError, match="without a stated reason"):
        live.excluded_because("browser-use", dataclasses.replace(lookup, arms=("fastbrowse",)))


def test_both_hosted_browser_use_arms_are_catalogued_on_the_same_tasks() -> None:
    """bu-ultrafast is the same hosted agent on another model: it runs and skips exactly what the default model does."""
    import json

    rendered = json.loads(catalog.render())
    assert rendered["arms"]["browser-use"]["pin"].endswith("API v4; default model, reported per row")
    assert rendered["arms"]["browser-use-ultrafast"]["pin"].endswith("API v4; model bu-ultrafast, reported per row")
    for suite in rendered["suites"].values():
        for task in suite:
            assert ("browser-use" in task["arms"]) == ("browser-use-ultrafast" in task["arms"])
            excluded = task.get("excluded", {})
            assert excluded.get("browser-use") == excluded.get("browser-use-ultrafast")


def test_an_exclusion_reason_the_site_cannot_parse_refuses_the_export(monkeypatch) -> None:
    from fastbrowse_evals import live

    monkeypatch.setattr(catalog, "excluded_because", lambda arm, task: None if live.eligible(arm, task) else "a/b")
    with pytest.raises(ValueError, match="reason"):
        catalog.render()


def test_every_exclusion_reason_reads_plainly_after_the_agents_name() -> None:
    """The site shows each reason after the agent's name ("Browser Harness: cannot attempt 6 tasks (...): <reason>"),
    so a reason names no harness arm id and no harness term a first-time reader would not know."""
    import json

    from fastbrowse_evals import live

    rendered = json.loads(catalog.render())
    reasons = {r for suite in rendered["suites"].values() for t in suite for r in t["excluded"].values()}
    assert reasons
    for reason in reasons:
        words = set(reason.replace(":", " ").replace(",", " ").split())
        assert not words & (set(live.ARMS) | {"arm", "DONE", "BLOCKED", "V4"}), reason
