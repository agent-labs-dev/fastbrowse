"""Execute a pinned external corpus: selection, preflight, attempts and independent grades.

The loaders parse pinned bytes as data. External evaluator code runs only through a trusted, hash-pinned grader.
Completion and grading stay apart, so an attempt with no grader is ungraded with `passed: null`, never a failure.

    uv run --extra eval-data python -m fastbrowse.evals.corpus windtunnel --site-urls sites.json --out run/
    uv run --extra eval-data python -m fastbrowse.evals.corpus online-mind2web --sha256 <digest> --out run/ --execute
"""

import argparse
import asyncio
import hashlib
import json
import os
import sys
import time
from collections.abc import Awaitable, Callable, Mapping, Sequence
from pathlib import Path
from typing import Literal

import httpx
from pydantic import BaseModel, ConfigDict, Field, JsonValue, TypeAdapter

from fastbrowse.evals.datasets import (
    SOURCES,
    ExternalTask,
    Reachability,
    Source,
    Stratum,
    load_tasks,
    precheck,
    sample,
)
from fastbrowse.evals.live_tasks import Outcome
from fastbrowse.evals.status import Ending, normalize
from fastbrowse.evals.versions import provenance
from fastbrowse.models import Authorization, Limits, RunResult, Status

SourceId = Literal["online-mind2web", "windtunnel"]

SCHEMA_VERSION = 1

MAX_STEPS = 50
"""The benchmark plan's step cap, mapped here to `Limits.max_steps`."""


class _Model(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")


def _digest(payload: object) -> str:
    """A canonical digest: sorted keys and no whitespace, so two equal values never hash apart."""
    body = json.dumps(payload, sort_keys=True, separators=(",", ":"), default=str)
    return hashlib.sha256(body.encode()).hexdigest()


class TaskRef(_Model):
    """One task with the bytes it came from, so a row names a pin rather than a dataset that moves underneath it."""

    source: SourceId
    revision: str = Field(pattern=r"^[a-f0-9]{40}$")
    sha256: str = Field(pattern=r"^[a-f0-9]{64}$")
    id: str
    stratum: Stratum
    start: str
    task: str
    site: str | None = None
    metadata: dict[str, JsonValue] = {}
    digest: str = Field(pattern=r"^[a-f0-9]{64}$")
    """The task fields and its source pin hashed: two selections agree only when every digest agrees."""

    @classmethod
    def of(cls, source: Source, task: ExternalTask) -> "TaskRef":
        if source.sha256 is None:
            raise ValueError(f"{source.id}: execution requires the verified sha256 the loader pins")
        payload = {
            "source": task.source,
            "revision": source.revision,
            "sha256": source.sha256,
            "id": task.id,
            "stratum": task.stratum,
            "start": task.start,
            "task": task.task,
            "site": task.site,
            "metadata": task.metadata,
        }
        return cls(
            source=task.source,
            revision=source.revision,
            sha256=source.sha256,
            id=task.id,
            stratum=task.stratum,
            start=task.start,
            task=task.task,
            site=task.site,
            metadata=task.metadata,
            digest=_digest(payload),
        )


class Corpus(_Model):
    """A reproducible selection: the pin, the seed and the drawn tasks, hashed into one identity."""

    schema_version: Literal[1] = 1
    source: SourceId
    revision: str
    sha256: str
    seed: int
    per_stratum: int
    tasks: tuple[TaskRef, ...]
    digest: str
    """The selection's identity, hashed over its parameters and tasks in draw order. A run names this, not a date."""

    @classmethod
    def build(cls, source: Source, tasks: Sequence[ExternalTask], *, per_stratum: int, seed: int) -> "Corpus":
        if source.sha256 is None:
            raise ValueError(f"{source.id}: execution requires the verified sha256 the loader pins")
        chosen = sample(tasks, per_stratum=per_stratum, seed=seed)
        refs = tuple(TaskRef.of(source, task) for task in chosen)
        payload = {
            "schema_version": SCHEMA_VERSION,
            "source": source.id,
            "revision": source.revision,
            "sha256": source.sha256,
            "seed": seed,
            "per_stratum": per_stratum,
            "tasks": [ref.digest for ref in refs],
        }
        return cls(
            source=source.id,
            revision=source.revision,
            sha256=source.sha256,
            seed=seed,
            per_stratum=per_stratum,
            tasks=refs,
            digest=_digest(payload),
        )


class Preflight(_Model):
    """The harness's own HTTP look at a task's start address, before any agent spends a step on it."""

    task: str
    digest: str
    start: str
    reachable: bool
    status_code: int | None = None
    final_url: str | None = None
    reason: str


async def preflight(corpus: Corpus, http: httpx.AsyncClient) -> list[Preflight]:
    """The HTTP load check the plan's pre-screen starts from. It judges reachability only; feasibility, login walls
    and bot checks still need a person or a browser, so an unreachable task is reported as that and nothing more."""
    rows = []
    for ref in corpus.tasks:
        # `precheck` takes the loader's own task shape; the digest is carried alongside so the record identifies
        # the same bytes the selection did.
        external = ExternalTask.model_validate(
            {
                "source": ref.source,
                "id": ref.id,
                "start": ref.start,
                "task": ref.task,
                "stratum": ref.stratum,
                "metadata": ref.metadata,
            }
        )
        result: Reachability = await precheck(external, http)
        rows.append(
            Preflight(
                task=ref.id,
                digest=ref.digest,
                start=ref.start,
                reachable=result.reachable,
                status_code=result.status_code,
                final_url=result.final_url,
                reason=result.reason,
            )
        )
    return rows


class RunOutcome(_Model):
    """What one runner hands back about a single attempt, before any grading. A thin shape, so a test runner or a
    second arm does not have to build a full `RunResult`."""

    status: Status
    answer: str | None = None
    data: JsonValue = None
    final_url: str | None = None
    error: str | None = None
    quotes: tuple[tuple[str, str], ...] = ()
    dollars: float | None = None
    unknown_cost: bool = False


type Runner = Callable[[TaskRef, httpx.AsyncClient, Path], Awaitable[RunOutcome]]
type Grader = Callable[[TaskRef, Outcome], Awaitable["Grade"]]


class Grade(_Model):
    """One independent judgement of one attempt. The grader names itself and its version, so two grades are never
    compared as if one method produced both."""

    grader: str = Field(min_length=1)
    version: str = Field(min_length=1)
    passed: bool
    failure: str | None = None
    evidence: dict[str, JsonValue] = {}


class Attempt(_Model):
    """One agent attempt, with its completion and its grade kept apart. `passed` is null until a grader ran, and a
    pass requires both a pass from the grader and the agent's own completion."""

    schema_version: Literal[1] = 1
    corpus: str
    source: SourceId
    revision: str
    sha256: str
    task: str
    task_digest: str
    stratum: Stratum
    repeat: int
    arm: str
    raw_status: str | None
    completion: Ending
    completed: bool
    graded: bool
    grade: Grade | None = None
    passed: bool | None = None
    grade_error: str | None = None
    seconds: float
    dollars: float | None = None
    unknown_cost: bool = False
    answer: str | None = None
    data: JsonValue = None
    final_url: str | None = None
    error: str | None = None
    run_artifact: str | None = None
    run: dict[str, JsonValue] = {}


async def run_attempt(
    ref: TaskRef,
    *,
    corpus: str,
    repeat: int,
    runner: Runner,
    http: httpx.AsyncClient,
    downloads: Path,
    run: Mapping[str, JsonValue],
    grader: Grader | None = None,
    arm: str = "fastbrowse",
) -> Attempt:
    """One attempt at one task. A runner that raises is recorded as that attempt's error; one task must not discard
    the rest of a paid run. A grader that raises leaves the attempt ungraded and names why."""
    started = time.monotonic()
    try:
        outcome = await runner(ref, http, downloads)
    except Exception as exc:
        outcome = RunOutcome(status=Status.ERROR, error=f"{type(exc).__name__}: {exc}")
    seconds = time.monotonic() - started
    artifact = downloads / "run-result.json"
    completion = normalize(outcome.status.value)
    grade: Grade | None = None
    grade_error: str | None = None
    if grader is not None:
        try:
            grade = await grader(
                ref, Outcome(outcome.answer, outcome.data, outcome.final_url, quotes=outcome.quotes or None)
            )
        except Exception as exc:
            grade_error = f"grader raised {type(exc).__name__}: {exc}"
    passed = None if grade is None else grade.passed and completion is Ending.DONE
    return Attempt(
        corpus=corpus,
        source=ref.source,
        revision=ref.revision,
        sha256=ref.sha256,
        task=ref.id,
        task_digest=ref.digest,
        stratum=ref.stratum,
        repeat=repeat,
        arm=arm,
        raw_status=outcome.status.value,
        completion=completion,
        completed=completion is Ending.DONE,
        graded=grade is not None,
        grade=grade,
        passed=passed,
        grade_error=grade_error,
        seconds=round(seconds, 2),
        dollars=outcome.dollars,
        unknown_cost=outcome.unknown_cost,
        answer=outcome.answer,
        data=outcome.data,
        final_url=outcome.final_url,
        error=outcome.error,
        run_artifact=str(artifact) if artifact.is_file() else None,
        run=dict(run),
    )


async def execute(
    corpus: Corpus,
    *,
    runner: Runner,
    http: httpx.AsyncClient,
    downloads: Path,
    repeats: int = 1,
    run: Mapping[str, JsonValue],
    grader: Grader | None = None,
    arm: str = "fastbrowse",
    on_attempt: Callable[[Attempt], None] | None = None,
) -> list[Attempt]:
    """Every selected task, `repeats` times, in draw order. Attempts are written by the caller as they finish, so a
    run interrupted partway keeps what it already paid for."""
    if repeats < 1:
        raise ValueError("repeats must be positive")
    attempts = []
    for repeat in range(repeats):
        for ref in corpus.tasks:
            attempt = await run_attempt(
                ref,
                corpus=corpus.digest,
                repeat=repeat,
                runner=runner,
                http=http,
                # One directory per attempt, so a second repeat or a concurrent run never overwrites a trace.
                downloads=downloads / ref.digest[:16] / f"repeat-{repeat}",
                run=run,
                grader=grader,
                arm=arm,
            )
            attempts.append(attempt)
            if on_attempt is not None:
                on_attempt(attempt)
    return attempts


class Summary(_Model):
    """Counts that keep completion and independent grading apart. `passed` counts only attempts a grader called a
    pass and that also completed; `ungraded` attempts have no known pass state, which is not the same as failing."""

    corpus: str
    arm: str
    attempts: int
    repeats: int
    tasks: int
    completed: int
    ungraded: int
    graded: int
    passed: int
    note: str


def summarize(corpus: Corpus, attempts: Sequence[Attempt], *, arm: str = "fastbrowse", repeats: int = 1) -> Summary:
    graded = [attempt for attempt in attempts if attempt.graded]
    passed = [attempt for attempt in graded if attempt.passed]
    ungraded = len(attempts) - len(graded)
    if ungraded == len(attempts) and attempts:
        note = (
            "No independent grader ran, so every attempt is ungraded: completion counts what the agent reported "
            "about itself, not a benchmark score."
        )
    else:
        note = "A pass needs the grader's pass and the agent's own completion; an ungraded attempt has no pass state."
    return Summary(
        corpus=corpus.digest,
        arm=arm,
        attempts=len(attempts),
        repeats=repeats,
        tasks=len(corpus.tasks),
        completed=sum(attempt.completed for attempt in attempts),
        ungraded=ungraded,
        graded=len(graded),
        passed=len(passed),
        note=note,
    )


def _write_run_result(downloads: Path, result: RunResult) -> None:
    downloads.mkdir(parents=True, exist_ok=True)
    (downloads / "run-result.json").write_text(result.model_dump_json(indent=2) + "\n", encoding="utf-8")


def runner_for(
    *,
    cdp_url: str | None = None,
    browser_api_key: str | None = None,
    limits: Limits | None = None,
    authorization: Authorization | None = None,
) -> Runner:
    """The default runner: fastbrowse's own agent through the embedding API, so the corpus uses the same loop as
    every other caller rather than a second copy. Imported here, because the preflight path needs no browser."""

    async def run(ref: TaskRef, http: httpx.AsyncClient, downloads: Path) -> RunOutcome:
        from fastbrowse.run import run_task

        await asyncio.to_thread(downloads.mkdir, parents=True, exist_ok=True)
        await asyncio.to_thread((downloads / "attempt.json").write_text, ref.model_dump_json() + "\n", encoding="utf-8")
        result = await run_task(
            ref.task,
            start=ref.start,
            cdp_url=cdp_url,
            browser_api_key=browser_api_key,
            limits=limits or Limits(max_steps=MAX_STEPS),
            authorization=authorization,
            http=http,
            downloads=downloads,
        )
        await asyncio.to_thread(_write_run_result, downloads, result)
        return RunOutcome(
            status=result.status,
            answer=result.answer,
            data=result.data,
            final_url=result.final_url,
            error=result.error,
            quotes=tuple((evidence.url, evidence.quote) for evidence in result.evidence),
            dollars=None if result.cost.has_unknown else round(result.cost.known_dollars, 5),
            unknown_cost=result.cost.has_unknown,
        )

    return run


def _source(args: argparse.Namespace) -> Source:
    source = SOURCES[args.source]
    if args.sha256:
        if source.id != "online-mind2web":
            raise ValueError("--sha256 is only for the gated Online-Mind2Web pin")
        source = Source.model_validate(source.model_dump() | {"sha256": args.sha256})
    return source


def _build_grader(args: argparse.Namespace) -> Grader:
    # Imported here, not at module scope: external_grade imports corpus, so a top-level import would be a cycle.
    from fastbrowse.evals.external_grade import DEFAULT_TIMEOUT, predicate_grader

    if args.grader_command is None:
        return predicate_grader()
    if args.grader_code is None:
        raise ValueError("--grader-command requires --grader-code to identify the evaluator")
    if args.grader_sha256 is None:
        raise ValueError("--grader-command requires --grader-sha256 for the reviewed evaluator")
    return predicate_grader(
        args.grader_command,
        digest=args.grader_sha256,
        code=args.grader_code,
        timeout=args.grader_timeout if args.grader_timeout is not None else DEFAULT_TIMEOUT,
    )


def _exit_code(attempts: Sequence[Attempt], *, expected: int) -> int:
    """0 only when every expected attempt ran and passed. An ungraded, failed or missing attempt is nonzero."""
    if len(attempts) < expected:
        return 1
    return 0 if attempts and all(attempt.passed is True for attempt in attempts) else 1


def _recorded_argv(argv: Sequence[str]) -> list[str]:
    """CDP addresses and evaluator arguments can carry credentials, so neither belongs in retained artifacts."""
    recorded: list[str] = []
    arguments = iter(argv)
    for argument in arguments:
        if argument == "--cdp-url":
            recorded.extend((argument, "[redacted]"))
            next(arguments, None)
        elif argument.startswith("--cdp-url="):
            recorded.append("--cdp-url=[redacted]")
        elif argument == "--grader-command" or argument.startswith("--grader-command="):
            recorded.extend(("--grader-command", "[redacted]"))
            break
        else:
            recorded.append(argument)
    return recorded


async def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("source", choices=list(SOURCES))
    parser.add_argument("--cache", type=Path)
    parser.add_argument("--sha256", help="verified digest for the pinned gated Online-Mind2Web file")
    parser.add_argument("--site-urls", type=Path, help="WindTunnel site id to local URL, as JSON")
    parser.add_argument("--per-stratum", type=int, default=1)
    parser.add_argument("--seed", type=int, default=20260925)
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument(
        "--out", type=Path, required=True, help="directory for the corpus, preflight, attempts and summary"
    )
    parser.add_argument("--execute", action="store_true", help="run the agent; without it this is preflight only")
    parser.add_argument("--cdp-url", help="attach the run to a browser already running")
    parser.add_argument("--cloud", action="store_true", help="start a Browser Use Cloud browser from Settings")
    parser.add_argument("--authorize", action="store_true", help="authorize irreversible actions for these tasks")
    parser.add_argument(
        "--grader-command",
        nargs=argparse.REMAINDER,
        metavar="TOKEN",
        help="trusted evaluator as an argument vector, given last; stdin gets one JSON request, stdout one Grade",
    )
    parser.add_argument("--grader-code", type=Path, help="file whose sha256 pins --grader-command")
    parser.add_argument("--grader-sha256", help="required sha256 of --grader-code, so an edited evaluator is refused")
    parser.add_argument("--grader-timeout", type=float, help="seconds one grader command may run before it is killed")
    args = parser.parse_args(argv)
    if args.repeat < 1:
        parser.error("--repeat must be positive")
    if args.cloud and args.cdp_url:
        parser.error("--cloud and --cdp-url are two browsers; pick one")
    if args.execute and args.site_urls is None and args.source == "windtunnel":
        parser.error("windtunnel requires --site-urls")
    if args.grader_command is None and (
        args.grader_code is not None or args.grader_sha256 is not None or args.grader_timeout is not None
    ):
        parser.error("--grader-code, --grader-sha256 and --grader-timeout need --grader-command")
    try:
        sites = None if args.site_urls is None else json.loads(args.site_urls.read_text(encoding="utf-8"))
        source = _source(args)
        # Built now, before any network use: a command that is missing or does not match its pin is refused early.
        grader = _build_grader(args)
    except ValueError as exc:
        parser.error(str(exc))
    run = TypeAdapter(dict[str, JsonValue]).validate_python(provenance(source=source.id, argv=_recorded_argv(argv)))
    if args.out.exists():
        parser.error(f"refusing to overwrite existing output directory: {args.out}")
    async with httpx.AsyncClient(timeout=60) as http:
        try:
            tasks = await load_tasks(source, http, cache=args.cache, token=os.environ.get("HF_TOKEN"), site_urls=sites)
            corpus = Corpus.build(source, tasks, per_stratum=args.per_stratum, seed=args.seed)
        except (ValueError, httpx.HTTPError) as exc:
            parser.error(str(exc))
        try:
            await asyncio.to_thread(args.out.mkdir, parents=True, exist_ok=False)
        except FileExistsError:
            parser.error(f"refusing to overwrite existing output directory: {args.out}")
        (args.out / "corpus.json").write_text(
            json.dumps({"run": run, "corpus": corpus.model_dump(mode="json")}, indent=2) + "\n", encoding="utf-8"
        )
        checks = await preflight(corpus, http)
        (args.out / "preflight.jsonl").write_text(
            "".join(row.model_dump_json() + "\n" for row in checks), encoding="utf-8"
        )
        unreachable = sum(not row.reachable for row in checks)
        print(f"CORPUS {corpus.digest} {len(corpus.tasks)} tasks, {len(checks) - unreachable} reachable")
        if not args.execute:
            print(f"PREFLIGHT only; wrote {args.out}. Pass --execute to run the agent.")
            return 1 if unreachable else 0
        browser_api_key = None
        if args.cloud:
            from fastbrowse.clients.environment import load_settings

            browser_api_key = load_settings().browser_key()
        runner = runner_for(
            cdp_url=args.cdp_url,
            browser_api_key=browser_api_key,
            authorization=Authorization(irreversible_actions=args.authorize),
        )
        attempts: list[Attempt] = []
        try:
            with (args.out / "attempts.jsonl").open("w", encoding="utf-8") as out:

                def write(attempt: Attempt) -> None:
                    attempts.append(attempt)
                    out.write(attempt.model_dump_json() + "\n")
                    out.flush()
                    mark = "PASS" if attempt.passed else "ungraded" if not attempt.graded else "FAIL"
                    print(f"{mark:8} {attempt.task:20} {attempt.completion.value} {attempt.seconds}s", flush=True)

                await execute(
                    corpus,
                    runner=runner,
                    http=http,
                    downloads=args.out / "downloads",
                    repeats=args.repeat,
                    run=run,
                    grader=grader,
                    on_attempt=write,
                )
        except KeyboardInterrupt:
            print("interrupted; finished attempts are on disk", flush=True)
        except asyncio.CancelledError:
            print("cancelled; finished attempts are on disk", flush=True)
            raise
        finally:
            # Runs on interruption too: the summary counts every finished attempt, never an empty ledger.
            summary = summarize(corpus, attempts, repeats=args.repeat)
            (args.out / "summary.json").write_text(summary.model_dump_json(indent=2) + "\n", encoding="utf-8")
            print(
                f"{summary.completed}/{summary.attempts} completed, {summary.ungraded} ungraded, "
                f"{summary.passed} passed. {summary.note}"
            )
    return _exit_code(attempts, expected=len(corpus.tasks) * args.repeat)


if __name__ == "__main__":
    # A grader imports the canonical module, so executing its main avoids a second, incompatible Grade model.
    from fastbrowse.evals import corpus as corpus_module

    raise SystemExit(asyncio.run(corpus_module.main(sys.argv[1:])))
