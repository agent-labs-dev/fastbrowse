"""Compact published scores needed by the offline regression gate, without execution logs."""

import json
from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict

from fastbrowse.evals.evidence import Source

NAME = "publication-baselines.json"
FIELDS = {
    "arm",
    "task",
    "task_version",
    "suite",
    "suite_version",
    "repeat",
    "passed",
    "correct",
    "normalized_status",
    "seconds",
    "dollars",
    "model",
    "text_model",
    "step_cap",
    "step_cap_unit",
    "decision_cap",
    "at",
    "replaces_run_id",
    "run",
}
RUN_FIELDS = {"fastbrowse_version", "run_started", "run_id", "providers", "max_steps", "git_sha", "git_dirty"}


class Baseline(BaseModel):
    model_config = ConfigDict(extra="forbid")
    source: Source
    rows: list[dict[str, Any]]


def scores(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    projected = [{key: value for key, value in row.items() if key in FIELDS} for row in rows]
    for row in projected:
        if isinstance(row.get("run"), dict):
            row["run"] = {key: value for key, value in row["run"].items() if key in RUN_FIELDS}
    return projected


def load(text: str) -> list[Baseline]:
    data = json.loads(text)
    if not isinstance(data, list):
        raise ValueError("published baselines must be a list")
    entries = [Baseline.model_validate(entry) for entry in data]
    if len({entry.source.path for entry in entries}) != len(entries):
        raise ValueError("duplicate baseline sources")
    for entry in entries:
        if scores(entry.rows) != entry.rows:
            raise ValueError("execution logs belong in Langfuse, not publication baselines")
    return entries


def read(directory: Path) -> list[Baseline]:
    path = directory / NAME
    return load(path.read_text()) if path.exists() else []


def release(entry: Baseline) -> str:
    return Path(entry.source.path).stem


def published(directory: Path) -> list[tuple[str, list[dict[str, Any]]]]:
    releases = {release(entry): entry.rows for entry in read(directory)}
    for path in sorted(directory.glob("*.jsonl")):
        if path.name.endswith(".attempts.jsonl"):
            continue
        if path.stem in releases:
            raise ValueError("a published release cannot have both a compact baseline and raw rows")
        releases[path.stem] = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    return list(releases.items())
