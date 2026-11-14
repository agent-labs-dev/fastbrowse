"""The release's package synchronization preserves rendered files when reusing its PR branch."""

import os
import shutil
import subprocess
from pathlib import Path

import pytest
import yaml

REPO = Path(__file__).resolve().parent.parent


@pytest.mark.parametrize("existing_branch", [False, True], ids=["first-run", "rerun"])
def test_aur_sync_keeps_the_rendered_package(tmp_path: Path, existing_branch: bool) -> None:
    git = shutil.which("git")
    bash = shutil.which("bash")
    if git is None or bash is None:
        pytest.skip("the release shell regression needs Git and Bash")

    def run_git(*args: str) -> str:
        return subprocess.run(
            [git, "-C", str(tmp_path), *args], capture_output=True, text=True, check=True
        ).stdout.strip()

    def run_remote(*args: str, preserve_newline: bool = False) -> str:
        output = subprocess.run(
            [git, "--git-dir", str(remote), *args], capture_output=True, text=True, check=True
        ).stdout
        return output if preserve_newline else output.strip()

    run_git("init", "-q", "-b", "main")
    run_git("config", "user.name", "Package fixture")
    run_git("config", "user.email", "fixture@example.invalid")
    run_git("config", "commit.gpgsign", "false")
    run_git("config", "core.hooksPath", str(tmp_path / "no-hooks"))
    package = tmp_path / "packaging" / "aur"
    package.mkdir(parents=True)
    for name in ("PKGBUILD", ".SRCINFO"):
        (package / name).write_text("old package\n")
    run_git("add", "packaging/aur")
    run_git("commit", "-qm", "fixture: old package")
    old = run_git("rev-parse", "HEAD")
    remote = tmp_path / "remote.git"
    run_git("init", "-q", "--bare", str(remote))
    run_git("remote", "add", "origin", str(remote))
    branch = "chore/aur-9.9.9"
    rendered = {"PKGBUILD": "new package\n", ".SRCINFO": "new metadata\n"}
    if existing_branch:
        run_git("checkout", "-qb", branch)
        for name, content in rendered.items():
            (package / name).write_text(content)
        run_git("add", "packaging/aur")
        run_git("commit", "-qm", "fixture: release package")
        run_git("push", "-q", "origin", branch)
    expected = run_git("rev-parse", "HEAD")
    run_git("checkout", "-q", "--detach", old)
    for name, content in rendered.items():
        (package / name).write_text(content)

    tools = tmp_path / "tools"
    tools.mkdir()
    # The real workflow must exercise branch checkout, commits and pushes, all against the local bare repository.
    (tools / "git").write_text(
        f'#!/bin/sh\nif [ "$1" = remote ] && [ "$2" = set-url ]; then exit 0; fi\nexec "{git}" "$@"\n'
    )
    (tools / "gh").write_text('#!/bin/sh\nif [ "$2" = list ]; then echo 123; fi\n')
    for name in ("git", "gh"):
        (tools / name).chmod(0o755)
    workflow = yaml.safe_load((REPO / ".github/workflows/release.yml").read_text())
    step = next(
        step for step in workflow["jobs"]["aur-sync"]["steps"] if step.get("name", "").startswith("Open the PR")
    )
    completed = subprocess.run(
        [bash, "-c", step["run"]],
        cwd=tmp_path,
        env=os.environ
        | {
            "PATH": str(tools) + os.pathsep + os.environ["PATH"],
            "RUNNER_TEMP": str(tmp_path),
            "TOKEN": "fixture",
            "VERSION": "9.9.9",
            "GITHUB_REPOSITORY": "fixture/repository",
            "GITHUB_SHA": old,
        },
        capture_output=True,
        text=True,
    )
    assert completed.returncode == 0, completed.stdout + completed.stderr
    assert {name: (package / name).read_text() for name in rendered} == rendered
    assert run_git("status", "--porcelain", "--", "packaging/aur") == ""
    assert run_git("merge-base", "--is-ancestor", old, "HEAD") == ""
    if existing_branch:
        assert run_git("rev-parse", "HEAD") == expected, "a rerun must not replace the existing package commit"
    remote_head = run_remote("rev-parse", f"refs/heads/{branch}")
    assert remote_head == run_git("rev-parse", "HEAD"), "the package commit must be pushed to the remote branch"
    assert {
        name: run_remote("show", f"{remote_head}:packaging/aur/{name}", preserve_newline=True) for name in rendered
    } == rendered
