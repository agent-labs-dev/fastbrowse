"""Every bench cassette replays in this process at zero latency without a model request it did not record.

The `replay` CI job runs this module (`-m cassette_replay`), which installs Chrome so it cannot skip. A replay
miss means the agent now asks something its cassette lacks: refresh that task with `python -m fastbrowse.evals.bench
fill --only TASK`. Whether a task passes depends on timing and is a benchmark measurement, so only the three tasks
below, which have no race, must also pass.
"""

import socket
from pathlib import Path
from typing import Any

import pytest

from fastbrowse.evals.bench import CASSETTES, IN_PROCESS, BenchTask, bench_task, corpus
from fastbrowse.evals.harness import find_chrome
from fastbrowse.evals.replay import Cassette, Latency, Proxy

pytestmark = [
    pytest.mark.skipif(find_chrome() is None, reason="Chrome is not installed"),
    pytest.mark.cassette_replay,
]

PASSING = (
    "table-extract",
    "mock-sign-in",
    "mock-upload-document",
    "mock-long-description",
    "mock-custom-select",
    "mock-unfinished-write",
)
# Cassettes the agent's requests have moved past, each waiting on a paid `bench fill --only TASK` a maintainer starts.
# Strict, so the replay job lists them on every run and fails once the fill lands until the entry is removed.
STALE: dict[str, str] = {}


def recorded_tasks() -> list[Any]:
    """The corpus task of every cassette; a cassette no task owns is an error, not a skip."""
    tasks = {task.id: task for task in corpus()}
    ids = sorted(path.name.removesuffix(".jsonl.gz") for path in CASSETTES.glob("*.jsonl.gz"))
    orphans = [task_id for task_id in ids if task_id not in tasks]
    if orphans:
        raise LookupError(f"cassettes with no corpus task: {', '.join(orphans)}")
    return [
        pytest.param(tasks[task_id], marks=pytest.mark.xfail(reason=STALE[task_id], strict=True))
        if task_id in STALE
        else tasks[task_id]
        for task_id in ids
    ]


async def replayed(task: BenchTask, logs: Path) -> dict[str, Any]:
    proxy = Proxy(Cassette.load(CASSETTES / f"{task.id}.jsonl.gz"), forward=None, replay=True, latency=Latency.ZERO)
    return await bench_task(task, agent_argv=[IN_PROCESS], proxy=proxy, logs=logs)


def _misses(entry: dict[str, Any]) -> str:
    misses = entry["misses"]
    listed = "\n".join(f"- {miss['purpose']} ({miss['model']})" for miss in misses)
    return f"{len(misses)} replay misses; refresh with bench fill --only {entry['task']}\n{listed}\n{misses[0]['diff']}"


@pytest.mark.timeout(300)
@pytest.mark.parametrize("task", recorded_tasks(), ids=lambda task: task.id)
async def test_a_recorded_task_replays_without_a_miss(task: BenchTask, tmp_path: Path) -> None:
    entry = await replayed(task, tmp_path)
    assert entry["misses"] == [], _misses(entry)
    if task.id in PASSING:
        assert entry["result"]["grade"]["passed"], entry["result"]
    if task.id == "table-extract":
        assert entry["result"]["data"] == {"city": "Lyon", "population": 522250}


@pytest.mark.timeout(300)
async def test_a_replay_does_not_need_the_port_it_was_recorded_on(tmp_path: Path) -> None:
    """A replay bound the cassette's recorded fixture port, an ephemeral one another socket on a CI runner sometimes
    held, and failed the job (#675, #1285). Note ids are numbered in the request key and swapped back into the
    response, so a task whose requests cite notes replays with that port taken."""
    task = next(task for task in corpus() if task.id == "mock-long-description")
    cassette = Cassette.load(CASSETTES / f"{task.id}.jsonl.gz")
    origin = next(e.fixture_origin for e in reversed(cassette.exchanges) if e.fixture_origin)
    with socket.socket() as held:
        held.bind(("127.0.0.1", int(origin.rsplit(":", 1)[1])))
        held.listen()
        entry = await replayed(task, tmp_path)
    assert entry["fixture_origin"] != origin
    assert entry["misses"] == [], _misses(entry)
    assert entry["result"]["grade"]["passed"], entry["result"]
