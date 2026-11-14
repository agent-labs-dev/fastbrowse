"""A recipe benchmark timeout must let the CLI close its owned browser."""

import json
import sys
from pathlib import Path

from fastbrowse_evals.recipes import TASKS, run


def test_a_timeout_allows_the_browser_process_to_clean_up(tmp_path: Path) -> None:
    started = tmp_path / "started"
    stopped = tmp_path / "stopped"
    binary = tmp_path / "fastbrowse"
    binary.write_text(
        f"#!{sys.executable}\n"
        "import signal, sys, time\n"
        "from pathlib import Path\n"
        "def cancel(*args):\n"
        f"    Path({str(stopped)!r}).write_text('browser stopped')\n"
        f"    print({json.dumps({'status': 'error', 'error': 'cancelled'})!r}, flush=True)\n"
        "    sys.exit(0)\n"
        "signal.signal(signal.SIGTERM, cancel)\n"
        "signal.signal(signal.SIGINT, cancel)\n"
        f"Path({str(started)!r}).touch()\n"
        "while True:\n"
        "    time.sleep(0.01)\n"
    )
    binary.chmod(0o755)

    run(str(binary), ["--cloud"], TASKS[0], tmp_path, timeout=1.0)

    assert started.exists(), "the fake browser must have started before the timeout"
    assert stopped.exists(), "timeout killed the CLI without letting it stop its owned browser"
