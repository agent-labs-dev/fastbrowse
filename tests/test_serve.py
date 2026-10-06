"""`fastbrowse serve --stdio` as a client sees it: the messages on the wire and the process exiting.

Most cases drive the server over in-memory streams with the fake client. The ones about the real pipe, which
are the file descriptors, line length and the exit code, start the command as a subprocess.
"""

import json
import subprocess
import sys
from importlib.metadata import version
from typing import Any, cast

import pytest
from pydantic import ValidationError

from fastbrowse.protocol import PROTOCOL_VERSION, Error, ErrorCode
from tests.serve_client import serving


async def test_initialize_answers_with_the_protocol_version_and_the_package_version() -> None:
    async with serving() as client:
        response = await client.request("initialize", {"protocol_version": 1})

    assert response == {
        "jsonrpc": "2.0",
        "id": 1,
        "result": {"protocol_version": 1, "fastbrowse_version": version("fastbrowse")},
    }
    assert PROTOCOL_VERSION == 1


async def test_a_client_on_another_protocol_version_still_learns_the_servers() -> None:
    # The client is the one that refuses a mismatch, and to name both versions it needs this reply.
    async with serving() as client:
        result = await client.result("initialize", {"protocol_version": 2, "added_in_two": True})

    assert result["protocol_version"] == 1


async def test_shutdown_is_answered_and_then_the_server_exits_zero() -> None:
    async with serving() as client:
        await client.result("initialize", {"protocol_version": 1})
        response = await client.request("shutdown")

        assert response == {"jsonrpc": "2.0", "id": 2, "result": None}
        assert await client.exit_code() == 0


async def test_end_of_input_is_a_shutdown() -> None:
    async with serving() as client:
        await client.result("initialize", {"protocol_version": 1})
        await client.close_input()

        assert await client.exit_code() == 0
        assert client.silent


async def test_an_unknown_method_is_refused_as_method_not_found() -> None:
    async with serving() as client:
        response = await client.request("run/teleport")

    assert response["id"] == 1
    assert response["error"]["code"] == -32601
    assert "run/teleport" in response["error"]["message"]
    assert "result" not in response


@pytest.mark.parametrize(
    ("params", "named"),
    [
        ({}, "protocol_version"),
        ({"protocol_version": "one"}, "protocol_version"),
        ([1], "params"),
    ],
    ids=["missing", "wrong type", "by position"],
)
async def test_malformed_params_are_refused_as_invalid_params(params: Any, named: str) -> None:
    async with serving() as client:
        await client.send({"jsonrpc": "2.0", "id": 7, "method": "initialize", "params": params})
        response = await client.reply(7)

    assert response["error"]["code"] == -32602
    assert named in response["error"]["message"]


async def test_a_param_the_method_does_not_take_is_refused_and_not_dropped() -> None:
    async with serving() as client:
        response = await client.request("shutdown", {"force": True})

        assert response["error"]["code"] == -32602
        assert "force" in response["error"]["message"]
        # A refused shutdown did not shut anything down.
        assert (await client.request("initialize", {"protocol_version": 1}))["id"] == 2


async def test_a_line_that_is_not_json_gets_a_parse_error_and_the_server_keeps_serving() -> None:
    async with serving() as client:
        await client.send_line(b'{"jsonrpc": "2.0", "id": 1, "method"\n')
        refused = await client.receive()
        await client.send_line(b"\xff\xfe\n")
        undecodable = await client.receive()
        result = await client.result("initialize", {"protocol_version": 1})

    assert refused == {"jsonrpc": "2.0", "id": None, "error": {"code": -32700, "message": refused["error"]["message"]}}
    assert undecodable["error"]["code"] == -32700
    assert result["protocol_version"] == 1


@pytest.mark.parametrize(
    ("message", "echoed_id"),
    [
        ([{"jsonrpc": "2.0", "id": 1, "method": "initialize"}], None),
        ({"jsonrpc": "1.0", "id": 4, "method": "initialize"}, 4),
        ({"jsonrpc": "2.0", "id": "abc", "method": 5}, "abc"),
        ({"jsonrpc": "2.0", "id": True, "method": "initialize"}, None),
    ],
    ids=["a batch", "another jsonrpc version", "a method that is not a name", "an id that is not one"],
)
async def test_json_that_is_not_a_request_is_refused_as_invalid_request(message: Any, echoed_id: Any) -> None:
    async with serving() as client:
        await client.send_line(json.dumps(message).encode() + b"\n")
        response = await client.receive()

    assert response["error"]["code"] == -32600
    assert response["id"] == echoed_id


async def test_a_notification_is_never_answered_even_when_its_method_is_unknown() -> None:
    async with serving() as client:
        await client.send({"jsonrpc": "2.0", "method": "run/teleport"})
        response = await client.request("initialize", {"protocol_version": 1})

        # Had the notification been answered, that answer would have come first and be waiting here.
        assert response["id"] == 1
        assert client.inbox == []


def test_the_error_codes_are_a_closed_set() -> None:
    assert {code.name.lower(): code.value for code in ErrorCode} == {
        "parse_error": -32700,
        "invalid_request": -32600,
        "method_not_found": -32601,
        "invalid_params": -32602,
        "internal_error": -32603,
        "busy": -32001,
        "cancelled": -32002,
        "configuration": -32003,
        "unsupported_schema": -32004,
    }
    with pytest.raises(ValidationError):
        Error(code=cast(ErrorCode, -32099), message="not one of them")


# The console script, as `fastbrowse serve --stdio` runs it, without depending on where the script was installed.
COMMAND = [sys.executable, "-c", "from fastbrowse.cli import main; main()", "serve", "--stdio"]

INITIALIZE = b'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":1}}\n'


def _start(command: list[str] = COMMAND) -> subprocess.Popen[bytes]:
    return subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)


def _lines(stdout: bytes) -> list[dict[str, Any]]:
    """Every line the process wrote to stdout, each of which has to be a message."""
    assert stdout.endswith(b"\n") and b"\r" not in stdout
    return [json.loads(line) for line in stdout.splitlines()]


def test_the_command_answers_shutdown_and_exits_zero_while_its_input_is_still_open() -> None:
    with _start() as process:
        assert process.stdin is not None and process.stdout is not None
        process.stdin.write(INITIALIZE + b'{"jsonrpc":"2.0","id":2,"method":"shutdown"}\n')
        process.stdin.flush()

        assert process.wait(timeout=30) == 0
        replies = _lines(process.stdout.read())

    assert replies[0]["result"] == {"protocol_version": 1, "fastbrowse_version": version("fastbrowse")}
    assert replies[1] == {"jsonrpc": "2.0", "id": 2, "result": None}


def test_the_command_exits_zero_when_its_input_ends() -> None:
    with _start() as process:
        stdout, _ = process.communicate(INITIALIZE, timeout=30)

    assert process.returncode == 0
    assert [reply["id"] for reply in _lines(stdout)] == [1]


def test_a_request_line_over_64_kib_is_read_whole() -> None:
    # The id comes back in the reply, so a line cut short or split in two could not produce this one.
    long_id = "x" * 200_000
    request = {"jsonrpc": "2.0", "id": long_id, "method": "initialize", "params": {"protocol_version": 1}}

    with _start() as process:
        stdout, _ = process.communicate(json.dumps(request).encode() + b"\n", timeout=30)

    (reply,) = _lines(stdout)
    assert reply["id"] == long_id
    assert reply["result"]["protocol_version"] == 1


def test_a_write_to_stdout_from_inside_the_process_goes_to_stderr_and_not_into_the_stream() -> None:
    # What a stray `print` in a dependency and a C extension writing to descriptor 1 would each do, once the
    # transport holds the real stdout.
    script = """
import asyncio, os, sys
from fastbrowse import serve

transport = serve.stdio_transport()
print("printed by a dependency", flush=True)
os.write(1, b"written to descriptor 1\\n")
sys.exit(asyncio.run(serve.Server(transport).serve()))
"""
    with _start([sys.executable, "-c", script]) as process:
        stdout, stderr = process.communicate(INITIALIZE, timeout=30)

    assert process.returncode == 0
    assert [reply["id"] for reply in _lines(stdout)] == [1]
    assert b"printed by a dependency" in stderr
    assert b"written to descriptor 1" in stderr


def test_the_command_logs_to_stderr() -> None:
    with _start() as process:
        stdout, stderr = process.communicate(b'{"jsonrpc":"2.0","id":1,"method":"run/teleport"}\n', timeout=30)

    (reply,) = _lines(stdout)
    assert reply["error"]["code"] == -32601
    assert b"fastbrowse: WARNING serve: unknown method 'run/teleport'" in stderr


def test_serve_without_a_transport_is_a_usage_error() -> None:
    with _start(COMMAND[:-1]) as process:
        stdout, stderr = process.communicate(b"", timeout=30)

    assert process.returncode == 2
    assert stdout == b""
    assert b"--stdio" in stderr
