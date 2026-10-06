"""`run` over the serve protocol, as a client sees it, with `run_task` replaced by a recorder.

No browser opens and no model is called: what is asserted is what the recorder was handed, and every message
the server wrote.
"""

import asyncio
import json
import os
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path
from typing import Any

import pytest
from pydantic import SecretStr

from fastbrowse.clients.environment import ConfigurationError, Settings
from fastbrowse.models import (
    Attachment,
    Authorization,
    BrowserEvent,
    CostBreakdown,
    Decider,
    Limits,
    LocalChrome,
    Operation,
    RunResult,
    Status,
    StepEvent,
    StepOutcome,
    StepResult,
)
from tests.serve_client import serving

START = "https://shop.example.com/cart"
REPO = Path(__file__).resolve().parent.parent


def _result(status: Status = Status.COMPLETE, **fields: Any) -> RunResult:
    shape: dict[str, Any] = {
        "status": status,
        "answer": "42",
        "data": None,
        "evidence": (),
        "steps": (),
        "cost": CostBreakdown(lines=()),
        "artifacts": (),
    }
    return RunResult(**(shape | fields))


def _step(index: int) -> StepResult:
    return StepResult(
        index=index,
        operation=Operation.CLICK,
        decided_by=Decider.JEV,
        outcome=StepOutcome.EXECUTED,
        url=START,
        target="Add to cart",
        duration_ms=5,
    )


class Recorder:
    """Stands where `run_task` does, and keeps what it was called with."""

    def __init__(
        self,
        result: RunResult | None = None,
        *,
        events: Sequence[StepEvent | BrowserEvent] = (),
        hold: asyncio.Event | None = None,
    ) -> None:
        self.calls: list[dict[str, Any]] = []
        self._result = result or _result()
        self._events = events
        self._hold = hold

    async def __call__(self, task: str, **arguments: Any) -> RunResult:
        self.calls.append({"task": task, **arguments})
        for event in self._events:
            await arguments["on_event"](event)
        if self._hold is not None:
            await self._hold.wait()
        return self._result


def _settings(**overrides: Any) -> Settings:
    """Every key set and nothing read from a checkout's `.env` or the environment of whoever runs the tests."""
    fields: dict[str, Any] = {
        "openrouter_api_key": SecretStr("or"),
        "ai_gateway_api_key": SecretStr("gw"),
        "typesafe_api_key": None,
        "browser_use_api_key": SecretStr("bu"),
        "profile": None,
        "headed": False,
        # Any binary that exists stands in for Chrome, which the server only looks for.
        "chrome": sys.executable,
    }
    return Settings.model_construct(**(fields | overrides))


def _run(run_id: str = "run-1", **fields: Any) -> dict[str, Any]:
    return {"run_id": run_id, "task": "Add the kettle to the cart"} | fields


async def _received(**fields: Any) -> dict[str, Any]:
    """What `run_task` is called with for a `run` carrying these fields."""
    recorder = Recorder()
    async with serving(runner=recorder, settings=_settings) as client:
        await client.result("run", _run(**fields))
    (call,) = recorder.calls
    return call


@pytest.fixture
def ffmpeg(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A PATH whose only program is something named ffmpeg, which is all a recording is checked for."""
    for name in ("ffmpeg", "ffmpeg.exe"):
        (tmp_path / name).write_text("")
        (tmp_path / name).chmod(0o755)
    monkeypatch.setenv("PATH", str(tmp_path))


async def test_run_calls_run_task_and_replies_with_its_result() -> None:
    recorder = Recorder(_result(answer="The latest version is 0.28.1", final_url="https://pypi.org/project/httpx/"))

    async with serving(runner=recorder, settings=_settings) as client:
        response = await client.request("run", {"run_id": "run-1", "task": "What is the latest version of httpx?"})

    assert response["id"] == 1
    assert response["result"]["status"] == "complete"
    assert response["result"]["answer"] == "The latest version is 0.28.1"
    assert response["result"]["final_url"] == "https://pypi.org/project/httpx/"
    assert [call["task"] for call in recorder.calls] == ["What is the latest version of httpx?"]


async def test_the_fields_that_describe_the_task_arrive_as_run_tasks_arguments(tmp_path: Path, ffmpeg: None) -> None:
    call = await _received(
        start=START,
        inputs={"email": "ada@example.com"},
        # Bytes that are not text, and whose base64 differs between the standard and the URL-safe alphabet.
        attachments=[{"name": "receipt.pdf", "mime_type": "application/pdf", "content": "+//+"}],
        limits={"max_steps": 12, "max_dollars": 0.5, "max_seconds": 90},
        authorization={"irreversible_actions": True},
        downloads=str(tmp_path / "downloads"),
        record=str(tmp_path / "run.mp4"),
        viewport=[1280, 720],
        cloud_allow_resizing=True,
    )

    assert call["task"] == "Add the kettle to the cart"
    assert call["start"] == START
    assert call["inputs"] == {"email": "ada@example.com"}
    assert call["attachments"] == (
        Attachment(name="receipt.pdf", mime_type="application/pdf", content=b"\xfb\xff\xfe"),
    )
    assert call["limits"] == Limits(max_steps=12, max_dollars=0.5, max_seconds=90)
    assert call["authorization"] == Authorization(irreversible_actions=True)
    assert call["downloads"] == tmp_path / "downloads"
    assert call["record"] == tmp_path / "run.mp4"
    assert call["viewport"] == (1280, 720)
    assert call["cloud_allow_resizing"] is True


async def test_fields_left_out_leave_run_task_its_own_defaults() -> None:
    call = await _received()

    assert call["start"] is None and call["inputs"] is None and call["attachments"] == ()
    # None is what `run_task` reads as "the default limits" and "nothing authorized".
    assert call["limits"] is None and call["authorization"] is None
    assert call["downloads"] is None and call["record"] is None and call["viewport"] is None
    assert call["cloud_allow_resizing"] is False and call["attach"] is False


async def test_the_browser_is_the_cloud_one_unless_the_run_asks_for_another() -> None:
    call = await _received()

    assert call["browser_api_key"] == "bu"
    assert call["cdp_url"] is None and call["cdp_port"] is None
    # The CLI's default country, since an unset one would browse from wherever the cloud put the browser.
    assert call["proxy_country"] == "us"


async def test_cloud_options_reach_the_cloud_browser() -> None:
    call = await _received(cloud_profile="prof_1", proxy_country="UK")

    assert call["browser_api_key"] == "bu"
    assert call["cloud_profile"] == "prof_1"
    assert call["proxy_country"] == "uk"


@pytest.mark.parametrize(
    ("fields", "chrome"),
    [
        ({"local": True}, LocalChrome(binary=sys.executable)),
        ({"chrome": {"headed": True}}, LocalChrome(binary=sys.executable, headed=True)),
        ({"chrome": {"profile": "/tmp/kept"}}, LocalChrome(binary=sys.executable, profile=Path("/tmp/kept"))),
    ],
    ids=["local", "a headed window", "a kept profile"],
)
async def test_local_chrome_runs_without_the_cloud_key(fields: dict[str, Any], chrome: LocalChrome) -> None:
    # A headed window and a kept profile are things only local Chrome has, so each implies it, as in the CLI.
    call = await _received(**fields)

    assert call["browser_api_key"] is None
    assert call["chrome"] == chrome


async def test_local_chrome_needs_no_cloud_key_in_the_environment() -> None:
    recorder = Recorder()
    async with serving(runner=recorder, settings=lambda: _settings(browser_use_api_key=None)) as client:
        result = await client.result("run", _run(local=True))

    assert result["status"] == "complete"


async def test_headed_and_profile_from_the_environment_count_as_they_do_for_the_cli() -> None:
    recorder = Recorder()
    settings = _settings(headed=True, profile=Path("/tmp/from-env"))
    async with serving(runner=recorder, settings=lambda: settings) as client:
        await client.result("run", _run())

    (call,) = recorder.calls
    assert call["browser_api_key"] is None
    assert call["chrome"] == LocalChrome(binary=sys.executable, headed=True, profile=Path("/tmp/from-env"))


async def test_a_chrome_binary_named_by_the_run_replaces_the_one_in_the_environment() -> None:
    recorder = Recorder()
    async with serving(runner=recorder, settings=lambda: _settings(chrome="not-installed-anywhere")) as client:
        await client.result("run", _run(local=True, chrome={"binary": sys.executable}))

    assert recorder.calls[0]["chrome"].binary == sys.executable


@pytest.mark.parametrize(
    ("fields", "url", "port"),
    [
        ({"cdp_url": "ws://127.0.0.1:9222/devtools/browser/abc"}, "ws://127.0.0.1:9222/devtools/browser/abc", None),
        ({"cdp_port": 9222, "attach": True, "target_match": "Inbox"}, None, 9222),
    ],
    ids=["by url", "by port, attaching to a window"],
)
async def test_a_browser_handed_over_is_driven_without_starting_one(
    fields: dict[str, Any], url: str | None, port: int | None
) -> None:
    call = await _received(**fields)

    assert call["cdp_url"] == url and call["cdp_port"] == port
    assert call["browser_api_key"] is None
    assert call["attach"] is fields.get("attach", False)
    assert call["target_match"] == fields.get("target_match")


async def test_events_arrive_in_order_with_the_run_id_and_before_the_reply() -> None:
    events = [
        BrowserEvent(live_url="https://live.example.com/b1", browser_id="b1"),
        StepEvent(step=_step(1)),
        StepEvent(step=_step(2), frame=b"\xfb\xff\xfe"),
    ]
    async with serving(runner=Recorder(events=events), settings=_settings) as client:
        request_id = await client.call("run", _run("run-7"))
        messages = [await client.receive() for _ in range(4)]

    assert [message.get("method") for message in messages] == ["run/event", "run/event", "run/event", None]
    assert all("id" not in message for message in messages[:3])
    assert [message["params"]["run_id"] for message in messages[:3]] == ["run-7", "run-7", "run-7"]
    browser, first, second = (message["params"]["event"] for message in messages[:3])
    assert browser == {"type": "browser", "live_url": "https://live.example.com/b1", "browser_id": "b1"}
    assert (first["type"], first["step"]["index"], first["frame"]) == ("step", 1, None)
    assert (second["step"]["index"], second["step"]["target"]) == (2, "Add to cart")
    # Base64 in the standard alphabet. The URL-safe one would have written "-__-".
    assert second["frame"] == "+//+"
    assert messages[3]["id"] == request_id and messages[3]["result"]["status"] == "complete"


@pytest.mark.parametrize("status", list(Status))
async def test_a_run_that_ends_in_any_status_replies_with_its_result(status: Status) -> None:
    ended = _result(status, answer=None, error="what stopped it", final_frame=b"\x89PNG\xff")

    async with serving(runner=Recorder(ended), settings=_settings) as client:
        response = await client.request("run", _run())

    assert "error" not in response
    assert response["result"]["status"] == status.value
    assert response["result"]["error"] == "what stopped it"
    assert "final_frame" not in response["result"]


@pytest.mark.parametrize(
    ("params", "named"),
    [
        ({"run_id": "run-1"}, "task"),
        ({"task": "t"}, "run_id"),
        (_run(headless=True), "headless"),
        (_run(limits={"max_steps": 0}), "limits.max_steps"),
        (_run(chrome={"headed": True, "devtools": True}), "chrome.devtools"),
        (
            _run(attachments=[{"name": "a", "mime_type": "text/plain", "content": "not base64!"}]),
            "attachments.0.content",
        ),
        (_run(viewport=[1280]), "viewport"),
        (_run(proxy_country="gb"), "proxy_country"),
    ],
    ids=[
        "no task",
        "no run id",
        "an unknown field",
        "a limit of zero",
        "an unknown chrome field",
        "content that is not base64",
        "half a viewport",
        "a country the cloud does not take",
    ],
)
async def test_invalid_params_are_refused_before_run_task_is_called(params: dict[str, Any], named: str) -> None:
    recorder = Recorder()
    async with serving(runner=recorder, settings=_settings) as client:
        response = await client.request("run", params)

    assert response["error"]["code"] == -32602
    assert named in response["error"]["message"]
    assert recorder.calls == []
    assert client.inbox == []


@pytest.mark.parametrize(
    ("settings", "fields", "named"),
    [
        ({"openrouter_api_key": None}, {}, "OPENROUTER_API_KEY"),
        ({"openrouter_api_key": None}, {"local": True}, "OPENROUTER_API_KEY"),
        ({"browser_use_api_key": None}, {}, "BROWSER_USE_API_KEY"),
        ({"chrome": "not-installed-anywhere"}, {"local": True}, "Chrome was not found"),
        ({}, {"local": True, "cloud_profile": "prof_1"}, "--cloud-profile"),
        ({}, {"local": True, "proxy_country": "uk"}, "--proxy-country"),
        ({}, {"cdp_url": "ws://b.test/devtools", "cdp_port": 9222}, "--cdp-port"),
        ({}, {"cdp_url": "http://b.test/devtools"}, "ws://"),
        ({}, {"cdp_port": 70000}, "65535"),
        ({}, {"attach": True}, "--attach"),
        ({}, {"cdp_port": 9222, "chrome": {"headed": True}}, "--headed"),
        ({"profile": Path("/tmp/from-env")}, {"cdp_port": 9222}, "--profile"),
    ],
    ids=[
        "no model key",
        "no model key on local Chrome",
        "no cloud browser key",
        "no Chrome to run locally",
        "a cloud profile on local Chrome",
        "a country on local Chrome",
        "two browsers handed over",
        "a cdp url that is not a websocket",
        "a port out of range",
        "attach with nothing to attach to",
        "a handed-over browser with a headed one",
        "a handed-over browser with a profile from the environment",
    ],
)
async def test_a_configuration_error_is_refused_before_run_task_is_called(
    settings: dict[str, Any], fields: dict[str, Any], named: str
) -> None:
    recorder = Recorder()
    async with serving(runner=recorder, settings=lambda: _settings(**settings)) as client:
        response = await client.request("run", _run(**fields))

    assert response["error"]["code"] == -32003
    assert named in response["error"]["message"]
    assert recorder.calls == []
    assert client.inbox == []


async def test_a_recording_without_ffmpeg_is_a_configuration_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("PATH", str(tmp_path))
    recorder = Recorder()
    async with serving(runner=recorder, settings=_settings) as client:
        response = await client.request("run", _run(record=str(tmp_path / "run.mp4")))

    assert response["error"]["code"] == -32003
    assert "ffmpeg" in response["error"]["message"]
    assert recorder.calls == []


async def test_a_configuration_error_from_the_run_itself_gets_the_same_code() -> None:
    async def unconfigured(task: str, **_: Any) -> RunResult:
        raise ConfigurationError("invalid settings: FASTBROWSE_LLM_REASONING")

    async with serving(runner=unconfigured, settings=_settings) as client:
        response = await client.request("run", _run())

    assert response["error"] == {"code": -32003, "message": "invalid settings: FASTBROWSE_LLM_REASONING"}


async def test_a_second_run_while_one_is_active_is_busy_and_does_not_disturb_the_first() -> None:
    hold = asyncio.Event()
    recorder = Recorder(events=[BrowserEvent(live_url=None)], hold=hold)

    async with serving(runner=recorder, settings=_settings) as client:
        first = await client.call("run", _run("run-1"))
        # The first run's event is the proof that it is under way and not merely queued.
        assert (await client.receive())["params"]["run_id"] == "run-1"

        refused = await client.request("run", _run("run-2"))
        assert refused["error"]["code"] == -32001
        assert "run-1" in refused["error"]["message"]
        assert len(recorder.calls) == 1

        hold.set()
        assert (await client.reply(first))["result"]["status"] == "complete"


async def test_a_run_after_the_first_has_finished_succeeds() -> None:
    recorder = Recorder()
    async with serving(runner=recorder, settings=_settings) as client:
        first = await client.result("run", _run("run-1"))
        second = await client.result("run", _run("run-2", task="Then check out"))

    assert first["status"] == second["status"] == "complete"
    assert [call["task"] for call in recorder.calls] == ["Add the kettle to the cart", "Then check out"]


@pytest.mark.parametrize(
    "refused",
    [_run("run-2", limits={"max_steps": 0}), _run("run-2", local=True, cloud_profile="prof_1")],
    ids=["invalid params", "a configuration error"],
)
async def test_a_refused_run_leaves_the_server_free_for_the_next(refused: dict[str, Any]) -> None:
    async with serving(runner=Recorder(), settings=_settings) as client:
        assert "error" in await client.request("run", refused)
        result = await client.result("run", _run("run-3"))

    assert result["status"] == "complete"


async def test_the_server_answers_other_requests_while_a_run_is_active() -> None:
    hold = asyncio.Event()
    async with serving(runner=Recorder(hold=hold), settings=_settings) as client:
        run = await client.call("run", _run())
        handshake = await client.result("initialize", {"protocol_version": 1})
        hold.set()
        reply = await client.reply(run)

    assert handshake["protocol_version"] == 1
    assert reply["result"]["status"] == "complete"


async def test_a_run_that_raises_ends_as_an_error_result_and_the_server_keeps_serving() -> None:
    attempts = 0

    async def flaky(task: str, **_: Any) -> RunResult:
        nonlocal attempts
        attempts += 1
        if attempts == 1:
            raise RuntimeError("Chrome went away")
        return _result()

    async with serving(runner=flaky, settings=_settings) as client:
        failed = await client.request("run", _run("run-1"))
        result = await client.result("run", _run("run-2"))

    assert (failed["result"]["status"], failed["result"]["error"]) == ("error", "RuntimeError: Chrome went away")
    assert result["status"] == "complete"


def _command(*arguments: str) -> list[str]:
    return [sys.executable, "-c", "from fastbrowse.cli import main; main()", "serve", *arguments]


def _environment() -> dict[str, str]:
    """The keys a run is refused without, and the tests importable by name."""
    return os.environ | {"PYTHONPATH": str(REPO), "OPENROUTER_API_KEY": "unused", "AI_GATEWAY_API_KEY": "unused"}


def _serve(*arguments: str) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        _command(*arguments), input=b"", capture_output=True, timeout=60, env=_environment(), check=False
    )


def test_the_hidden_option_swaps_in_a_named_callable() -> None:
    request = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "run",
        "params": _run("run-1", local=True, chrome={"binary": sys.executable}),
    }

    with subprocess.Popen(
        _command("--stdio", "--run-task", "tests.serve_scripts:echo"),
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        env=_environment(),
    ) as process:
        assert process.stdin is not None and process.stdout is not None
        process.stdin.write(json.dumps(request).encode() + b"\n")
        process.stdin.flush()
        # Read before the input is closed: the end of input ends the server, and a run with it.
        event, reply = json.loads(process.stdout.readline()), json.loads(process.stdout.readline())
        process.stdin.close()
        assert process.wait(timeout=30) == 0

    assert event == {
        "jsonrpc": "2.0",
        "method": "run/event",
        "params": {"run_id": "run-1", "event": {"type": "browser", "live_url": None, "browser_id": None}},
    }
    assert reply["id"] == 1
    assert reply["result"]["answer"] == "echo: Add the kettle to the cart"


def test_the_hidden_option_is_not_in_the_help() -> None:
    served = _serve("--help")

    assert served.returncode == 0
    assert b"--stdio" in served.stdout
    assert b"run-task" not in served.stdout and b"run_task" not in served.stdout.lower()


@pytest.mark.parametrize(
    "name",
    ["tests.serve_scripts:missing", "tests.no_such_module:echo", "tests.serve_scripts", "tests.serve_scripts:__doc__"],
    ids=["no such attribute", "no such module", "no attribute named", "not callable"],
)
def test_a_name_that_is_not_a_callable_is_a_usage_error(name: str) -> None:
    served = _serve("--stdio", "--run-task", name)

    assert served.returncode == 2
    assert served.stdout == b""
    assert name.encode() in served.stderr
