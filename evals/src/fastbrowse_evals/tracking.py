"""Project an internal campaign onto Parallax's tracked-run shape, for `track-ledger` and publication readback.

No agent or grader runs here."""

import hashlib
import importlib.metadata
import json
import statistics
from collections import defaultdict
from collections.abc import Mapping
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any

from fastbrowse.evals.evidence import Campaign
from parallax.judging import Judgment
from parallax.models import Verdict
from parallax.track import projection as tracking
from parallax.track import sink
from pydantic import BaseModel, Field

from fastbrowse_evals.execution_receipts import ValidatedExecution, coverage
from fastbrowse_evals.judging import browser_verdict
from fastbrowse_evals.registry import BENCHMARKS, Suite


class SemanticReceipt(BaseModel):
    judgment: Judgment
    policy_sha256: str = Field(pattern=r"^[a-f0-9]{64}$")


def source_rows(campaign: Campaign, root: Path) -> list[tuple[Any, dict[str, Any]]]:
    """Each projected attempt beside its row in the exact hash-pinned raw source ledger; empty without one."""
    ledgers = [source for source in campaign.sources if source.path.endswith(".attempts.jsonl")]
    if not ledgers:
        ledgers = [
            source
            for source in campaign.sources
            if source.path.endswith(".jsonl") and not source.path.endswith(".budget.jsonl")
        ]
    if not ledgers:
        return []
    source = ledgers[-1]
    content = (root / source.path).read_bytes()
    if hashlib.sha256(content).hexdigest() != source.sha256:
        raise ValueError(f"{source.path}: source changed since evidence collection")
    rows = [json.loads(line) for line in content.splitlines() if line.strip()]
    if len(rows) != len(campaign.attempts):
        raise ValueError(f"{source.path}: raw ledger and projected attempts differ")
    return list(zip(campaign.attempts, rows, strict=True))


def semantic_receipts(campaign: Campaign, root: Path) -> dict[str, SemanticReceipt]:
    """Recover complete semantic receipts from the exact hash-pinned raw source ledger."""
    receipts = {}
    for attempt, row in source_rows(campaign, root):
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


@dataclass(frozen=True)
class LedgerRun(tracking.TrackedRun):
    """A tracked campaign with each item's semantic receipts, keyed by (trace version, item id). A receipt is one
    attempt's whole judgment, so it travels in that item's evidence zip, not in run metadata."""

    receipts: Mapping[tuple[str, str], tuple[dict[str, Any], ...]] = field(default_factory=dict)


def ledger_rows(campaign: Campaign, root: Path) -> dict[str, dict[str, Any]]:
    """Each attempt's raw ledger row by attempt id, from the same hash-pinned source as its receipt."""
    return {attempt.id: row.get("attempt", row) for attempt, row in source_rows(campaign, root)}


PARALLAX = f"parallax-evals=={importlib.metadata.version('parallax-evals')}"

ROW_CONFIG = ("model", "text_model", "step_cap", "step_cap_unit")
RUN_CONFIG = (
    "providers",
    "timing_basis",
    "session_open_limit_seconds",
    "max_steps",
    "max_seconds",
    "max_dollars",
    "agent_limits",
    "hosted_max_cost_usd",
    "cloud_browser_timeout_minutes",
)
"""What configures an arm, as a ledger row and its run record it. Concurrency, budget and argv choose how a campaign
runs, not what an arm is, so two campaigns of one arm compare whatever they were scheduled with."""


def provenance_of(row: dict[str, Any] | None) -> dict[str, Any]:
    """The run record a ledger row carries (`live.provenance`), or empty for a row without one."""
    run = (row or {}).get("run")
    return run if isinstance(run, dict) and "git_sha" in run else {}


def short_hash(value: object) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True, default=str).encode()).hexdigest()[:16]


def arm_version(arm: str, run: dict[str, Any]) -> str:
    """The arm's version as Parallax means it: the release a fastbrowse arm installed, else what the arm is pinned to.
    The commit is the treatment a comparison measures, so it is metadata (`git_sha`), not the version."""
    pin = str(((run.get("arms") or {}).get(arm) or {}).get("pin") or "")
    if pin.startswith("run.fastbrowse_version"):
        return str(run.get("fastbrowse_version") or "unknown")
    return pin[: tracking.META_VALUE_MAX] or "unknown"


def arm_config(arm: str, row: dict[str, Any] | None) -> str:
    run = provenance_of(row)
    if not run:
        return "unknown"
    return short_hash(
        {
            "arm": arm,
            "pin": (run.get("arms") or {}).get(arm),
            "browser": (run.get("browsers") or {}).get(arm),
            **{key: (row or {}).get(key) for key in ROW_CONFIG},
            **{key: run.get(key) for key in RUN_CONFIG},
        }
    )


def judges_of(receipt: SemanticReceipt) -> set[str]:
    """The judge models a receipt's verdict came from: each fallback call's model, and Jev when it was asked."""
    models = {call.model for call in receipt.judgment.fallback_calls}
    if receipt.judgment.jev_call is not None:
        models.add("jev")
    return models


def run_metadata(runs: list[dict[str, Any]]) -> dict[str, object]:
    """Every field of the run records behind a row, under `run.`: one value when the runs agree, else each value.
    A resumed campaign retains its run records; the first is the one that started it."""
    out: dict[str, object] = {}
    for key in dict.fromkeys(key for run in runs for key in run):
        values = list({json.dumps(run.get(key), sort_keys=True): run.get(key) for run in runs}.values())
        out[f"run.{key}"] = values[0] if len(values) == 1 else values
    return out


def project(
    campaign: Campaign,
    suite: Suite,
    *,
    receipts: dict[str, SemanticReceipt] | None = None,
    rows: dict[str, dict[str, Any]] | None = None,
    extra: dict[str, object] | None = None,
    execution: dict[str, ValidatedExecution] | None = None,
) -> Any:
    """`rows` are the attempts' raw ledger rows (`ledger_rows`), which carry what configured and ran each attempt;
    `extra` is run metadata from outside the ledger, such as the CI run that tracked it."""
    known_rows = {} if rows is None else rows
    known_receipts = {} if receipts is None else receipts
    known_execution = {} if execution is None else execution
    if known_execution:
        coverage(
            {a.id: {**(rows or {})[a.id], "semantic_judge": known_receipts.get(a.id)} for a in campaign.attempts},
            known_execution,
        )
    semantic = bool(known_receipts)
    benchmark = BENCHMARKS[suite]
    if any(a.arm not in benchmark.arms for a in campaign.attempts):
        raise ValueError("campaign contains an agent outside this benchmark's declared comparison")
    if suite == Suite.BU_BENCH:
        raise ValueError("BU Bench requires weighted-score tracking, not a binary evidence projection")
    allowed = (
        {
            "core",
            "dev",
            "heldout",
            "stretch-dev",
            "stretch-heldout",
            "local",
            "mock",
            "mock-completion",
            "mock-safety",
            "sweep",
        }
        if suite == Suite.INTERNAL
        else {suite.value}
    )
    if any(a.suite not in allowed for a in campaign.attempts):
        raise ValueError("campaign contains tasks from another benchmark")
    dataset = f"browser-use/{suite.value}"
    identity = {
        **campaign.model_dump(),
        "semantic_receipts": {k: v.model_dump(mode="json") for k, v in known_receipts.items()},
        **(
            {"execution_receipts": {k: v.model_dump(mode="json") for k, v in known_execution.items()}}
            if known_execution
            else {}
        ),
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

    def agent_ms(attempt: Any) -> int | None:
        seconds = known_rows.get(attempt.id, {}).get("agent_seconds")
        return round(seconds * 1000) if isinstance(seconds, int | float) else None

    def detail(attempt: Any, index: int) -> Any:
        value = passed(attempt)
        verdict = Verdict.PASS if value is True else Verdict.FAIL if value is False else Verdict.UNGRADED
        row = known_rows.get(attempt.id)
        return tracking.RepeatDetail(
            repeat=index,
            verdict=verdict if semantic else Verdict.UNGRADED,
            score=float(value is True),
            cost_usd=attempt.dollars,
            agent_ms=agent_ms(attempt),
            config_hash=arm_config(attempt.arm, row),
            harness_version=arm_version(attempt.arm, provenance_of(row)),
            actual_models=tuple(m for m in ((row or {}).get("model"), (row or {}).get("text_model")) if m),
            wall_ms=round(attempt.seconds * 1000) if attempt.seconds is not None else None,
            invalid_reason=None
            if semantic and value is not None
            else "provider worker never started; no semantic judgment exists"
            if attempt.id in known_execution
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
    evidence: dict[tuple[str, str], tuple[dict[str, Any], ...]] = {}
    runs = []
    for (arm, commit), attempts in groups.items():
        recorded_models = sorted({a.model for a in attempts if a.model is not None})
        model = ", ".join(recorded_models) or None
        # The row is what Parallax pairs across runs, so it names the arm and model, not the commit under test; a
        # trace's version keeps the commit, which dashboards break a series down by.
        label = f"{arm}/{model or 'unknown'}"
        row = f"{label}/{commit or 'unknown'}"
        group_rows = [known_rows[a.id] for a in attempts if a.id in known_rows]
        group_runs = list(
            {json.dumps(run, sort_keys=True): run for r in group_rows if (run := provenance_of(r))}.values()
        )
        versions = sorted({arm_version(arm, provenance_of(r)) for r in group_rows}) or ["unknown"]
        configs = sorted({arm_config(arm, r) for r in group_rows}) or ["unknown"]
        judges = sorted({m for a in attempts if a.id in known_receipts for m in judges_of(known_receipts[a.id])})
        grading = sorted({known_receipts[a.id].judgment.policy for a in attempts if a.id in known_receipts})
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
                agent_ms=statistics.fmean(times) if (times := [t for a in selected if (t := agent_ms(a))]) else None,
                harness_version=", ".join(versions),
                config_hash=", ".join(configs),
                actual_models=sorted({a.model for a in task_attempts if a.model is not None}),
                repeats=[detail(a, i) for i, a in enumerate(task_attempts)],
            )
            evidence[row, iid] = tuple(
                {"attempt": a.id, "repeat": a.repeat, "retry": a.retry, **known_receipts[a.id].model_dump(mode="json")}
                for a in task_attempts
                if a.id in known_receipts
            )
            evidence[row, iid] += tuple(
                {
                    "attempt": a.id,
                    "repeat": a.repeat,
                    "retry": a.retry,
                    "execution_receipt": known_execution[a.id].model_dump(mode="json"),
                }
                for a in task_attempts
                if a.id in known_execution
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
            "semantic_receipts": sum(a.id in known_receipts for a in attempts),
            **({"non_execution_receipts": sum(a.id in known_execution for a in attempts)} if known_execution else {}),
            "campaign_scheduled_slots": campaign.scheduled_slots,
            "campaign_covered_slots": covered if campaign.scheduled_slots is not None else None,
            "campaign_missing_slots": max(0, campaign.scheduled_slots - covered)
            if campaign.scheduled_slots is not None
            else None,
            "coverage": "unknown" if campaign.scheduled_slots is None else "recorded",
            "git_sha": commit or "unknown",
            "git_dirty": sorted({a.git_dirty for a in attempts}),
            "physical_attempts": str(len(attempts)),
            "sources": [s.model_dump() for s in campaign.sources],
            **run_metadata(group_runs),
            **dict(extra or {}),
        }
        selected = [a for a in attempts if a.selected and a.started]
        graded = [a for a in selected if passed(a) is not None]
        group_starts = [datetime.fromisoformat(a.started_at) for a in attempts if a.started_at is not None]
        # Parallax's own run metadata, last so nothing above replaces it: `parallax show` lists a row by it, and
        # `parallax compare` pairs two rows only when their tasks, grading, judges, versions and configs agree.
        metadata |= {
            "run_id": digest,
            "row": label,
            "harness": arm,
            "model": model,
            "harness_versions": versions,
            "config_hashes": configs,
            "validity_control": False,
            "parallax_version": PARALLAX,
            "parallax_head": PARALLAX,
            "judge": judges,
            "judge_policy": short_hash({"grading_policy": grading or ["recorded-source-grade"], "judges": judges}),
            "repeats": max((a.repeat for a in selected), default=-1) + 1,
            "isolation": ", ".join(
                sorted({str(((run.get("browsers") or {}).get(arm) or {}).get("mode")) for run in group_runs})
            )
            or None,
            "selection_sha": short_hash(sorted(tasks))[:12],
            "tasks": len(tasks),
            "started_at": min(group_starts).isoformat() if group_starts else None,
        }
        # Langfuse holds a null as "", flattens an object into dotted keys and writes 30.0 as 30, so a value is sent
        # as the compact JSON text Parallax reads it back as (`sink.metadata_value`), and a null is left out.
        metadata = {key: sink.metadata_value(value) for key, value in metadata.items() if value is not None}
        scores = [
            tracking.Score("valid_trials", float(len(graded)), "NUMERIC"),
            tracking.Score("invalid_trials", float(len(selected) - len(graded)), "NUMERIC"),
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
                row=label,
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
    return LedgerRun(
        dataset=dataset,
        run_id=digest,
        created_at=min(datetime.fromisoformat(value) for value in starts),
        items=tuple(items.values()),
        runs=tuple(runs),
        receipts=evidence,
    )
