"""The state watcher: a real observer subprocess beside a fake arm, through the real grader bridge.

Nothing here spends: the arms are fakes, the observer and graders are small Python scripts pinned by digest, and
the capsule is a JSON file the fake arm edits while the watcher reads it.
"""

import asyncio
import hashlib
import json
import os
import sys
import time
from pathlib import Path
from typing import Any

import httpx
import pytest

from fastbrowse.evals import corpus_compare as cc
from fastbrowse.evals.corpus import Corpus, Grade, Grader, TaskRef
from fastbrowse.evals.datasets import SOURCES, ExternalTask
from fastbrowse.evals.external_grade import predicate_grader
from fastbrowse.evals.live import ARMS, ArmReport, ArmSpec
from fastbrowse.evals.live_tasks import Outcome
from fastbrowse.evals.native_state import TrustedObservations, canonical_digest
from fastbrowse.evals.state_watch import MAX_SAMPLES, ObserverSpec, StateWatcher, build_observer

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "native_grader.py"
PREDICATE = {"type": "state", "probe": "database", "assert": {"equals": 1}}

# Records what the process was handed, then reads the capsule's state file named by its own argument.
OBSERVER = """
import json, os, sys
state, log, mode = sys.argv[1:4]
request = json.loads(sys.stdin.read())
with open(log, "a") as handle:
    handle.write(json.dumps({"keys": sorted(request), "phase": request["phase"], "env": sorted(os.environ)}) + "\\n")
if mode == "fail-during" and request["phase"] == "during":
    sys.exit(7)
print(json.dumps({"state": json.load(open(state))}))
"""

GRADER = """
import json, sys
print(json.dumps({"grader": "up", "version": "1", "passed": True}))
"""


def _ref(task_id: str = "t-1") -> TaskRef:
    task = ExternalTask(
        source="windtunnel",
        id=task_id,
        start="https://capsule.test/",
        task="Book then cancel.",
        stratum="transaction",
        site="easyappointments",
        metadata={"predicate": PREDICATE},
    )
    return Corpus.build(SOURCES["windtunnel"], [task], per_stratum=1, seed=1).tasks[0]


def _digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _observer(tmp_path: Path, mode: str = "ok", interval: float = 0.1) -> tuple[ObserverSpec, Path, Path]:
    script = tmp_path / "observer.py"
    script.write_text(OBSERVER, encoding="utf-8")
    state, log = tmp_path / "state.json", tmp_path / "observer.log"
    state.write_text(json.dumps({"n": 0}), encoding="utf-8")
    spec = ObserverSpec(
        command=(sys.executable, str(script), str(state), str(log), mode),
        code=script,
        digest=_digest(script),
        interval=interval,
        timeout=5,
    )
    return spec, state, log


async def _phases(watcher: StateWatcher, state: Path, *, change_after: float, total: float) -> None:
    async with watcher:
        await asyncio.sleep(change_after)
        _write_state(state, 1)
        await asyncio.sleep(total - change_after)


async def test_before_and_during_are_read_by_a_separate_process_that_sees_only_task_and_phase(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("OPENROUTER_API_KEY", "sk-must-not-leak")
    spec, state, log = _observer(tmp_path)
    watcher = StateWatcher(spec, _ref())
    await _phases(watcher, state, change_after=0.25, total=0.7)
    samples = watcher.samples
    assert samples[0].phase == "before" and samples[0].data == {"state": {"n": 0}}
    during = [sample for sample in samples if sample.phase == "during"]
    assert during and during[0].data == {"state": {"n": 0}}
    assert during[-1].data == {"state": {"n": 1}}
    assert [sample.sequence for sample in samples] == list(range(len(samples)))
    assert watcher.errors == ()
    seen = [json.loads(line) for line in log.read_text(encoding="utf-8").splitlines()]
    assert {tuple(item["keys"]) for item in seen} == {("phase", "task")}
    assert (
        not {name for item in seen for name in item["env"]}
        - {"HOME", "LANG", "LC_ALL", "LC_CTYPE", "PATH"}
        - {
            "TMPDIR",
            "TZ",
        }
    )
    assert "OPENROUTER_API_KEY" not in {name for item in seen for name in item["env"]}


async def test_a_failing_observer_leaves_errors_and_no_invented_sample(tmp_path: Path) -> None:
    spec, state, _ = _observer(tmp_path, mode="fail-during")
    watcher = StateWatcher(spec, _ref())
    await _phases(watcher, state, change_after=0.1, total=0.4)
    assert [sample.phase for sample in watcher.samples] == ["before"]
    assert any(error.startswith("during:") and "exited 7" in error for error in watcher.errors)
    trusted = watcher.trusted()
    assert trusted is not None and len(trusted.samples) == 1


async def test_an_observer_edited_after_it_was_pinned_stops_sampling_under_the_old_digest(tmp_path: Path) -> None:
    spec, _, _ = _observer(tmp_path)
    watcher = StateWatcher(spec, _ref())
    async with watcher:
        spec.code.write_text(OBSERVER + "\n# edited\n", encoding="utf-8")
        await asyncio.sleep(0.35)
    assert [sample.phase for sample in watcher.samples] == ["before"]
    assert any("changed since it was pinned" in error for error in watcher.errors)


async def test_nothing_is_sampled_when_the_pinned_code_is_already_different(tmp_path: Path) -> None:
    spec, _, _ = _observer(tmp_path)
    spec.code.write_text(OBSERVER + "\n# edited\n", encoding="utf-8")
    watcher = StateWatcher(spec, _ref())
    async with watcher:
        pass
    assert watcher.samples == () and watcher.trusted() is None and watcher.errors


def _write_state(path: Path, n: int) -> None:
    path.write_text(json.dumps({"n": n}), encoding="utf-8")


def _load(path: Path | str | None) -> Any:
    assert path is not None
    return json.loads(Path(path).read_text(encoding="utf-8"))


def _alive(pid: int) -> bool:
    try:
        return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0] != "Z"
    except (FileNotFoundError, ProcessLookupError):
        return False


async def test_cancellation_kills_the_observer_process_group_and_leaves_no_sampler_task(tmp_path: Path) -> None:
    pids = tmp_path / "pids"
    script = tmp_path / "slow.py"
    script.write_text(
        "import json, os, subprocess, sys, time\n"
        "request = json.loads(sys.stdin.read())\n"
        "if request['phase'] == 'during':\n"
        "    child = subprocess.Popen(['sleep', '60'])\n"
        f"    open({str(pids)!r}, 'w').write(f'{{os.getpid()}} {{child.pid}}')\n"
        "    time.sleep(60)\n"
        "print('{}')\n",
        encoding="utf-8",
    )
    spec = ObserverSpec(command=(sys.executable, str(script)), code=script, digest=_digest(script), interval=0.1)
    started = asyncio.Event()

    async def attempt() -> None:
        async with StateWatcher(spec, _ref()):
            started.set()
            await asyncio.sleep(30)

    task = asyncio.create_task(attempt())
    await started.wait()
    for _ in range(100):
        if pids.exists() and pids.read_text().count(" "):
            break
        await asyncio.sleep(0.05)
    parent, child = (int(item) for item in pids.read_text().split())
    assert _alive(parent) and _alive(child)
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    await asyncio.sleep(0.2)
    assert not _alive(parent) and not _alive(child)
    assert [t for t in asyncio.all_tasks() if t.get_name() == "state-watch"] == []


async def test_the_sample_count_is_bounded(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("fastbrowse.evals.state_watch.MAX_SAMPLES", 3)
    spec, _, _ = _observer(tmp_path)
    watcher = StateWatcher(spec, _ref())
    async with watcher:
        await asyncio.sleep(1.0)
    assert len(watcher.samples) == 3 and watcher.record()["truncated"] is True
    assert MAX_SAMPLES >= 3


@pytest.mark.parametrize(
    ("kwargs", "message"),
    [
        ({"command": "not json"}, "JSON array"),
        ({"command": '"just a string"'}, "JSON array"),
        ({"command": "[1, 2]"}, "JSON array"),
        ({"command": "[]"}, "non-empty"),
        ({"sha256": "0" * 64}, "does not match"),
        ({"interval": 0.01}, "interval"),
        ({"interval": float("nan")}, "interval"),
        ({"timeout": 0}, "timeout"),
        ({"timeout": 9999}, "timeout"),
        ({"command": '["no-such-observer-program", "{code}"]'}, "not found"),
        ({"command": '["{python}"]'}, "must name a file"),
    ],
)
def test_observer_options_are_refused_before_any_spend(tmp_path: Path, kwargs: dict[str, object], message: str) -> None:
    spec, _, _ = _observer(tmp_path)
    options: dict[str, object] = {
        "command": json.dumps(list(spec.command)),
        "code": spec.code,
        "sha256": spec.digest,
        "interval": 0.5,
        "timeout": 5.0,
    }
    options.update(kwargs)
    if isinstance(options["command"], str):
        options["command"] = options["command"].replace("{code}", str(spec.code)).replace("{python}", sys.executable)
    with pytest.raises(ValueError, match=message):
        build_observer(**options)  # type: ignore[arg-type]


def test_observer_options_without_a_command_are_refused_and_none_means_no_watcher(tmp_path: Path) -> None:
    assert build_observer(None, code=None, sha256=None, interval=None, timeout=None) is None
    with pytest.raises(ValueError, match="need --observer-command"):
        build_observer(None, code=tmp_path, sha256=None, interval=None, timeout=None)


def test_the_run_pin_keeps_the_program_and_digest_but_no_argument(tmp_path: Path) -> None:
    spec, _, _ = _observer(tmp_path)
    pin = spec.pin()
    assert pin["command"] == [sys.executable] and pin["sha256"] == spec.digest
    assert "state.json" not in json.dumps(pin)
    assert cc._recorded_argv(["--observer-command", '["x","--auth-file","/secret"]', "--seed", "7"]) == [
        "--observer-command",
        "[redacted]",
        "--seed",
        "7",
    ]
    assert cc._recorded_argv(['--observer-command=["x"]'])[0] == "--observer-command=[redacted]"


async def test_main_refuses_a_bad_or_orphaned_observer_before_anything_is_written(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    sites = tmp_path / "sites.json"
    sites.write_text(json.dumps({"tailwind-nextjs-blog": "http://127.0.0.1:34871"}), encoding="utf-8")
    spec, _, _ = _observer(tmp_path)
    arm = next(name for name, item in ARMS.items() if item.tier != "hosted")
    out = tmp_path / "run"
    base = ["windtunnel", "--site-urls", str(sites), "--arms", arm, "--out", str(out), "--seed", "7"]
    good = [
        "--observer-command",
        json.dumps(list(spec.command)),
        "--observer-code",
        str(spec.code),
        "--observer-sha256",
        spec.digest,
    ]
    with pytest.raises(SystemExit) as info:
        await cc.main([*base, *good])
    assert info.value.code == 2 and "--grader-command" in capsys.readouterr().err
    with pytest.raises(SystemExit):
        await cc.main([*base, "--observer-command", "[]"])
    assert "observer" in capsys.readouterr().err and not out.exists()


def _chain(tmp_path: Path, *, forged: object) -> tuple[ObserverSpec, cc.ResetSpec, Grader, TaskRef, Path]:
    """The real chain: native_grader.py is both the independent observer and, wrapping a fake upstream, the grader."""
    ref = _ref()
    state = tmp_path / "state.json"
    state.write_text(json.dumps({"n": 0}), encoding="utf-8")
    probe = tmp_path / "probe.py"
    probe.write_text(f"import json\nprint(json.dumps(json.load(open({str(state)!r}))))\n", encoding="utf-8")
    upstream = tmp_path / "up.py"
    upstream.write_text(GRADER, encoding="utf-8")
    observe = [{"kind": "probe", "name": "state", "probe": "p"}]
    supplement = {
        "id": "s",
        "task_id": ref.id,
        "version": "1",
        "source": ref.source,
        "revision": ref.revision,
        "dataset_sha256": ref.sha256,
        "site": ref.site,
        "task_sha256": hashlib.sha256(ref.task.encode()).hexdigest(),
        "upstream_sha256": canonical_digest(PREDICATE),
        "purpose": "p",
        "witnesses": [
            {"label": "before", "observe": observe, "checks": [{"kind": "equals", "path": "state.n", "value": 0}]},
            {"label": "during", "observe": observe, "checks": [{"kind": "equals", "path": "state.n", "value": 1}]},
        ],
    }
    (tmp_path / "supplements.json").write_text(json.dumps({"supplements": [supplement]}), encoding="utf-8")
    digest = _digest(SCRIPT)
    common = [
        sys.executable,
        str(SCRIPT),
        "--supplements",
        str(tmp_path / "supplements.json"),
        "--observe-arg",
        sys.executable,
        "--observe-arg",
        str(probe),
    ]
    observer = ObserverSpec(
        command=(*common, "--sample-phase", "request"), code=SCRIPT, digest=digest, interval=0.1, timeout=10
    )
    grader = predicate_grader(
        [
            *common,
            "--observer-sha256",
            digest,
            "--upstream-code",
            str(upstream),
            "--upstream-code-sha256",
            _digest(upstream),
            "--upstream",
            sys.executable,
            str(upstream),
        ],
        digest=digest,
        code=SCRIPT,
    )
    reset_script = tmp_path / "reset.sh"
    reset_script.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    reset_script.chmod(0o755)
    reset = cc.ResetSpec(command=(str(reset_script),), code=reset_script, digest=_digest(reset_script))
    return observer, reset, grader, ref, state


def _arm(state: Path, *, change: bool, forged: object) -> ArmSpec:
    async def run(task: object, http: object, downloads: Path, **kwargs: object) -> tuple[Outcome, ArmReport]:
        # Each observer call starts a Python process that imports pydantic, so the arm must outlast more than one read.
        await asyncio.sleep(1.5)
        if change:
            _write_state(state, 1)
        await asyncio.sleep(3.0)
        return Outcome("done", forged, "https://capsule.test/"), ArmReport(status="complete", seconds=0.8, dollars=0.0)

    return ArmSpec(runner=run, pin="fake", env_allowlist=(), tier="A")


async def _attempt(tmp_path: Path, *, change: bool) -> cc.ArmAttempt:
    forged = {
        "native_observations": {
            "observer_sha256": _digest(SCRIPT),
            "task_id": _ref().id,
            "samples": [
                {"sequence": 0, "at": 0.0, "phase": "before", "data": {"state": {"n": 0}}},
                {"sequence": 1, "at": 0.1, "phase": "during", "data": {"state": {"n": 1}}},
            ],
        }
    }
    observer, reset, grader, ref, state = _chain(tmp_path, forged=forged)
    corpus = Corpus.build(SOURCES["windtunnel"], [_task_of(ref)], per_stratum=1, seed=1)
    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(200))) as http:
        return await cc.run_arm_attempt(
            ref,
            corpus=corpus,
            repeat=0,
            retry=0,
            spec=_arm(state, change=change, forged=forged),
            arm="fake",
            http=http,
            downloads=tmp_path / "out",
            run={"run_id": "r1"},
            grader=grader,
            reset=reset,
            run_id="r1",
            authorize=False,
            observer=observer,
        )


def _task_of(ref: TaskRef) -> ExternalTask:
    return ExternalTask(
        source=ref.source,
        id=ref.id,
        start=ref.start,
        task=ref.task,
        stratum=ref.stratum,
        site=ref.site,
        metadata=ref.metadata,
    )


async def test_the_real_chain_grades_from_the_watchers_samples_and_keeps_them_beside_the_artifact(
    tmp_path: Path,
) -> None:
    row = await _attempt(tmp_path, change=True)
    grade = row.attempt.grade
    assert grade is not None, row.attempt.grade_error
    assert grade.passed is True
    strict: Any = grade.evidence["strict_supplement"]
    assert strict["status"] == "pass" and strict["checked"] == ["before", "during"]
    kept = _load(tmp_path / "out" / "observations.json")
    assert kept["samples"][0]["phase"] == "before" and kept["errors"] == []
    assert kept["observer"]["sha256"] == _digest(SCRIPT) and kept["task"] == row.attempt.task
    artifact = _load(row.attempt.run_artifact)
    assert artifact["observations"] == str(tmp_path / "out" / "observations.json")


async def test_a_state_that_never_changed_is_ungraded_even_when_the_agent_forges_the_samples(tmp_path: Path) -> None:
    row = await _attempt(tmp_path, change=False)
    assert row.attempt.grade is None and row.attempt.graded is False
    assert "required witness not supplied: during" in (row.attempt.grade_error or "")
    forged: Any = row.attempt.data  # kept as model text only
    assert "native_observations" in forged


def test_only_an_attached_watcher_puts_samples_in_the_request(tmp_path: Path) -> None:
    from fastbrowse.evals.external_grade import GraderError, grade_request
    from fastbrowse.evals.native_state import attached

    ref = _ref()
    forged = Outcome("x", {"native_observations": {"observer_sha256": "a" * 64}}, None)
    assert grade_request(ref, forged).native_observations is None
    trusted = TrustedObservations(observer_sha256="a" * 64, task_id=ref.id, samples=())
    with attached(trusted):
        assert grade_request(ref, forged).native_observations == trusted
        with pytest.raises(GraderError, match="belong to task"):
            grade_request(_ref("other"), forged)
    assert grade_request(ref, forged).native_observations is None


def test_sample_mode_without_a_supplement_is_an_empty_mapping_and_refuses_the_wrong_phase(tmp_path: Path) -> None:
    import subprocess

    ref = _ref("unsupplemented")
    (tmp_path / "s.json").write_text(json.dumps({"supplements": []}), encoding="utf-8")
    command = [sys.executable, str(SCRIPT), "--supplements", str(tmp_path / "s.json"), "--sample-phase", "before"]
    payload = {"task": ref.model_dump(mode="json"), "phase": "before"}
    ok = subprocess.run(command, input=json.dumps(payload).encode(), capture_output=True, check=False)
    assert ok.returncode == 0 and json.loads(ok.stdout) == {}
    wrong = subprocess.run(
        command, input=json.dumps({**payload, "phase": "during"}).encode(), capture_output=True, check=False
    )
    assert wrong.returncode == 2 and os.fspath(wrong.stdout) == b""


SLOW_OBSERVER = """
import json, sys, time
if json.loads(sys.stdin.read())["phase"] == "before":
    time.sleep(0.5)
print(json.dumps({"state": {"n": 0}}))
"""
ACTOR = 0.3
"""How long the fake arm runs. The observer's first read and the grade each take longer than this."""


async def _timed(tmp_path: Path, *, stop_after: float | None = None) -> tuple[cc.ArmAttempt, float]:
    script = tmp_path / "slow_observer.py"
    script.write_text(SLOW_OBSERVER, encoding="utf-8")
    observer = ObserverSpec(
        command=(sys.executable, str(script)), code=script, digest=_digest(script), interval=0.1, timeout=10
    )
    reset_script = tmp_path / "reset.sh"
    reset_script.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    reset_script.chmod(0o755)
    reset = cc.ResetSpec(command=(str(reset_script),), code=reset_script, digest=_digest(reset_script))
    ref = _ref()
    corpus = Corpus.build(SOURCES["windtunnel"], [_task_of(ref)], per_stratum=1, seed=1)
    entered: list[float] = []

    async def run(task: object, http: object, downloads: Path, *, started: float, **kwargs: object):
        entered.append(started)
        await asyncio.sleep(ACTOR if stop_after is None else 60)
        return Outcome("done", None, "https://capsule.test/"), ArmReport(
            status="complete", seconds=time.monotonic() - started, dollars=0.0
        )

    async def grader(_: TaskRef, __: Outcome) -> Grade:
        await asyncio.sleep(0.5)
        return Grade(grader="g", version="1", passed=True)

    rows: list[cc.ArmAttempt] = []
    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(200))) as http:
        call = cc.run_arm_attempt(
            ref,
            corpus=corpus,
            repeat=0,
            retry=0,
            spec=ArmSpec(runner=run, pin="fake", env_allowlist=(), tier="A"),
            arm="fake",
            http=http,
            downloads=tmp_path / "out",
            run={"run_id": "r1"},
            grader=grader,
            reset=reset,
            run_id="r1",
            authorize=False,
            observer=observer,
            on_attempt=rows.append,
        )
        if stop_after is None:
            row = await call
        else:
            with pytest.raises(asyncio.TimeoutError):
                await asyncio.wait_for(call, stop_after)
            row = rows[0]
    return row, entered[0]


async def test_arm_seconds_exclude_the_observers_first_read_and_the_grade(tmp_path: Path) -> None:
    row, _ = await _timed(tmp_path)
    assert row.attempt.seconds == pytest.approx(ACTOR, abs=0.15)  # not the 1.3s the whole attempt took
    artifact = _load(row.attempt.run_artifact)
    assert artifact["report"]["seconds"] == pytest.approx(row.attempt.seconds, abs=0.15)
    overhead = artifact["overhead_seconds"]
    assert overhead["observer_before"] >= 0.5 and overhead["grade"] >= 0.5 and overhead["reset"] >= 0


async def test_an_interrupted_attempt_reports_the_time_the_arm_actually_ran(tmp_path: Path) -> None:
    # The cancel lands after the observer's 0.5s first read and before the grade: the arm ran for the rest.
    row, _ = await _timed(tmp_path, stop_after=0.9)
    assert row.attempt.error == "interrupted"
    assert 0 < row.attempt.seconds < 0.75
