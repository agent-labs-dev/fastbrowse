"""The single Parallax task source for fastbrowse's browser benchmark."""

from parallax.models import TaskRecord

from fastbrowse_evals.task_specs import TaskSpec, records, specs

__fingerprint_namespace__ = "fastbrowse_evals.live_tasks"

TASKS: tuple[TaskSpec, ...] = specs()


def task_records() -> list[TaskRecord]:
    """Return catalog and diagnostic tasks for the `parallax.task_sources` entry point."""
    return records()


__all__ = ["TASKS", "TaskSpec", "task_records"]
