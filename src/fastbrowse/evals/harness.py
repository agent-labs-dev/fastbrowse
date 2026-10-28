"""What the eval harness brings to a run itself: its own Chrome, the secrets a task declares, and what it reads back.

The harness drives fastbrowse only as an embedder does, through `run_task`, `serve --stdio` and the trace, so it
measures whichever core the package runs. A helper the core also has is written again here rather than imported,
since the core's copy is free to change.
"""

import json
import os
import shutil
import subprocess
import tempfile
import time
import urllib.request
from collections.abc import Generator, Iterable, Mapping
from contextlib import contextmanager
from pathlib import Path
from typing import IO
from urllib.parse import urlsplit

from fastbrowse.models import SecretRef

CHROME_ENV = "FASTBROWSE_CHROME"
"""The Chrome binary, a name or a path, as the core reads it; unset, the usual install locations are searched."""


def find_chrome(override: str | None = None) -> str | None:
    override = override or os.environ.get(CHROME_ENV)
    if override:
        return shutil.which(override)
    for name in (
        "google-chrome-stable",
        "google-chrome",
        "chromium",
        "chromium-browser",
        "chrome",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ):
        if binary := shutil.which(name):
            return binary
    return None


@contextmanager
def chrome(binary: str | None = None, *, timeout: float = 90.0) -> Generator[str]:
    """A headless Chrome on a throwaway profile, killed on exit, yielding its DevTools websocket URL.

    The harness owns the browser and hands it to the run by `cdp_url`, so one Chrome serves a whole campaign and
    a process measured for its CPU and memory is the agent alone. Launched as the core launches its own.
    """
    found = find_chrome(binary)
    if found is None:
        raise RuntimeError("Chrome is not installed")
    with tempfile.TemporaryDirectory(ignore_cleanup_errors=True) as profile, tempfile.TemporaryFile() as log:
        # Chrome's password manager takes input after a sign-in, outside the page the run sees.
        preferences = Path(profile, "Default", "Preferences")
        preferences.parent.mkdir(parents=True)
        preferences.write_text(
            json.dumps(
                {
                    "credentials_enable_service": False,
                    "profile": {"password_manager_enabled": False, "password_manager_leak_detection": False},
                }
            ),
            encoding="utf-8",
        )
        process = subprocess.Popen(
            [
                found,
                "--headless=new",
                "--window-size=1280,900",
                "--remote-debugging-port=0",
                f"--user-data-dir={profile}",
                "--no-first-run",
                "--disable-popup-blocking",
                "--no-startup-window",
            ],
            stdout=subprocess.DEVNULL,
            stderr=log,
        )
        try:
            yield _devtools(Path(profile, "DevToolsActivePort"), process, log, timeout)
        finally:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def _devtools(active: Path, process: subprocess.Popen[bytes], log: IO[bytes], timeout: float) -> str:
    deadline = time.monotonic() + timeout
    while True:
        if process.poll() is not None:
            raise RuntimeError(f"Chrome exited with status {process.returncode} before DevTools started{_tail(log)}")
        try:
            port = int(active.read_text(encoding="utf-8").split("\n")[0])
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version", timeout=1) as response:
                return str(json.load(response)["webSocketDebuggerUrl"])
        except (OSError, ValueError):
            # Not written yet, written only partly, or written and not yet listening.
            if time.monotonic() > deadline:
                raise RuntimeError(f"Chrome did not start DevTools within {timeout:.0f}s{_tail(log)}") from None
            time.sleep(0.1)


def _tail(log: IO[bytes]) -> str:
    log.seek(0)
    text = log.read()[-2000:].decode(errors="replace").strip()
    return f":\n{text}" if text else ""


def origin_of(url: str) -> str:
    """`scheme://host[:port]`, the port dropped when the scheme implies it: the form a run names an origin in."""
    parts = urlsplit(url)
    default = {"http": 80, "https": 443}.get(parts.scheme)
    port = "" if parts.port in (None, default) else f":{parts.port}"
    return f"{parts.scheme}://{(parts.hostname or '').lower()}{port}"


class TaskSecrets:
    """A task's secrets, each given only on the one origin its fixture site is served from: a `SecretResolver`."""

    def __init__(self, values: Mapping[str, str], origin: str) -> None:
        self._values = dict(values)
        self._origin = origin_of(origin)

    def available(self) -> tuple[SecretRef, ...]:
        return tuple(SecretRef(name=name, origins=(self._origin,)) for name in self._values)

    async def resolve(self, name: str, origin: str) -> str | None:
        return self._values.get(name) if origin_of(origin) == self._origin else None


def transient_seconds(events: Iterable[object], began: float, ended: float) -> float:
    """Time within `began`..`ended` that calls spent on transient provider failures, calls that overlapped
    counted once. Evals run an attempt with any of it again rather than subtract it, which would credit the one arm
    whose requests are visible. An upper bound on the delay: a call running beside the failed one may have taken as
    long anyway."""
    spans = sorted(
        (max(float(event["began"]), began), min(float(event["ended"]), ended))
        for event in events
        if isinstance(event, dict) and event.get("event") == "request_transient"
    )
    total, reach = 0.0, began
    for start, end in spans:
        start = max(start, reach)
        if end > start:
            total += end - start
            reach = end
    return total


def providers(events: Iterable[object]) -> str | None:
    """The providers a run called, from its `route` trace event; None from a run that described none."""
    for event in events:
        if isinstance(event, dict) and event.get("event") == "route":
            return str(event["providers"])
    return None
