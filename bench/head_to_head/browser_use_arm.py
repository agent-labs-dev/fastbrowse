"""The Browser Use arm: one JSON request on stdin, one JSON result on the file the request names.

Run by run.py with the interpreter of an environment holding the pinned `browser-use`. Every completion's usage is
appended to `meter` as it arrives, so a run the harness has to kill still shows what it had spent.
"""

import asyncio
import json
import sys
import time
import traceback
from typing import Any

import httpx
from browser_use import Agent, Browser, ChatOpenRouter

STEP_CEILING = 1_000_000
"""`Agent.run` takes an integer and no None; no run reaches this one, so the arm has no step cap, as fastbrowse has
none by default. The harness's wall-clock limit is what ends a run that never stops."""


async def run(request: dict[str, Any]) -> dict[str, Any]:
    with open(request["meter"], "a", encoding="utf-8") as meter:  # noqa: ASYNC230

        async def record(response: httpx.Response) -> None:
            if not response.request.url.path.endswith("/chat/completions"):
                return
            await response.aread()
            usage = response.json().get("usage") if response.is_success else None
            meter.write(json.dumps({"status": response.status_code, "at": time.time(), "usage": usage}) + "\n")
            meter.flush()

        async with httpx.AsyncClient(event_hooks={"response": [record]}, timeout=None) as http:
            llm = ChatOpenRouter(
                model=request["model"],
                temperature=0,
                http_client=http,
                extra_body={"reasoning": {"effort": request["reasoning"]}} if request["reasoning"] else None,
            )
            agent = Agent(
                task=request["goal"],
                browser=Browser(cdp_url=request["cdp_url"], keep_alive=True),
                llm=llm,
                sensitive_data=request["secrets"] or None,
                calculate_cost=False,
                directly_open_url=True,
            )
            history = await agent.run(max_steps=STEP_CEILING)
            urls = [url for url in history.urls() if url]
            return {
                "done": history.is_done(),
                "successful": history.is_successful(),
                "answer": history.final_result(),
                "final_url": urls[-1] if urls else None,
                "steps": history.number_of_steps(),
                "tool_calls": history.action_names(),
                "errors": [error for error in history.errors() if error],
            }


def main() -> None:
    request = json.load(sys.stdin)
    try:
        result = asyncio.run(run(request))
    except Exception:
        result = {"done": False, "crash": traceback.format_exc()}
    with open(request["result"], "w", encoding="utf-8") as out:
        json.dump(result, out)


if __name__ == "__main__":
    main()
