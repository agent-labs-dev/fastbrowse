"""Required CI checks fail closed when a dependency fails or does not run."""

import os
import re
import shlex
import subprocess
from pathlib import Path
from typing import Any

import pytest
import yaml

RESULTS = ("GATE_RESULT", "SIFT_RESULT", "SDK_RESULT", "RUST_RESULT", "SECRETS_RESULT", "CHANGES_RESULT")
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


def _jobs() -> dict[str, Any]:
    return yaml.safe_load(Path(".github/workflows/ci.yml").read_text())["jobs"]


def test_the_rust_job_runs_on_every_pull_request() -> None:
    """The Rust crates read fixtures from outside crates/, so a change elsewhere can break them.
    A path filter on the job would let that change merge without the Rust checks seeing it."""
    rust = _jobs()["rust"]
    assert "if" not in rust
    assert "needs" not in rust


def _binaries_pathspecs() -> list[str]:
    built = next(step for step in _jobs()["changes"]["steps"] if step.get("id") == "built")
    listed = built["run"].split(" -- ", 1)[1].split("\nthen", 1)[0]
    return shlex.split(listed.replace("\\\n", " "))


def test_every_file_a_rust_crate_embeds_rebuilds_the_binaries() -> None:
    """The Rust binary compiles in files (the page scripts), so a change to one of them must rebuild and smoke the
    binaries, or the shipped binary breaks on a page script it was never run against."""
    embedded = set()
    for source in Path("crates").rglob("*.rs"):
        for included in re.findall(r'include_(?:str|bytes)!\("([^"]+)"\)', source.read_text(encoding="utf-8")):
            embedded.add((source.parent / included).resolve().relative_to(Path.cwd()).as_posix())
    assert "crates/fastbrowse-browser/js/snapshot.js" in embedded
    covered = subprocess.run(
        ["git", "ls-files", "--", *_binaries_pathspecs()], capture_output=True, text=True, check=True
    ).stdout.splitlines()
    assert embedded <= set(covered), sorted(embedded - set(covered))


@pytest.mark.parametrize(
    ("payload", "passes"),
    [
        ("null", True),
        ("{}", True),
        ('{"repeat": "3", "budget": 50}', True),
        ('{"repeat": 0}', False),
        ('{"budget": ""}', False),
        ('{"budget": 0}', False),
    ],
)
def test_a_campaign_refuses_a_bad_payload_and_runs_with_none(payload: str, passes: bool) -> None:
    workflow = Path(".github/workflows/benchmark-campaign.yml").read_text()
    start = workflow.index("for key in repeat budget")
    check = workflow[start : workflow.index("done\n", start) + 4]
    result = subprocess.run(["bash", "-c", check], env={**os.environ, "PAYLOAD": payload}, capture_output=True)
    assert (result.returncode == 0) is passes
