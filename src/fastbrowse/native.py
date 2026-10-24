"""`run_task` on the Rust core (crates/fastbrowse-py), in this process, when `FASTBROWSE_RUST_CORE=1`.

The arguments become the `run` request's params that `fastbrowse serve` reads, and the result comes back as the
`RunResult` it writes, so the two cores are compared through the same models. A call whose arguments the protocol
has no field for (a client of the caller's own, a cloud key, an origin scope, a non-default `Config`) runs on
Python as before.
"""

import asyncio
import base64
import json
import uuid
from collections.abc import Awaitable, Callable
from pathlib import Path
from typing import Any

from pydantic import TypeAdapter

from fastbrowse.clients.environment import ConfigurationError
from fastbrowse.config import Config
from fastbrowse.models import BrowserEvent, EventHandler, FrameHandler, RunResult, SecretResolver, StepEvent, UntilCheck
from fastbrowse.protocol import ErrorCode, RunAttachment, RunParams

_EVENT: TypeAdapter[StepEvent | BrowserEvent] = TypeAdapter(StepEvent | BrowserEvent)

_RUN_TASK_DEFAULTS: dict[str, Any] = {
    "browser_api_key": None,
    "cloud_profile": None,
    "allowed_origins": None,
    "check_access": None,
    "cloud_timeout_minutes": 15,
    "jev": None,
    "llm": None,
}
"""The `run_task` arguments the protocol has no field for, at the values that let a call cross anyway. The cloud
options among them belong to a browser started with a key, and the server reads its key from the environment."""


def params(task: str, arguments: dict[str, Any]) -> RunParams | None:
    """`run_task`'s arguments as `run` params, or None when one of them has no field to cross in."""
    if arguments["cloud_extensions"] or any(arguments[name] != default for name, default in _RUN_TASK_DEFAULTS.items()):
        return None
    config = arguments["config"]
    if config is not None and config != Config():
        return None
    handed_over = arguments["cdp_url"] is not None or arguments["cdp_port"] is not None
    output_schema = arguments["output_schema"]
    secrets: SecretResolver | None = arguments["secrets"]
    downloads: Path | None = arguments["downloads"]
    record: Path | None = arguments["record"]
    return RunParams(
        run_id=uuid.uuid4().hex,
        task=task,
        start=arguments["start"],
        inputs=None if arguments["inputs"] is None else dict(arguments["inputs"]),
        attachments=tuple(
            RunAttachment(name=a.name, mime_type=a.mime_type, content=base64.b64encode(a.content))
            for a in arguments["attachments"]
        ),
        limits=arguments["limits"],
        authorization=arguments["authorization"],
        output_schema=None if output_schema is None else output_schema.model_json_schema(),
        secrets=() if secrets is None else tuple(secrets.available()),
        frames=arguments["on_frame"] is not None,
        until=arguments["until"] is not None,
        downloads=downloads,
        record=record,
        cursor=arguments["cursor"],
        # Without a key `run_task` runs local Chrome, and the server would start a cloud browser.
        local=not handed_over,
        chrome=arguments["chrome"],
        cdp_url=arguments["cdp_url"],
        cdp_port=arguments["cdp_port"],
        attach=arguments["attach"],
        target_match=arguments["target_match"],
        foreground=arguments["foreground"],
    )


class _Raised:
    """The first exception a callback raised. `run_task` on Python lets it through, so this one does too."""

    def __init__(self) -> None:
        self.first: BaseException | None = None

    def wrap[**P, R](self, call: Callable[P, Awaitable[R]]) -> Callable[P, Awaitable[R]]:
        async def called(*args: P.args, **kwargs: P.kwargs) -> R:
            try:
                return await call(*args, **kwargs)
            except Exception as exc:
                if self.first is None:
                    self.first = exc
                raise

        return called


def _event(payload: str) -> StepEvent | BrowserEvent:
    event = json.loads(payload)
    # JSON carries the step's PNG as base64, which the model's bytes field would read as text.
    if isinstance(event.get("frame"), str):
        event["frame"] = base64.b64decode(event["frame"])
    return _EVENT.validate_python(event)


async def run(
    run_params: RunParams,
    *,
    on_event: EventHandler | None,
    on_frame: FrameHandler | None,
    until: UntilCheck | None,
    secrets: SecretResolver | None,
) -> RunResult:
    """Run `run_params` on the Rust core and return its result as Python's `run_task` returns one."""
    import fastbrowse_native  # ty: ignore[unresolved-import]

    raised = _Raised()

    async def event(payload: str) -> None:
        if on_event is not None:
            await on_event(_event(payload))

    native = fastbrowse_native.run_task(
        run_params.model_dump_json(),
        on_event=raised.wrap(event) if on_event is not None else None,
        on_frame=None if on_frame is None else raised.wrap(on_frame),
        until=None if until is None else raised.wrap(until),
        resolve_secret=None if secrets is None else raised.wrap(secrets.resolve),
    )
    try:
        payload = await native
    except fastbrowse_native.RunRefused as exc:
        code, message = exc.args
        if code == ErrorCode.CONFIGURATION:
            raise ConfigurationError(message) from None
        if code == ErrorCode.CANCELLED:
            raise asyncio.CancelledError(message) from None
        raise ValueError(message) from None
    except asyncio.CancelledError:
        # Python's `run_task` closes its browser before the cancellation reaches its caller, and so does this.
        await asyncio.shield(native.finished)
        raise
    if raised.first is not None:
        raise raised.first
    return RunResult.model_validate_json(payload)
