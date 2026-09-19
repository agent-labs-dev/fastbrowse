"""Chrome on this machine: headless with a throwaway profile by default."""

import asyncio
import json
import os
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.request
from collections.abc import AsyncGenerator, Generator
from contextlib import asynccontextmanager, contextmanager, nullcontext
from pathlib import Path
from typing import IO

from fastbrowse.models import BrowserConnection, LocalChrome


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


def find_chrome(override: str | None) -> str | None:
    """`override` is `Settings.chrome`, a name or path that replaces discovery rather than joining it."""
    if override:
        return shutil.which(override)
    for name in (
        "google-chrome-stable",
        "google-chrome",
        "chromium",
        "chromium-browser",
        "chrome",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        *_windows_installs(),
    ):
        if binary := shutil.which(name):
            return binary
    return None


def _windows_installs() -> tuple[str, ...]:
    """Chrome's per-machine and per-user installs, which its Windows installer leaves off PATH."""
    roots = (os.environ.get(name) for name in ("PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"))
    return tuple(str(Path(root, "Google", "Chrome", "Application", "chrome.exe")) for root in roots if root)


@asynccontextmanager
async def async_local_chrome(options: LocalChrome) -> AsyncGenerator[BrowserConnection]:
    manager = local_chrome(options)
    opening = asyncio.create_task(asyncio.to_thread(manager.__enter__))
    try:
        try:
            connection = await asyncio.shield(opening)
        finally:
            # Threads cannot be cancelled; wait for startup before attempting to release its process.
            await asyncio.gather(opening, return_exceptions=True)
        yield connection
    finally:
        closing = asyncio.create_task(asyncio.to_thread(manager.__exit__, None, None, None))
        try:
            await asyncio.shield(closing)
        finally:
            await asyncio.gather(closing, return_exceptions=True)


@contextmanager
def local_chrome(options: LocalChrome) -> Generator[BrowserConnection]:
    """Yield a connection to a Chrome launched with `options`, killed on exit."""
    binary = find_chrome(options.binary)
    if binary is None:
        raise RuntimeError("Chrome is not installed")
    kept = options.profile
    with (
        nullcontext(str(kept.expanduser()))
        if kept
        else tempfile.TemporaryDirectory(ignore_cleanup_errors=True) as profile,
        tempfile.TemporaryFile() as log,
    ):
        # Chrome picks its own port and writes it to the profile. Choosing a free port here and handing it over
        # left a window in which something else could take it, and Chrome then never answered.
        active = Path(profile) / "DevToolsActivePort"
        active.unlink(missing_ok=True)
        proc = subprocess.Popen(
            [
                binary,
                *(() if options.headed else ("--headless=new",)),
                # Headless defaults to 800x600, where responsive sites collapse their header into a
                # toggle and the control the agent needs is not in the page at all.
                "--window-size=1280,900",
                "--remote-debugging-port=0",
                f"--user-data-dir={profile}",
                "--no-first-run",
                "--disable-popup-blocking",
                "about:blank",
            ],
            stdout=subprocess.DEVNULL,
            stderr=log,
        )
        try:
            yield BrowserConnection(cdp_url=_wait_for_ws(active, proc, log), live_url=None, remote=False)
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()


def _wait_for_ws(active: Path, proc: subprocess.Popen[bytes], log: IO[bytes], timeout: float = 90.0) -> str:
    """Wait for Chrome to report its DevTools address, failing with its own output if it exits or never does.

    A first start reads Chrome's binary and resources from a cold disk: on CI runners that took 7s in one run
    and just over 30s in another, with every Chrome process blocked on page-in, and printed "DevTools
    listening" as the old 30s limit failed the whole browser suite.
    """
    deadline = time.monotonic() + timeout
    while True:
        if proc.poll() is not None:
            raise RuntimeError(f"Chrome exited with status {proc.returncode} before DevTools started{_tail(log)}")
        try:
            port = int(active.read_text().split("\n")[0])
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version", timeout=1) as response:
                return str(json.load(response)["webSocketDebuggerUrl"])
        except OSError, ValueError:
            # Not written yet, written only partly, or written and not yet listening.
            if time.monotonic() > deadline:
                raise RuntimeError(f"Chrome did not start DevTools within {timeout:.0f}s{_tail(log)}") from None
            time.sleep(0.1)


def _tail(log: IO[bytes]) -> str:
    log.seek(0)
    text = log.read()[-2000:].decode(errors="replace").strip()
    return f":\n{text}" if text else ""
