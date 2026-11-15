"""Run ty from pre-commit only when staged files can affect Python typing."""

from __future__ import annotations

import subprocess
import sys


def staged_paths() -> list[str]:
    result = subprocess.run(
        ["git", "diff", "--cached", "--name-only", "--diff-filter=ACDMRT"],
        check=True,
        capture_output=True,
        text=True,
    )
    return [path for path in result.stdout.splitlines() if path]


def needs_ty(paths: list[str]) -> bool:
    if not paths:
        return True
    return any(path.endswith(".py") or path in {"pyproject.toml", "uv.lock"} for path in paths)


def main() -> int:
    if not needs_ty(staged_paths()):
        return 0
    return subprocess.run(["uv", "run", "--no-sync", "ty", "check", *sys.argv[1:]], check=False).returncode


if __name__ == "__main__":
    raise SystemExit(main())
