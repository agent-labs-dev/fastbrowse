"""Run an arm's runner or a reset command as its own process group, and stop the whole group when interrupted."""

import asyncio
import contextlib
import os
import signal
from collections.abc import Mapping, Sequence


class CommandError(RuntimeError):
    pass


def _kill(process: asyncio.subprocess.Process) -> None:
    # start_new_session makes the child its own group leader, so its pid is the group id. Signal the group even
    # when the leader has already exited, or a child it spawned would survive the timeout.
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except (AttributeError, ProcessLookupError, PermissionError):
        with contextlib.suppress(ProcessLookupError):
            process.kill()


async def _terminate(process: asyncio.subprocess.Process) -> None:
    _kill(process)
    with contextlib.suppress(asyncio.CancelledError, ProcessLookupError):
        await process.wait()


async def run_command(
    command: Sequence[str],
    payload: bytes,
    *,
    seconds: float | None,
    environment: Mapping[str, str],
    cwd: str | None = None,
) -> bytes:
    """Run `command` with `payload` on stdin and return its stdout. `seconds` bounds a reset command; None waits for
    the process however long it takes, which is how an arm's runner is started, since no benchmark arm is ever
    time-limited. Either way an interrupted wait kills the whole process group."""
    try:
        process = await asyncio.create_subprocess_exec(
            *command,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=dict(environment),
            cwd=cwd,
            start_new_session=True,
        )
    except FileNotFoundError as exc:
        raise CommandError(f"command not found: {command[0]}") from exc
    except OSError as exc:
        raise CommandError(f"command could not start: {exc}") from exc
    try:
        async with asyncio.timeout(seconds):
            stdout, stderr = await process.communicate(payload)
    except TimeoutError as exc:
        await _terminate(process)
        raise CommandError(f"command exceeded {seconds:g}s and was terminated") from exc
    except BaseException:
        await _terminate(process)
        raise
    if process.returncode != 0:
        detail = stderr.decode("utf-8", errors="replace").strip()[:500]
        raise CommandError(f"command exited {process.returncode}: {detail}")
    return stdout
