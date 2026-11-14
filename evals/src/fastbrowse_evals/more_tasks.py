"""The development and stretch portions of the Parallax browser task source."""

from fastbrowse_evals.task_specs import TaskSpec, specs

DEV: tuple[TaskSpec, ...] = tuple(spec for spec in specs() if spec.suite == "dev")
STRETCH_DEV: tuple[TaskSpec, ...] = tuple(spec for spec in specs() if spec.suite == "stretch-dev")

__all__ = ["DEV", "STRETCH_DEV"]
