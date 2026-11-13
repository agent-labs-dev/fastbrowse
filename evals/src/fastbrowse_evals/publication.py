"""Export the sanitized benchmark candidate Fastbrowse's release gate and site read.

Every figure comes from private run evidence that this module re-derives and checks, never from a claim: the
internal ledgers against the canonical catalog, the official runs against their actual task source, and each
campaign against what the private Langfuse project holds. The output has closed fields and no detailed receipts.
The public shape comes from `fastbrowse.evals.benchmarks`, the same contract CI validates.
"""

import argparse
import dataclasses
import hashlib
import json
import math
import re
import subprocess
import sys
import tempfile
from collections import defaultdict
from collections.abc import Awaitable, Callable, Collection, Iterable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal

from fastbrowse.evals.benchmarks import (
    LINK,
    SCHEMA,
    Candidate,
    CompetitorReference,
    ExcludedTask,
    Fixture,
    Group,
    ReferenceArm,
    Row,
    Source,
    UnjudgedSlot,
    check_catalog,
    check_fixture,
    fixture_slots,
)
from fastbrowse.models import Limits
from parallax.track import sink
from parallax.track.projection import ItemInput, ItemOutput
from pydantic import BaseModel, ConfigDict

from fastbrowse_evals import execution_receipts, langfuse_runs, official, official_tracking, tracking
from fastbrowse_evals.adapters.browser_official_bridge import BillingError, judge_charge
from fastbrowse_evals.attempt_clock import SESSION_OPEN_SECONDS, TIMING_BASIS
from fastbrowse_evals.budget import POLICY
from fastbrowse_evals.catalog import render
from fastbrowse_evals.judging import JUDGE_MODEL, OPENROUTER_BASE_URL
from fastbrowse_evals.live import (
    AGENT_LIMITS,
    HOSTED_MODELS,
    HOSTED_UNCAPPED_USD,
    JUDGE_POLICY,
    OUTAGE_RETRIES,
    judged_unavailable,
)
from fastbrowse_evals.registry import Suite
from fastbrowse_evals.versions import installed_catalog

PROJECT = langfuse_runs.PROJECT
INTERNAL_REPEATS = 3
# Mirrors the public limit rules: short ASCII phrases, no slash and nothing that names a link.
LIMIT = re.compile(r"[A-Za-z0-9 ,.;:()'%+_=-]{1,240}")


class PublicationError(ValueError):
    """Evidence that cannot support a published figure; the message says what to fix."""


# The three head-to-heads: agent against agent (internal-hosted), browser backend against browser backend
# (internal-backend), and fastbrowse on its own backend against Browser Use Ultrafast (internal-ultrafast).
# internal-fastbrowse stays exportable because it is already published and a published group cannot disappear.
# Each group's arms fix its task set: the tasks every one of them runs. The hosted group measures whichever of
# `HOSTED_ARMS` its invocation ran, on that one task set.
GROUPS: dict[str, tuple[int, int, str, set[str]]] = {
    "internal-fastbrowse": (53, 3, "task-success", {"fastbrowse"}),
    "internal-hosted": (53, 3, "task-success", {"fastbrowse"}),
    "internal-ultrafast": (52, 3, "task-success", {"fastbrowse-pool", "browser-use-ultrafast"}),
    "internal-backend": (53, 3, "task-success", {"fastbrowse", "fastbrowse-pool"}),
    # Official groups take whatever repeat count their runs recorded (`official_group`).
    "bu-bench": (200, 1, "weighted-rubric", {"fastbrowse"}),
    "online-mind2web": (300, 1, "full-task-success", {"fastbrowse"}),
}


# --- small validators ---


def note(text: str) -> str:
    """A public limit: the contract's character set, with slashes in model ids shown as colons."""
    text = text.replace("/", ":")
    if not LIMIT.fullmatch(text) or LINK.search(text):
        raise PublicationError(f"recorded protocol text cannot be published as a short phrase: {text[:80]!r}")
    return text


def amount(value: object, what: str) -> float:
    """A recorded finite non-negative number; unknown (None), NaN, negative and boolean values are refused."""
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < 0:
        raise PublicationError(f"{what} is missing or not a known finite non-negative number")
    return float(value)


def call_counts(attempts: list[dict[str, Any]]) -> dict[str, Any]:
    """Different count bases cannot be added, and a partial count is unknown."""
    bases: set[str] = set()
    for attempt in attempts:
        basis = attempt.get("llm_calls_basis")
        if basis is None:
            basis = "cost_lines"
        if not isinstance(basis, str) or basis not in {"cost_lines", "physical_requests"}:
            raise PublicationError("llm_calls_basis is not a supported measurement")
        bases.add(basis)
    counts = [a.get("llm_calls") for a in attempts]
    for count in counts:
        if count is not None and (isinstance(count, bool) or not isinstance(count, int) or count < 0):
            raise PublicationError("llm_calls is not a non-negative integer")
    known = len(bases) == 1 and all(count is not None for count in counts)
    result: dict[str, Any] = {"llm_calls": sum(count for count in counts if count is not None) if known else None}
    if bases == {"physical_requests"}:
        result["llm_calls_basis"] = "physical_requests"
    return result


def sha(value: object, what: str) -> str:
    if not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{40}", value):
        raise PublicationError(f"{what} is not a full commit")
    return value


def text(value: object, what: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise PublicationError(f"{what} is not recorded")
    return value


def lines(path: Path) -> list[dict[str, Any]]:
    if not path.is_file():
        raise PublicationError(f"{path.name} is missing")
    rows = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    if not rows or not all(isinstance(r, dict) for r in rows):
        raise PublicationError(f"{path.name} holds no result objects")
    return rows


@dataclass(frozen=True)
class Collected:
    group: Group
    agent_sha: str
    runner_sha: str
    tracked: tuple[Any, ...]
    """The TrackedRuns the private Langfuse project must hold, one per run; official groups are read back by run."""
    physical_runs: int
    started: str | None = None
    """When the group's newest run started; None for an official group."""


def build_group(
    group_id: str,
    source_sha256: str,
    task_ids: list[str],
    limits: list[str],
    rows: list[Row],
    run: dict[str, Any],
    repeats: int | None = None,
    unjudged: list[UnjudgedSlot] | None = None,
    sources: list[Source] | None = None,
    excluded: list[ExcludedTask] | None = None,
) -> Group:
    return Group(
        id=group_id,  # ty: ignore[invalid-argument-type]
        agent_sha=sha(run.get("git_sha"), "agent commit"),
        runner_sha=sha(run.get("benchmark_runner_sha"), "runner"),
        metric=GROUPS[group_id][2],  # ty: ignore[invalid-argument-type]
        source_sha256=source_sha256,
        task_ids=task_ids,
        repeats=repeats or GROUPS[group_id][1],
        limits=[note(n) for n in limits],
        rows=sorted(rows, key=lambda r: (r.arm, r.task, r.repeat)),
        unjudged=unjudged or [],
        excluded=excluded or [],
        sources=sources or [],
    )


def exclusions(arms: Iterable[str], task_ids: Iterable[str], catalog: dict[str, Any]) -> list[ExcludedTask]:
    """Each task of the group the catalog says one of `arms` cannot run, with the catalog's reason. It is a failure
    for that arm at every repeat: a comparison across browser agents counts a capability one lacks against it."""
    tasks = {t["id"]: t for suite in catalog["suites"].values() for t in suite}
    found = []
    for arm in arms:
        for task in task_ids:
            if arm in tasks[task]["arms"]:
                continue
            reason = tasks[task]["excluded"].get(arm)
            if reason is None:
                raise PublicationError(f"{arm}/{task}: the catalog does not say why the arm cannot run the task")
            found.append(ExcludedTask(arm=arm, task=task, reason=reason))  # ty: ignore[invalid-argument-type]
    return sorted(found, key=lambda e: (e.arm, e.task))


# --- internal comparisons ---

INTERNAL = {
    "internal-fastbrowse": None,
    "internal-hosted": "browser-use",
    "internal-ultrafast": "browser-use-ultrafast",
    "internal-backend": "fastbrowse-pool",
}
SLOT_FIELDS = (
    "status", "passed", "seconds", "dollars", "unknown_cost", "provisional_hosted_cost",
    "llm_calls", "llm_calls_basis", "timing_note",
)  # fmt: skip
FASTBROWSE_ARMS = {"fastbrowse", "fastbrowse-pool"}
"""Arms that are fastbrowse itself, whose completion is its own `complete` status."""
HOSTED_ARMS = (
    "fastbrowse",
    "browser-use",
    "browser-use-ultrafast",
    "browser-harness",
    "browser-use-oss",
)
"""The arms the hosted head-to-head may measure, in display order: any of them its one invocation ran."""
BROWSER_MODES = {
    "fastbrowse": {"local-chrome", "browser-use-cloud"},
    "fastbrowse-pool": {"fastbrowse-pool"},
    "browser-use": {"browser-use-hosted"},
    "browser-use-ultrafast": {"browser-use-hosted"},
    "browser-harness": {"browser-use-cloud"},
    "browser-use-oss": {"browser-use-cloud"},
}
"""The browser each arm may have driven; which one it was is recorded, checked and published per arm."""
BROWSER_USE_MODES = {"browser-use-cloud", "browser-use-hosted"}
UNCAPPED_NOTES = (
    "No time, step or spend limit on any agent; the campaign budget only gates new runs",
    "Browser Use enforces a 4 hour maximum session on its hosted agent and cloud browsers",
)


def uncapped(recorded: object, where: str) -> None:
    """`recorded` must be the complete agent limits with every field None: any recorded cap refuses publication."""
    if not isinstance(recorded, dict) or set(recorded) != set(Limits.model_fields):
        raise PublicationError(f"{where}: every agent limit must be recorded")
    if recorded != AGENT_LIMITS.model_dump(mode="json"):
        raise PublicationError(f"{where}: an agent limit was recorded; every arm must run uncapped")


def browser_identity(run: dict[str, Any], arm: str, group_id: str) -> str:
    """The recorded browser of one arm, checked; returns its mode."""
    browsers = run.get("browsers")
    browser = browsers.get(arm) if isinstance(browsers, dict) else None
    if not isinstance(browser, dict) or browser.get("mode") not in BROWSER_MODES.get(arm, set()):
        raise PublicationError(f"{group_id}: the browser {arm} drove is not recorded")
    mode = browser["mode"]
    if mode == "local-chrome" and (
        not isinstance(browser.get("binary"), str)
        or not Path(browser["binary"]).is_absolute()
        or not isinstance(browser.get("version"), str)
        or not browser["version"].strip()
        or not isinstance(browser.get("sha256"), str)
        or re.fullmatch(r"[0-9a-f]{64}", browser["sha256"]) is None
        or browser.get("headed") is not False
        or browser.get("profile") is not False
    ):
        raise PublicationError(f"{group_id}: complete headless local Chrome identity is required")
    if mode == "fastbrowse-pool" and not text(browser.get("host"), f"{group_id}: pool host"):
        raise PublicationError(f"{group_id}: the pool host is not recorded")
    return mode


TIMING_NOTE = (
    "Seconds run from the browser session being ready to the agent's final answer; "
    "opening and closing the browser session are excluded"
)


PHYSICAL_TIMING_NOTE = (
    "Never-started attempts retain dispatch elapsed time; executed attempts retain session-ready-to-answer time. "
    "Executed session opening and closing are excluded; complete phase totals are unavailable"
)
MIXED_TIMING_NOTE = (
    "Rows state their clock; physical-attempt-wall-time includes dispatch and admission interruptions. "
    "Times on different bases are not compared"
)


def timed(fields: dict[str, Any], where: str) -> None:
    """Refuse a row or a run whose seconds were not measured on the one clock every arm is compared on."""
    if fields.get("timing_basis") != TIMING_BASIS:
        raise PublicationError(f"{where}: its seconds are not measured from session ready to the answer")


def timing_fields(
    tries: Iterable[dict[str, Any]], execution: Mapping[str, execution_receipts.ValidatedExecution] | None = None
) -> dict[str, Any]:
    """A public row's timing basis and session seconds, totalled as its seconds are. The pinned public contract may
    predate these fields, and it forbids any it does not name."""
    if "timing_basis" not in getattr(Row, "model_fields", {}):
        return {}
    tries = list(tries)
    if execution and any(execution_receipts.key(t) in execution for t in tries):
        # Admission failed before an agent ran, so its elapsed wait has no ready-to-answer clock or phase totals.
        return {
            "timing_basis": "physical-attempt-wall-time",
            "timing_note": PHYSICAL_TIMING_NOTE,
            "session_open_seconds": None,
            "session_close_seconds": None,
        }
    total = {
        key: math.fsum(values) if (values := [t[key] for t in tries if t.get(key) is not None]) else None
        for key in ("session_open_seconds", "session_close_seconds")
    }
    agent = {}
    if "agent_seconds" in Row.model_fields and all(t.get("agent_seconds") is not None for t in tries):
        agent = {"agent_seconds": math.fsum(t["agent_seconds"] for t in tries)}
    # Seconds summed over tries are one clock only if every try kept the same browser setup on it.
    notes = {t.get("timing_note") for t in tries}
    if len(notes) != 1:
        raise PublicationError("the tries of one slot kept different browser setup in their seconds")
    [setup] = notes
    note = {"timing_note": setup} if setup is not None and "timing_note" in Row.model_fields else {}
    return {"timing_basis": TIMING_BASIS, **total, **agent, **note}


def timed_schema(schema: int) -> Any:
    """`schema`, or the contract's schema for rows that record their timing basis when the pinned contract has them."""
    return SCHEMA if "timing_basis" in getattr(Row, "model_fields", {}) else schema


def reference_basis(row: Row) -> str | None:
    """A reference row's basis; a contract without the field has none, so its reference is refused."""
    return getattr(row, "timing_basis", None)


def internal_protocol(
    run: dict[str, Any], group_id: str, arms: set[str], catalog: dict[str, Any], claimed: set[str] | None = None
) -> int:
    """Check that one invocation produced the ledger under the uncapped start-gate protocol; returns its concurrency.

    One protocol for every internal group: no arm had a step, time, dollar or model-call limit, the approved budget
    only gated new physical runs (`start-gate-v1`), and a hosted Browser Use arm overrode the provider's default cost
    limit. Any recorded cap refuses publication rather than being described. Every run is judged semantically, so
    every run also held a positive judge reserve. The run may predate a change to the catalog: each task it supplies
    is held to the catalog's version of that task (`invocation`), and only the `claimed` arms it supplies rows for
    (all of `arms` when None) must match the catalog's pins, so a re-measured arm does not void another arm's rows."""
    if amount(run.get("judge_reserve_dollars"), f"{group_id}: judge_reserve_dollars") <= 0:
        raise PublicationError(f"{group_id}: a positive declared judge reserve is required")
    recorded = run.get("arms")
    if (
        not isinstance(recorded, dict)
        or set(recorded) != arms
        or any(
            recorded[name] != {"pin": catalog["arms"][name]["pin"], "tier": catalog["arms"][name]["tier"]}
            for name in (arms if claimed is None else claimed & arms)
        )
    ):
        raise PublicationError(f"{group_id}: the run's arms differ from the group's agents and catalog pins")
    if run.get("benchmark_family") != "browser-use" or run.get("benchmark_origin") != "fastbrowse-internal":
        raise PublicationError(f"{group_id}: the run was not made against the canonical internal catalog")
    if run.get("budget_policy") != POLICY:
        raise PublicationError(f"{group_id}: the {POLICY} budget protocol is required")
    if amount(run.get("budget_usd"), f"{group_id}: budget") <= 0:
        raise PublicationError(f"{group_id}: a positive campaign budget is required")
    if amount(run.get("reserve_dollars"), f"{group_id}: reserve_dollars") <= 0:
        raise PublicationError(f"{group_id}: a positive per-run reservation is required")
    amount(run.get("browser_reserve_dollars"), f"{group_id}: browser_reserve_dollars")
    if any(key not in run or run[key] is not None for key in ("max_steps", "max_seconds", "max_dollars")):
        raise PublicationError(f"{group_id}: a step, time or dollar limit was recorded; every arm must run uncapped")
    if run.get("session_open_limit_seconds") != SESSION_OPEN_SECONDS:
        raise PublicationError(f"{group_id}: the run's limit on opening a browser session differs from the protocol")
    timed(run, group_id)
    uncapped(run.get("agent_limits"), group_id)
    if HOSTED_MODELS.keys() & arms and run.get("hosted_max_cost_usd") != HOSTED_UNCAPPED_USD:
        raise PublicationError(f"{group_id}: the hosted arm's explicit uncapped max_cost_usd is not recorded")
    for arm in arms:
        browser_identity(run, arm, group_id)
    concurrency = run.get("concurrency")
    if isinstance(concurrency, bool) or not isinstance(concurrency, int) or concurrency < 1:
        raise PublicationError(f"{group_id}: concurrency is not recorded")
    return concurrency


def attempt_dollars(row: dict[str, Any], where: str) -> float | None:
    """A partial receipt survives a later crash, so its unknown flag decides whether the total is complete."""
    unknown = row.get("unknown_cost")
    if not isinstance(unknown, bool):
        raise PublicationError(f"{where}: unknown_cost must be recorded as a boolean")
    dollars = None if row.get("dollars") is None else amount(row["dollars"], f"{where}: dollars")
    if not unknown and dollars is None:
        raise PublicationError(f"{where}: a known charge requires dollars")
    if row.get("started") is False and (unknown or dollars != 0.0):
        raise PublicationError(f"{where}: a refused start must record a known zero charge")
    if unknown and row["arm"] in FASTBROWSE_ARMS:
        raise PublicationError(f"{where}: a Fastbrowse attempt has an unknown charge")
    return None if unknown else dollars


def start_gate(
    journal: list[dict[str, Any]],
    physical: dict[str, dict[str, Any]],
    run: dict[str, Any],
    where: str,
    execution: dict[str, execution_receipts.ValidatedExecution] | None = None,
):
    """Replay an internal campaign's budget journal against its recorded budget and hold. Each run held its agent,
    browser and judge reserves, and settled at its agent spend plus every priced judge attempt."""
    hold = run["reserve_dollars"] + run["browser_reserve_dollars"] + run["judge_reserve_dollars"]
    charges = {}
    for key, row in physical.items():
        dollars = attempt_dollars(row, where)
        if row.get("started") is False:
            # Its provider refused to start it, so nothing ran or was charged and the ledger settled it at zero.
            charges[key] = 0.0
            continue
        if key in (execution or {}):
            charges[key] = hold
            continue
        judge_known, judge_unknown = tracking.SemanticReceipt.model_validate(row.get("semantic_judge")).judgment.cost()
        if judge_unknown:
            raise PublicationError(f"{where}: unpriced judge attempts leave settlement unknown")
        if dollars is None:
            # A later usage query cannot prove billing finality, so the original hold remains committed.
            charges[key] = hold
            continue
        charges[key] = dollars + judge_known
    replay_start_gate(journal, charges, run["budget_usd"], hold, where)


def replay_start_gate(journal: list[dict[str, Any]], physical: dict[str, Any], limit: Any, hold: float, where: str):
    """Replay the budget journal: every physical run reserved once, before it started, only while committed spend
    plus its hold fit the approved total, and settled once at its actual charge (`physical`, by run). A charge above
    its hold is allowed (it limited nothing); a start the gate should have refused is not."""
    if len(journal) != 2 * len(physical):
        raise PublicationError(f"{where}: every physical run requires one reservation and settlement")
    charges: dict[str, float] = {}
    active: set[str] = set()
    for receipt in journal:
        key = receipt.get("id")
        if (
            not isinstance(key, str)
            or key not in physical
            or receipt.get("limit") != limit
            or receipt.get("policy") != POLICY
            or not math.isclose(amount(receipt.get("reservation"), "journal hold"), hold, abs_tol=1e-9)
        ):
            raise PublicationError(f"{where}: budget journal protocol differs from the campaign")
        charge = amount(receipt.get("committed"), "journal charge")
        if key not in charges:
            if not math.isclose(charge, hold, abs_tol=1e-9) or math.fsum(charges.values()) + hold > limit + 1e-9:
                raise PublicationError(f"{where}: a physical run started that the budget gate should have refused")
            active.add(key)
        elif key in active:
            if not math.isclose(charge, physical[key], abs_tol=1e-9):
                raise PublicationError(f"{where}: settled journal charge differs from its physical run")
            active.remove(key)
        else:
            raise PublicationError(f"{where}: duplicated budget journal settlement")
        charges[key] = charge
    if active or charges.keys() != physical.keys():
        raise PublicationError(f"{where}: every physical run requires its own settlement")


def internal_campaign(results: Path, unmeasured: Collection[str] = ()) -> Any:
    """The evidence campaign `fastbrowse-evals browser track-ledger` projects, built from the exact ledger bytes, without the lines
    of `unmeasured` arms (`internal_group`), with its semantic receipts and each attempt's raw ledger row."""
    from fastbrowse.evals import evidence

    def kept(line: str) -> bool:
        row = json.loads(line)
        return (row["id"].split("/", 1)[0] if "id" in row else row.get("arm")) not in unmeasured

    with tempfile.TemporaryDirectory(prefix="candidate-") as directory:
        folder = Path(directory) / "docs" / "results"
        folder.mkdir(parents=True)
        for source in (results, results.with_suffix(".attempts.jsonl"), results.with_suffix(".budget.jsonl")):
            if source == results or source.exists():
                body = source.read_text().splitlines(keepends=True)
                (folder / source.name).write_text("".join(line for line in body if not line.strip() or kept(line)))
        campaign = evidence.campaign(folder / results.name, Path(directory), kind="published")
        receipts = tracking.semantic_receipts(campaign, Path(directory))
        return campaign, receipts, tracking.ledger_rows(campaign, Path(directory))


def measured_arms(group_id: str, run: dict[str, Any]) -> set[str]:
    """The arms a group measures: its own pair, or for the hosted group whichever of `HOSTED_ARMS` its one invocation
    ran. An arm that did not run is simply absent; one outside the group refuses it."""
    if group_id != "internal-hosted":
        return GROUPS[group_id][3]
    ran = run.get("arms")
    if not isinstance(ran, dict) or not ran or not set(ran) <= set(HOSTED_ARMS):
        raise PublicationError(f"{group_id}: the run's arms must be some of {', '.join(HOSTED_ARMS)}")
    return set(ran)


def ledger_slots(results: Path, group_id: str) -> tuple[dict[str, Any], dict[str, set[tuple[Any, Any]]]]:
    """The invocation a ledger's first result records, and the (arm, repeat) slots its results hold per task: enough
    to tell which tasks it measured in full before the whole ledger is checked."""
    rows = lines(results)
    run = rows[0].get("run")
    if not isinstance(run, dict):
        raise PublicationError(f"{group_id}: run identity is missing")
    held: dict[str, set[tuple[Any, Any]]] = defaultdict(set)
    for row in rows:
        held[str(row.get("task"))].add((row.get("arm"), row.get("repeat")))
    return run, held


def measured_in_full(
    run: dict[str, Any], held: dict[str, set[tuple[Any, Any]]], required: dict[str, Any]
) -> set[tuple[str, str]]:
    """The (arm, task) pairs of the catalog for which the ledger holds the arm at every repeat of the task."""
    recorded = run.get("arms")
    arms = recorded if isinstance(recorded, dict) else {}
    return {
        (arm, task)
        for task, slots in held.items()
        if task in required
        for arm in arms
        if arm in required[task]["arms"] and {(arm, repeat) for repeat in range(INTERNAL_REPEATS)} <= slots
    }


SITE_BLOCKED = "blocked by the site's bot check"
"""The failure of a slot with no verdict whose agent ended `blocked`: the site refused it, which another agent may get
past."""
SITE_REFUSED = "evidence unavailable after the agent reached the site"
"""The failure of a slot with no verdict whose evidence the judge found unavailable, on a run that did not end in an
`error` of the harness: the ledger cannot tell a site's refusal from ours, so it counts against the agent."""


def unjudged_reason(attempt: dict[str, Any]) -> Literal["evidence-unavailable", "no-verdict"] | None:
    """Why the selected attempt with no verdict measured nothing (`UnjudgedSlot`), or None when it is the agent's
    failure. A run that ended `blocked` reached the site and was refused by its bot check, which another agent may get
    past, so it is a failure. Evidence the judge found unavailable measured nothing only when the run ended in an
    `error` of the harness or its model provider; with any other ending the ledger cannot tell a site's refusal from
    ours, so it is a failure too. A judge with no verdict is ours alone."""
    if attempt.get("status") == "blocked":
        return None
    if judged_unavailable(attempt.get("semantic_judge")):
        return "evidence-unavailable" if attempt.get("status") == "error" else None
    return "no-verdict"


@dataclass(frozen=True)
class Invocation:
    """One invocation's ledger, checked whole, and the slots of the tasks the group takes from it."""

    run: dict[str, Any]
    arms: frozenset[str]
    unmeasured: frozenset[str]
    concurrency: int
    rows: list[Row]
    unjudged: list[UnjudgedSlot]
    claims: frozenset[tuple[str, str]]
    """The (arm, task) pairs the group takes from this invocation."""
    models: dict[str, set[str]]
    reported: dict[str, set[str]]
    """Each arm's models and text models as its attempts reported them, for the arm's `Source`."""
    text_models: set[str]
    modes: dict[str, str]
    policies: set[str]
    tracked: Any
    physical_runs: int


def invocation(
    group_id: str,
    results: Path,
    catalog: dict[str, Any],
    required: dict[str, Any],
    use: set[tuple[str, str]],
    execution_paths: Sequence[Path] = (),
) -> Invocation:
    """Check one invocation's ledger exactly as a single-run candidate is checked, its protocol, budget journal and
    judge receipts whole, and return the slots of the (arm, task) pairs in `use`, which it measured in full."""
    other = INTERNAL[group_id]
    task_arms = GROUPS[group_id][3]
    attempts = lines(results.with_suffix(".attempts.jsonl"))
    selected_rows = lines(results)

    def recorded_run(row: dict[str, Any]) -> dict[str, Any]:
        recorded = row.get("run")
        if not isinstance(recorded, dict):
            raise PublicationError(f"{group_id}: run identity is missing")
        uncapped(recorded.get("agent_limits"), f"{group_id}/{row.get('arm')}/{row.get('task')}")
        if other != "browser-use" and task_arms <= FASTBROWSE_ARMS:
            text(row.get("model"), f"{group_id}: choice model")
            text(row.get("text_model"), f"{group_id}: text model")
        return recorded

    runs = {json.dumps(recorded_run(r), sort_keys=True) for r in [*attempts, *selected_rows]}
    if len(runs) != 1 or not isinstance(attempts[0].get("run"), dict):
        raise PublicationError(f"{group_id}: every row must come from one recorded invocation")
    run: dict[str, Any] = attempts[0]["run"]
    ran = measured_arms(group_id, run)
    concurrency = internal_protocol(run, group_id, ran, catalog, {arm for arm, _ in use})
    # A hosted arm none of whose runs started (its provider refused every start: a bad key, say) measured nothing. It
    # is left out, as an arm that never ran is, rather than blocking the arms that did; one start refused among runs
    # that did start still blocks, as any failed slot does.
    unmeasured = (
        {arm for arm in ran if all(a.get("started") is False for a in attempts if a.get("arm") == arm)}
        if group_id == "internal-hosted"
        else set()
    )
    arms = ran - unmeasured
    if not arms:
        raise PublicationError(f"{group_id}: no arm started a run")
    # An arm none of whose runs started measured nothing here, so this run supplies none of its tasks.
    use = {(arm, task) for arm, task in use if arm in arms}
    left_out = {f"{a['arm']}/{a['task']}/{a['repeat']}/{a['retries']}" for a in attempts if a["arm"] in unmeasured}
    attempts = [a for a in attempts if a["arm"] not in unmeasured]
    selected_rows = [r for r in selected_rows if r.get("arm") not in unmeasured]
    sha(run.get("git_sha"), "agent commit")
    sha(run.get("benchmark_runner_sha"), "runner")
    if run.get("git_dirty") is not False or run.get("benchmark_runner_dirty") is not False:
        raise PublicationError(f"{group_id}: the agent build and the runner must both be clean")

    slots: dict[tuple[str, str, int], list[dict[str, Any]]] = defaultdict(list)
    for attempt in attempts:
        task, repeat = attempt.get("task"), attempt.get("repeat")
        where = f"{group_id}/{attempt.get('arm')}/{task}/{repeat}"
        if (
            attempt.get("arm") not in arms
            or task not in required
            or isinstance(repeat, bool)
            or not isinstance(repeat, int)
            or not 0 <= repeat < INTERNAL_REPEATS
        ):
            raise PublicationError(f"{where}: an attempt outside the approved task, agent and repeat grid")
        if attempt.get("concurrency") != concurrency:
            raise PublicationError(f"{where}: concurrency differs from the protocol")
        if isinstance(attempt.get("task_version"), bool) or not isinstance(attempt.get("task_version"), int):
            raise PublicationError(f"{where}: the task version is not recorded")
        if isinstance(attempt.get("retries"), bool) or not isinstance(attempt.get("retries"), int):
            raise PublicationError(f"{where}: the retry index is not recorded")
        amount(attempt.get("seconds"), f"{where}: seconds")
        timed(attempt, where)
        attempt_dollars(attempt, where)
        slots[str(attempt["arm"]), str(task), repeat].append(attempt)
    if math.fsum(a["dollars"] for a in attempts if a["dollars"] is not None) > run["budget_usd"] + 1e-9:
        raise PublicationError(f"{group_id}: physical spend exceeds its approved campaign budget")
    expected = {(arm, task, repeat) for arm, task in use for repeat in range(INTERNAL_REPEATS)}
    if not expected <= slots.keys():
        raise PublicationError(f"{group_id}: the ledger does not cover every agent and repeat of the tasks it supplies")

    physical = {f"{a['arm']}/{a['task']}/{a['repeat']}/{a['retries']}": a for a in attempts}
    if len(physical) != len(attempts):
        raise PublicationError(f"{group_id}: every physical run requires one reservation and settlement")
    journal = [r for r in lines(results.with_suffix(".budget.jsonl")) if r.get("id") not in left_out]
    from fastbrowse_evals.live import RUNNABLE, prompt

    try:
        execution = execution_receipts.load(
            execution_paths, results, {t.id: prompt(t) for tasks in RUNNABLE.values() for t in tasks}
        )
        execution_receipts.coverage(physical, execution)
    except ValueError as exc:
        raise PublicationError(
            f"{group_id}: every physical attempt needs its complete semantic judge receipt or validated non-execution coverage"
        ) from exc
    start_gate(journal, physical, run, group_id, execution)

    by_slot = {(r["arm"], r["task"], r["repeat"]): r for r in selected_rows}
    if len(by_slot) != len(selected_rows) or by_slot.keys() != slots.keys():
        raise PublicationError(f"{group_id}: results do not hold one row per slot")
    rows = []
    unjudged = []
    for (arm, task, repeat), tries in slots.items():
        tries.sort(key=lambda a: int(a["retries"]))
        where = f"{group_id}/{arm}/{task}/{repeat}"
        if [a.get("retries") for a in tries] != list(range(len(tries))):
            raise PublicationError(f"{where}: physical attempts are not a complete retry sequence")
        if [a.get("selected") for a in tries] != [False] * (len(tries) - 1) + [True]:
            raise PublicationError(f"{where}: exactly the final attempt must be the selected one")
        chosen = tries[-1]
        fields = (*SLOT_FIELDS, "run", "model", "text_model") if arms <= FASTBROWSE_ARMS else (*SLOT_FIELDS, "run")
        if any(chosen.get(k) != by_slot[arm, task, repeat].get(k) for k in fields):
            raise PublicationError(f"{where}: the results row differs from its selected attempt")
        if (arm, task) not in use:
            continue
        if chosen["task_version"] > required[task]["task_version"]:
            raise PublicationError(f"{where}: task version {chosen['task_version']} is newer than the catalog's")
        if chosen["task_version"] < required[task]["task_version"]:
            # Measured on an earlier version of a task that has changed since, as the release gate notes it: not
            # compared, so listed apart until a run of the current version supplies the task.
            unjudged.append(UnjudgedSlot(task=task, repeat=repeat, arm=arm, reason="task-changed"))  # ty: ignore[invalid-argument-type]
            continue
        verdict, failure = chosen.get("passed"), None
        if not isinstance(verdict, bool):
            reason = unjudged_reason(chosen)
            if reason is not None:
                # Our harness or judge failed, so the slot measured nothing: listed apart, never a pass or a failure.
                unjudged.append(UnjudgedSlot(task=task, repeat=repeat, arm=arm, reason=reason))  # ty: ignore[invalid-argument-type]
                continue
            verdict, failure = False, SITE_BLOCKED if chosen.get("status") == "blocked" else SITE_REFUSED
        status = chosen.get("status") or chosen.get("normalized_status")
        rows.append(
            Row(
                task=task,
                repeat=repeat,
                arm=arm,  # ty: ignore[invalid-argument-type]
                status=text(status, f"{where}: status"),
                completed=status == "complete" if arm in FASTBROWSE_ARMS else chosen.get("normalized_status") == "done",
                score=1.0 if verdict else 0.0,
                failure=failure,
                seconds=math.fsum(a["seconds"] for a in tries),
                dollars=(None if any(a["unknown_cost"] for a in tries) else math.fsum(a["dollars"] for a in tries)),
                physical_runs=len(tries),
                **call_counts(tries),
                jev_calls=sum(a.get("jev_calls") or 0 for a in tries),
                tokens_per_second=chosen.get("tokens_per_second") or 0.0,
                input_tokens=sum(a.get("input_tokens") or 0 for a in tries),
                output_tokens=sum(a.get("output_tokens") or 0 for a in tries),
                **timing_fields(tries, execution),
            )
        )

    campaign, receipts, ledger = internal_campaign(results, unmeasured)
    if len(receipts) + len(execution) != len(campaign.attempts):
        raise PublicationError(f"{group_id}: every physical attempt needs its complete semantic judge receipt")
    if any(receipt.judgment.policy != JUDGE_POLICY for receipt in receipts.values()):
        raise PublicationError(f"{group_id}: published figures are graded under {JUDGE_POLICY} ({JUDGE_MODEL} alone)")
    return Invocation(
        run=run,
        arms=frozenset(arms),
        unmeasured=frozenset(unmeasured),
        concurrency=concurrency,
        rows=rows,
        unjudged=unjudged,
        claims=frozenset(use),
        models={arm: {a["model"] for a in attempts if a["arm"] == arm and a.get("model")} for arm in arms},
        reported={
            arm: {a[key] for a in attempts if a["arm"] == arm for key in ("model", "text_model") if a.get(key)}
            for arm in arms
        },
        text_models={a["text_model"].strip() for a in attempts if isinstance(a.get("text_model"), str)},
        modes={arm: browser_identity(run, arm, group_id) for arm in arms},
        policies={receipt.policy_sha256 for receipt in receipts.values()},
        tracked=tracking.project(
            campaign,
            Suite.INTERNAL,
            receipts=receipts,
            rows=ledger,
            execution={
                a.id: execution[execution_receipts.key(ledger[a.id])]
                for a in campaign.attempts
                if execution_receipts.key(ledger[a.id]) in execution
            },
        ),
        physical_runs=len(attempts),
    )


def internal_group(
    group_id: str,
    results: Path | Sequence[Path],
    catalog: dict[str, Any],
    digest: str,
    execution_paths: Sequence[Path] = (),
) -> Collected:
    """One internal group from the ledgers of one or more runs of a campaign, as the release gate scores it: each
    arm's rows at a task from the newest run that measured that arm at every repeat of it. So the head-to-head may
    take fastbrowse from a newer fastbrowse-only run, as the release gate pools them, while each competitor keeps its
    own run; the runs must agree on each arm's browser, and every run is held to the one uncapped protocol and clock.
    A run that supplies nothing is not read further; every run that does is checked whole."""
    other = INTERNAL[group_id]
    task_arms = GROUPS[group_id][3]
    required = {t["id"]: t for suite in catalog["suites"].values() for t in suite if task_arms <= set(t["arms"])}
    if len(required) != GROUPS[group_id][0]:
        raise PublicationError(f"{group_id}: the catalog does not define the approved matched task set")
    paths = [results] if isinstance(results, Path) else list(results)
    found = [(*ledger_slots(path, group_id), path) for path in paths]
    found.sort(key=lambda item: text(item[0].get("run_started"), f"{group_id}: run start"), reverse=True)
    if len({json.dumps(run, sort_keys=True) for run, _, _ in found}) != len(found):
        raise PublicationError(f"{group_id}: one run's ledger was given twice")
    used: list[Invocation] = []
    claimed: set[tuple[str, str]] = set()
    for run, held, path in found:
        fresh = measured_in_full(run, held, required) - claimed
        if fresh:
            item = invocation(group_id, path, catalog, required, fresh, execution_paths)
            used.append(item)
            claimed |= item.claims
    used = [item for item in used if item.claims]
    arms = frozenset(arm for arm, _ in claimed)
    supported = {arm: [task for task, entry in required.items() if arm in entry["arms"]] for arm in arms}
    missing = sorted(f"{arm}/{task}" for arm in arms for task in supported[arm] if (arm, task) not in claimed)
    if not arms or missing:
        raise PublicationError(
            f"{group_id}: the ledger does not cover every task, agent and repeat slot; no run measured "
            f"{', '.join(missing[:5]) or 'any task'}{' and more' if len(missing) > 5 else ''} in full"
        )
    # An arm drove one browser in every run supplying the group, so a newer run cannot move the comparison onto another.
    modes: dict[str, str] = {}
    for item in used:
        for arm in item.arms:
            if modes.setdefault(arm, item.modes[arm]) != item.modes[arm]:
                raise PublicationError(f"{group_id}: its runs drove {arm} on different browsers; rerun them alike")
    policies = set().union(*(item.policies for item in used))
    if len(policies) != 1:
        raise PublicationError(f"{group_id}: publication requires one matched semantic judge policy")
    unmeasured = set().union(*(item.unmeasured for item in used)) - arms
    unpriced = {row.arm for item in used for row in item.rows if row.dollars is None}
    # A newer partial invocation must not turn the older priced slots into a complete arm total.
    rows = [
        row.model_copy(update={"dollars": None}) if row.arm in unpriced else row for item in used for row in item.rows
    ]

    def each(values: Iterable[Any]) -> str:
        return " or ".join(f"{value:g}" for value in sorted(set(values)))

    holds = (
        item.run["reserve_dollars"] + item.run["browser_reserve_dollars"] + item.run["judge_reserve_dollars"]
        for item in used
    )
    present = [arm for arm in HOSTED_ARMS if arm in arms]
    # One phrase within the public contract's 240 characters for all five head-to-head arms: each arm's count of tasks
    # it cannot run, every one a failure, follows the list of arms once.
    skipped = [f"{arm} {len(required) - len(supported[arm])}" for arm in present if len(supported[arm]) < len(required)]
    hosted = (
        f"Agent against agent on one set of {len(required)} tasks: {', '.join(present)}"
        + (f"; failed where it cannot run: {', '.join(skipped)}" if skipped else "")
        + "".join(f"; {arm} not measured, no run started" for arm in sorted(unmeasured))
    )
    agents = {
        None: "Fastbrowse only; competitor results are separate references",
        "browser-use": hosted,
        "browser-use-ultrafast": "Fastbrowse on its own browser pool against the hosted Browser Use agent on bu-ultrafast",
        "fastbrowse-pool": "Backend against backend: Fastbrowse on the Browser Use Cloud browser and on its own pool",
    }[other]
    limits = [
        "Internal live task suite, not an official Browser Use benchmark",
        agents,
        UNCAPPED_NOTES[0],
        f"Concurrency {each(item.concurrency for item in used)}; budget {each(item.run['budget_usd'] for item in used)} "
        f"dollars, each new run holding {each(holds)} until settled",
        "Browser per arm: " + "; ".join(f"{arm} {mode}" for arm, mode in sorted(modes.items())),
    ]
    if HOSTED_MODELS.keys() & arms:
        limits.append(f"Hosted Browser Use max_cost_usd set to {HOSTED_UNCAPPED_USD} to override its provider default")
    if BROWSER_USE_MODES & set(modes.values()):
        limits.append(UNCAPPED_NOTES[1])
    limits += [
        "Each slot totals physical runs including retries; an unresolved charge withholds its arm's complete cost",
        MIXED_TIMING_NOTE if any(row.timing_basis == "physical-attempt-wall-time" for row in rows) else TIMING_NOTE,
        f"Score is the recorded {JUDGE_MODEL.replace('/', ' ')} verdict; completion is the agent's own ending, recorded separately",
    ]
    models = {arm: sorted(set().union(*(item.models[arm] for item in used if arm in item.models))) for arm in arms}
    # One note for every arm's models keeps a pairing within the public contract's twelve notes.
    recorded = [f"{arm} {', '.join(found)}" for arm, found in sorted(models.items()) if found]
    if recorded:
        limits.append("Recorded models: " + "; ".join(recorded))
    if other is None:
        text_models = sorted(set().union(*(item.text_models for item in used)))
        limits += [f"Recorded text models for fastbrowse: {model}" for model in text_models]
    limits.append(f"Semantic judge policy {next(iter(policies))}")
    if len(limits) > 12:
        raise PublicationError(f"{group_id}: more protocol notes than the public contract holds")
    sources = [
        Source(
            run_id=text(item.run.get("run_id"), f"{group_id}: run id"),
            arm=arm,  # ty: ignore[invalid-argument-type]
            agent_sha=item.run["git_sha"],
            runner_sha=item.run["benchmark_runner_sha"],
            measured_on=item.run["run_started"][:10],
            tasks=sorted(task for claimed_arm, task in item.claims if claimed_arm == arm),
            # A fastbrowse arm's pin names where its build is recorded; the source's agent commit is that build.
            version=None if arm in FASTBROWSE_ARMS else note(item.run["arms"][arm]["pin"]),
            models=sorted({note(model) for model in item.reported.get(arm, ())}),
        )
        for item in used
        for arm in sorted({a for a, _ in item.claims})
    ]
    group = build_group(
        group_id,
        digest,
        list(required),
        limits,
        rows,
        used[0].run,
        unjudged=sorted((slot for item in used for slot in item.unjudged), key=lambda s: (s.arm, s.task, s.repeat)),
        sources=sources,
        excluded=exclusions(arms, required, catalog),
    )
    newest = used[0].run
    return Collected(
        group,
        newest["git_sha"],
        newest["benchmark_runner_sha"],
        tuple(item.tracked for item in used),
        sum(item.physical_runs for item in used),
        newest["run_started"],
    )


# --- official runs ---

OFFICIAL = {"bu-bench": (Suite.BU_BENCH, "browser-use benchmark"), "online-mind2web": (Suite.MIND2WEB, "Mind2Web")}
CONFIG_EXTRAS = {
    "config_sha256",
    "total_budget",
    "published",
    "run",
    "directly_comparable",
    "comparison_limits",
}


@dataclass(frozen=True)
class OfficialSource:
    """A private official run directory and the task source it claims; the source is read, not trusted."""

    folder: Path
    dataset: Path
    upstream_root: Path | None = None
    judge_python: Path | None = None


def settled_judge_usd(folder: Path, billing: dict[str, Any]) -> float:
    """Judge dollars from the private receipts, which must all be settled: a reservation is an unknown charge."""
    path = folder / "receipts.jsonl"
    if not path.is_file():
        raise PublicationError(f"{folder.name}: a graded run has no judge receipts")
    reserved: set[int] = set()
    settled = 0.0
    for row in official.read_rows(path):
        if row["event"] == "reserved":
            reserved.add(row["n"])
        elif row["event"] == "settled":
            if row["n"] not in reserved or any(
                isinstance(row.get(key), bool) or not isinstance(row.get(key), int) or row[key] < 0
                for key in ("input_tokens", "output_tokens")
            ):
                raise PublicationError(f"{folder.name}: a judge charge has no reconciled usage receipt")
            reserved.remove(row["n"])
            try:
                charge = judge_charge(billing, row["input_tokens"], row["output_tokens"])
            except (BillingError, KeyError) as exc:
                raise PublicationError(f"{folder.name}: invalid judge pricing") from exc
            if not math.isclose(charge, amount(row["usd"], f"{folder.name}: judge charge"), abs_tol=1e-12):
                raise PublicationError(f"{folder.name}: judge charge differs from its token receipt")
            settled += charge
    if reserved:
        raise PublicationError(f"{folder.name}: a judge charge is unsettled")
    return settled


class OfficialRows(BaseModel):
    """One arm's official run as the public may see it, written where the private run directory is readable.

    The run directory is age-encrypted because it holds BU Bench's decrypted goals and upstream records, so the
    job that ran it checks it (`official_group`) and hands on only this: opaque task ids, scores, seconds, dollars and
    the protocol facts. Fields are closed, so nothing else can travel in it. `publish-candidate` adds it as its suite's
    group."""

    model_config = ConfigDict(extra="forbid", strict=True)

    group: Literal["bu-bench", "online-mind2web"]
    arm: Literal["fastbrowse"]
    measured_on: str
    measured_on: str
    agent_sha: str
    runner_sha: str
    dataset_sha256: str
    task_ids: list[str]
    repeats: int
    upstream_commit: str
    judge_model: str
    judge_reasoning: str
    score_threshold: int
    judge_versions: dict[str, str]
    judge_pricing_note: str | None
    screenshot_timing: str
    missing_captures: int | None
    concurrency: int
    browser: str
    authorized: bool
    langfuse_dataset: str
    langfuse_run: str
    rows: list[Row]
    unjudged: list[UnjudgedSlot] = []


def check_blocked(result: dict[str, Any], folder: Path, where: str) -> None:
    """A slot is blocked only on its evidence: every retry spent, the last check finding the site at fault, and nothing
    sent to the upstream judge. Any other ungraded slot refuses publication."""
    if (
        len(result.get("interruptions", [])) != OUTAGE_RETRIES
        or not judged_unavailable(result.get("interruption_check"))
        or result.get("upstream_score") is not None
        or (folder / "receipts.jsonl").exists()
    ):
        raise PublicationError(f"{where}: a slot blocked by the site must show every retry and its last check")


async def load_grades(args: argparse.Namespace, records: dict[str, dict[str, str]]) -> dict[str, Any]:
    return await official.bridge(args, {"suite": args.suite, "operation": "regrade", "records": records}, "regrade")


async def official_group(
    group_id: str,
    source: OfficialSource,
    load_tasks: Callable[[argparse.Namespace], Awaitable[list[dict[str, Any]]]] = official.tasks_for,
    grades: Callable[[argparse.Namespace, dict[str, dict[str, str]]], Awaitable[dict[str, Any]]] = load_grades,
) -> tuple[OfficialRows, Any]:
    """Check one arm's private run against its task source and saved evidence; returns its public rows and the
    TrackedRun the private Langfuse project must hold."""
    suite, label = OFFICIAL[group_id]
    manifest = json.loads((source.folder / "manifest.json").read_text())
    if manifest.get("sample") is not None:
        raise PublicationError(f"{group_id}: a sampled run is a smoke test and is never published")
    identity = official_tracking.SourceIdentity.model_validate(manifest)
    repeats = manifest.get("repeats")
    if (
        manifest.get("suite") != suite.value
        or manifest.get("published") is not False
        or isinstance(repeats, bool)
        or not isinstance(repeats, int)
        or repeats < 1
    ):
        raise PublicationError(f"{group_id}: the run is not a private {label} run with at least one repeat")
    arm = manifest.get("arm", "fastbrowse")
    if arm != "fastbrowse":
        raise PublicationError(f"{group_id}: {arm} cannot be published, only fastbrowse's official runs are")
    if manifest.get("judge_model") != JUDGE_MODEL or manifest.get("judge_base_url") != OPENROUTER_BASE_URL:
        raise PublicationError(f"{group_id}: a published grade comes from {JUDGE_MODEL} through OpenRouter alone")
    config = {k: v for k, v in manifest.items() if k not in CONFIG_EXTRAS}
    if official.config_fingerprint(config) != identity.config_sha256:
        raise PublicationError(f"{group_id}: the manifest differs from its recorded configuration fingerprint")
    run = manifest["run"]
    sha(run.get("git_sha"), "agent commit")
    sha(run.get("benchmark_runner_sha"), "runner")
    if (
        run.get("git_dirty") is not False
        or run.get("benchmark_runner_dirty") is not False
        or run.get("benchmark_origin") != "upstream"
        or run.get("benchmark_suite") != suite.value
    ):
        raise PublicationError(f"{group_id}: the agent build and runner must be clean and identify the upstream suite")
    versions = manifest.get("judge_versions")
    if not isinstance(versions, dict) or not versions or not manifest.get("judge_billing"):
        raise PublicationError(f"{group_id}: judge versions and billing identity are required")
    text(manifest.get("judge_pricing_checked"), f"{group_id}: judge pricing check date")
    policies = {official.FRAME_TIMING[suite.value]}
    if suite == Suite.BU_BENCH:
        policies.update(official.LEGACY_BU_FRAME_TIMINGS)
    if manifest.get("screenshot_timing") not in policies:
        raise PublicationError(f"{group_id}: screenshot timing differs from the suite's protocol")

    namespace = argparse.Namespace(
        suite=suite,
        dataset=source.dataset,
        dataset_sha256=identity.dataset_sha256,
        upstream_root=source.upstream_root,
        upstream_commit=identity.upstream_commit,
        judge_python=source.judge_python,
        out=source.folder,
    )
    actual = [t["task_id"] for t in await load_tasks(namespace)]
    if actual != manifest["task_ids"]:
        raise PublicationError(f"{group_id}: the manifest's task ids differ from the actual task source")

    attempts, results = official.attempt_state(source.folder)
    expected = {(task, repeat) for task in actual for repeat in range(repeats)}
    if attempts.keys() != expected or results.keys() != expected:
        raise PublicationError(f"{group_id}: attempts and results must cover every task and repeat exactly once")
    graded = sorted(key for key in expected if not official.blocked_by_site(results[key]))
    saved_grades = await grades(
        namespace, {f"{t}-{r}": {"task": t, "folder": str(source.folder / f"{t}-{r}")} for t, r in graded}
    )
    # The same uncapped protocol as the internal groups: no agent limit, the budget only gating new tasks.
    if (
        manifest.get("budget_policy") != POLICY
        or amount(manifest.get("total_budget"), f"{group_id}: total budget") <= 0
        or any(
            key not in manifest or manifest[key] is not None for key in ("max_steps", "task_timeout", "agent_budget")
        )
    ):
        raise PublicationError(f"{group_id}: the uncapped {POLICY} protocol is required")
    if manifest.get("session_open_limit_seconds") != SESSION_OPEN_SECONDS:
        raise PublicationError(f"{group_id}: the limit on opening a browser session differs from the protocol")
    timed(manifest, group_id)
    uncapped(manifest.get("agent_limits"), group_id)
    for key, attempt in attempts.items():
        uncapped(attempt.get("agent_limits"), f"{group_id}/{key[0]}")
        if results[key].get("agent_limits") != attempt.get("agent_limits"):
            raise PublicationError(f"{group_id}/{key[0]}: result and attempt agent limits differ")
    rows = []
    unjudged: list[UnjudgedSlot] = []
    physical: dict[str, float] = {}
    missing_captures = 0
    for (task, repeat), result in sorted(results.items()):
        where = f"{group_id}/{task}/{repeat}"
        if result.get("agent_cost_unknown") is not False:
            raise PublicationError(f"{where}: the agent charge is unknown")
        folder = source.folder / f"{task}-{repeat}"
        is_blocked = official.blocked_by_site(result)
        if is_blocked:
            check_blocked(result, folder, where)
        elif manifest["screenshot_timing"] == official.FRAME_TIMING[suite.value]:
            available = result.get("final_capture_available")
            if not isinstance(available, bool) or available != (folder / "final.png").is_file():
                raise PublicationError(f"{where}: final capture availability differs from the saved artifacts")
            missing_captures += not available
        saved = official.saved_agent(folder)
        if not saved or any(
            result.get(k) != saved.get(k)
            for k in ("status", "completed", "agent_dollars", "agent_cost_unknown", "seconds", "timing_basis")
        ):
            raise PublicationError(f"{where}: agent metrics differ from the saved artifacts")
        grade = saved_grades.get(f"{task}-{repeat}")
        if not is_blocked and (
            not isinstance(grade, dict)
            or result.get("upstream_score") != grade.get("score")
            or (result.get("upstream_passed") != grade.get("passed"))
        ):
            raise PublicationError(f"{where}: source grade differs from the saved judge response")
        judge = 0.0 if is_blocked else settled_judge_usd(folder, manifest["judge_billing"])
        if not math.isclose(judge, amount(result.get("judge_usd"), f"{where}: judge dollars"), abs_tol=1e-9):
            raise PublicationError(f"{where}: judge dollars differ from the private receipts")
        seconds = amount(result.get("seconds"), f"{where}: timing")
        timed(result, where)
        dollars = amount(result.get("agent_dollars"), f"{where}: agent dollars")
        # The row publishes what the agent spent, as an internal row does; the start gate also held the judging.
        final_run = dollars + judge
        final_run += amount(result.get("interruption_check_usd", 0.0), f"{where}: interruption check dollars")
        # Runs the interruption check sent back (`official.Interrupted`) are physical runs of this slot: their time
        # and spend count, as an internal slot's retries do.
        interruptions = result.get("interruptions", [])
        for n, earlier in enumerate(interruptions):
            kept = source.folder / f"{task}-{repeat}.interrupted-{n}"
            if earlier.get("folder") != kept.name or earlier.get("agent_cost_unknown") is not False:
                raise PublicationError(f"{where}: interrupted run {n} is not a complete, priced record")
            if (kept / "receipts.jsonl").exists() or earlier.get("judge_usd") != 0:
                raise PublicationError(f"{where}: interrupted run {n} was sent to the upstream judge")
            saved = official.saved_agent(kept)
            if not saved or any(earlier.get(k) != saved.get(k) for k in ("status", "agent_dollars", "seconds")):
                raise PublicationError(f"{where}: interrupted run {n} differs from its saved artifacts")
            seconds += amount(earlier.get("seconds"), f"{where}: interrupted run {n} timing")
            timed(earlier, f"{where}: interrupted run {n}")
            spent = amount(earlier.get("agent_dollars"), f"{where}: interrupted run {n} agent dollars")
            dollars += spent
            check = amount(earlier.get("interruption_check_usd"), f"{where}: interrupted run {n} check dollars")
            physical[official.run_id(task, repeat, n)] = spent + check
        physical[official.run_id(task, repeat, len(interruptions))] = final_run
        status = text(result.get("status"), f"{where}: status")
        if is_blocked and status == "error":
            # The run ended in an error of the harness or its model provider, so it measured nothing, as an internal
            # slot judged unavailable on that ending measured nothing (`unjudged_reason`).
            unjudged.append(UnjudgedSlot(task=task, repeat=repeat, arm=arm, reason="evidence-unavailable"))
            continue
        rows.append(
            Row(
                task=task,
                repeat=repeat,
                arm=arm,
                status=status,
                completed=result.get("completed") is True,
                # A slot the site refused on every retry is the agent's failure, as an internal one is: another agent
                # may get past what stopped this one.
                score=0.0 if is_blocked else amount(result.get("upstream_score"), f"{where}: upstream grade"),
                seconds=seconds,
                dollars=dollars,
                physical_runs=1 + len(interruptions),
                llm_calls=None,
                llm_calls_basis="physical_requests" if arm in FASTBROWSE_ARMS else None,
                **timing_fields([*interruptions, result]),
                failure=(SITE_BLOCKED if status == "blocked" else SITE_REFUSED) if is_blocked else None,
            )
        )
    if math.fsum(physical.values()) > manifest["total_budget"] + 1e-9:
        raise PublicationError(f"{group_id}: physical spend exceeds its approved campaign budget")
    # Total spend alone cannot show which holds overlapped, so the start gate's own journal is replayed as well.
    hold = manifest["reserve_dollars"] + manifest["browser_reserve_dollars"] + manifest["judge_billing"]["budget_usd"]
    if any(not math.isclose(amount(a.get("reserved_usd"), "hold"), hold, abs_tol=1e-9) for a in attempts.values()):
        raise PublicationError(f"{group_id}: an attempt's hold differs from the recorded reservation")
    try:
        journal = [json.loads(line) for line in (source.folder / official.JOURNAL).read_text().splitlines()]
    except (OSError, ValueError):
        raise PublicationError(f"{group_id}: the budget journal is missing or unreadable") from None
    replay_start_gate(journal, physical, manifest["total_budget"], hold, group_id)
    billing = manifest["judge_billing"]
    tracked = official_tracking.project(source.folder)
    public = OfficialRows(
        group=group_id,  # ty: ignore[invalid-argument-type]
        arm=arm,
        measured_on=text(run.get("run_started"), f"{group_id}: run start")[:10],
        agent_sha=run["git_sha"],
        runner_sha=run["benchmark_runner_sha"],
        dataset_sha256=identity.dataset_sha256,
        task_ids=actual,
        repeats=repeats,
        upstream_commit=identity.upstream_commit,
        judge_model=identity.judge_model,
        judge_reasoning=identity.judge_reasoning,
        score_threshold=identity.score_threshold,
        judge_versions={k: str(v) for k, v in versions.items()},
        judge_pricing_note=(
            f"Judge requests above {billing['context_threshold_tokens']} input tokens cost "
            f"{billing['context_input_usd_per_mtok']:g} input and {billing['context_output_usd_per_mtok']:g} "
            "output dollars per million tokens for the whole request"
            if "context_threshold_tokens" in billing
            else None
        ),
        screenshot_timing=manifest["screenshot_timing"],
        missing_captures=(
            missing_captures if manifest["screenshot_timing"] == official.FRAME_TIMING[suite.value] else None
        ),
        concurrency=manifest["concurrency"],
        browser=manifest["agent_runtime"]["browser"]["mode"],
        authorized=manifest["authorize_irreversible_actions"],
        langfuse_dataset=tracked.dataset,
        langfuse_run=tracked.runs[0].name,
        rows=rows,
        unjudged=unjudged,
    )
    return public, tracked


def official_limits(group_id: str, first: OfficialRows) -> list[str]:
    """The group's public protocol notes."""
    suite, label = OFFICIAL[group_id]
    compared = f"Fastbrowse only; published comparator figures are references, not reruns of {label}"
    meaning = (
        "Score is the upstream weighted rubric grade, never a pass rate"
        if suite == Suite.BU_BENCH
        else "Score is the upstream full task grade; completion is the agent's own ending"
    )
    screenshots = f"Screenshot evidence: {first.screenshot_timing}"
    if first.missing_captures is not None:
        total = first.repeats * len(first.task_ids)
        screenshots += f"; runs without a final capture of {total}: {first.missing_captures}"
    limits = [
        meaning,
        compared,
        f"Judge {first.judge_model.replace('/', ' ')} via OpenRouter, reasoning {first.judge_reasoning}; "
        f"score threshold {first.score_threshold}",
        f"{first.repeats} repeat(s) per task; a task's figure is the mean of its graded repeat scores; "
        "a slot the site refused on every retry is a failed attempt, unless the run ended in a harness error",
        f"Upstream commit {first.upstream_commit}",
        "Judge libraries: " + ", ".join(f"{k} {v}" for k, v in sorted(first.judge_versions.items())),
        screenshots,
        f"{UNCAPPED_NOTES[0]}; concurrency: {first.concurrency}",
        f"Browser: {first.browser}; irreversible actions authorized: {first.authorized}",
        f"{TIMING_NOTE}; dollars are agent spend excluding judging",
    ]
    if first.judge_pricing_note:
        limits.append(first.judge_pricing_note)
    if first.browser in BROWSER_USE_MODES:
        limits.append(UNCAPPED_NOTES[1])
    return limits


def official_collected(part: OfficialRows) -> Collected:
    """One public group from its run's public rows."""
    run = {"git_sha": part.agent_sha, "benchmark_runner_sha": part.runner_sha}
    limits = official_limits(part.group, part)
    group = build_group(
        part.group, part.dataset_sha256, part.task_ids, limits, part.rows, run, part.repeats, part.unjudged
    )
    return Collected(group, part.agent_sha, part.runner_sha, (), len(part.rows) + len(part.unjudged))


# --- Langfuse readback ---


def confirm_langfuse(client: Any, project: Any, tracked: Any) -> None:
    """Every experiment and item of the projection is in the private project with identical stored output."""
    if project.name != PROJECT:
        raise PublicationError(f"Langfuse readback must use the {PROJECT} project, not {project.name!r}")
    try:
        existing = langfuse_runs.experiment_ids(client, client.get_dataset(tracked.dataset).id)
    except Exception as exc:
        raise PublicationError(f"{tracked.dataset}: not readable in Langfuse ({type(exc).__name__})") from exc
    for run in tracked.runs:
        if run.name not in existing or not langfuse_runs.recorded_in_full(client, existing[run.name], run):
            raise PublicationError(
                f"{run.name}: not fully uploaded; run `fastbrowse-evals browser track-ledger --push` first"
            )
        # The stored output also carries the item's evidence zip, which the projection's output model ignores.
        stored = {
            item.experiment_item_id: (
                ItemInput.model_validate(sink.decode(item.input)),
                ItemOutput.model_validate(sink.decode(item.output)),
            )
            for item in sink.experiment_items(client, existing[run.name], "core,io")
        }
        if stored != {t.item_id: (t.input, t.output) for t in run.traces}:
            raise PublicationError(f"{run.name}: stored items differ from the exact source rows and physical runs")


def confirm_official(client: Any, project: Any, part: OfficialRows) -> None:
    """The run the official job read back is still in the private project. Its plaintext stays in that job, which
    compared every item (`confirm_langfuse`) before it wrote the rows."""
    if project.name != PROJECT:
        raise PublicationError(f"Langfuse readback must use the {PROJECT} project, not {project.name!r}")
    try:
        existing = langfuse_runs.experiment_ids(client, client.get_dataset(part.langfuse_dataset).id)
    except Exception as exc:
        raise PublicationError(f"{part.langfuse_dataset}: not readable in Langfuse ({type(exc).__name__})") from exc
    if part.langfuse_run not in existing:
        raise PublicationError(
            f"{part.langfuse_run}: not in Langfuse; run `fastbrowse-evals browser track-official --push`"
        )


# --- fixtures and the runner ---


def is_ancestor(commit: str, head: str) -> bool:
    done = subprocess.run(
        ["git", "merge-base", "--is-ancestor", commit, head],
        cwd=Path(__file__).parents[2],
        capture_output=True,
        timeout=30,
    )
    return done.returncode == 0


def runner_pinned(commit: str) -> None:
    if not is_ancestor(commit, "HEAD"):
        raise PublicationError("the runner commit is not part of this checkout's history")


@dataclass(frozen=True)
class Checks:
    """External evidence checks; tests substitute them, the CLI uses the defaults."""

    readback: Callable[[Any], None]
    official_readback: Callable[[OfficialRows], None]
    fixture: Callable[[Candidate], None] = check_fixture
    runner: Callable[[str], None] = runner_pinned


def default_checks() -> tuple[Checks, Callable[[], None]]:
    client, project = sink.connect(write=False)
    checks = Checks(
        readback=lambda tracked: confirm_langfuse(client, project, tracked),
        official_readback=lambda part: confirm_official(client, project, part),
    )
    return checks, client.shutdown


# --- export ---


REFERENCE_ARMS = {
    "browser-use": "Browser Use",
    "browser-use-ultrafast": "Browser Use Ultrafast",
    "browser-harness": "Browser Harness",
}
"""Competitors a campaign shows from the fixed reference. Only a `refresh_competitors` dispatch runs them live."""


def competitor_reference(candidate: Candidate, catalog: dict[str, Any], measured_on: str) -> CompetitorReference:
    """The rows a `refresh_competitors` campaign leaves for later campaigns to reuse, as they were published."""
    group = next((g for g in candidate.groups if g.id == "internal-hosted"), None)
    missing = [arm for arm in REFERENCE_ARMS if group is None or not any(r.arm == arm for r in group.rows)]
    if group is None or missing:
        raise PublicationError(f"a competitor reference needs every competitor's rows: {', '.join(missing)} not run")
    rows = [r for r in group.rows if r.arm in REFERENCE_ARMS]
    versions = {t["id"]: t["task_version"] for suite in catalog["suites"].values() for t in suite}
    return CompetitorReference(
        catalog_sha256=candidate.catalog_sha256,
        judge_policy=JUDGE_POLICY,
        task_versions={r.task: versions[r.task] for r in rows},
        arms=[
            ReferenceArm(
                arm=arm,  # ty: ignore[invalid-argument-type]
                vendor=vendor,
                measured_on=measured_on,
                version=note(catalog["arms"][arm]["pin"]),
                browser=note(next(iter(BROWSER_MODES[arm]))),
                campaign_id=candidate.campaign_id,
                models=sorted({model for source in group.sources if source.arm == arm for model in source.models}),
            )
            for arm, vendor in REFERENCE_ARMS.items()
        ],
        rows=rows,
    )


def with_reference(item: Collected, path: Path | None, catalog: dict[str, Any]) -> Collected:
    """The hosted group with the competitors' rows beside its own. A group that ran them live keeps its own. A reference
    that is missing, judged under another policy, missing its clock or stale for any task refuses the comparison: nothing is left out."""
    group = item.group
    ran = {r.arm for r in group.rows} & REFERENCE_ARMS.keys()
    if ran == REFERENCE_ARMS.keys():
        return item
    if ran:
        raise PublicationError(f"{group.id}: a refresh must run every competitor, not only {', '.join(sorted(ran))}")
    if path is None or not path.is_file():
        raise PublicationError(
            f"{group.id}: no competitor reference; dispatch the campaign once with refresh_competitors"
        )
    reference = CompetitorReference.model_validate_json(path.read_bytes())
    if reference.judge_policy != JUDGE_POLICY:
        raise PublicationError(
            f"{group.id}: the reference is graded under {reference.judge_policy}, not {JUDGE_POLICY}"
        )
    if any(reference_basis(row) is None for row in reference.rows):
        raise PublicationError(
            f"{group.id}: the reference has an unrecorded clock; dispatch the campaign once with refresh_competitors"
        )
    tasks = {t["id"]: t for suite in catalog["suites"].values() for t in suite}
    stale = sorted(
        {
            f"{r.arm}/{r.task}"
            for r in reference.rows
            if r.task not in tasks or reference.task_versions[r.task] != tasks[r.task]["task_version"]
        }
        | {
            f"{arm}/{task}"
            for arm in REFERENCE_ARMS
            for task, entry in tasks.items()
            if arm in entry["arms"]
            and {r.repeat for r in reference.rows if (r.arm, r.task) == (arm, task)} != set(range(INTERNAL_REPEATS))
        }
    )
    if stale:
        raise PublicationError(
            f"{group.id}: the competitor reference no longer matches the catalog on {', '.join(stale)}; "
            "dispatch the campaign once with refresh_competitors"
        )
    arms = {r.arm for r in group.rows} | {a.arm for a in reference.arms}
    rows = sorted([*group.rows, *reference.rows], key=lambda r: (r.arm, r.task, r.repeat))
    limits = group.limits
    if any(row.timing_basis == "physical-attempt-wall-time" for row in rows):
        limits = [MIXED_TIMING_NOTE if entry == TIMING_NOTE else entry for entry in limits]
    merged = group.model_copy(
        update={
            "rows": rows,
            "limits": limits,
            "references": reference.arms,
            "excluded": exclusions(arms, group.task_ids, catalog),
        }
    )
    return dataclasses.replace(item, group=Group.model_validate(merged.model_dump(mode="json")))


Ledgers = Path | Sequence[Path] | None
"""One run's ledger of a group, or the ledgers of several runs of one campaign (`internal_group`)."""


def _ledgers(ledgers: Ledgers) -> list[Path]:
    return [] if ledgers is None else [ledgers] if isinstance(ledgers, Path) else list(ledgers)


def export(
    *,
    campaign_id: str,
    catalog: Path,
    hosted: Ledgers,
    ultrafast: Ledgers,
    official_rows: Collection[OfficialRows] = (),
    fixture_run: int,
    checks: Checks,
    internal_fastbrowse: Ledgers = None,
    backend: Ledgers = None,
    reference: Path | None = None,
    execution_paths: Sequence[Path] = (),
) -> Candidate:
    raw = catalog.read_bytes()
    if raw != render().encode():
        raise PublicationError("the catalog is not the canonical export of the current internal tasks")
    if raw != installed_catalog():
        raise PublicationError("the measured Fastbrowse build ships a different benchmark catalog than these tasks")
    digest = hashlib.sha256(raw).hexdigest()
    catalog_data = json.loads(raw)

    official_groups = [part.group for part in official_rows]
    if len(official_groups) != len(set(official_groups)):
        raise PublicationError("two official runs of one suite; pass one")
    collected = [
        *[
            internal_group(name, source, catalog_data, digest, execution_paths)
            for name, source in (
                ("internal-fastbrowse", internal_fastbrowse),
                # fastbrowse's rows at a head-to-head task may come from a newer fastbrowse-only run, as the release
                # gate pools them; each competitor keeps its own run.
                ("internal-hosted", [*_ledgers(hosted), *_ledgers(internal_fastbrowse)] if hosted else None),
                ("internal-ultrafast", ultrafast),
                ("internal-backend", backend),
            )
            if source
        ],
        *[official_collected(part) for part in official_rows],
    ]
    if not collected:
        raise PublicationError("at least one complete benchmark group is required")
    collected = [
        with_reference(c, reference, catalog_data) if c.group.id == "internal-hosted" else c for c in collected
    ]
    # The candidate is of the newest run's build: the fixtures must pass on it and an official group must be of it.
    # An internal group names the build of each of its runs. A run on an earlier build stays valid whatever branch that
    # build was on: a squash merge leaves a measured branch commit outside main's history without changing what it
    # measured, and each source keeps its own commits and date.
    internal = [c for c in collected if c.started is not None]
    newest = max(internal, key=lambda c: c.started or "") if internal else collected[0]
    agent_sha, runner_sha = newest.agent_sha, newest.runner_sha
    if any((c.agent_sha, c.runner_sha) != (agent_sha, runner_sha) for c in collected if c.started is None):
        raise PublicationError("every group must come from one clean agent build and one clean runner")
    checks.runner(runner_sha)
    for item in collected:
        for tracked in item.tracked:
            checks.readback(tracked)
    for part in official_rows:
        checks.official_readback(part)
    # The receipt claims every fixture slot passed; `checks.fixture` holds it to the run's own rows.
    slots = len(fixture_slots())
    candidate = Candidate(
        # A contract whose rows record their timing basis writes its own schema, which from 7 pairs fastbrowse-pool
        # with browser-use-ultrafast; 5 paired it with jev-ultrafast, and 4 is the uncapped start-gate protocol with
        # the internal-backend group.
        schema_version=timed_schema(5 if ultrafast else 4 if backend else 3 if internal_fastbrowse else 2),
        langfuse_verified=True,
        campaign_id=campaign_id,
        agent_sha=agent_sha,
        runner_sha=runner_sha,
        catalog_sha256=digest,
        fixture=Fixture(head_sha=agent_sha, run_id=fixture_run, passed=slots, total=slots),
        groups=[c.group for c in collected],
    )

    checks.fixture(candidate)
    return candidate


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(prog="fastbrowse-evals browser publish-candidate", description=__doc__)
    parser.add_argument(
        "--execution-receipts", nargs="*", type=Path, default=[], help="private hash-bound non-execution sidecars"
    )
    parser.add_argument("--campaign-id", required=True)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--catalog", required=True, type=Path, help="the exported benchmark-catalog.json")
    # Each internal group takes the ledgers of one or more runs of the campaign, in any order: each task comes from
    # the newest run (by its recorded start) that measured it in full, as the release gate scores it.
    runs = " of one or more runs of the campaign"
    parser.add_argument(
        "--internal-fastbrowse",
        nargs="+",
        type=Path,
        help="Fastbrowse-only internal ledgers" + runs + " (published; no head-to-head)",
    )
    parser.add_argument(
        "--hosted",
        nargs="+",
        type=Path,
        help="agent against agent: results.jsonl" + runs + ", each running any of " + ", ".join(HOSTED_ARMS),
    )
    parser.add_argument(
        "--backend", nargs="+", type=Path, help="backend against backend: fastbrowse against fastbrowse-pool" + runs
    )
    parser.add_argument(
        "--ultrafast",
        nargs="+",
        type=Path,
        help="fastbrowse on its pool against BU Ultrafast: fastbrowse-pool against browser-use-ultrafast" + runs,
    )
    parser.add_argument(
        "--official-rows",
        nargs="*",
        default=[],
        type=Path,
        help="public rows from `fastbrowse-evals browser official-rows`, one file per suite and arm",
    )
    parser.add_argument(
        "--competitor-reference", type=Path, help="the checked-in reference holding the competitors' rows"
    )
    parser.add_argument(
        "--write-reference",
        type=Path,
        help="write the competitors' rows here, from a campaign that ran them; needs --measured-on",
    )
    parser.add_argument("--measured-on", help="the date a --write-reference campaign ran, YYYY-MM-DD")
    parser.add_argument("--fixture-run", required=True, type=int, help="GitHub run id of the passing fixture workflow")
    args = parser.parse_args(argv)
    if args.write_reference is not None and args.measured_on is None:
        parser.error("--write-reference needs --measured-on")
    selected = (args.internal_fastbrowse, args.hosted, args.ultrafast, args.backend, *args.official_rows)
    if not any(selected):
        parser.error("select at least one complete benchmark group")
    if args.out.exists():
        parser.error(f"{args.out} exists; remove it deliberately before exporting again")
    checks, close = default_checks()
    try:
        candidate = export(
            campaign_id=args.campaign_id,
            catalog=args.catalog,
            internal_fastbrowse=args.internal_fastbrowse,
            hosted=args.hosted,
            ultrafast=args.ultrafast,
            backend=args.backend,
            official_rows=[OfficialRows.model_validate_json(p.read_bytes()) for p in args.official_rows],
            fixture_run=args.fixture_run,
            checks=checks,
            reference=args.competitor_reference,
            execution_paths=args.execution_receipts,
        )
        if args.write_reference is not None:
            refreshed = competitor_reference(candidate, json.loads(args.catalog.read_bytes()), args.measured_on)
            args.write_reference.write_text(json.dumps(refreshed.model_dump(mode="json"), indent=2) + "\n")
    except ValueError as exc:
        raise SystemExit(f"publish-candidate refused: {exc}") from exc
    finally:
        close()
    content = (json.dumps(candidate.model_dump(mode="json"), indent=2) + "\n").encode()
    check_catalog(Candidate.model_validate_json(content), args.catalog)
    args.out.write_bytes(content)
    print(
        json.dumps(
            {
                "candidate": str(args.out),
                "sha256": hashlib.sha256(content).hexdigest(),
                "physical_runs": {c.id: sum(r.physical_runs for r in c.rows) for c in candidate.groups},
                "core_contract": "validated",
            }
        )
    )


if __name__ == "__main__":
    main(sys.argv[1:])
