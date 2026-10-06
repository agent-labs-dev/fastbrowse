"""The frozen binary's smoke script, run here against the checkout's own `fastbrowse` command.

CI runs it against each frozen build. Running it here keeps the script and the scripted models honest between
builds, and shows that a broken executable fails it.
"""

import shutil
import subprocess
import sys
from pathlib import Path

import pytest

from fastbrowse.adapters.local_chrome import find_chrome

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
    if find_chrome(None) is None:
        pytest.skip("Chrome is not installed")

    done = _smoke(_command(), "--case", "run")

    assert done.returncode == 0, done.stderr
    assert done.stdout.startswith("run: complete in 1 steps against http://127.0.0.1:")


def test_an_executable_that_is_not_fastbrowse_fails_and_names_the_case() -> None:
    done = _smoke(sys.executable, "--case", "initialize")

    assert done.returncode == 1
    assert done.stderr.splitlines()[-1].startswith("FAILED initialize: ")
