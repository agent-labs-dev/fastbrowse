"""A hook that runs `uv` must not sync the project, so a commit never rebuilds the native library.

pre-commit stashes unstaged changes before a hook runs, so a hook whose command syncs the environment rebuilds the
editable package - maturin writes `src/fastbrowse/_native.abi3.so` in place - from the stashed (clean) tree.
pre-commit then puts the unstaged files back and leaves the extension built from the old sources. The `.so` is
gitignored, so nothing reports it, and every later run on that checkout quietly executes old native code
([#766](https://github.com/agent-labs-dev/fastbrowse/issues/766)).

The `ty` hook's entry comes from `astral-sh/ty-pre-commit` and is `uv check ...`, which syncs first, so the project
has to pass `--no-sync` to it itself. Any other hook this file names is checked the same way.
"""

from pathlib import Path
from typing import Any

import yaml

CONFIG = Path(".pre-commit-config.yaml")


def test_ty_wrapper_runs_for_python_deletions_and_manual_all_files() -> None:
    from scripts.pre_commit_ty import needs_ty

    assert needs_ty(["src/fastbrowse/deleted.py"])
    assert needs_ty([])
    assert not needs_ty([".github/workflows/ci.yml", "README.md"])


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
