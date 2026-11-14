"""Cancelling the recipe benchmark must stop the CLI that owns its browser."""

import os
import signal
import subprocess
import sys
import time
from contextlib import suppress
from pathlib import Path

import pytest


@pytest.mark.parametrize("interrupt", [signal.SIGINT, signal.SIGTERM])
def test_interrupting_the_benchmark_stops_its_browser_process(tmp_path: Path, interrupt: signal.Signals) -> None:
    started = tmp_path / "started"
    stopped = tmp_path / "stopped"
    binary = tmp_path / "fastbrowse"
    binary.write_text(
        f"#!{sys.executable}\n"
        "import os, signal, sys, time\n"
        "from pathlib import Path\n"
        "def cancel(*args):\n"
        f"    Path({str(stopped)!r}).touch()\n"
        "    sys.exit(0)\n"
        "signal.signal(signal.SIGTERM, cancel)\n"
        "signal.signal(signal.SIGINT, cancel)\n"
        f"Path({str(started)!r}).write_text(str(os.getpid()))\n"
        "while True:\n"
        "    time.sleep(0.01)\n"
    )
    binary.chmod(0o755)
    benchmark = subprocess.Popen(
        [
            sys.executable,
            "-c",
            "import sys; from pathlib import Path; "
            "from fastbrowse_evals.recipes import TASKS, run; "
            "run(sys.argv[1], ['--cloud'], TASKS[0], Path(sys.argv[2]), 60)",
            str(binary),
            str(tmp_path),
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        deadline = time.monotonic() + 10
        while not started.exists() and time.monotonic() < deadline:
            time.sleep(0.01)
        assert started.exists(), "the CLI must start before the benchmark is interrupted"
        benchmark.send_signal(interrupt)
        benchmark.wait(timeout=40)
        assert stopped.exists(), "benchmark cancellation left its CLI and owned cloud browser running"
    finally:
        if benchmark.poll() is None:
            benchmark.kill()
            benchmark.wait()
        if started.exists() and not stopped.exists():
            with suppress(ProcessLookupError):
                os.kill(int(started.read_text()), signal.SIGTERM)
