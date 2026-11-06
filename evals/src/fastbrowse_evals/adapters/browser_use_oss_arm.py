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

STEP_CEILING = 1_000_000
"""What browser-use's `Agent.run(max_steps: int)` receives when the request has no limit (`max_steps: null`, which the
live harness always sends). Its loop is `for step in range(max_steps)` and accepts no None, so an integer no run can
reach stands in for "no limit"; it is reported back as `step_ceiling` and recorded with the row."""


def step_log(history: Any) -> list[dict[str, Any]]:
    """What each step did and saw, from browser-use's own history; its screenshots stay on disk."""
    steps = []
    for index, item in enumerate(history.history):
        output = item.model_output
        steps.append(
            {
                "index": index,
                "operation": "reason",
                "target": [action.model_dump(exclude_none=True, mode="json") for action in output.action]
                if output
                else None,
                "url": item.state.url,
                "observation": "\n".join(
                    str(text) for result in item.result for text in (result.extracted_content, result.error) if text
                ),
            }
        )
    return steps


def ending(history: Any, max_steps: int | None, outage: str | None = None) -> str:
    """How the run ended. A run the model provider's outage ended is `unavailable`, by the rule every arm's provider
    failures follow, so the harness runs it again rather than counting it against the agent."""
    if history.is_done():
        return "done" if history.is_successful() is True else "stopped"
    if outage is not None:
        return "unavailable"
    return "budget_exceeded" if max_steps is not None and len(history.history) >= max_steps else "stopped"


def provider_outage(status: int, body: object, retryable: frozenset[int]) -> str | None:
    """Why a model response says the provider is out, or None. OpenRouter can hold a request open, then send its error
    in a 200."""
    if status in retryable:
        return f"Model provider returned HTTP {status}"
    error = body.get("error") if isinstance(body, dict) else None
    code = error.get("code") if isinstance(error, dict) else None
    return f"Model provider returned error {code} in HTTP {status}" if code in retryable else None


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
    # What the last model request met, if the provider was out. Each answer replaces it, so an outage the agent's own
    # retries outlasted ends nothing.
    outage: str | None = None
    retryable = frozenset(request["retryable_status"])

    class Monitored(httpx.AsyncHTTPTransport):
        async def handle_async_request(self, sent: httpx.Request) -> httpx.Response:
            nonlocal outage, unknown
            try:
                return await super().handle_async_request(sent)
            except (httpx.TimeoutException, httpx.NetworkError, httpx.RemoteProtocolError, httpx.ProxyError) as error:
                # No answer means no receipt, and the provider may still have charged the request.
                outage, unknown = f"Model connection failed ({type(error).__name__})", True
                raise

    async def meter(response: httpx.Response) -> None:
        nonlocal outage, unknown
        if not response.request.url.path.endswith("/chat/completions"):
            return
        try:
            await response.aread()
        except httpx.TransportError as error:
            outage, unknown = f"Model connection failed ({type(error).__name__})", True
            raise
        body = response.json() if response.is_success else None
        outage = provider_outage(response.status_code, body, retryable)
        if response.is_success:
            cost = (body.get("usage") or {}).get("cost")
            if cost is None:
                unknown = True
            else:
                costs.append(float(cost))

    agent = None
    try:
        schema = output_model(request.get("output_schema"))
        async with httpx.AsyncClient(transport=Monitored(), event_hooks={"response": [meter]}) as http:
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
            ceiling = STEP_CEILING if request["max_steps"] is None else request["max_steps"]
            at = time.time()
            history = await agent.run(max_steps=ceiling)
            # browser-use connects and opens the start page before its first step, so that step's own start ends setup.
            first = next((item.metadata for item in history.history if item.metadata is not None), None)
            setup = 0.0 if first is None else max(0.0, first.step_start_time - at)
            answer = history.final_result()
            data = schema.model_validate_json(answer).model_dump() if schema is not None and answer else None
            return {
                "status": ending(history, request["max_steps"], outage),
                "error": outage,
                "answer": answer,
                "data": data,
                "steps": len(history.history),
                "step_log": step_log(history),
                "seconds": time.monotonic() - started - setup,
                "setup_seconds": round(setup, 2),
                "dollars": sum(costs) if costs and not unknown else None,
                "step_ceiling": ceiling,
            }
    except Exception as exc:
        return {
            "status": "unavailable" if outage else "error",
            "error": outage or type(exc).__name__,
            "steps": 0 if agent is None else len(agent.history.history),
            "seconds": time.monotonic() - started,
            "dollars": None,
        }


if __name__ == "__main__":
    request = json.load(sys.stdin)
    # Framework banners must not corrupt the caller's JSON protocol.
    with contextlib.redirect_stdout(sys.stderr):
        result = asyncio.run(run(request))
    print(json.dumps(result))
