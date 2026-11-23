"""Required CI checks fail closed when a dependency fails or does not run."""

import json
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
        ('{"repeat": "3"}', True),
        ('{"repeat": 0}', False),
        ('{"repeat": ""}', False),
        ('{"repeat": null}', False),
    ],
)
def test_a_campaign_validates_repeat_when_the_payload_provides_it(payload: str, passes: bool) -> None:
    workflow = Path(".github/workflows/benchmark-campaign.yml").read_text()
    start = workflow.index("jq -e '(. // {}) | if has(\"repeat\")")
    check = workflow[start : workflow.index('case "$REPEAT"', start)]
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


def test_a_failed_langfuse_send_leaves_the_campaign_job_and_reruns_from_its_artifact() -> None:
    """A network fault at Langfuse fails only the send, which "Re-run failed jobs" repeats from the run artifact."""
    jobs = yaml.safe_load(Path(".github/workflows/benchmark-campaign.yml").read_text())["jobs"]
    assert not any("track-ledger" in step.get("run", "") for step in jobs["campaign"]["steps"])
    track = jobs["track"]
    assert "campaign" in track["needs"]
    assert "always()" in track["if"]
    downloads = [step for step in track["steps"] if step.get("uses", "").startswith("actions/download-artifact@")]
    assert [step["with"]["pattern"] for step in downloads] == ["run-${{ matrix.key }}"]
    assert any("Check the Parallax run artifacts" in step.get("name", "") for step in track["steps"])
    assert any("parallax-verify.json" in step.get("run", "") for step in track["steps"])
    assert "track" in jobs["stage"]["needs"]
    assert "needs.track.result" in jobs["stage"]["if"]


def test_registry_parity_takes_a_version_only_from_a_tag_pushed_release() -> None:
    """A dispatched Release rehearsal runs on a branch, and its name is not a version to check against."""
    steps = yaml.safe_load(Path(".github/workflows/registry-parity.yml").read_text())["jobs"]["parity"]["steps"]
    release = next(step for step in steps if step.get("id") == "release")
    assert "github.event.workflow_run.event == 'push'" in release["if"]


def _campaign_step(name: str) -> dict[str, Any]:
    jobs = yaml.safe_load(Path(".github/workflows/benchmark-campaign.yml").read_text())["jobs"]
    return next(step for step in jobs["resolve"]["steps"] if step.get("name") == name)


@pytest.mark.parametrize(
    "concurrency,passes",
    [
        ("", True),
        ("4", True),
        ("8", True),
        ("1", True),
        ("0", False),
        ("00", False),
        ("-1", False),
        ("1.5", False),
        ("four", False),
    ],
)
def test_campaign_admits_positive_concurrency_without_extra_arms(concurrency: str, passes: bool) -> None:
    result = _require_protocol(RUN_CONCURRENCY=concurrency)
    assert (result.returncode == 0) is passes, result.stdout + result.stderr


@pytest.mark.parametrize(
    "arm,passes", [("agent-browser", True), ("browser-harness", True), ("cua-codex", False), ("nonesuch", False)]
)
def test_campaign_head_to_head_arms_admit_only_competitor_arms(arm: str, passes: bool) -> None:
    result = _require_protocol(GROUPS_CHOSEN="head-to-head", HEAD_TO_HEAD_ARMS=arm)
    assert (result.returncode == 0) is passes, result.stdout + result.stderr


def _require_protocol(**overrides: str) -> subprocess.CompletedProcess[str]:
    env = dict.fromkeys(("EXTRA_ARMS", "HEAD_TO_HEAD_ARMS", "OFFICIAL_RUNS", "ONLY", "RUN_CONCURRENCY"), "")
    env |= {
        "GROUPS_CHOSEN": "all",
        "CANDIDATE_ONLY": "false",
        "SMOKE": "false",
        "REFRESH": "false",
        "SUITES": "core dev stretch-dev",
        "PAYLOAD": "null",
        "REPEAT": "3",
        "BUDGET": "38",
        "RESERVE": "3",
        "CAMPAIGN_ID": "test",
    } | overrides
    return subprocess.run(
        ["bash", "-e", "-c", _campaign_step("Require the full comparison protocol")["run"]],
        env=os.environ | env,
        capture_output=True,
        text=True,
    )


@pytest.mark.parametrize("concurrency", ["", "4"])
@pytest.mark.parametrize("smoke", ["true", "false"])
@pytest.mark.parametrize("groups", ["all", "fastbrowse", "head-to-head"])
def test_campaign_matrix_preserves_concurrency_for_every_group(
    tmp_path: Path, concurrency: str, smoke: str, groups: str
) -> None:
    output, summary = tmp_path / "output", tmp_path / "summary"
    env = {
        "RUN_CONCURRENCY": concurrency,
        "SMOKE": smoke,
        "GROUP_SET": groups,
        "REFRESH": "false",
        "EXTRA_ARMS": "fastbrowse-hosted",
        "HEAD_TO_HEAD_ARMS": "",
        "GITHUB_OUTPUT": str(output),
        "GITHUB_STEP_SUMMARY": str(summary),
    }
    subprocess.run(
        ["bash", "-e", "-c", _campaign_step("Build the group matrix")["run"]],
        env=os.environ | env,
        capture_output=True,
        text=True,
        check=True,
    )
    assert f"Concurrency: `{concurrency or '8'}`" in summary.read_text()
    matrix = json.loads(output.read_text().removeprefix("matrix="))
    assert {entry["conc"] for entry in matrix} == {concurrency}
    assert {entry["label"] for entry in matrix} == (
        {"fastbrowse", "head-to-head", "hosted"} if groups == "all" else {groups, "hosted"}
    )


@pytest.mark.parametrize(
    "value,passes",
    [(0, False), (4, True), (1.5, False), (True, False), (None, False), ("", True), ("4", True), ("00", False)],
)
def test_campaign_checks_raw_payload_concurrency_before_defaulting(value: Any, passes: bool) -> None:
    script = _campaign_step("Require the full comparison protocol")["run"]
    start = script.index('jq -e \'(. // {}) | if has("concurrency")')
    end = script.index('case "$RUN_CONCURRENCY"', start)
    result = subprocess.run(
        ["bash", "-e", "-c", script[start:end]],
        env=os.environ | {"PAYLOAD": json.dumps({"concurrency": value})},
        capture_output=True,
    )
    assert (result.returncode == 0) is passes


@pytest.mark.parametrize("workflow", ["benchmark-campaign", "official-benchmarks", "paper-campaign"])
@pytest.mark.parametrize(
    ("models", "passes"),
    [
        ("deepseek/deepseek-v4.1-flash-1017", True),
        ("openai/gpt-6.1-terra,deepseek/deepseek-v4.1-flash-1017", True),
        ("anthropic/claude-opus-5.5", False),
        ("deepseek/deepseek-v4.1-flash-1017,anthropic/claude-opus-5.5", False),
        ("deepseek/deepseek-v4.1-flash-1017, anthropic/claude-opus-5.5", False),
        ("deepseek/deepseek-v4.1-flash-1017 , openai/gpt-6.1-terra", True),
    ],
)
def test_a_campaign_refuses_claude_opus_as_a_judge(workflow: str, models: str, passes: bool) -> None:
    text = Path(f".github/workflows/{workflow}.yml").read_text()
    start = text.index('case ",${PARALLAX_JUDGE_MODELS//[[:space:]]/}," in')
    check = text[start : text.index("esac\n", start) + 5]
    env = {**os.environ, "PARALLAX_JUDGE_MODELS": models}
    assert (subprocess.run(["bash", "-c", check], env=env, capture_output=True).returncode == 0) is passes


@pytest.mark.parametrize("workflow", ["official-benchmarks", "paper-campaign"])
def test_the_online_judge_identity_is_the_first_model_without_spaces(workflow: str) -> None:
    text = Path(f".github/workflows/{workflow}.yml").read_text()
    start = text.index('judges="')
    export = text[start : text.index("\n", text.index("export PARALLAX_OFFICIAL_ONLINE_JUDGE_MODEL", start))]
    env = {**os.environ, "PARALLAX_JUDGE_MODELS": "deepseek/deepseek-v4.1-flash-1017 , openai/gpt-6.1-terra"}
    out = subprocess.run(
        ["bash", "-c", export + '; printf "%s" "$PARALLAX_OFFICIAL_ONLINE_JUDGE_MODEL"'],
        env=env,
        capture_output=True,
        text=True,
    )
    assert out.stdout == "deepseek/deepseek-v4.1-flash-1017"


def test_the_official_online_judge_is_the_panels_first_model_not_a_second_setting() -> None:
    """The upstream judge reads its own variable, so a repository setting could name Opus past the panel check."""
    text = Path(".github/workflows/official-benchmarks.yml").read_text()
    assert "vars.PARALLAX_OFFICIAL_ONLINE_JUDGE_MODEL" not in text
    assert 'export PARALLAX_OFFICIAL_ONLINE_JUDGE_MODEL="${judges%%,*}"' in text


@pytest.mark.parametrize("status", [0, 1])
def test_a_run_whose_upload_verified_keeps_its_receipt_and_its_own_exit_status(tmp_path: Path, status: int) -> None:
    """Parallax proves the upload before it judges quality, so a quality failure must not hide the receipt."""
    workflow = Path(".github/workflows/benchmark-campaign.yml").read_text()
    start = workflow.index("# Parallax writes this receipt")
    end = workflow.index('test "$status" -eq 0 || exit "$status"', start)
    block = workflow[start : end + len('test "$status" -eq 0 || exit "$status"')]
    run_out, mirror = tmp_path / "run", tmp_path / "mirror"
    mirror.mkdir()
    receipt = tmp_path / "run.verified.json"
    receipt.write_text(json.dumps({"verified": True, "run_id": "r1"}))
    result = subprocess.run(
        ["bash", "-c", f"set -e; status={status}; run_out={run_out}; mirror={mirror}; OUT={tmp_path}\n{block}"],
        capture_output=True,
    )
    assert result.returncode == status
    assert json.loads((mirror / "parallax-verify.json").read_text()) == {"verified": True, "run_id": "r1"}


def test_a_run_without_a_verified_upload_leaves_no_receipt(tmp_path: Path) -> None:
    workflow = Path(".github/workflows/benchmark-campaign.yml").read_text()
    start = workflow.index("# Parallax writes this receipt")
    end = workflow.index('test "$status" -eq 0 || exit "$status"', start)
    block = workflow[start : end + len('test "$status" -eq 0 || exit "$status"')]
    mirror = tmp_path / "mirror"
    mirror.mkdir()
    subprocess.run(
        ["bash", "-c", f"set -e; status=0; run_out={tmp_path / 'run'}; mirror={mirror}; OUT={tmp_path}\n{block}"],
        check=True,
    )
    assert not (mirror / "parallax-verify.json").exists()
