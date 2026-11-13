"""Export the task metadata consumed by Fastbrowse's public benchmark gate."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import NamedTuple

from fastbrowse.evals.benchmarks import ExclusionReason
from fastbrowse.models import Status
from pydantic import TypeAdapter, ValidationError

from fastbrowse_evals.task_specs import TaskSpec, specs
from fastbrowse_evals.versions import load_lock


class ArmSpec(NamedTuple):
    pin: str
    tier: str
    default: bool = True


ARMS = {
    "fastbrowse": ArmSpec("run.fastbrowse_version + run.git_sha", "A"),
    "fastbrowse-pool": ArmSpec("fastbrowse plugin + Parallax browser pool", "A", False),
    "fastbrowse-hosted": ArmSpec("api.fastbrowse.ai at its deployed build", "hosted", False),
    "browser-use": ArmSpec("browser-use-sdk==3.12.1 API v4, default model", "hosted"),
    "browser-use-ultrafast": ArmSpec("browser-use-sdk==3.12.1 API v4, model bu-ultrafast", "hosted"),
    "browser-use-oss": ArmSpec("browser-use==1.0.3, default model", "A", False),
    "cua-codex": ArmSpec("cua-codex pin from Parallax", "local", False),
    "browser-harness": ArmSpec("browser-harness pin from Parallax, Codex CLI", "A", False),
}
HEAD_TO_HEAD_ARMS = ("fastbrowse", "browser-use", "browser-use-ultrafast", "browser-harness", "browser-use-oss")
DERIVED_ARMS = (
    "fastbrowse-pool",
    "fastbrowse-hosted",
    "cua-codex",
    "browser-harness",
    "browser-use-ultrafast",
    "browser-use-oss",
)

RETIRED = (
    {
        "id": "stretch-wizard-correction",
        "suite": "stretch-dev",
        "date": "2026-10-17",
        "reason": "site removed from DNS: lab.hakdogan.com answers NXDOMAIN (zone SOA serial 2026101610)",
    },
)
REASON = TypeAdapter(ExclusionReason)
SUITES: dict[str, tuple[TaskSpec, ...]] = {
    name: tuple(spec for spec in specs() if spec.suite == name) for name in ("core", "dev", "stretch-dev")
}
DIAGNOSTIC_SUITES = {"sweep": tuple(spec for spec in specs() if spec.suite == "sweep")}


def reason(arm: str, task_id: str, value: str) -> str:
    try:
        return REASON.validate_python(value)
    except ValidationError:
        raise ValueError(f"{task_id}: {arm}'s exclusion reason cannot be published: {value!r}") from None


def eligible(arm: str, task: TaskSpec) -> bool:
    if arm == "fastbrowse-pool":
        return "fastbrowse" in task.arms
    if arm == "fastbrowse-hosted":
        return (
            "fastbrowse" in task.arms
            and not task.secret_names
            and task.bitwarden_item is None
            and task.category != "navigate"
        )
    if arm == "browser-use-ultrafast":
        return eligible("browser-use", task)
    if arm in {"cua-codex", "browser-harness", "browser-use-oss"}:
        return task.expected_status == Status.COMPLETE.value and not task.secret_names and task.bitwarden_item is None
    return arm in task.arms


def excluded_because(arm: str, task: TaskSpec) -> str | None:
    if eligible(arm, task):
        return None
    if arm in {"browser-use", "browser-use-ultrafast"} and task.expected_status != Status.COMPLETE.value:
        return (
            "the task passes only by stopping before an irreversible step it does not authorize, and the hosted "
            "Browser Use API takes no authorization setting and no pause, so it cannot be told the step is unauthorized"
        )
    if arm in {"browser-harness", "browser-use-oss"} and task.expected_status != Status.COMPLETE.value:
        return (
            "the task passes only by stopping before an irreversible step it does not authorize, and it takes no "
            "authorization setting and has no way to pause"
        )
    if arm == "browser-harness":
        return "the task needs login credentials, and we give it none, because it would hold them in its own shell"
    if arm == "browser-use-oss":
        return "the task needs login credentials, and we give it none, because the code its model writes can read any it is given"
    raise ValueError(f"{task.id}: {arm} is excluded without a stated reason")


def render() -> str:
    lock = load_lock()
    root = Path(__file__).parent
    source_names = ("live_tasks.py", "more_tasks.py", "sweep_tasks.py", "task_specs.py", "versions.json")
    catalog = {
        "schema_version": 1,
        "family": "browser-use",
        "origin": "fastbrowse-internal",
        "owner": "parallax",
        "task_sources": {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in source_names},
        "suites": {
            name: [
                {
                    "id": task.id,
                    "category": task.category,
                    "arms": list(task.arms)
                    + [arm for arm in DERIVED_ARMS if arm not in task.arms and eligible(arm, task)],
                    "excluded": {
                        arm: reason(arm, task.id, why)
                        for arm in HEAD_TO_HEAD_ARMS
                        if (why := excluded_because(arm, task)) is not None
                    },
                    "expect": task.expected_status,
                    "source_fingerprint": lock[task.id]["fingerprint"],
                    "task_version": lock[task.id]["version"],
                }
                for task in tasks
            ]
            for name, tasks in SUITES.items()
        },
        "arms": {name: value._asdict() for name, value in ARMS.items()},
        "retired": list(RETIRED),
    }
    return json.dumps(catalog, indent=2) + "\n"


PATH = Path(__file__).parents[3] / "src" / "fastbrowse" / "evals" / "benchmark-catalog.json"


def export(path: Path, *, check: bool = False) -> None:
    content = render()
    if check:
        if path.read_text() != content:
            raise ValueError("public benchmark metadata differs from the canonical Parallax task source")
    else:
        path.write_text(content)


CATALOG = None
CATALOG_SHA256 = ""
SUITES_PUBLIC = SUITES


def _load_catalog() -> None:
    global CATALOG, CATALOG_SHA256
    from fastbrowse.evals.catalog import Catalog

    raw = PATH.read_bytes()
    CATALOG = Catalog.model_validate_json(raw)
    CATALOG_SHA256 = hashlib.sha256(raw).hexdigest()


_load_catalog()
