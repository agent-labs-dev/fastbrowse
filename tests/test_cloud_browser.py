"""`open_cloud_browser` and `stop_cloud_browser` on the Rust core, against a stand-in for Browser Use Cloud."""

import asyncio
import json
import threading
import time
from collections.abc import Callable, Iterator
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

import pytest

import fastbrowse
from fastbrowse import CloudBrowser, CloudBrowserError, CloudBrowserUnavailable, native

EXTENSION = "6f1c2d3e-4a5b-4c6d-8e7f-0a1b2c3d4e5f"

type Reply = tuple[int, dict[str, Any] | None]


@dataclass
class Cloud:
    """Answers each request with `answer(method, path)` and records what it was sent."""

    origin: str
    answer: Callable[[str, str], Reply]
    requests: list[tuple[str, str, dict[str, Any] | None, str | None]] = field(default_factory=list)

    def calls(self) -> list[str]:
        return [f"{method} {path}" for method, path, _, _ in self.requests]

    def wait_for(self, call: str) -> None:
        for _ in range(100):
            if call in self.calls():
                return
            time.sleep(0.02)
        raise AssertionError(f"{call} never arrived: {self.calls()}")


def created(cloud: Cloud, cdp_url: str | None = None) -> dict[str, Any]:
    return {
        "id": "kept",
        "cdpUrl": cdp_url or cloud.origin,
        "liveUrl": "https://live.test/kept",
        "webSocketDebuggerUrl": "ws://cdp.test/devtools/browser/kept",
    }


@pytest.fixture
def cloud() -> Iterator[Cloud]:
    holder: list[Cloud] = []

    class Handler(BaseHTTPRequestHandler):
        def _serve(self) -> None:
            length = int(self.headers.get("Content-Length") or 0)
            body = json.loads(self.rfile.read(length)) if length else None
            server = holder[0]
            server.requests.append((self.command, self.path, body, self.headers.get("X-Browser-Use-API-Key")))
            status, reply = server.answer(self.command, self.path)
            payload = b"" if reply is None else json.dumps(reply).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        do_GET = do_POST = do_PATCH = _serve

        def log_message(self, format: str, *args: Any) -> None:
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    fake = Cloud(f"http://127.0.0.1:{server.server_port}", lambda method, path: (500, None))
    fake.answer = lambda method, path: (200, created(fake))
    holder.append(fake)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield fake
    server.shutdown()


def arguments(**changes: Any) -> dict[str, Any]:
    defaults = {
        "browser_api_key": "key",
        "profile": None,
        "extensions": (),
        "timeout_minutes": 15,
        "proxy_country": "us",
        "viewport": None,
    }
    return defaults | changes


def test_the_kept_browser_api_is_public() -> None:
    for name in ("open_cloud_browser", "stop_cloud_browser", "CloudBrowser", "CloudBrowserError"):
        assert name in fastbrowse.__all__


async def test_a_kept_browser_is_started_as_a_run_starts_one_and_left_running(cloud: Cloud) -> None:
    options = arguments(profile="profile-1", timeout_minutes=120, viewport=(1280, 800))
    browser = await native.open_cloud_browser(options, origin=cloud.origin)
    assert browser == CloudBrowser(
        id="kept", cdp_url="ws://cdp.test/devtools/browser/kept", live_url="https://live.test/kept"
    )
    await asyncio.sleep(0.2)
    assert cloud.calls() == ["POST /api/v4/browsers", "GET /json/version"]
    _, _, body, key = cloud.requests[0]
    assert key == "key"
    assert body == {
        "timeout": 120,
        "proxyCountryCode": "us",
        "solveCaptchas": True,
        "profileId": "profile-1",
        "browserScreenWidth": 1280,
        "browserScreenHeight": 800,
    }
    await native.stop_cloud_browser(browser, "key", origin=cloud.origin)
    assert cloud.calls()[-1] == "PATCH /api/v4/browsers/kept"
    assert cloud.requests[-1][2] == {"action": "stop"}


async def test_a_browser_with_extensions_is_created_and_stopped_by_its_stored_fields(cloud: Cloud) -> None:
    cloud.answer = lambda method, path: (200, created(cloud, "wss://cdp.test/browser/kept"))
    browser = await native.open_cloud_browser(arguments(extensions=[EXTENSION]), origin=cloud.origin)
    assert browser.cdp_url == "wss://cdp.test/browser/kept"
    assert cloud.requests[0][2] == {
        "timeout": 15,
        "extensionIds": [EXTENSION],
        "proxyCountryCode": "us",
        "solveCaptchas": True,
        "browserScreenWidth": 1120,
        "browserScreenHeight": 780,
    }
    stored = CloudBrowser.model_validate(json.loads(browser.model_dump_json()))
    await native.stop_cloud_browser(stored, "key", origin=cloud.origin)
    assert cloud.calls() == ["POST /api/v4/browsers", "PATCH /api/v4/browsers/kept"]


async def test_a_failed_start_stops_the_browser_it_created(cloud: Cloud) -> None:
    cloud.answer = lambda method, path: (404, None) if method == "GET" else (200, created(cloud))
    with pytest.raises(CloudBrowserError, match="answered HTTP 404") as raised:
        await native.open_cloud_browser(arguments(), origin=cloud.origin)
    assert not isinstance(raised.value, CloudBrowserUnavailable)
    assert cloud.calls() == ["POST /api/v4/browsers", "GET /json/version", "PATCH /api/v4/browsers/kept"]


async def test_a_cancelled_start_stops_the_browser_it_created(cloud: Cloud) -> None:
    def slow(method: str, path: str) -> Reply:
        if method == "POST":
            time.sleep(0.3)
        return 200, created(cloud)

    cloud.answer = slow
    opening = asyncio.create_task(native.open_cloud_browser(arguments(), origin=cloud.origin))
    await asyncio.sleep(0.05)
    opening.cancel()
    with pytest.raises(asyncio.CancelledError):
        await opening
    await asyncio.to_thread(cloud.wait_for, "PATCH /api/v4/browsers/kept")


async def test_an_unreachable_service_is_unavailable(cloud: Cloud) -> None:
    cloud.answer = lambda method, path: (503, None)
    with pytest.raises(CloudBrowserUnavailable):
        await native.open_cloud_browser(arguments(), origin=cloud.origin)


async def test_bad_options_are_refused_before_any_request(cloud: Cloud) -> None:
    with pytest.raises(CloudBrowserError, match="UUIDs"):
        await native.open_cloud_browser(arguments(extensions=["not-a-uuid"]), origin=cloud.origin)
    with pytest.raises(ValueError, match="timeout_minutes"):
        await native.open_cloud_browser(arguments(timeout_minutes=-1), origin=cloud.origin)
    assert cloud.requests == []


async def test_a_stop_the_service_refuses_raises(cloud: Cloud) -> None:
    cloud.answer = lambda method, path: (404, None)
    browser = CloudBrowser(id="gone", cdp_url="ws://x")
    with pytest.raises(CloudBrowserError, match="HTTP 404"):
        await native.stop_cloud_browser(browser, "key", origin=cloud.origin)
