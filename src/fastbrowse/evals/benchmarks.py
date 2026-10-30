"""Validate sanitized benchmark figures before owner approval and site publication."""

import argparse
import hashlib
import json
import re
import subprocess
import tempfile
from collections.abc import Collection
from pathlib import Path
from typing import Annotated, Literal, NamedTuple, Self

from pydantic import BaseModel, ConfigDict, Field, ValidationError, model_validator

from fastbrowse.evals.catalog import CatalogTask, eligible
from fastbrowse.evals.publication import moves
from fastbrowse.models import Status

Identifier = Annotated[str, Field(pattern=r"^[a-zA-Z0-9_.:+-]{1,160}$")]
Sha = Annotated[str, Field(pattern=r"^[0-9a-f]{40}$")]
Digest = Annotated[str, Field(pattern=r"^[0-9a-f]{64}$")]
Amount = Annotated[float, Field(ge=0, allow_inf_nan=False)]
Limit = Annotated[str, Field(pattern=r"^[A-Za-z0-9 ,.;:()'%+_=-]{1,240}$")]
GroupId = Literal[
    "internal-fastbrowse",
    "internal-hosted",
    "internal-ultrafast",
    "internal-backend",
    "bu-bench",
    "online-mind2web",
]
Arm = Literal["fastbrowse", "fastbrowse-pool", "browser-use", "jev-ultrafast", "browser-harness"]
FASTBROWSE_ARMS: frozenset[Arm] = frozenset({"fastbrowse", "fastbrowse-pool"})
RESULTS = Path("docs/results")
PUBLISHED = RESULTS / "benchmarks.json"
APPROVAL = RESULTS / "benchmark-approval.json"
STAGED = RESULTS / "benchmark-candidate.json"


class PublicModel(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True)


class Row(PublicModel):
    task: Identifier
    repeat: Annotated[int, Field(ge=0)]
    arm: Arm
    status: Identifier
    completed: bool
    score: Annotated[float, Field(ge=0, le=1, allow_inf_nan=False)]
    seconds: Amount
    dollars: Amount | None
    """None when the arm does not report its cost, so its rows carry a pass rate and seconds and the site shows
    "cost not reported" in place of a cost."""
    physical_runs: Annotated[int, Field(gt=0)]
    # The efficiency figures a comparison is normalized onto, so a slow page or a provider's margin cannot move it.
    # A build from before they existed omits them and they read as zero; the site then shows them as not recorded
    # rather than falling back to the measured wall clock or billed charge they replace.
    llm_calls: Annotated[int, Field(ge=0)] = 0
    jev_calls: Annotated[int, Field(ge=0)] = 0
    tokens_per_second: Amount = 0.0
    input_tokens: Annotated[int, Field(ge=0)] = 0
    output_tokens: Annotated[int, Field(ge=0)] = 0

    @model_validator(mode="after")
    def completion(self) -> Self:
        # fastbrowse-pool is fastbrowse itself on another browser, so its completion is its own run status too.
        if self.arm in FASTBROWSE_ARMS:
            status = Status(self.status)
            if self.completed != (status is Status.COMPLETE):
                raise ValueError("Fastbrowse completion must agree with its run status")
            if self.dollars is None:
                raise ValueError("Fastbrowse rows report their cost")
        return self


class Coverage(NamedTuple):
    counts: frozenset[int]
    repeats: int | None
    """The approved repeat count, or None where any positive count is approved: an official suite's figure per task
    is the mean of its repeats' upstream scores, each kept as the upstream defines it."""
    metric: Literal["task-success", "weighted-rubric", "full-task-success"]
    arms: tuple[Arm, ...]
    """The arms the group may measure, the Fastbrowse arm first: it is the one whose figures the regression check
    guards."""
    required: tuple[Arm, ...] | None = None
    """The arms every group must measure, or None for all of them. Any other arm in `arms` is measured when it ran:
    a vendor arm that could not run has no rows, and the site shows it as not
    measured."""
    partial: tuple[Arm, ...] = ()
    """The arms that run only the tasks of the group's task set the catalog lists them for. The task set is the
    tasks every other arm in `arms` runs."""


# Each count the approved protocol has had. Retiring stretch-wizard-correction, whose site left DNS, took one task
# from the internal catalog; figures approved before then keep their count. A new candidate is held to the exact
# current catalog by `check_catalog`. The hosted group grew to 52 when the hosted agent's final page became readable,
# so it runs the six navigation tasks too.
#
# The head-to-head (internal-hosted) is one Parallax invocation over one task set, the tasks fastbrowse and the hosted
# Browser Use agent both run, with rows from whichever of its arms ran. Browser Use
# Ultrafast returns no answer, so it runs only the tasks of that set graded without one; Browser Harness, a coding agent
# given no credentials, runs only its credential-free completion tasks. The official suites publish
# the fastbrowse arm; a vendor arm's rows reach a candidate only when Parallax marks them publishable.
HEAD_TO_HEAD: tuple[Arm, ...] = ("fastbrowse", "browser-use", "jev-ultrafast", "browser-harness")
OFFICIAL: tuple[Arm, ...] = ("fastbrowse", "browser-use")
COVERAGE: dict[GroupId, Coverage] = {
    "internal-fastbrowse": Coverage(frozenset({53, 54}), 3, "task-success", ("fastbrowse",)),
    "internal-hosted": Coverage(
        frozenset({46, 47, 52}), 3, "task-success", HEAD_TO_HEAD, (), ("jev-ultrafast", "browser-harness")
    ),
    "internal-ultrafast": Coverage(frozenset({6}), 3, "task-success", ("fastbrowse-pool", "jev-ultrafast")),
    "internal-backend": Coverage(frozenset({53, 54}), 3, "task-success", ("fastbrowse", "fastbrowse-pool")),
    "bu-bench": Coverage(frozenset({200}), None, "weighted-rubric", OFFICIAL, ("fastbrowse",)),
    "online-mind2web": Coverage(frozenset({300}), None, "full-task-success", OFFICIAL, ("fastbrowse",)),
}
POOL_ULTRAFAST_SCHEMA = 5
"""From this schema the Ultrafast comparison is fastbrowse with its own browser pool against `jev-ultrafast`: the
two whole products, each on its own backend. Before it, fastbrowse ran there on a Browser Use Cloud browser, so
figures approved under an older schema keep that pairing."""


def group_arms(group: GroupId, schema_version: int) -> tuple[Arm, ...]:
    """The arms a group may measure under the candidate's schema."""
    if group == "internal-ultrafast" and schema_version < POOL_ULTRAFAST_SCHEMA:
        return ("fastbrowse", "jev-ultrafast")
    return COVERAGE[group].arms


class Group(PublicModel):
    id: GroupId
    agent_sha: Sha
    runner_sha: Sha
    metric: Literal["task-success", "weighted-rubric", "full-task-success"]
    source_sha256: Digest
    task_ids: list[Identifier]
    repeats: Annotated[int, Field(gt=0)]
    limits: Annotated[list[Limit], Field(min_length=1, max_length=12)]
    rows: list[Row]

    @model_validator(mode="after")
    def coverage(self) -> Self:
        spec = COVERAGE[self.id]
        if len(self.task_ids) not in spec.counts or len(set(self.task_ids)) != len(self.task_ids):
            raise ValueError("task coverage differs from the approved full protocol")
        if spec.repeats not in (None, self.repeats) or self.metric != spec.metric or not self.limits:
            raise ValueError("repeats, metric and recorded limits are required")
        if self.metric != "weighted-rubric" and any(r.score not in {0, 1} for r in self.rows):
            raise ValueError("binary source grades cannot contain partial scores")
        return self

    def measured_arms(self, arms: tuple[Arm, ...]) -> tuple[Arm, ...]:
        """The arms of `arms` this group has rows for, in `arms` order."""
        present = {r.arm for r in self.rows}
        return tuple(arm for arm in arms if arm in present)

    def check_slots(self, arms: tuple[Arm, ...]) -> None:
        """The arms depend on the candidate's schema, so the candidate checks each group's slots against them.

        Each measured arm has one row for every repeat of every task it runs: the whole task set, or for a partial
        arm the part of it its rows name, which `check_catalog` holds to the catalog. Its cost is reported on every
        row or on none."""
        spec = COVERAGE[self.id]
        measured = self.measured_arms(arms)
        if {r.arm for r in self.rows} - set(arms) or not measured:
            raise ValueError(f"{self.id} measures only {', '.join(arms)}")
        if missing := [arm for arm in (arms if spec.required is None else spec.required) if arm not in measured]:
            raise ValueError(f"{self.id} must measure {', '.join(missing)}")
        for arm in measured:
            rows = [r for r in self.rows if r.arm == arm]
            slots = [(r.task, r.repeat) for r in rows]
            tasks = {task for task, _ in slots} if arm in spec.partial else set(self.task_ids)
            expected = {(task, repeat) for task in tasks for repeat in range(self.repeats)}
            if len(slots) != len(set(slots)) or set(slots) != expected or not tasks <= set(self.task_ids):
                raise ValueError("every scheduled slot must have exactly one measured row")
            if len({r.dollars is None for r in rows}) > 1:
                raise ValueError(f"{self.id}/{arm} reports its cost on every row or on none")


class Fixture(PublicModel):
    head_sha: Sha
    run_id: Annotated[int, Field(gt=0)]
    passed: Annotated[int, Field(gt=0)]
    total: Annotated[int, Field(gt=0)]

    @model_validator(mode="after")
    def all_passed(self) -> Self:
        if self.passed != self.total:
            raise ValueError("every fixture row must pass")
        return self


class ExportReceipt(PublicModel):
    """A verified measurement export that stands in for the live fixture run.

    A campaign measured on a build `main` has since moved past, or measured while workflow dispatch was
    unavailable, cannot mint the live `Evals` receipt the gate otherwise requires. This records the export that
    was verified instead: the bundle's digest, the campaign and build it covers, its row count and the trace it
    was read back from. It is a field of the reviewed candidate, so the owner approves it with the figures it
    carries; the receipt is never inferred from an aggregate or waived in silence.
    """

    bundle_sha256: Digest
    campaign_id: Identifier
    agent_sha: Sha
    groups: Annotated[list[GroupId], Field(min_length=1)]
    rows: Annotated[int, Field(gt=0)]
    trace_id: Annotated[str, Field(pattern=r"^[A-Za-z0-9_.:-]{1,64}$")]


SCHEMA = 5
"""The schema a new or changed record is written under. Older schemas still read: Parallax writes schema 2 for a
candidate of official suites alone, and the approved published figures and the staged candidate were written under 3
and 4, where an approval binds the bytes it approved."""


class Candidate(PublicModel):
    schema_version: Literal[2, 3, 4, 5]
    campaign_id: Identifier
    agent_sha: Sha
    runner_sha: Sha
    catalog_sha256: Digest
    fixture: Fixture | None = None
    export: ExportReceipt | None = None
    langfuse_verified: Literal[True]
    groups: list[Group]

    @model_validator(mode="after")
    def suites(self) -> Self:
        ids = {g.id for g in self.groups}
        if not self.groups or len(ids) != len(self.groups):
            raise ValueError("at least one complete, unique benchmark group is required")
        for group in self.groups:
            group.check_slots(group_arms(group.id, self.schema_version))
            for field in ("agent_sha", "runner_sha"):
                if getattr(group, field) != getattr(self, field):
                    raise ValueError("each group must identify the measured agent and runner build")
        if (self.fixture is None) == (self.export is None):
            raise ValueError(
                "a candidate carries exactly one measurement receipt: a live fixture run or a verified export"
            )
        if self.fixture is not None and self.agent_sha != self.fixture.head_sha:
            raise ValueError("fixtures must pass on the measured agent build")
        if self.export is not None:
            if self.export.agent_sha != self.agent_sha or self.export.campaign_id != self.campaign_id:
                raise ValueError("a verified export must name the measured build and campaign")
            if set(self.export.groups) != {group.id for group in self.groups}:
                raise ValueError("a verified export must cover exactly the published groups")
            # The receipt stands in for the live run, so it must account for every row about to be published: an
            # export covering a subset cannot vouch for rows it never measured.
            published_rows = sum(len(group.rows) for group in self.groups)
            if self.export.rows != published_rows:
                raise ValueError(
                    f"a verified export must account for every published row: the receipt claims "
                    f"{self.export.rows}, the groups hold {published_rows}"
                )
        return self


ARCHIVE_NAME = r"[A-Za-z0-9._+-]{1,64}\.[0-9a-f]{16}"
"""An archive name: a readable prefix of the campaign id and 16 hex of the archived figures' SHA256, at most 81
characters."""
ARCHIVE_FIGURES = rf"^docs/results/benchmarks\.{ARCHIVE_NAME}\.json$"
ARCHIVE_APPROVAL = rf"^docs/results/benchmark-approval\.{ARCHIVE_NAME}\.json$"


class GroupProtocol(PublicModel):
    """One published group whose protocol the owner changed: what it was measured under, and what it now is."""

    id: GroupId
    previous_source_sha256: Digest
    source_sha256: Digest
    previous_task_ids: list[Identifier]
    task_ids: list[Identifier]
    previous_limits: list[Limit]
    limits: list[Limit]


class ProtocolChange(PublicModel):
    """The owner's deliberate change of protocol. The changed groups are a fresh baseline, never compared with the
    figures they replace, which stay published at `archived_figures` with their approval at `archived_approval`."""

    note: Annotated[str, Field(pattern=r"^[A-Za-z0-9 ,.;:()'%+_=#/-]{1,500}$")]
    previous_candidate_sha256: Digest
    archived_figures: Annotated[str, Field(pattern=ARCHIVE_FIGURES)]
    archived_approval: Annotated[str, Field(pattern=ARCHIVE_APPROVAL)]
    groups: Annotated[list[GroupProtocol], Field(min_length=1)]


class Approval(PublicModel):
    candidate_sha256: Digest
    workflow_run: Annotated[int, Field(gt=0)]
    head_sha: Sha
    approved_by: Annotated[str, Field(min_length=1)]
    approved_at: Annotated[str, Field(pattern=r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z$")]
    protocol_change: ProtocolChange | None = None


def read(path: Path) -> Candidate:
    return Candidate.model_validate_json(path.read_bytes())


def check_catalog(candidate: Candidate, path: Path) -> None:
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() != candidate.catalog_sha256:
        raise ValueError("catalog must match the current pinned task source")
    catalog = json.loads(raw)
    tasks = [CatalogTask.model_validate(task) for suite in catalog["suites"].values() for task in suite]
    for group in candidate.groups:
        if group.id in {"bu-bench", "online-mind2web"}:
            continue
        # Approved figures keep the pairing they were measured under, but a new measurement is held to the current
        # one: fastbrowse with its own browser pool against jev-ultrafast.
        if group.id == "internal-ultrafast" and candidate.schema_version < POOL_ULTRAFAST_SCHEMA:
            raise ValueError(f"new internal-ultrafast rows pair fastbrowse-pool, from schema {POOL_ULTRAFAST_SCHEMA}")
        # A comparison covers the tasks every one of its arms is eligible for: the pool runs every fastbrowse task,
        # so the Ultrafast comparison covers the tasks fastbrowse and jev-ultrafast share whichever backend it ran.
        # A partial arm is left out of the task set, which is the same whichever arms ran, and runs its own part.
        arms = group_arms(group.id, candidate.schema_version)
        partial = COVERAGE[group.id].partial
        required = {task.id for task in tasks if all(eligible(arm, task) for arm in arms if arm not in partial)}
        if set(group.task_ids) != required or group.source_sha256 != candidate.catalog_sha256:
            kind = "fastbrowse" if group.id == "internal-fastbrowse" else "matched"
            raise ValueError(f"internal rows must cover the exact {kind} catalog")
        for arm in group.measured_arms(partial):
            own = {task.id for task in tasks if task.id in required and eligible(arm, task)}
            if {r.task for r in group.rows if r.arm == arm} != own:
                raise ValueError(f"{group.id}/{arm} must run exactly the tasks of its set the catalog lists it for")


CATALOG_PATH = "src/fastbrowse/evals/benchmark-catalog.json"


def catalog_at(digest: str) -> bytes | None:
    """The committed catalog whose bytes hash to `digest`, found in this checkout's history of the catalog."""
    done = subprocess.run(
        ["git", "log", "--format=%H", "--", CATALOG_PATH], capture_output=True, text=True, timeout=30, check=False
    )
    for commit in done.stdout.split():
        raw = git_file(commit, CATALOG_PATH)
        if raw is not None and hashlib.sha256(raw).hexdigest() == digest:
            return raw
    return None


def retired_between(before: bytes, after: bytes) -> set[str] | None:
    """The tasks `after` retires from `before` when listed retirements are the whole difference, else None.

    Every task left keeps its exact entry (version, fingerprint, arms, category, expectation), as do the arms and
    models; a retirement stays listed once made. Only the task-source hashes may also move, since removing a task
    edits those sources."""
    old, new = json.loads(before), json.loads(after)
    old_retired = {r["id"]: r for r in old.get("retired", [])}
    new_retired = {r["id"]: r for r in new.get("retired", [])}
    if any(new_retired.get(task) != entry for task, entry in old_retired.items()):
        return None
    newly = set(new_retired) - set(old_retired)
    where = {t["id"]: name for name, tasks in old["suites"].items() for t in tasks}
    if any(where.get(task) != new_retired[task]["suite"] for task in newly):
        return None
    left = {name: [t for t in tasks if t["id"] not in newly] for name, tasks in old["suites"].items()}
    ignored = ("task_sources", "suites", "retired")
    if left != new["suites"] or {k: v for k, v in old.items() if k not in ignored} != {
        k: v for k, v in new.items() if k not in ignored
    }:
        return None
    return newly


def _retirement_only(group: Group, old: Group, catalog: bytes | None) -> bool:
    """Whether `group` is `old` on the current catalog less tasks retired since, under the same limits."""
    if catalog is None or hashlib.sha256(catalog).hexdigest() != group.source_sha256 or group.limits != old.limits:
        return False
    before = catalog_at(old.source_sha256)
    newly = None if before is None else retired_between(before, catalog)
    # A catalog that only moved its task-source hashes retired nothing; that is a protocol change, not a retirement.
    return bool(newly) and set(group.task_ids) == set(old.task_ids) - newly


def unapproved_changes(candidate: Candidate, baseline: Candidate, catalog: bytes | None) -> set[str]:
    """The published groups whose difference is more than a listed retirement: a protocol change only the owner's
    protocol_change approval can publish."""
    previous = {g.id: g for g in baseline.groups}
    groups = {g.id: g for g in candidate.groups}
    return {
        change.id
        for change in changed_protocols(candidate, baseline)
        if _arms_changed(groups[change.id], candidate, baseline)
        or not _retirement_only(groups[change.id], previous[change.id], catalog)
    }


def _arms_changed(group: Group, candidate: Candidate, baseline: Candidate) -> bool:
    return group_arms(group.id, candidate.schema_version) != group_arms(group.id, baseline.schema_version)


def changed_protocols(candidate: Candidate, baseline: Candidate) -> list[GroupProtocol]:
    """The published groups `candidate` measures under a different catalog, task list, limits or pairing of arms."""
    previous = {g.id: g for g in baseline.groups}
    return [
        GroupProtocol(
            id=group.id,
            previous_source_sha256=old.source_sha256,
            source_sha256=group.source_sha256,
            previous_task_ids=sorted(old.task_ids),
            task_ids=sorted(group.task_ids),
            previous_limits=old.limits,
            limits=group.limits,
        )
        for group in candidate.groups
        if (old := previous.get(group.id)) is not None
        and (
            (group.source_sha256, sorted(group.task_ids), group.limits)
            != (old.source_sha256, sorted(old.task_ids), old.limits)
            or _arms_changed(group, candidate, baseline)
        )
    ]


def archive_paths(baseline: Candidate, figures: bytes) -> tuple[Path, Path]:
    """Where superseded figures and their approval stay published: the measuring campaign's id, sanitized and cut
    to 64 characters to stay readable, then the first 16 hex of the figures' own SHA256. The name is bounded whatever
    the id, a reused id with other figures gets another name, and the name can be checked against the content. The
    record keeps the exact paths, so a reader follows them rather than recomputing names."""
    prefix = re.sub(r"[^A-Za-z0-9._+-]", "-", baseline.campaign_id)[:64]
    name = f"{prefix}.{hashlib.sha256(figures).hexdigest()[:16]}"
    return RESULTS / f"benchmarks.{name}.json", RESULTS / f"benchmark-approval.{name}.json"


def protocol_change(
    candidate: Candidate, baseline: Candidate, baseline_bytes: bytes, note: str, catalog: bytes | None
) -> ProtocolChange:
    """The record an owner approves to replace `baseline`'s changed groups; refused when no protocol changed.

    A group that differs only by retirements listed in `catalog` is not a protocol change: it stays out of the record
    and is still compared with its published figures on the tasks they share."""
    unapproved = unapproved_changes(candidate, baseline, catalog)
    groups = [g for g in changed_protocols(candidate, baseline) if g.id in unapproved]
    if not groups:
        raise ValueError("no published group changed protocol; publish without a protocol change")
    figures, approval = archive_paths(baseline, baseline_bytes)
    return ProtocolChange(
        note=note,
        previous_candidate_sha256=hashlib.sha256(baseline_bytes).hexdigest(),
        archived_figures=str(figures),
        archived_approval=str(approval),
        groups=groups,
    )


def regressions(
    candidate: Candidate, baseline: Candidate, catalog: bytes | None = None, changed: Collection[str] = ()
) -> None:
    """Refuse a dropped group, a schema downgrade, an unapproved protocol change or a matched regression.

    `catalog` is the current catalog's bytes. With it, a group whose only difference from the published one is
    tasks the catalog lists as retired since is compared on the tasks they share. `changed` names groups whose
    protocol change the owner approved: they are a fresh baseline and are not compared with the figures they
    replace. Any other difference is a protocol change that needs the owner's approval."""
    if candidate.schema_version < baseline.schema_version:
        raise ValueError("publication schema cannot downgrade the approved evidence checks")
    previous = {g.id: g for g in baseline.groups}
    if not previous.keys() <= {g.id for g in candidate.groups}:
        raise ValueError("published benchmark groups cannot disappear")
    for group in candidate.groups:
        old = previous.get(group.id)
        if old is None or group.id in changed:
            continue
        if _arms_changed(group, candidate, baseline) or (
            (group.source_sha256, sorted(group.task_ids), group.limits)
            != (old.source_sha256, sorted(old.task_ids), old.limits)
            and not _retirement_only(group, old, catalog)
        ):
            raise ValueError(f"protocol changed: {group.id}; matched baseline measurement is required")
        arm = group_arms(group.id, candidate.schema_version)[0]
        if arm not in group.measured_arms((arm,)):
            raise ValueError(f"protocol changed: {group.id} no longer measures {arm}")
        for task in group.task_ids:
            now = [r for r in group.rows if r.arm == arm and r.task == task]
            before = [r for r in old.rows if r.arm == arm and r.task == task]
            if sum(r.score for r in now) / len(now) < sum(r.score for r in before) / len(before):
                raise ValueError(f"source-score regression: {group.id}/{task}")
            if sum(r.completed for r in now) < sum(r.completed for r in before):
                raise ValueError(f"completion regression: {group.id}/{task}")
            moved = moves(f"{group.id}/{task}", "the published figures", _dumped(before), _dumped(now))
            if moved:
                raise ValueError(f"{moved[0].check} regression: {moved[0].detail}")


def _dumped(rows: list[Row]) -> list[dict]:
    return [row.model_dump() for row in rows]


def github_run(run_id: int) -> dict:
    result = subprocess.run(
        ["gh", "api", f"repos/agent-labs-dev/fastbrowse/actions/runs/{run_id}"],
        capture_output=True,
        text=True,
        timeout=30,
        check=True,
    )
    return json.loads(result.stdout)


def on_main(sha: str) -> bool:
    """Whether `sha` is in main's history, so a run at it used a workflow file and runner main once held."""
    result = subprocess.run(
        ["gh", "api", f"repos/agent-labs-dev/fastbrowse/compare/{sha}...main", "--jq", ".status"],
        capture_output=True,
        text=True,
        timeout=30,
        check=True,
    )
    return result.stdout.strip() in {"ahead", "identical"}


def check_fixture(candidate: Candidate) -> None:
    if candidate.export is not None:
        # A verified export stands in for the live fixture run: the campaign was measured on a build `main` has
        # moved past, or while workflow dispatch was unavailable, so the receipt is the export itself, bound by
        # digest and carried in the reviewed candidate the owner approves. Nothing is waived in silence.
        return
    assert candidate.fixture is not None  # the model requires exactly one receipt
    run = github_run(candidate.fixture.run_id)
    if (
        run["head_sha"] != candidate.agent_sha
        or run["conclusion"] != "success"
        or run["status"] != "completed"
        or run["path"] != ".github/workflows/evals.yml"
        or run["repository"]["full_name"] != "agent-labs-dev/fastbrowse"
        or run["event"] != "workflow_dispatch"
        # A branch other than main is accepted only at a commit in main's history: the run then used the workflow
        # file and runner main held, so a maintainer can produce the receipt for a build main has moved past,
        # but not from a branch whose own commit replaces the workflow or its runner.
        or (run["head_branch"] != "main" and not on_main(run["head_sha"]))
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
    if len(slots) != len(set(slots)) or set(slots) != expected:
        raise ValueError("fixture artifact coverage must be every fixture task across three repeats")
    if candidate.fixture.total != len(rows):
        raise ValueError("the fixture receipt must claim every row the artifact holds")
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
        raise ValueError("a successful publish-evals dispatch on main is required")


def approval_artifact(run_id: int) -> bytes:
    """The receipt the owner's publish-evals run wrote, as its `benchmark-approval` artifact holds it."""
    with tempfile.TemporaryDirectory(prefix="benchmark-approval-") as directory:
        subprocess.run(
            [
                "gh",
                "run",
                "download",
                str(run_id),
                "--repo",
                "agent-labs-dev/fastbrowse",
                "--name",
                "benchmark-approval",
                "--dir",
                directory,
            ],
            capture_output=True,
            timeout=60,
            check=True,
        )
        return (Path(directory) / "benchmark-approval.json").read_bytes()


def git_names(ref: str, directory: Path) -> list[str]:
    done = subprocess.run(
        ["git", "ls-tree", "--name-only", f"{ref}:{directory}"], capture_output=True, text=True, timeout=30, check=False
    )
    return done.stdout.split() if done.returncode == 0 else []


ARCHIVE = re.compile(rf"benchmark(s|-approval)\.{ARCHIVE_NAME}\.json")


def check_archives(base: str, added: set[str]) -> None:
    """Archived figures and approvals are published results: once at the base, never rewritten or removed. A new
    archive file is allowed only when it is one of `added`, the pair a validated protocol change names."""
    archived = {name for name in git_names(base, RESULTS) if ARCHIVE.fullmatch(name)}
    for name in archived:
        path = RESULTS / name
        if not path.exists() or path.read_bytes() != git_file(base, str(path)):
            raise ValueError(f"archived benchmark figures cannot change or disappear: {path}")
    for path in sorted(RESULTS.glob("benchmarks.*.json")):
        if ARCHIVE.fullmatch(path.name) and not path.name.endswith(
            f".{hashlib.sha256(path.read_bytes()).hexdigest()[:16]}.json"
        ):
            raise ValueError(f"{path} is not named by the digest of the figures it holds")
    for path in sorted(RESULTS.glob("benchmark*.json")):
        if ARCHIVE.fullmatch(path.name) and path.name not in archived and str(path) not in added:
            raise ValueError(f"{path} is not the archive of an approved protocol change")


def check_protocol_change(
    candidate: Candidate,
    baseline: Candidate,
    baseline_bytes: bytes,
    base_approval: bytes,
    approval: Approval,
    catalog: bytes | None,
) -> set[str]:
    """The groups whose protocol the published approval changed, after proving its record names exactly the groups
    that changed, from exactly the figures at the base, and that those figures and their approval are archived."""
    record = approval.protocol_change
    if record is None:
        # Without the owner's record nothing is exempt: `regressions` refuses every change but a listed retirement.
        return set()
    if record != protocol_change(candidate, baseline, baseline_bytes, record.note, catalog):
        raise ValueError("the protocol_change record must name exactly the changed groups of the published figures")
    for path, content in ((record.archived_figures, baseline_bytes), (record.archived_approval, base_approval)):
        if not Path(path).exists() or Path(path).read_bytes() != content:
            raise ValueError(f"superseded figures must stay published unchanged at {path}")
    return {g.id for g in record.groups}


def check_diff(base: str, catalog: Path) -> None:
    published = PUBLISHED
    approval_path = APPROVAL
    added: set[str] = set()
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
    for path in (STAGED, published):
        if not path.exists():
            continue
        candidate = read(path)
        if git_file(base, str(path)) == path.read_bytes():
            continue
        if candidate.schema_version != SCHEMA:
            raise ValueError(f"a new or changed benchmark record must use schema {SCHEMA}")
        check_catalog(candidate, catalog)
        check_fixture(candidate)
        if baseline is None or old is None or old_approval is None:
            continue
        if path == published:
            if not approval_path.exists():
                raise ValueError("published figures require an owner approval receipt")
            approval = Approval.model_validate_json(approval_path.read_bytes())
            approved = check_protocol_change(candidate, baseline, old, old_approval, approval, catalog.read_bytes())
            regressions(candidate, baseline, catalog.read_bytes(), approved)
            if approval.protocol_change is not None:
                added = {approval.protocol_change.archived_figures, approval.protocol_change.archived_approval}
        else:
            # A staged candidate is reviewed, not published: a changed protocol is reported here and refused at
            # approval unless the owner dispatches it as a protocol change. Unchanged groups are still compared.
            changed = unapproved_changes(candidate, baseline, catalog.read_bytes())
            if changed:
                print(f"Staged protocol change for {', '.join(sorted(changed))}: needs the owner's protocol_change")
            regressions(candidate, baseline, catalog.read_bytes(), changed)
    if published.exists():
        if not approval_path.exists():
            raise ValueError("published figures require an owner approval receipt")
        receipt = approval_path.read_bytes()
        approval = Approval.model_validate_json(receipt)
        check_approval(published.read_bytes(), approval)
        # A new receipt is the owner's run's own bytes: its protocol_change note, groups and archive links are the
        # ones the run recorded, not text edited after download.
        if receipt != old_approval and approval_artifact(approval.workflow_run) != receipt:
            raise ValueError("the approval receipt differs from the one the owner's publish-evals run produced")
    check_archives(base, added)


def install(candidate_path: Path, approval_path: Path) -> None:
    """Put approved figures in place, as the data PR commits them. A protocol change first archives the figures and
    approval it supersedes, so they stay published under their campaign's name."""
    content = candidate_path.read_bytes()
    approval_bytes = approval_path.read_bytes()
    approval = Approval.model_validate_json(approval_bytes)
    if hashlib.sha256(content).hexdigest() != approval.candidate_sha256:
        raise ValueError("the approval does not cover these candidate bytes")
    record = approval.protocol_change
    if record is not None:
        old = PUBLISHED.read_bytes()
        if hashlib.sha256(old).hexdigest() != record.previous_candidate_sha256:
            raise ValueError("the published figures are not the ones this protocol change supersedes")
        for target, source in ((Path(record.archived_figures), PUBLISHED), (Path(record.archived_approval), APPROVAL)):
            if target.exists() and target.read_bytes() != source.read_bytes():
                raise ValueError(f"{target} already holds other figures")
            target.write_bytes(source.read_bytes())
    PUBLISHED.write_bytes(content)
    APPROVAL.write_bytes(approval_bytes)


def _next_action(
    candidate: Candidate, *, catalog_ok: bool, receipt_ok: bool, regression_ok: bool, has_baseline: bool
) -> str:
    """The one action that clears the next blocker, chosen from the checks that just ran.

    A passing candidate is never told to repeat a step it has already cleared, and a failing one is never told to
    publish: the action follows the first failure in the order the gate applies them.
    """
    if not catalog_ok:
        return "correct the catalog coverage or the group task list, then stage the candidate again"
    if not receipt_ok:
        if candidate.export is not None:
            return "re-verify the export bundle against the published rows and stage the candidate again"
        return "dispatch Evals at the measured build for the fixture receipt"
    if not regression_ok:
        return "re-measure the regressed group, or get the owner's protocol_change approval for the change"
    if not has_baseline:
        return "the first publication requires the owner's --bootstrap approval"
    return "commit the reviewed candidate and the owner approval, then deploy the site"


def status(candidate_path: Path, catalog_path: Path, baseline_path: Path) -> str:
    """What stands between the staged candidate and publication, and the single next action.

    The operator's view of the gate: it runs every check that needs no network, names the receipt the candidate
    carries, and states the one action that clears the next blocker. A blocked publication then says why, instead
    of surfacing as a terse failure deep in a workflow log.
    """
    if not candidate_path.exists():
        return f"no candidate at {candidate_path}; stage one before publishing"
    candidate = read(candidate_path)
    lines = [
        f"candidate {candidate.campaign_id} (schema {candidate.schema_version}) on {candidate.agent_sha[:12]}",
        f"groups: {', '.join(group.id for group in candidate.groups)}",
    ]
    try:
        check_catalog(candidate, catalog_path)
        catalog_ok = True
        lines.append("PASS catalog: internal groups cover the pinned catalog")
    except ValueError as exc:
        catalog_ok = False
        lines.append(f"FAIL catalog: {exc}")
    receipt_ok = True
    if candidate.export is not None:
        lines.append(
            f"PASS export: {candidate.export.bundle_sha256[:12]} accounts for all {candidate.export.rows} "
            f"published rows (trace {candidate.export.trace_id})"
        )
    else:
        assert candidate.fixture is not None
        try:
            check_fixture(candidate)
            lines.append(f"PASS fixture: run {candidate.fixture.run_id} is a successful receipt on the measured build")
        except Exception as exc:
            receipt_ok = False
            lines.append(f"FAIL fixture: {exc}")
    regression_ok = True
    has_baseline = baseline_path.exists()
    if has_baseline:
        try:
            regressions(candidate, Candidate.model_validate_json(baseline_path.read_bytes()), catalog_path.read_bytes())
            lines.append("PASS regression: no task-version matched decline against the published baseline")
        except ValueError as exc:
            regression_ok = False
            lines.append(f"FAIL regression: {exc}")
    else:
        lines.append("no published baseline: the first publication needs --bootstrap approval")
    action = _next_action(
        candidate,
        catalog_ok=catalog_ok,
        receipt_ok=receipt_ok,
        regression_ok=regression_ok,
        has_baseline=has_baseline,
    )
    lines.append("next: " + action)
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    # Resolved per mode: a bare `--status` inspects the staged candidate, everything else the published figures.
    parser.add_argument("--candidate", type=Path, help="the figures to read (defaults to the staged or published file)")
    parser.add_argument("--catalog", type=Path, default=Path("src/fastbrowse/evals/benchmark-catalog.json"))
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--verify-fixture", action="store_true")
    parser.add_argument("--status", action="store_true", help="print what blocks publication and the next action")
    parser.add_argument("--check-diff")
    parser.add_argument("--bootstrap", action="store_true", help="owner approval for the first publication")
    parser.add_argument("--protocol-change", action="store_true", help="owner approval of a deliberate protocol change")
    parser.add_argument("--protocol-note", help="why the protocol changed; required with --protocol-change")
    parser.add_argument("--protocol-record", type=Path, help="where to write the protocol_change record")
    parser.add_argument("--install", type=Path, metavar="APPROVAL", help="install approved figures and archive")
    args = parser.parse_args()
    args.candidate = args.candidate or (STAGED if args.status else PUBLISHED)
    if args.status:
        print(status(args.candidate, args.catalog, args.baseline or PUBLISHED))
        return
    if args.install:
        install(args.candidate, args.install)
        print(f"Installed {PUBLISHED} and {APPROVAL}")
        return
    if args.protocol_change and not args.protocol_note:
        parser.error("--protocol-change requires --protocol-note")
    if args.check_diff:
        check_diff(args.check_diff, args.catalog)
        print("Benchmark publication coverage and approval checked")
        return
    candidate = read(args.candidate)
    check_catalog(candidate, args.catalog)
    if args.baseline:
        if args.baseline.exists():
            catalog = args.catalog.read_bytes()
            baseline_bytes = args.baseline.read_bytes()
            baseline = Candidate.model_validate_json(baseline_bytes)
            if args.protocol_change:
                # Dropped groups and regressions in groups that kept their protocol, a retirement-only group on the
                # tasks it shares, are refused before the change is recorded.
                regressions(candidate, baseline, catalog, unapproved_changes(candidate, baseline, catalog))
                try:
                    record = protocol_change(candidate, baseline, baseline_bytes, args.protocol_note, catalog)
                except ValidationError as error:
                    parser.error(f"invalid protocol note: {error}")
                if args.protocol_record:
                    args.protocol_record.write_text(record.model_dump_json(indent=2) + "\n")
                print(f"Protocol change for {', '.join(g.id for g in record.groups)}: a fresh baseline")
            else:
                regressions(candidate, baseline, catalog)
        elif args.protocol_change:
            parser.error("a protocol change needs a published baseline; use --bootstrap for the first publication")
        elif not args.bootstrap:
            parser.error("the first publication requires explicit --bootstrap approval")
    if args.verify_fixture:
        check_fixture(candidate)
    print(f"Validated {candidate.campaign_id}, selected full groups covered")


if __name__ == "__main__":
    main()
