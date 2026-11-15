"""Record the measured Fastbrowse build and the eval plugin checkout in Parallax runs."""

from __future__ import annotations

import json
import subprocess
from importlib.metadata import PackageNotFoundError, distribution
from pathlib import Path


def _installed_fastbrowse_sha() -> str | None:
    try:
        raw = distribution("fastbrowse").read_text("direct_url.json")
    except PackageNotFoundError:
        return None
    if raw is None:
        return None
    value = json.loads(raw).get("vcs_info", {}).get("commit_id")
    return value if isinstance(value, str) and len(value) == 40 else None


def _runner_sha() -> str | None:
    root = Path(__file__).resolve()
    for parent in root.parents:
        if (parent / ".git").exists():
            result = subprocess.run(
                ["git", "-C", str(parent), "rev-parse", "HEAD"], capture_output=True, text=True, check=False
            )
            value = result.stdout.strip()
            return value if len(value) == 40 else None
    return None


def fields(_: Path) -> dict[str, str]:
    result = {}
    if agent_sha := _installed_fastbrowse_sha():
        result["agent_sha"] = agent_sha
    if runner_sha := _runner_sha():
        result["runner_sha"] = runner_sha
    return result
