"""Smoke-test a fastbrowse executable: the handshake, and one run on local Chrome against a page served here.

    python scripts/smoke_binary.py dist/fastbrowse/fastbrowse

Two cases, and it stays at two. They exist to catch what only freezing breaks: a data file that was not
collected, an import PyInstaller did not see. Everything else is the test suite's to find, on the checkout.

The run needs no model and no key. Its models are the scripted ones in `fastbrowse.scripted`, and the page is
served from this process, so the one thing it needs from the machine is Chrome.

Standard library only: it runs on a build runner before anything is installed there.
"""

import argparse
import json
import os
import queue
import subprocess
import sys
import tempfile
import threading
from collections.abc import Iterator
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import IO, Any

PROTOCOL_VERSION = 1

# The click leaves a mark on the server as well as on the page, so the run is known to have reached a real
# document, which a result that only says "complete" would not show.
PAGE = b"""<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Counter</title></head>
<body>
<h1>Counter</h1>
<p id="count">Count: 0</p>
<button type="button" id="add">Add one</button>
<script>
document.getElementById('add').addEventListener('click', async () => {
  await fetch('/added', {method: 'POST'});
  document.getElementById('count').textContent = 'Count: 1';
});
</script>
</body>
</html>
"""
TASK = "click Add one"

# A cold Chrome on a CI runner has taken over 30 seconds to start, and the run follows it.
RUN_SECONDS = 240.0
HANDSHAKE_SECONDS = 60.0

MODEL_KEYS = ("TYPESAFE_API_KEY", "AI_GATEWAY_API_KEY", "OPENROUTER_API_KEY", "BROWSER_USE_API_KEY")


class Failed(Exception):
    """A case that did not hold, with what was seen instead."""


def environment() -> dict[str, str]:
    """The caller's environment with every model key taken out, and one stand-in put back.

    `serve` refuses a run before a browser opens when the key its models would need is unset, whatever runs
    the task. The stand-in passes that check and opens nothing: a request made with it would be refused by the
    provider, so a run that completes made none.
    """
    kept = {name: value for name, value in os.environ.items() if name not in MODEL_KEYS}
    return kept | {"OPENROUTER_API_KEY": "smoke-test-stand-in"}


class Client:
    """The other end of `serve --stdio`: requests out, and each line that comes back."""

    def __init__(self, process: subprocess.Popen[bytes]) -> None:
        self._process = process
        self._lines: queue.Queue[bytes | None] = queue.Queue()
        self._ids = 0
        assert process.stdout is not None
        threading.Thread(target=self._pump, args=(process.stdout,), daemon=True).start()

    def _pump(self, stdout: IO[bytes]) -> None:
        for line in stdout:
            self._lines.put(line)
        self._lines.put(None)

    def request(self, method: str, params: dict[str, Any] | None, seconds: float) -> tuple[Any, list[dict[str, Any]]]:
        """The result of one request, and the notifications that arrived ahead of it, in order."""
        self._ids += 1
        message: dict[str, Any] = {"jsonrpc": "2.0", "id": self._ids, "method": method}
        if params is not None:
            message["params"] = params
        assert self._process.stdin is not None
        self._process.stdin.write(json.dumps(message).encode() + b"\n")
        self._process.stdin.flush()
        ahead: list[dict[str, Any]] = []
        while True:
            try:
                line = self._lines.get(timeout=seconds)
            except queue.Empty:
                raise Failed(f"{method}: no reply within {seconds:.0f}s") from None
            if line is None:
                raise Failed(f"{method}: the server closed its output, exit status {self._process.wait()}")
            try:
                reply = json.loads(line)
            except ValueError:
                raise Failed(f"{method}: stdout carried a line that is not a message: {line[:200]!r}") from None
            if "method" in reply:
                ahead.append(reply)
            elif reply.get("id") == self._ids:
                if "error" in reply:
                    raise Failed(f"{method}: refused with {reply['error']}")
                return reply["result"], ahead


@contextmanager
def serving(executable: Path, *arguments: str) -> Iterator[Client]:
    """`serve --stdio` under a client, shut down and checked for a clean exit on the way out."""
    # Started from an empty directory, so a `.env` beside the caller cannot hand the server a real key.
    with (
        tempfile.TemporaryDirectory() as empty,
        subprocess.Popen(
            [str(executable), "serve", "--stdio", *arguments],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            env=environment(),
            cwd=empty,
        ) as process,
    ):
        client = Client(process)
        try:
            yield client
            client.request("shutdown", None, HANDSHAKE_SECONDS)
            status = process.wait(timeout=HANDSHAKE_SECONDS)
            if status != 0:
                raise Failed(f"the server exited with status {status} after shutdown")
        finally:
            if process.poll() is None:
                process.kill()


def handshake(client: Client) -> str:
    result, _ = client.request("initialize", {"protocol_version": PROTOCOL_VERSION}, HANDSHAKE_SECONDS)
    if result.get("protocol_version") != PROTOCOL_VERSION:
        raise Failed(f"initialize: expected protocol {PROTOCOL_VERSION}, got {result}")
    return str(result["fastbrowse_version"])


def check_initialize(executable: Path) -> None:
    """The executable is the CLI and the server both, and the two agree on which build they are."""
    told = subprocess.run(
        [str(executable), "--version"], capture_output=True, text=True, env=environment(), timeout=HANDSHAKE_SECONDS
    )
    if told.returncode != 0:
        raise Failed(f"--version exited with status {told.returncode}: {told.stderr.strip()}")
    with serving(executable) as client:
        version = handshake(client)
    if told.stdout.split() != ["fastbrowse", version]:
        raise Failed(f"--version printed {told.stdout.strip()!r} and initialize reported {version!r}")
    print(f"initialize: fastbrowse {version}, protocol {PROTOCOL_VERSION}")


@contextmanager
def fixture_site() -> Iterator[tuple[str, list[str]]]:
    """The page on a local port, and the requests it has had beyond the page itself."""
    marks: list[str] = []

    class Handler(BaseHTTPRequestHandler):
        def _answer(self, body: bytes) -> None:
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self) -> None:
            self._answer(PAGE)

        def do_POST(self) -> None:
            marks.append(self.path)
            self._answer(b"")

        def log_message(self, format: str, *args: Any) -> None:
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_address[1]}/", marks
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


def check_run(executable: Path) -> None:
    """One run on local Chrome ends `complete`, having clicked the page's button."""
    with fixture_site() as (start, marks), serving(executable, "--run-task", "fastbrowse.scripted:run_task") as client:
        handshake(client)
        params = {"run_id": "smoke", "task": TASK, "start": start, "local": True}
        result, ahead = client.request("run", params, RUN_SECONDS)
    events = [message["params"]["event"] for message in ahead if message["method"] == "run/event"]
    clicks = [
        event["step"]
        for event in events
        if event["type"] == "step" and event["step"]["operation"] == "click" and event["step"]["outcome"] == "executed"
    ]
    if result["status"] != "complete":
        raise Failed(f"run: ended {result['status']!r} ({result.get('error')}), steps {result['steps']}")
    if not events or events[0]["type"] != "browser":
        raise Failed(f"run: the browser event did not come first: {[event['type'] for event in events]}")
    if [step["target"] for step in clicks] != ["Add one"]:
        raise Failed(f"run: expected one click on 'Add one', got events {events}")
    if marks != ["/added"]:
        raise Failed(f"run: the page was not clicked once, its server saw {marks}")
    print(f"run: complete in {len(result['steps'])} steps against {start}")


CASES = {"initialize": check_initialize, "run": check_run}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("executable", type=Path, help="the fastbrowse executable to test")
    parser.add_argument("--case", choices=sorted(CASES), action="append", help="run only this case (repeatable)")
    args = parser.parse_args()
    executable = args.executable.resolve()
    if not executable.exists() and executable.with_suffix(".exe").exists():
        # One command line for every target: a Windows build puts the suffix on.
        executable = executable.with_suffix(".exe")
    failed = False
    for name in args.case or list(CASES):
        try:
            CASES[name](executable)
        except Failed as exc:
            failed = True
            print(f"FAILED {name}: {exc}", file=sys.stderr)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
