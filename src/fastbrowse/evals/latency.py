"""Which model should each purpose use? Measure, because the answer moves every few weeks.

The LLM is most of a run's wall clock: a measured cloud run spent 64% of 49 seconds inside
`generate` and 7% inside Jev. So the model behind each `LLMPurpose` is the latency decision in this
system, and the per-purpose routing in `OpenAICompatibleLLM` exists to act on it.

This sends the same two requests to every candidate, shaped like the two that dominate a run: a plan
(small input, structured requirements) and a read (a page-sized input, quotes that must
come back verbatim). It reports median latency and whether the schema came back valid at all, since
a model that is fast and unparseable costs a retry and is slower than it looks.

    uv run python -m fastbrowse.evals.latency [--models A B ...] [--repeat N]
"""

import argparse
import asyncio
import hashlib
import re
import statistics
import sys
import time
from datetime import UTC, datetime
from pathlib import Path

import httpx

from fastbrowse.clients.environment import ConfigurationError, load_settings
from fastbrowse.clients.openai_compatible import OpenAICompatibleLLM, ReasoningEffort
from fastbrowse.llm import LLMError
from fastbrowse.memory import Notes
from fastbrowse.models import LLMPurpose
from fastbrowse.page import Block, BlockKind, Capture
from fastbrowse.planner import make_plan
from fastbrowse.retrieval import read

CANDIDATES = (
    "google/gemini-3.8-flash",
    "google/gemini-3.5-flash-lite",
    "z-ai/glm-5.3-flash",
    "qwen/qwen3.8-flash",
    "deepseek/deepseek-v4-flash-0731",
    "inclusionai/ling-3.0-flash",
    "bytedance-seed/seed-2-1-turbo",
    "stepfun/step-3.7-flash",
    "minimax/minimax-m3",
)


def capture() -> Capture:
    """A page-sized input, taken from a fixture so the measurement needs no network."""
    html = (Path(__file__).parent / "fixtures" / "shop.html").read_text(encoding="utf-8")
    text = re.sub(r"<[^>]+>", " ", html)
    text = re.sub(r"\s+", " ", text).strip()
    return Capture(
        url="https://shop.test/",
        title="Shop",
        captured_at=datetime.now(UTC),
        text=text,
        sha256=hashlib.sha256(text.encode()).hexdigest(),
        blocks=(Block(source_id="b1", kind=BlockKind.PARAGRAPH, frame_id=None, start=0, end=len(text)),),
    )


async def measure(model: str, http: httpx.AsyncClient, key: str, reasoning: ReasoningEffort, repeat: int) -> None:
    llm = OpenAICompatibleLLM(
        key,
        http=http,
        base_url="https://openrouter.ai/api/v1",
        models=dict.fromkeys(LLMPurpose, model),
        reasoning_effort=reasoning,
    )
    page = capture()
    for label in ("plan", "read"):
        timings: list[float] = []
        failures = 0
        for _ in range(repeat):
            started = time.monotonic()
            try:
                if label == "plan":
                    await make_plan(
                        llm, "Find the cheapest kettle on the shop and tell me its price and its delivery time."
                    )
                else:
                    await read(llm, page, "What products are listed, and at what prices?", ("r1",), Notes())
            except LLMError:
                # Timed out of the median deliberately: a failure's duration says nothing about how
                # fast this model answers, and averaging it in would flatter a model that gave up early.
                failures += 1
                continue
            timings.append(time.monotonic() - started)
        median = statistics.median(timings) if timings else float("nan")
        note = f"  {failures}/{repeat} unparseable" if failures else ""
        print(f"{model:<36} {label:<5} median {median:5.2f}s{note}", flush=True)


async def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--models", nargs="*", default=list(CANDIDATES))
    parser.add_argument("--repeat", type=int, default=3)
    args = parser.parse_args(argv)
    settings = load_settings()
    try:
        key = settings.openrouter_key()
    except ConfigurationError as exc:
        print(exc, file=sys.stderr)
        return 1
    async with httpx.AsyncClient(timeout=120) as http:
        # One model at a time: concurrent candidates would measure our own contention.
        for model in args.models:
            await measure(model, http, key, settings.llm_reasoning, args.repeat)
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main(sys.argv[1:])))
