"""The fastbrowse pool harness.

Campaign execution, retries, grading, budgets, evidence, and result storage are
provided by Parallax. The pool adapter is kept here because it is specific to
fastbrowse's browser service.
"""

from __future__ import annotations

import asyncio
import logging
import os
import time
import uuid
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from dataclasses import dataclass
from urllib.parse import quote, urlsplit, urlunsplit

import httpx
from parallax.harnesses.browser.cloud import Browser
from parallax.harnesses.browser.site import Unavailable, retryable_status, transient_transport
from parallax.models import CostSource, HarnessInfo, HarnessKind, Isolation

from fastbrowse_evals.backend import FastbrowseHarness

POOL_ORGANIZATION = "parallax-benchmark"
POOL_LEASE_MINUTES = 120
POOL_EXTEND_EVERY = 300
POOL_EXTEND_SECONDS = 900
POOL_FULL_WAIT = (2.0, 30.0)
_pool_log = logging.getLogger("fastbrowse_evals.pool")


@dataclass
class PoolLease:
    lease_id: str
    ws_url: str
    acquired: float
    full_wait: float
    extend_failures: int = 0
    released: float | None = None


def pool_settings() -> tuple[str, str]:
    url, token = os.environ.get("PARALLAX_POOL_URL"), os.environ.get("PARALLAX_POOL_TOKEN")
    if not url or not token:
        raise RuntimeError("fastbrowse-pool needs PARALLAX_POOL_URL and PARALLAX_POOL_TOKEN")
    parts = urlsplit(url)
    if parts.scheme not in {"http", "https"} or not parts.netloc:
        raise RuntimeError("PARALLAX_POOL_URL must be an http(s) URL")
    return url.rstrip("/"), token


def pool_host() -> str | None:
    url = os.environ.get("PARALLAX_POOL_URL")
    return urlsplit(url).hostname if url else None


def on_pool_origin(url: str, pool: str) -> str:
    parts, base = urlsplit(url), urlsplit(pool)
    secure = base.scheme == "https"
    scheme = ("wss" if secure else "ws") if parts.scheme in {"ws", "wss"} else base.scheme
    return urlunsplit((scheme, base.netloc, parts.path, parts.query, parts.fragment))


def _pool_failure(what: str, status: int | None = None, error: Exception | None = None) -> Exception:
    if error is not None:
        failure = Unavailable if transient_transport(error) else RuntimeError
        return failure(f"browser pool {what} failed ({type(error).__name__})")
    failure = Unavailable if retryable_status(status) else RuntimeError
    return failure(f"browser pool {what} returned HTTP {status}")


async def _keep_lease(http: httpx.AsyncClient, lease_url: str, headers: dict[str, str], lease: PoolLease) -> None:
    while True:
        await asyncio.sleep(POOL_EXTEND_EVERY)
        try:
            response = await http.post(
                f"{lease_url}/extend", params={"seconds": POOL_EXTEND_SECONDS}, headers=headers, timeout=30
            )
            ok, why = response.is_success, f"HTTP {response.status_code}"
        except httpx.HTTPError as error:
            ok, why = False, type(error).__name__
        if not ok:
            lease.extend_failures += 1
            _pool_log.warning("browser pool lease %s was not extended (%s)", lease.lease_id, why)


@asynccontextmanager
async def pool_lease(http: httpx.AsyncClient, run_id: str) -> AsyncIterator[PoolLease]:
    pool, token = pool_settings()
    headers = {"Authorization": f"Bearer {token}"}
    began = time.monotonic()
    full_wait = 0.0
    wait, longest = POOL_FULL_WAIT
    while True:
        try:
            response = await http.post(
                f"{pool}/leases",
                headers=headers,
                json={"organization_id": POOL_ORGANIZATION, "run_id": run_id, "timeout_minutes": POOL_LEASE_MINUTES},
                timeout=60,
            )
        except httpx.HTTPError as error:
            raise _pool_failure("lease", error=error) from None
        if response.status_code != 503:
            break
        paused = time.monotonic()
        await asyncio.sleep(wait)
        full_wait += time.monotonic() - paused
        wait = min(wait * 2, longest)
    if not response.is_success:
        raise _pool_failure("lease", response.status_code)
    try:
        body = response.json()
        lease_id = str(body["lease_id"])
        cdp_base = on_pool_origin(str(body["cdp_url"]).rstrip("/"), pool)
    except (ValueError, KeyError, TypeError):
        raise RuntimeError("browser pool returned an unusable lease") from None
    lease_url = f"{pool}/leases/{quote(lease_id, safe='')}"
    lease: PoolLease | None = None
    parts = urlsplit(cdp_base)
    discovery = urlunsplit((parts.scheme, parts.netloc, parts.path.rstrip("/") + "/json/version", parts.query, ""))
    try:
        try:
            version = await http.get(discovery, headers=headers, timeout=30)
        except httpx.HTTPError as error:
            raise _pool_failure("browser discovery", error=error) from None
        if not version.is_success:
            raise _pool_failure("browser discovery", version.status_code)
        try:
            ws_url = on_pool_origin(str(version.json()["webSocketDebuggerUrl"]), pool)
        except (ValueError, KeyError, TypeError):
            raise RuntimeError("browser pool handed out a browser with no WebSocket address") from None
        if urlsplit(ws_url).scheme not in {"ws", "wss"}:
            raise RuntimeError("browser pool handed out an address that is not a WebSocket")
        lease = PoolLease(lease_id, ws_url, time.monotonic() - began - full_wait, full_wait)
        keeper = asyncio.create_task(_keep_lease(http, lease_url, headers, lease))
        try:
            yield lease
        finally:
            keeper.cancel()
            await asyncio.gather(keeper, return_exceptions=True)
    finally:
        closing = time.monotonic()
        try:
            await http.delete(lease_url, headers=headers, timeout=30)
        except httpx.HTTPError as error:
            _pool_log.warning("browser pool lease %s was not released (%s)", lease_id, type(error).__name__)
        if lease is not None:
            lease.released = round(time.monotonic() - closing, 2)


class FastbrowsePoolHarness(FastbrowseHarness):
    info = HarnessInfo(
        name="fastbrowse-pool",
        kind=HarnessKind.OPEN,
        capabilities=FastbrowseHarness.info.capabilities,
        pins_model=False,
        cost_source=CostSource.HARNESS,
        isolation=Isolation.HOST,
        credential_env=("OPENROUTER_API_KEY", "TYPESAFE_API_KEY", "EXA_API_KEY", "PARALLAX_POOL_TOKEN"),
        source_url="https://github.com/agent-labs-dev/fastbrowse",
    )

    @asynccontextmanager
    async def browser(self, http: httpx.AsyncClient) -> AsyncIterator[Browser]:
        async with pool_lease(http, f"{self.trial.task_id}-{uuid.uuid4().hex}") as lease:
            browser = Browser(cdp_url=lease.ws_url, open_seconds=lease.acquired)
            self._details.update({"pool": pool_host(), "pool_wait_seconds": round(lease.full_wait, 2)})
            try:
                yield browser
            finally:
                self._details.update({"pool_extend_failures": lease.extend_failures})
        self._details["pool_close_seconds"] = lease.released


__all__ = [
    "FastbrowsePoolHarness",
    "on_pool_origin",
    "pool_host",
    "pool_lease",
    "pool_settings",
]
