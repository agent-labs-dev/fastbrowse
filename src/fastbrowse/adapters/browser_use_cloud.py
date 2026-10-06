"""A remote Chrome from Browser Use Cloud, with its metered browser and proxy cost.

async with BrowserUseCloudBrowser(api_key, http=http) as cloud:
    async with BrowserSession(cloud.connection, sink) as session: ...
cloud.cost  # metered lines, available after exit
"""

import asyncio
from collections.abc import Sequence
from contextlib import suppress
from decimal import Decimal
from types import TracebackType
from typing import Self
from uuid import UUID

import httpx
from pydantic import BaseModel, ConfigDict, Field, JsonValue

from fastbrowse.clients.validation import RETRYABLE_STATUS, TRANSIENT_TRANSPORT
from fastbrowse.models import BrowserConnection, CostBasis, CostComponent, CostLine, Unavailable

API_V3 = "https://api.browser-use.com/api/v3"
API_V4 = "https://api.browser-use.com/api/v4"
# https://docs.browser-use.com/cloud/api-v4/browsers/create-browser-session: "up to 3 ready extensions".
MAX_EXTENSIONS = 3


class _BrowserView(BaseModel):
    model_config = ConfigDict(extra="ignore", populate_by_name=True)

    id: str
    cdp_url: str | None = Field(default=None, alias="cdpUrl")
    live_url: str | None = Field(default=None, alias="liveUrl")
    browser_cost: Decimal = Field(default=Decimal(0), alias="browserCost")
    proxy_cost: Decimal = Field(default=Decimal(0), alias="proxyCost")


class BrowserUseCloudError(RuntimeError):
    pass


class BrowserUseCloudUnavailable(BrowserUseCloudError, Unavailable):
    """Browser Use Cloud could not be reached, or answered with a retryable status."""


def _extension_ids(extensions: Sequence[str]) -> list[str]:
    try:
        ids = [str(UUID(extension)) for extension in extensions]
    except ValueError:
        raise BrowserUseCloudError("cloud_extensions takes extension IDs, which are UUIDs") from None
    if len(set(ids)) != len(ids):
        raise BrowserUseCloudError("cloud_extensions names the same extension twice")
    if len(ids) > MAX_EXTENSIONS:
        raise BrowserUseCloudError(f"a cloud browser takes at most {MAX_EXTENSIONS} extensions")
    return ids


class BrowserUseCloudBrowser:
    def __init__(
        self,
        api_key: str,
        *,
        http: httpx.AsyncClient,
        proxy_country: str | None = "us",
        timeout_minutes: int = 15,
        profile: str | None = None,
        viewport: tuple[int, int] | None = None,
        allow_resizing: bool = False,
        extensions: Sequence[str] = (),
    ) -> None:
        self._http = http
        self._headers = {"X-Browser-Use-API-Key": api_key}
        # Only the V4 create call takes extensions, so a run that asks for none stays on the V3 path it was built on.
        self._api = API_V4 if extensions else API_V3
        self._body: dict[str, str | int | list[str]] = {"timeout": timeout_minutes}
        if extensions:
            self._body["extensionIds"] = _extension_ids(extensions)
        if allow_resizing:
            self._body["allowResizing"] = True
        if proxy_country is not None:
            self._body["proxyCountryCode"] = proxy_country
        # A cloud profile is the remote counterpart of `LocalChrome.profile`: the browser starts with the
        # cookies the profile already holds, so a site signed into once stays signed in. Whoever runs the
        # task never sees those cookies, which is the point of naming a profile rather than typing a secret.
        if profile is not None:
            self._body["profileId"] = profile
        # Headless defaults are small enough that a responsive site collapses its header into a toggle and
        # the control the run needs is not in the page at all, which is why local Chrome sets a size too.
        if viewport is not None:
            self._body["browserScreenWidth"], self._body["browserScreenHeight"] = viewport
        self._browser_id: str | None = None
        self._connection: BrowserConnection | None = None
        self.cost: tuple[CostLine, ...] = ()

    @property
    def connection(self) -> BrowserConnection:
        if self._connection is None:
            raise BrowserUseCloudError("the cloud browser is not running")
        return self._connection

    async def __aenter__(self) -> Self:
        creation = asyncio.create_task(self._create())
        try:
            # A cancelled POST can still create a billable browser; wait until its id is known.
            browser = await asyncio.shield(creation)
            if browser.cdp_url is None:
                raise BrowserUseCloudError(f"browser {browser.id} started without a CDP URL")
            try:
                version = await self._http.get(f"{browser.cdp_url}/json/version")
            except TRANSIENT_TRANSPORT as error:
                raise BrowserUseCloudUnavailable(
                    f"browser {browser.id} did not answer ({type(error).__name__})"
                ) from None
            if version.status_code in RETRYABLE_STATUS:
                raise BrowserUseCloudUnavailable(f"browser {browser.id} answered HTTP {version.status_code}")
            version.raise_for_status()
            ws_url = str(version.json()["webSocketDebuggerUrl"])
            self._connection = BrowserConnection(
                cdp_url=ws_url, live_url=browser.live_url, browser_id=browser.id, remote=True
            )
        except BaseException:
            await asyncio.gather(creation, return_exceptions=True)
            with suppress(BaseException):
                await self._finish()
            raise
        return self

    async def _create(self) -> _BrowserView:
        created = await self._call("POST", "/browsers", json=self._body)
        raw: JsonValue = created.json()
        # Validation of optional fields must not lose the id needed for rollback.
        if isinstance(raw, dict) and isinstance(browser_id := raw.get("id"), str):
            self._browser_id = browser_id
        browser = _BrowserView.model_validate(raw)
        self._record_cost(browser)
        return browser

    async def __aexit__(
        self, exc_type: type[BaseException] | None, exc: BaseException | None, tb: TracebackType | None
    ) -> None:
        try:
            await self._finish()
        except BaseException:
            if exc is None:
                raise

    async def _finish(self) -> None:
        stopping = asyncio.create_task(self._stop())
        try:
            await asyncio.shield(stopping)
        finally:
            # The remote browser keeps billing until this request finishes, even after caller cancellation.
            await asyncio.gather(stopping, return_exceptions=True)

    async def _stop(self) -> None:
        """Stop the browser (it bills until stopped or timed out) and record what it cost."""
        if self._browser_id is None:
            return
        self._connection = None
        # Forget the browser only once the stop succeeded, so a failed stop can be retried rather than left billing.
        response = await self._call("PATCH", f"/browsers/{self._browser_id}", json={"action": "stop"})
        self._browser_id = None
        stopped = _BrowserView.model_validate_json(response.content)
        self._record_cost(stopped)

    def _record_cost(self, browser: _BrowserView) -> None:
        self.cost = (
            CostLine(component=CostComponent.BROWSER, basis=CostBasis.METERED, dollars=float(browser.browser_cost)),
            CostLine(component=CostComponent.PROXY, basis=CostBasis.METERED, dollars=float(browser.proxy_cost)),
        )

    async def _call(self, method: str, path: str, *, json: dict[str, str | int | list[str]]) -> httpx.Response:
        try:
            response = await self._http.request(method, f"{self._api}{path}", headers=self._headers, json=json)
        except TRANSIENT_TRANSPORT:
            # The request carries the API key header; never let the transport error's request escape.
            raise BrowserUseCloudUnavailable(f"Browser Use Cloud {method} {path} failed") from None
        except httpx.HTTPError:
            raise BrowserUseCloudError(f"Browser Use Cloud {method} {path} failed") from None
        if response.status_code in RETRYABLE_STATUS:
            raise BrowserUseCloudUnavailable(f"Browser Use Cloud {method} {path}: HTTP {response.status_code}")
        if not response.is_success:
            raise BrowserUseCloudError(f"Browser Use Cloud {method} {path}: HTTP {response.status_code}")
        return response
