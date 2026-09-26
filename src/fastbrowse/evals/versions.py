"""Versions for the evals: of each task, of each suite, of the build that ran them, and of what was published.

A score means something only next to what produced it. Every result row therefore carries the fastbrowse version
and commit (and whether the tree was dirty), the models it called, and the version of the task and suite it ran.
Two rows compare only when their task versions match.

A task's version is bumped whenever what it asks or how it is graded changes, and nothing else can keep up with a
change: `versions.json` holds a fingerprint of each task, made of its fields and the tokens (not the comments or
layout) of its grader and answer key, followed into every `fastbrowse.evals` helper and constant they use. A test
fails when a fingerprint no longer matches, so a task cannot change and keep its version.

Published results are rows committed to `docs/results/<release>.jsonl`, never retyped: the tables in
`docs/evals.md` and the README headline are generated from them, and a test fails when either differs.

    uv run python -m fastbrowse.evals.versions --bump TASK_ID ...                   # after changing a task
    uv run python -m fastbrowse.evals.versions --publish RELEASE artifacts/evals/live.jsonl
    uv run python -m fastbrowse.evals.versions --docs                              # regenerate the docs
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
from importlib.metadata import PackageNotFoundError, version
from pathlib import Path
from types import CodeType
from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, Field, FiniteFloat, ValidationError

LOCK = Path(__file__).with_name("versions.json")
ROOT = Path(__file__).parents[3]
DOCS = ROOT / "docs" / "evals.md"
README = ROOT / "README.md"
RESULTS = ROOT / "docs" / "results"
LEGACY = RESULTS / "legacy.json"
_ADDRESS = re.compile(r" at 0x[0-9a-f]+")
_OWN = "fastbrowse.evals"
_CONSTANT = (str, bytes, int, float, bool, tuple, frozenset, dict, re.Pattern, enum.Enum)
_SKIP = {tokenize.COMMENT, tokenize.NL, tokenize.NEWLINE, tokenize.INDENT, tokenize.DEDENT, tokenize.ENDMARKER}
ARM_LABELS = {"fastbrowse": "fastbrowse", "browser-use": "Browser Use agent", "jev-ultrafast": "jev-ultrafast"}


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
            reached[name] = _stable(value, seen)
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
    assert dataclasses.is_dataclass(task) and not isinstance(task, type)
    seen: set[str] = set()
    # `rolling` says how to fingerprint the task, not what it asks: its text is hashed under its stable name.
    fields = {f.name: _stable(getattr(task, f.name), seen) for f in dataclasses.fields(task) if f.name != "rolling"}
    if hasattr(task, "expect"):
        from fastbrowse.evals.live_tasks import prompt
        from fastbrowse.evals.status import status_matches

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
    changes, so a score from an unreviewed tree is marked as one.
    """
    try:
        build = version("fastbrowse")
    except PackageNotFoundError:
        build = None
    sha = _git("rev-parse", "HEAD")
    status = None if sha is None else _git("status", "--porcelain", "--untracked-files=no")
    return {
        "run_id": uuid.uuid4().hex[:12],
        "run_started": datetime.now(UTC).isoformat(timespec="seconds"),
        "fastbrowse_version": build,
        "git_sha": sha,
        "git_dirty": None if status is None else bool(status),
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
    from fastbrowse.evals.live import SUITES
    from fastbrowse.evals.tasks import TASKS

    return SUITES, TASKS


# Published results.

_KEPT = ("arm", "task", "category", "suite", "suite_version", "task_version", "status",
         "normalized_status", "task_successful", "passed", "correct",
         "seconds", "dollars", "retries", "failure", "model", "text_model", "transient_seconds", "at",
         "answered", "session_seconds")  # fmt: skip
_RUN_KEPT = ("run_id", "run_started", "fastbrowse_version", "git_sha", "git_dirty", "providers", "max_steps",
             "concurrency", "jev_ultrafast", "arms", "python", "argv")  # fmt: skip


def slim(row: Mapping[str, Any]) -> dict[str, Any]:
    """What a published row keeps: its grade and provenance, not the traces that make a raw row 9 KB."""
    kept = {key: row.get(key) for key in _KEPT}
    if isinstance(kept["failure"], str):
        kept["failure"] = kept["failure"][:300]
    kept["run"] = {key: (row.get("run") or {}).get(key) for key in _RUN_KEPT}
    return kept


class _StatisticsRow(BaseModel):
    model_config = ConfigDict(strict=True)

    arm: str = Field(min_length=1)
    passed: bool
    correct: bool
    seconds: FiniteFloat = Field(ge=0)
    dollars: FiniteFloat | None = Field(ge=0)


def publish(release: str, source: Path) -> Path:
    """Commit `source`'s rows as `release`'s published results; refuse rows that cannot be traced or compared."""
    if release == "auto":
        rows = [json.loads(line) for line in source.read_text(encoding="utf-8").splitlines() if line.strip()]
        releases = {(row.get("run") or {}).get("fastbrowse_version") for row in rows}
        if len(releases) != 1 or None in releases:
            raise ValueError("rows must identify one fastbrowse release")
        release = str(releases.pop())
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", release):
        raise ValueError("release must be a numeric x.y.z version")
    target = RESULTS / f"{release}.jsonl"
    if target.exists():
        raise ValueError(f"{target} exists: published results are never rewritten; publish under a new release")
    rows = [json.loads(line) for line in source.read_text(encoding="utf-8").splitlines() if line.strip()]
    lock = load_lock()
    problems = []
    for row in rows:
        run = row.get("run") or {}
        where = f"{row.get('arm')} {row.get('task')}"
        try:
            _StatisticsRow.model_validate(row)
        except ValidationError as exc:
            problems.append(f"{where}: {exc.error_count()} invalid fields ({exc.errors()[0]['loc']})")
            continue
        if not run.get("git_sha") or run.get("git_dirty") is not False:
            problems.append(f"{where}: not from a clean, committed tree")
        elif run.get("fastbrowse_version") != release:
            problems.append(f"{where}: ran fastbrowse {run.get('fastbrowse_version')}, not {release}")
        if not run.get("run_started") or not run.get("run_id"):
            problems.append(f"{where}: missing run date or id")
        if not row.get("suite") or not row.get("suite_version"):
            problems.append(f"{where}: missing suite or suite version")
        current = task_version(str(row.get("task")), lock)
        if current is None:
            problems.append(f"{where}: no task by that id is in versions.json")
        elif row.get("task_version") != current:
            problems.append(f"{where}: task version {row.get('task_version')} is not the current one")
    if not rows or problems:
        raise ValueError("\n".join(problems) or f"{source} has no rows")
    # The results file is never rewritten, so every page it regenerates must be checked before it exists.
    if "<!-- evals:headline -->" not in README.read_text(encoding="utf-8"):
        raise ValueError("README.md has no <!-- evals:headline --> block")
    if _RESULTS_HEADING not in DOCS.read_text(encoding="utf-8"):
        raise ValueError("docs/evals.md has no Results section")
    RESULTS.mkdir(parents=True, exist_ok=True)
    ordered = sorted((slim(r) for r in rows), key=lambda r: (r["arm"], r["task"], r["run"]["run_started"] or ""))
    target.write_text("".join(json.dumps(r, sort_keys=True) + "\n" for r in ordered), encoding="utf-8")
    return target


def _release_key(release: str) -> tuple[int, ...]:
    return tuple(int(part) for part in re.findall(r"\d+", release))


def published() -> list[tuple[str, list[dict[str, Any]]]]:
    """Every published release with its rows, newest first."""
    files = sorted(RESULTS.glob("*.jsonl"), key=lambda f: _release_key(f.stem), reverse=True)
    return [(f.stem, [json.loads(line) for line in f.read_text(encoding="utf-8").splitlines() if line]) for f in files]


def _measured(rows: Sequence[Mapping[str, Any]]) -> list[Mapping[str, Any]]:
    """The attempts that measured the arm. One that ended `unavailable` was stopped by a provider outage outlasting
    every retry, so it says nothing about the arm."""
    return [r for r in rows if r.get("normalized_status") != "unavailable"]


def _active(row: Mapping[str, Any]) -> float:
    """A row's time without the provider outage waits measured inside it: a 503 and its backoff say nothing about
    the agent, so no published time includes them."""
    return max(0.0, row["seconds"] - (row.get("transient_seconds") or 0.0))


def _arm_stats(rows: Sequence[Mapping[str, Any]]) -> dict[str, str]:
    if not rows:
        return dict.fromkeys(
            ("passed", "correct", "median time", "mean time", "median cost", "mean cost", "total cost"), "-"
        )
    seconds = [_active(r) for r in rows]
    dollars = [r["dollars"] for r in rows if r["dollars"] is not None]
    unpriced = f" ({len(rows) - len(dollars)} unpriced)" if len(dollars) < len(rows) else ""
    return {
        "passed": f"{sum(r['passed'] for r in rows)}/{len(rows)}",
        "correct": f"{sum(r['correct'] for r in rows)}/{len(rows)}",
        "median time": f"{statistics.median(seconds):.1f}s",
        "mean time": f"{statistics.mean(seconds):.1f}s",
        "median cost": f"${statistics.median(dollars):.4f}" if not unpriced else "unknown",
        "mean cost": f"${statistics.mean(dollars):.4f}" if not unpriced else "unknown",
        "total cost": f"${sum(dollars):.2f}{unpriced}",
    }


def _label(arm: str) -> str:
    return ARM_LABELS.get(arm, arm)


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
    """Every attempt made, outages included."""
    scored: list[Mapping[str, Any]]
    """The attempts every figure is taken from: as many per arm at each task."""
    left_out: list[str]
    """Tasks some arm has no measured attempt at, so none of their attempts are scored."""

    @property
    def title(self) -> str:
        tasks = len({r["task"] for r in self.scored})
        noun = "task" if tasks == 1 else "tasks"
        if len(self.arms) == 1:
            return f"{_label(self.arms[0])} alone, on the {tasks} {noun} only it ran"
        return " against ".join(_label(a) for a in self.arms) + f", on the same {tasks} {noun}"

    @property
    def note(self) -> str:
        """How many attempts each arm made and why fewer are scored, so no gap in the counts is left unexplained."""
        made = {arm: len(attempts) for arm, attempts in _by_arm(self.rows).items()}
        same = len(set(made.values())) == 1
        note = (
            f"Each arm made {next(iter(made.values()))} attempts."
            if same
            else "Attempts made: " + ", ".join(f"{_label(arm)} {n}" for arm, n in made.items()) + "."
        )
        outages = {arm: len(a) - len(_measured(a)) for arm, a in _by_arm(self.rows).items()}
        if not any(outages.values()):
            return note
        named = " and ".join(f"{n} of {_label(arm)}'s" for arm, n in outages.items() if n)
        note += (
            f" Provider outages ended {named}, so each arm is scored on the same {len(self.scored) // len(self.arms)}"
        )
        note += ": an attempt one arm lost is dropped for every arm at that task." if len(self.arms) > 1 else "."
        for task in self.left_out:
            missing = [
                a for a in self.arms if not _measured([r for r in self.rows if r["task"] == task and r["arm"] == a])
            ]
            note += f" `{task}` is left out, with no {' or '.join(map(_label, missing))} attempt measured."
        return note


def _by_comparison(rows: Sequence[Mapping[str, Any]]) -> list[_Comparison]:
    """`rows` split by the arms their task ran on, each scored on matched attempts.

    A task runs only on the arms it grades on equal terms (`LiveTask.arms`): pooled, a suite set fastbrowse on 21
    tasks beside Browser Use on 14. An attempt a provider outage ended measured nothing; dropping it from one arm
    alone would score the arms on different tasks, so at each task every arm keeps as many attempts as the arm with
    fewest measured, its earliest, and a task some arm has none at is left out. Groups with most arms first."""
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
                fewest = min(map(len, per_arm.values()))
                for attempts in per_arm.values():
                    scored += sorted(attempts, key=lambda r: r.get("at") or 0.0)[:fewest]
        left_out = sorted({r["task"] for r in group} - {r["task"] for r in scored})
        comparisons.append(_Comparison(arms, group, scored, left_out))
    return comparisons


def results_table(release: str, rows: Sequence[Mapping[str, Any]]) -> str:
    return "\n\n".join(
        _results_table(release, comparison)
        for suite in _by_suite(rows).values()
        for comparison in _by_comparison(suite)
    )


def _by_suite(rows: Sequence[Mapping[str, Any]]) -> dict[tuple[str, str], list[Mapping[str, Any]]]:
    groups: dict[tuple[str, str], list[Mapping[str, Any]]] = {}
    for row in rows:
        groups.setdefault((row["suite"], row["suite_version"]), []).append(row)
    return dict(sorted(groups.items()))


def _results_table(release: str, comparison: _Comparison) -> str:
    """One comparison in `release`'s published rows as a results table, with the suite version and runs behind it
    and any task changed since, so an old score cannot pass for one of the tasks as they stand."""
    columns = ["passed", "correct", "median time", "mean time", "median cost", "mean cost", "total cost"]
    rows = comparison.rows
    (suite, revision), *_ = _by_suite(rows)
    lines = [f"`{suite}` `{revision}`: {comparison.title}.", "",
             "| | " + " | ".join(columns) + " |", "|:--|" + ":--|" * len(columns)]  # fmt: skip
    scored = _by_arm(comparison.scored)
    for arm in comparison.arms:
        stats = _arm_stats(scored.get(arm, []))
        label = f"{_label(arm)} ({release})" if arm == "fastbrowse" else _label(arm)
        lines.append(f"| {label} | " + " | ".join(stats[c] for c in columns) + " |")
    runs = sorted({(r["run"]["run_id"], (r["run"]["git_sha"] or "")[:7]) for r in rows})
    lines += ["", comparison.note + " Runs: " + ", ".join(f"`{run}` at `{sha}`" for run, sha in runs) + "."]
    lock = load_lock()
    changed = sorted(
        {(r["task"], r["task_version"], task_version(r["task"], lock)) for r in rows}
        - {(r["task"], r["task_version"], r["task_version"]) for r in rows}
    )
    if changed:
        lines.append(
            "Changed since these runs: "
            + ", ".join(f"`{task}` v{then} → {'removed' if now is None else f'v{now}'}" for task, then, now in changed)
            + "; compare them only against runs of the same version."
        )
    return "\n".join(lines)


def headline(release: str, rows: Sequence[Mapping[str, Any]]) -> str:
    """The README's comparison table, from the newest published results."""
    days = sorted(r["run"]["run_started"][:10] for r in rows if r["run"]["run_started"])
    tasks = {r["task"] for r in rows}
    lines = [
        f"Measured on {days[-1]} with the build released as {release}: {len(tasks)} tasks, "
        f"{len(rows)} attempts across all arms, on cloud browsers.",
        "",
    ]
    alone = 0
    for (suite, revision), suite_rows in _by_suite(rows).items():
        for comparison in _by_comparison(suite_rows):
            if len(comparison.arms) == 1:
                alone += len({r["task"] for r in comparison.rows})
                continue
            lines += [f"Suite `{suite}` `{revision}`: {comparison.title}.", "",
                      "| | passed | cost per task | median time |", "|:--|:--|:--|:--|"]  # fmt: skip
            scored = _by_arm(comparison.scored)
            for arm in comparison.arms:
                s = _arm_stats(scored.get(arm, []))
                cost = f"{s['median cost']} (median), {s['mean cost']} mean"
                lines.append(f"| {_label(arm)} | {s['passed']} | {cost} | {s['median time']} |")
            lines += ["", comparison.note, ""]
    if alone:
        lines.append(f"{alone} {'task' if alone == 1 else 'tasks'} graded on fastbrowse alone "
                     f"{'is' if alone == 1 else 'are'} in [docs/evals.md](docs/evals.md#results).")  # fmt: skip
    return "\n".join(lines).rstrip()


class _LegacyArm(BaseModel):
    passed: int
    total: int
    correct: int
    median_seconds: float
    mean_seconds: float
    median_dollars: float
    mean_dollars: float
    total_dollars: float


class _LegacyResults(BaseModel):
    release: str
    date: str
    tasks: int
    repeats: int
    concurrency: int
    max_steps: int
    source: str
    arms: dict[str, _LegacyArm]


def legacy_docs(*, headline_only: bool = False) -> str:
    old = _LegacyResults.model_validate_json(LEGACY.read_text(encoding="utf-8"))
    lines = [
        f"Measured on {old.date} with the build released as {old.release}: {old.tasks} answer tasks, "
        f"{old.repeats} attempts each on cloud browsers.",
        "",
        "| | passed | cost per task | median time |"
        if headline_only
        else "| | passed | correct answer | median time | mean time | median cost | mean cost | suite total |",
        "|:--|:--|:--|:--|" if headline_only else "|:--|:--|:--|:--|:--|:--|:--|:--|",
    ]
    for arm, stats in old.arms.items():
        label = ARM_LABELS[arm]
        if headline_only:
            lines.append(
                f"| {label} | {stats.passed}/{stats.total} | ${stats.median_dollars:.4f} (median), "
                f"${stats.mean_dollars:.4f} mean | {stats.median_seconds:.1f}s |"
            )
        else:
            lines.append(
                f"| {label} | {stats.passed}/{stats.total} | {stats.correct}/{stats.total} | "
                f"{stats.median_seconds:.1f}s | {stats.mean_seconds:.1f}s | ${stats.median_dollars:.4f} | "
                f"${stats.mean_dollars:.4f} | ${stats.total_dollars:.2f} |"
            )
    lines += ["", "These are historical aggregates, predating versioned rows and the current stricter graders."]
    if not headline_only:
        lines += [
            f"That release used {old.concurrency} concurrent runs and a {old.max_steps}-step limit for "
            "fastbrowse and jev-ultrafast. The current limit is generated above.",
            f"The [archived report]({old.source}) preserves task, navigation and split-suite detail.",
            "`docs/results/legacy.json` records these aggregates; it is excluded from the site feed.",
        ]
    return "\n".join(lines)


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
                    seconds=_metrics([_active(r) for r in arm_rows]),
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
        previous = versions
    # Newest release first, and within it the suites in their defined order, core first: a reader of the feed that
    # takes its first entry gets the head-to-head, not whichever suite sorts last by name.
    order = list(all_tasks()[0])
    rank = {suite: i for i, suite in enumerate(order)}
    result.sort(key=lambda r: (rank.get(r.suite, len(order)), r.suite, r.suite_version))
    result.sort(key=lambda r: _release_key(r.fastbrowse_version), reverse=True)
    return ResultsSummary(releases=result)


def render_summary() -> str:
    return summary().model_dump_json(indent=2) + "\n"


def _cell(text: str) -> str:
    return " ".join(text.split()).replace("|", "\\|")


def protocol_docs() -> str:
    from fastbrowse.evals.live import ARMS, MAX_STEPS

    lines = [
        "Every arm receives `Start at {start}. {task}`. CDP runners also receive the declared start URL.",
        f"fastbrowse, jev-ultrafast and browser-use OSS use a {MAX_STEPS}-step limit. "
        "The hosted API exposes no step limit.",
        "The existing harness has no common dollar or wall-time cap; "
        "cloud browsers expire after their configured lifetime.",
        "Default arms: " + ", ".join(f"`{name}`" for name, arm in ARMS.items() if arm.default) + ".",
        "",
        "| Arm | Pin | Tier |",
        "|---|---|---|",
    ]
    lines.extend(f"| `{name}` | {arm.pin} | {arm.tier} |" for name, arm in ARMS.items())
    lines += [
        "",
        "`browser-use-oss` is opt-in and installed in an isolated uv environment only when selected.",
        "Its Pydantic pin conflicts with the hosted SDK, so it is not a project extra.",
        "The jev-ultrafast runner answers a one-option choice itself, as fastbrowse does, because Jev refuses it, "
        "and drops a code fence its text helper's model wraps around JSON, which upstream's strict parse rejects.",
        "Rows keep raw `status`, `task_successful` and `normalized_status`: "
        "`done`, `stopped`, `budget`, `timeout`, `error`, `blocked` or `unavailable`.",
        "A pass requires a correct grade and `done`, or the exact expected fastbrowse stop.",
        "The hosted arm is `done` when its agent answered: it called `done` with a result that was not an error, "
        "or, never calling `done`, replied with the answer the session kept as its output. That is its own "
        "completion, as fastbrowse is held to its own. Browser Use's `is_task_successful` is kept in the row "
        "(`task_successful`) but decides nothing: it is Browser Use's later judgement of the session, and it failed "
        "correct answers whose sessions showed no sign of failing or giving up.",
        "An attempt that fails while the task's site answers its start URL with a 5xx, or not at all, is an outage "
        "too: a site serving errors fails every arm alike.",
        "A fastbrowse attempt in which any Jev call took over 2 seconds, retries included, is a Jev outage: healthy "
        "calls take about half a second at any page size, and no worse than 0.93 seconds in 45 measured.",
        "An attempt an outage ended is waited out and run again, up to five times over about 25 minutes.",
        "A row still unavailable after that is recorded but scores nothing, and neither does one attempt of every "
        "other arm at that task: each comparison scores its arms on the same attempts at the same tasks.",
        "Time runs from the start of an attempt to the agent's answer. Provider outage waits measured inside an "
        "attempt (`transient_seconds`: failed requests and the backoff between them) are left out of every time "
        "published; only fastbrowse's client can see its own, so the other arms' are 0.",
        "The hosted arm's time ends at its agent's answer, by Browser Use's own clock from the session's creation. "
        "Its API reports the session stopped as much as two minutes later (`session_seconds`), which is not counted.",
        "Earlier unavailable attempts are counted by `retries`; their time and cost are not aggregated into the row.",
        "Existing timing includes browser setup. These rows do not claim the planned handoff-only timing protocol.",
        "Jev is priced at list ($0.042 per million input tokens) whenever the gateway meters a request at $0, for "
        "fastbrowse and jev-ultrafast alike.",
    ]
    return "\n\n".join(lines[:4]) + "\n" + "\n".join(lines[4:])


def feed_schema_docs() -> str:
    models = (ResultsSummary, ReleaseSummary, ArmSummary, MetricSummary, TaskChange)
    lines = [
        f"Schema version {ResultsSummary.model_fields['schema_version'].default}. "
        "Each releases entry represents one comparison in one release, suite and suite version.",
        "",
        "| Object | Fields |",
        "|---|---|",
    ]
    for model in models:
        lines.append(f"| `{model.__name__}` | " + ", ".join(f"`{name}`" for name in model.model_fields) + " |")
    lines += [
        "",
        "`releases` is newest first, and within a release the suites run in their defined order, `core` first. "
        "`date` is the latest UTC run date in that group.",
        "Each entry is one comparison: `compared` names the arms scored on its `tasks`, every arm on all of them, "
        "since a task runs only on the arms it grades on equal terms. A suite has one entry per comparison, "
        "those with most arms first; the first `core` entry is fastbrowse against Browser Use.",
        "Schema version 2 added `compared` and `tasks`; version 1 pooled a suite's comparisons into one entry.",
        "`arms` maps registry names to statistics across the scored attempts, failures included. `total` counts "
        "them; `excluded` counts attempts made but not scored, ended by an outage or matched to one.",
        "`seconds` and `dollars` contain numeric median and mean values; dollars are USD. "
        "Seconds leave out measured outage waits.",
        "`priced` counts attempts with known cost. Both dollar statistics are null if any attempt is unpriced.",
        "`task_versions_changed` compares observed task versions with the previous published release:",
        "`task`, `previous` and `current` version lists. New tasks have an empty previous list;",
        "tasks absent from the current group are not reported as removed. The first release has no changes.",
        "Separate suite versions never share an aggregate. No wall-clock generation timestamp is emitted.",
    ]
    if not published():
        lines += ["There are no published JSONL rows yet; the generated releases array is empty."]
    return "\n".join(lines)


def docs_blocks(releases: Sequence[tuple[str, list[dict[str, Any]]]] | None = None) -> dict[str, str]:
    """The generated parts of docs/evals.md, by marker name: a task table per split suite, the versions, and a
    table per published release."""
    suites, local = all_tasks()
    lock = load_lock()
    blocks = {"protocol": protocol_docs(), "feed-schema": feed_schema_docs(), "legacy": legacy_docs()}
    for name, tasks in suites.items():
        if name == "core":
            continue  # the core suite's table says how each task is graded, which is prose; a test checks its ids
        rows = [f"| `{t.id}` | {t.category.value} | {task_version(t.id, lock)} | {_cell(t.task)} |" for t in tasks]
        blocks[f"tasks:{name}"] = "| Task | Category | Version | Asks |\n|---|---|---|---|\n" + "\n".join(rows)
    named = [(f"`{name}`", tasks) for name, tasks in suites.items()] + [("local fixtures", local)]
    versions = [f"| {name} | {len(tasks)} | `{suite_version((t.id for t in tasks), lock)}` |" for name, tasks in named]
    changed = sorted((task_id, entry["version"]) for task_id, entry in lock.items() if entry["version"] > 1)
    table = "| Suite | Tasks | Version |\n|---|---|---|\n" + "\n".join(versions)
    if changed:
        table += "\n\nTasks past version 1: " + ", ".join(f"`{task_id}` v{v}" for task_id, v in changed) + "."
    blocks["versions"] = table
    for release, rows in published() if releases is None else releases:
        blocks[f"results:{release}"] = results_table(release, rows)
    return blocks


def _render(text: str, blocks: Mapping[str, str], where: str) -> str:
    """`text` with each `<!-- evals:NAME -->` ... `<!-- /evals:NAME -->` block replaced by its generated body."""
    for name, body in blocks.items():
        pattern = re.compile(rf"(<!-- evals:{re.escape(name)} -->).*?(<!-- /evals:{re.escape(name)} -->)", re.S)
        if not pattern.search(text):
            raise ValueError(f"{where} has no <!-- evals:{name} --> block")
        text = pattern.sub(lambda m, body=body: f"{m.group(1)}\n{body}\n{m.group(2)}", text)
    return text


_RESULTS_HEADING = "\n## Results\n"


def render_docs(text: str) -> str:
    """docs/evals.md with its generated blocks; a newly published release gets its own section, newest first."""
    releases = published()
    for release, rows in releases:
        name = f"results:{release}"
        if f"<!-- evals:{name} -->" in text:
            continue
        start = text.index(_RESULTS_HEADING) + len(_RESULTS_HEADING)
        end = next((m.start() for m in re.finditer(r"\n## ", text) if m.start() >= start), len(text))
        older = (
            m.start()
            for m in re.finditer(r"\n### (\S+), \d{4}-\d{2}-\d{2}\n", text)
            if start <= m.start() < end and _release_key(m[1]) < _release_key(release)
        )
        at = next(older, end)
        day = rows[0]["run"]["run_started"][:10]
        text = f"{text[:at]}\n### {release}, {day}\n\n<!-- evals:{name} -->\n<!-- /evals:{name} -->\n{text[at:]}"
    return _render(text, docs_blocks(releases), "docs/evals.md")


def render_readme(text: str) -> str:
    """The README headline from published rows, falling back to the recorded historical aggregates."""
    newest = published()[:1]
    return _render(text, {"headline": headline(*newest[0]) if newest else legacy_docs(headline_only=True)}, "README.md")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="check or update the eval versions and published results")
    parser.add_argument("--bump", nargs="*", default=[], metavar="TASK_ID")
    parser.add_argument("--publish", nargs=2, metavar=("RELEASE", "ROWS"), help="commit a results file as RELEASE")
    parser.add_argument(
        "--docs", action="store_true", help="regenerate eval docs, README headline and docs/results/summary.json"
    )
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
        DOCS.write_text(render_docs(DOCS.read_text(encoding="utf-8")), encoding="utf-8")
        README.write_text(render_readme(README.read_text(encoding="utf-8")), encoding="utf-8")
        RESULTS.mkdir(parents=True, exist_ok=True)
        (RESULTS / "summary.json").write_text(render_summary(), encoding="utf-8")
    problems = mismatches(list(tasks.values()))
    print("\n".join(problems) or f"{len(tasks)} tasks match versions.json")
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
