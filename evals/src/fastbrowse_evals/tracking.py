"""Project an internal campaign onto Parallax's tracked-run shape, for `track-ledger` and publication readback.

No agent or grader runs here."""

import hashlib
import json
import statistics
from collections import defaultdict
from datetime import datetime
from pathlib import Path
from typing import Any

from fastbrowse.evals.evidence import Campaign
from parallax.judging import Judgment
from parallax.models import Verdict
from parallax.track import projection as tracking
from pydantic import BaseModel, Field

from fastbrowse_evals.judging import browser_verdict
from fastbrowse_evals.registry import BENCHMARKS, Suite


class SemanticReceipt(BaseModel):
    judgment: Judgment
    policy_sha256: str = Field(pattern=r"^[a-f0-9]{64}$")


def semantic_receipts(campaign: Campaign, root: Path) -> dict[str, SemanticReceipt]:
    """Recover complete semantic receipts from the exact hash-pinned raw source ledger."""
    ledgers = [source for source in campaign.sources if source.path.endswith(".attempts.jsonl")]
    if not ledgers:
        ledgers = [
            source
            for source in campaign.sources
            if source.path.endswith(".jsonl") and not source.path.endswith(".budget.jsonl")
        ]
    if not ledgers:
        return {}
    source = ledgers[-1]
    content = (root / source.path).read_bytes()
    if hashlib.sha256(content).hexdigest() != source.sha256:
        raise ValueError(f"{source.path}: source changed since evidence collection")
    rows = [json.loads(line) for line in content.splitlines() if line.strip()]
    if len(rows) != len(campaign.attempts):
        raise ValueError(f"{source.path}: raw ledger and projected attempts differ")
    receipts = {}
    for attempt, row in zip(campaign.attempts, rows, strict=True):
        raw = row.get("attempt", row)
        receipt = raw.get("semantic_judge") or (raw.get("run") or {}).get("semantic_judge")
        if receipt is None and raw.get("grade"):
            evidence = raw["grade"].get("evidence", {})
            if "judgment" in evidence:
                receipt = evidence
        if receipt is not None:
            identity = (
                raw.get("task") or row.get("task"),
                raw.get("arm") or row.get("arm"),
                raw.get("repeat", row.get("repeat", 0)),
                row.get("retry", raw.get("retries") or 0),
            )
            expected = (attempt.task, attempt.arm, attempt.repeat, attempt.retry)
            if identity != expected:
                raise ValueError(f"{attempt.id}: raw ledger identity disagrees with projected attempt")
            parsed = SemanticReceipt.model_validate(receipt)
            if browser_verdict(parsed.judgment)[0] is not attempt.passed:
                raise ValueError(f"{attempt.id}: raw verdict disagrees with semantic judgment")
            receipts[attempt.id] = parsed
    return receipts


def project(campaign: Campaign, suite: Suite, *, receipts: dict[str, SemanticReceipt] | None = None) -> Any:
    known_receipts = {} if receipts is None else receipts
    semantic = bool(known_receipts)
    benchmark = BENCHMARKS[suite]
    if any(a.arm not in benchmark.arms for a in campaign.attempts):
        raise ValueError("campaign contains an agent outside this benchmark's declared comparison")
    if suite == Suite.BU_BENCH:
        raise ValueError("BU Bench requires weighted-score tracking, not a binary evidence projection")
    allowed = (
        {"core", "dev", "heldout", "stretch-dev", "stretch-heldout", "local", "mock", "mock-completion", "mock-safety"}
        if suite == Suite.INTERNAL
        else {suite.value}
    )
    if any(a.suite not in allowed for a in campaign.attempts):
        raise ValueError("campaign contains tasks from another benchmark")
    dataset = f"browser-use/{suite.value}"
    identity = {
        **campaign.model_dump(),
        "semantic_receipts": {k: v.model_dump(mode="json") for k, v in known_receipts.items()},
    }
    digest = hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()

    def passed(attempt: Any) -> bool | None:
        if not semantic:
            return attempt.passed
        receipt = known_receipts.get(attempt.id)
        return None if receipt is None else browser_verdict(receipt.judgment)[0]

    def judge_scores(attempts: list[Any]) -> list[Any]:
        measured = [known_receipts[a.id].judgment.cost() for a in attempts if a.id in known_receipts]
        return (
            [
                tracking.Score("known_judge_cost_usd", sum(item[0] for item in measured), "NUMERIC"),
                tracking.Score("unpriced_judge_attempts", sum(item[1] for item in measured), "NUMERIC"),
            ]
            if semantic
            else []
        )

    def detail(attempt: Any, index: int) -> Any:
        value = passed(attempt)
        verdict = Verdict.PASS if value is True else Verdict.FAIL if value is False else Verdict.UNGRADED
        return tracking.RepeatDetail(
            repeat=index,
            verdict=verdict if semantic else Verdict.UNGRADED,
            score=float(value is True),
            cost_usd=attempt.dollars,
            agent_ms=None,
            wall_ms=round(attempt.seconds * 1000) if attempt.seconds is not None else None,
            invalid_reason=None
            if semantic and value is not None
            else "semantic judgment unavailable"
            if semantic
            else "raw ledger result only; semantic receipt unavailable",
            flags=[
                f"repeat:{attempt.repeat}",
                f"retry:{attempt.retry}",
                f"selected:{attempt.selected}",
                f"started:{attempt.started}",
                f"completed:{attempt.completed}",
                f"status:{attempt.status}",
                f"{'semantic' if semantic else 'source'}-grade:{value}",
                *(["score:not-applicable"] if value is None else []),
            ],
        )

    groups: dict[tuple[str, str | None], list[Any]] = defaultdict(list)
    for attempt in campaign.attempts:
        groups[attempt.arm, attempt.git_sha].append(attempt)
    covered = len({(a.suite, a.task, a.arm, a.repeat) for a in campaign.attempts if a.selected and a.started})
    policies: dict[tuple[str, str], str] = {}
    for attempt in campaign.attempts:
        receipt = known_receipts.get(attempt.id)
        if receipt is not None:
            key = (attempt.suite, attempt.task)
            previous = policies.get(key)
            if previous is not None and previous != receipt.policy_sha256:
                raise ValueError(f"{key}: incompatible semantic policies in one campaign")
            policies[key] = receipt.policy_sha256
    items: dict[str, Any] = {}
    runs = []
    for (arm, commit), attempts in groups.items():
        recorded_models = sorted({a.model for a in attempts if a.model is not None})
        model = ", ".join(recorded_models) or None
        row = f"{arm}/{model or 'unknown'}/{commit or 'unknown'}"
        tasks: dict[str, list[Any]] = defaultdict(list)
        for attempt in attempts:
            # An unknown grader version cannot pair with another campaign's task by accident.
            task_id = f"{attempt.suite}/{attempt.task}:{attempt.task_version or digest}"
            if semantic:
                policy = policies.get((attempt.suite, attempt.task))
                task_id += f":{policy or 'unavailable'}"
            tasks[task_id].append(attempt)
        traces = []
        for task_id, task_attempts in tasks.items():
            version = task_attempts[0].task_version or 1
            item_input = tracking.ItemInput(task_id=task_id, task_version=version)
            iid = tracking.item_id(dataset, task_id, version)
            items[iid] = tracking.ItemSpec(
                id=iid,
                input=item_input,
                metadata={
                    "benchmark": benchmark.model_dump(mode="json"),
                    "task_version": task_attempts[0].task_version,
                },
            )
            selected = [a for a in task_attempts if a.selected and a.started]
            verdicts = [float(value) for a in selected if (value := passed(a)) is not None]
            unknown = sum(a.unknown_cost for a in task_attempts)
            known_cost = sum(a.dollars for a in task_attempts if a.dollars is not None)
            cost = known_cost / len(selected) if selected and not unknown else None
            scores = [
                tracking.Score(
                    "graded_semantic_runs" if semantic else "recorded_grade_runs", float(len(verdicts)), "NUMERIC"
                ),
                tracking.Score(
                    "ungraded_semantic_runs" if semantic else "missing_recorded_grade_runs",
                    float(len(selected) - len(verdicts)),
                    "NUMERIC",
                ),
                tracking.Score("known_attempt_cost_usd", float(known_cost), "NUMERIC"),
                tracking.Score("unknown_cost_attempts", float(unknown), "NUMERIC"),
            ]
            scores.extend(judge_scores(task_attempts))
            if verdicts:
                scores.append(
                    tracking.Score(
                        "pass_rate" if semantic else "recorded_positive_rate", statistics.fmean(verdicts), "NUMERIC"
                    )
                )
            if selected:
                scores.append(
                    tracking.Score("completion_rate", statistics.fmean(float(a.completed) for a in selected), "NUMERIC")
                )
            if cost is not None:
                scores.append(tracking.Score("cost_usd", float(cost), "NUMERIC", "all attempts per selected slot"))
            output = tracking.ItemOutput(
                outcome=(
                    tracking.Outcome.PASS
                    if verdicts and all(verdicts)
                    else tracking.Outcome.FAIL
                    if verdicts and not any(verdicts)
                    else tracking.Outcome.FLAKY
                    if verdicts
                    else tracking.Outcome.INVALID
                )
                if semantic
                else tracking.Outcome.INVALID,
                verdicts=verdicts if semantic else [],
                failed_checks=[],
                cost_usd=cost,
                agent_ms=None,
                harness_version=commit or "unknown",
                config_hash=digest,
                actual_models=sorted({a.model for a in task_attempts if a.model is not None}),
                repeats=[detail(a, i) for i, a in enumerate(task_attempts)],
            )
            traces.append(
                tracking.TraceSpec(
                    item_id=iid,
                    name=task_id,
                    task_id=task_id,
                    tags=(
                        f"campaign:{campaign.id}",
                        f"harness:{arm}",
                        f"kind:{campaign.kind}",
                        "family:browser-use",
                        f"benchmark:{suite.value}",
                        f"origin:{benchmark.origin}",
                        "visibility:private",
                        "grade:semantic" if semantic else "grade:unclassified",
                    ),
                    version=row,
                    input=item_input,
                    output=output,
                    scores=tuple(scores),
                )
            )
        metadata: dict[str, object] = {
            "benchmark_family": "browser-use",
            "benchmark_suite": suite.value,
            "benchmark_origin": benchmark.origin,
            "metric": "pass_rate" if semantic else "recorded_positive_rate",
            "campaign": campaign.id,
            "kind": campaign.kind,
            "projection_sha256": digest,
            "grading": "recorded-semantic-judge" if semantic else "recorded-unclassified-grade",
            "semantic_receipts": {
                a.id: known_receipts[a.id].model_dump(mode="json") for a in attempts if a.id in known_receipts
            },
            "campaign_scheduled_slots": campaign.scheduled_slots,
            "campaign_covered_slots": covered if campaign.scheduled_slots is not None else None,
            "campaign_missing_slots": max(0, campaign.scheduled_slots - covered)
            if campaign.scheduled_slots is not None
            else None,
            "coverage": "unknown" if campaign.scheduled_slots is None else "recorded",
            "git_sha": commit or "unknown",
            "physical_attempts": str(len(attempts)),
            "sources": [s.model_dump() for s in campaign.sources],
        }
        selected = [a for a in attempts if a.selected and a.started]
        graded = [a for a in selected if passed(a) is not None]
        scores = [
            tracking.Score(
                "semantic_passes" if semantic else "recorded_positive_results",
                float(sum(passed(a) is True for a in graded)),
                "NUMERIC",
            ),
            tracking.Score("graded_selected_rows", float(len(graded)), "NUMERIC"),
            tracking.Score("ungraded_selected_rows", float(len(selected) - len(graded)), "NUMERIC"),
            tracking.Score("completed_selected_rows", float(sum(a.completed for a in selected)), "NUMERIC"),
            tracking.Score(
                "passed_and_completed_selected_rows",
                float(sum(a.completed and passed(a) is True for a in selected)),
                "NUMERIC",
            ),
            tracking.Score(
                "known_attempt_cost_usd", float(sum(a.dollars for a in attempts if a.dollars is not None)), "NUMERIC"
            ),
            tracking.Score("unknown_cost_attempts", float(sum(a.unknown_cost for a in attempts)), "NUMERIC"),
        ]
        scores.extend(judge_scores(attempts))
        if graded:
            scores.append(
                tracking.Score(
                    "pass_rate" if semantic else "recorded_positive_rate",
                    statistics.fmean(float(value) for a in graded if (value := passed(a)) is not None),
                    "NUMERIC",
                )
            )
        runs.append(
            tracking.RunSpec(
                name=f"{campaign.id}/{row}/{digest[:16]}",
                row=row,
                harness=arm,
                model=model,
                description="Recorded semantic judgments and completion, no rerun or regrading."
                if semantic
                else "Recorded source grades and completion, no rerun or regrading.",
                metadata=metadata,
                scores=tuple(scores),
                traces=tuple(traces),
            )
        )
    starts = [a.started_at for a in campaign.attempts if a.started_at is not None]
    if not starts:
        raise ValueError(f"{campaign.id}: no recorded start time; cannot invent a historical timestamp")
    return tracking.TrackedRun(
        dataset=dataset,
        run_id=digest,
        created_at=min(datetime.fromisoformat(value) for value in starts),
        items=tuple(items.values()),
        runs=tuple(runs),
    )
