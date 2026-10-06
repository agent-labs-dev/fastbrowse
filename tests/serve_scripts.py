"""Stand-ins for `run_task` that a `fastbrowse serve` subprocess can be pointed at by name.

A test in the same process hands the server a recorder. One that starts the command cannot, and names one of
these in the hidden `--run-task` option instead, as `tests.serve_scripts:<name>`.
"""

from typing import Any

from fastbrowse.models import BrowserEvent, CostBreakdown, EventHandler, RunResult, Status


async def echo(task: str, *, on_event: EventHandler, **_: Any) -> RunResult:
    """A run that opens nothing and answers with its own task, after the one event every run sends."""
    await on_event(BrowserEvent(live_url=None))
    return RunResult(
        status=Status.COMPLETE,
        answer=f"echo: {task}",
        data=None,
        evidence=(),
        steps=(),
        cost=CostBreakdown(lines=()),
        artifacts=(),
    )
