"""Stand-ins for `run_task` that a `fastbrowse serve` subprocess can be pointed at by name.

A test in the same process hands the server a recorder. One that starts the command cannot, and names one of
these in the hidden `--run-task` option instead, as `tests.serve_scripts:<name>`.
"""

import asyncio
from pathlib import Path
from typing import Any

from fastbrowse.models import BrowserEvent, CostBreakdown, EventHandler, RunResult, SecretResolver, Status
from fastbrowse.safety import origin_of, resolve_secret


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
    try:
        await on_event(BrowserEvent(live_url=None))
        return await asyncio.Future()
    finally:
        # A real browser takes a moment to close, and the server has to wait for it.
        await asyncio.sleep(0.05)
        await asyncio.to_thread(Path(task).write_text, "closed")


async def type_secret(task: str, *, start: str, secrets: SecretResolver, on_event: EventHandler, **_: Any) -> RunResult:
    """A run that resolves its first secret for the start page, the way the agent does before it types one.

    The answer says how long the value was and never what it was, which is as much as a real run reports.
    """
    value = await resolve_secret(secrets, secrets.available()[0].name, origin_of(start))
    await on_event(BrowserEvent(live_url=None))
    return RunResult(
        status=Status.COMPLETE,
        answer="nothing to type" if value is None else f"typed {len(value)} characters",
        data=None,
        evidence=(),
        steps=(),
        cost=CostBreakdown(lines=()),
        artifacts=(),
    )
