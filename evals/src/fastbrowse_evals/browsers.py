"""The browsers the harness starts for its arms, each yielding the DevTools websocket every arm drives it through.

The harness owns every browser an arm runs in, fastbrowse's included: fastbrowse is handed it by `cdp_url` and
attaches to its tab, so the browser outlives the run and the harness reads the final page itself
(`fastbrowse_evals.final_page`), the same way for every arm. A Browser Use Cloud browser bills until it is
stopped, so it is stopped on every path out, a cancelled run included, and its metered cost is read from the stop.
"""

import asyncio
import contextlib
import logging
import time
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from pathlib import Path

import httpx
from fastbrowse.evals.harness import chrome
from fastbrowse.models import CostBasis, CostComponent, CostLine, Unavailable

from fastbrowse_evals.backend import RETRYABLE_STATUS, TRANSIENT_TRANSPORT, NotStarted, browser_key, refusal

CLOUD_BROWSER_MINUTES = 240
"""Lifetime requested for every Browser Use Cloud browser an arm drives: the provider's own maximum, so the only bound
left is the one no caller can lift (`fastbrowse_evals.live.PROVIDER_LIMITS`). Asked for shorter, a long run would
lose its browser partway, an implicit time cap."""
PROXY_SETTLE_SECONDS = 45
"""Browser Use meters a browser's proxy data after it stops, about 30 seconds later, and the stop's own answer says
zero until then; read sooner, a run's proxy dollars go missing from its cost."""
STOP_ATTEMPTS = 4
"""A browser whose stop failed bills until its own timeout, so a stop the service could not take is sent again."""
AGENT_SCREEN = (1120, 780)
"""The screen fastbrowse gives a cloud browser it starts itself (`DEFAULT_SCREEN` in crates/fastbrowse-browser), so a run
handed a browser instead sees the same size."""

_log = logging.getLogger(__name__)


@dataclass
class Browser:
    cdp_url: str
    """The DevTools websocket address every arm attaches to."""
    live_url: str | None = None
    """Where a cloud browser can be watched live."""
    session_id: str | None = None
    """The cloud browser's Browser Use session id; None for any other browser."""
    screen_sized: bool = False
    """Started at the size asked for, on a real screen, so no tab needs an override to reach it."""
    cost: tuple[CostLine, ...] = field(default=())
    """The cloud browser's metered browser and proxy cost, set once it has stopped; empty for any other browser."""
    open_seconds: float | None = None
    """Opening this browser, kept apart from every arm's `seconds`; None for a browser handed over ready."""
    close_seconds: float | None = None
    """Closing this browser, set once it has closed; a cloud browser's excludes the wait for its proxy cost to settle."""


def _failure(what: str, error: Exception, *, starting: bool = False) -> Exception:
    """A Browser Use API failure as an outage when a retry can clear it, and as `NotStarted` when it refused the
    request that starts the browser; never echoes a body, which can quote a key."""
    from browser_use_sdk.v4 import BrowserUseError

    if isinstance(error, BrowserUseError):
        refused = starting and refusal(error.status_code)
        failed = Unavailable if error.status_code in RETRYABLE_STATUS else NotStarted if refused else RuntimeError
        return failed(f"Browser Use Cloud {what} returned HTTP {error.status_code}")
    failed = Unavailable if isinstance(error, TRANSIENT_TRANSPORT) else RuntimeError
    return failed(f"Browser Use Cloud {what} failed ({type(error).__name__})")


def _cost(view: object, *, settled: bool = True) -> tuple[CostLine, ...]:
    def dollars(value: object) -> float | None:
        return None if value is None else float(str(value))

    browser, proxy = dollars(getattr(view, "browser_cost", None)), dollars(getattr(view, "proxy_cost", None))
    return (
        CostLine(
            component=CostComponent.BROWSER,
            basis=CostBasis.UNKNOWN if browser is None else CostBasis.METERED,
            dollars=browser,
        ),
        CostLine(
            component=CostComponent.PROXY,
            basis=CostBasis.UNKNOWN if proxy is None else CostBasis.METERED if settled else CostBasis.UNSETTLED,
            dollars=proxy,
        ),
    )


async def devtools_websocket(http: httpx.AsyncClient, cdp_url: str) -> str:
    if cdp_url.startswith(("ws://", "wss://")):
        return cdp_url
    try:
        response = await http.get(f"{cdp_url.rstrip('/')}/json/version", timeout=30)
    except httpx.HTTPError as error:
        raise _failure("browser discovery", error) from None
    if not response.is_success:
        failed = Unavailable if response.status_code in RETRYABLE_STATUS else RuntimeError
        raise failed(f"Browser Use Cloud browser discovery returned HTTP {response.status_code}")
    return str(response.json()["webSocketDebuggerUrl"])


@contextlib.asynccontextmanager
async def cloud_browser(http: httpx.AsyncClient, *, screen: tuple[int, int] | None = None) -> AsyncIterator[Browser]:
    """A Browser Use Cloud browser on its V4 API, asked for `CLOUD_BROWSER_MINUTES`, with the service's protections
    named: its US residential proxy and CAPTCHA solving, under its stealth defaults. `screen` is its real screen size;
    the service's `allowResizing` is left off because it documents that as less stealthy, and a browser without it
    ignores tab size overrides."""
    from browser_use_sdk.v4 import AsyncBrowserUse, BrowserUseError

    client = AsyncBrowserUse(api_key=browser_key())
    width, height = screen or (None, None)
    began = time.monotonic()
    created = None
    creation = asyncio.ensure_future(
        client.browsers.create(
            proxy_country_code="us",
            solve_captchas=True,
            timeout=CLOUD_BROWSER_MINUTES,
            browser_screen_width=width,
            browser_screen_height=height,
        )
    )
    try:
        try:
            # A cancelled create can still start a billable browser, so its id is waited for.
            created = await asyncio.shield(creation)
        except (BrowserUseError, httpx.HTTPError) as error:
            raise _failure("create", error, starting=True) from None
        browser = None
        try:
            if created.cdp_url is None:
                raise RuntimeError(f"Browser Use Cloud browser {created.id} started without a CDP URL")
            websocket = await devtools_websocket(http, created.cdp_url)
            opened = round(time.monotonic() - began, 2)
            browser = Browser(
                websocket,
                created.live_url,
                session_id=str(created.id),
                screen_sized=screen is not None,
                open_seconds=opened,
            )
            yield browser
        finally:
            closing = time.monotonic()
            stopped = await asyncio.shield(_stop(client, str(created.id)))
            if browser is not None:
                browser.close_seconds = round(time.monotonic() - closing, 2)
            settled = None if stopped is None else await asyncio.shield(_settled(client, str(created.id)))
            if browser is not None:
                browser.cost = _cost(settled or stopped, settled=settled is not None)
    finally:
        await asyncio.gather(creation, return_exceptions=True)
        # An attempt cancelled while its create was in flight never reached the stop above.
        if created is None and not creation.cancelled() and creation.exception() is None:
            await asyncio.shield(_stop(client, str(creation.result().id)))
        await client.close()


async def _settled(client: object, browser_id: str) -> object | None:
    """The stopped browser's view once its proxy data is metered; None if it could not be read."""
    from browser_use_sdk.v4 import BrowserUseError

    await asyncio.sleep(PROXY_SETTLE_SECONDS)
    with contextlib.suppress(BrowserUseError, httpx.HTTPError):
        return await client.browsers.get(browser_id)  # ty: ignore[unresolved-attribute]
    return None


async def _stop(client: object, browser_id: str) -> object | None:
    """The stopped browser's view, which carries its cost; None if every attempt failed, which is logged."""
    from browser_use_sdk.v4 import BrowserUseError

    for attempt in range(STOP_ATTEMPTS):
        try:
            return await client.browsers.stop(browser_id)  # ty: ignore[unresolved-attribute]
        except (BrowserUseError, httpx.HTTPError) as error:
            retryable = isinstance(error, TRANSIENT_TRANSPORT) or (
                isinstance(error, BrowserUseError) and error.status_code in RETRYABLE_STATUS
            )
            if not retryable or attempt + 1 == STOP_ATTEMPTS:
                _log.warning(
                    "Browser Use Cloud browser %s was not stopped (%s); it runs until its %d-minute timeout",
                    browser_id,
                    type(error).__name__,
                    CLOUD_BROWSER_MINUTES,
                )
                return None
            after = getattr(error, "retry_after", None)
            await asyncio.sleep(after if isinstance(after, int | float) else 2**attempt)
    return None


@contextlib.asynccontextmanager
async def local_browser(binary: Path) -> AsyncIterator[Browser]:
    """`binary` started headless on a throwaway profile, as fastbrowse's eval harness starts its own Chrome."""
    owned = chrome(str(binary))
    began = time.monotonic()
    cdp_url = await asyncio.to_thread(owned.__enter__)
    browser = Browser(cdp_url, open_seconds=round(time.monotonic() - began, 2))
    try:
        yield browser
    finally:
        closing = time.monotonic()
        await asyncio.to_thread(owned.__exit__, None, None, None)
        browser.close_seconds = round(time.monotonic() - closing, 2)
