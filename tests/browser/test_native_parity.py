"""`run_task` in this process gives the same result on the Rust core as on Python, given the same model responses.

A few fixture tasks from the bench, replayed from `bench/cassettes` at zero latency: one with an answer read from a
table, one that signs in with a secret, one that uploads an attachment. `python -m fastbrowse.evals.bench replay
--agent in-process` runs all of them, once per core.
"""

from pathlib import Path
from typing import Any

import pytest

from fastbrowse.adapters.local_chrome import find_chrome
from fastbrowse.evals.bench import CASSETTES, IN_PROCESS, BenchTask, bench_task, corpus
from fastbrowse.evals.replay import Cassette, Latency, Proxy
from fastbrowse.models import LocalChrome

try:
    import fastbrowse_native  # ty: ignore[unresolved-import]

    NATIVE = hasattr(fastbrowse_native, "run_task")
except ImportError:
    NATIVE = False

pytestmark = [
    pytest.mark.skipif(find_chrome(None) is None, reason="Chrome is not installed"),
    pytest.mark.skipif(not NATIVE, reason="fastbrowse_native with run_task is not installed"),
]

TASKS = ("table-extract", "mock-sign-in", "mock-upload-document")


async def replayed(task: BenchTask, logs: Path) -> dict[str, Any]:
    proxy = Proxy(Cassette.load(CASSETTES / f"{task.id}.jsonl.gz"), forward=None, replay=True, latency=Latency.ZERO)
    entry = await bench_task(task, agent_argv=[IN_PROCESS], proxy=proxy, logs=logs, chrome=LocalChrome())
    return entry["result"]


@pytest.mark.timeout(300)
@pytest.mark.parametrize("task", [task for task in corpus() if task.id in TASKS], ids=lambda task: task.id)
async def test_both_cores_reach_the_same_result(task: BenchTask, monkeypatch: pytest.MonkeyPatch, tmp_path: Path):
    monkeypatch.delenv("FASTBROWSE_RUST_CORE", raising=False)
    python = await replayed(task, tmp_path)
    monkeypatch.setenv("FASTBROWSE_RUST_CORE", "1")
    rust = await replayed(task, tmp_path)
    assert python["grade"]["passed"], python
    assert rust == python
