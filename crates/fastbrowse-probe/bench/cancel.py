"""Cancel fastbrowse-probe mid-snapshot and check what it leaves in a browser it does not own.

    cargo build --profile release-small
    uv run python crates/fastbrowse-probe/bench/cancel.py

Starts one headless Chrome with a fixed debugging port, then cancels the probe during the snapshot's loading wait
on bench/sites/slow.html (its spinner never clears, so the wait takes 3s): once by `--timeout-ms` in a tab the
probe opened, which must be closed, once by SIGINT in a tab it attached to, which must stay open.
"""

import json
import signal
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from serve import ROOT, serve_all

from fastbrowse.adapters.local_chrome import find_chrome, free_port

PROBE = ROOT / "target" / "release-small" / "fastbrowse-probe"


def pages(port: int) -> list[str]:
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list") as reply:
        return sorted(t["url"] for t in json.load(reply) if t["type"] == "page")


def report(name: str, done: subprocess.CompletedProcess | tuple[int, str], before: list[str], port: int) -> None:
    code, stderr = (done.returncode, done.stderr) if isinstance(done, subprocess.CompletedProcess) else done
    lines = [line for line in stderr.splitlines() if "pending" in line or "cancelled" in line]
    print(f"{name}: exit {code}; {' | '.join(lines)}")
    print(f"  pages before {before}\n  pages after  {pages(port)}")


def main() -> None:
    slow = serve_all()["slow"]
    port = free_port()
    with tempfile.TemporaryDirectory() as profile:
        chrome = subprocess.Popen(
            [
                str(find_chrome(None)),
                "--headless=new",
                f"--remote-debugging-port={port}",
                f"--user-data-dir={profile}",
                "--no-first-run",
                "about:blank",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            for _ in range(100):
                try:
                    with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version") as reply:
                        ws = json.load(reply)["webSocketDebuggerUrl"]
                    break
                except OSError:
                    time.sleep(0.1)
            before = pages(port)
            started = time.monotonic()
            done = subprocess.run(
                [str(PROBE), slow, "--cdp-url", ws, "--timeout-ms", "1500"], capture_output=True, text=True
            )
            print(f"(took {time.monotonic() - started:.2f}s; the snapshot alone would take 3s)")
            report("opened tab, --timeout-ms 1500", done, before, port)

            # A tab of the user's, on the slow page, that the probe attaches to.
            urllib.request.urlopen(urllib.request.Request(f"http://127.0.0.1:{port}/json/new?{slow}", method="PUT"))
            time.sleep(1)
            before = pages(port)
            probe = subprocess.Popen(
                [str(PROBE), "--attach", "slow.html", "--cdp-url", ws], stderr=subprocess.PIPE, text=True
            )
            time.sleep(1.2)  # inside the snapshot's loading wait
            probe.send_signal(signal.SIGINT)
            _, stderr = probe.communicate(timeout=10)
            report("attached tab, SIGINT", (probe.returncode, stderr), before, port)
        finally:
            chrome.kill()
            chrome.wait()


main()
