"""Optional browser-use OSS arm, run as a conversation of JSON lines with the harness.

browser-use 1.0 as a user of it runs it: its default model (`bu-fast` on the Browser Use gateway, read with the
Browser Use key) in its default mode (`ultrafast` on bu-fast), on the Browser Use Cloud browser its own `Browser()`
creates, so every feature 1.0 gives its own cloud browser is on. The conversation lets the harness time the run on its
own clock and read the final page before the browser stops:

    harness -> {request}                     this process starts its browser
    harness <- {"event": "ready", ...}       the browser is up: the harness's clock starts
    harness <- {"event": "result", ...}      the agent answered: the clock stops, the harness reads the final page
    harness -> {}                            this process stops its browser
    harness <- {"event": "stopped", ...}

A line from the harness that never comes (it exited) stops the browser too. Install only on explicit selection, using
the exact package pin in the live arm registry.
"""

import asyncio
import contextlib
import json
import sys
import time
from collections.abc import Awaitable, Callable
from typing import Any

STEP_CEILING = 1_000_000
"""What browser-use's `Agent.run(max_steps: int)` receives when the request has no limit (`max_steps: null`, which the
live harness always sends). 1.0 defaults to 100 steps and takes no None, so an integer no run can reach stands in for
"no limit"; it is reported back as `step_ceiling` and recorded with the row."""
TIMEOUT_CEILING = 86_400.0
"""What `Agent.run(timeout: float)` receives in place of no limit. 1.0 defaults to 1700 s and takes no None; a day is
far past any run, so it overrides the default without ever being reached, and is recorded with the row."""
OBSERVATION_CHARS = 2000


def ending(result: Any, outage: str | None) -> str:
    """How the run ended. 1.0 delivers once more at its step or time limit and still says `completed`, with the agent's
    own verdict in `success`: None when it gave none, which 1.0 says to read as 0.x's default of success. A run the
    model provider's outage ended is `unavailable`, by the rule every arm's provider failures follow."""
    if result.status == "completed":
        return "stopped" if result.success is False else "done"
    if result.status in {"max_steps", "timeout"}:
        return "budget_exceeded"
    return "unavailable" if outage is not None else "error"


def provider_outage(error: str | None, transient: frozenset[str]) -> str | None:
    """Why the error that ended the run says the model provider is out, or None. 1.0 hands that error back only as
    text, written `<ExceptionType>: <message>` (its `described()`), and names the provider failures it treats as
    transient in `browser_use.agent.loop.TRANSIENT` (rate limited, unavailable, server error, connection failed). It
    has already retried such an error once, so this one outlasted its own retry."""
    if error is None:
        return None
    kind = error.partition(":")[0]
    return f"Model provider failed ({kind})" if kind in transient else None


def step_log(messages: list[Any]) -> list[dict[str, Any]]:
    """What each cell ran and what it printed, from the run's transcript (Pydantic AI messages); screenshots stay out."""
    returns = {
        part.tool_call_id: part.content
        for message in messages
        for part in getattr(message, "parts", ())
        if getattr(part, "part_kind", None) == "tool-return"
    }
    steps = []
    for message in messages:
        for part in getattr(message, "parts", ()):
            if getattr(part, "part_kind", None) != "tool-call":
                continue
            seen = returns.get(part.tool_call_id)
            steps.append(
                {
                    "index": len(steps),
                    "operation": part.tool_name,
                    "target": part.args if isinstance(part.args, (str, dict)) else str(part.args),
                    "url": None,
                    "observation": (seen if isinstance(seen, str) else json.dumps(seen, default=str))[
                        :OBSERVATION_CHARS
                    ]
                    if seen is not None
                    else "",
                }
            )
    return steps


def output_model(schema: dict[str, Any] | None) -> Any:
    from pydantic import create_model

    if schema is None:
        return None
    types = {"string": str, "integer": int, "number": float, "boolean": bool}
    fields = {}
    for name, definition in schema["properties"].items():
        if definition.get("type") not in types:
            raise ValueError(f"unsupported output schema field {name}")
        fields[name] = (types[definition["type"]], ...)
    return create_model(schema.get("title", "Answer"), **fields)


def failed(exc: Exception) -> dict[str, Any]:
    return {"status": "error", "error": f"{type(exc).__name__}: {exc}", "steps": 0, "dollars": None}


async def run(request: dict[str, Any], browser: Any) -> dict[str, Any]:
    """One agent run on a started browser; the harness times it, so no seconds are reported."""
    from browser_use import Agent
    from browser_use.agent.loop import TRANSIENT

    try:
        agent = Agent(
            task=request["goal"],
            browser=browser,
            output_model_schema=output_model(request.get("output_schema")),
            sensitive_data=request.get("secrets") or None,
        )
        ceiling = STEP_CEILING if request["max_steps"] is None else request["max_steps"]
        result = await agent.run(max_steps=ceiling, timeout=TIMEOUT_CEILING)
    except Exception as exc:
        return failed(exc)
    outage = provider_outage(result.error, frozenset(kind.__name__ for kind in TRANSIENT))
    structured = result.structured_output
    return {
        "status": ending(result, outage),
        "error": outage or result.error,
        "answer": result.final_result(),
        "data": structured.model_dump() if structured is not None and hasattr(structured, "model_dump") else None,
        "steps": result.steps,
        "step_log": step_log(result.messages),
        "dollars": result.cost_usd,
        "step_ceiling": ceiling,
        "timeout_ceiling": TIMEOUT_CEILING,
        "usage": dict(result.usage),
    }


async def session(read: Callable[[], Awaitable[str]], emit: Callable[[dict[str, Any]], None]) -> None:
    from browser_use import Browser

    request = json.loads(await read())
    # 1.0's default browser in every respect but two: it stays up after the run so the harness can read the final
    # page, and it lives for the provider's maximum instead of its 60 minute default, a time cap the protocol lifts.
    browser = Browser(keep_alive=True, cloud_timeout=request["browser_minutes"])
    try:
        began = time.monotonic()
        try:
            await browser.start()
        except Exception as exc:
            emit({"event": "result", **failed(exc)})
            return
        emit(
            {
                "event": "ready",
                "cdp_url": browser.cloud_cdp_url,
                "live_url": browser.live_url,
                "browser_id": browser.id,
                "open_seconds": round(time.monotonic() - began, 2),
            }
        )
        emit({"event": "result", **await run(request, browser)})
        await read()
    finally:
        browser_id = browser.id
        stopped = True
        try:
            await browser.stop()
        except Exception:
            stopped = False
        emit({"event": "stopped", "browser_id": browser_id, "stopped": stopped})


if __name__ == "__main__":
    out = sys.stdout

    def emit(message: dict[str, Any]) -> None:
        print(json.dumps(message, default=str), file=out, flush=True)

    async def read() -> str:
        return await asyncio.to_thread(sys.stdin.readline)

    # Framework banners must not corrupt the caller's JSON protocol.
    with contextlib.redirect_stdout(sys.stderr):
        asyncio.run(session(read, emit))
