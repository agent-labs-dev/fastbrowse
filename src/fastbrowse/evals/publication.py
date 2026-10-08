"""The gate a results file must clear before it becomes a published release.

A score is published only with what produced it, so this gate answers two separate questions and keeps their
answers apart, because they fail for different reasons:

- Integrity checks are deterministic. They read the candidate rows, the attempt ledger beside them and the task
  versions in `versions.json`, and refuse a row that cannot be traced to a clean build, a current task version, a
  current suite version, a dataset pin and every physical attempt it made. None of these depend on the numbers, so
  a failure reproduces from the same files.
- The regression check is bounded and matched. It compares fastbrowse against fastbrowse at the same suite,
  task and task version, only against the latest published release that ran them at the same protocol and model
  route, and it refuses a pass rate that declines, a live task with fewer than three distinct measured repeats
  even before a baseline exists, or a median wall time or cost that grows past its bound. It picks the newest
  matching release that itself has three attempts, so a thin newest release cannot hide an older full one; a
  task with no matching baseline, or a matching baseline too small to be a rate, is reported and never blocks
  once the candidate has its three repeats. Other arms and older protocols are never pooled into the comparison.

The gate is a pure function of the rows, the ledger, the baselines and the lock, so it runs in CI without a model
key and without a paid run:

    uv run python -m fastbrowse.evals.publication --rows artifacts/evals/nightly.jsonl --baseline docs/results

`fastbrowse.evals.versions.publish` calls the same gate with `require_ledger` set, so a release cannot be written
around it. `--check-diff BASE` validates the `docs/results` files a pull request adds or changes against the rows
at BASE, so a published row cannot be hand-edited past the gate.
"""

import argparse
import hashlib
import json
import math
import re
import statistics
import subprocess
import sys
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any, Literal, TypeGuard

from pydantic import BaseModel, ConfigDict

from fastbrowse.evals import baseline, versions
from fastbrowse.evals.storage import ArchiveReceipt, read_archive
from fastbrowse.models import Limits

REFUSE: Literal["refuse"] = "refuse"
"""A deterministic integrity failure: the rows cannot be published."""
REGRESS: Literal["regress"] = "regress"
"""A task-version matched regression past the baseline's lower bound: the comparison is not published."""
ADVISE: Literal["advise"] = "advise"
"""A noisy signal or a comparison that could not be made: reported, never blocking."""

REGRESSION_MIN_ATTEMPTS = 3
"""Below this a rate is a diagnostic, not a comparison: fewer repeats than this are never published as one."""
REGRESSION_FACTOR = 1.2
"""A candidate median over this much of the baseline's is a material movement."""
REGRESSION_SPEED_FLOOR_SECONDS = 1.0
"""The smallest wall-time rise, in seconds, that counts beside the relative bound."""
REGRESSION_COST_FLOOR_DOLLARS = 0.001
"""The smallest cost rise, in dollars, that counts beside the relative bound."""

_LEDGER_SUFFIX = ".attempts.jsonl"
_HEX64 = re.compile(r"^[a-f0-9]{64}$")
_HEX40 = re.compile(r"^[0-9a-f]{40}$")
_LIVE_SUITES = ("core", "dev", "heldout", "stretch-dev", "stretch-heldout")


class Finding(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")

    severity: Literal["refuse", "regress", "advise"]
    check: str
    detail: str

    @property
    def blocking(self) -> bool:
        return self.severity != ADVISE


class Operational(BaseModel):
    """What the attempts cost beyond the selected rows. Unknown stays unknown: a missing ledger is not zero."""

    model_config = ConfigDict(frozen=True, extra="forbid")

    selected_attempts: int
    ledger_attempts: int | None = None
    retries: int | None = None
    retry_seconds: float | None = None
    retry_dollars: float | None = None


class Report(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")

    findings: tuple[Finding, ...]
    operational: Operational

    @property
    def blocking(self) -> tuple[Finding, ...]:
        return tuple(finding for finding in self.findings if finding.blocking)

    def render(self) -> str:
        lines = [f"{finding.severity.upper():7} {finding.check}: {finding.detail}" for finding in self.findings]
        found = self.operational
        lines.append(
            f"OPERATIONAL {found.selected_attempts} selected attempts, "
            + (
                "ledger missing"
                if found.ledger_attempts is None
                else f"{found.ledger_attempts} physical attempts, {found.retries} retries, "
                f"{found.retry_seconds:.1f}s and "
                f"{'unknown' if found.retry_dollars is None else f'${found.retry_dollars:.4f}'} in retries"
            )
        )
        return "\n".join(lines)


def read_jsonl(path: Path) -> list[dict[str, Any]]:
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def ledger_path(rows_path: Path) -> Path:
    """Where the live harness writes the attempt ledger beside a results file (`x.jsonl` -> `x.attempts.jsonl`)."""
    return rows_path.with_suffix(_LEDGER_SUFFIX)


def baselines(path: Path) -> list[dict[str, Any]]:
    """Every published row under `path`: a `.jsonl` file, or every results `.jsonl` in a directory.

    The attempt ledgers live beside the results files and share the extension, so they are skipped: an attempt
    is not a baseline, and reading one as a row would count it twice.
    """
    if path.is_dir():
        return [row for _, rows in baseline.published(path) for row in rows]
    return [] if path.name.endswith(_LEDGER_SUFFIX) else read_jsonl(path)


def _git(root: Path, *args: str) -> str:
    done = subprocess.run(["git", *args], cwd=root, capture_output=True, text=True, timeout=60, check=False)
    if done.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)}: {done.stderr.strip()}")
    return done.stdout


def changed_results(base: str, root: Path = versions.ROOT) -> list[Path]:
    """The `docs/results` JSONL files a branch adds, changes or removes against `base`.

    `--no-renames` makes a rename a deletion and an addition, so a published file cannot be moved past the gate,
    and `D` is included so a deletion is seen rather than silently dropping a baseline.
    """
    compare = _merge_base(base, root)
    listing = _git(
        root, "diff", "--name-only", "--no-renames", "--diff-filter=AMD", compare, "HEAD", "--", "docs/results"
    )
    found: set[str] = set()
    for line in listing.splitlines():
        line = line.strip()
        if not line.endswith(".jsonl"):
            continue
        if line.endswith(_LEDGER_SUFFIX):
            found.add(line.removesuffix(_LEDGER_SUFFIX) + ".jsonl")
        else:
            found.add(line)
    return sorted(root / line for line in found)


def _merge_base(base: str, root: Path) -> str:
    try:
        return _git(root, "merge-base", "HEAD", base).strip() or base
    except RuntimeError:
        return base


def trusted_rows(base: str, root: Path = versions.ROOT) -> list[dict[str, Any]]:
    """Every published row at `base`, the trusted side of the comparison a branch cannot edit."""
    listing = _git(root, "ls-tree", "-r", "--name-only", base, "--", "docs/results")
    rows: list[dict[str, Any]] = []
    if f"docs/results/{baseline.NAME}" in listing.splitlines():
        rows.extend(
            row
            for entry in baseline.load(_git(root, "show", f"{base}:docs/results/{baseline.NAME}"))
            for row in entry.rows
        )
    for line in listing.splitlines():
        line = line.strip()
        if not line.endswith(".jsonl") or line.endswith(_LEDGER_SUFFIX):
            continue
        rows.extend(json.loads(text) for text in _git(root, "show", f"{base}:{line}").splitlines() if text.strip())
    return rows


def validate_diff(base: str, root: Path = versions.ROOT) -> list[Finding]:
    """What blocks the published rows and ledgers a branch adds or changes, checked against `base`'s rows.

    Historical files are not re-validated: a row published before this gate existed stays readable. A changed
    file is, and a new results file with no attempt ledger beside it is refused, so `--publish` cannot be
    bypassed by hand-editing `docs/results`.
    """
    base = _merge_base(base, root)
    changed = changed_results(base, root)
    findings: list[Finding] = []
    raw_validation = _git(root, "diff", "--name-only", "--diff-filter=AM", base, "--", "docs/validation")
    for name in raw_validation.splitlines():
        findings.append(_finding(REFUSE, "storage", f"{name}: detailed validation belongs in Langfuse"))
    archived = {item["path"]: item for item in read_archive(root / "docs/results/evidence.manifest.json")}
    try:
        previous_archives = json.loads(_git(root, "show", f"{base}:docs/results/evidence.manifest.archive.json"))
    except RuntimeError:
        previous_archives = []
    for item in previous_archives:
        receipt = ArchiveReceipt.model_validate(item).model_dump(mode="json")
        if archived.get(receipt["path"]) != receipt:
            findings.append(_finding(REFUSE, "history", f"{receipt['path']}: archive receipt changed or removed"))

    def preserved(name: str) -> bool:
        receipt = archived.get(name)
        if receipt is None:
            return False
        try:
            original = subprocess.run(
                ["git", "show", f"{base}:{name}"], cwd=root, capture_output=True, check=True, timeout=60
            ).stdout
        except subprocess.CalledProcessError:
            return False
        return (
            receipt.get("sha256") == hashlib.sha256(original).hexdigest()
            and receipt.get("bytes") == len(original)
            and bool(receipt.get("observation_ids"))
        )

    try:
        baseline.published(root / "docs/results")
        compact = {entry.source.path: entry for entry in baseline.read(root / "docs/results")}
    except ValueError as exc:
        return [*findings, _finding(REFUSE, "history", str(exc))]
    removed = _git(root, "diff", "--name-only", "--diff-filter=D", base, "--", "docs/results", "docs/validation")
    for name in removed.splitlines():
        if not preserved(name):
            findings.append(_finding(REFUSE, "history", f"{name}: removed evidence needs its original archive receipt"))
    try:
        previous_compact = baseline.load(_git(root, "show", f"{base}:docs/results/{baseline.NAME}"))
    except RuntimeError:
        previous_compact = []
    for entry in previous_compact:
        if compact.get(entry.source.path) != entry:
            findings.append(_finding(REFUSE, "history", f"{entry.source.path}: compact publication baseline changed"))
    for name, entry in compact.items():
        if any(old.source.path == name for old in previous_compact):
            continue
        try:
            original = _git(root, "show", f"{base}:{name}")
        except RuntimeError:
            findings.append(_finding(REFUSE, "history", f"{name}: a compact baseline needs already published rows"))
            continue
        if (
            hashlib.sha256(original.encode()).hexdigest() != entry.source.sha256
            or baseline.scores([json.loads(line) for line in original.splitlines() if line.strip()]) != entry.rows
        ):
            findings.append(_finding(REFUSE, "history", f"{name}: compact baseline differs from published rows"))
    trusted = trusted_rows(base, root)
    for path in changed:
        relative = path.relative_to(root).as_posix()
        release = path.stem
        if not path.exists():
            if relative in compact:
                continue
            if path.parent != root / "docs/results" and preserved(relative):
                continue
            findings.append(_finding(REFUSE, "history", f"{relative}: a published result file was removed"))
            continue
        rows = read_jsonl(path)
        try:
            before = _git(root, "show", f"{base}:{relative}")
        except RuntimeError:
            before = ""
        existing = {json.dumps(row, sort_keys=True) for row in rows}
        for old in (json.loads(text) for text in before.splitlines() if text.strip()):
            if json.dumps(old, sort_keys=True) not in existing:
                findings.append(_finding(REFUSE, "history", f"{relative}: a published row was changed or removed"))
        ledger_file = ledger_path(path)
        ledger = read_jsonl(ledger_file) if ledger_file.exists() else None
        # A published ledger is history too: an entry cannot be edited or dropped to make a retry or an outage
        # disappear, exactly as a published row cannot. Entries added by the branch are new history and allowed.
        base_ledger = relative.removesuffix(".jsonl") + _LEDGER_SUFFIX
        try:
            before_ledger = _git(root, "show", f"{base}:{base_ledger}")
        except RuntimeError:
            before_ledger = ""
        kept = {json.dumps(entry, sort_keys=True) for entry in (ledger or [])}
        for old in (json.loads(text) for text in before_ledger.splitlines() if text.strip()):
            if json.dumps(old, sort_keys=True) not in kept:
                findings.append(
                    _finding(REFUSE, "history", f"{relative}: an attempt ledger entry was changed or removed")
                )
        baseline_rows = [row for row in trusted if _published_release(row) != release]
        findings += list(
            gate(rows, release=release, ledger=ledger, baselines_=baseline_rows, require_ledger=True).blocking
        )
    return findings


def _number(value: object) -> TypeGuard[int | float]:
    return isinstance(value, int | float) and not isinstance(value, bool) and math.isfinite(value)


def _where(row: Mapping[str, Any]) -> str:
    return f"{row.get('arm')} {row.get('task')}"


def _finding(severity: Literal["refuse", "regress", "advise"], check: str, detail: str) -> Finding:
    return Finding(severity=severity, check=check, detail=detail)


def _suite_ids() -> dict[str, tuple[str, ...]]:
    """Every suite this build knows, so a row's suite version can be checked against the tasks it ran."""
    from fastbrowse.evals.catalog import SUITES
    from fastbrowse.evals.mock_tasks import TASKS as MOCK
    from fastbrowse.evals.tasks import TASKS as LOCAL

    known = {name: tuple(task.id for task in tasks) for name, tasks in SUITES.items()}
    known["local"] = tuple(task.id for task in LOCAL)
    known["mock"] = tuple(task.id for task in MOCK)
    return known


def _shape(row: Mapping[str, Any]) -> list[Finding]:
    where = _where(row)
    problems = []
    for field in ("arm", "task", "category", "suite", "suite_version"):
        if not isinstance(row.get(field), str) or not row.get(field):
            problems.append(_finding(REFUSE, "shape", f"{where}: missing or empty {field}"))
    if not isinstance(row.get("task_version"), int) or isinstance(row.get("task_version"), bool):
        problems.append(_finding(REFUSE, "shape", f"{where}: task_version is not an integer"))
    if not _number(row.get("seconds")) or row["seconds"] < 0:
        problems.append(_finding(REFUSE, "shape", f"{where}: seconds is not a finite non-negative number"))
    if row.get("dollars") is not None and (not _number(row.get("dollars")) or row["dollars"] < 0):
        problems.append(_finding(REFUSE, "shape", f"{where}: dollars is neither null nor a finite non-negative number"))
    for field in ("passed", "correct"):
        if not isinstance(row.get(field), bool):
            problems.append(_finding(REFUSE, "shape", f"{where}: {field} is not a boolean"))
    if row.get("passed") is True and row.get("correct") is not True:
        problems.append(_finding(REFUSE, "grade", f"{where}: passed a row whose grade is not correct"))
    return problems


def _remaining_budget(run: Mapping[str, Any]) -> bool:
    try:
        limits = Limits.model_validate(run.get("agent_limits"), strict=True)
    except ValueError:
        return False
    budget, reserve = run.get("budget_usd"), run.get("browser_reserve_dollars")
    if not _number(budget) or not _number(reserve) or budget <= 0 or reserve < 0:
        return False
    browser = run.get("browser")
    return (
        run.get("concurrency") == 1
        and type(run.get("concurrency")) is int
        and all(run.get(name) is None for name in ("max_steps", "max_seconds", "max_dollars"))
        and run.get("agent_limits") == limits.model_dump(mode="json")
        and all(
            getattr(limits, name) is None for name in ("max_steps", "max_seconds", "max_llm_calls", "max_jev_calls")
        )
        and limits.max_dollars is not None
        and math.isfinite(limits.max_dollars)
        and limits.max_dollars + reserve <= budget
        and (reserve > 0 or (isinstance(browser, Mapping) and browser.get("mode") == "local-chrome"))
    )


def _provenance(row: Mapping[str, Any], *, release: str | None, require_clean: bool, live: bool) -> list[Finding]:
    where = _where(row)
    run = row.get("run")
    if not isinstance(run, Mapping):
        return [_finding(REFUSE, "provenance", f"{where}: no run provenance")]
    problems = []
    if not isinstance(run.get("run_id"), str) or not run["run_id"]:
        problems.append(_finding(REFUSE, "provenance", f"{where}: no run id"))
    if not isinstance(run.get("run_started"), str) or not run["run_started"]:
        problems.append(_finding(REFUSE, "provenance", f"{where}: no run date"))
    if require_clean:
        sha = run.get("git_sha")
        if not isinstance(sha, str) or not sha:
            problems.append(_finding(REFUSE, "build", f"{where}: not from a committed tree"))
        elif not _HEX40.fullmatch(sha):
            # A ref name, an option or an abbreviation is not a build: only a full commit sha can be diffed
            # against the release and resolved to the tree the measurement ran.
            problems.append(_finding(REFUSE, "build", f"{where}: git_sha {sha!r} is not a full 40-hex commit"))
        elif run.get("git_dirty") is not False:
            problems.append(_finding(REFUSE, "build", f"{where}: not from a clean tree"))
    if release is not None and run.get("fastbrowse_version") != release:
        problems.append(
            _finding(REFUSE, "build", f"{where}: ran fastbrowse {run.get('fastbrowse_version')}, not {release}")
        )
    if not run.get("providers"):
        problems.append(_finding(REFUSE, "provenance", f"{where}: no model route recorded"))
    if live:
        from fastbrowse.evals.catalog import CATALOG_SHA256

        if run.get("benchmark_family") != "browser-use" or run.get("benchmark_origin") != "fastbrowse-internal":
            problems.append(_finding(REFUSE, "catalog", f"{where}: benchmark origin is not the internal task set"))
        if run.get("benchmark_catalog_sha256") != CATALOG_SHA256:
            problems.append(_finding(REFUSE, "catalog", f"{where}: private benchmark catalog does not match"))
        runner_sha = run.get("benchmark_runner_sha")
        if not isinstance(runner_sha, str) or not _HEX40.fullmatch(runner_sha):
            problems.append(_finding(REFUSE, "catalog", f"{where}: no committed benchmark runner identity"))
        if require_clean and run.get("benchmark_runner_dirty") is not False:
            problems.append(_finding(REFUSE, "catalog", f"{where}: benchmark runner is not clean"))
        arms = run.get("arms")
        if not isinstance(arms, Mapping) or row.get("arm") not in arms:
            problems.append(_finding(REFUSE, "provenance", f"{where}: arm is not pinned in the run"))
        elif not isinstance((arms[row["arm"]] or {}).get("pin"), str) or not arms[row["arm"]]["pin"]:
            problems.append(_finding(REFUSE, "provenance", f"{where}: arm has no pin"))
        if run.get("budget_policy") == "remaining-campaign-v1":
            if not _remaining_budget(run):
                problems.append(_finding(REFUSE, "protocol", f"{where}: invalid remaining-campaign limits"))
        elif not isinstance(run.get("max_steps"), int) or run["max_steps"] <= 0:
            problems.append(_finding(REFUSE, "protocol", f"{where}: no positive step limit"))
        if not isinstance(run.get("concurrency"), int) or run["concurrency"] <= 0:
            problems.append(_finding(REFUSE, "protocol", f"{where}: no positive concurrency"))
    return problems


def _version(row: Mapping[str, Any], lock: Mapping[str, Mapping[str, Any]], suites: Mapping[str, tuple[str, ...]]):
    where = _where(row)
    task = str(row.get("task"))
    problems = []
    current = versions.task_version(task, lock)
    if current is None:
        problems.append(_finding(REFUSE, "version", f"{where}: no task by that id is in versions.json"))
    elif row.get("task_version") != current:
        problems.append(
            _finding(REFUSE, "version", f"{where}: task version {row.get('task_version')} is not the current {current}")
        )
    ids = suites.get(str(row.get("suite")))
    if ids is None:
        # A row that names a suite this build does not know cannot be checked against its tasks, its version or
        # the arm pins that make it comparable, so it is refused rather than reported and published.
        problems.append(_finding(REFUSE, "version", f"{where}: suite {row.get('suite')} is not defined in this build"))
    else:
        expected = versions.suite_version(ids, lock)
        if row.get("suite_version") != expected:
            problems.append(
                _finding(REFUSE, "version", f"{where}: suite version {row.get('suite_version')} is not {expected}")
            )
    return problems


def _protocol(row: Mapping[str, Any]) -> list[Finding]:
    """A navigation pass must rest on a document the harness read, at the comparison viewport."""
    from pydantic import ValidationError

    from fastbrowse.evals.live_tasks import PageEvidence, page_defect
    from fastbrowse.evals.observe import VIEWPORT

    where = _where(row)
    if row.get("category") != "navigate":
        return []
    problems = []
    if row.get("passed"):
        try:
            defect = page_defect(PageEvidence.model_validate(row.get("final_page")))
        except ValidationError:
            defect = "missing or invalid final-page evidence"
        if defect:
            problems.append(_finding(REFUSE, "protocol", f"{where}: {defect}"))
    if row.get("arm") in {"fastbrowse", "jev-ultrafast"} and row.get("normalized_status") != "unavailable":
        try:
            page = PageEvidence.model_validate(row.get("final_page"))
            dimensions = (page.inner_width, page.inner_height, page.device_pixel_ratio)
        except ValidationError:
            dimensions = (None, None, None)
        if (
            dimensions[:2] != (VIEWPORT["width"], VIEWPORT["height"])
            or dimensions[2] is None
            or not math.isclose(dimensions[2], VIEWPORT["deviceScaleFactor"], rel_tol=1e-6)
        ):
            problems.append(_finding(REFUSE, "protocol", f"{where}: navigation viewport is missing or differs"))
    return problems


def _cost(row: Mapping[str, Any]) -> list[Finding]:
    """A total is a total: an unknown or unmetered run must not carry a number, and a null total must say unknown."""
    where = _where(row)
    unknown = row.get("unknown_cost")
    dollars = row.get("dollars")
    problems = []
    if unknown is True and dollars is not None:
        problems.append(_finding(REFUSE, "cost", f"{where}: cost is unknown but a total is claimed"))
    if unknown is False and dollars is None:
        problems.append(_finding(REFUSE, "cost", f"{where}: cost is null but not marked unknown"))
    if isinstance(row.get("unmetered_requests"), int) and row["unmetered_requests"] > 0 and dollars is not None:
        problems.append(_finding(REFUSE, "cost", f"{where}: unmetered requests but a total is claimed"))
    return problems


def _dataset(row: Mapping[str, Any]) -> list[Finding]:
    """A corpus attempt names the pinned bytes it came from; a pin that does not match the source is not a run."""
    from fastbrowse.evals.sources import SOURCES

    if "corpus" not in row and "task_digest" not in row:
        return []
    where = _where(row)
    problems = []
    source = SOURCES.get(str(row.get("source")))
    if source is None:
        problems.append(_finding(REFUSE, "dataset", f"{where}: unknown dataset source"))
        return problems
    if row.get("revision") != source.revision:
        problems.append(_finding(REFUSE, "dataset", f"{where}: revision is not the pinned {source.revision}"))
    if source.sha256 is not None and row.get("sha256") != source.sha256:
        problems.append(_finding(REFUSE, "dataset", f"{where}: sha256 is not the pinned {source.sha256}"))
    for field in ("corpus", "task_digest"):
        if not isinstance(row.get(field), str) or not _HEX64.match(row[field]):
            problems.append(_finding(REFUSE, "dataset", f"{where}: {field} is not a sha256 digest"))
    return problems


def _attempt_shape(entry: Mapping[str, Any]) -> list[Finding]:
    """An attempt's own numbers must be as honest as the row's: finite, non-negative, and unknown stays unknown."""
    where = _where(entry)
    problems = []
    if "seconds" in entry and (not _number(entry.get("seconds")) or entry["seconds"] < 0):
        problems.append(
            _finding(REFUSE, "ledger", f"{where}: an attempt's seconds is not a finite non-negative number")
        )
    if (
        "dollars" in entry
        and entry.get("dollars") is not None
        and (not _number(entry.get("dollars")) or entry["dollars"] < 0)
    ):
        problems.append(
            _finding(
                REFUSE, "ledger", f"{where}: an attempt's dollars is neither null nor a finite non-negative number"
            )
        )
    unknown = entry.get("unknown_cost")
    if unknown is True and entry.get("dollars") is not None:
        problems.append(_finding(REFUSE, "ledger", f"{where}: an attempt's cost is unknown but a total is claimed"))
    if unknown is False and "dollars" in entry and entry.get("dollars") is None:
        problems.append(_finding(REFUSE, "ledger", f"{where}: an attempt's cost is null but not marked unknown"))
    if (
        isinstance(entry.get("unmetered_requests"), int)
        and entry["unmetered_requests"] > 0
        and entry.get("dollars") is not None
    ):
        problems.append(
            _finding(REFUSE, "ledger", f"{where}: an attempt has unmetered requests but a total is claimed")
        )
    return problems


def _attempt_provenance(entry: Mapping[str, Any], row: Mapping[str, Any]) -> list[Finding]:
    """Every physical attempt ran the row's build, route and arm pins; a retry is not a different measurement."""
    where = _where(row)
    run = row.get("run") or {}
    entry_run = entry.get("run") or {}
    problems = []
    for field in ("run_id", "git_sha", "git_dirty", "providers"):
        if field in run and entry_run.get(field) != run.get(field):
            detail = (
                "a physical attempt is from another run"
                if field == "run_id"
                else f"a physical attempt has a different {field}"
            )
            problems.append(_finding(REFUSE, "ledger", f"{where}: {detail}"))
    arms = run.get("arms")
    if isinstance(arms, Mapping) and entry_run.get("arms") != arms:
        problems.append(_finding(REFUSE, "ledger", f"{where}: a physical attempt has different arm pins"))
    return problems


def _selected_matches(entry: Mapping[str, Any], row: Mapping[str, Any]) -> list[Finding]:
    """The retained attempt is the row: its time, cost and grade cannot differ from what was published."""
    where = _where(row)
    problems = []
    for field in ("seconds", "dollars", "unknown_cost", "correct", "passed"):
        if field in row and field in entry and entry.get(field) != row.get(field):
            problems.append(_finding(REFUSE, "ledger", f"{where}: the selected attempt's {field} differs from the row"))
    return problems


def _ledger(
    rows: Sequence[Mapping[str, Any]], ledger: Sequence[Mapping[str, Any]] | None, *, require_ledger: bool
) -> tuple[list[Finding], Operational]:
    """Reconcile every selected row with every physical attempt beside it, including the retries a score hides."""
    selected_attempts = len(rows)
    retried = sum(int(row.get("retries") or 0) for row in rows)
    if ledger is None:
        problems = []
        if require_ledger:
            problems.append(_finding(REFUSE, "ledger", "no attempt ledger: every physical attempt must be retained"))
        elif retried:
            problems.append(_finding(REFUSE, "ledger", f"rows report {retried} retries but no attempt ledger exists"))
        return problems, Operational(selected_attempts=selected_attempts, retries=retried)
    problems = []
    index: dict[tuple[object, object, object], list[Mapping[str, Any]]] = {}
    for entry in ledger:
        index.setdefault((entry.get("arm"), entry.get("task"), entry.get("repeat")), []).append(entry)
    known: set[tuple[object, object, object]] = set()
    for row in rows:
        where = _where(row)
        key = (row.get("arm"), row.get("task"), row.get("repeat"))
        # Three copies of one passing row are not three repeats, so a duplicate key is refused rather than
        # allowed to inflate a rate that a single attempt measured.
        if key in known:
            problems.append(_finding(REFUSE, "ledger", f"{where}: repeat {row.get('repeat')} appears more than once"))
            continue
        known.add(key)
        if row.get("repeat") is None:
            problems.append(_finding(REFUSE, "ledger", f"{where}: no repeat to match its attempts to"))
            continue
        entries = index.get(key)
        if not entries:
            problems.append(_finding(REFUSE, "ledger", f"{where}: no ledger entry for this attempt"))
            continue
        chosen = [entry for entry in entries if entry.get("selected") is True]
        if len(chosen) != 1:
            problems.append(_finding(REFUSE, "ledger", f"{where}: {len(chosen)} selected attempts, expected one"))
            continue
        expected = int(row.get("retries") or 0) + 1
        if len(entries) != expected:
            problems.append(
                _finding(
                    REFUSE, "ledger", f"{where}: ledger holds {len(entries)} physical attempts, row reports {expected}"
                )
            )
        counts = sorted(int(entry.get("retries") or 0) for entry in entries)
        if counts != list(range(expected)):
            problems.append(_finding(REFUSE, "ledger", f"{where}: ledger retry numbering is {counts}"))
        selected = chosen[0]
        for field in ("status", "normalized_status"):
            if field in row and selected.get(field) != row.get(field):
                problems.append(_finding(REFUSE, "ledger", f"{where}: ledger {field} differs from the row"))
        problems += _selected_matches(selected, row)
        # Every physical attempt is checked, not only the retained one: a hidden retry from another build or run
        # would otherwise disappear into the operational total without anyone seeing it was not this row.
        for entry in entries:
            problems += _attempt_shape(entry)
            problems += _attempt_provenance(entry, row)
    # An attempt no row reports is refused even when it is unselected: a ledger entry with no row is work this
    # publication cannot account for, and dropping it would hide spend and an outage from the totals.
    for key in index:
        if key in known:
            continue
        problems.append(_finding(REFUSE, "ledger", f"{key[0]} {key[1]}: ledger holds an attempt no row reports"))
    hidden = [entry for entry in ledger if entry.get("selected") is not True]
    seconds = sum(float(entry["seconds"]) for entry in hidden if _number(entry.get("seconds")))
    prices = [entry.get("dollars") for entry in hidden]
    priced = [float(price) for price in prices if isinstance(price, int | float) and not isinstance(price, bool)]
    return problems, Operational(
        selected_attempts=selected_attempts,
        ledger_attempts=len(ledger),
        retries=len(hidden),
        retry_seconds=round(seconds, 2),
        retry_dollars=round(sum(priced, 0.0), 5) if len(priced) == len(prices) else None,
    )


def _measured(rows: Sequence[Mapping[str, Any]]) -> list[Mapping[str, Any]]:
    return [row for row in rows if row.get("normalized_status") != "unavailable"]


def _distinct_repeats(rows: Sequence[Mapping[str, Any]]) -> set[object]:
    """The repeats that measured this group. Duplicates are refused elsewhere, so a set cannot hide one."""
    return {row.get("repeat") for row in _measured(rows) if row.get("repeat") is not None}


_MATCHED_CONFIG = ("providers", "model", "text_model", "max_steps", "step_cap", "step_cap_unit", "decision_cap")


def _published_release(row: Mapping[str, Any]) -> str | None:
    run = row.get("run")
    return run.get("fastbrowse_version") if isinstance(run, Mapping) else None


def _fastbrowse(rows: Sequence[Mapping[str, Any]]) -> list[Mapping[str, Any]]:
    """Only fastbrowse rows: another arm's attempts must neither block publication nor mask a regression."""
    return [row for row in rows if row.get("arm") == "fastbrowse"]


def _config(row: Mapping[str, Any]) -> tuple[object, ...]:
    """The protocol and model route a row ran, so a comparison never silently crosses either."""
    run = row.get("run")
    run = run if isinstance(run, Mapping) else {}
    values = (run.get(field) if field in {"providers", "max_steps"} else row.get(field) for field in _MATCHED_CONFIG)
    return tuple(values)


def _config_label(config: tuple[object, ...]) -> str:
    return ", ".join(f"{field}={value!r}" for field, value in zip(_MATCHED_CONFIG, config, strict=True))


def _prices(rows: Sequence[Mapping[str, Any]]) -> list[float]:
    """Only a set that priced every attempt has a comparable median; an unknown cost stays unknown."""
    return [
        float(row["dollars"])
        for row in rows
        if isinstance(row.get("dollars"), int | float) and not isinstance(row.get("dollars"), bool)
    ]


def _over(base: float, candidate: float, floor: float) -> bool:
    """A movement counts only past both the relative bound and the absolute floor, so noise under the floor
    never blocks."""
    return base > 0 and candidate > base * REGRESSION_FACTOR and candidate - base > floor


def _moves(
    where: str,
    release: str,
    measured_base: Sequence[Mapping[str, Any]],
    measured_candidate: Sequence[Mapping[str, Any]],
) -> list[Finding]:
    """Wall time and cost move with cloud load, so each is gated only past its relative bound and floor together."""
    findings = []
    base_seconds = statistics.median(float(row.get("seconds") or 0.0) for row in measured_base)
    candidate_seconds = statistics.median(float(row.get("seconds") or 0.0) for row in measured_candidate)
    if _over(base_seconds, candidate_seconds, REGRESSION_SPEED_FLOOR_SECONDS):
        findings.append(
            _finding(
                REGRESS,
                "speed",
                f"{where}: median {candidate_seconds:.1f}s against {release} {base_seconds:.1f}s; "
                f"over {round((REGRESSION_FACTOR - 1) * 100)}% and more than {REGRESSION_SPEED_FLOOR_SECONDS:.0f}s",
            )
        )
    base_prices = _prices(measured_base)
    candidate_prices = _prices(measured_candidate)
    if len(base_prices) == len(measured_base) and len(candidate_prices) == len(measured_candidate):
        base_cost = statistics.median(base_prices)
        candidate_cost = statistics.median(candidate_prices)
        if _over(base_cost, candidate_cost, REGRESSION_COST_FLOOR_DOLLARS):
            findings.append(
                _finding(
                    REGRESS,
                    "cost",
                    f"{where}: median ${candidate_cost:.4f} against {release} ${base_cost:.4f}; "
                    f"over {round((REGRESSION_FACTOR - 1) * 100)}% and more than ${REGRESSION_COST_FLOOR_DOLLARS}",
                )
            )
    return findings


def _regression(rows: Sequence[Mapping[str, Any]], baselines_: Sequence[Mapping[str, Any]]) -> list[Finding]:
    """Compare each fastbrowse task against the latest published release at the same version, protocol and route.

    Pooling releases would let an old high baseline hide a fall against the current one, pooling arms would score
    fastbrowse on another agent's attempts, and pooling configs would compare across protocols, so none is done.
    A live task needs three distinct measured repeats before any baseline: fewer is a diagnostic, not a
    comparison, whether or not a baseline exists. The newest matching release with at least three attempts is
    chosen whole, so a thin newest release does not hide an older full one, and a task version is not the only
    thing that makes two rows comparable.
    """
    by_key: dict[tuple[object, object, object], dict[str, dict[tuple[object, ...], list[Mapping[str, Any]]]]] = {}
    for row in _fastbrowse(baselines_):
        release = _published_release(row)
        if release is None:
            continue
        key = (row.get("suite"), row.get("task"), row.get("task_version"))
        by_key.setdefault(key, {}).setdefault(release, {}).setdefault(_config(row), []).append(row)
    groups: dict[tuple[object, object, object, tuple[object, ...]], list[Mapping[str, Any]]] = {}
    for row in _fastbrowse(rows):
        groups.setdefault((row.get("suite"), row.get("task"), row.get("task_version"), _config(row)), []).append(row)
    findings: list[Finding] = []
    # Every arm on a live task needs three distinct measured repeats, not only fastbrowse: a competitor measured
    # twice is a diagnostic, and a published suite that scores it would publish that diagnostic as a rate.
    arm_groups: dict[tuple[object, object, object, object], list[Mapping[str, Any]]] = {}
    for row in rows:
        if str(row.get("suite")) in _LIVE_SUITES:
            arm = (row.get("arm"), row.get("suite"), row.get("task"), row.get("task_version"))
            arm_groups.setdefault(arm, []).append(row)
    for (arm, _suite, task, revision), candidate in sorted(
        arm_groups.items(), key=lambda item: (str(item[0][1]), str(item[0][2]), str(item[0][0]))
    ):
        repeats = _distinct_repeats(candidate)
        if len(repeats) < REGRESSION_MIN_ATTEMPTS:
            findings.append(
                _finding(
                    REGRESS,
                    "regression",
                    f"{arm} {task} v{revision}: {len(repeats)} distinct measured repeats; "
                    f"fewer than {REGRESSION_MIN_ATTEMPTS} is a diagnostic, not a published comparison",
                )
            )
    for key, candidate in sorted(groups.items(), key=lambda item: (str(item[0][0]), str(item[0][1]))):
        suite, task, revision, config = key
        where = f"{task} v{revision}"
        measured_candidate = _measured(candidate)
        candidate_repeats = _distinct_repeats(candidate)
        # A live comparison rests on repeats, not rows: this blocks a new task, a bumped task version or a
        # re-routed task with one attempt even before any baseline exists, and duplicate rows cannot count. The
        # finding was already emitted per arm above, so here it only stops a comparison that is not a rate.
        if str(suite) in _LIVE_SUITES and len(candidate_repeats) < REGRESSION_MIN_ATTEMPTS:
            continue
        releases = by_key.get((suite, task, revision))
        if not releases:
            older = sorted({str(other[2]) for other in by_key if other[0] == suite and other[1] == task})
            note = f"{where}: no published baseline at this task version"
            if older:
                note += f"; a baseline exists only at v{', v'.join(older)}"
            findings.append(_finding(ADVISE, "baseline", note))
            continue
        matching = [release for release, configs in releases.items() if config in configs]
        if not matching:
            seen = sorted({_config_label(other) for configs in releases.values() for other in configs})
            findings.append(
                _finding(
                    ADVISE,
                    "baseline",
                    f"{where}: a baseline exists at this task version under another protocol or model route "
                    f"({'; '.join(seen)}), so it is not compared",
                )
            )
            continue
        # A baseline is counted by measured rows rather than distinct repeats: older published files may predate
        # the repeat field, and the baseline's own integrity was checked when it was published.
        rated = [
            release for release in matching if len(_measured(releases[release][config])) >= REGRESSION_MIN_ATTEMPTS
        ]
        if not rated:
            newest = max(matching, key=versions._release_key)
            findings.append(
                _finding(
                    ADVISE,
                    "baseline",
                    f"{where}: baseline {newest} has {len(_measured(releases[newest][config]))} attempts; "
                    f"fewer than {REGRESSION_MIN_ATTEMPTS} is not a rate",
                )
            )
            continue
        release = max(rated, key=versions._release_key)
        measured_base = _measured(releases[release][config])
        passed_base = sum(row.get("passed") is True for row in measured_base)
        passed_candidate = sum(row.get("passed") is True for row in measured_candidate)
        if passed_candidate / len(measured_candidate) < passed_base / len(measured_base):
            findings.append(
                _finding(
                    REGRESS,
                    "regression",
                    f"{where}: {passed_candidate}/{len(measured_candidate)} against {release} "
                    f"{passed_base}/{len(measured_base)}; a per-task pass rate cannot decline",
                )
            )
        findings += _moves(where, release, measured_base, measured_candidate)
    return findings


def _coverage(rows_: Sequence[Mapping[str, Any]]) -> list[Finding]:
    """A live suite is published only as whole as this build defines it.

    The candidate names the suites it claims. For each, every current task must have a row from every arm the
    candidate ran there, and fastbrowse is required whenever the suite is claimed at all. `live.eligible` uses
    each arm's own arms list, so an arm is never asked for a task it cannot run. The build's task list is the
    only source of truth: no baseline is consulted, so the first publication of a suite cannot drop a current
    task, a suite the candidate did not claim is not its business, and a task this build removed is never asked.
    """
    from fastbrowse.evals.catalog import SUITES, eligible

    present: dict[str, set[tuple[str, str]]] = {}
    candidate_arms: dict[str, set[str]] = {}
    for row in rows_:
        suite = str(row.get("suite"))
        if suite not in SUITES:
            continue
        present.setdefault(suite, set()).add((str(row.get("arm")), str(row.get("task"))))
        candidate_arms.setdefault(suite, set()).add(str(row.get("arm")))
    findings: list[Finding] = []
    for suite, arms in sorted(candidate_arms.items()):
        tasks = {task.id: task for task in SUITES[suite]}
        for arm in sorted(arms | {"fastbrowse"}):
            for task_id in sorted(tasks):
                if not eligible(arm, tasks[task_id]) or (arm, task_id) in present[suite]:
                    continue
                findings.append(_finding(REFUSE, "coverage", f"the candidate omits {arm} {task_id} from {suite}"))
    return findings


def gate(
    rows_: Sequence[Mapping[str, Any]],
    *,
    release: str | None = None,
    ledger: Sequence[Mapping[str, Any]] | None = None,
    baselines_: Sequence[Mapping[str, Any]] = (),
    lock: Mapping[str, Mapping[str, Any]] | None = None,
    require_ledger: bool = False,
    require_coverage: bool | None = None,
    require_clean: bool = True,
) -> Report:
    """Every finding about `rows_`; `Report.blocking` is what stops a publication.

    `require_ledger` marks a publication rather than a diagnostic, and `require_coverage` follows it unless set:
    the full-suite check belongs to what is published, so unit-shaped rows can still be checked against one rule.
    """
    if require_coverage is None:
        require_coverage = require_ledger
    if not rows_:
        return Report(
            findings=(_finding(REFUSE, "shape", "no rows: a publication with no results is not a publication"),),
            operational=Operational(selected_attempts=0),
        )
    lock = versions.load_lock() if lock is None else lock
    suites = _suite_ids()
    findings: list[Finding] = []
    for row in rows_:
        live = str(row.get("suite")) in _LIVE_SUITES
        findings += _shape(row)
        findings += _provenance(row, release=release, require_clean=require_clean, live=live)
        findings += _version(row, lock, suites)
        findings += _protocol(row)
        findings += _cost(row)
        findings += _dataset(row)
    if require_coverage and require_ledger:
        findings += _coverage(rows_)
    ledger_findings, operational = _ledger(rows_, ledger, require_ledger=require_ledger)
    findings += ledger_findings
    findings += _regression(rows_, baselines_)
    return Report(findings=tuple(findings), operational=operational)


def _release_of(rows_: Sequence[Mapping[str, Any]]) -> str | None:
    releases = {(row.get("run") or {}).get("fastbrowse_version") for row in rows_}
    return str(releases.pop()) if len(releases) == 1 and None not in releases else None


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="check eval rows against the publication gate")
    parser.add_argument("--rows", type=Path, help="the results JSONL a run wrote")
    parser.add_argument("--check-diff", metavar="BASE", help="validate the docs/results files changed since BASE")
    parser.add_argument("--ledger", type=Path, help="the attempt ledger; defaults beside --rows")
    parser.add_argument("--release", help="the release the rows must be from; defaults to the one they record")
    parser.add_argument(
        "--baseline", type=Path, default=versions.RESULTS, help="published JSONL rows to compare against"
    )
    parser.add_argument("--require-ledger", action="store_true", help="refuse rows whose attempt ledger is missing")
    parser.add_argument(
        "--require-coverage",
        action="store_true",
        help="refuse a live suite published without a row for every task an arm can run (with --require-ledger)",
    )
    parser.add_argument("--allow-dirty", action="store_true", help="do not refuse a tree with uncommitted changes")
    parser.add_argument("--json", action="store_true", help="print the report as JSON")
    args = parser.parse_args(argv)
    if args.check_diff:
        try:
            findings = validate_diff(args.check_diff)
        except RuntimeError as exc:
            print(f"cannot compare changed published rows against {args.check_diff}: {exc}")
            return 1
        for finding in findings:
            print(f"{finding.severity.upper():7} {finding.check}: {finding.detail}")
        if findings:
            print(f"BLOCKED {len(findings)} findings; the changed published rows are not publishable")
            return 1
        print("PASS the changed published rows against the base")
        return 0
    if args.rows is None:
        parser.error("--rows is required unless --check-diff is given")
    if not args.rows.exists():
        print(f"no eval rows: {args.rows} does not exist, so the runner recorded nothing")
        return 1
    rows_ = read_jsonl(args.rows)
    ledger_file = args.ledger or ledger_path(args.rows)
    ledger = read_jsonl(ledger_file) if ledger_file.exists() else None
    release = args.release or _release_of(rows_)
    report = gate(
        rows_,
        release=release,
        ledger=ledger,
        baselines_=baselines(args.baseline) if args.baseline.exists() else (),
        require_ledger=args.require_ledger,
        require_coverage=args.require_coverage,
        require_clean=not args.allow_dirty,
    )
    print(report.model_dump_json(indent=2) if args.json else report.render())
    if report.blocking:
        print(f"BLOCKED {len(report.blocking)} findings; the rows are not publishable")
        return 1
    print(f"PASS {len(rows_)} rows; {len(report.findings)} findings, none blocking")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
