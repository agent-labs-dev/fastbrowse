"""Export measured outcomes without answers, page content, credentials or local paths.

The public browser and private tracking adapter read this same projection. Original rows and
ledgers remain the authority; this export never grades a run or authorizes a comparison figure.
"""

import argparse
import hashlib
import json
from collections import Counter
from collections.abc import Mapping
from datetime import datetime
from pathlib import Path
from typing import Annotated, Any, Literal

from pydantic import (
    BaseModel,
    ConfigDict,
    Field,
    SerializerFunctionWrapHandler,
    StringConstraints,
    TypeAdapter,
    model_serializer,
    model_validator,
)

from fastbrowse.evals.api_cost import ApiCostEstimate

Identifier = Annotated[str, StringConstraints(pattern=r"^[a-zA-Z0-9_.:+-]{1,160}$")]
Digest = Annotated[str, StringConstraints(pattern=r"^[0-9a-f]{64}$")]
Commit = Annotated[str, StringConstraints(pattern=r"^[0-9a-f]{40}$")]
Number = Annotated[float, Field(ge=0, allow_inf_nan=False)]


class EvidenceModel(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True, validate_assignment=True)


class Source(EvidenceModel):
    path: Annotated[
        str, StringConstraints(pattern=r"^docs/(results|validation)/[a-zA-Z0-9_][a-zA-Z0-9_.-]*\.(jsonl|json)$")
    ]
    sha256: Digest


class Attempt(EvidenceModel):
    id: Digest
    task: Identifier
    task_version: Identifier | None
    suite: Identifier
    arm: Identifier
    repeat: Annotated[int, Field(ge=0)]
    retry: Annotated[int, Field(ge=0)]
    selected: bool
    started: bool
    status: Identifier
    passed: bool | None
    completed: bool
    seconds: Number | None
    dollars: Number | None
    unknown_cost: bool
    estimated_api_cost: ApiCostEstimate | None = None
    git_sha: Commit | None
    git_dirty: bool | None
    model: Annotated[str, StringConstraints(pattern=r"^[a-zA-Z0-9_./,:+() -]{1,160}$")] | None
    started_at: (
        Annotated[str, StringConstraints(pattern=r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})$")]
        | None
    )

    @model_serializer(mode="wrap")
    def serialize_estimate(self, handler: SerializerFunctionWrapHandler) -> dict[str, Any]:
        data = handler(self)
        if self.estimated_api_cost is None:
            data.pop("estimated_api_cost", None)
        return data

    @model_validator(mode="after")
    def cost_is_known(self) -> "Attempt":
        if self.started_at is not None:
            datetime.fromisoformat(self.started_at)
        if self.estimated_api_cost is not None and (not self.started or self.model != self.estimated_api_cost.model):
            raise ValueError("estimated usage must name the model of a started run")
        if self.unknown_cost != (self.dollars is None):
            raise ValueError("unknown cost must be null, known cost must be recorded")
        return self


class Campaign(EvidenceModel):
    id: Identifier
    kind: Literal["published", "diagnostic"]
    sources: list[Source]
    scheduled_slots: Annotated[int, Field(ge=0)] | None = None
    attempts: list[Attempt]


class Evidence(EvidenceModel):
    schema_version: Literal[1] = 1
    campaigns: list[Campaign]

    @model_validator(mode="after")
    def unique_ids(self) -> "Evidence":
        if len({c.id for c in self.campaigns}) != len(self.campaigns):
            raise ValueError("duplicate campaign ids")
        for campaign in self.campaigns:
            if len({a.id for a in campaign.attempts}) != len(campaign.attempts):
                raise ValueError("duplicate attempt ids")
        return self


def _digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _identity(row: Mapping[str, Any]) -> tuple[object, ...]:
    return row.get("suite"), row.get("task"), row.get("arm"), row.get("repeat", 0)


def _project(row: Mapping[str, Any], source: Source, index: int, selected: bool) -> Attempt:
    payload = row.get("attempt")
    corpus = isinstance(payload, dict)
    data = payload if corpus else row
    run = data.get("run") or {}
    raw_status = data.get("raw_status") if corpus else data.get("status")
    status = raw_status or ("reset_failed" if row.get("reset", {}).get("ok") is False else "not_started")
    unknown = data.get("unknown_cost", data.get("dollars") is None)
    task_version = data.get("task_digest") if corpus else data.get("task_version")
    grade = data.get("grade") or {}
    if corpus and task_version:
        task_version = str(task_version)
    completed = (
        data.get("completed")
        if corpus
        else (status == "complete" if data.get("arm") == "fastbrowse" else data.get("normalized_status") == "done")
    )
    passed = grade.get("passed") if corpus else data.get("passed")
    if corpus and not data.get("graded"):
        passed = None
    return Attempt(
        id=_digest(f"{source.path}:{source.sha256}:{index}".encode()),
        task=data.get("task") or row.get("task") or "unknown",
        task_version=str(task_version) if task_version is not None else None,
        suite=data.get("suite") or (data.get("source") if corpus else None) or "unknown",
        arm=data.get("arm") or row.get("arm") or "unknown",
        repeat=data.get("repeat", row.get("repeat", 0)),
        retry=row.get("retry", data.get("retries") or 0),
        selected=selected,
        started=data.get("started", raw_status is not None and not data.get("budget_refused", False)),
        status=status,
        passed=passed,
        completed=completed is True,
        seconds=data.get("seconds"),
        dollars=None if unknown else data.get("dollars"),
        unknown_cost=bool(unknown or data.get("dollars") is None),
        estimated_api_cost=data.get("estimated_api_cost"),
        git_sha=run.get("git_sha") or data.get("git_sha"),
        git_dirty=run.get("git_dirty", data.get("git_dirty")),
        model=data.get("model") or row.get("model"),
        started_at=run.get("run_started"),
    )


def campaign(path: Path, root: Path, *, kind: Literal["published", "diagnostic"] = "diagnostic") -> Campaign:
    content = path.read_bytes()
    source = Source(path=path.relative_to(root).as_posix(), sha256=_digest(content))
    rows = [json.loads(line) for line in content.splitlines() if line.strip()]
    if not rows or not all(isinstance(row, dict) for row in rows):
        raise ValueError(f"{source.path}: expected result objects")
    ledger = path if path.name.endswith("attempts.jsonl") else path.with_suffix(".attempts.jsonl")
    sources = [source]
    selected_rows = rows
    if ledger != path and ledger.exists():
        content = ledger.read_bytes()
        source = Source(path=ledger.relative_to(root).as_posix(), sha256=_digest(content))
        sources.append(source)
        rows = [json.loads(line) for line in content.splitlines() if line.strip()]
    selected_ids = {(r.get("run") or {}).get("run_id") for r in selected_rows} - {None}
    chosen: set[int] = set()
    if rows and "attempt" in rows[0]:
        # The corpus runner chooses the final executed retry, not the most flattering grade.
        last: dict[tuple[object, ...], int] = {}
        for i, row in enumerate(rows):
            data = row.get("attempt")
            if isinstance(data, dict):
                last[_identity(data)] = i
        chosen = set(last.values())
    elif ledger == path or (ledger != path and len(sources) > 1):
        chosen = {
            i
            for i, row in enumerate(rows)
            if row.get("selected") is True
            or ("selected" not in row and (row.get("run") or {}).get("run_id") in selected_ids)
        }
    else:
        chosen = set(range(len(rows)))
    projected = []
    legacy: Counter[tuple[object, ...]] = Counter()
    for i, row in enumerate(rows):
        data = row
        if "attempt" not in row and "repeat" not in row:
            # Older ledgers record trials in order but omit repeat ids, so each gets its own ordinal.
            key = _identity(row)
            data = {**row, "repeat": legacy[key]}
            legacy[key] += 1
        projected.append(_project(data, source, i, i in chosen))
    return Campaign(
        id=path.stem.removesuffix(".attempts"),
        kind=kind,
        sources=sources,
        attempts=projected,
    )


def export(root: Path) -> Evidence:
    campaigns = []
    paths = sorted((root / "docs/results").glob("*.jsonl")) + sorted((root / "docs/validation").glob("*.jsonl"))
    for path in paths:
        if (
            path.name.endswith(".attempts.jsonl")
            and path.with_name(path.name.replace(".attempts.jsonl", ".jsonl")).exists()
        ):
            continue
        first = next((line for line in path.read_text().splitlines() if line.strip()), None)
        if first is None:
            continue
        row = json.loads(first)
        if "task" not in row and "attempt" not in row:
            continue
        campaigns.append(campaign(path, root, kind="published" if path.parent.name == "results" else "diagnostic"))
    archive = root / "docs/results/evidence-archive.json"
    if archive.exists():
        content = archive.read_bytes()
        source = Source(path=archive.relative_to(root).as_posix(), sha256=_digest(content))
        archived = Evidence.model_validate_json(content)
        for entry in archived.campaigns:
            entry.sources.append(source)
        campaigns.extend(archived.campaigns)
    schedules = root / "docs/results/evidence-schedules.json"
    if schedules.exists():
        content = schedules.read_bytes()
        source = Source(path=schedules.relative_to(root).as_posix(), sha256=_digest(content))
        configured = TypeAdapter(dict[Identifier, Annotated[int, Field(ge=0, strict=True)]]).validate_json(content)
        missing = configured.keys() - {entry.id for entry in campaigns}
        if missing:
            raise ValueError(f"schedules name unknown campaigns: {sorted(missing)}")
        for entry in campaigns:
            if entry.id in configured:
                entry.sources.append(source)
                entry.scheduled_slots = configured[entry.id]
    return Evidence(campaigns=campaigns)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).parents[3])
    parser.add_argument("--out", type=Path)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    output = args.out or args.root / "artifacts/evals/evidence.json"
    rendered = json.dumps(export(args.root).model_dump(mode="json"), indent=2, sort_keys=True) + "\n"
    if args.check:
        if output.read_text() != rendered:
            raise SystemExit("eval evidence is stale; run python -m fastbrowse.evals.evidence")
    else:
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(rendered)


if __name__ == "__main__":
    main()
