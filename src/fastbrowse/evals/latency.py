"""Which model should each purpose use? Measure, because the answer moves every few weeks.

The LLM is most of a run's wall clock: a measured cloud run spent 64% of 49 seconds inside
`generate` and 7% inside Jev. So the model behind each `LLMPurpose` is the latency decision in this
system, and `FASTBROWSE_LLM_MODEL_<PURPOSE>` exists to act on it.

This runs one fixture task (`table-extract`) through `run_task` with every purpose on each candidate in turn,
on whichever core the package runs, and reports the median seconds of the two calls that dominate a run: the
plan (small input, structured requirements) and the read (a page, quotes that must come back verbatim). A run
that did not complete is counted, since a model that is fast and unparseable costs retries and is slower than it
looks.

    uv run python -m fastbrowse.evals.latency [--models A B ...] [--repeat N]

Needs a Chrome and the keys a run needs (README, Models); Jev is called too, and left out of the timings.
"""

import argparse
import asyncio
import os
import statistics
import sys
from collections.abc import Generator
from contextlib import contextmanager

from fastbrowse import run_task
from fastbrowse.evals.harness import chrome
from fastbrowse.evals.local import fixture_server
from fastbrowse.evals.runner import LOCAL_LIMITS, fixture_config
from fastbrowse.evals.tasks import TASKS
from fastbrowse.models import CostComponent, LLMPurpose, RunResult, Status

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

TASK = next(task for task in TASKS if task.id == "table-extract")
MEASURED = (LLMPurpose.PLAN, LLMPurpose.READ)


@contextmanager
def every_purpose_on(model: str) -> Generator[None]:
    """`model` for every purpose, through the variables the README documents, restored on exit."""
    names = ["FASTBROWSE_LLM_MODEL", *(f"FASTBROWSE_LLM_MODEL_{purpose.name}" for purpose in LLMPurpose)]
    saved = {name: os.environ.get(name) for name in names}
    os.environ.update(dict.fromkeys(names, model))
    try:
        yield
    finally:
        for name, value in saved.items():
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value


def seconds(results: list[RunResult], purpose: LLMPurpose) -> list[float]:
    """Each call's wall time for `purpose`, retries included, across `results`."""
    return [
        line.seconds
        for result in results
        for line in result.cost.lines
        if line.component is CostComponent.LLM and line.purpose is purpose and line.seconds is not None
    ]


async def measure(model: str, start: str, cdp_url: str, repeat: int) -> None:
    results: list[RunResult] = []
    with every_purpose_on(model):
        # One run at a time: concurrent runs would measure our own contention.
        for _ in range(repeat):
            results.append(
                await run_task(
                    TASK.task,
                    start=start,
                    cdp_url=cdp_url,
                    config=fixture_config(),
                    inputs=TASK.inputs,
                    output_schema=TASK.output_schema,
                    authorization=TASK.authorization,
                    limits=LOCAL_LIMITS,
                )
            )
    failed = sum(result.status is not Status.COMPLETE for result in results)
    note = f"  {failed}/{repeat} runs did not complete" if failed else ""
    for purpose in MEASURED:
        timings = seconds(results, purpose)
        measured = f"median {statistics.median(timings):5.2f}s over {len(timings)} calls" if timings else "no calls"
        print(f"{model:<36} {purpose.value:<5} {measured}{note}", flush=True)


async def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--models", nargs="*", default=list(CANDIDATES))
    parser.add_argument("--repeat", type=int, default=3)
    args = parser.parse_args(argv)
    with fixture_server() as (base_url, _), chrome() as cdp_url:
        for model in args.models:
            try:
                await measure(model, base_url + TASK.start, cdp_url, args.repeat)
            except Exception as exc:
                # A missing key or a refused setting stops every model alike, so it ends the measurement.
                print(f"{model}: {type(exc).__name__}: {exc}", file=sys.stderr)
                return 1
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main(sys.argv[1:])))
