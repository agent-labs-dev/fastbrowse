"""A Store compatibility refusal must precede every tagged release's publication."""

from pathlib import Path

import pytest
import yaml

WORKFLOW = Path(".github/workflows/release.yml")
# The jobs that publish. A registry this workflow no longer publishes to (crates, maven, nuget) has no job here and
# so nothing to check; when one returns it is covered again by being named.
PUBLISHERS = ["github-release", "pypi", "npm", "npm-only", "crates", "maven", "nuget", "go", "aur"]
PRESENT = [name for name in PUBLISHERS if name in yaml.safe_load(WORKFLOW.read_text())["jobs"]]


@pytest.mark.parametrize("publisher", PRESENT)
def test_store_gate_precedes_publication(publisher: str) -> None:
    jobs = yaml.safe_load(WORKFLOW.read_text())["jobs"]
    gate_jobs = {
        name
        for name, job in jobs.items()
        if any("scripts/extension_store_gate.py" in step.get("run", "") for step in job.get("steps", []))
    }
    assert gate_jobs, "a tagged release must check Store compatibility"

    def prerequisites(name: str) -> set[str]:
        needs = jobs[name].get("needs", [])
        if isinstance(needs, str):
            needs = [needs]
        return {name}.union(*(prerequisites(needed) for needed in needs))

    assert gate_jobs & prerequisites(publisher), (
        f"{publisher} can publish a build the Store cannot support before the Store gate passes"
    )
    assert publisher not in gate_jobs, f"{publisher} would have the Store checked only once it is already running"


def test_a_publishing_registry_that_exists_is_checked() -> None:
    # Guards against the list silently emptying: the release does publish somewhere.
    assert PRESENT, "no publishing jobs found in the release workflow"
