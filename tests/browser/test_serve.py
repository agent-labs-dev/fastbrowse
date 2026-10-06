"""`fastbrowse serve` as a subprocess on local Chrome: one run with the real `run_task`, and what is left after.

The models are the scripted ones in `fastbrowse.scripted`, reached through the hidden `--run-task` option, so
the run needs no key. Everything else is what a client gets: the command, its pipes and a Chrome it starts.
"""

import json
import os
import queue
import subprocess
import sys
import threading
import time
from collections.abc import Iterator
from contextlib import contextmanager
from http.server import ThreadingHTTPServer
from pathlib import Path
from typing import IO, Any

import pytest

from fastbrowse.adapters.local_chrome import find_chrome
from tests.browser.conftest import SITES, _handler_for

# Windows has no `ps`, and the process tree is how a test finds the Chrome that the server started.
pytestmark = pytest.mark.skipif(sys.platform == "win32", reason="reads the process tree with ps")

TOKEN = "s3cr3t-T0ken-225"
TASK = "fill Token = secret:token; click Save"
CANCELLED = -32002
MODEL_KEYS = ("TYPESAFE_API_KEY", "AI_GATEWAY_API_KEY", "OPENROUTER_API_KEY", "BROWSER_USE_API_KEY")

# A cold Chrome on a CI runner has taken over 30 seconds to start, and the suite's own limit is two minutes.
PATIENCE = 100.0
# Chrome's helper processes go a moment after the one the server waited for.
GONE_WITHIN = 15.0


@pytest.fixture
def chrome() -> None:
    if find_chrome(None) is None:
        pytest.skip("Chrome is not installed")


@pytest.fixture
def site() -> Iterator[tuple[str, list[str]]]:
    """The main fixture site on a port of its own, and the body of every POST it has had."""
    posted: list[str] = []
    server = ThreadingHTTPServer(("127.0.0.1", 0), _handler_for(SITES / "main", posted=posted))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_address[1]}", posted
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


class Client:
    """The other end of the pipe: lines of JSON out, and each message that comes back, in order."""

    def __init__(self, process: subprocess.Popen[bytes], stderr: Path) -> None:
        self.process = process
        self._stderr = stderr
        self._lines: queue.Queue[bytes | None] = queue.Queue()
        # Every line the server wrote, as it wrote it, for what must appear nowhere at all.
        self.written = b""
        assert process.stdout is not None
        threading.Thread(target=self._pump, args=(process.stdout,), daemon=True).start()

    def _pump(self, stdout: IO[bytes]) -> None:
        for line in stdout:
            self._lines.put(line)
        self._lines.put(None)

    def send(self, message: dict[str, Any]) -> None:
        assert self.process.stdin is not None
        self.process.stdin.write(json.dumps({"jsonrpc": "2.0", **message}).encode() + b"\n")
        self.process.stdin.flush()

    def receive(self) -> dict[str, Any] | None:
        """The next message, or None once the server has closed its output."""
        try:
            line = self._lines.get(timeout=PATIENCE)
        except queue.Empty:
            raise AssertionError(f"nothing from the server in {PATIENCE:.0f}s, stderr: {self.stderr()}") from None
        if line is None:
            return None
        self.written += line
        return json.loads(line)

    def until(self, wanted: str) -> list[dict[str, Any]]:
        """Messages up to and including the first that is the method `wanted`, or the reply with that id."""
        seen: list[dict[str, Any]] = []
        while (message := self.receive()) is not None:
            seen.append(message)
            if message.get("method", message.get("id")) == wanted:
                return seen
        raise AssertionError(f"the server closed its output before {wanted!r}: {seen}, stderr: {self.stderr()}")

    def rest(self) -> list[dict[str, Any]]:
        """What the server wrote from here to the end of its output."""
        seen: list[dict[str, Any]] = []
        while (message := self.receive()) is not None:
            seen.append(message)
        return seen

    def stderr(self) -> str:
        return self._stderr.read_text(encoding="utf-8", errors="replace")

    def chrome(self) -> set[int]:
        """The processes under the server, which are the Chrome it started and that Chrome's own helpers."""
        listed = subprocess.run(["ps", "-axo", "pid=,ppid=,command="], capture_output=True, text=True, check=True)
        rows = [line.split(None, 2) for line in listed.stdout.splitlines()]
        children: dict[int, list[int]] = {}
        commands: dict[int, str] = {}
        for pid, parent, *command in rows:
            children.setdefault(int(parent), []).append(int(pid))
            commands[int(pid)] = " ".join(command)
        found: set[int] = set()
        ahead = [self.process.pid]
        while ahead:
            under = children.get(ahead.pop(), [])
            found.update(under)
            ahead.extend(under)
        assert any("--remote-debugging-port" in commands[pid] for pid in found), f"no Chrome under the server: {rows}"
        return found


def _alive(pids: set[int]) -> set[int]:
    alive: set[int] = set()
    for pid in pids:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            continue
        except PermissionError:
            # The number was given to someone else's process, so ours is gone.
            continue
        alive.add(pid)
    return alive


def _gone(pids: set[int]) -> bool:
    deadline = time.monotonic() + GONE_WITHIN
    while _alive(pids) and time.monotonic() < deadline:
        time.sleep(0.1)
    return not _alive(pids)


@contextmanager
def serving(tmp_path: Path) -> Iterator[Client]:
    """The command with the scripted models, from an empty directory and with no model key of the caller's.

    A `.env` beside the caller would otherwise hand the server a real key. The one stand-in passes the check
    `serve` makes before a browser opens, and a request made with it would be refused by the provider.
    """
    empty = tmp_path / "empty"
    empty.mkdir()
    stderr = tmp_path / "stderr"
    kept = {name: value for name, value in os.environ.items() if name not in MODEL_KEYS}
    with (
        stderr.open("wb") as log,
        subprocess.Popen(
            [
                sys.executable,
                "-c",
                "from fastbrowse.cli import main; main()",
                "serve",
                "--stdio",
                "--run-task",
                "fastbrowse.scripted:run_task",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=log,
            env=kept | {"OPENROUTER_API_KEY": "stand-in"},
            cwd=empty,
        ) as process,
    ):
        try:
            yield Client(process, stderr)
        finally:
            if process.poll() is None:
                process.kill()


def _run(client: Client, origin: str) -> None:
    client.send(
        {
            "id": "run",
            "method": "run",
            "params": {
                "run_id": "run-1",
                "task": TASK,
                "start": f"{origin}/token.html",
                "local": True,
                "secrets": [{"name": "token", "origins": [origin]}],
            },
        }
    )


def test_one_run_types_a_secret_the_client_holds_and_leaves_no_chrome_behind(
    chrome: None, site: tuple[str, list[str]], tmp_path: Path
) -> None:
    origin, posted = site
    with serving(tmp_path) as client:
        _run(client, origin)
        *before, question = client.until("secrets/resolve")
        started = client.chrome()
        client.send({"id": question["id"], "result": TOKEN})
        *after, reply = client.until("run")

        client.send({"id": "bye", "method": "shutdown"})
        assert client.rest() == [{"jsonrpc": "2.0", "id": "bye", "result": None}]
        assert client.process.wait(timeout=30) == 0
        assert _gone(started)
        stderr = client.stderr()

    assert question["params"] == {"run_id": "run-1", "name": "token", "origin": origin}
    assert "error" not in reply, reply
    result = reply["result"]
    assert result["status"] == "complete", (result, stderr)
    assert posted == [TOKEN]

    notifications = [*before, *after]
    assert {message["method"] for message in notifications} == {"run/event"}
    assert {message["params"]["run_id"] for message in notifications} == {"run-1"}
    events = [message["params"]["event"] for message in notifications]
    assert [event["type"] for event in events] == ["browser", "step", "step"]
    steps = [event["step"] for event in events[1:]]
    assert [(step["index"], step["operation"], step["target"], step["outcome"]) for step in steps] == [
        (0, "fill", "Token", "executed"),
        (1, "click", "Save", "executed"),
    ]
    assert steps == result["steps"]
    # The question goes out before the step that typed its answer is reported.
    assert [event["type"] for event in (message["params"]["event"] for message in before)] == ["browser"]

    assert TOKEN.encode() not in client.written
    assert TOKEN not in stderr


def _waiting_on_the_secret(client: Client, origin: str) -> set[int]:
    """Start the run and stop where it asks for the secret: Chrome is open, and the next move is the client's."""
    _run(client, origin)
    client.until("secrets/resolve")
    return client.chrome()


def test_shutdown_during_a_run_leaves_neither_the_server_nor_its_chrome(
    chrome: None, site: tuple[str, list[str]], tmp_path: Path
) -> None:
    origin, posted = site
    with serving(tmp_path) as client:
        started = _waiting_on_the_secret(client, origin)
        assert _alive(started) == started

        client.send({"id": "bye", "method": "shutdown"})
        replies = {message["id"]: message for message in client.rest()}
        # The input is still open, as it is when the SDK's `close()` waits for the exit.
        assert client.process.wait(timeout=30) == 0
        assert _gone(started)

    assert replies["bye"]["result"] is None
    assert replies["run"]["error"]["code"] == CANCELLED
    assert posted == []


def test_stdin_closing_during_a_run_leaves_neither_the_server_nor_its_chrome(
    chrome: None, site: tuple[str, list[str]], tmp_path: Path
) -> None:
    origin, posted = site
    with serving(tmp_path) as client:
        started = _waiting_on_the_secret(client, origin)
        assert _alive(started) == started

        assert client.process.stdin is not None
        client.process.stdin.close()
        (reply,) = client.rest()
        assert client.process.wait(timeout=30) == 0
        assert _gone(started)

    assert (reply["id"], reply["error"]["code"]) == ("run", CANCELLED)
    assert posted == []
