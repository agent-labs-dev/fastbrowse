"""Diagnostic sweep tasks exposed by the same Parallax task source."""

from fastbrowse_evals.task_specs import TaskSpec, specs

SWEEP: tuple[TaskSpec, ...] = tuple(spec for spec in specs() if spec.suite == "sweep")

__all__ = ["SWEEP"]
