"""A hook that runs `uv` must not sync the project, so a commit never rebuilds the native library.

pre-commit stashes unstaged changes before a hook runs, so a hook whose command syncs the environment rebuilds the
editable package - maturin writes `src/fastbrowse/_native.abi3.so` in place - from the stashed (clean) tree.
pre-commit then puts the unstaged files back and leaves the extension built from the old sources. The `.so` is
gitignored, so nothing reports it, and every later run on that checkout quietly executes old native code
([#766](https://github.com/agent-labs-dev/fastbrowse/issues/766)).

The `ty` hook's wrapper must retain `--no-sync` when it invokes the project's type checker.
"""

import os
import shlex
import subprocess
import sys
from pathlib import Path
from typing import Any

import pytest
import yaml

CONFIG = Path(".pre-commit-config.yaml")


@pytest.mark.parametrize("change", ["docs", "all-files", "delete", "rename", "stub", "config", "newline"])
def test_ty_hook_invocation_preserves_relevant_changes(tmp_path: Path, change: str) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    (ty,) = [hook for hook in hooks() if hook["id"] == "ty"]
    local = {
        **ty,
        "name": "ty",
        "entry": shlex.join([sys.executable, str(Path("scripts/pre_commit_ty.py").resolve())]),
        "language": "system",
        "types_or": ["python", "pyi", "jupyter"],
        "always_run": True,
        "pass_filenames": ty.get("pass_filenames", False),
    }
    (repo / ".pre-commit-config.yaml").write_text(yaml.safe_dump({"repos": [{"repo": "local", "hooks": [local]}]}))
    for name in ("module.py", "types.pyi", "README.md", "ty.toml", "module\nname.py"):
        (repo / name).write_text("")

    def git(*args: str) -> None:
        subprocess.run(["git", *args], cwd=repo, check=True, capture_output=True)

    git("init", "-q")
    git("add", ".")
    git(
        "-c",
        "user.name=Hook Test",
        "-c",
        "user.email=hook@example.com",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-qm",
        "base",
    )
    if change in {"docs", "all-files"}:
        (repo / "README.md").write_text("Documentation changed.\n")
    elif change == "delete":
        (repo / "module.py").unlink()
    elif change == "rename":
        (repo / "module.py").rename(repo / "module.txt")
    else:
        name = {"stub": "types.pyi", "config": "ty.toml", "newline": "module\nname.py"}[change]
        (repo / name).write_text("# changed\n")
    git("add", "-A")
    executable = tmp_path / "bin"
    executable.mkdir()
    uv = executable / "uv"
    uv.write_text('#!/bin/sh\nprintf "%s\\n" "$@" > "$HOOK_UV_LOG"\n')
    uv.chmod(0o755)
    receipt = tmp_path / "uv-args"
    env = {**os.environ, "PATH": f"{executable}{os.pathsep}{os.environ['PATH']}", "HOOK_UV_LOG": str(receipt)}
    result = subprocess.run(
        [sys.executable, "-m", "pre_commit", "run", "ty", *(["--all-files"] if change == "all-files" else [])],
        cwd=repo,
        env=env,
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    if change == "docs":
        assert not receipt.exists()
    else:
        assert receipt.read_text().splitlines() == ["run", "--no-sync", "ty", "check"]


# `uv` commands whose default is to sync the environment before they run; `--no-sync` is what stops them.
SYNCING = ("uv check", "uv run", "uv sync")
NO_SYNC = ("--no-sync", "--frozen")


def hooks() -> list[dict[str, Any]]:
    config = yaml.safe_load(CONFIG.read_text())
    return [hook for repo in config["repos"] for hook in repo["hooks"]]


def command(hook: dict[str, Any]) -> list[str]:
    """The entry this config sets, if it sets one, and the args it appends to the hook's own entry."""
    return [*(str(hook.get("entry", "")).split()), *(str(arg) for arg in hook.get("args") or ())]


def test_a_hook_with_its_own_uv_entry_does_not_sync() -> None:
    offending = [
        f"{hook['id']}: {' '.join(command(hook))}"
        for hook in hooks()
        if str(hook.get("entry", "")).startswith(SYNCING) and not set(NO_SYNC) & set(command(hook))
    ]
    assert not offending, (
        "these hooks sync the environment, which rebuilds the native library from the stashed tree: "
        + "; ".join(offending)
    )


def test_the_ty_hook_checks_without_syncing() -> None:
    """The wrapper must keep `uv run` from rebuilding the native extension."""
    assert "--no-sync" in Path("scripts/pre_commit_ty.py").read_text()
