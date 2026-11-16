"""Versions for the Parallax task source records."""

import json
from collections.abc import Mapping
from pathlib import Path
from typing import Any

LOCK = Path(__file__).with_name("versions.json")


def load_lock() -> dict[str, dict[str, Any]]:
    return json.loads(LOCK.read_text())


def task_version(task_id: str, lock: Mapping[str, Mapping[str, Any]] | None = None) -> int:
    entries = load_lock() if lock is None else lock
    entry = entries.get(task_id)
    if entry is None:
        raise KeyError(f"task {task_id!r} has no pinned version")
    version = entry.get("version")
    if not isinstance(version, int) or version < 1:
        raise ValueError(f"task {task_id!r} has invalid version {version!r}")
    return version
