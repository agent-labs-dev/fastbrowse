"""Send the public evidence projection through the optional, privately installed Parallax tracker.

Install Parallax with its track extra in a maintainer environment. No agent or grader runs here.
The default is a dry run; --push additionally requires project-scoped Langfuse credentials and
PARALLAX_LANGFUSE_PROJECT=fastbrowse-evals. The tracker verifies those keys before writing.
"""

import argparse
import hashlib
import importlib
import json
import statistics
from collections import defaultdict
from datetime import datetime
from pathlib import Path
from typing import Any

from fastbrowse.evals.evidence import Campaign, Evidence


def project(campaign: Campaign) -> Any:
    tracking = importlib.import_module("parallax.track.projection")
    models = importlib.import_module("parallax.models")
    identity = campaign.model_dump(exclude={"sources"})
    digest = hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()
    groups: dict[tuple[str, str | None], list[Any]] = defaultdict(list)
    for attempt in campaign.attempts:
        groups[attempt.arm, attempt.git_sha].append(attempt)
    covered = len({(a.suite, a.task, a.arm, a.repeat) for a in campaign.attempts if a.selected and a.started})
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
            tasks[task_id].append(attempt)
        traces = []
        for task_id, task_attempts in tasks.items():
            item_input = tracking.ItemInput(task_id=task_id, task_version=1)
            iid = tracking.item_id("fastbrowse-evals", task_id, 1)
            items[iid] = tracking.ItemSpec(
                id=iid,
                task_id=task_id,
                task_version=1,
                input=item_input,
                metadata={"source": "fastbrowse", "task_version": task_attempts[0].task_version},
            )
            selected = [a for a in task_attempts if a.selected]
            verdicts = [float(a.passed) for a in selected if a.passed is not None]
            unknown = sum(a.unknown_cost for a in task_attempts)
            known_cost = sum(a.dollars for a in task_attempts if a.dollars is not None)
            cost_slots = [a for a in selected if a.started]
            cost = known_cost / len(cost_slots) if cost_slots and not unknown else None
            scores = [
                tracking.Score("outcome", tracking.outcome_of(verdicts).value, "CATEGORICAL"),
                tracking.Score("known_attempt_cost_usd", float(known_cost), "NUMERIC"),
                tracking.Score("unknown_cost_attempts", float(unknown), "NUMERIC"),
            ]
            if verdicts:
                scores.append(tracking.Score("pass_rate", statistics.fmean(verdicts), "NUMERIC"))
            started_slots = [a for a in selected if a.started]
            if started_slots:
                scores.append(
                    tracking.Score(
                        "completion_rate", statistics.fmean(float(a.completed) for a in started_slots), "NUMERIC"
                    )
                )
            if cost is not None:
                scores.append(tracking.Score("cost_usd", float(cost), "NUMERIC", "all attempts per selected slot"))
            output = tracking.ItemOutput(
                outcome=tracking.outcome_of(verdicts),
                verdicts=verdicts,
                failed_checks=["source-grade"] if any(a.passed is False for a in selected) else [],
                cost_usd=cost,
                agent_ms=None,
                harness_version=commit or "unknown",
                config_hash=digest,
                actual_models=sorted({a.model for a in task_attempts if a.model is not None}),
                repeats=[
                    tracking.RepeatDetail(
                        repeat=i,
                        verdict=models.Verdict.PASS
                        if a.passed is True
                        else models.Verdict.FAIL
                        if a.passed is False
                        else models.Verdict.UNGRADED,
                        # Parallax requires a scalar here, but an UNGRADED verdict never enters grade scores.
                        score=float(a.passed is True),
                        cost_usd=a.dollars,
                        agent_ms=None,
                        wall_ms=round(a.seconds * 1000) if a.seconds is not None else None,
                        invalid_reason=None,
                        flags=[
                            f"repeat:{a.repeat}",
                            f"retry:{a.retry}",
                            f"selected:{a.selected}",
                            f"started:{a.started}",
                            f"completed:{a.completed}",
                            f"status:{a.status}",
                            *(["score:not-applicable"] if a.passed is None else []),
                        ],
                    )
                    for i, a in enumerate(task_attempts)
                ],
            )
            traces.append(
                tracking.TraceSpec(
                    item_id=iid,
                    name=task_id,
                    task_id=task_id,
                    tags=(f"campaign:{campaign.id}", f"harness:{arm}", f"kind:{campaign.kind}"),
                    version=row,
                    input=item_input,
                    output=output,
                    scores=tuple(scores),
                )
            )
        metadata = {
            "campaign": campaign.id,
            "kind": campaign.kind,
            "projection_sha256": digest,
            "grading": "recorded-source-grade",
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
        selected = [a for a in attempts if a.selected]
        graded = [a for a in selected if a.passed is not None]
        scores = [
            tracking.Score("source_grade_passes", float(sum(a.passed is True for a in graded)), "NUMERIC"),
            tracking.Score("graded_selected_rows", float(len(graded)), "NUMERIC"),
            tracking.Score("ungraded_selected_rows", float(len(selected) - len(graded)), "NUMERIC"),
            tracking.Score("completed_selected_rows", float(sum(a.completed for a in selected)), "NUMERIC"),
            tracking.Score(
                "passed_and_completed_selected_rows",
                float(sum(a.completed and a.passed is True for a in selected)),
                "NUMERIC",
            ),
            tracking.Score(
                "known_attempt_cost_usd", float(sum(a.dollars for a in attempts if a.dollars is not None)), "NUMERIC"
            ),
            tracking.Score("unknown_cost_attempts", float(sum(a.unknown_cost for a in attempts)), "NUMERIC"),
        ]
        if graded:
            scores.append(
                tracking.Score("source_grade_rate", statistics.fmean(float(a.passed) for a in graded), "NUMERIC")
            )
        runs.append(
            tracking.RunSpec(
                name=f"{campaign.id}/{row}/{digest[:16]}",
                row=row,
                harness=arm,
                model=model,
                description="Recorded source grades and completion, no rerun or regrading.",
                metadata=metadata,
                scores=tuple(scores),
                traces=tuple(traces),
            )
        )
    starts = [a.started_at for a in campaign.attempts if a.started_at is not None]
    if not starts:
        raise ValueError(f"{campaign.id}: no recorded start time; cannot invent a historical timestamp")
    return tracking.TrackedRun(
        dataset="fastbrowse-evals",
        run_id=digest,
        created_at=min(datetime.fromisoformat(value) for value in starts),
        items=tuple(items.values()),
        runs=tuple(runs),
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("evidence", type=Path)
    selection = parser.add_mutually_exclusive_group(required=True)
    selection.add_argument("--campaign", action="append")
    selection.add_argument("--all", action="store_true")
    parser.add_argument("--push", action="store_true")
    args = parser.parse_args()
    evidence = Evidence.model_validate_json(args.evidence.read_text())
    indexed = {c.id: c for c in evidence.campaigns}
    names = list(indexed) if args.all else list(dict.fromkeys(args.campaign))
    missing = set(names) - indexed.keys()
    if missing:
        parser.error(f"unknown campaigns: {sorted(missing)}")
    tracked = []
    skipped = []
    for name in names:
        if not any(a.started_at is not None for a in indexed[name].attempts):
            skipped.append(name)
            continue
        tracked.append(project(indexed[name]))
    if skipped:
        print(json.dumps({"skipped_no_recorded_timestamp": skipped}))
    if not tracked:
        raise SystemExit("no campaigns have a recorded start time")
    if not args.push:
        for run in tracked:
            print(
                json.dumps(
                    {
                        "dataset": run.dataset,
                        "run_id": run.run_id,
                        "items": len(run.items),
                        "experiments": [r.name for r in run.runs],
                    }
                )
            )
        return
    sink = importlib.import_module("parallax.track.sink")
    client, project_info = sink.connect(write=True)
    if project_info.name != "fastbrowse-evals":
        raise SystemExit("refusing to write outside the dedicated fastbrowse-evals project")
    try:
        for run in tracked:
            report = sink.push(client, project_info, run)
            print(json.dumps({"dataset_url": report.dataset_url, "pushed": report.pushed, "skipped": report.skipped}))
    finally:
        client.shutdown()


if __name__ == "__main__":
    main()
