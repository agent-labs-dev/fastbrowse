"""One-time codes from an authenticator key (a base32 secret or an `otpauth://` URI), as the Rust core makes them."""

import asyncio
import importlib
import time
from collections.abc import Awaitable, Callable
from typing import Any

# Compiled, so the checker has no types for it.
_core: Any = importlib.import_module("fastbrowse._native")

# A code with less than this left would die between being typed and being submitted.
_GUARD_SECONDS = 5.0


class TotpError(ValueError):
    """The key cannot make codes; the message says why and never repeats the key."""


def totp_at(key: str, when: float) -> str:
    """The code `key` gives at unix time `when`."""
    try:
        return _core.totp_at(key, when)
    except ValueError as exc:
        raise TotpError(str(exc)) from None


def one_time_code(key: str) -> Callable[[], Awaitable[str]]:
    """Validate `key` now and return a coroutine function that makes a fresh code each time it is awaited.

    With under five seconds of the period left (at most half of it), it waits for the next period first, as the
    core's own one-time codes do.
    """
    try:
        period = float(_core.totp_period(key))
    except ValueError as exc:
        raise TotpError(str(exc)) from None
    guard = min(_GUARD_SECONDS, period / 2)

    async def code() -> str:
        now = time.time()
        remaining = period - now % period
        if remaining < guard:
            await asyncio.sleep(remaining)
            now = time.time()
        return totp_at(key, now)

    return code
