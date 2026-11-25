"""A campaign's task set comes from the catalog: its suites by default, and each arm's exclusions."""

import json
from pathlib import Path

import yaml

from fastbrowse_evals.catalog import PATH, excluded_ids

WORKFLOW = Path(__file__).parents[2] / ".github/workflows/benchmark-campaign.yml"


def test_an_arm_skips_the_tasks_the_catalog_excludes_it_from() -> None:
    catalog = json.loads(PATH.read_text())
    login = {t["id"] for tasks in catalog["suites"].values() for t in tasks if t["id"].endswith("-login")}
    assert login and login <= excluded_ids("browser-use-1-luna")
    assert "saucedemo-pause" in excluded_ids("browser-use-1-luna")
    assert excluded_ids("fastbrowse") == set()


def test_default_suites_are_the_catalogs_not_a_list_kept_beside_it() -> None:
    workflow = yaml.safe_load(WORKFLOW.read_text())
    assert workflow[True]["workflow_dispatch"]["inputs"]["suites"]["default"] == ""
    assert "|| '' }}" in workflow["env"]["SUITES"]
