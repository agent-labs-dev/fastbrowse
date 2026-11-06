"""Track official source grades privately without inventing Parallax semantic verdicts."""

import argparse
import hashlib
import json
import statistics
from collections import defaultdict
from datetime import datetime
from pathlib import Path
from typing import Annotated, Any

from parallax.models import Verdict
from parallax.track import projection as tracking
from pydantic import BaseModel, ConfigDict, Field, StringConstraints, TypeAdapter

from fastbrowse_evals import langfuse_runs
from fastbrowse_evals.registry import BENCHMARKS, Suite

Identifier = Annotated[str, StringConstraints(pattern=r"^[A-Za-z0-9_-]{1,160}$")]
Digest = Annotated[str, StringConstraints(pattern=r"^[a-f0-9]{64}$")]


class SourceIdentity(BaseModel):
    model_config = ConfigDict(strict=True)

    suite: str
    upstream_commit: Annotated[str, StringConstraints(pattern=r"^[a-f0-9]{40}$")]
    dataset_sha256: Digest
    judge_model: Annotated[str, StringConstraints(pattern=r"^[A-Za-z0-9_./-]{1,160}$")]
    judge_reasoning: Annotated[str, StringConstraints(pattern=r"^[a-z]{1,20}$")]
    score_threshold: Annotated[int, Field(ge=1)]
    config_sha256: Digest


class SourceRow(BaseModel):
    model_config = ConfigDict(strict=True)

    task: Identifier
    repeat: Annotated[int, Field(ge=0)]
    upstream_score: Annotated[float, Field(ge=0, le=1, allow_inf_nan=False)] | None = None
    completed: bool = False
    final_capture_available: bool | None = None
    agent_cost_unknown: bool = True
    agent_dollars: Annotated[float, Field(ge=0, allow_inf_nan=False)] | None = None
    judge_usd: Annotated[float, Field(ge=0, allow_inf_nan=False)] | None = None


def project(folder: Path) -> tracking.TrackedRun:
    manifest = json.loads((folder / "manifest.json").read_text())
    suite = Suite(manifest["suite"])
    arm = manifest.get("arm", "fastbrowse")
    if suite not in (Suite.BU_BENCH, Suite.MIND2WEB) or manifest.get("published") is not False:
        raise ValueError("expected a private official benchmark")
    benchmark = BENCHMARKS[suite]
    identity = SourceIdentity.model_validate(manifest).model_dump()
    rows = [
        SourceRow.model_validate_json(line).model_dump()
        for line in (folder / "results.jsonl").read_text().splitlines()
        if line.strip()
    ]
    attempts = [
        SourceRow.model_validate_json(line).model_dump()
        for line in (folder / "attempts.jsonl").read_text().splitlines()
        if line.strip()
    ]
    # Legacy rows have no capture receipt; adding a null field would change their verified source digest.
    for row in [*rows, *attempts]:
        if row.get("final_capture_available") is None:
            row.pop("final_capture_available", None)
    keyed = {(r["task"], r["repeat"]): r for r in rows}
    if len(keyed) != len(rows):
        raise ValueError("duplicate official result")
    task_ids = TypeAdapter(list[Identifier]).validate_python(manifest["task_ids"], strict=True)
    if len(set(task_ids)) != len(task_ids):
        raise ValueError("duplicate official task identity")
    allowed = set(task_ids)
    agent_commit = TypeAdapter(Annotated[str, StringConstraints(pattern=r"^[a-f0-9]{40}$")] | None).validate_python(
        manifest["run"].get("git_sha"), strict=True
    )
    groups: dict[str, list[dict[str, Any]]] = defaultdict(list)
    seen = set()
    for attempt in attempts:
        key = attempt["task"], attempt["repeat"]
        if key in seen or key[0] not in allowed:
            raise ValueError("duplicate or out-of-scope official attempt")
        seen.add(key)
        row = keyed.get(key, attempt | {"upstream_score": None, "agent_cost_unknown": True})
        groups[key[0]].append(row)
    if keyed.keys() - seen:
        raise ValueError("official result has no physical attempt receipt")
    version_hash = hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()
    digest = hashlib.sha256(
        json.dumps({"manifest": manifest, "rows": rows, "attempts": attempts}, sort_keys=True).encode()
    ).hexdigest()
    dataset = f"browser-use/{suite.value}"
    items = []
    traces = []
    all_scores = []
    for task, repeats in sorted(groups.items()):
        task_id = f"{task}:{version_hash}"
        input_ = tracking.ItemInput(task_id=task_id, task_version=1)
        iid = tracking.item_id(dataset, task_id, 1)
        items.append(
            tracking.ItemSpec(
                iid, input_, {"benchmark": benchmark.model_dump(mode="json"), "grader_identity": identity}
            )
        )
        scores = [r["upstream_score"] for r in repeats if r.get("upstream_score") is not None]
        all_scores.extend(scores)
        metrics = [
            tracking.Score("graded_source_runs", float(len(scores)), "NUMERIC"),
            tracking.Score("ungraded_source_runs", float(len(repeats) - len(scores)), "NUMERIC"),
            tracking.Score("known_agent_charges_usd", sum(r["agent_dollars"] or 0 for r in repeats), "NUMERIC"),
            tracking.Score(
                "judge_committed_usd",
                sum(r["judge_usd"] or 0 for r in repeats),
                "NUMERIC",
                "settled charges plus outstanding conservative reservations",
            ),
        ]
        if any(r.get("final_capture_available") is not None for r in repeats):
            metrics.append(
                tracking.Score(
                    "runs_without_final_capture",
                    float(sum(r.get("final_capture_available") is False for r in repeats)),
                    "NUMERIC",
                )
            )
        if scores:
            metrics.append(tracking.Score(benchmark.metric, statistics.fmean(scores), "NUMERIC"))
        # Source reports do not enter Parallax's binary semantic comparison pipeline.
        output = tracking.ItemOutput(
            outcome=tracking.Outcome.INVALID,
            verdicts=[],
            failed_checks=[],
            cost_usd=None,
            agent_ms=None,
            harness_version=agent_commit or "unknown",
            config_hash=version_hash,
            actual_models=[],
            repeats=[
                tracking.RepeatDetail(
                    repeat=r["repeat"],
                    verdict=Verdict.UNGRADED,
                    score=r.get("upstream_score") or 0,
                    cost_usd=None,
                    agent_ms=None,
                    wall_ms=None,
                    invalid_reason="source grade only; Parallax semantic verdict not requested",
                    flags=[
                        f"source-score:{r.get('upstream_score')}",
                        f"completed:{r.get('completed', False)}",
                        f"agent-cost-unknown:{r['agent_cost_unknown']}",
                    ],
                )
                for r in repeats
            ],
        )
        traces.append(
            tracking.TraceSpec(
                iid,
                task_id,
                task_id,
                (
                    "family:browser-use",
                    f"benchmark:{suite.value}",
                    "origin:upstream",
                    "visibility:private",
                    "grade:source-only",
                ),
                arm,
                input_,
                output,
                tuple(metrics),
            )
        )
    run_scores = [
        tracking.Score("started_source_runs", float(len(attempts)), "NUMERIC"),
        tracking.Score("graded_source_runs", float(len(all_scores)), "NUMERIC"),
        tracking.Score("ungraded_source_runs", float(len(attempts) - len(all_scores)), "NUMERIC"),
        tracking.Score("known_agent_charges_usd", sum(r["agent_dollars"] or 0 for r in keyed.values()), "NUMERIC"),
        tracking.Score(
            "judge_committed_usd",
            sum(r["judge_usd"] or 0 for r in keyed.values()),
            "NUMERIC",
            "settled charges plus outstanding conservative reservations",
        ),
    ]
    if all_scores:
        run_scores.append(tracking.Score(benchmark.metric, statistics.fmean(all_scores), "NUMERIC"))
    timestamp = manifest.get("created_at") or manifest["run"].get("run_started")
    if not timestamp:
        raise ValueError("official run has no recorded timestamp")
    run = tracking.RunSpec(
        f"{suite.value}/{arm}/{digest[:16]}",
        arm,
        arm,
        None,
        "Imported official source scores; no rerun or Parallax semantic regrading.",
        {
            "benchmark": benchmark.model_dump(mode="json"),
            "grader_identity": identity,
            "published": False,
            "expected_tasks": len(allowed),
            "started_tasks": len(groups),
            "directly_comparable": False,
        },
        tuple(run_scores),
        tuple(traces),
    )
    return tracking.TrackedRun(dataset, digest, datetime.fromisoformat(timestamp), tuple(items), (run,))


def main(argv: list[str]) -> None:
    parser = argparse.ArgumentParser(prog="fastbrowse-evals browser track-official", description=__doc__)
    parser.add_argument("folder", type=Path)
    parser.add_argument("--push", action="store_true")
    parser.add_argument("--smoke", action="store_true", help="mark the experiment and traces as a smoke run")
    args = parser.parse_args(argv)
    run = project(args.folder)
    if args.smoke:
        run = langfuse_runs.marked_smoke(run)
    if not args.push:
        print(json.dumps({"dataset": run.dataset, "items": len(run.items), "published": False}))
        return
    langfuse_runs.push(run)
