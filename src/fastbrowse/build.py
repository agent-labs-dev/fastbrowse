"""Which build is running: the package version, its commit, and whether the tree had uncommitted changes.

The CLI puts this in its `--json` result and its trace, and the eval harness in every row, so a result can be traced
to the commit that produced it.
"""

import json
import re
import subprocess
from importlib.metadata import PackageNotFoundError, distribution, version
from pathlib import Path

ROOT = Path(__file__).parents[2]


def _git(*args: str) -> str | None:
    # An installed package can sit inside another project's checkout, so parent discovery is not build identity.
    if not (ROOT / "src" / "fastbrowse" / "build.py").is_file():
        return None
    try:
        done = subprocess.run(
            ["git", *args], cwd=Path(__file__).parent, capture_output=True, text=True, timeout=10, check=True
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return done.stdout.strip()


def identity() -> dict[str, object]:
    """`fastbrowse_version`, `git_sha` (a full commit, or None) and `git_dirty`.

    `git_dirty` is None outside a checkout (an installed wheel) and True when tracked files had uncommitted
    changes or untracked, non-ignored files were present, so a score from an unreviewed tree is marked as one.
    Ignored paths (an `artifacts/` directory) do not dirty a build; an untracked executable does. A package
    installed from a git URL has no checkout, and its install receipt names the commit.
    """
    try:
        package = version("fastbrowse")
    except PackageNotFoundError:
        package = None
    sha = _git("rev-parse", "HEAD")
    status = None if sha is None else _git("status", "--porcelain")
    dirty = None if status is None else bool(status)
    if sha is None:
        try:
            receipt = json.loads(distribution("fastbrowse").read_text("direct_url.json") or "{}")
        except (PackageNotFoundError, ValueError):
            receipt = {}
        vcs = receipt.get("vcs_info") if isinstance(receipt, dict) else None
        commit = vcs.get("commit_id") if isinstance(vcs, dict) else None
        if isinstance(commit, str) and re.fullmatch(r"[0-9a-f]{40}", commit):
            sha, dirty = commit, False
    return {"fastbrowse_version": package, "git_sha": sha, "git_dirty": dirty}
