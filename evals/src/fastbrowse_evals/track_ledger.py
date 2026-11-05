"""Send one campaign ledger to the private Langfuse project, every arm it holds.

`track` reads the published evidence bundle; a campaign in CI has only its ledger, and `publish-candidate` refuses a
group Langfuse does not hold. An unfinished ledger is tracked too: its experiment lists the attempts that ran."""

import argparse
import json
import os
from pathlib import Path

from fastbrowse_evals import langfuse_runs, publication, tracking
from fastbrowse_evals.langfuse_runs import marked_smoke
from fastbrowse_evals.registry import Suite


def ci_metadata(env: dict[str, str]) -> dict[str, object]:
    """The dispatch and CI run that tracked the ledger, which the ledger itself cannot know."""
    out: dict[str, object] = {}
    if campaign := env.get("CAMPAIGN_ID"):
        out["campaign_id"] = campaign
    if run_id := env.get("GITHUB_RUN_ID"):
        server, repository = env.get("GITHUB_SERVER_URL", "https://github.com"), env.get("GITHUB_REPOSITORY", "")
        out["ci.run_url"] = f"{server}/{repository}/actions/runs/{run_id}/attempts/{env.get('GITHUB_RUN_ATTEMPT', '1')}"
        out["ci.workflow_ref"] = env.get("GITHUB_WORKFLOW_REF")
        out["ci.workflow_sha"] = env.get("GITHUB_SHA")
        out["ci.job"] = env.get("GITHUB_JOB")
    return {key: value for key, value in out.items() if value}


def main(argv: list[str]) -> None:
    parser = argparse.ArgumentParser(prog="fastbrowse-evals browser track-ledger", description=__doc__)
    parser.add_argument("results", type=Path, help="the campaign's results.jsonl, beside its attempts and budget")
    parser.add_argument("--push", action="store_true")
    parser.add_argument("--smoke", action="store_true", help="mark the experiments and traces as a smoke run")
    args = parser.parse_args(argv)
    campaign, receipts, rows = publication.internal_campaign(args.results)
    tracked = tracking.project(
        campaign, Suite.INTERNAL, receipts=receipts, rows=rows, extra=ci_metadata(dict(os.environ))
    )
    if args.smoke:
        tracked = marked_smoke(tracked)
    summary = {"dataset": tracked.dataset, "items": len(tracked.items), "experiments": [r.name for r in tracked.runs]}
    print(json.dumps(summary))
    if not args.push:
        return
    langfuse_runs.push(tracked)
