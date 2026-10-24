"""Send the same JSON-RPC transcripts to `fastbrowse serve --stdio` in Python and in Rust, and diff every line.

Both servers run a stand-in for `run_task` (the SDK's `servers/scripted_run.py` runs and `tests/serve_scripts.py`,
which the Rust binary carries under the same names), so what is compared is the protocol: replies, errors,
notifications, the server's own requests, their order, and the exit code.

    cargo build -p fastbrowse-cli
    uv run python scripts/rust_serve_diff.py [--rust target/debug/fastbrowse] [--only NAME] [--show]

A transcript is a list of steps. After each one the output is read until the server has been quiet for a moment,
so each step's lines are compared as a group, in order.
"""

import argparse
import json
import os
import queue
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from typing import Any

REPO = Path(__file__).resolve().parent.parent
PYTHON = REPO / ".venv" / "bin" / "python"
SCRIPTED_RUN = REPO / "packages" / "sdk" / "test" / "servers" / "scripted_run.py"
# Long enough for Python to answer after a step; the first reply waits for the interpreter to start.
QUIET = 0.4
FIRST = 15.0

INIT = {"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocol_version": 1}}
SHOP = "https://shop.example.com"


def run(request_id: int, run_id: str = "run-1", **fields: Any) -> dict[str, Any]:
    params = {"run_id": run_id, "task": "Add the kettle to the cart"} | fields
    return {"jsonrpc": "2.0", "id": request_id, "method": "run", "params": params}


def call(request_id: Any, method: str, params: Any = None) -> dict[str, Any]:
    message: dict[str, Any] = {"jsonrpc": "2.0", "id": request_id, "method": method}
    if params is not None:
        message["params"] = params
    return message


def send(*messages: Any) -> dict[str, Any]:
    """Lines written together, so the server finds them all waiting. A str is sent as it is."""
    return {"send": [m if isinstance(m, str) else json.dumps(m) for m in messages]}


def answer(result: Any) -> dict[str, Any]:
    return {"answer": result}


def refuse(error: Any) -> dict[str, Any]:
    return {"error": error}


CLOSE = {"close": True}
LOCAL = {"local": True, "chrome": {"binary": str(PYTHON)}}

# (name, run, steps). `run` is a scripted_run.py run or a `tests.serve_scripts:` name.
CASES: list[tuple[str, str, list[dict[str, Any]]]] = [
    ("handshake", "echo", [send(INIT), send(call(1, "initialize", {"protocol_version": 2, "x": 1})), CLOSE]),
    ("shutdown", "echo", [send(INIT), send(call(1, "shutdown")), CLOSE]),
    ("shutdown refused with a param", "echo", [send(call(1, "shutdown", {"force": True})), send(INIT), CLOSE]),
    (
        "unknown methods and notifications",
        "echo",
        [
            send(call(1, "run/teleport")),
            send({"jsonrpc": "2.0", "method": "run/teleport"}),
            send({"jsonrpc": "2.0", "method": "initialize", "params": {}}),
            send(call("str-id", "initialize", {"protocol_version": 1})),
            CLOSE,
        ],
    ),
    (
        "lines that are not JSON",
        # Not the SDK's scripted server, whose own reading of each line would fail on these first.
        "tests.serve_scripts:echo",
        [
            send('{"jsonrpc": "2.0", "id": 1, "method"'),
            send(""),
            send("   "),
            send("[1,]"),
            send('{"a":1 "b":2}'),
            send('"abc'),
            send('"a\\x"'),
            send('{"a":\n'),
            CLOSE,
        ],
    ),
    (
        "JSON that is not a request",
        "tests.serve_scripts:echo",
        [
            send([{"jsonrpc": "2.0", "id": 1, "method": "initialize"}]),
            send({"jsonrpc": "1.0", "id": 4, "method": "initialize"}),
            send({"jsonrpc": "2.0", "id": "abc", "method": 5}),
            send({"jsonrpc": "2.0", "id": True, "method": "initialize"}),
            send({"jsonrpc": "2.0", "id": 1.5}),
            send({"jsonrpc": "2.0", "id": None, "method": "x", "params": "s"}),
            send("5"),
            send("null"),
            send({"jsonrpc": 2, "id": 9, "method": "x"}),
            CLOSE,
        ],
    ),
    (
        "invalid params",
        "echo",
        [
            send(call(1, "initialize", {})),
            send(call(2, "initialize", {"protocol_version": "one"})),
            send(call(3, "initialize", [1])),
            send(call(4, "initialize", {"protocol_version": True})),
            send(call(5, "run/cancel", {})),
            send(call(6, "run/cancel", {"run_id": "a", "x": 1})),
            send(call(7, "run", {})),
            send(call(8, "run", {"run_id": 5, "task": None})),
            send(call(9, "run", {"run_id": "", "task": "t"})),
            send(run(10, headless=True, zzz=1)),
            send(run(11, limits={"max_steps": 0})),
            send(run(12, limits={"max_steps": "x", "max_dollars": -1, "max_seconds": "abc", "max_jev_calls": 1.5})),
            send(run(13, limits="x")),
            send(run(14, chrome={"headed": True, "devtools": True})),
            send(run(15, chrome={"headed": "maybe", "binary": 3, "profile": 4})),
            send(run(16, attachments=[{"name": "a", "mime_type": "text/plain", "content": "not base64!"}])),
            send(run(17, attachments=[{"name": "a", "mime_type": "text/plain", "content": "aGVsbG8"}])),
            send(run(18, attachments=[{"name": "a"}])),
            send(run(19, attachments="x")),
            send(run(20, viewport=[1280])),
            send(run(21, viewport=[1280, 720, 3])),
            send(run(22, viewport=["a", 1.5])),
            send(run(23, secrets=[{"name": "SHOP_PASSWORD"}])),
            send(run(24, secrets=[{"name": "S", "origins": ["https://a"], "value": "v"}])),
            send(run(25, secrets={"S": "v"})),
            send(run(26, output_schema="order")),
            send(run(27, inputs={"a": 1})),
            send(run(28, frames="maybe", until=2, cursor=[], local={}, foreground=None)),
            send(run(29, cdp_port="abc")),
            send(run(30, cdp_port=1.5)),
            send(run(31, downloads=5, record=True)),
            send(run(32, start=5, cloud_profile=1, cdp_url=False, target_match=[], proxy_country={})),
            send(run(33, authorization={"irreversible_actions": "x", "y": 1})),
            send(call(34, "run", [])),
            send(run(35, attachments=[{"name": "a", "mime_type": "m", "content": 5}])),
            send(run(36, viewport=[])),
            send(run(37, proxy_country="gb")),
            send(run(38, proxy_country="usa")),
            CLOSE,
        ],
    ),
    (
        "configuration errors",
        "echo",
        [
            send(run(1, local=True, cloud_profile="prof_1")),
            send(run(2, local=True, proxy_country="uk")),
            send(run(3, cdp_url="ws://b.test/devtools", cdp_port=9222)),
            send(run(4, cdp_url="http://b.test/devtools")),
            send(run(5, cdp_port=70000)),
            send(run(6, attach=True)),
            send(run(7, cdp_port=9222, chrome={"headed": True})),
            send(run(8, local=True, chrome={"binary": "not-installed-anywhere"})),
            send(run(9, output_schema={"type": "object", "properties": {"id": {"type": "string", "pattern": "^A-"}}})),
            send(run(10, output_schema={"type": "array", "items": {"type": "string"}})),
            send(run(11, target_match="Inbox")),
            send(run(12, cdp_port=0, local=True, cloud_profile="p", proxy_country="de")),
            CLOSE,
        ],
    ),
    (
        "a run's request as the run received it",
        "echo",
        [
            send(
                run(
                    1,
                    start=f"{SHOP}/cart",
                    inputs={"email": "ada@example.com"},
                    attachments=[{"name": "receipt.pdf", "mime_type": "application/pdf", "content": "+//+"}],
                    limits={"max_steps": 12, "max_dollars": 0.5, "max_seconds": 90},
                    authorization={"irreversible_actions": True},
                    viewport=[1280, "720"],
                    cloud_allow_resizing=True,
                    proxy_country="UK",
                    frames="yes",
                )
            ),
            send(run(2, "run-2", **LOCAL, cdp_port=None)),
            CLOSE,
        ],
    ),
    ("events in order", "three_events", [send(run(1, "run-7")), send(run(2, "run-8")), CLOSE]),
    ("needs login", "needs_login", [send(run(1)), CLOSE]),
    ("request count", "request_count", [send(run(1, limits={"max_steps": 0})), send(run(2)), send(run(3)), CLOSE]),
    (
        "output schema",
        "found",
        [
            send(
                run(
                    1,
                    inputs={"found": json.dumps({"order-id": "A-17", "class": "express", "n": 2, "ok": True})},
                    output_schema={
                        "type": "object",
                        "properties": {
                            "order-id": {"type": "string", "description": "The order number"},
                            "class": {"type": "string"},
                            "n": {"type": "integer"},
                            "ok": {"type": "boolean"},
                        },
                        "required": ["order-id", "class", "n", "ok"],
                        "additionalProperties": False,
                    },
                )
            ),
            CLOSE,
        ],
    ),
    (
        "busy, then free",
        "never_ends",
        [
            send(run(1, "run-1")),
            send(run(2, "run-2")),
            send(call(3, "initialize", {"protocol_version": 1})),
            send(call(4, "run/cancel", {"run_id": "run-0"})),
            send(call(5, "run/cancel", {"run_id": "run-1"})),
            send(call(6, "run/cancel", {"run_id": "run-1"})),
            send(run(7, "run-3")),
            send(call(8, "shutdown")),
            CLOSE,
        ],
    ),
    ("cancel sent straight after the run", "never_ends", [send(run(1), call(2, "run/cancel", {"run_id": "run-1"})), CLOSE]),
    ("shutdown sent straight after the run", "never_ends", [send(run(1), call(2, "shutdown")), CLOSE]),
    ("end of input during a run", "never_ends", [send(run(1)), CLOSE]),
    ("a refused run then another", "never_ends", [send(run(1, local=True, cloud_profile="p"), run(2, "run-2")), CLOSE]),
    (
        "secrets resolved",
        "types_secrets",
        [
            send(
                run(
                    1,
                    "run-7",
                    secrets=[{"name": "SHOP_TOTP", "origins": ["https://*.example.com"]}],
                    inputs={"name": "SHOP_TOTP", "origin": "https://accounts.example.com"},
                )
            ),
            answer("492817"),
            answer(None),
            send({"jsonrpc": "2.0", "id": 99, "result": "late"}),
            send({"jsonrpc": "2.0", "id": 98, "error": {"code": -32000, "message": "no vault"}}),
            send(INIT),
            CLOSE,
        ],
    ),
    (
        "secrets on an origin the ref does not cover",
        "types_secrets",
        [
            send(run(1, secrets=[{"name": "P", "origins": [SHOP]}], inputs={"name": "P", "origin": "https://evil.test"})),
            send(run(2, "run-2", secrets=[{"name": "P", "origins": [SHOP]}], inputs={"name": "Q", "origin": SHOP})),
            CLOSE,
        ],
    ),
    (
        "a secret reply of the wrong shape",
        "types_secrets",
        [send(run(1, secrets=[{"name": "P", "origins": [SHOP]}], inputs={"name": "P", "origin": SHOP})), answer({"v": 1}), CLOSE],
    ),
    (
        "an error reply to a secret",
        "types_secrets",
        [
            send(run(1, secrets=[{"name": "P", "origins": [SHOP]}], inputs={"name": "P", "origin": SHOP})),
            refuse({"code": -32000, "message": "the vault refused hunter2"}),
            send(run(2, "run-2", secrets=[{"name": "P", "origins": [SHOP]}], inputs={"name": "P", "origin": SHOP})),
            refuse("text"),
            CLOSE,
        ],
    ),
    (
        "cancel while a secret is awaited",
        "types_secrets",
        [
            send(run(1, secrets=[{"name": "P", "origins": [SHOP]}], inputs={"name": "P", "origin": SHOP})),
            send(call(2, "run/cancel", {"run_id": "run-1"})),
            send({"jsonrpc": "2.0", "id": 1, "result": "late"}),
            send(INIT),
            CLOSE,
        ],
    ),
    ("until yes", "ends_on_an_order", [send(run(1, "run-7", until=True)), answer(True), CLOSE]),
    ("until no", "ends_on_an_order", [send(run(1, until=True)), answer(False), CLOSE]),
    ("until not a boolean", "ends_on_an_order", [send(run(1, until=True)), answer("yes"), CLOSE]),
    (
        "until refused",
        "ends_on_an_order",
        [
            send(run(1, until=True)),
            refuse({"code": -32000, "message": "orders page never loaded"}),
            send(run(2, "run-2", until=True)),
            refuse({"code": True, "message": "not yet"}),
            send(run(3, "run-3", until=True)),
            refuse({"code": "-32000", "message": 7}),
            send(run(4, "run-4", until=True)),
            {"raw_reply": {"result": True, "error": None}},
            CLOSE,
        ],
    ),
    ("no check held", "ends_on_an_order", [send(run(1)), CLOSE]),
    ("frames", "two_frames", [send(run(1, "run-7", frames=True)), send(run(2, "run-8")), CLOSE]),
    ("serve_scripts echo", "tests.serve_scripts:echo", [send(run(1, **LOCAL)), CLOSE]),
    (
        "serve_scripts type_secret",
        "tests.serve_scripts:type_secret",
        [send(run(1, **LOCAL, start=f"{SHOP}/login", secrets=[{"name": "P", "origins": [SHOP]}])), answer("hunter2"), CLOSE],
    ),
]


class Server:
    """One server process, its stdout read line by line on a thread."""

    def __init__(self, command: list[str], env: dict[str, str], cwd: str) -> None:
        self.process = subprocess.Popen(
            command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env, cwd=cwd
        )
        self.lines: queue.Queue[bytes | None] = queue.Queue()
        self.last_request: dict[str, Any] | None = None
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self) -> None:
        assert self.process.stdout is not None
        for line in self.process.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def write(self, data: bytes) -> None:
        assert self.process.stdin is not None
        try:
            self.process.stdin.write(data)
            self.process.stdin.flush()
        except BrokenPipeError:
            pass

    def read(self, first: float) -> list[str]:
        """Lines until the server has been quiet for QUIET seconds, waiting up to `first` for the first one."""
        out: list[str] = []
        timeout = first
        while True:
            try:
                line = self.lines.get(timeout=timeout)
            except queue.Empty:
                return out
            if line is None:
                self.lines.put(None)
                return out
            text = line.decode(errors="replace").rstrip("\n")
            message = json.loads(text) if text.startswith("{") else None
            if isinstance(message, dict) and "method" in message and "id" in message:
                self.last_request = message
            out.append(text)
            timeout = QUIET


def transcript(command: list[str], env: dict[str, str], cwd: str, steps: list[dict[str, Any]]) -> list[Any]:
    server = Server(command, env, cwd)
    record: list[Any] = []
    first = FIRST
    for step in steps:
        if "send" in step:
            server.write(b"".join(line.encode() + b"\n" for line in step["send"]))
        elif "answer" in step or "error" in step or "raw_reply" in step:
            asked = server.last_request or {"id": None}
            reply: dict[str, Any] = {"jsonrpc": "2.0", "id": asked["id"]}
            if "answer" in step:
                reply["result"] = step["answer"]
            elif "error" in step:
                reply["error"] = step["error"]
            else:
                reply |= step["raw_reply"]
            server.write(json.dumps(reply).encode() + b"\n")
        elif step.get("close"):
            assert server.process.stdin is not None
            try:
                server.process.stdin.close()
            except BrokenPipeError:
                pass
        record.append(server.read(first))
        first = QUIET * 4
    try:
        code = server.process.wait(timeout=20)
    except subprocess.TimeoutExpired:
        server.process.kill()
        code = "timeout"
    record.append(server.read(QUIET) + [f"exit {code}"])
    return record


def commands(run: str, rust: Path) -> tuple[list[str], list[str]]:
    if run.startswith("tests."):
        python = [str(PYTHON), "-c", "from fastbrowse.cli import main; main()", "serve", "--stdio", "--run-task", run]
        return python, [str(rust), "serve", "--stdio", "--run-task", run]
    python = [str(PYTHON), str(SCRIPTED_RUN), run, "serve", "--stdio"]
    return python, [str(rust), "serve", "--stdio", "--run-task", f"scripted_run:{run}"]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--rust", type=Path, default=REPO / "target" / "debug" / "fastbrowse")
    parser.add_argument("--only", help="run the cases whose name contains this")
    parser.add_argument("--show", action="store_true", help="print every transcript, not only differences")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory() as scratch:
        # `--record` is refused without an ffmpeg, and this machine may have none.
        (Path(scratch) / "ffmpeg").write_text("#!/bin/sh\n")
        (Path(scratch) / "ffmpeg").chmod(0o755)
        env = {
            "PATH": f"{scratch}{os.pathsep}{os.environ['PATH']}",
            "HOME": os.environ.get("HOME", scratch),
            "PYTHONPATH": str(REPO),
            "OPENROUTER_API_KEY": "unused",
            "AI_GATEWAY_API_KEY": "unused",
            "BROWSER_USE_API_KEY": "unused",
        }
        failed = 0
        lines = 0
        for name, run, steps in CASES:
            if args.only and args.only not in name:
                continue
            python_command, rust_command = commands(run, args.rust)
            # Run from a directory with no `.env`, so neither side reads a checkout's settings.
            started = time.perf_counter()
            expected = transcript(python_command, env, scratch, steps)
            actual = transcript(rust_command, env, scratch, steps)
            same = expected == actual
            lines += sum(len(group) for group in expected)
            failed += not same
            print(f"{'ok  ' if same else 'DIFF'} {name} ({time.perf_counter() - started:.1f}s)")
            if not same or args.show:
                for index, (want, got) in enumerate(zip(expected, actual, strict=True)):
                    if want != got or args.show:
                        print(f"  step {index}:")
                        for line in want:
                            print(f"    python {line}")
                        for line in got:
                            print(f"    rust   {line}")
        print(f"{failed} of {len(CASES) if not args.only else 'the selected'} transcripts differ; {lines} Python lines compared")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
