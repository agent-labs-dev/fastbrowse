"""Optional browser-use OSS arm: one JSON request on stdin, one result on stdout.

The caller owns the CDP browser and observes it after this process exits. Install only on explicit selection,
using the exact package pin in the live arm registry.
"""

import asyncio
import contextlib
import json
import sys
import time
from typing import Any


def ending(history: Any, max_steps: int) -> str:
    if history.is_done():
        return "done" if history.is_successful() is True else "stopped"
    return "budget_exceeded" if len(history.history) >= max_steps else "stopped"


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


async def run(request: dict[str, Any]) -> dict[str, Any]:
    import httpx
    from browser_use import Agent, Browser, ChatOpenRouter, Tools

    started = time.monotonic()
    costs: list[float] = []
    unknown = False

    async def meter(response: httpx.Response) -> None:
        nonlocal unknown
        if response.request.url.path.endswith("/chat/completions") and response.is_success:
            await response.aread()
            cost = (response.json().get("usage") or {}).get("cost")
            if cost is None:
                unknown = True
            else:
                costs.append(float(cost))

    try:
        schema = output_model(request.get("output_schema"))
        async with httpx.AsyncClient(event_hooks={"response": [meter]}) as http:
            llm = ChatOpenRouter(model=request["model"], temperature=0, http_client=http)
            agent = Agent(
                task=request["goal"],
                browser=Browser(cdp_url=request["cdp_ws"], keep_alive=True),
                llm=llm,
                tools=Tools(output_model=schema),
                sensitive_data=request.get("secrets") or None,
                calculate_cost=True,
                directly_open_url=True,
            )
            history = await agent.run(max_steps=request["max_steps"])
            answer = history.final_result()
            data = schema.model_validate_json(answer).model_dump() if schema is not None and answer else None
            return {
                "status": ending(history, request["max_steps"]),
                "answer": answer,
                "data": data,
                "steps": len(history.history),
                "seconds": time.monotonic() - started,
                "dollars": sum(costs) if costs and not unknown else None,
            }
    except Exception as exc:
        return {
            "status": "error",
            "error": type(exc).__name__,
            "steps": 0,
            "seconds": time.monotonic() - started,
            "dollars": None,
        }


if __name__ == "__main__":
    request = json.load(sys.stdin)
    # Framework banners must not corrupt the caller's JSON protocol.
    with contextlib.redirect_stdout(sys.stderr):
        result = asyncio.run(run(request))
    print(json.dumps(result))
