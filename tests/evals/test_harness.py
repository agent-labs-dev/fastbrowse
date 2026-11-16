import os
import signal
import subprocess
import sys
import time
from contextlib import suppress
from io import BytesIO
from pathlib import Path

import pytest

from fastbrowse.evals import harness
from fastbrowse.evals.harness import TaskSecrets, _diagnostic, environment, origin_of, providers, transient_seconds


def test_the_environment_reads_the_dotenv_beside_it_under_real_settings(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    dotenv = tmp_path / ".env"
    dotenv.write_text(
        "# a comment\n"
        "TYPESAFE_API_KEY=direct  # the recommended route\n"
        "export OPENROUTER_API_KEY='router'\n"
        "AI_GATEWAY_API_KEY=\n"
        "FASTBROWSE_DECISION_SOURCE=openrouter\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FASTBROWSE_DECISION_SOURCE", "typesafe")
    monkeypatch.delenv("TYPESAFE_API_KEY", raising=False)
    monkeypatch.delenv("OPENROUTER_API_KEY", raising=False)
    monkeypatch.delenv("AI_GATEWAY_API_KEY", raising=False)

    read = environment(dotenv)

    assert (read["TYPESAFE_API_KEY"], read["OPENROUTER_API_KEY"]) == ("direct", "router")
    assert "AI_GATEWAY_API_KEY" not in read
    assert read["FASTBROWSE_DECISION_SOURCE"] == "typesafe"


def test_transient_time_counts_overlapping_calls_once_and_only_within_the_run() -> None:
    def span(began: float, ended: float) -> dict[str, object]:
        return {"event": "request_transient", "call": "jev", "began": began, "ended": ended}

    # Parallel batches failing over 2..5 and 4..6 lost 4s, not 5; a span before the run and other events add none.
    events = [span(4.0, 6.0), span(2.0, 5.0), span(-3.0, -1.0), {"event": "request_retry"}, "a message"]
    assert transient_seconds(events, 0.0, 10.0) == pytest.approx(4.0)
    assert transient_seconds(events, 0.0, 3.0) == pytest.approx(1.0)


def test_the_route_is_read_from_the_runs_route_event() -> None:
    assert providers([{"event": "request_retry"}, {"event": "route", "providers": "Jev x; LLM y"}]) == "Jev x; LLM y"
    assert providers([{"event": "request_retry"}]) is None


@pytest.mark.parametrize(
    ("url", "origin"),
    [
        ("http://127.0.0.1:8123/sign-in?next=/", "http://127.0.0.1:8123"),
        ("https://Shop.Example.com:443/", "https://shop.example.com"),
        ("http://user@host.test:80/", "http://host.test"),
    ],
)
def test_an_origin_drops_the_path_credentials_and_an_implied_port(url: str, origin: str) -> None:
    assert origin_of(url) == origin


async def test_task_secrets_are_given_only_on_the_fixture_origin() -> None:
    secrets = TaskSecrets({"password": "hunter2"}, "http://127.0.0.1:8123/sign-in")
    assert [(ref.name, ref.origins) for ref in secrets.available()] == [("password", ("http://127.0.0.1:8123",))]
    assert await secrets.resolve("password", "http://127.0.0.1:8123") == "hunter2"
    assert await secrets.resolve("password", "http://127.0.0.1:8124") is None
    assert await secrets.resolve("other", "http://127.0.0.1:8123") is None


def test_chrome_startup_diagnostic_keeps_the_cause_before_a_long_crash_stack() -> None:
    output = "FATAL: Socket path too long\n" + "stack frame\n" * 500
    log = BytesIO(output.encode())
    log.seek(0, 2)
    assert _diagnostic(log) == f":\n{output.strip()}"


def test_the_chrome_helper_preserves_a_child_exit_while_its_owner_pipe_is_open(tmp_path: Path) -> None:
    helper = Path(harness.__file__).with_name("_chrome.py")
    profile = tmp_path / "profile"
    profile.mkdir()
    with subprocess.Popen(
        [sys.executable, str(helper), str(profile), sys.executable, "-c", "raise SystemExit(7)"],
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    ) as process:
        try:
            assert process.wait(timeout=10) == 7
            assert process.stderr is not None
            assert b"Fatal Python error" not in process.stderr.read()
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
    assert not profile.exists()


@pytest.mark.skipif(os.name != "posix", reason="checks the owned process with POSIX signals")
@pytest.mark.parametrize("abort", [False, True])
def test_the_chrome_helper_stops_its_child_when_the_owner_closes_or_aborts(tmp_path: Path, abort: bool) -> None:
    helper = Path(harness.__file__).with_name("_chrome.py")
    marker = tmp_path / "browser.pid"
    profile = tmp_path / "profile"
    profile.mkdir()
    child = (
        "import os,sys,time; from pathlib import Path; "
        "p=Path(sys.argv[1]); ready=p.with_suffix('.ready'); "
        "ready.write_text(str(os.getpid())); ready.replace(p); time.sleep(60)"
    )
    owner = """
import os,subprocess,sys,time
from pathlib import Path
process = subprocess.Popen([sys.executable,sys.argv[1],sys.argv[5],sys.executable,'-c',sys.argv[3],sys.argv[2]],
                           stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
deadline = time.monotonic() + 10
while not Path(sys.argv[2]).exists():
    if process.poll() is not None or time.monotonic() > deadline:
        raise RuntimeError('browser did not start')
    time.sleep(.01)
if sys.argv[4] == 'abort':
    os._exit(17)
process.stdin.close()
process.wait(timeout=15)
"""
    stopped = False
    try:
        result = subprocess.run(
            [sys.executable, "-c", owner, str(helper), str(marker), child, "abort" if abort else "close", str(profile)],
            capture_output=True,
            timeout=25,
        )
        assert result.returncode == (17 if abort else 0), result.stderr.decode()
        pid = int(marker.read_text())
        deadline = time.monotonic() + 10
        while True:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                stopped = True
                break
            assert time.monotonic() < deadline, "owned browser survived its owner's pipe closing"
            time.sleep(0.01)
        while profile.exists():
            assert time.monotonic() < deadline, "owned profile survived its helper"
            time.sleep(0.01)
    finally:
        if not stopped and marker.exists():
            with suppress(ProcessLookupError):
                os.kill(int(marker.read_text()), signal.SIGTERM)
