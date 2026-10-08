"""Validate sanitized benchmark figures before owner approval and site publication."""

import argparse
import hashlib
import json
import statistics
import subprocess
import tempfile
from pathlib import Path
from typing import Annotated, Literal, Self

from pydantic import BaseModel, ConfigDict, Field, model_validator

from fastbrowse.evals.publication import REGRESSION_FACTOR
from fastbrowse.models import Status

Identifier = Annotated[str, Field(pattern=r"^[a-zA-Z0-9_.:+-]{1,160}$")]
Sha = Annotated[str, Field(pattern=r"^[0-9a-f]{40}$")]
Digest = Annotated[str, Field(pattern=r"^[0-9a-f]{64}$")]
Amount = Annotated[float, Field(ge=0, allow_inf_nan=False)]


class PublicModel(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True)


class Row(PublicModel):
    task: Identifier
    repeat: Annotated[int, Field(ge=0)]
    arm: Literal["fastbrowse", "browser-use", "jev-ultrafast"]
    status: Identifier
    completed: bool
    score: Annotated[float, Field(ge=0, le=1, allow_inf_nan=False)]
    seconds: Amount
    dollars: Amount
    physical_runs: Annotated[int, Field(gt=0)]

    @model_validator(mode="after")
    def completion(self) -> Self:
        if self.arm == "fastbrowse":
            status = Status(self.status)
            if self.completed != (status is Status.COMPLETE):
                raise ValueError("Fastbrowse completion must agree with its run status")
        return self


class Group(PublicModel):
    id: Literal["internal-hosted", "internal-ultrafast", "bu-bench", "online-mind2web"]
    metric: Literal["task-success", "weighted-rubric", "full-task-success"]
    source_sha256: Digest
    task_ids: list[Identifier]
    repeats: Annotated[int, Field(gt=0)]
    limits: Annotated[
        list[Annotated[str, Field(pattern=r"^[A-Za-z0-9 ,.;:()'%+_=-]{1,240}$")]], Field(min_length=1, max_length=12)
    ]
    rows: list[Row]

    @model_validator(mode="after")
    def coverage(self) -> Self:
        spec = {
            "internal-hosted": (47, 3, "task-success", {"fastbrowse", "browser-use"}),
            "internal-ultrafast": (6, 3, "task-success", {"fastbrowse", "jev-ultrafast"}),
            "bu-bench": (200, 1, "weighted-rubric", {"fastbrowse"}),
            "online-mind2web": (300, 1, "full-task-success", {"fastbrowse"}),
        }
        count, repeats, metric, arms = spec[self.id]
        if len(self.task_ids) != count or len(set(self.task_ids)) != count:
            raise ValueError("task coverage differs from the approved full protocol")
        if self.repeats != repeats or self.metric != metric or not self.limits:
            raise ValueError("repeats, metric and recorded limits are required")
        expected = {(task, repeat, arm) for task in self.task_ids for repeat in range(repeats) for arm in arms}
        actual = [(r.task, r.repeat, r.arm) for r in self.rows]
        if len(actual) != len(set(actual)) or set(actual) != expected:
            raise ValueError("every scheduled slot must have exactly one measured row")
        if self.metric != "weighted-rubric" and any(r.score not in {0, 1} for r in self.rows):
            raise ValueError("binary source grades cannot contain partial scores")
        return self


class Fixture(PublicModel):
    head_sha: Sha
    run_id: Annotated[int, Field(gt=0)]
    passed: Literal[108]
    total: Literal[108]


class Candidate(PublicModel):
    schema_version: Literal[1, 2]
    campaign_id: Identifier
    agent_sha: Sha
    runner_sha: Sha
    catalog_sha256: Digest
    fixture: Fixture
    langfuse_verified: Literal[True]
    groups: list[Group]

    @model_validator(mode="after")
    def suites(self) -> Self:
        expected = {"internal-hosted", "internal-ultrafast", "bu-bench", "online-mind2web"}
        ids = {g.id for g in self.groups}
        if not self.groups or len(ids) != len(self.groups):
            raise ValueError("at least one complete, unique benchmark group is required")
        if self.schema_version == 1 and (len(self.groups) != 4 or ids != expected):
            raise ValueError("all four full benchmark groups are required")
        if self.agent_sha != self.fixture.head_sha:
            raise ValueError("fixtures must pass on the measured agent build")
        return self


class Approval(PublicModel):
    candidate_sha256: Digest
    workflow_run: Annotated[int, Field(gt=0)]
    head_sha: Sha
    approved_by: Literal["cjber"]
    approved_at: Annotated[str, Field(pattern=r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z$")]


def read(path: Path) -> Candidate:
    return Candidate.model_validate_json(path.read_bytes())


def check_catalog(candidate: Candidate, path: Path) -> None:
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() != candidate.catalog_sha256:
        raise ValueError("catalog must match the current pinned task source")
    catalog = json.loads(raw)
    tasks = [task for suite in catalog["suites"].values() for task in suite]
    for group, other in (("internal-hosted", "browser-use"), ("internal-ultrafast", "jev-ultrafast")):
        required = {task["id"] for task in tasks if {"fastbrowse", other} <= set(task["arms"])}
        found = next((g for g in candidate.groups if g.id == group), None)
        if found is None:
            continue
        if set(found.task_ids) != required or found.source_sha256 != candidate.catalog_sha256:
            raise ValueError("internal rows must cover the exact matched catalog")


def regressions(candidate: Candidate, baseline: Candidate) -> None:
    previous = {g.id: g for g in baseline.groups}
    if not previous.keys() <= {g.id for g in candidate.groups}:
        raise ValueError("published benchmark groups cannot disappear")
    for group in candidate.groups:
        old = previous.get(group.id)
        if old is None:
            continue
        if (group.source_sha256, sorted(group.task_ids), group.limits) != (
            old.source_sha256,
            sorted(old.task_ids),
            old.limits,
        ):
            raise ValueError(f"protocol changed: {group.id}; matched baseline measurement is required")
        for task in group.task_ids:
            now = [r for r in group.rows if r.arm == "fastbrowse" and r.task == task]
            before = [r for r in old.rows if r.arm == "fastbrowse" and r.task == task]
            if sum(r.score for r in now) / len(now) < sum(r.score for r in before) / len(before):
                raise ValueError(f"source-score regression: {group.id}/{task}")
            if sum(r.completed for r in now) < sum(r.completed for r in before):
                raise ValueError(f"completion regression: {group.id}/{task}")
            for field, floor in (("seconds", 1.0), ("dollars", 0.001)):
                a = statistics.median(getattr(r, field) for r in now)
                b = statistics.median(getattr(r, field) for r in before)
                if a > b * REGRESSION_FACTOR and a - b > floor:
                    raise ValueError(f"{field} regression: {group.id}/{task}")


def github_run(run_id: int) -> dict:
    result = subprocess.run(
        ["gh", "api", f"repos/agent-labs-dev/fastbrowse/actions/runs/{run_id}"],
        capture_output=True,
        text=True,
        timeout=30,
        check=True,
    )
    return json.loads(result.stdout)


def check_fixture(candidate: Candidate) -> None:
    run = github_run(candidate.fixture.run_id)
    if (
        run["head_sha"] != candidate.agent_sha
        or run["conclusion"] != "success"
        or run["status"] != "completed"
        or run["path"] != ".github/workflows/evals.yml"
        or run["repository"]["full_name"] != "agent-labs-dev/fastbrowse"
        or run["head_branch"] != "main"
        or run["event"] != "workflow_dispatch"
    ):
        raise ValueError("a successful fixture workflow receipt on the measured build is required")
    from fastbrowse.evals.runner import _tasks

    tasks, _ = _tasks([], ["local", "mock"])
    expected = {(task.id, repeat) for _, task in tasks for repeat in range(3)}
    with tempfile.TemporaryDirectory(prefix="benchmark-fixture-") as directory:
        subprocess.run(
            [
                "gh",
                "run",
                "download",
                str(candidate.fixture.run_id),
                "--repo",
                "agent-labs-dev/fastbrowse",
                "--name",
                "eval-rows",
                "--dir",
                directory,
            ],
            capture_output=True,
            timeout=60,
            check=True,
        )
        paths = list(Path(directory).rglob("nightly.jsonl"))
        if len(paths) != 1:
            raise ValueError("the fixture artifact must contain one complete ledger")
        rows = [json.loads(line) for line in paths[0].read_text().splitlines() if line.strip()]
    slots = [(row["task"], row["repeat"]) for row in rows]
    if len(slots) != len(set(slots)) or set(slots) != expected or len(rows) != 108:
        raise ValueError("fixture artifact coverage must be 36 tasks across three repeats")
    if any(
        row["passed"] is not True
        or row["run"]["git_sha"] != candidate.agent_sha
        or row["run"]["git_dirty"] is not False
        for row in rows
    ):
        raise ValueError("all fixture rows must pass on the clean measured build")


def git_file(ref: str, path: str) -> bytes | None:
    done = subprocess.run(["git", "show", f"{ref}:{path}"], capture_output=True, timeout=30, check=False)
    if done.returncode:
        return None
    return done.stdout


def check_approval(content: bytes, approval: Approval) -> None:
    digest = hashlib.sha256(content).hexdigest()
    reviewed = git_file(approval.head_sha, "docs/results/benchmark-candidate.json")
    if digest != approval.candidate_sha256 or reviewed is None or hashlib.sha256(reviewed).hexdigest() != digest:
        raise ValueError("approval must bind the published and reviewed candidate bytes")
    run = github_run(approval.workflow_run)
    if (
        run["head_sha"] != approval.head_sha
        or run["head_branch"] != "main"
        or run["actor"]["login"] != approval.approved_by
        or run["triggering_actor"]["login"] != approval.approved_by
        or run["event"] != "workflow_dispatch"
        or run["path"] != ".github/workflows/publish-evals.yml"
        or run["status"] != "completed"
        or run["conclusion"] != "success"
    ):
        raise ValueError("a successful owner dispatch on main is required")


def check_diff(base: str, catalog: Path) -> None:
    published = Path("docs/results/benchmarks.json")
    approval_path = Path("docs/results/benchmark-approval.json")
    old = git_file(base, str(published))
    old_approval = git_file(base, str(approval_path))
    baseline = None
    if (old is None) != (old_approval is None):
        raise ValueError("trusted published figures and approval must exist together")
    if old is not None and old_approval is not None:
        receipt = Approval.model_validate_json(old_approval)
        if hashlib.sha256(old).hexdigest() != receipt.candidate_sha256:
            raise ValueError("the trusted baseline has an invalid approval digest")
        baseline = Candidate.model_validate_json(old)
    if baseline is not None and not published.exists():
        raise ValueError("an approved benchmark baseline cannot disappear")
    for path in (Path("docs/results/benchmark-candidate.json"), published):
        if not path.exists():
            continue
        candidate = read(path)
        if git_file(base, str(path)) == path.read_bytes():
            continue
        check_catalog(candidate, catalog)
        check_fixture(candidate)
        if baseline is not None:
            regressions(candidate, baseline)
    if published.exists():
        if not approval_path.exists():
            raise ValueError("published figures require an owner approval receipt")
        check_approval(published.read_bytes(), Approval.model_validate_json(approval_path.read_bytes()))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, default=Path("docs/results/benchmarks.json"))
    parser.add_argument("--catalog", type=Path, default=Path("src/fastbrowse/evals/benchmark-catalog.json"))
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--verify-fixture", action="store_true")
    parser.add_argument("--check-diff")
    parser.add_argument("--bootstrap", action="store_true", help="owner approval for the first publication")
    args = parser.parse_args()
    if args.check_diff:
        check_diff(args.check_diff, args.catalog)
        print("Benchmark publication coverage and approval checked")
        return
    candidate = read(args.candidate)
    check_catalog(candidate, args.catalog)
    if args.baseline:
        if args.baseline.exists():
            regressions(candidate, read(args.baseline))
        elif not args.bootstrap:
            parser.error("the first publication requires explicit --bootstrap approval")
    if args.verify_fixture:
        check_fixture(candidate)
    print(f"Validated {candidate.campaign_id}, selected full groups covered")


if __name__ == "__main__":
    main()
