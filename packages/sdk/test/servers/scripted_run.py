"""The real server, with one of the runs below standing where `run_task` does.

Started as `<this file> <run> serve --stdio`. The run is handed to the server through its hidden `--run-task`
option, so everything between the SDK and the run is the server's own code.
"""

import asyncio
import json
import os
import signal
import sys
from pathlib import Path
from typing import Any

from pydantic import BaseModel

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


async def found(task: str, *, inputs: dict[str, str], output_schema: type[BaseModel], **_: Any) -> RunResult:
    """Fills the model the server built from the request's schema with the JSON in the `found` input."""
    names = {field.alias or name: name for name, field in output_schema.model_fields.items()}
    values = {names[key]: value for key, value in json.loads(inputs["found"]).items()}
    return _result(data=output_schema.model_validate(values).model_dump(mode="json"))


async def request_count(task: str, **_: Any) -> RunResult:
    """Answers with how many `run` requests the server has read, this one included."""
    return _result(data=len(requests))


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


async def until_cancelled(task: str, *, inputs: dict[str, str] | None, on_event: EventHandler, **_: Any) -> RunResult:
    """Answers with how many runs the server has been asked for.

    Given a `closed` input it never finishes, and writes the file that input names once its browser has closed.
    """
    await on_event(BrowserEvent(live_url=None))
    if not inputs or "closed" not in inputs:
        return _result(data=len(requests))
    try:
        return await asyncio.Future()
    finally:
        # A real browser takes a moment to close, and the server has to wait for it.
        await asyncio.sleep(0.05)
        await asyncio.to_thread(Path(inputs["closed"]).write_text, "closed")


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
