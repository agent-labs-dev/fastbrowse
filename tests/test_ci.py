"""Required CI checks fail closed when a dependency fails or does not run."""

import os
import subprocess
from pathlib import Path

import pytest
import yaml

RESULTS = ("GATE_RESULT", "SIFT_RESULT", "SDK_RESULT", "SECRETS_RESULT", "CHANGES_RESULT")
# Left unbuilt by a change that cannot alter them, which is a pass.
OPTIONAL = ("BINARIES_RESULT", "AUR_RESULT")


def run_check(**results: str) -> subprocess.CompletedProcess[str]:
    workflow = yaml.safe_load(Path(".github/workflows/ci.yml").read_text())
    script = workflow["jobs"]["check"]["steps"][0]["run"]
    env = {**os.environ, **dict.fromkeys((*RESULTS, *OPTIONAL), "success"), **results}
    return subprocess.run(["bash", "-e", "-c", script], env=env, capture_output=True, text=True)


@pytest.mark.parametrize("dependency", (*RESULTS, *OPTIONAL))
@pytest.mark.parametrize("result", ("failure", "cancelled"))
def test_required_check_rejects_failed_dependency(dependency: str, result: str) -> None:
    assert run_check(**{dependency: result}).returncode != 0


@pytest.mark.parametrize("dependency", RESULTS)
def test_required_check_rejects_skipped_dependency(dependency: str) -> None:
    assert run_check(**{dependency: "skipped"}).returncode != 0


@pytest.mark.parametrize("dependency", OPTIONAL)
@pytest.mark.parametrize("result", ("success", "skipped"))
def test_required_check_accepts_an_unbuilt_optional_job(dependency: str, result: str) -> None:
    assert run_check(**{dependency: result}).returncode == 0
