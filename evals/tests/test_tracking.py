import hashlib
import json
from pathlib import Path

import pytest
from fastbrowse.evals.evidence import Attempt, Campaign, Source
from parallax.judging import Decision, Judgment
from parallax.models import Verdict

from fastbrowse_evals.registry import Suite
from fastbrowse_evals.tracking import SemanticReceipt, project, semantic_receipts


@pytest.mark.parametrize("arm", ["fastbrowse", "cua-codex"])
def test_internal_source_grades_do_not_enter_parallax_semantic_comparisons(arm: str) -> None:
    attempt = Attempt(
        id="a" * 64,
        task="test-task",
        task_version="1",
        suite="dev",
        arm=arm,
        repeat=0,
        retry=0,
        selected=True,
        started=True,
        status="complete",
        passed=True,
        completed=True,
        seconds=1,
        dollars=0.2,
        unknown_cost=False,
        git_sha="b" * 40,
        git_dirty=False,
        model=None,
        started_at="2026-10-07T09:00:00Z",
    )
    run = project(Campaign(id="internal", kind="diagnostic", sources=[], attempts=[attempt]), Suite.INTERNAL)
    trace = run.runs[0].traces[0]
    assert trace.output.verdicts == []
    assert trace.output.repeats[0].verdict is Verdict.UNGRADED
    scores = {s.name: s.value for s in trace.scores}
    assert scores["recorded_positive_rate"] == 1
    assert "pass_rate" not in scores
    assert "grade:unclassified" in trace.tags


@pytest.mark.parametrize("nested", [False, True])
@pytest.mark.parametrize("value", [True, False, None])
def test_hash_pinned_browser_judgments_keep_semantic_verdicts_and_abstentions(
    tmp_path: Path, value: bool | None, nested: bool
) -> None:
    judgment = Judgment(
        policy="browser-internal-jev-v2",
        state_sha256="d" * 64,
        questions={"task_satisfied": "Task satisfied?"},
        decisions={
            "evidence_available": Decision(passed=True, source="llm", reason="Recorded execution"),
            "task_satisfied": Decision(passed=value, source="llm", reason="Recorded judgment"),
        },
    )
    receipt = SemanticReceipt(judgment=judgment, policy_sha256="e" * 64)
    attempt = Attempt(
        id="a" * 64,
        task="test-task",
        task_version="1",
        suite="dev",
        arm="fastbrowse",
        repeat=0,
        retry=1 if nested else 0,
        selected=True,
        started=True,
        status="complete",
        passed=value,
        completed=True,
        seconds=1,
        dollars=0.2,
        unknown_cost=False,
        git_sha="b" * 40,
        git_dirty=False,
        model=None,
        started_at="2026-10-07T09:00:00Z",
    )
    path = tmp_path / "docs/validation/run.jsonl"
    path.parent.mkdir(parents=True)
    raw = {"task": "test-task", "arm": "fastbrowse", "repeat": 0, "semantic_judge": receipt.model_dump(mode="json")}
    content = json.dumps({"attempt": raw, "retry": 1} if nested else raw).encode() + b"\n"
    path.write_bytes(content)
    campaign = Campaign(
        id="internal",
        kind="diagnostic",
        attempts=[attempt],
        sources=[Source(path="docs/validation/run.jsonl", sha256=hashlib.sha256(content).hexdigest())],
    )
    receipts = semantic_receipts(campaign, tmp_path)
    run = project(campaign, Suite.INTERNAL, receipts=receipts)
    trace = run.runs[0].traces[0]
    expected = Verdict.PASS if value is True else Verdict.FAIL if value is False else Verdict.UNGRADED
    assert trace.output.repeats[0].verdict is expected
    assert trace.output.verdicts == ([] if value is None else [float(value)])
    assert "grade:semantic" in trace.tags
    scores = {s.name: s.value for s in run.runs[0].scores}
    assert scores["passed_and_completed_selected_rows"] == float(value is True)
    assert "e" * 64 in trace.task_id
    assert (
        run.runs[0].metadata["semantic_receipts"][attempt.id]["judgment"]["decisions"]["task_satisfied"]["passed"]
        is value
    )
    path.write_bytes(content + b" ")
    with pytest.raises(ValueError, match="source changed"):
        semantic_receipts(campaign, tmp_path)
