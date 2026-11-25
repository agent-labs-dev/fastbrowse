"""Versions for the Parallax task source records."""

import json
from collections.abc import Iterable, Mapping
from pathlib import Path
from typing import Any

from parallax.models import TaskRecord
from parallax.task import record_hash

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


def fingerprint(record: TaskRecord) -> str:
    """A task's lock fingerprint: the head of Parallax's own `record_hash`, the identity the publication gate
    checks. It covers the record's version, so bump a version, then relock."""
    return record_hash(record)[:16]


def relocked(records: Iterable[TaskRecord]) -> dict[str, dict[str, Any]]:
    """The lock with each record's fingerprint recomputed; versions are the lock's and are never moved here."""
    return {
        record.id: {"version": task_version(record.id), "fingerprint": fingerprint(record)}
        for record in sorted(records, key=lambda r: r.id)
    }


def relock() -> None:
    from fastbrowse_evals.task_specs import records

    LOCK.write_text(json.dumps(relocked(records()), indent=2) + "\n")
