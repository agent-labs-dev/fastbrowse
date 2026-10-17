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


class RetiredTask(Closed):
    """A task taken out because it can no longer run (its site is gone), with when and why."""

    id: str
    suite: str
    date: Annotated[str, Field(pattern=r"^\d{4}-\d{2}-\d{2}$")]
    reason: Annotated[str, Field(min_length=1)]


class Catalog(Closed):
    schema_version: Literal[1]
    family: Literal["browser-use"]
    origin: Literal["fastbrowse-internal"]
    owner: Literal["parallax"]
    task_sources: dict[str, Digest]
    suites: dict[str, tuple[CatalogTask, ...]]
    arms: dict[str, ArmMetadata]
    ultrafast_text_model: str
    retired: tuple[RetiredTask, ...] = ()

    @model_validator(mode="after")
    def matches_versions(self) -> "Catalog":
        lock = json.loads(Path(__file__).with_name("versions.json").read_text())
        tasks = [t for entries in self.suites.values() for t in entries]
        if len({t.id for t in tasks}) != len(tasks):
            raise ValueError("duplicate benchmark tasks")
        retired = {r.id for r in self.retired}
        if len(retired) != len(self.retired) or retired & {t.id for t in tasks}:
            raise ValueError("a retired task is listed twice or still in a suite")
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
    if arm == "fastbrowse-pool":
        return "fastbrowse" in task.arms
    return arm in task.arms or (arm == "browser-use-oss" and task.expect == Status.COMPLETE)
