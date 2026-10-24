"""`fastbrowse.native`: `run_task` on the Rust core in this process, when FASTBROWSE_RUST_CORE=1.

The conversion is checked here without the module. The runs need `fastbrowse_native` built with `run_task`
(`just rust` installs it) and skip without it.
"""

import asyncio
import base64
import importlib
from pathlib import Path
from typing import Any

import pytest
from pydantic import BaseModel

from fastbrowse import native, run_task
from fastbrowse.clients.environment import ConfigurationError
from fastbrowse.config import Config
from fastbrowse.models import Attachment, BrowserEvent, LocalChrome, SecretRef, StepEvent
from fastbrowse.safety import ScopedSecrets

try:
    fastbrowse_native = importlib.import_module("fastbrowse_native")

    NATIVE = hasattr(fastbrowse_native, "run_task")
except ImportError:
    NATIVE = False
needs_native = pytest.mark.skipif(not NATIVE, reason="fastbrowse_native with run_task is not installed")


class Release(BaseModel):
    package: str
    version: str


def arguments(**changes: Any) -> dict[str, Any]:
    """`run_task`'s arguments at their defaults, as `locals()` holds them at its first line."""
    defaults: dict[str, Any] = {
        "start": None,
        "browser_api_key": None,
        "chrome": None,
        "cloud_profile": None,
        "cdp_url": None,
        "cdp_port": None,
        "attach": False,
        "target_match": None,
        "foreground": False,
        "allowed_origins": None,
        "check_access": None,
        "proxy_country": "us",
        "viewport": None,
        "cloud_allow_resizing": False,
        "cloud_extensions": (),
        "cloud_timeout_minutes": 15,
        "jev": None,
        "llm": None,
        "output_schema": None,
        "inputs": None,
        "attachments": (),
        "limits": None,
        "authorization": None,
        "secrets": None,
        "downloads": None,
        "on_event": None,
        "on_frame": None,
        "until": None,
        "config": None,
        "http": None,
        "record": None,
        "cursor": False,
    }
    return defaults | changes


async def _until(_: str) -> bool:
    return True


def test_arguments_become_the_run_params_serve_reads() -> None:
    secrets = ScopedSecrets({"password": "hunter2"}, "https://shop.example")
    params = native.params(
        "Find the release",
        arguments(
            start="https://pypi.org/",
            chrome=LocalChrome(headed=True),
            output_schema=Release,
            inputs={"name": "httpx"},
            attachments=(Attachment(name="a.txt", mime_type="text/plain", content=b"hello"),),
            secrets=secrets,
            until=_until,
            downloads=Path("/tmp/downloads"),
        ),
    )
    assert params is not None
    assert params.task == "Find the release"
    assert params.local is True
    assert params.chrome == LocalChrome(headed=True)
    assert params.output_schema == Release.model_json_schema()
    assert params.attachments[0].content == b"hello"
    assert params.secrets == (SecretRef(name="password", origins=("https://shop.example",)),)
    assert (params.until, params.frames) == (True, False)
    assert "hunter2" not in params.model_dump_json()
    assert base64.b64decode(params.model_dump(mode="json")["attachments"][0]["content"]) == b"hello"


def test_a_handed_over_browser_is_not_local() -> None:
    params = native.params("t", arguments(cdp_url="ws://127.0.0.1:9222/devtools/browser/x", attach=True))
    assert params is not None
    assert (params.local, params.cdp_url, params.attach) == (False, "ws://127.0.0.1:9222/devtools/browser/x", True)


@pytest.mark.parametrize(
    "changes",
    [
        {"browser_api_key": "bu-key"},
        {"allowed_origins": ["https://a.example"]},
        {"cloud_extensions": ["ext"]},
        {"cloud_timeout_minutes": 60},
        {"jev": object()},
        {"llm": object()},
        {"config": Config(step_frames=True)},
    ],
)
def test_an_argument_the_protocol_cannot_carry_keeps_the_run_on_python(changes: dict[str, Any]) -> None:
    assert native.params("t", arguments(**changes)) is None


def test_a_default_config_still_crosses() -> None:
    assert native.params("t", arguments(config=Config())) is not None


@pytest.fixture
def rust_core(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setenv("FASTBROWSE_RUST_CORE", "1")
    # A checkout's `.env` would hand the run a real key.
    monkeypatch.chdir(tmp_path)
    for name in ("OPENROUTER_API_KEY", "AI_GATEWAY_API_KEY", "TYPESAFE_API_KEY", "BROWSER_USE_API_KEY"):
        monkeypatch.delenv(name, raising=False)


@needs_native
@pytest.mark.usefixtures("rust_core")
async def test_a_missing_key_raises_configuration_error() -> None:
    with pytest.raises(ConfigurationError, match="OPENROUTER_API_KEY"):
        await run_task("t", cdp_url="ws://127.0.0.1:9/devtools/browser/x")


class Silent:
    """A DevTools address that accepts the connection and never answers, so a run waits there until stopped."""

    def __init__(self) -> None:
        self.closed = asyncio.Event()
        self.connected = asyncio.Event()

    async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        self.connected.set()
        await reader.read()
        self.closed.set()
        writer.close()


@needs_native
@pytest.mark.usefixtures("rust_core")
async def test_cancelling_the_awaiting_task_stops_the_rust_run_and_closes_its_connection(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("OPENROUTER_API_KEY", "unused")
    silent = Silent()
    server = await asyncio.start_server(silent.handle, "127.0.0.1", 0)
    port = server.sockets[0].getsockname()[1]
    events: list[StepEvent | BrowserEvent] = []

    async def on_event(event: StepEvent | BrowserEvent) -> None:
        events.append(event)

    async with server:
        run = asyncio.create_task(run_task("t", cdp_url=f"ws://127.0.0.1:{port}/devtools/browser/x", on_event=on_event))
        await asyncio.wait_for(silent.connected.wait(), 30)
        run.cancel()
        with pytest.raises(asyncio.CancelledError):
            await run
        await asyncio.wait_for(silent.closed.wait(), 10)
    assert [event.type for event in events] == ["browser"]


@needs_native
@pytest.mark.usefixtures("rust_core")
async def test_an_exception_a_callback_raises_comes_out_of_run_task(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("OPENROUTER_API_KEY", "unused")

    class Refused(Exception):
        pass

    async def on_event(_: StepEvent | BrowserEvent) -> None:
        raise Refused("not this browser")

    with pytest.raises(Refused, match="not this browser"):
        await run_task("t", cdp_url="ws://127.0.0.1:9/devtools/browser/x", on_event=on_event)
