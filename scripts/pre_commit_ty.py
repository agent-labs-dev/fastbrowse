"""Run ty from pre-commit only when staged files can affect Python typing."""

from __future__ import annotations

import subprocess
import sys


def staged_paths() -> list[str]:
    result = subprocess.run(
        ["git", "diff", "--cached", "--name-only", "--no-renames", "-z"],
        check=True,
        capture_output=True,
        text=True,
    )
    return [path for path in result.stdout.split("\0") if path]


def needs_ty(paths: list[str], staged: list[str]) -> bool:
    paths = [*paths, *staged]
    return any(
        path.endswith((".py", ".pyi", ".ipynb"))
        or path.rsplit("/", 1)[-1] in {"pyproject.toml", "uv.lock", "ty.toml", ".ty.toml", ".pre-commit-config.yaml"}
        for path in paths
    )


def main() -> int:
    if not needs_ty(sys.argv[1:], staged_paths()):
        return 0
    return subprocess.run(["uv", "run", "--no-sync", "ty", "check"], check=False).returncode


if __name__ == "__main__":
    raise SystemExit(main())
