"""Versions for the evals: of each task, of each suite, of the build that ran them, and of what was published.

A score means something only next to what produced it. Every result row therefore carries the fastbrowse version
and commit (and whether the tree was dirty), the models it called, and the version of the task and suite it ran.
Two rows compare only when their task versions match.

A task's version is bumped whenever what it asks or how it is graded changes, and nothing else can keep up with a
change: `versions.json` holds a fingerprint of each task, made of its fields and the tokens (not the comments or
layout) of its grader and answer key, followed into every `fastbrowse.evals` helper and constant they use. A test
fails when a fingerprint no longer matches, so a task cannot change and keep its version.

Approved compact results generate the site feed. Full evidence and source grades are tracked in Parallax.

    uv run python -m fastbrowse.evals.versions --bump TASK_ID ...                   # after changing a task
    uv run python -m fastbrowse.evals.versions --publish RELEASE artifacts/evals/live.jsonl
    uv run python -m fastbrowse.evals.versions --docs                              # regenerate the approved feed
"""

import argparse
import dataclasses
import enum
import functools
import hashlib
import inspect
import io
import json
import platform
import re
import statistics
import subprocess
import sys
import textwrap
import tokenize
import uuid
from collections.abc import Callable, Iterable, Mapping, Sequence
from datetime import UTC, datetime
from importlib.metadata import PackageNotFoundError, distribution, version
from pathlib import Path
from types import CodeType
from typing import Any, Literal

from pydantic import BaseModel

from fastbrowse.evals import baseline

LOCK = Path(__file__).with_name("versions.json")
ROOT = Path(__file__).parents[3]
RESULTS = ROOT / "docs" / "results"
_ADDRESS = re.compile(r" at 0x[0-9a-f]+")
_OWN = ("fastbrowse.evals", "parallax.browser_use")
_CONSTANT = (str, bytes, int, float, bool, tuple, frozenset, dict, re.Pattern, enum.Enum)
_SKIP = {tokenize.COMMENT, tokenize.NL, tokenize.NEWLINE, tokenize.INDENT, tokenize.DEDENT, tokenize.ENDMARKER}
ARM_LABELS = {
    "fastbrowse": "fastbrowse",
    "browser-use": "Browser Use agent",
    "jev-ultrafast": "Browser Use Ultrafast",
}


def _tokens(source: str) -> str:
    """`source` as its tokens alone, so a comment or a reflow is not a change. A lambda's source is the line it sits
    on, which need not tokenize by itself; that line is then taken as written."""
    try:
        tokens = tokenize.generate_tokens(io.StringIO(textwrap.dedent(source)).readline)
        return " ".join(t.string for t in tokens if t.type not in _SKIP)
    except (tokenize.TokenError, IndentationError, SyntaxError):
        return source.strip()


def _source(obj: object) -> str:
    try:
        return _tokens(inspect.getsource(obj))  # ty: ignore[invalid-argument-type]
    except (OSError, TypeError):
        return getattr(obj, "__qualname__", repr(obj))


def _names(code: CodeType) -> set[str]:
    names = set(code.co_names)
    for const in code.co_consts:
        if isinstance(const, CodeType):
            names |= _names(const)
    return names


def _function(fn: Callable[..., object], seen: set[str]) -> object:
    """A grader or answer key: its tokens, what its closure captured (`_has("a")` and `_has("b")` share one body),
    and every eval helper, class and constant it reaches, so a shared helper's change reaches each task using it."""
    reached: dict[str, object] = {}
    code = getattr(fn, "__code__", None)
    scope = getattr(fn, "__globals__", {})
    for name in sorted(_names(code)) if code is not None else ():
        value = scope.get(name)
        own = getattr(value, "__module__", "") or ""
        key = f"{own}.{name}"
        if key in seen:
            continue
        if inspect.isfunction(value) and own.startswith(_OWN):
            seen.add(key)
            reached[name] = _function(value, seen)
        elif inspect.isclass(value) and own.startswith(_OWN):
            seen.add(key)
            reached[name] = _source(value)
        elif isinstance(value, _CONSTANT) and not inspect.isclass(value):
            # Exception.__name__ also reaches a module global, so a file move must preserve its logical namespace.
            reached[name] = (
                scope.get("__fingerprint_namespace__", value) if name == "__name__" else _stable(value, seen)
            )
    cells = [cell.cell_contents for cell in getattr(fn, "__closure__", None) or ()]
    defaults = getattr(fn, "__defaults__", None) or ()
    return {
        "source": _source(fn),
        "closure": _stable(cells, seen),
        "defaults": _stable(defaults, seen),
        "reaches": reached,
    }


def _stable(value: object, seen: set[str]) -> object:
    """A JSON-able form of `value` that is the same on every interpreter: no addresses, no set ordering."""
    if isinstance(value, functools.partial):
        return {"partial": _stable([value.func, value.args, value.keywords], seen)}
    if isinstance(value, type) and issubclass(value, BaseModel):
        return value.model_json_schema()
    if inspect.isfunction(value) or inspect.ismethod(value):
        return _function(value, seen)
    if isinstance(value, enum.Enum):
        return value.value
    if isinstance(value, re.Pattern):
        return {"pattern": value.pattern, "flags": value.flags}
    if isinstance(value, Mapping):
        return {str(k): _stable(v, seen) for k, v in sorted(value.items(), key=lambda item: str(item[0]))}
    if isinstance(value, set | frozenset):
        return sorted(json.dumps(_stable(v, seen), sort_keys=True) for v in value)
    if isinstance(value, list | tuple):
        return [_stable(v, seen) for v in value]
    if dataclasses.is_dataclass(value) and not isinstance(value, type):
        return {f.name: _stable(getattr(value, f.name), seen) for f in dataclasses.fields(value)}
    if value is None or isinstance(value, str | int | float | bool):
        return value
    return _ADDRESS.sub("", repr(value))


def fingerprint(task: object) -> str:
    """What the task asks and how it is graded, hashed: equal fingerprints grade the same run the same way."""
    from fastbrowse.evals.catalog import CatalogTask

    if isinstance(task, CatalogTask):
        return task.source_fingerprint
    assert dataclasses.is_dataclass(task) and not isinstance(task, type)
    seen: set[str] = set()
    # `rolling` says how to fingerprint the task, not what it asks: its text is hashed under its stable name.
    fields = {f.name: _stable(getattr(task, f.name), seen) for f in dataclasses.fields(task) if f.name != "rolling"}
    if hasattr(task, "expect"):
        from fastbrowse.evals.grade import grade
        from fastbrowse.evals.live_tasks import LiveTask, prompt
        from fastbrowse.evals.status import status_matches

        if isinstance(task, LiveTask):
            fields["grade_rule"] = _stable(grade, seen)
        fields["status_rule"] = _stable(status_matches, seen)
        fields["prompt"] = _stable(prompt, seen)
    body = json.dumps(fields, sort_keys=True, default=str)
    for text, name in getattr(task, "rolling", {}).items():
        body = body.replace(text, name)
    return hashlib.sha256(body.encode()).hexdigest()[:16]


def load_lock() -> dict[str, dict[str, Any]]:
    return json.loads(LOCK.read_text(encoding="utf-8")) if LOCK.exists() else {}


def task_version(task_id: str, lock: Mapping[str, Mapping[str, Any]] | None = None) -> int | None:
    entry = (load_lock() if lock is None else lock).get(task_id)
    return None if entry is None else int(entry["version"])


def suite_version(task_ids: Iterable[str], lock: Mapping[str, Mapping[str, Any]] | None = None) -> str:
    """A suite's version: its tasks and their versions, hashed. Adding, dropping or bumping a task changes it."""
    lock = load_lock() if lock is None else lock
    body = "\n".join(sorted(f"{task_id}@{task_version(task_id, lock)}" for task_id in task_ids))
    return hashlib.sha256(body.encode()).hexdigest()[:8]


def _git(*args: str) -> str | None:
    # An installed package can sit inside another project's checkout, so parent discovery is not build identity.
    if not (ROOT / "src" / "fastbrowse" / "evals" / "versions.py").is_file():
        return None
    try:
        done = subprocess.run(
            ["git", *args], cwd=Path(__file__).parent, capture_output=True, text=True, timeout=10, check=True
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return done.stdout.strip()


def provenance(**extra: object) -> dict[str, object]:
    """Who ran: the build, its commit, the interpreter, and a run id shared by every row of one invocation.

    `git_dirty` is None outside a checkout (an installed wheel) and True when tracked files had uncommitted
    changes or untracked, non-ignored files were present, so a score from an unreviewed tree is marked as one.
    Ignored paths (an `artifacts/` directory) do not dirty a build; an untracked executable does.
    """
    try:
        build = version("fastbrowse")
    except PackageNotFoundError:
        build = None
    sha = _git("rev-parse", "HEAD")
    status = None if sha is None else _git("status", "--porcelain")
    dirty = None if status is None else bool(status)
    if sha is None:
        try:
            receipt = json.loads(distribution("fastbrowse").read_text("direct_url.json") or "{}")
        except (PackageNotFoundError, ValueError):
            receipt = {}
        vcs = receipt.get("vcs_info") if isinstance(receipt, dict) else None
        commit = vcs.get("commit_id") if isinstance(vcs, dict) else None
        if isinstance(commit, str) and re.fullmatch(r"[0-9a-f]{40}", commit):
            sha, dirty = commit, False
    return {
        "run_id": uuid.uuid4().hex[:12],
        "run_started": datetime.now(UTC).isoformat(timespec="seconds"),
        "fastbrowse_version": build,
        "git_sha": sha,
        "git_dirty": dirty,
        "python": platform.python_version(),
        **extra,
    }


def mismatches(tasks: Sequence[Any]) -> list[str]:
    """Every way `versions.json` disagrees with the tasks as defined; empty when they agree."""
    lock = load_lock()
    problems = []
    ids = [task.id for task in tasks]
    for task in tasks:
        entry = lock.get(task.id)
        if entry is None:
            problems.append(f"{task.id}: not in versions.json; run --bump {task.id}")
        elif entry["fingerprint"] != fingerprint(task):
            problems.append(f"{task.id}: changed since version {entry['version']}; run --bump {task.id} --docs")
    problems += [f"{task_id}: in versions.json but no task has that id" for task_id in lock if task_id not in ids]
    problems += [f"{task_id}: two tasks share this id" for task_id in sorted({i for i in ids if ids.count(i) > 1})]
    return problems


def all_tasks() -> tuple[dict[str, tuple[Any, ...]], tuple[Any, ...]]:
    """The live suites and the local fixtures; imported here so the harness can import this module."""
    from fastbrowse.evals.catalog import SUITES
    from fastbrowse.evals.mock_tasks import TASKS as MOCK_TASKS
    from fastbrowse.evals.tasks import TASKS

    return SUITES, (*TASKS, *MOCK_TASKS)


# Published results.

_KEPT = ("arm", "task", "repeat", "category", "suite", "suite_version", "task_version", "status",
         "normalized_status", "task_successful", "passed", "correct",
         "seconds", "dollars", "retries", "failure", "model", "text_model", "transient_seconds", "at",
         "answered", "session_seconds", "replaces_run_id", "final_url", "final_page", "failure_class", "site_probe",
         "actions", "decisions", "step_cap", "step_cap_unit", "decision_cap", "provenance")  # fmt: skip
_RUN_KEPT = ("run_id", "run_started", "fastbrowse_version", "git_sha", "git_dirty", "providers", "max_steps",
             "concurrency", "jev_ultrafast", "arms", "python", "argv", "benchmark_family", "benchmark_origin",
             "benchmark_suite", "benchmark_runner_sha", "benchmark_runner_dirty",
             "benchmark_catalog_sha256")  # fmt: skip


def slim(row: Mapping[str, Any]) -> dict[str, Any]:
    """What a published row keeps: its grade and provenance, not the traces that make a raw row 9 KB."""
    kept = {key: row.get(key) for key in _KEPT}
    if isinstance(kept["failure"], str):
        kept["failure"] = kept["failure"][:300]
    kept["run"] = {key: (row.get("run") or {}).get(key) for key in _RUN_KEPT}
    return kept


def publish(release: str, source: Path) -> Path:
    """Commit `source`'s rows as `release`'s published results; refuse rows the publication gate blocks.

    The gate reads the attempt ledger beside `source`, so retries and their spend are published with the score.
    Each live task needs three distinct measured repeats, and each fastbrowse task is compared at the task version
    it ran against the newest published release that ran the same protocol and model route with three attempts.
    """
    from fastbrowse.evals.publication import gate, ledger_path

    def read(file: Path) -> list[dict[str, Any]]:
        return [json.loads(line) for line in file.read_text(encoding="utf-8").splitlines() if line.strip()]

    if release == "auto":
        rows = read(source)
        releases = {(row.get("run") or {}).get("fastbrowse_version") for row in rows}
        if len(releases) != 1 or None in releases:
            raise ValueError("rows must identify one fastbrowse release")
        release = str(releases.pop())
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", release):
        raise ValueError("release must be a numeric x.y.z version")
    target = RESULTS / f"{release}.jsonl"
    if target.exists() or ledger_path(target).exists() or any(name == release for name, _ in published()):
        raise ValueError(f"{target} exists: published results are never rewritten; publish under a new release")
    rows = read(source)
    ledger_file = ledger_path(source)
    ledger = read(ledger_file) if ledger_file.exists() else None
    report = gate(
        rows,
        release=release,
        ledger=ledger,
        baselines_=[row for _, published_rows in published() for row in published_rows],
        require_ledger=True,
    )
    if report.blocking:
        raise ValueError("\n".join(f"{finding.check}: {finding.detail}" for finding in report.blocking))
    RESULTS.mkdir(parents=True, exist_ok=True)
    ordered = sorted((slim(r) for r in rows), key=lambda r: (r["arm"], r["task"], r["run"]["run_started"] or ""))
    assert ledger is not None
    bundle = (
        (ledger_path(target), "".join(json.dumps(row, sort_keys=True) + "\n" for row in ledger)),
        (target, "".join(json.dumps(row, sort_keys=True) + "\n" for row in ordered)),
    )
    created: list[Path] = []
    try:
        for path, text in bundle:
            with path.open("x", encoding="utf-8") as output:
                created.append(path)
                output.write(text)
    except BaseException:
        # A disk error must not leave a half-publication that prevents a clean retry of the same release.
        for path in created:
            path.unlink()
        raise
    return target


def _release_key(release: str) -> tuple[int, ...]:
    return tuple(int(part) for part in re.findall(r"\d+", release))


def published() -> list[tuple[str, list[dict[str, Any]]]]:
    """Every published release with its rows, newest first."""
    return sorted(baseline.published(RESULTS), key=lambda item: _release_key(item[0]), reverse=True)


def _measured(rows: Sequence[Mapping[str, Any]]) -> list[Mapping[str, Any]]:
    """The attempts that measured the arm. One that ended `unavailable` was stopped by a provider outage outlasting
    every retry, so it says nothing about the arm."""
    return [r for r in rows if r.get("normalized_status") != "unavailable"]


def _arm_rank(arm: str) -> int:
    return list(ARM_LABELS).index(arm) if arm in ARM_LABELS else len(ARM_LABELS)


def _by_arm(rows: Sequence[Mapping[str, Any]]) -> dict[str, list[Mapping[str, Any]]]:
    arms: dict[str, list[Mapping[str, Any]]] = {}
    for row in rows:
        arms.setdefault(row["arm"], []).append(row)
    return dict(sorted(arms.items(), key=lambda item: _arm_rank(item[0])))


@dataclasses.dataclass(frozen=True)
class _Comparison:
    """Arms set against each other on the same attempts at the same tasks."""

    arms: tuple[str, ...]
    rows: list[Mapping[str, Any]]
    """Selected results, including unavailable outcomes. Earlier retries remain in the attempt ledger."""
    scored: list[Mapping[str, Any]]
    """The attempts every figure is taken from: as many per arm at each task."""
    left_out: list[str]
    """Tasks some arm has no measured attempt at, so none of their attempts are scored."""


def _by_comparison(rows: Sequence[Mapping[str, Any]]) -> list[_Comparison]:
    """`rows` split by the arms their task ran on, each scored on matched attempts.

    A task runs only on the arms it grades on equal terms (`LiveTask.arms`): pooled, a suite set fastbrowse on 21
    tasks beside Browser Use on 14. An attempt a provider outage ended measured nothing; dropping it from one arm
    alone would score the arms on different attempts, so at each task an attempt counts only where every arm measured
    the same repeat of it (rows from before repeats were recorded pair earliest first), and a task some arm has none at
    is left out. Groups with most arms first."""
    ran: dict[str, set[str]] = {}
    for row in rows:
        ran.setdefault(row["task"], set()).add(row["arm"])
    groups: dict[tuple[str, ...], list[Mapping[str, Any]]] = {}
    for row in rows:
        groups.setdefault(tuple(sorted(ran[row["task"]], key=_arm_rank)), []).append(row)
    comparisons = []
    for arms, group in sorted(groups.items(), key=lambda item: (-len(item[0]), [_arm_rank(a) for a in item[0]])):
        tasks: dict[str, dict[str, list[Mapping[str, Any]]]] = {}
        for row in _measured(group):
            tasks.setdefault(row["task"], {}).setdefault(row["arm"], []).append(row)
        scored: list[Mapping[str, Any]] = []
        for per_arm in tasks.values():
            if len(per_arm) == len(arms):
                scored += _paired(per_arm)
        left_out = sorted({r["task"] for r in group} - {r["task"] for r in scored})
        comparisons.append(_Comparison(arms, group, scored, left_out))
    return comparisons


def _paired(per_arm: Mapping[str, list[Mapping[str, Any]]]) -> list[Mapping[str, Any]]:
    """The attempts at one task every arm measured: matched by repeat, or earliest first for rows without one."""
    if all(r.get("repeat") is not None for attempts in per_arm.values() for r in attempts):
        shared = set.intersection(*({_pass(r) for r in attempts} for attempts in per_arm.values()))
        return [r for attempts in per_arm.values() for r in attempts if _pass(r) in shared]
    fewest = min(map(len, per_arm.values()))
    return [r for attempts in per_arm.values() for r in sorted(attempts, key=lambda r: r.get("at") or 0.0)[:fewest]]


def _pass(row: Mapping[str, Any]) -> tuple[object, int]:
    return row.get("replaces_run_id") or (row.get("run") or {}).get("run_id"), row["repeat"]


def _by_suite(rows: Sequence[Mapping[str, Any]]) -> dict[tuple[str, str], list[Mapping[str, Any]]]:
    groups: dict[tuple[str, str], list[Mapping[str, Any]]] = {}
    for row in rows:
        groups.setdefault((row["suite"], row["suite_version"]), []).append(row)
    return dict(sorted(groups.items()))


class MetricSummary(BaseModel):
    median: float | None
    mean: float | None


class ArmSummary(BaseModel):
    passed: int
    total: int
    excluded: int = 0
    """Attempts made but not scored: those a provider outage ended, and as many of this arm's at the same task when
    another arm lost one, so every arm is scored on the same attempts."""
    priced: int
    seconds: MetricSummary
    dollars: MetricSummary


class TaskChange(BaseModel):
    task: str
    previous: list[int]
    current: list[int]


class ReleaseSummary(BaseModel):
    fastbrowse_version: str
    date: str
    suite: str
    suite_version: str
    compared: list[str]
    """The arms that ran these tasks, each on all of them: the comparison this entry is."""
    tasks: int
    arms: dict[str, ArmSummary]
    task_versions_changed: list[TaskChange]


class ResultsSummary(BaseModel):
    schema_version: Literal[2] = 2
    releases: list[ReleaseSummary]


def _metrics(values: list[float]) -> MetricSummary:
    return MetricSummary(
        median=statistics.median(values) if values else None, mean=statistics.mean(values) if values else None
    )


def summary(releases: Sequence[tuple[str, list[dict[str, Any]]]] | None = None) -> ResultsSummary:
    """Release/suite pairs, newest first, without mixing task versions or inventing unpriced costs."""
    releases = published() if releases is None else releases
    previous: dict[str, list[int]] = {}
    result: list[ReleaseSummary] = []
    for release, rows in sorted(releases, key=lambda item: _release_key(item[0])):
        versions = {
            task: sorted({r["task_version"] for r in rows if r["task"] == task})
            for task in sorted({r["task"] for r in rows})
        }
        comparisons = [
            (suite, revision, comparison)
            for (suite, revision), suite_rows in _by_suite(rows).items()
            for comparison in _by_comparison(suite_rows)
        ]
        for suite, revision, comparison in comparisons:
            group = comparison.rows
            arms = {}
            scored = _by_arm(comparison.scored)
            for arm, attempts in _by_arm(group).items():
                arm_rows = scored.get(arm, [])
                prices = [r["dollars"] for r in arm_rows if r["dollars"] is not None]
                arms[arm] = ArmSummary(
                    passed=sum(bool(r["passed"]) for r in arm_rows),
                    total=len(arm_rows),
                    excluded=len(attempts) - len(arm_rows),
                    priced=len(prices),
                    seconds=_metrics([r["seconds"] for r in arm_rows]),
                    dollars=_metrics(prices if len(prices) == len(arm_rows) else []),
                )
            changes = [
                TaskChange(task=task, previous=previous.get(task, []), current=current)
                for task in sorted({r["task"] for r in group})
                if previous
                and previous.get(task) != (current := sorted({r["task_version"] for r in group if r["task"] == task}))
            ]
            result.append(
                ReleaseSummary(
                    fastbrowse_version=release,
                    date=max(r["run"]["run_started"][:10] for r in group),
                    suite=suite,
                    suite_version=revision,
                    compared=list(comparison.arms),
                    tasks=len({r["task"] for r in comparison.scored}),
                    arms=arms,
                    task_versions_changed=changes,
                )
            )
        previous.update(versions)
    # Newest release first, and within it the suites in their defined order, core first: a reader of the feed that
    # takes its first entry gets the head-to-head, not whichever suite sorts last by name.
    order = list(all_tasks()[0])
    rank = {suite: i for i, suite in enumerate(order)}
    result.sort(key=lambda r: (rank.get(r.suite, len(order)), r.suite, r.suite_version))
    result.sort(key=lambda r: _release_key(r.fastbrowse_version), reverse=True)
    return ResultsSummary(releases=result)


def render_summary() -> str:
    return summary().model_dump_json(indent=2) + "\n"


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="check or update the eval versions and published results")
    parser.add_argument("--bump", nargs="*", default=[], metavar="TASK_ID")
    parser.add_argument("--publish", nargs=2, metavar=("RELEASE", "ROWS"), help="commit a results file as RELEASE")
    parser.add_argument("--docs", action="store_true", help="regenerate the compact approved results feed")
    args = parser.parse_args(argv)
    suites, local = all_tasks()
    tasks = {t.id: t for t in (*(t for s in suites.values() for t in s), *local)}
    if unknown := sorted(set(args.bump) - tasks.keys()):
        parser.error(f"no task has the id {', '.join(unknown)}")
    lock = {task_id: entry for task_id, entry in load_lock().items() if task_id in tasks}
    for task_id in args.bump:
        lock[task_id] = {"version": (task_version(task_id, lock) or 0) + 1, "fingerprint": fingerprint(tasks[task_id])}
    if args.bump:
        LOCK.write_text(json.dumps(dict(sorted(lock.items())), indent=2) + "\n", encoding="utf-8")
    if args.publish:
        try:
            print(f"published {publish(args.publish[0], Path(args.publish[1]))}")
        except ValueError as exc:
            parser.error(str(exc))
    if args.docs or args.publish:
        RESULTS.mkdir(parents=True, exist_ok=True)
        (RESULTS / "summary.json").write_text(render_summary(), encoding="utf-8")
    problems = mismatches(list(tasks.values()))
    print("\n".join(problems) or f"{len(tasks)} tasks match versions.json")
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
