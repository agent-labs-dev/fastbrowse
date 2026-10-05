"""The external-corpus execution seam: selection, truthfulness of completion against grade, and the runner edge."""

import argparse
import asyncio
import hashlib
import json
from pathlib import Path
from typing import Literal

import httpx
import pytest

from fastbrowse.evals import corpus as corpus_module
from fastbrowse.evals.datasets import ExternalTask, Source
from fastbrowse.evals.live_tasks import Outcome
from fastbrowse.evals.status import Ending
from fastbrowse.models import Status


def _source() -> Source:
    return Source(
        id="online-mind2web",
        upstream="https://fixture.test",
        revision="a" * 40,
        url="https://fixture.test/data",
        sha256="b" * 64,
    )


def _tasks() -> list[ExternalTask]:
    strata: tuple[Literal["easy", "medium", "hard"], ...] = ("easy", "medium", "hard", "hard")
    return [
        ExternalTask(
            source="online-mind2web",
            id=f"task-{name}-{index}",
            start=f"https://fixture.test/{name}",
            task="Read.",
            stratum=name,
        )
        for index, name in enumerate(strata)
    ]


def _corpus(*, seed: int = 7) -> corpus_module.Corpus:
    return corpus_module.Corpus.build(_source(), _tasks(), per_stratum=1, seed=seed)


def test_selection_is_reproducible_and_names_the_bytes_it_drew() -> None:
    first = _corpus()
    assert first.digest == _corpus().digest
    assert len(first.tasks) == 3
    assert all(task.source == "online-mind2web" for task in first.tasks)
    assert all(len(task.digest) == 64 and task.revision == "a" * 40 and task.sha256 == "b" * 64 for task in first.tasks)
    # The loader's input order must not decide the draw, or two callers of the same seed disagree.
    reversed_corpus = corpus_module.Corpus.build(_source(), _tasks()[::-1], per_stratum=1, seed=7)
    assert [task.id for task in reversed_corpus.tasks] == [task.id for task in first.tasks]
    assert reversed_corpus.digest == first.digest


def test_a_changed_task_changes_its_digest_and_the_corpus_digest() -> None:
    original = _corpus()
    changed_tasks = _tasks()
    changed_tasks[0] = changed_tasks[0].model_copy(update={"task": "Count the rows."})
    changed = corpus_module.Corpus.build(_source(), changed_tasks, per_stratum=1, seed=7)
    assert changed.digest != original.digest
    before = {task.id: task.digest for task in original.tasks}
    moved = {task.id: task.digest for task in changed.tasks}
    # easy has one task, so it is drawn at every seed; its text is the only one that changed.
    assert [task_id for task_id in before if before[task_id] != moved[task_id]] == ["task-easy-0"]
    with pytest.raises(ValueError, match="verified sha256"):
        corpus_module.Corpus.build(_source().model_copy(update={"sha256": None}), _tasks(), per_stratum=1, seed=7)


def _ref() -> corpus_module.TaskRef:
    return _corpus().tasks[0]


def _outcome(
    status: Status = Status.COMPLETE, *, dollars: float | None = None, unknown_cost: bool = False
) -> corpus_module.RunOutcome:
    return corpus_module.RunOutcome(
        status=status,
        answer="42",
        quotes=(("https://fixture.test/", "42"),),
        dollars=dollars,
        unknown_cost=unknown_cost,
    )


async def _run(runner: corpus_module.Runner, *, grader: corpus_module.Grader | None = None) -> corpus_module.Attempt:
    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(200))) as http:
        return await corpus_module.run_attempt(
            _ref(),
            corpus="c" * 64,
            repeat=0,
            runner=runner,
            http=http,
            downloads=Path("/tmp/unused"),
            run={"run_id": "fixture"},
            grader=grader,
        )


async def test_no_grader_leaves_the_pass_state_unknown_not_false() -> None:
    async def runner(ref: corpus_module.TaskRef, http: httpx.AsyncClient, downloads: Path) -> corpus_module.RunOutcome:
        return _outcome()

    attempt = await _run(runner)
    assert attempt.completed is True and attempt.completion == Ending.DONE
    assert attempt.graded is False and attempt.passed is None and attempt.grade is None
    assert attempt.raw_status == Status.COMPLETE.value and attempt.dollars is None


async def test_passed_needs_both_the_grade_and_the_agents_completion() -> None:
    async def runner(ref: corpus_module.TaskRef, http: httpx.AsyncClient, downloads: Path) -> corpus_module.RunOutcome:
        return _outcome(status=Status.NEEDS_LOGIN)

    async def passing(ref: corpus_module.TaskRef, outcome: Outcome) -> corpus_module.Grade:
        assert outcome.quotes == (("https://fixture.test/", "42"),)
        return corpus_module.Grade(grader="webjudge", version="1", passed=True)

    stopped = await _run(runner, grader=passing)
    assert stopped.graded is True and stopped.grade is not None and stopped.grade.passed is True
    # The grader passed the answer, but this agent never completed the task, so the attempt is not a pass.
    assert stopped.completed is False and stopped.passed is False

    async def completing(
        ref: corpus_module.TaskRef, http: httpx.AsyncClient, downloads: Path
    ) -> corpus_module.RunOutcome:
        return _outcome()

    passed = await _run(completing, grader=passing)
    assert passed.completed is True and passed.graded is True and passed.passed is True


async def test_a_raising_grader_stays_ungraded_and_a_raising_runner_is_recorded() -> None:
    async def runner(ref: corpus_module.TaskRef, http: httpx.AsyncClient, downloads: Path) -> corpus_module.RunOutcome:
        return _outcome()

    async def broken(ref: corpus_module.TaskRef, outcome: Outcome) -> corpus_module.Grade:
        raise RuntimeError("judge key missing")

    ungraded = await _run(runner, grader=broken)
    assert ungraded.graded is False and ungraded.passed is None
    assert ungraded.grade_error == "grader raised RuntimeError: judge key missing"

    async def crashing(
        ref: corpus_module.TaskRef, http: httpx.AsyncClient, downloads: Path
    ) -> corpus_module.RunOutcome:
        raise TimeoutError("browser gone")

    crashed = await _run(crashing)
    assert crashed.completion == Ending.ERROR and crashed.completed is False
    assert crashed.error == "TimeoutError: browser gone" and crashed.graded is False


async def test_execute_writes_every_selected_attempt_and_summarizes_truthfully() -> None:
    seen: list[corpus_module.Attempt] = []

    async def runner(ref: corpus_module.TaskRef, http: httpx.AsyncClient, downloads: Path) -> corpus_module.RunOutcome:
        return _outcome(dollars=0.01)

    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(200))) as http:
        attempts = await corpus_module.execute(
            _corpus(),
            runner=runner,
            http=http,
            downloads=Path("/tmp/unused"),
            repeats=2,
            run={"run_id": "fixture"},
            on_attempt=seen.append,
        )
        assert len(attempts) == 6 and len(seen) == 6
        assert {attempt.corpus for attempt in attempts} == {_corpus().digest}
        assert all(attempt.dollars == 0.01 for attempt in attempts)
        summary = corpus_module.summarize(_corpus(), attempts, repeats=2)
        assert (summary.attempts, summary.completed) == (6, 6)
        assert (summary.ungraded, summary.graded, summary.passed) == (6, 0, 0)
        assert "no independent grader" in summary.note.lower()
        with pytest.raises(ValueError, match="repeats must be positive"):
            await corpus_module.execute(
                _corpus(), runner=runner, http=http, downloads=Path("/tmp/unused"), repeats=0, run={}
            )


async def test_preflight_reports_reachability_only_without_an_agent() -> None:
    def recorded(request: httpx.Request) -> httpx.Response:
        return httpx.Response(503 if request.url.path.endswith("medium") else 200)

    async with httpx.AsyncClient(transport=httpx.MockTransport(recorded)) as http:
        rows = await corpus_module.preflight(_corpus(), http)
    assert len(rows) == 3
    unreachable = [row for row in rows if not row.reachable]
    assert len(unreachable) == 1 and unreachable[0].status_code == 503
    # The preflight identifies the same bytes the selection did, and adds no feasibility claim of its own.
    assert {row.digest for row in rows} == {task.digest for task in _corpus().tasks}
    assert all("feasib" not in row.reason for row in rows)


def _attempt(passed: bool | None) -> corpus_module.Attempt:
    return corpus_module.Attempt(
        corpus="c" * 64,
        source="online-mind2web",
        revision="a" * 40,
        sha256="b" * 64,
        task="task",
        task_digest="d" * 64,
        stratum="easy",
        repeat=0,
        arm="fastbrowse",
        raw_status="complete",
        completion=Ending.DONE,
        completed=True,
        graded=passed is not None,
        passed=passed,
        seconds=0.1,
    )


def test_exit_code_is_zero_only_when_every_expected_attempt_passed() -> None:
    assert corpus_module._exit_code([_attempt(True), _attempt(True)], expected=2) == 0
    assert corpus_module._exit_code([_attempt(True), _attempt(False)], expected=2) == 1
    assert corpus_module._exit_code([_attempt(True), _attempt(None)], expected=2) == 1
    assert corpus_module._exit_code([_attempt(True)], expected=2) == 1
    assert corpus_module._exit_code([], expected=0) == 1


async def test_execute_gives_every_attempt_its_own_downloads_and_run_artifact(tmp_path: Path) -> None:
    seen: list[Path] = []

    async def runner(ref: corpus_module.TaskRef, http: httpx.AsyncClient, downloads: Path) -> corpus_module.RunOutcome:
        seen.append(downloads)
        await asyncio.to_thread(downloads.mkdir, parents=True, exist_ok=True)
        await asyncio.to_thread((downloads / "run-result.json").write_text, "{}")
        return _outcome()

    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(200))) as http:
        attempts = await corpus_module.execute(
            _corpus(), runner=runner, http=http, downloads=tmp_path, repeats=2, run={}
        )
    assert len(set(seen)) == len(seen) == 6
    assert all(attempt.run_artifact is not None for attempt in attempts)


def _args(**overrides: object) -> argparse.Namespace:
    base: dict[str, object] = {
        "grader_command": None,
        "grader_code": None,
        "grader_sha256": None,
        "grader_timeout": None,
    }
    base.update(overrides)
    return argparse.Namespace(**base)


def test_build_grader_defaults_to_native_predicates_and_pins_a_command(tmp_path: Path) -> None:
    assert callable(corpus_module._build_grader(_args()))
    with pytest.raises(ValueError, match="requires --grader-code"):
        corpus_module._build_grader(_args(grader_command=["python", "grader.py"]))
    script = tmp_path / "grader.py"
    script.write_text("print('{}')\n", encoding="utf-8")
    # A digest that does not match the code is refused before a task is fetched.
    with pytest.raises(ValueError, match="does not match"):
        corpus_module._build_grader(
            _args(grader_command=["python", str(script)], grader_code=script, grader_sha256="0" * 64)
        )


class _FakeClient:
    async def __aenter__(self) -> "_FakeClient":
        return self

    async def __aexit__(self, *exc: object) -> bool:
        return False


async def test_cli_rejects_grader_options_without_a_command(tmp_path: Path) -> None:
    with pytest.raises(SystemExit) as exit_info:
        await corpus_module.main(
            ["online-mind2web", "--sha256", "b" * 64, "--out", str(tmp_path / "out"), "--grader-timeout", "5"]
        )
    assert exit_info.value.code == 2


async def test_cli_refuses_to_overwrite_existing_output(tmp_path: Path) -> None:
    out = tmp_path / "run"
    out.mkdir()
    (out / "corpus.json").write_text("already here\n", encoding="utf-8")
    with pytest.raises(SystemExit) as exit_info:
        await corpus_module.main(["online-mind2web", "--sha256", "b" * 64, "--out", str(out)])
    assert exit_info.value.code == 2


async def test_cli_parses_a_grader_command_vector_with_flags(tmp_path: Path) -> None:
    script = tmp_path / "grader.py"
    script.write_text("print('{}')\n", encoding="utf-8")
    digest = hashlib.sha256(script.read_bytes()).hexdigest()
    out = tmp_path / "run"
    out.mkdir()
    (out / "corpus.json").write_text("already here\n", encoding="utf-8")
    with pytest.raises(SystemExit) as exit_info:
        await corpus_module.main(
            [
                "online-mind2web",
                "--sha256",
                "b" * 64,
                "--out",
                str(out),
                "--grader-code",
                str(script),
                "--grader-sha256",
                digest,
                "--grader-command",
                "python",
                "-m",
                "some_grader",
            ]
        )
    # Reaching the overwrite guard proves the vector, flags included, parsed and the digest matched.
    assert exit_info.value.code == 2


async def test_cli_keeps_finished_attempts_in_the_ledger_on_interrupt(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    async def fake_load(source: object, http: object, **kwargs: object) -> list[ExternalTask]:
        return _tasks()

    async def fake_preflight(corpus: corpus_module.Corpus, http: object) -> list[corpus_module.Preflight]:
        return [
            corpus_module.Preflight(
                task=task.id, digest=task.digest, start=task.start, reachable=True, reason="http_ok"
            )
            for task in corpus.tasks
        ]

    calls = {"n": 0}

    async def runner(ref: corpus_module.TaskRef, http: object, downloads: Path) -> corpus_module.RunOutcome:
        calls["n"] += 1
        if calls["n"] == 2:
            raise KeyboardInterrupt
        return _outcome()

    async def passing(ref: corpus_module.TaskRef, outcome: Outcome) -> corpus_module.Grade:
        return corpus_module.Grade(grader="fixture", version="1", passed=True)

    monkeypatch.setattr(corpus_module, "load_tasks", fake_load)
    monkeypatch.setattr(corpus_module, "preflight", fake_preflight)
    monkeypatch.setattr(corpus_module, "runner_for", lambda **kwargs: runner)
    monkeypatch.setattr(corpus_module, "_build_grader", lambda args: passing)
    monkeypatch.setattr(corpus_module.httpx, "AsyncClient", lambda **kwargs: _FakeClient())
    out = tmp_path / "run"
    code = await corpus_module.main(
        ["online-mind2web", "--sha256", "b" * 64, "--out", str(out), "--execute", "--seed", "7"]
    )
    assert code == 1
    assert len((out / "attempts.jsonl").read_text(encoding="utf-8").strip().splitlines()) == 1
    assert json.loads((out / "summary.json").read_text(encoding="utf-8"))["attempts"] == 1
