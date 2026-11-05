"""Version privately owned tasks using the public result contract."""

import hashlib
import json
import subprocess
from collections.abc import Iterable, Mapping
from pathlib import Path
from typing import Any

from fastbrowse.evals import versions as contract
from fastbrowse.evals.versions import fingerprint as fingerprint
from fastbrowse.evals.versions import provenance as core_provenance

LOCK = Path(__file__).with_name("versions.json")


def load_lock() -> dict[str, dict[str, Any]]:
    return json.loads(LOCK.read_text())


def task_version(task_id: str, lock: Mapping[str, Mapping[str, Any]] | None = None) -> int | None:
    return contract.task_version(task_id, load_lock() if lock is None else lock)


def suite_version(task_ids: Iterable[str], lock: Mapping[str, Mapping[str, Any]] | None = None) -> str:
    return contract.suite_version(task_ids, load_lock() if lock is None else lock)


def installed_catalog() -> bytes:
    """The benchmark catalog the pinned Fastbrowse ships, which every run must have been made against."""
    from fastbrowse.evals import catalog

    return Path(catalog.__file__).with_name("benchmark-catalog.json").read_bytes()


def provenance(*, benchmark_origin: str = "fastbrowse-internal", **extra: object) -> dict[str, object]:
    root = Path(__file__).parents[2]

    def git(*args: str) -> str:
        return subprocess.run(["git", *args], cwd=root, check=True, capture_output=True, text=True).stdout.strip()

    from fastbrowse_evals.catalog import render

    canonical = render().encode()
    if canonical != installed_catalog():
        raise ValueError("installed Fastbrowse benchmark metadata differs from the canonical fastbrowse_evals tasks")

    return core_provenance(
        **extra,
        benchmark_family="browser-use",
        benchmark_origin=benchmark_origin,
        benchmark_runner_sha=git("rev-parse", "HEAD"),
        benchmark_runner_dirty=bool(git("status", "--porcelain")),
        benchmark_catalog_sha256=hashlib.sha256(canonical).hexdigest(),
    )


def local_browser_identity(binary: Path) -> dict[str, Any]:
    binary = binary.resolve(strict=True)
    version = subprocess.run([str(binary), "--version"], check=True, capture_output=True, text=True, timeout=10)
    with binary.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    return {
        "mode": "local-chrome",
        "binary": str(binary),
        "version": version.stdout.strip(),
        "sha256": digest,
        "headed": False,
        "profile": False,
    }
