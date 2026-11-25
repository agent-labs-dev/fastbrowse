"""The binary's smoke script, run here against the checkout's own `fastbrowse` command.

CI runs it against each built binary and wheel. Running it here keeps the script and the scripted models honest between
builds, and shows that a broken executable fails it.
"""

import shutil
import subprocess
import sys
from contextlib import suppress
from pathlib import Path

import pytest

from fastbrowse.evals.harness import find_chrome
from scripts.smoke_binary import Client, Failed, serving

REPO = Path(__file__).resolve().parent.parent
SMOKE = REPO / "scripts" / "smoke_binary.py"


def _smoke(executable: str, *arguments: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run([sys.executable, str(SMOKE), executable, *arguments], capture_output=True, text=True)


def _command() -> str:
    command = shutil.which("fastbrowse", path=str(Path(sys.executable).parent))
    assert command is not None, "the project is not installed in this environment"
    return command


def test_the_checkout_passes_the_handshake_case() -> None:
    done = _smoke(_command(), "--case", "initialize")

    assert done.returncode == 0, done.stderr
    assert done.stdout.startswith("initialize: fastbrowse ")


def test_the_checkout_completes_the_scripted_run_on_local_chrome() -> None:
    if find_chrome() is None:
        pytest.skip("Chrome is not installed")

    done = _smoke(_command(), "--case", "run")

    assert done.returncode == 0, done.stderr
    # The scripted click, then the read of the page the driver called done on.
    assert done.stdout.startswith("run: complete in 2 steps against http://127.0.0.1:")


def test_an_executable_that_is_not_fastbrowse_fails_and_names_the_case() -> None:
    done = _smoke(sys.executable, "--case", "initialize")

    assert done.returncode == 1
    assert done.stderr.splitlines()[-1].startswith("FAILED initialize: ")


def test_client_reports_a_server_that_exited_before_accepting_a_request() -> None:
    process = subprocess.Popen([sys.executable, "-c", "pass"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    try:
        process.wait()
        client = Client(process)
        with pytest.raises(Failed, match=r"initialize: server exited before accepting request, exit status 0"):
            client.request("initialize", None, 1)
    finally:
        if process.stdin is not None:
            with suppress(BrokenPipeError):
                process.stdin.close()
        if process.poll() is None:
            process.kill()


def test_serving_preserves_pipe_failure_during_context_cleanup() -> None:
    with (
        pytest.raises(Failed, match=r"initialize: server exited before accepting request, exit status 2"),
        serving(Path(sys.executable)) as client,
    ):
        client._process.wait()
        client.request("initialize", None, 1)


def test_serving_rejects_an_unexpected_exit_during_shutdown() -> None:
    with (
        pytest.raises(Failed, match=r"shutdown: server exited before accepting request, exit status 2"),
        serving(Path(sys.executable)) as client,
    ):
        client._process.wait()
