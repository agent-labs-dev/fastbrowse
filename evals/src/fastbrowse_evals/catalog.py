"""Export the exact private task metadata used by Fastbrowse's public release gate."""

import hashlib
import json
from pathlib import Path

from fastbrowse.evals.benchmarks import ExclusionReason
from pydantic import TypeAdapter, ValidationError

from fastbrowse_evals.live import ARMS, HEAD_TO_HEAD_ARMS, SUITES, eligible, excluded_because
from fastbrowse_evals.versions import fingerprint, load_lock

DERIVED_ARMS = (
    "fastbrowse-pool",
    "fastbrowse-hosted",
    "cua-codex",
    "browser-harness",
    "browser-use-ultrafast",
    "browser-use-oss",
)
"""Arms whose tasks follow from `eligible` rather than a task's own `arms`, so adding one changes no task fingerprint."""

RETIRED: tuple[dict[str, str], ...] = (
    {
        "id": "stretch-wizard-correction",
        "suite": "stretch-dev",
        "date": "2026-10-17",
        "reason": "site removed from DNS: lab.hakdogan.com answers NXDOMAIN (zone SOA serial 2026101610)",
    },
)
"""Tasks taken out of the catalog because they can no longer run, never because an agent did badly on them.
Fastbrowse's publication gate lets an approved group drop exactly these tasks and nothing else."""


REASON = TypeAdapter(ExclusionReason)


def reason(arm: str, task_id: str, text: str) -> str:
    """`text` as the site parses an exclusion reason, so a reason it would refuse stops the export, not the site."""
    try:
        return REASON.validate_python(text)
    except ValidationError:
        raise ValueError(f"{task_id}: {arm}'s exclusion reason cannot be published: {text!r}") from None


def render() -> str:
    """The catalog. It names no step cap: no arm has one (`fastbrowse_evals.live.AGENT_LIMITS`)."""
    lock = load_lock()
    for tasks in SUITES.values():
        for task in tasks:
            if lock[task.id]["fingerprint"] != fingerprint(task):
                raise ValueError(f"{task.id}: grading changed without a task version update")
    root = Path(__file__).parent
    sources = {
        name: hashlib.sha256((root / name).read_bytes()).hexdigest()
        for name in ("live_tasks.py", "more_tasks.py", "versions.json")
    }
    catalog = {
        "schema_version": 1,
        "family": "browser-use",
        "origin": "fastbrowse-internal",
        "owner": "parallax",
        "task_sources": sources,
        "suites": {
            name: [
                {
                    "id": t.id,
                    "category": t.category.value,
                    "arms": list(t.arms) + [arm for arm in DERIVED_ARMS if arm not in t.arms and eligible(arm, t)],
                    "excluded": {
                        arm: reason(arm, t.id, why)
                        for arm in HEAD_TO_HEAD_ARMS
                        if (why := excluded_because(arm, t)) is not None
                    },
                    "expect": t.expect.value,
                    "source_fingerprint": lock[t.id]["fingerprint"],
                    "task_version": lock[t.id]["version"],
                }
                for t in tasks
            ]
            for name, tasks in SUITES.items()
        },
        "arms": {name: {"pin": a.pin, "tier": a.tier, "default": a.default} for name, a in ARMS.items()},
        "retired": list(RETIRED),
    }
    return json.dumps(catalog, indent=2) + "\n"


def export(path: Path, *, check: bool = False) -> None:
    content = render()
    if check:
        if path.read_text() != content:
            raise ValueError("public benchmark metadata differs from the canonical fastbrowse_evals tasks")
    else:
        path.write_text(content)
