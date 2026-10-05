"""The cross-arm corpus driver: reset ordering, per-arm completion, durable rows and the preflight refusals.

Everything here runs offline: the arms are fakes, the grader is the canonical native predicate scorer and the
reset is a small shell script pinned by its own digest. The point is the driver's own invariants, which a paid
run would otherwise be the only way to observe.
"""

import asyncio
import hashlib
import itertools
import json
import shlex
from collections.abc import Awaitable, Callable
from pathlib import Path
from typing import Literal

import httpx
import pytest
from pydantic import JsonValue

from fastbrowse.evals import corpus_compare as cc
from fastbrowse.evals import live
from fastbrowse.evals.corpus import Corpus, Grade, Grader, Preflight, TaskRef
from fastbrowse.evals.datasets import SOURCES, ExternalTask
from fastbrowse.evals.external_grade import predicate_grader
from fastbrowse.evals.live import ArmReport, ArmSpec
from fastbrowse.evals.live_tasks import Outcome
from fastbrowse.evals.status import Ending
from fastbrowse.models import Unavailable

ArmRunner = Callable[..., Awaitable[tuple[Outcome, ArmReport]]]


def _answer_task(task_id: str = "a-1") -> ExternalTask:
    return ExternalTask(
        source="windtunnel",
        id=task_id,
        start="https://capsule.test/posts",
        task="What does the title say?",
        stratum="answer",
        site="tailwind-nextjs-blog",
        metadata={"predicate": {"type": "answer", "contains": ["hello"]}},
    )


def _state_task(*, probe: bool, task_id: str = "p-1") -> ExternalTask:
    predicate: dict[str, object] = {"type": "state", "assert": {"equals": 1}}
    if probe:
        predicate["probe"] = "database"
    return ExternalTask(
        source="windtunnel",
        id=task_id,
        start="https://capsule.test/records",
        task="Create one record.",
        stratum="act-short" if not probe else "transaction",
        site="easyappointments",
        metadata={"predicate": predicate},
    )


def _auth_task(
    task_id: str = "auth-1",
    *,
    prompt: str = (
        "Log in as admin@admin.com with password admin123, then report which client invoice 1 is billed to."
    ),
    declared: bool | None = True,
) -> ExternalTask:
    metadata: dict[str, JsonValue] = {"predicate": {"type": "state", "assert": {"equals": 1}}}
    if declared is not None:
        metadata["auth"] = declared
    return ExternalTask(
        source="windtunnel",
        id=task_id,
        start="https://capsule.test/login",
        task=prompt,
        stratum="act-short",
        site="idurar-erp-crm",
        metadata=metadata,
    )


def _corpus(tasks: list[ExternalTask] | None = None, *, seed: int = 7) -> Corpus:
    chosen = tasks if tasks is not None else [_answer_task(), _state_task(probe=False)]
    return Corpus.build(SOURCES["windtunnel"], chosen, per_stratum=1, seed=seed)


def _spec(
    runner: ArmRunner,
    *,
    tier: Literal["A", "hosted"] = "A",
    pin: str = "pin",
    default: bool = True,
) -> ArmSpec:
    return ArmSpec(runner=runner, pin=pin, env_allowlist=(), tier=tier, default=default)


def _reset(tmp_path: Path, log: Path) -> cc.ResetSpec:
    """A real, pinned reset command: a shell script that appends one line to the log it is handed."""
    script = tmp_path / "reset.sh"
    script.write_text('#!/bin/sh\nprintf "reset\\n" >> "$1"\n', encoding="utf-8")
    script.chmod(0o755)
    return cc.ResetSpec(
        command=(str(script), str(log), "{site}", "{run_id}"),
        code=script,
        digest=hashlib.sha256(script.read_bytes()).hexdigest(),
    )


def _arm_runner(
    arm: str,
    log: Path,
    *,
    status: str = "complete",
    answered: bool | None = None,
    answer: str = "hello world",
    dollars: float | None = 0.02,
    error: Exception | None = None,
) -> ArmRunner:
    async def run(task: object, http: object, downloads: Path, **kwargs: object) -> tuple[Outcome, ArmReport]:
        with log.open("a", encoding="utf-8") as handle:
            handle.write(f"{arm}\n")
        if error is not None:
            raise error
        return Outcome(answer, None, "https://capsule.test/posts"), ArmReport(
            status=status, seconds=0.1, dollars=dollars, answered=answered
        )

    return run


def _http() -> httpx.AsyncClient:
    return httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(200)))


_ATTEMPT_DIR = itertools.count()


async def _run_one(
    tmp_path: Path,
    *,
    spec: ArmSpec,
    reset: cc.ResetSpec,
    grader: Grader,
    task: TaskRef,
    corpus: Corpus,
    log: Path,
) -> cc.ArmAttempt:
    async with _http() as http:
        return await cc.run_arm_attempt(
            task,
            corpus=corpus,
            repeat=0,
            retry=0,
            spec=spec,
            arm="fastbrowse",
            http=http,
            downloads=tmp_path / "downloads" / str(next(_ATTEMPT_DIR)),
            run={},
            grader=grader,
            reset=reset,
            run_id="run1",
            authorize=False,
        )


def test_remote_endpoints_refuse_localhost_credentials_and_query_secrets() -> None:
    local = [
        "127.0.0.1",
        "localhost",
        "10.0.0.5",
        "100.64.1.2",
        "host.docker.internal",
        "printer.local",
        "intranet",
        "svc.internal",
        "127.0.0.1.nip.io",
        "127.1",
        "10.1",
        "192.168.1",
        "0x7f.0.0.1",
        "fe80::1",
        "::1",
    ]
    assert all(cc.is_local_host(host) for host in local)
    assert not cc.is_local_host("ab.trycloudflare.com") and not cc.is_local_host("example.com")
    assert not cc.is_local_host("1.2.3.4") and not cc.is_local_host("8.8.8.8")
    assert not cc.is_local_host("2001:4860:4860::8888")
    assert cc.validate_site_urls({"site": "https://[2001:4860:4860::8888]"}, remote=True)
    with pytest.raises(ValueError, match="public tunnel"):
        cc.validate_site_urls({"site": "http://127.0.0.1:34871"}, remote=True)
    with pytest.raises(ValueError, match="public tunnel"):
        cc.validate_site_urls({"site": "https://127.0.0.1.nip.io"}, remote=True)
    with pytest.raises(ValueError, match="public tunnel"):
        cc.validate_site_urls({"site": "http://127.1:34871"}, remote=True)
    cc.validate_site_urls({"site": "http://127.0.0.1:34871"}, remote=False)
    assert cc.validate_site_urls({"site": "https://ab.trycloudflare.com"}, remote=True)
    with pytest.raises(ValueError, match="bare origin"):
        cc.validate_site_urls({"site": "https://ab.trycloudflare.com/?token=secret"}, remote=True)
    with pytest.raises(ValueError, match="bare origin"):
        cc.validate_site_urls({"site": "https://user:pass@ab.trycloudflare.com"}, remote=True)


def test_reset_spec_pins_the_reviewed_program_and_refuses_credential_names(tmp_path: Path) -> None:
    script = tmp_path / "reset.sh"
    script.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    digest = hashlib.sha256(script.read_bytes()).hexdigest()
    with pytest.raises(ValueError, match="must name the program"):
        cc.ResetSpec(command=("/bin/true",), code=script, digest=digest)
    with pytest.raises(ValueError, match="unknown placeholder"):
        cc.ResetSpec(command=(str(script), "{port}"), code=script, digest=digest)
    spec = cc.ResetSpec(command=(str(script),), code=script, digest=digest)
    assert spec.resolved(site="s", run_id="r") == (str(script),)
    with pytest.raises(ValueError, match="credential"):
        cc.reset_environment(
            cc.ResetSpec(command=(str(script),), code=script, digest=digest, environment={"MY_SECRET_VALUE": "1"})
        )
    with pytest.raises(ValueError, match="credential"):
        cc.reset_environment(
            cc.ResetSpec(
                command=(str(script),),
                code=script,
                digest=digest,
                environment={"OPENROUTER_API_KEY": "sk-live"},
            )
        )


async def test_a_changed_reset_digest_refuses_to_run_and_leaves_no_log(tmp_path: Path) -> None:
    log = tmp_path / "reset.log"
    spec = _reset(tmp_path, log)
    stale = spec.model_copy(update={"digest": "0" * 64})
    record = await cc.run_reset(stale, site="easyappointments", run_id="run1", payload=b"{}")
    assert record.ok is False and "changed since it was pinned" in (record.error or "")
    assert not log.exists()


async def test_reset_runs_before_every_arm_attempt_in_rotated_order(tmp_path: Path) -> None:
    log = tmp_path / "order.log"
    reset = _reset(tmp_path, log)
    corpus = _corpus()
    specs = {
        "fastbrowse": _spec(_arm_runner("fb", log), tier="A"),
        "browser-use": _spec(_arm_runner("bu", log, status="stopped", answered=True), tier="hosted"),
    }
    async with _http() as http:
        await cc.run_comparison(
            corpus,
            specs=specs,
            arms=("fastbrowse", "browser-use"),
            http=http,
            downloads=tmp_path / "downloads",
            run={},
            grader=predicate_grader(),
            reset=reset,
            run_id="run1",
            repeats=2,
        )
    lines = log.read_text(encoding="utf-8").splitlines()
    # One reset immediately precedes every arm run, and no arm ever starts from a capsule the run did not reset.
    assert lines[0::2] == ["reset"] * (len(lines) // 2)
    # Two tasks per arm and two repeats, arm order rotated each repeat: arms pair on a task before moving on.
    assert lines[1::2] == ["fb", "bu", "fb", "bu", "bu", "fb", "bu", "fb"]


async def test_completion_is_each_arms_own_and_correctness_is_the_grade_alone(tmp_path: Path) -> None:
    log = tmp_path / "reset.log"
    reset = _reset(tmp_path, log)
    corpus = _corpus([_answer_task()])
    task = corpus.tasks[0]
    grader = predicate_grader()

    hosted = _spec(_arm_runner("bu", log, status="stopped", answered=True), tier="hosted")
    attempt = await _run_one(tmp_path, spec=hosted, reset=reset, grader=grader, task=task, corpus=corpus, log=log)
    assert isinstance(attempt.attempt.grade, Grade) and attempt.attempt.grade.grader == "windtunnel-answer"
    assert attempt.attempt.grade.passed is True
    # The hosted agent's `stopped` with an answer is its completion, unlike the same status on fastbrowse.
    assert attempt.attempt.completion is Ending.DONE and attempt.attempt.completed is True
    assert attempt.correct is True and attempt.attempt.passed is True

    stopped = _spec(_arm_runner("fb", log, status="stopped"), tier="A")
    second = await _run_one(tmp_path, spec=stopped, reset=reset, grader=grader, task=task, corpus=corpus, log=log)
    assert second.attempt.grade is not None and second.attempt.grade.passed is True
    # The grader passed the answer, but this arm never completed, so correct and completed tell different stories.
    assert second.correct is True and second.attempt.completed is False and second.attempt.passed is False


async def test_an_unpriced_or_crashed_attempt_reports_unknown_cost(tmp_path: Path) -> None:
    log = tmp_path / "reset.log"
    reset = _reset(tmp_path, log)
    corpus = _corpus([_answer_task()])
    task = corpus.tasks[0]
    grader = predicate_grader()

    unknown = _spec(_arm_runner("fb", log, dollars=None))
    attempt = await _run_one(tmp_path, spec=unknown, reset=reset, grader=grader, task=task, corpus=corpus, log=log)
    assert attempt.attempt.dollars is None and attempt.attempt.unknown_cost is True

    crashed = _spec(_arm_runner("fb", log, error=Unavailable("provider down")))
    second = await _run_one(tmp_path, spec=crashed, reset=reset, grader=grader, task=task, corpus=corpus, log=log)
    assert second.attempt.completion is Ending.UNAVAILABLE
    assert second.attempt.graded is False and second.correct is None
    assert second.attempt.dollars is None and second.attempt.unknown_cost is True
    assert second.attempt.error is not None and "Unavailable" in second.attempt.error

    priced = _spec(_arm_runner("fb", log, dollars=0.03))
    third = await _run_one(tmp_path, spec=priced, reset=reset, grader=grader, task=task, corpus=corpus, log=log)
    totals = cc.arm_totals([attempt, third], "fastbrowse")
    # One unknown price makes the arm's total unknown, while the known floor is still named.
    assert totals.dollars is None and totals.known_dollars == 0.03 and totals.unknown_cost == 1


async def test_a_failed_reset_is_a_durable_ungraded_row_and_the_arm_never_starts(tmp_path: Path) -> None:
    log = tmp_path / "reset.log"
    reset = _reset(tmp_path, log)
    stale = reset.model_copy(update={"digest": "0" * 64})
    corpus = _corpus([_answer_task()])
    called = False

    async def runner(task: object, http: object, downloads: Path, **kwargs: object) -> tuple[Outcome, ArmReport]:
        nonlocal called
        called = True
        return Outcome("hello", None, None), ArmReport(status="complete", seconds=0.1, dollars=0.0)

    attempt = await _run_one(
        tmp_path,
        spec=_spec(runner),
        reset=stale,
        grader=predicate_grader(),
        task=corpus.tasks[0],
        corpus=corpus,
        log=log,
    )
    assert called is False
    assert attempt.reset.ok is False and attempt.attempt.completion is Ending.ERROR
    assert attempt.attempt.graded is False and attempt.correct is None
    assert attempt.attempt.dollars == 0.0 and attempt.attempt.unknown_cost is False
    assert "reset failed" in (attempt.attempt.grade_error or "")


def test_an_answer_predicate_cannot_judge_an_arm_that_returns_no_answer() -> None:
    answer = _corpus([_answer_task()]).tasks[0]
    assert cc.arm_eligible("jev-ultrafast", answer) == cc.NO_ANSWER_REASON
    assert cc.arm_eligible("fastbrowse", answer) is None
    state = _corpus([_state_task(probe=False)]).tasks[0]
    assert cc.arm_eligible("jev-ultrafast", state) is None


def test_an_arm_that_cannot_resolve_fixture_credentials_is_skipped_for_a_different_reason() -> None:
    auth = _corpus([_auth_task()]).tasks[0]
    # The declared-auth task is a state task, so the answer closure does not apply; the credential closure does.
    assert cc.arm_eligible("jev-ultrafast", auth) == cc.NO_CREDENTIAL_REASON
    assert cc.arm_eligible("jev-ultrafast", auth) != cc.NO_ANSWER_REASON
    # An arm whose runner resolves named secrets is judged on the same task.
    assert cc.arm_eligible("browser-use", auth) is None
    assert cc.arm_eligible("fastbrowse", auth) is None


def test_a_task_that_declares_no_login_is_never_skipped_for_credentials() -> None:
    for task in (_auth_task(declared=False), _auth_task(declared=None), _state_task(probe=False)):
        ref = _corpus([task]).tasks[0]
        assert cc.arm_eligible("jev-ultrafast", ref) is None, task.id


async def test_skipped_ineligible_pairs_are_recorded_not_faked(tmp_path: Path) -> None:
    log = tmp_path / "reset.log"
    reset = _reset(tmp_path, log)
    corpus = _corpus([_answer_task(), _state_task(probe=False)])
    specs = {
        "fastbrowse": _spec(_arm_runner("fb", log)),
        "jev-ultrafast": _spec(_arm_runner("uf", log)),
    }
    async with _http() as http:
        result = await cc.run_comparison(
            corpus,
            specs=specs,
            arms=("fastbrowse", "jev-ultrafast"),
            http=http,
            downloads=tmp_path / "downloads",
            run={},
            grader=predicate_grader(),
            reset=reset,
            run_id="run1",
        )
    answer_digest = next(ref.digest for ref in corpus.tasks if ref.stratum == "answer")
    skipped = [item for item in result.skipped if item.arm == "jev-ultrafast"]
    assert [item.task_digest for item in skipped] == [answer_digest]
    assert all(item.attempt.task_digest != answer_digest for item in result.attempts if item.arm == "jev-ultrafast")


async def test_a_fixture_auth_task_drops_from_the_pairing_when_one_arm_cannot_log_in(tmp_path: Path) -> None:
    log = tmp_path / "reset.log"
    reset = _reset(tmp_path, log)
    corpus = Corpus.build(
        SOURCES["windtunnel"], [_state_task(probe=False, task_id="plain-1"), _auth_task()], per_stratum=2, seed=7
    )
    plain = next(ref for ref in corpus.tasks if ref.id == "plain-1")
    auth = next(ref for ref in corpus.tasks if ref.id == "auth-1")
    arms = ("fastbrowse", "jev-ultrafast")
    specs = {arm: _spec(_arm_runner(arm, log)) for arm in arms}

    async def grades(ref: TaskRef, outcome: Outcome) -> Grade:
        return Grade(grader="fixture", version="1", passed=True)

    repeats = 3
    async with _http() as http:
        result = await cc.run_comparison(
            corpus,
            specs=specs,
            arms=arms,
            http=http,
            downloads=tmp_path / "downloads",
            run={},
            grader=grades,
            reset=reset,
            run_id="run1",
            repeats=repeats,
        )
    # jev-ultrafast never attempted the declared-auth task, and the skip names the credential closure rather than
    # the answer one.
    skipped = [item for item in result.skipped if item.arm == "jev-ultrafast"]
    assert [(item.task_digest, item.reason) for item in skipped] == [(auth.digest, cc.NO_CREDENTIAL_REASON)] * repeats
    # fastbrowse did grade it, but a task only one requested arm could attempt is not a paired denominator.
    mine = [item for item in result.attempts if item.arm == "fastbrowse" and item.attempt.task_digest == auth.digest]
    assert len(mine) == repeats and all(item.attempt.graded for item in mine)
    report = cc.build_report(
        corpus,
        arms=arms,
        attempts=result.attempts,
        skipped=result.skipped,
        truncated=False,
        repeats=repeats,
        sites={},
        remote=False,
        setup_seconds=None,
        preflight_seconds=0.0,
        prepare_seconds={},
        grader={},
        reset={},
        specs=specs,
    )
    assert {digest for digest, _ in report.paired_tasks} == {plain.digest}
    assert report.paired["fastbrowse"].graded == report.paired["jev-ultrafast"].graded == repeats


def test_the_headline_refuses_without_fair_repeats_and_pairs_only_equal_denominators(tmp_path: Path) -> None:
    log = tmp_path / "reset.log"
    reset = _reset(tmp_path, log)
    corpus = _corpus([_answer_task()])
    task = corpus.tasks[0]
    arms = ("fastbrowse", "browser-use")

    def report(attempts: list[cc.ArmAttempt]) -> cc.ComparisonReport:
        return cc.build_report(
            corpus,
            arms=arms,
            attempts=attempts,
            skipped=[],
            truncated=False,
            repeats=3,
            sites={},
            remote=False,
            setup_seconds=None,
            preflight_seconds=0.0,
            prepare_seconds={},
            grader={},
            reset={},
            specs={arm: _spec(_arm_runner(arm, log)) for arm in arms},
        )

    def pairs(*, repeats: int, arms_graded: dict[str, tuple[int, ...]]) -> list[cc.ArmAttempt]:
        return [
            _arm_attempt(reset, task, arm=arm, repeat=repeat)
            for arm, graded_repeats in arms_graded.items()
            for repeat in range(repeats)
            if repeat in graded_repeats
        ]

    # One arm graded and the other missing entirely: not a pair, so no claim.
    one_sided = report(pairs(repeats=3, arms_graded={"fastbrowse": (0, 1, 2)}))
    assert one_sided.headline.startswith("INSUFFICIENT PAIRED DATA")
    assert one_sided.paired_tasks == ()

    # Both arms graded, but one repeat short of the three-repeat floor: the task is not paired at all.
    two = report(pairs(repeats=2, arms_graded={"fastbrowse": (0, 1), "browser-use": (0, 1)}))
    assert two.headline.startswith("INSUFFICIENT PAIRED DATA")
    assert two.paired_tasks == ()
    assert two.paired["fastbrowse"].graded == two.paired["browser-use"].graded == 0
    # Every raw attempt survives the exclusion, so the under-repeated task is still in the report.
    assert len(two.attempts) == 4

    # The second arm's repeat 2 is ungraded, so only repeats 0 and 1 pair: again below the floor, so excluded.
    missing = report(pairs(repeats=3, arms_graded={"fastbrowse": (0, 1, 2), "browser-use": (0, 1)}))
    assert missing.paired_tasks == ()
    assert missing.paired["fastbrowse"].graded == missing.paired["browser-use"].graded == 0
    assert len(missing.attempts) == 5

    # Every requested arm graded at every repeat: a fair headline over three repeats.
    full = report(pairs(repeats=3, arms_graded={"fastbrowse": (0, 1, 2), "browser-use": (0, 1, 2)}))
    assert full.headline.startswith("paired on 3 task-repeats across 1 tasks")
    assert full.paired["fastbrowse"].graded == full.paired["browser-use"].graded == 3
    assert full.paired["fastbrowse"].correct == 3 and full.paired["browser-use"].correct == 3


def _report(
    corpus: Corpus,
    arms: tuple[str, ...],
    attempts: list[cc.ArmAttempt],
    *,
    repeats: int,
    specs: dict[str, ArmSpec],
    truncated: bool = False,
    interrupted: bool = False,
) -> cc.ComparisonReport:
    return cc.build_report(
        corpus,
        arms=arms,
        attempts=attempts,
        skipped=[],
        truncated=truncated,
        interrupted=interrupted,
        repeats=repeats,
        sites={},
        remote=False,
        setup_seconds=None,
        preflight_seconds=0.0,
        prepare_seconds={},
        grader={},
        reset={},
        specs=specs,
    )


def test_scattered_partial_repeats_do_not_add_up_to_a_headline(tmp_path: Path) -> None:
    """Two tasks with two paired repeats each at different indices must not share a three-repeat headline."""
    log = tmp_path / "reset.log"
    reset = _reset(tmp_path, log)
    corpus = _corpus([_answer_task("a-1"), _state_task(probe=False, task_id="s-1")])
    first, second = corpus.tasks
    arms = ("fastbrowse", "browser-use")
    specs = {arm: _spec(_arm_runner(arm, log)) for arm in arms}
    # Task one pairs at repeats 1 and 2; task two pairs at repeats 0 and 1. The union is {0, 1, 2} but neither
    # task has three graded repeats of its own, so the per-task floor must refuse both.
    attempts = [
        _arm_attempt(reset, ref, arm=arm, repeat=repeat)
        for ref, repeats in ((first, (1, 2)), (second, (0, 1)))
        for repeat in repeats
        for arm in arms
    ]
    report = _report(corpus, arms, attempts, repeats=3, specs=specs)
    assert report.paired_tasks == ()
    assert report.headline.startswith("INSUFFICIENT PAIRED DATA")
    assert report.paired["fastbrowse"].graded == report.paired["browser-use"].graded == 0
    assert len(report.attempts) == 8


def test_an_inadequate_task_is_excluded_from_totals_but_kept_in_diagnostics(tmp_path: Path) -> None:
    """One adequate task headlines while a two-repeat task stays out of every total and every raw row stays."""
    log = tmp_path / "reset.log"
    reset = _reset(tmp_path, log)
    corpus = _corpus([_answer_task("a-1"), _state_task(probe=False, task_id="s-1")])
    adequate, inadequate = corpus.tasks
    arms = ("fastbrowse", "browser-use")
    specs = {arm: _spec(_arm_runner(arm, log)) for arm in arms}
    attempts = [
        _arm_attempt(reset, ref, arm=arm, repeat=repeat)
        for ref, repeats in ((adequate, (0, 1, 2)), (inadequate, (0, 1)))
        for repeat in repeats
        for arm in arms
    ]
    report = _report(corpus, arms, attempts, repeats=3, specs=specs)
    assert {digest for digest, _ in report.paired_tasks} == {adequate.digest}
    assert len(report.paired_tasks) == 3
    assert report.paired["fastbrowse"].graded == report.paired["browser-use"].graded == 3
    assert report.headline.startswith("paired on 3 task-repeats across 1 tasks")
    # All ten physical rows are retained, including the four the inadequate task produced.
    assert len(report.attempts) == 10
    assert inadequate.digest in {record.task_digest for record in report.attempts}


@pytest.mark.parametrize("stop", ["truncated", "interrupted"])
def test_a_stopped_draw_cannot_headline_its_completed_subset(tmp_path: Path, stop: str) -> None:
    log = tmp_path / "reset.log"
    reset = _reset(tmp_path, log)
    corpus = _corpus([_answer_task()])
    arms = ("fastbrowse", "browser-use")
    specs = {arm: _spec(_arm_runner(arm, log)) for arm in arms}
    attempts = [_arm_attempt(reset, corpus.tasks[0], arm=arm, repeat=repeat) for repeat in range(3) for arm in arms]
    report = _report(
        corpus, arms, attempts, repeats=3, specs=specs, truncated=stop == "truncated", interrupted=stop == "interrupted"
    )
    assert len(report.paired_tasks) == 3
    assert len(report.attempts) == 6
    assert report.headline.startswith("INSUFFICIENT PAIRED DATA")


def _record(reset: cc.ResetSpec) -> cc.ResetRecord:
    return cc.ResetRecord(
        command=reset.command,
        code=str(reset.code),
        sha256=reset.digest,
        environment=(),
        ok=True,
        seconds=0.0,
    )


def _attempt(ref: TaskRef, *, graded: bool, passed: bool, arm: str = "fastbrowse", repeat: int = 0) -> cc.Attempt:
    grade = Grade(grader="fixture", version="1", passed=passed) if graded else None
    return cc.Attempt(
        corpus="c" * 64,
        source=ref.source,
        revision=ref.revision,
        sha256=ref.sha256,
        task=ref.id,
        task_digest=ref.digest,
        stratum=ref.stratum,
        repeat=repeat,
        arm=arm,
        raw_status="complete",
        completion=Ending.DONE,
        completed=True,
        graded=graded,
        grade=grade,
        passed=(grade.passed and True) if grade is not None else None,
        seconds=0.1,
    )


def _arm_attempt(
    reset: cc.ResetSpec, ref: TaskRef, *, arm: str, repeat: int, graded: bool = True, passed: bool = True
) -> cc.ArmAttempt:
    return cc.ArmAttempt(
        arm=arm,
        pin="pin",
        tier="A",
        reset=_record(reset),
        attempt=_attempt(ref, graded=graded, passed=passed, arm=arm, repeat=repeat),
    )


async def test_a_reported_site_or_browser_outage_is_unavailable_and_retryable(tmp_path: Path) -> None:
    log = tmp_path / "reset.log"
    reset = _reset(tmp_path, log)
    corpus = _corpus([_answer_task()])

    async def runner(task: object, http: object, downloads: Path, **kwargs: object) -> tuple[Outcome, ArmReport]:
        return Outcome(None, None, None), ArmReport(status="error", seconds=0.1, dollars=0.0, failure_class="site_http")

    attempt = await _run_one(
        tmp_path,
        spec=_spec(runner),
        reset=reset,
        grader=predicate_grader(),
        task=corpus.tasks[0],
        corpus=corpus,
        log=log,
    )
    assert attempt.attempt.completion is Ending.UNAVAILABLE
    assert attempt.attempt.graded is False and attempt.correct is None


async def test_a_retry_resets_again_and_is_its_own_physical_row(tmp_path: Path) -> None:
    log = tmp_path / "retry.log"
    reset = _reset(tmp_path, log)
    corpus = _corpus([_answer_task()])
    calls = {"n": 0}

    async def runner(task: object, http: object, downloads: Path, **kwargs: object) -> tuple[Outcome, ArmReport]:
        calls["n"] += 1
        with log.open("a", encoding="utf-8") as handle:
            handle.write(f"arm-{calls['n']}\n")
        if calls["n"] == 1:
            raise Unavailable("provider down")
        return Outcome("hello world", None, None), ArmReport(status="complete", seconds=0.1, dollars=0.02)

    async with _http() as http:
        result = await cc.run_comparison(
            corpus,
            specs={"fastbrowse": _spec(runner)},
            arms=("fastbrowse",),
            http=http,
            downloads=tmp_path / "downloads",
            run={},
            grader=predicate_grader(),
            reset=reset,
            run_id="run1",
            retries=1,
        )
    assert [item.retry for item in result.attempts] == [0, 1]
    assert result.attempts[0].attempt.completion is Ending.UNAVAILABLE
    assert result.attempts[1].attempt.completion is Ending.DONE
    # A retry is a fresh physical attempt, so it resets first and both rows are kept.
    assert log.read_text(encoding="utf-8").splitlines() == ["reset", "arm-1", "reset", "arm-2"]


def test_recorded_arguments_redact_the_grader_and_reset_commands() -> None:
    recorded = cc._recorded_argv(
        [
            "--seed",
            "7",
            "--reset-command",
            "reset.sh --token abc",
            "--grader-command",
            "python",
            "grader.py",
            "--token",
            "secret",
        ]
    )
    assert recorded == ["--seed", "7", "--reset-command", "[redacted]", "--grader-command", "[redacted]"]
    assert "abc" not in json.dumps(recorded) and "secret" not in json.dumps(recorded)
    assert cc._recorded_argv(["--reset-command=reset.sh --token abc", "--seed", "7"]) == [
        "--reset-command=[redacted]",
        "--seed",
        "7",
    ]


async def test_a_pinned_reset_never_leaks_its_arguments_or_environment(tmp_path: Path) -> None:
    log = tmp_path / "reset.log"
    script = tmp_path / "reset.sh"
    script.write_text('#!/bin/sh\necho "$1" 1>&2\nexit 3\n', encoding="utf-8")
    script.chmod(0o755)
    secret = "s3cr3t-token"
    spec = cc.ResetSpec(
        command=(str(script), secret, str(log)),
        code=script,
        digest=hashlib.sha256(script.read_bytes()).hexdigest(),
        environment={"DATABASE_URL": "postgres://user:pw@host/db"},
    )
    record = await cc.run_reset(spec, site="s", run_id="run1", payload=b"{}")
    assert record.ok is False
    # The failed reset echoes the secret to stderr; neither the record nor its error may retain it.
    assert secret not in record.model_dump_json()
    assert "postgres://user:pw@host/db" not in record.model_dump_json()
    assert record.command == (str(script),)
    assert cc._redacted_command(spec.command) == [str(script)]


async def test_a_reset_that_cannot_substitute_or_read_returns_a_failed_record(tmp_path: Path) -> None:
    script = tmp_path / "reset.sh"
    script.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    digest = hashlib.sha256(script.read_bytes()).hexdigest()
    # `{site}` needs a capsule site id; a task without one is a failed reset record, not a crash mid-run.
    bad_site = cc.ResetSpec(command=(str(script), "{site}"), code=script, digest=digest)
    record = await cc.run_reset(bad_site, site="", run_id="run1", payload=b"{}")
    assert record.ok is False and "invalid capsule site id" in (record.error or "")
    # A pinned code file that has since gone is a failed reset record too, not an OSError that abandons the run.
    missing_code = tmp_path / "gone.sh"
    missing = cc.ResetSpec(command=(str(missing_code),), code=missing_code, digest=digest)
    second = await cc.run_reset(missing, site="s", run_id="run1", payload=b"{}")
    assert second.ok is False and "FileNotFoundError" in (second.error or "")


async def test_main_refuses_execute_without_a_pinned_reset(tmp_path: Path) -> None:
    sites = _write_sites(tmp_path, {"tailwind-nextjs-blog": "http://127.0.0.1:34871"})
    out = tmp_path / "run"
    with pytest.raises(SystemExit) as info:
        await cc.main(
            [
                "windtunnel",
                "--site-urls",
                str(sites),
                "--arms",
                "fastbrowse",
                "--out",
                str(out),
                "--execute",
                "--seed",
                "7",
            ]
        )
    assert info.value.code == 2
    assert not out.exists()


async def test_main_rejects_a_reset_whose_digest_does_not_match(tmp_path: Path) -> None:
    sites = _write_sites(tmp_path, {"tailwind-nextjs-blog": "http://127.0.0.1:34871"})
    spec = _reset(tmp_path, tmp_path / "r.log")
    out = tmp_path / "run"
    with pytest.raises(SystemExit) as info:
        await cc.main(
            [
                "windtunnel",
                "--site-urls",
                str(sites),
                "--arms",
                "fastbrowse",
                "--out",
                str(out),
                "--execute",
                "--seed",
                "7",
                "--reset-command",
                shlex.join(spec.command),
                "--reset-code",
                str(spec.code),
                "--reset-sha256",
                "0" * 64,
            ]
        )
    assert info.value.code == 2
    assert not out.exists()


class _FakeClient:
    async def __aenter__(self) -> "_FakeClient":
        return self

    async def __aexit__(self, *exc: object) -> bool:
        return False

    async def get(self, url: str, **kwargs: object) -> httpx.Response:
        return httpx.Response(200, request=httpx.Request("GET", url))


def _write_sites(tmp_path: Path, body: dict[str, str]) -> Path:
    path = tmp_path / "sites.json"
    path.write_text(json.dumps(body), encoding="utf-8")
    return path


def _reset_args(tmp_path: Path) -> tuple[list[str], cc.ResetSpec]:
    log = tmp_path / "main-reset.log"
    spec = _reset(tmp_path, log)
    args = [
        "--reset-command",
        shlex.join(spec.command),
        "--reset-code",
        str(spec.code),
        "--reset-sha256",
        spec.digest,
    ]
    return args, spec


def _reachable(corpus: Corpus) -> list[Preflight]:
    return [
        Preflight(task=ref.id, digest=ref.digest, start=ref.start, reachable=True, reason="http_ok")
        for ref in corpus.tasks
    ]


async def test_main_refuses_a_hosted_arm_without_a_public_tunnel(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    sites = _write_sites(tmp_path, {"tailwind-nextjs-blog": "http://127.0.0.1:34871"})
    reset_args, _ = _reset_args(tmp_path)
    out = tmp_path / "run"
    with pytest.raises(SystemExit) as info:
        await cc.main(
            [
                "windtunnel",
                "--site-urls",
                str(sites),
                "--arms",
                "browser-use",
                "--out",
                str(out),
                "--execute",
                "--seed",
                "7",
                *reset_args,
            ]
        )
    assert info.value.code == 2
    assert not out.exists()

    local_public = _write_sites(tmp_path, {"tailwind-nextjs-blog": "http://127.0.0.1:34871"})
    with pytest.raises(SystemExit) as info:
        await cc.main(
            [
                "windtunnel",
                "--site-urls",
                str(sites),
                "--public-site-urls",
                str(local_public),
                "--arms",
                "browser-use",
                "--out",
                str(out),
                "--execute",
                "--seed",
                "7",
                *reset_args,
            ]
        )
    assert info.value.code == 2
    assert not out.exists()


async def test_main_refuses_a_probe_task_without_a_grader_before_spending(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    sites = _write_sites(tmp_path, {"easyappointments": "http://127.0.0.1:34871"})

    async def fake_load(source: object, http: object, **kwargs: object) -> list[ExternalTask]:
        return [_state_task(probe=True)]

    async def fake_preflight(corpus: Corpus, http: object) -> list[Preflight]:
        return _reachable(corpus)

    monkeypatch.setattr(cc, "load_tasks", fake_load)
    monkeypatch.setattr(cc, "preflight", fake_preflight)
    monkeypatch.setattr(cc.httpx, "AsyncClient", lambda **kwargs: _FakeClient())
    reset_args, _ = _reset_args(tmp_path)
    out = tmp_path / "run"
    code = await cc.main(
        [
            "windtunnel",
            "--site-urls",
            str(sites),
            "--arms",
            "fastbrowse",
            "--out",
            str(out),
            "--execute",
            "--seed",
            "7",
            *reset_args,
        ]
    )
    assert code == 2
    assert not (out / "attempts.jsonl").exists()


async def test_main_records_an_interrupted_attempt_with_unknown_cost_and_an_accurate_report(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    sites = _write_sites(tmp_path, {"easyappointments": "http://127.0.0.1:34871"})
    seen = {"n": 0}

    async def runner(task: object, http: object, downloads: Path, **kwargs: object) -> tuple[Outcome, ArmReport]:
        seen["n"] += 1
        if seen["n"] == 2:
            raise KeyboardInterrupt
        return Outcome("hello world", None, None), ArmReport(status="complete", seconds=0.1, dollars=0.02)

    monkeypatch.setattr(cc, "ARMS", {"fastbrowse": _spec(runner)})

    async def fake_load(source: object, http: object, **kwargs: object) -> list[ExternalTask]:
        return [_state_task(probe=False), _answer_task()]

    async def fake_preflight(corpus: Corpus, http: object) -> list[Preflight]:
        return _reachable(corpus)

    monkeypatch.setattr(cc, "load_tasks", fake_load)
    monkeypatch.setattr(cc, "preflight", fake_preflight)
    monkeypatch.setattr(cc.httpx, "AsyncClient", lambda **kwargs: _FakeClient())
    reset_args, _ = _reset_args(tmp_path)
    out = tmp_path / "run"
    code = await cc.main(
        [
            "windtunnel",
            "--site-urls",
            str(sites),
            "--arms",
            "fastbrowse",
            "--out",
            str(out),
            "--execute",
            "--seed",
            "7",
            *reset_args,
        ]
    )
    assert code == 1
    # The in-flight attempt is written before the interrupt propagates, so no spend is silently dropped.
    rows = [json.loads(line) for line in (out / "attempts.jsonl").read_text(encoding="utf-8").strip().splitlines()]
    assert len(rows) == 2
    interrupted = rows[1]
    assert interrupted["attempt"]["error"] == "interrupted"
    assert interrupted["attempt"]["completion"] == "error"
    assert interrupted["attempt"]["dollars"] is None and interrupted["attempt"]["unknown_cost"] is True
    report = json.loads((out / "report.json").read_text(encoding="utf-8"))
    assert len(report["attempts"]) == 2
    assert report["interrupted"] is True and report["truncated"] is False
    assert "interrupt" in report["note"] and "attempt cap" not in report["note"]


async def test_main_dedupes_repeated_arms_so_an_arm_runs_once(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    sites = _write_sites(tmp_path, {"easyappointments": "http://127.0.0.1:34871"})
    calls = {"n": 0}

    async def runner(task: object, http: object, downloads: Path, **kwargs: object) -> tuple[Outcome, ArmReport]:
        calls["n"] += 1
        return Outcome("hello world", None, None), ArmReport(status="complete", seconds=0.1, dollars=0.0)

    monkeypatch.setattr(cc, "ARMS", {"fastbrowse": _spec(runner)})

    async def fake_load(source: object, http: object, **kwargs: object) -> list[ExternalTask]:
        return [_state_task(probe=False)]

    async def fake_preflight(corpus: Corpus, http: object) -> list[Preflight]:
        return _reachable(corpus)

    monkeypatch.setattr(cc, "load_tasks", fake_load)
    monkeypatch.setattr(cc, "preflight", fake_preflight)
    monkeypatch.setattr(cc.httpx, "AsyncClient", lambda **kwargs: _FakeClient())
    reset_args, _ = _reset_args(tmp_path)
    out = tmp_path / "run"
    await cc.main(
        [
            "windtunnel",
            "--site-urls",
            str(sites),
            "--arms",
            "fastbrowse",
            "fastbrowse",
            "--out",
            str(out),
            "--execute",
            "--seed",
            "7",
            *reset_args,
        ]
    )
    assert calls["n"] == 1


@pytest.mark.parametrize(("repeats", "cap", "expected"), [(2, None, 1), (3, 5, 1), (3, None, 0)])
async def test_main_requires_an_unstopped_draw_with_three_paired_repeats(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, repeats: int, cap: int | None, expected: int
) -> None:
    sites = _write_sites(tmp_path, {"tailwind-nextjs-blog": "http://127.0.0.1:34871"})

    async def runner(task: object, http: object, downloads: Path, **kwargs: object) -> tuple[Outcome, ArmReport]:
        return Outcome("hello world", None, None), ArmReport(status="complete", seconds=0.1, dollars=0.0)

    async def fake_load(source: object, http: object, **kwargs: object) -> list[ExternalTask]:
        return [_answer_task("a-1"), _answer_task("a-2").model_copy(update={"task": "What does the heading say?"})]

    async def fake_preflight(corpus: Corpus, http: object) -> list[Preflight]:
        return _reachable(corpus)

    monkeypatch.setattr(cc, "ARMS", {"fastbrowse": _spec(runner)})
    monkeypatch.setattr(cc, "load_tasks", fake_load)
    monkeypatch.setattr(cc, "preflight", fake_preflight)
    monkeypatch.setattr(cc.httpx, "AsyncClient", lambda **kwargs: _FakeClient())
    reset_args, _ = _reset_args(tmp_path)
    out = tmp_path / "run"
    code = await cc.main(
        [
            "windtunnel",
            "--site-urls",
            str(sites),
            "--arms",
            "fastbrowse",
            "--repeat",
            str(repeats),
            "--per-stratum",
            "2",
            "--out",
            str(out),
            "--execute",
            "--seed",
            "7",
            *reset_args,
            *(["--max-attempts", str(cap)] if cap is not None else []),
        ]
    )
    assert code == expected
    report = json.loads((out / "report.json").read_text(encoding="utf-8"))
    assert report["truncated"] is (cap is not None)
    assert len(report["paired_tasks"]) == (0 if repeats < 3 else 3 if cap is not None else 6)
    assert len(report["attempts"]) == (cap if cap is not None else repeats * 2)
    assert report["headline"].startswith("paired on" if expected == 0 else "INSUFFICIENT PAIRED DATA")


def test_artifacts_are_exclusive(tmp_path: Path) -> None:
    downloads = tmp_path / "downloads"
    cc._write_artifact(downloads, {"arm": "fastbrowse"})
    with pytest.raises(FileExistsError):
        cc._write_artifact(downloads, {"arm": "fastbrowse"})


async def test_a_hung_arm_is_stopped_as_an_outage(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    log = tmp_path / "reset.log"
    reset = _reset(tmp_path, log)
    corpus = _corpus([_answer_task()])

    async def runner(task: object, http: object, downloads: Path, **kwargs: object) -> tuple[Outcome, ArmReport]:
        await asyncio.sleep(5)
        raise AssertionError("unreachable")

    monkeypatch.setattr(cc, "STUCK_SECONDS", 0.05)
    attempt = await _run_one(
        tmp_path,
        spec=_spec(runner),
        reset=reset,
        grader=predicate_grader(),
        task=corpus.tasks[0],
        corpus=corpus,
        log=log,
    )
    assert attempt.attempt.completion is Ending.UNAVAILABLE
    assert attempt.attempt.graded is False and attempt.correct is None
    assert "still running after" in (attempt.attempt.error or "")


async def test_a_cancelled_attempt_writes_its_row_before_the_cancellation_spreads(tmp_path: Path) -> None:
    log = tmp_path / "reset.log"
    reset = _reset(tmp_path, log)
    corpus = _corpus([_answer_task()])
    rows: list[cc.ArmAttempt] = []
    calls = {"n": 0}

    async def runner(task: object, http: object, downloads: Path, **kwargs: object) -> tuple[Outcome, ArmReport]:
        calls["n"] += 1
        if calls["n"] == 2:
            raise asyncio.CancelledError
        return Outcome("hello world", None, None), ArmReport(status="complete", seconds=0.1, dollars=0.02)

    specs = {"fastbrowse": _spec(runner), "browser-use": _spec(runner, tier="hosted")}
    async with _http() as http:
        with pytest.raises(asyncio.CancelledError):
            await cc.run_comparison(
                corpus,
                specs=specs,
                arms=("fastbrowse", "browser-use"),
                http=http,
                downloads=tmp_path / "downloads",
                run={},
                grader=predicate_grader(),
                reset=reset,
                run_id="run1",
                on_attempt=rows.append,
            )
    # The interrupted attempt may already have spent, so its row survives the cancel with an unknown cost.
    assert len(rows) == 2
    assert rows[1].attempt.error == "interrupted" and rows[1].attempt.unknown_cost is True
    assert rows[1].attempt.dollars is None and rows[1].attempt.completion is Ending.ERROR


async def test_ineligible_skips_survive_a_cancelled_run(tmp_path: Path) -> None:
    log = tmp_path / "reset.log"
    reset = _reset(tmp_path, log)
    corpus = _corpus([_answer_task()])
    rows: list[cc.ArmAttempt] = []
    skips: list[cc.SkipRecord] = []

    async def runner(task: object, http: object, downloads: Path, **kwargs: object) -> tuple[Outcome, ArmReport]:
        raise asyncio.CancelledError

    specs = {"jev-ultrafast": _spec(runner), "fastbrowse": _spec(runner)}
    async with _http() as http:
        with pytest.raises(asyncio.CancelledError):
            await cc.run_comparison(
                corpus,
                specs=specs,
                arms=("jev-ultrafast", "fastbrowse"),
                http=http,
                downloads=tmp_path / "downloads",
                run={},
                grader=predicate_grader(),
                reset=reset,
                run_id="run1",
                on_attempt=rows.append,
                on_skip=skips.append,
            )
    # The skip is decided before the interrupted arm, so a caller that is cancelled still holds it.
    assert [item.arm for item in skips] == ["jev-ultrafast"]
    assert len(rows) == 1 and rows[0].attempt.error == "interrupted"


async def test_main_keeps_reset_command_secrets_out_of_every_artifact(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    sites = _write_sites(tmp_path, {"easyappointments": "http://127.0.0.1:34871"})
    secret = "reset-token-abc123"
    script = tmp_path / "reset.sh"
    script.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    script.chmod(0o755)
    digest = hashlib.sha256(script.read_bytes()).hexdigest()
    command = (str(script), "--token", secret)

    async def runner(task: object, http: object, downloads: Path, **kwargs: object) -> tuple[Outcome, ArmReport]:
        raise KeyboardInterrupt

    async def fake_load(source: object, http: object, **kwargs: object) -> list[ExternalTask]:
        return [_state_task(probe=False)]

    async def fake_preflight(corpus: Corpus, http: object) -> list[Preflight]:
        return _reachable(corpus)

    monkeypatch.setattr(cc, "ARMS", {"fastbrowse": _spec(runner)})
    monkeypatch.setattr(cc, "load_tasks", fake_load)
    monkeypatch.setattr(cc, "preflight", fake_preflight)
    monkeypatch.setattr(cc.httpx, "AsyncClient", lambda **kwargs: _FakeClient())
    out = tmp_path / "run"
    code = await cc.main(
        [
            "windtunnel",
            "--site-urls",
            str(sites),
            "--arms",
            "fastbrowse",
            "--out",
            str(out),
            "--execute",
            "--seed",
            "7",
            "--reset-command",
            shlex.join(command),
            "--reset-code",
            str(script),
            "--reset-sha256",
            digest,
        ]
    )
    assert code == 1
    # A credential in the reset argv must not survive in the provenance, the reset pin or any ledger row.
    for name in ("corpus.json", "attempts.jsonl", "report.json"):
        assert secret not in (out / name).read_text(encoding="utf-8")


def test_pairing_drops_a_task_one_requested_arm_is_ineligible_for(tmp_path: Path) -> None:
    log = tmp_path / "reset.log"
    reset = _reset(tmp_path, log)
    corpus = _corpus([_answer_task(), _state_task(probe=False)])
    arms = ("fastbrowse", "jev-ultrafast")
    answer_digest = next(ref.digest for ref in corpus.tasks if ref.stratum == "answer")
    state_digest = next(ref.digest for ref in corpus.tasks if ref.stratum != "answer")
    # jev-ultrafast has no row at all on the answer task, so that task is not a fair pair for either arm.
    attempts = [
        _arm_attempt(reset, ref, arm=arm, repeat=repeat)
        for repeat in range(3)
        for ref in corpus.tasks
        for arm in arms
        if not (arm == "jev-ultrafast" and ref.digest == answer_digest)
    ]
    report = cc.build_report(
        corpus,
        arms=arms,
        attempts=attempts,
        skipped=[],
        truncated=False,
        repeats=3,
        sites={},
        remote=False,
        setup_seconds=None,
        preflight_seconds=0.0,
        prepare_seconds={},
        grader={},
        reset={},
        specs={arm: _spec(_arm_runner(arm, log)) for arm in arms},
    )
    # Both arms share one denominator over the state repeats, and the headline names only what every arm graded.
    assert {digest for digest, _ in report.paired_tasks} == {state_digest}
    assert report.paired["fastbrowse"].graded == report.paired["jev-ultrafast"].graded == 3
    assert report.headline.startswith("paired on 3 task-repeats across 1 tasks")


async def test_a_navigation_failure_at_a_reachable_site_is_an_arm_error_not_an_outage(tmp_path: Path) -> None:
    log = tmp_path / "reset.log"
    reset = _reset(tmp_path, log)
    corpus = _corpus([_answer_task()])
    failed = Unavailable("Page.navigate failed (net::ERR_CONNECTION_REFUSED)")
    attempt = await _run_one(
        tmp_path,
        spec=_spec(_arm_runner("fb", log, error=failed)),
        reset=reset,
        grader=predicate_grader(),
        task=corpus.tasks[0],
        corpus=corpus,
        log=log,
    )
    # The site answered the harness probe, so a page that never loaded is this arm's browser failing.
    assert attempt.attempt.completion is Ending.ERROR
    assert attempt.attempt.raw_status is None and attempt.attempt.graded is False
    assert "Page.navigate failed" in (attempt.attempt.error or "")


async def test_a_navigation_failure_with_the_site_down_stays_an_outage(tmp_path: Path) -> None:
    log = tmp_path / "reset.log"
    reset = _reset(tmp_path, log)
    corpus = _corpus([_answer_task()])
    failed = Unavailable("Page.navigate failed (net::ERR_CONNECTION_REFUSED)")

    async def runner(task: object, http: object, downloads: Path, **kwargs: object) -> tuple[Outcome, ArmReport]:
        raise failed

    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(503))) as http:
        attempt = await cc.run_arm_attempt(
            corpus.tasks[0],
            corpus=corpus,
            repeat=0,
            retry=0,
            spec=_spec(runner),
            arm="fastbrowse",
            http=http,
            downloads=tmp_path / "downloads",
            run={},
            grader=predicate_grader(),
            reset=reset,
            run_id="run1",
            authorize=False,
        )
    # The site itself is down, so every arm would fail the same way and this is an outage to wait out.
    assert attempt.attempt.completion is Ending.UNAVAILABLE
    assert attempt.attempt.graded is False and attempt.correct is None


def test_declared_fixture_auth_keeps_the_ref_and_replaces_only_the_credential_spans() -> None:
    definition = _auth_task()
    ref = _corpus([definition]).tasks[0]
    task = cc.live_task(ref, authorize=False)
    # The model sees secret names and the resolver gets the public fixture values.
    assert task.task == "Log in as username with password password, then report which client invoice 1 is billed to."
    assert "admin@admin.com" not in task.task and "admin123" not in task.task
    assert dict(task.secrets) == {"username": "admin@admin.com", "password": "admin123"}
    # The pinned task bytes and digest are the grader's, so neither changes when the model text does.
    assert ref.task == definition.task


async def test_fixture_auth_reaches_the_scoped_resolver_and_the_hosted_named_data() -> None:
    ref = _corpus([_auth_task()]).tasks[0]
    task = cc.live_task(ref, authorize=False)
    # The canonical fastbrowse arm builds this resolver from the same mapping, scoped to the start origin.
    resolver = live._secrets(task, bitwarden=False)
    assert resolver is not None
    assert {item.name for item in resolver.available()} == {"username", "password"}
    assert await resolver.resolve("password", "https://capsule.test") == "admin123"
    assert await resolver.resolve("password", "https://elsewhere.test") is None
    # The hosted runner passes the same mapping as its named sensitive data.
    assert dict(task.secrets) == {"username": "admin@admin.com", "password": "admin123"}


def test_an_ordinary_task_is_left_untouched_and_carries_no_secrets() -> None:
    for task in (_answer_task(), _auth_task(declared=False), _auth_task(declared=None)):
        ref = _corpus([task]).tasks[0]
        built = cc.live_task(ref, authorize=False)
        assert built.task == ref.task
        assert dict(built.secrets) == {}


def test_fixture_credentials_record_only_names_not_values() -> None:
    ref = _corpus([_auth_task()]).tasks[0]
    recorded = cc.fixture_credentials([ref])
    assert recorded == {"auth-1": ["password", "username"]}
    assert "admin123" not in json.dumps(recorded) and "admin@admin.com" not in json.dumps(recorded)


async def test_an_unexpected_declared_auth_prompt_is_refused_before_any_arm_runs(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    sites = _write_sites(tmp_path, {"idurar-erp-crm": "http://127.0.0.1:34871"})
    calls = {"n": 0}

    async def runner(task: object, http: object, downloads: Path, **kwargs: object) -> tuple[Outcome, ArmReport]:
        calls["n"] += 1
        raise AssertionError("no arm may run when setup refused the declared-auth prompt")

    async def fake_load(source: object, http: object, **kwargs: object) -> list[ExternalTask]:
        return [_auth_task(prompt="Log in to the admin backend and report the customer.")]

    async def fake_preflight(corpus: Corpus, http: object) -> list[Preflight]:
        return _reachable(corpus)

    monkeypatch.setattr(cc, "ARMS", {"fastbrowse": _spec(runner)})
    monkeypatch.setattr(cc, "load_tasks", fake_load)
    monkeypatch.setattr(cc, "preflight", fake_preflight)
    monkeypatch.setattr(cc.httpx, "AsyncClient", lambda **kwargs: _FakeClient())
    reset_args, _ = _reset_args(tmp_path)
    out = tmp_path / "run"
    with pytest.raises(SystemExit) as info:
        await cc.main(
            [
                "windtunnel",
                "--site-urls",
                str(sites),
                "--arms",
                "fastbrowse",
                "--out",
                str(out),
                "--execute",
                "--seed",
                "7",
                *reset_args,
            ]
        )
    assert info.value.code == 2
    assert calls["n"] == 0
    assert not out.exists()


async def test_the_run_protocol_names_fixture_credentials_without_their_values(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    sites = _write_sites(tmp_path, {"idurar-erp-crm": "http://127.0.0.1:34871"})

    async def runner(task: object, http: object, downloads: Path, **kwargs: object) -> tuple[Outcome, ArmReport]:
        raise KeyboardInterrupt

    async def fake_load(source: object, http: object, **kwargs: object) -> list[ExternalTask]:
        return [_auth_task()]

    async def fake_preflight(corpus: Corpus, http: object) -> list[Preflight]:
        return _reachable(corpus)

    monkeypatch.setattr(cc, "ARMS", {"fastbrowse": _spec(runner)})
    monkeypatch.setattr(cc, "load_tasks", fake_load)
    monkeypatch.setattr(cc, "preflight", fake_preflight)
    monkeypatch.setattr(cc.httpx, "AsyncClient", lambda **kwargs: _FakeClient())
    reset_args, _ = _reset_args(tmp_path)
    out = tmp_path / "run"
    code = await cc.main(
        [
            "windtunnel",
            "--site-urls",
            str(sites),
            "--arms",
            "fastbrowse",
            "--out",
            str(out),
            "--execute",
            "--seed",
            "7",
            *reset_args,
        ]
    )
    assert code == 1
    protocol = json.loads((out / "corpus.json").read_text(encoding="utf-8"))["run"]
    assert protocol["fixture_auth"] == {"auth-1": ["password", "username"]}
    # The protocol names the credentials; no run row carries the public fixture values.
    for name in ("attempts.jsonl", "report.json"):
        body = (out / name).read_text(encoding="utf-8")
        assert "admin123" not in body and "admin@admin.com" not in body
