"""Write the website's benchmark group files from one campaign's candidate.

The site (agent-labs-dev/fastbrowse-site) shows four groups, one file each under `data/benchmarks/`, parsed by its
`src/lib/benchmark-groups.ts`. A measured group file holds no figure: it names the candidate group, campaign, build
and candidate digest, and the site computes every figure from the candidate's per-attempt rows, which this writes
beside it as `data/benchmark-<campaign>.json`, byte for byte. A group the candidate does not hold keeps its file.

Every site system has one of three states in a measured group file: `systems` maps it to the arm whose rows it shows,
`cost_not_reported` lists the shown systems whose rows carry no cost (`"dollars": null`), so the site shows their pass
rate and seconds with "cost not reported", and `not_measured` lists the systems with no rows, shown as "Not measured".
A vendor figure the site already shows for the group is kept only as per-task rows over exactly the campaign's tasks.
A `reference` block is a vendor figure on another task set, which the site labels and never compares; it stays as it is.
"""

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Literal

from pydantic import BaseModel, ConfigDict, Field

from fastbrowse.evals.benchmarks import Candidate, GroupId, group_arms

SiteGroup = Literal["live-suite", "live-suite-fastbrowse", "online-mind2web", "bu-bench"]

SYSTEMS = {
    "fastbrowse": "fastbrowse",
    "browser-use": "browser-use",
    "jev-ultrafast": "browser-use-ultrafast",
    "browser-harness": "browser-harness",
}
"""The site system each candidate arm is shown as (`SYSTEM_ARMS` in the site's benchmark-groups.ts)."""

SOURCES: dict[SiteGroup, tuple[GroupId, ...]] = {
    "live-suite": ("internal-hosted",),
    "live-suite-fastbrowse": ("internal-fastbrowse",),
    "online-mind2web": ("online-mind2web",),
    "bu-bench": ("bu-bench",),
}
SUITES: dict[SiteGroup, str] = {
    "live-suite": "fastbrowse live task suite",
    "live-suite-fastbrowse": "fastbrowse live task suite",
    "online-mind2web": "Online-Mind2Web",
    "bu-bench": "BU Bench V2",
}
ROWS_NAME = re.compile(r"^[a-zA-Z0-9_.-]+$")


class GroupFacts(BaseModel):
    """An official suite's own date and upstream judge, which differ from the campaign's."""

    model_config = ConfigDict(extra="forbid", strict=True)
    run_on: str = Field(pattern=r"^\d{4}-\d{2}-\d{2}$")
    judge: str = Field(min_length=1)


class RunFacts(BaseModel):
    """What the campaign run records about itself and the candidate cannot: when it ran, on what, judged by whom."""

    model_config = ConfigDict(extra="forbid", strict=True)
    run_on: str = Field(pattern=r"^\d{4}-\d{2}-\d{2}$")
    harness: str = Field(min_length=1)
    judge: str = Field(min_length=1)
    groups: dict[SiteGroup, GroupFacts] = {}

    def shown(self, group: SiteGroup) -> dict[str, str]:
        own = self.groups.get(group)
        return {
            "run_on": own.run_on if own else self.run_on,
            "harness": self.harness,
            "judge": own.judge if own else self.judge,
        }


def group_files(content: bytes, facts: RunFacts, current: dict[str, dict]) -> dict[str, bytes]:
    """Every site file this candidate changes, by path under the site root."""
    candidate = Candidate.model_validate_json(content)
    if not ROWS_NAME.fullmatch(candidate.campaign_id):
        raise ValueError(f"campaign id {candidate.campaign_id!r} cannot name a site data file")
    rows = f"data/benchmark-{candidate.campaign_id}.json"
    digest = hashlib.sha256(content).hexdigest()
    held = {group.id: group for group in candidate.groups}
    files = {rows: content}
    for site_group, sources in SOURCES.items():
        source = next((held[g] for g in sources if g in held), None)
        if source is None:
            # The site reads one candidate, so a file still bound to an earlier campaign would stop its build.
            if current.get(site_group, {}).get("measured") is not None:
                raise ValueError(f"{site_group} is bound to an earlier campaign; this candidate has no figures for it")
            continue
        arms = source.measured_arms(group_arms(source.id, candidate.schema_version))
        if unmapped := [arm for arm in arms if arm not in SYSTEMS]:
            raise ValueError(f"{source.id} has arms the site shows as no system: {', '.join(unmapped)}")
        unpriced = {r.arm for r in source.rows if r.dollars is None}
        # A vendor's published per-task rows on exactly this task set stay beside ours; any other published figure
        # (a run summary, another task set, or a system now measured) is not on this task set and is dropped.
        shown = {SYSTEMS[arm] for arm in arms}
        held_figures = current.get(site_group, {})
        published = [
            p
            for p in held_figures.get("published", [])
            if p["system"] not in shown
            and p.get("rows")
            and sorted(r["task"] for r in p["rows"]) == sorted(source.task_ids)
        ]
        group = {
            "schema_version": 1,
            "id": site_group,
            "suite": SUITES[site_group],
            "metric": source.metric,
            "tasks": None,
            "repeats": None,
            "measured": {
                "group": source.id,
                "campaign_id": candidate.campaign_id,
                "agent_sha": candidate.agent_sha,
                "candidate_sha256": digest,
                "rows": rows,
                **facts.shown(site_group),
                "systems": {SYSTEMS[arm]: arm for arm in arms},
                "cost_not_reported": [SYSTEMS[arm] for arm in arms if arm in unpriced],
                "not_measured": [system for arm, system in SYSTEMS.items() if arm not in arms],
            },
            "published": published,
            **({"reference": held_figures["reference"]} if "reference" in held_figures else {}),
        }
        files[f"data/benchmarks/{site_group}.json"] = (json.dumps(group, indent=2) + "\n").encode()
    return files


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("candidate", type=Path, help="the campaign's benchmark-candidate.json")
    parser.add_argument("facts", type=Path, help="the campaign's run-facts.json")
    parser.add_argument("site", type=Path, help="a fastbrowse-site checkout to write into")
    args = parser.parse_args(argv)
    paths = {group: args.site / "data" / "benchmarks" / f"{group}.json" for group in SOURCES}
    current: dict[str, dict] = {group: json.loads(path.read_text()) for group, path in paths.items() if path.exists()}
    files = group_files(args.candidate.read_bytes(), RunFacts.model_validate_json(args.facts.read_bytes()), current)
    for path, content in files.items():
        (args.site / path).write_bytes(content)
        print(path)


if __name__ == "__main__":
    main()
