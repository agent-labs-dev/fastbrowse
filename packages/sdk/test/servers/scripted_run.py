"""The real server, with one of the runs below standing where `run_task` does.

Started as `<this file> <run> serve --stdio`. The run is handed to the server through its hidden `--run-task`
option, so everything between the SDK and the run is the server's own code.
"""

import asyncio
import json
import os
import signal
import sys
from typing import Any

from fastbrowse import serve
from fastbrowse.models import (
    Attachment,
    BrowserEvent,
    CostBreakdown,
    Decider,
    EventHandler,
    Operation,
    RunResult,
    Status,
    StepEvent,
    StepOutcome,
    StepResult,
)

# The params of each `run` request, as the lines the client wrote and before the server has read anything into them.
requests: list[dict[str, Any]] = []


def _result(status: Status = Status.COMPLETE, **fields: Any) -> RunResult:
    return RunResult(
        **{"answer": None, "data": None, "evidence": (), "steps": (), "cost": CostBreakdown(lines=()), "artifacts": ()}
        | fields,
        status=status,
    )


def _step(index: int) -> StepEvent:
    return StepEvent(
        step=StepResult(
            index=index,
            operation=Operation.CLICK,
            decided_by=Decider.JEV,
            outcome=StepOutcome.EXECUTED,
            url="https://shop.example/",
            duration_ms=1,
        )
    )


async def echo(task: str, *, attachments: tuple[Attachment, ...], **_: Any) -> RunResult:
    """Answers with the request it was started by, and with each attachment's bytes as the run received them."""
    received = {"params": requests[-1], "attachments": [list(attachment.content) for attachment in attachments]}
    return _result(answer=task, data=received)


async def three_events(task: str, *, on_event: EventHandler, **_: Any) -> RunResult:
    await on_event(BrowserEvent(live_url="https://live.example/1", browser_id="browser-1"))
    await on_event(_step(0))
    await on_event(_step(1))
    return _result()


async def needs_login(task: str, **_: Any) -> RunResult:
    return _result(Status.NEEDS_LOGIN, error="the site asked for a password")


async def never_ends(task: str, *, on_event: EventHandler, **_: Any) -> RunResult:
    await on_event(BrowserEvent(live_url=None))
    await asyncio.Event().wait()
    return _result()


async def exits(task: str, *, on_event: EventHandler, **_: Any) -> RunResult:
    await on_event(BrowserEvent(live_url=None))
    os._exit(7)


async def killed(task: str, *, on_event: EventHandler, **_: Any) -> RunResult:
    await on_event(BrowserEvent(live_url=None))
    os.kill(os.getpid(), signal.SIGKILL)
    return _result()


def _listening(transport: serve.StdioTransport) -> serve.StdioTransport:
    receive = transport.receive

    async def receive_and_note() -> bytes | None:
        line = await receive()
        if line and (message := json.loads(line)).get("method") == "run":
            requests.append(message["params"])
        return line

    transport.receive = receive_and_note
    return transport


open_stdio = serve.stdio_transport
serve.stdio_transport = lambda: _listening(open_stdio())

sys.exit(serve.main([*sys.argv[3:], "--run-task", f"__main__:{sys.argv[1]}"]))
