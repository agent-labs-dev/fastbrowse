"""Imported benchmark metadata for public release checks, without private runner code."""

import hashlib
import json
from pathlib import Path
from typing import Annotated, Literal

from pydantic import BaseModel, ConfigDict, Field, model_validator

from fastbrowse.evals.live_tasks import Category
from fastbrowse.models import Status

Digest = Annotated[str, Field(pattern=r"^[a-f0-9]{64}$")]


class Closed(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True)


class CatalogTask(Closed):
    id: str
    category: Category
    arms: tuple[str, ...]
    expect: Status
    source_fingerprint: Annotated[str, Field(pattern=r"^[a-f0-9]{16}$")]
    task_version: Annotated[int, Field(ge=1)]


class ArmMetadata(Closed):
    pin: str
    tier: str
    default: bool


class Catalog(Closed):
    schema_version: Literal[1]
    family: Literal["browser-use"]
    origin: Literal["fastbrowse-internal"]
    owner: Literal["parallax"]
    task_sources: dict[str, Digest]
    suites: dict[str, tuple[CatalogTask, ...]]
    arms: dict[str, ArmMetadata]
    max_steps: Annotated[int, Field(ge=1)]
    ultrafast_text_model: str

    @model_validator(mode="after")
    def matches_versions(self) -> "Catalog":
        lock = json.loads(Path(__file__).with_name("versions.json").read_text())
        tasks = [t for entries in self.suites.values() for t in entries]
        if len({t.id for t in tasks}) != len(tasks):
            raise ValueError("duplicate benchmark tasks")
        for task in tasks:
            if lock.get(task.id) != {"version": task.task_version, "fingerprint": task.source_fingerprint}:
                raise ValueError("benchmark catalog differs from the imported task-version lock")
            if any(arm not in self.arms for arm in task.arms):
                raise ValueError("unknown benchmark arm")
        return self


PATH = Path(__file__).with_name("benchmark-catalog.json")
CATALOG = Catalog.model_validate_json(PATH.read_bytes())
CATALOG_SHA256 = hashlib.sha256(PATH.read_bytes()).hexdigest()
SUITES = CATALOG.suites


def eligible(arm: str, task: CatalogTask) -> bool:
    return arm in task.arms or (arm == "browser-use-oss" and task.expect == Status.COMPLETE)
