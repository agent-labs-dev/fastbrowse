"""Required CI checks fail closed when a dependency fails or does not run."""

import os
import re
import shlex
import subprocess
from pathlib import Path
from typing import Any

import pytest
import yaml

RESULTS = (
    "GATE_RESULT",
    "EVALS_RESULT",
    "PROJECT_RULES_RESULT",
    "SDK_RESULT",
    "RUST_RESULT",
    "SECRETS_RESULT",
    "CHANGES_RESULT",
    "REPLAY_RESULT",
    "BLOG_RESULT",
)
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
    A path filter on the job would let that change merge without the Rust checks seeing it. Only a
    real-site dispatch, which checks no change, skips it."""
    rust = _jobs()["rust"]
    assert rust["if"] == "${{ !inputs.task }}"
    assert "needs" not in rust


def _binaries_pathspecs() -> list[str]:
    built = next(step for step in _jobs()["changes"]["steps"] if step.get("id") == "built")
    listed = built["run"].split(" -- ", 1)[1].split("\nthen", 1)[0]
    return shlex.split(listed.replace("\\\n", " "))


def test_every_file_a_rust_crate_embeds_rebuilds_the_binaries() -> None:
    """The Rust binary compiles in files (the page scripts), so a change to one of them must rebuild and smoke the
    binaries, or the shipped binary breaks on a page script it was never run against. A file in a crate's own
    `tests/` directory is a test fixture, which no shipped binary compiles in."""
    embedded = set()
    for source in Path("crates").rglob("*.rs"):
        for included in re.findall(r'include_(?:str|bytes)!\("([^"]+)"\)', source.read_text(encoding="utf-8")):
            path = (source.parent / included).resolve().relative_to(Path.cwd())
            if path.parts[2] != "tests":
                embedded.add(path.as_posix())
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


@pytest.mark.parametrize(
    ("payload", "passes"),
    [
        ("null", True),
        ("{}", True),
        ('{"groups": "all", "suites": "core"}', True),
        ('{"groups": ""}', False),
        ('{"suites": ""}', False),
    ],
)
def test_a_campaign_refuses_an_empty_choice_rather_than_widening(payload: str, passes: bool) -> None:
    workflow = Path(".github/workflows/benchmark-campaign.yml").read_text()
    start = workflow.index("for key in groups suites")
    check = workflow[start : workflow.index("done\n", start) + 4]
    result = subprocess.run(["bash", "-c", check], env={**os.environ, "PAYLOAD": payload}, capture_output=True)
    assert (result.returncode == 0) is passes


@pytest.mark.parametrize(
    ("payload", "passes"),
    [
        ("null", True),
        ('{"smoke": true, "refresh_competitors": false}', True),
        ('{"smoke": "true"}', False),
        ('{"refresh_competitors": 1}', False),
    ],
)
def test_a_campaign_takes_only_a_json_boolean_for_smoke(payload: str, passes: bool) -> None:
    # A string "true" is not true to the workflow's expressions, so it would start a full paid run.
    workflow = Path(".github/workflows/benchmark-campaign.yml").read_text()
    start = workflow.index("for key in smoke refresh_competitors")
    check = workflow[start : workflow.index("done\n", start) + 4]
    result = subprocess.run(["bash", "-c", check], env={**os.environ, "PAYLOAD": payload}, capture_output=True)
    assert (result.returncode == 0) is passes


@pytest.mark.parametrize(
    ("suites", "passes"),
    [
        ("core dev stretch-dev", True),
        ("stretch-dev", True),
        # A suite the catalog no longer carries, because Parallax moved its tasks into dev.
        ("core dev heldout", False),
        ("   ", False),
        ("core --output /tmp/elsewhere", False),
        ("core,dev", False),
    ],
)
def test_a_campaign_refuses_a_suite_outside_the_catalog(suites: str, passes: bool) -> None:
    workflow = Path(".github/workflows/benchmark-campaign.yml").read_text()
    start = workflow.index('read -ra chosen <<< "$SUITES"')
    check = workflow[start : workflow.index("          done\n", start) + 15]
    result = subprocess.run(["bash", "-c", check], env={**os.environ, "SUITES": suites}, capture_output=True)
    assert (result.returncode == 0) is passes


def test_registry_parity_takes_a_version_only_from_a_tag_pushed_release() -> None:
    """A dispatched Release rehearsal runs on a branch, and its name is not a version to check against."""
    steps = yaml.safe_load(Path(".github/workflows/registry-parity.yml").read_text())["jobs"]["parity"]["steps"]
    release = next(step for step in steps if step.get("id") == "release")
    assert "github.event.workflow_run.event == 'push'" in release["if"]


def _campaign_steps(job: str) -> list[dict[str, Any]]:
    return yaml.safe_load(Path(".github/workflows/benchmark-campaign.yml").read_text())["jobs"][job]["steps"]


def _step(steps: list[dict[str, Any]], name: str) -> int:
    return next(i for i, step in enumerate(steps) if step.get("name") == name)


def _uses(steps: list[dict[str, Any]], action: str) -> int:
    return next(i for i, step in enumerate(steps) if str(step.get("uses", "")).startswith(action))


@pytest.mark.parametrize("sent", [True, False])
def test_a_failed_langfuse_send_leaves_the_campaign_its_result(tmp_path: Path, sent: bool) -> None:
    """The rows are kept before the send, and a send Langfuse or the network drops warns instead of failing a group
    whose runs all finished; the candidate job sends them again."""
    steps = _campaign_steps("campaign")
    send = _step(steps, "Send the ledger to Langfuse")
    assert _uses(steps, "actions/upload-artifact") < send
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "uv").write_text(f"#!/bin/sh\nexit {0 if sent else 1}\n")
    (bin_dir / "uv").chmod(0o755)
    (tmp_path / "harness" / "evals").mkdir(parents=True)
    ledger = tmp_path / "head-to-head.jsonl"
    ledger.write_text("{}\n")
    env = {**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}", "OUT": str(ledger), "SMOKE": "true"}
    result = subprocess.run(
        ["bash", "-e", "-c", steps[send]["run"]], cwd=tmp_path, env=env, capture_output=True, text=True
    )
    assert result.returncode == 0
    assert ("::warning title=Ledger not sent to Langfuse" in result.stdout) is not sent


def test_the_candidate_sends_this_runs_ledgers_before_it_reads_them_back() -> None:
    """`publish-candidate` refuses a group Langfuse does not hold, so a send the campaign job could not finish is
    repeated from this run's downloaded ledgers rather than skipping the candidate."""
    steps = _campaign_steps("stage")
    send = _step(steps, "Send this run's ledgers to Langfuse")
    download = _uses(steps, "actions/download-artifact")
    assert download < send < _step(steps, "Build the candidate")
    assert steps[send]["if"] == steps[download]["if"]
    assert "track-ledger" in steps[send]["run"]
