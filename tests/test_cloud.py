"""`fastbrowse.cloud`: a Browser Use Cloud browser started and stopped outside a run, against a fake API."""

import asyncio
import gc
import json
import threading
import time
from collections.abc import Callable, Iterator
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from fastbrowse import cloud
from fastbrowse.cloud import BrowserUseCloudBrowser, BrowserUseCloudError, BrowserUseCloudUnavailable
from fastbrowse.models import BrowserConnection, ConfigurationError, CostBasis, CostComponent, Unavailable

EXTENSION = "6f1c2d3e-4a5b-4c6d-8e7f-0a1b2c3d4e5f"
CREATED: dict[str, object] = {
    "id": "b1",
    "cdpUrl": "wss://cdp.test/devtools/browser/b1",
    "liveUrl": "https://live.test/b1",
    "browserCost": "0.01",
    "proxyCost": "0",
}
STOPPED: dict[str, object] = {"id": "b1", "browserCost": "0.02", "proxyCost": "0.003"}

type Reply = tuple[int, dict[str, str], dict[str, object]]
type Answer = Callable[[str, str], Reply]


@dataclass
class Request:
    method: str
    path: str
    key: str | None
    body: dict[str, object]


@dataclass
class Api:
    origin: str
    requests: list[Request] = field(default_factory=list)
    arrived: threading.Condition = field(default_factory=threading.Condition)

    async def received(self, count: int) -> None:
        """Wait until the API has had `count` requests."""

        def wait() -> bool:
            with self.arrived:
                return self.arrived.wait_for(lambda: len(self.requests) >= count, timeout=10)

        assert await asyncio.to_thread(wait), _calls(self)


def _default(method: str, _path: str) -> Reply:
    return 200, {}, CREATED if method == "POST" else STOPPED


@pytest.fixture
def api(monkeypatch: pytest.MonkeyPatch, request: pytest.FixtureRequest) -> Iterator[Api]:
    """The Browser Use API on localhost, answering with `request.param` or a browser that starts and stops."""
    answer: Answer = getattr(request, "param", _default)
    served = Api(origin="")

    class Handler(BaseHTTPRequestHandler):
        def _answer(self) -> None:
            length = int(self.headers.get("Content-Length") or 0)
            body = json.loads(self.rfile.read(length) or b"{}")
            with served.arrived:
                served.requests.append(
                    Request(self.command, self.path, self.headers.get("X-Browser-Use-API-Key"), body)
                )
                served.arrived.notify_all()
            status, headers, reply = answer(self.command, self.path)
            data = json.dumps(reply).encode()
            self.send_response(status)
            for name, value in headers.items():
                self.send_header(name, value)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        do_POST = do_PATCH = do_GET = _answer

        def log_message(self, format: str, *args: object) -> None:
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    served.origin = f"http://127.0.0.1:{server.server_address[1]}"
    monkeypatch.setattr(cloud, "_ORIGIN", served.origin)
    yield served
    server.shutdown()
    server.server_close()


def _calls(api: Api) -> list[str]:
    return [f"{r.method} {r.path}" for r in api.requests]


async def test_a_session_browser_starts_hands_over_its_connection_and_reports_its_cost(api: Api) -> None:
    browser = BrowserUseCloudBrowser(
        "key", profile="p1", extensions=[EXTENSION], timeout_minutes=60, proxy_country="DE", viewport=(1280, 800)
    )
    async with browser as started:
        assert started is browser
        assert browser.connection == BrowserConnection(
            cdp_url="wss://cdp.test/devtools/browser/b1", live_url="https://live.test/b1", browser_id="b1"
        )
    assert _calls(api) == ["POST /api/v4/browsers", "PATCH /api/v4/browsers/b1"]
    assert all(r.key == "key" for r in api.requests)
    assert api.requests[0].body == {
        "timeout": 60,
        "extensionIds": [EXTENSION],
        "proxyCountryCode": "de",
        "solveCaptchas": True,
        "profileId": "p1",
        "browserScreenWidth": 1280,
        "browserScreenHeight": 800,
    }
    assert api.requests[1].body == {"action": "stop"}
    assert [(line.component, line.basis, line.dollars) for line in browser.cost] == [
        (CostComponent.BROWSER, CostBasis.METERED, 0.02),
        (CostComponent.PROXY, CostBasis.UNSETTLED, 0.003),
    ]
    with pytest.raises(BrowserUseCloudError, match="not running"):
        _ = browser.connection


async def test_a_browser_outlives_the_call_that_started_it_until_stopped(api: Api) -> None:
    browser = BrowserUseCloudBrowser("key")
    connection = await browser.start()
    assert connection.cdp_url.startswith("wss://")
    assert browser.connection == connection
    assert _calls(api) == ["POST /api/v4/browsers"]
    with pytest.raises(BrowserUseCloudError, match="already started"):
        await browser.start()
    await browser.stop()
    await browser.stop()
    assert _calls(api) == ["POST /api/v4/browsers", "PATCH /api/v4/browsers/b1"]


async def test_the_key_comes_from_the_environment_as_a_runs_does(
    api: Api, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("BROWSER_USE_API_KEY", raising=False)
    with pytest.raises(ConfigurationError, match="BROWSER_USE_API_KEY"):
        BrowserUseCloudBrowser()
    monkeypatch.setenv("BROWSER_USE_API_KEY", "from-env")
    async with BrowserUseCloudBrowser():
        pass
    assert [r.key for r in api.requests] == ["from-env", "from-env"]


@pytest.mark.parametrize(
    ("options", "error"),
    [
        ({"extensions": ["not-a-uuid"]}, BrowserUseCloudError),
        ({"extensions": [EXTENSION, EXTENSION]}, BrowserUseCloudError),
        ({"proxy_country": "gb"}, ValueError),
        ({"viewport": (0, 600)}, ValueError),
    ],
)
async def test_options_are_refused_before_any_request(
    api: Api, options: dict[str, object], error: type[Exception]
) -> None:
    with pytest.raises(error):
        BrowserUseCloudBrowser("key", **options)  # ty: ignore[invalid-argument-type]
    assert api.requests == []


def _throttled(method: str, _path: str) -> Reply:
    return (429, {"Retry-After": "30"}, {}) if method == "POST" else (200, {}, STOPPED)


@pytest.mark.parametrize("api", [_throttled], indirect=True)
async def test_an_outage_is_unavailable_with_the_delay_the_service_asked_for(api: Api) -> None:
    with pytest.raises(BrowserUseCloudUnavailable) as caught:
        await BrowserUseCloudBrowser("key").start()
    assert isinstance(caught.value, Unavailable)
    assert caught.value.retry_after == 30
    assert "key" not in str(caught.value)


def _no_cdp(method: str, _path: str) -> Reply:
    return 200, {}, {"id": "b1"} if method == "POST" else STOPPED


@pytest.mark.parametrize("api", [_no_cdp], indirect=True)
async def test_a_start_that_fails_stops_the_browser_it_created(api: Api) -> None:
    browser = BrowserUseCloudBrowser("key")
    with pytest.raises(BrowserUseCloudError, match="without a CDP URL"):
        async with browser:
            pass
    assert _calls(api) == ["POST /api/v4/browsers", "PATCH /api/v4/browsers/b1"]
    assert [line.dollars for line in browser.cost] == [0.02, 0.003]


def _slow_create(method: str, path: str) -> Reply:
    if method == "POST":
        time.sleep(0.5)
    return _default(method, path)


@pytest.mark.parametrize("api", [_slow_create], indirect=True)
async def test_a_start_cancelled_mid_create_still_stops_its_browser(api: Api) -> None:
    browser = BrowserUseCloudBrowser("key")
    starting = asyncio.create_task(browser.start())
    await api.received(1)
    starting.cancel()
    with pytest.raises(asyncio.CancelledError):
        await starting
    assert _calls(api) == ["POST /api/v4/browsers", "PATCH /api/v4/browsers/b1"]
    with pytest.raises(BrowserUseCloudError, match="not running"):
        _ = browser.connection


async def test_a_browser_dropped_while_running_is_stopped_anyway(api: Api) -> None:
    browser = BrowserUseCloudBrowser("key")
    await browser.start()
    del browser
    gc.collect()
    await api.received(2)
    assert _calls(api) == ["POST /api/v4/browsers", "PATCH /api/v4/browsers/b1"]
