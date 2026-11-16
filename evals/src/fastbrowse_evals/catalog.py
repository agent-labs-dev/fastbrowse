"""Export the task metadata consumed by Fastbrowse's public benchmark gate."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import NamedTuple

from fastbrowse_evals.task_specs import TaskSpec, specs
from fastbrowse_evals.versions import load_lock


class ArmSpec(NamedTuple):
    pin: str
    tier: str
    default: bool = True


ARMS = {
    "fastbrowse": ArmSpec("run.fastbrowse_version + run.git_sha", "A"),
    "fastbrowse-pool": ArmSpec("run.fastbrowse_version + run.git_sha; fastbrowse-hosted browser pool", "A", False),
    "fastbrowse-hosted": ArmSpec(
        "api.fastbrowse.ai at its deployed build; customer charge, not metered cost", "hosted", False
    ),
    "browser-use": ArmSpec("browser-use-sdk==3.12.1 API v4; default model, reported per row", "hosted"),
    "browser-use-ultrafast": ArmSpec("browser-use-sdk==3.12.1 API v4; model bu-ultrafast, reported per row", "hosted"),
    "browser-use-oss": ArmSpec(
        "browser-use==1.0.3; default model bu-fast, default mode (ultrafast on bu-fast)", "A", False
    ),
    "cua-codex": ArmSpec("cua-driver==0.47.19; codex-cli==0.169.0", "local", False),
    "browser-harness": ArmSpec("browser-harness==0.1.13; codex-cli==0.169.0; cli+skill", "A", False),
}
RETIRED = (
    {
        "id": "stretch-wizard-correction",
        "suite": "stretch-dev",
        "date": "2026-10-17",
        "reason": "site removed from DNS: lab.hakdogan.com answers NXDOMAIN (zone SOA serial 2026101610)",
    },
)
SUITES: dict[str, tuple[TaskSpec, ...]] = {
    name: tuple(spec for spec in specs() if spec.suite == name) for name in ("core", "dev", "stretch-dev")
}


def render() -> str:
    lock = load_lock()
    root = Path(__file__).parent
    source_names = ("live_tasks.py", "more_tasks.py", "sweep_tasks.py", "task_specs.py", "versions.json")
    catalog = json.loads(PATH.read_text()) if PATH.is_file() else {}
    expected = {task.id: task for tasks in SUITES.values() for task in tasks}
    entries = {task["id"]: task for suite in catalog.get("suites", {}).values() for task in suite}
    if set(entries) != set(expected):
        raise ValueError("the public catalog task set differs from the Parallax task source")
    for task_id, spec in expected.items():
        entry = entries[task_id]
        entry["category"] = spec.category
        entry["expect"] = spec.expected_status
        entry["source_fingerprint"] = lock[task_id]["fingerprint"]
        entry["task_version"] = lock[task_id]["version"]
    catalog["task_sources"] = {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in source_names}
    catalog["arms"] = {name: value._asdict() for name, value in ARMS.items()}
    catalog["retired"] = list(RETIRED)
    return json.dumps(catalog, indent=2) + "\n"


PATH = Path(__file__).parents[3] / "src" / "fastbrowse" / "evals" / "benchmark-catalog.json"


def export(path: Path, *, check: bool = False) -> None:
    content = render()
    if check:
        if path.read_text() != content:
            raise ValueError("public benchmark metadata differs from the canonical Parallax task source")
    else:
        path.write_text(content)
