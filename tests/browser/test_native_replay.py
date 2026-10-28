"""`run_task` in this process passes recorded fixture tasks, given the model responses they were recorded with.

A few fixture tasks from the bench, replayed from `bench/cassettes` at zero latency: one with an answer read from a
table, one that signs in with a secret, one that uploads an attachment. `python -m fastbrowse.evals.bench replay
--agent in-process` runs all of them.
"""

from pathlib import Path
from typing import Any

import pytest

from fastbrowse.evals.bench import CASSETTES, IN_PROCESS, BenchTask, bench_task, corpus
from fastbrowse.evals.harness import find_chrome
from fastbrowse.evals.replay import Cassette, Latency, Proxy

pytestmark = pytest.mark.skipif(find_chrome() is None, reason="Chrome is not installed")

TASKS = ("table-extract", "mock-sign-in", "mock-upload-document")


async def replayed(task: BenchTask, logs: Path) -> dict[str, Any]:
    proxy = Proxy(Cassette.load(CASSETTES / f"{task.id}.jsonl.gz"), forward=None, replay=True, latency=Latency.ZERO)
    entry = await bench_task(task, agent_argv=[IN_PROCESS], proxy=proxy, logs=logs)
    return entry["result"]


@pytest.mark.timeout(300)
@pytest.mark.parametrize("task", [task for task in corpus() if task.id in TASKS], ids=lambda task: task.id)
async def test_a_recorded_task_passes(task: BenchTask, tmp_path: Path) -> None:
    result = await replayed(task, tmp_path)
    assert result["grade"]["passed"], result
