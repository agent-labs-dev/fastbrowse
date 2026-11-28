"""The campaign's validation step tells an arm with no eligible task from an arm that lost its run."""

import shutil
import subprocess
from pathlib import Path

import pytest
import yaml

REPO = Path(__file__).resolve().parent.parent


def validation_script() -> str:
    workflow = yaml.safe_load((REPO / ".github/workflows/benchmark-campaign.yml").read_text())
    steps = workflow["jobs"]["track"]["steps"]
    script = next(step["run"] for step in steps if step.get("name") == "Check the Parallax run artifacts")
    return script.replace("${{ matrix.label }}", "head-to-head / browser-harness")


def validate(tmp_path: Path, files: list[str]) -> tuple[int, str]:
    bash = shutil.which("bash")
    if bash is None:
        pytest.skip("the validation step needs Bash")
    out = tmp_path / "out"
    for name in files:
        path = out / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("{}")
    github_output = tmp_path / "github-output"
    github_output.touch()
    done = subprocess.run(
        [bash, "-e", "-c", validation_script()],
        env={"OUT": str(out), "GITHUB_OUTPUT": str(github_output), "PATH": "/usr/bin:/bin"},
        capture_output=True,
        text=True,
        check=False,
    )
    return done.returncode, github_output.read_text()


def test_an_arm_with_no_eligible_task_passes_without_a_report(tmp_path: Path) -> None:
    code, output = validate(tmp_path, ["mirror-c-g-browser-harness/no-eligible-tasks"])
    assert code == 0 and "eligible=false" in output


def test_an_arm_that_lost_its_run_still_fails(tmp_path: Path) -> None:
    code, _ = validate(tmp_path, [])
    assert code != 0


def test_a_complete_run_is_reported(tmp_path: Path) -> None:
    run = "mirror-c-g-fastbrowse/"
    code, output = validate(tmp_path, [run + "results.jsonl", run + "provenance.json", run + "parallax-verify.json"])
    assert code == 0 and "eligible=true" in output


def test_a_run_missing_its_receipt_files_fails(tmp_path: Path) -> None:
    code, _ = validate(tmp_path, ["mirror-c-g-fastbrowse/parallax-verify.json"])
    assert code != 0
