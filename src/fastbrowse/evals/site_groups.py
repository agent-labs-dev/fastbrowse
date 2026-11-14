"""Write the website's benchmark group files from one campaign's candidate.

The site (agent-labs-dev/fastbrowse-site) shows four groups, one file each under `data/benchmarks/`, parsed by its
`src/lib/benchmark-groups.ts`. A measured group file holds no figure: it names the candidate group, campaign, build
and candidate digest, and the site computes every figure from the candidate's per-attempt rows, which this writes
beside it as `data/benchmark-<campaign>.json`, byte for byte. A group the candidate does not hold keeps its file.

Every site system has one of three states in a measured group file: `systems` maps it to the arm whose rows it shows,
`cost_not_reported` lists the shown systems whose complete cost is unavailable (`"dollars": null`), so the site shows
their pass rate and seconds without a cost figure. `not_measured` lists systems with no rows, shown as "Not measured".
`setups` names what each shown system ran, its version and the models its rows reported, where the candidate records
them.
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

from fastbrowse.evals.benchmarks import Candidate, Group, GroupId, group_arms

SiteGroup = Literal["live-suite", "live-suite-fastbrowse", "online-mind2web", "bu-bench"]

SYSTEMS = {
    "fastbrowse": "fastbrowse",
    "browser-use": "browser-use",
    "browser-use-ultrafast": "browser-use-ultrafast",
    "jev-ultrafast": "jev-ultrafast",  # figures approved before schema 7 carry it
    "browser-harness": "browser-harness",
    "browser-use-oss": "browser-use-oss",
}
"""The site system each candidate arm is shown as (`SYSTEM_ARMS` in the site's benchmark-groups.ts)."""
RETIRED = frozenset({"jev-ultrafast"})
"""Systems no current group measures: shown where approved figures carry them, never listed as not measured."""

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


def setups(group: Group, arms: tuple[str, ...]) -> dict[str, dict[str, object]]:
    """What each shown system ran, as the candidate records it: every source of a measured arm, whose models are all
    shown, or a reference arm's own record. Sources of one arm that pinned different versions cannot share one label,
    so they are refused. A system with neither version nor models is left out, as a candidate from before them
    records none."""
    recorded: dict[str, dict[str, object]] = {}
    for arm in arms:
        held = [s for s in group.sources if s.arm == arm] or [r for r in group.references if r.arm == arm][:1]
        versions = list(dict.fromkeys(h.version for h in held if h.version))
        if len(versions) > 1:
            raise ValueError(f"{group.id} pools {arm} runs pinned to different versions: {', '.join(versions)}")
        models = list(dict.fromkeys(m for h in held for m in h.models))
        if versions or models:
            recorded[SYSTEMS[arm]] = {"version": versions[0] if versions else None, "models": models}
    return recorded


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
                "not_measured": [system for arm, system in SYSTEMS.items() if arm not in arms and arm not in RETIRED],
                **({"setups": ran} if (ran := setups(source, arms)) else {}),
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
