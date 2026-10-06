"""Stand-ins for `run_task` that a `fastbrowse serve` subprocess can be pointed at by name.

A test in the same process hands the server a recorder. One that starts the command cannot, and names one of
these in the hidden `--run-task` option instead, as `tests.serve_scripts:<name>`.
"""

import asyncio
from pathlib import Path
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


async def browse_until_cancelled(task: str, *, on_event: EventHandler, **_: Any) -> RunResult:
    """A run that never finishes. Its task is a path, and it writes that file once its browser has closed."""
    await on_event(BrowserEvent(live_url=None))
    try:
        return await asyncio.Future()
    finally:
        # A real browser takes a moment to close, and the server has to wait for it.
        await asyncio.sleep(0.05)
        await asyncio.to_thread(Path(task).write_text, "closed")
