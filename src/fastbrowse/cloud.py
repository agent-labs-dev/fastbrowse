"""A Browser Use Cloud browser the caller starts and stops, for runs to share.

`run_task` starts a cloud browser for one run and stops it when the run ends. A caller that keeps one browser per
user session starts it here instead, passes `connection.cdp_url` to each `run_task(cdp_url=...)` or `connect_cdp`, and
stops it when the session ends:

    async with BrowserUseCloudBrowser(profile=profile_id) as cloud:
        result = await run_task(task, cdp_url=cloud.connection.cdp_url, foreground=True)
    cloud.cost  # metered lines, read after the stop

It is the Rust core's adapter, the one a run starts its own browser with, so the request, the cancellation handling
and the cost are the run's.
"""

import asyncio
import contextlib
import importlib
import json
from collections.abc import Sequence
from types import TracebackType
from typing import Any, Self

from pydantic import TypeAdapter

from fastbrowse.models import BrowserConnection, ConfigurationError, CostLine, Unavailable

# Compiled, so the checker has no types for it.
_core: Any = importlib.import_module("fastbrowse._native")

_ORIGIN: str | None = None
"""Where the Browser Use API is served from, when not from Browser Use: a test serves it itself."""

_COST: TypeAdapter[tuple[CostLine, ...]] = TypeAdapter(tuple[CostLine, ...])


class BrowserUseCloudError(RuntimeError):
    """The cloud browser could not be made, started or stopped. The message never carries the API key."""


class BrowserUseCloudUnavailable(BrowserUseCloudError, Unavailable):
    """Browser Use Cloud could not be reached, or answered with a retryable status."""

    def __init__(self, message: str, *, retry_after: float | None = None) -> None:
        super().__init__(message)
        self.retry_after = retry_after
        """Seconds the service asked to be left alone for, when it said."""


def _raised(failed: Exception) -> Exception:
    kind, message, retry_after = failed.args
    match kind:
        case "invalid":
            return ValueError(message)
        case "configuration":
            return ConfigurationError(message)
        case "unavailable":
            return BrowserUseCloudUnavailable(message, retry_after=retry_after)
        case _:
            return BrowserUseCloudError(message)


class BrowserUseCloudBrowser:
    """One Browser Use Cloud browser, billed from `start` until `stop` or its timeout.

    `api_key` falls back to `BROWSER_USE_API_KEY` from the environment, then `.env`. The options are `run_task`'s:
    `profile` is its `cloud_profile`, `extensions` its `cloud_extensions` (up to three extension UUIDs),
    `timeout_minutes` its `cloud_timeout_minutes` (15 by default; the service allows up to 240), and `proxy_country` and
    `viewport` the same; `proxy_country=None` sends no proxy country. They are checked here, before any request.
    """

    def __init__(
        self,
        api_key: str | None = None,
        *,
        profile: str | None = None,
        extensions: Sequence[str] = (),
        timeout_minutes: int = 15,
        proxy_country: str | None = "us",
        viewport: tuple[int, int] | None = None,
    ) -> None:
        options = {
            "proxy_country": proxy_country,
            "timeout_minutes": timeout_minutes,
            "profile": profile,
            "viewport": None if viewport is None else list(viewport),
            "extensions": list(extensions),
        }
        try:
            self._browser = _core.CloudBrowser(json.dumps(options), api_key=api_key, origin=_ORIGIN)
        except _core.CloudFailed as exc:
            raise _raised(exc) from None
        self._started = False
        self._connection: BrowserConnection | None = None
        self.cost: tuple[CostLine, ...] = ()
        """The browser and proxy lines the service metered, as of its latest answer: final for the browser once
        `stop` returns. The proxy line is unsettled, since the service meters proxy data about 30 seconds after a
        stop."""

    @property
    def connection(self) -> BrowserConnection:
        """Where runs attach, between `start` and `stop`."""
        if self._connection is None:
            raise BrowserUseCloudError("the cloud browser is not running")
        return self._connection

    async def start(self) -> BrowserConnection:
        """Create the browser and find its DevTools address. A browser created by a start that fails or is cancelled
        is stopped before the error is raised."""
        if self._started:
            raise BrowserUseCloudError("the cloud browser is already started")
        self._started = True
        try:
            answer = await self._browser.start()
        except _core.CloudFailed as exc:
            self._started = False
            self._read_cost()
            raise _raised(exc) from None
        except BaseException:
            # A cancelled create can still make a billable browser, and nobody else holds it to stop.
            with contextlib.suppress(Exception):
                await asyncio.shield(asyncio.ensure_future(self.stop()))
            raise
        self._connection = BrowserConnection.model_validate_json(answer)
        return self._connection

    async def stop(self) -> None:
        """Stop the browser, which bills until stopped or timed out, and read its final cost. A start still in flight
        is waited for, so its browser is stopped too. Stopping twice is harmless; a stop that failed can be retried."""
        self._connection = None
        self._started = False
        try:
            await self._browser.stop()
        except _core.CloudFailed as exc:
            raise _raised(exc) from None
        finally:
            self._read_cost()

    def _read_cost(self) -> None:
        # Empty while a cancelled start or stop still holds the browser; the last reading stands until then.
        if lines := _COST.validate_json(self._browser.cost()):
            self.cost = lines

    async def __aenter__(self) -> Self:
        await self.start()
        return self

    async def __aexit__(
        self, exc_type: type[BaseException] | None, exc: BaseException | None, tb: TracebackType | None
    ) -> None:
        try:
            await self.stop()
        except Exception:
            # The block's own error is the one the caller needs; a failed stop is logged by the core.
            if exc is None:
                raise
