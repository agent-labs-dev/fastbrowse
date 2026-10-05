"""Module execution must use the grade model imported by the independent grader."""

import json
import subprocess
import sys
from pathlib import Path


def test_module_execution_writes_a_native_grade(tmp_path: Path) -> None:
    script = """
import runpy
import sys
from fastbrowse.evals import datasets
from fastbrowse.models import CostBreakdown, RunResult, Status
import fastbrowse.run as embedded

async def tasks(source, http, **kwargs):
    return [datasets.ExternalTask(
        source="windtunnel", id="fixture-answer", stratum="answer",
        start="https://fixture.test/", task="Name the provider.",
        metadata={"predicate": {"type": "answer", "contains": ["Jane"]}},
    )]
async def reachable(task, http):
    return datasets.Reachability(task=task.id, start=task.start, reachable=True, reason="fixture")
async def run_task(*args, **kwargs):
    return RunResult(status=Status.COMPLETE, answer="Jane", data=None, evidence=(),
                     steps=(), cost=CostBreakdown(), artifacts=())
datasets.load_tasks = tasks
datasets.precheck = reachable
embedded.run_task = run_task
# An embedding caller can import the grader before invoking the module entrypoint.
from fastbrowse.evals.external_grade import predicate_grader
sys.argv = ["corpus", "windtunnel", "--execute", "--out", sys.argv[1], "--site-urls", sys.argv[2]]
runpy.run_module("fastbrowse.evals.corpus", run_name="__main__")
"""
    out = tmp_path / "attempt"
    sites = tmp_path / "sites.json"
    sites.write_text("{}")
    result = subprocess.run(
        [sys.executable, "-c", script, str(out), str(sites)], capture_output=True, text=True, timeout=30
    )
    assert result.returncode == 0, result.stderr
    rows = [json.loads(line) for line in (out / "attempts.jsonl").read_text().splitlines()]
    assert len(rows) == 1
    assert rows[0]["grade"]["grader"] == "windtunnel-answer"
    assert rows[0]["graded"] is True and rows[0]["passed"] is True
