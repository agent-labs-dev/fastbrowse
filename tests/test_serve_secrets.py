"""Secrets over the serve protocol: the refs go to the server, and a value comes back only when it is typed.

`run_task` is replaced by a stand-in that resolves secrets the way a run does when it types one, and the fake
client answers `secrets/resolve` the way the JavaScript SDK does from the caller's vault.
"""

import json
import logging
import subprocess
import sys
from collections.abc import Sequence
from dataclasses import dataclass, field
from typing import Any

import pytest

from fastbrowse.models import BrowserEvent, EventHandler, RunResult, SecretRef, SecretResolver, StepEvent
from fastbrowse.serve import ClientError
from tests.serve_client import FakeClient, serving
from tests.test_serve_run import _command, _environment, _result, _run, _settings, _step

SHOP = "https://shop.example.com"
PASSWORD = {"name": "SHOP_PASSWORD", "origins": [SHOP]}
VALUE = "hunter2-correct-horse"


class Typist:
    """Stands where `run_task` does, and resolves each `(name, origin)` it was given, in order.

    It sends an event after each one and answers with how many resolved, so a value that leaked into what a
    run reports would have somewhere to show up.
    """

    def __init__(self, *typed: tuple[str, str]) -> None:
        self._typed = typed
        self.secrets: SecretResolver | None = None
        self.resolved: list[str | None] = []

    async def __call__(
        self, task: str, *, secrets: SecretResolver | None, on_event: EventHandler, **_: Any
    ) -> RunResult:
        self.secrets = secrets
        await on_event(BrowserEvent(live_url=None))
        for index, (name, origin) in enumerate(self._typed, start=1):
            assert secrets is not None
            self.resolved.append(await secrets.resolve(name, origin))
            await on_event(StepEvent(step=_step(index)))
        return _result(answer=f"typed {sum(value is not None for value in self.resolved)} of {len(self._typed)}")


class Catching(Typist):
    """A typist that keeps what resolving raised, where a run would end on it."""

    def __init__(self, *typed: tuple[str, str]) -> None:
        super().__init__(*typed)
        self.raised: list[ClientError] = []

    async def __call__(self, task: str, **arguments: Any) -> RunResult:
        try:
            return await super().__call__(task, **arguments)
        except ClientError as exc:
            self.raised.append(exc)
            return _result()


@dataclass
class Exchange:
    """One run as the client saw it."""

    reply: dict[str, Any]
    asked: list[dict[str, Any]] = field(default_factory=list)
    """Every `secrets/resolve` request the server sent."""
    written: bytes = b""
    """Every line the server wrote, from the `run` to its reply."""


async def _exchange(client: FakeClient, params: dict[str, Any], answers: Sequence[Any] = ()) -> Exchange:
    """Send a `run` and answer each `secrets/resolve` with the next of `answers`, until the run's reply."""
    run = await client.call("run", params)
    exchange = Exchange(reply={})
    remaining = iter(answers)
    while True:
        line = await client.receive_line()
        exchange.written += line
        message = client.parse(line)
        if message.get("method") == "secrets/resolve":
            exchange.asked.append(message)
            await client.respond(message, next(remaining))
        elif "method" not in message and message.get("id") == run:
            exchange.reply = message
            return exchange


async def _asked(client: FakeClient) -> dict[str, Any]:
    """The server's next `secrets/resolve`, left unanswered."""
    while (message := await client.receive()).get("method") != "secrets/resolve":
        pass
    return message


async def test_declared_refs_reach_run_task_as_a_resolver_that_lists_them() -> None:
    typist = Typist()
    refs = [PASSWORD, {"name": "SHOP_TOTP", "origins": ["https://*.example.com", "https://login.example.net"]}]

    async with serving(runner=typist, settings=_settings) as client:
        exchange = await _exchange(client, _run(secrets=refs))

    assert exchange.reply["result"]["status"] == "complete"
    assert typist.secrets is not None
    assert typist.secrets.available() == (
        SecretRef(name="SHOP_PASSWORD", origins=(SHOP,)),
        SecretRef(name="SHOP_TOTP", origins=("https://*.example.com", "https://login.example.net")),
    )
    assert exchange.asked == []


async def test_a_run_that_declares_no_secrets_gets_no_resolver() -> None:
    typist = Typist()

    async with serving(runner=typist, settings=_settings) as client:
        exchange = await _exchange(client, _run())

    assert exchange.reply["result"]["status"] == "complete"
    assert typist.secrets is None


async def test_resolving_on_a_covered_origin_asks_the_client_and_returns_its_value() -> None:
    typist = Typist(("SHOP_PASSWORD", SHOP))

    async with serving(runner=typist, settings=_settings) as client:
        exchange = await _exchange(client, _run("run-7", secrets=[PASSWORD]), [VALUE])

    assert exchange.asked == [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "secrets/resolve",
            "params": {"run_id": "run-7", "name": "SHOP_PASSWORD", "origin": SHOP},
        }
    ]
    assert typist.resolved == [VALUE]


async def test_a_wildcard_origin_covers_the_hosts_under_it() -> None:
    typist = Typist(("SHOP_PASSWORD", "https://accounts.example.com"))
    wildcard = {"name": "SHOP_PASSWORD", "origins": ["https://*.example.com"]}

    async with serving(runner=typist, settings=_settings) as client:
        exchange = await _exchange(client, _run(secrets=[wildcard]), [VALUE])

    assert [asked["params"]["origin"] for asked in exchange.asked] == ["https://accounts.example.com"]
    assert typist.resolved == [VALUE]


@pytest.mark.parametrize(
    "origin",
    ["https://evil.test", "http://shop.example.com", "https://shop.example.com:8443", "https://example.com"],
    ids=["another site", "another scheme", "another port", "the parent host"],
)
async def test_an_origin_the_ref_does_not_cover_resolves_to_nothing_and_the_client_is_not_asked(origin: str) -> None:
    typist = Typist(("SHOP_PASSWORD", origin))

    async with serving(runner=typist, settings=_settings) as client:
        exchange = await _exchange(client, _run(secrets=[PASSWORD]))

    assert typist.resolved == [None]
    assert exchange.asked == []


async def test_a_wildcard_does_not_cover_a_host_that_only_ends_with_the_same_letters() -> None:
    typist = Typist(("SHOP_PASSWORD", "https://example.com.evil.test"), ("SHOP_PASSWORD", "https://notexample.com"))
    wildcard = {"name": "SHOP_PASSWORD", "origins": ["https://*.example.com"]}

    async with serving(runner=typist, settings=_settings) as client:
        exchange = await _exchange(client, _run(secrets=[wildcard]))

    assert typist.resolved == [None, None]
    assert exchange.asked == []


async def test_a_name_that_was_never_declared_resolves_to_nothing_and_the_client_is_not_asked() -> None:
    typist = Typist(("ADMIN_PASSWORD", SHOP))

    async with serving(runner=typist, settings=_settings) as client:
        exchange = await _exchange(client, _run(secrets=[PASSWORD]))

    assert typist.resolved == [None]
    assert exchange.asked == []


async def test_each_resolution_asks_again_so_a_one_time_code_is_fresh() -> None:
    typist = Typist(("SHOP_TOTP", SHOP), ("SHOP_TOTP", SHOP))
    totp = {"name": "SHOP_TOTP", "origins": [SHOP]}

    async with serving(runner=typist, settings=_settings) as client:
        exchange = await _exchange(client, _run(secrets=[totp]), ["492817", "030551"])

    assert [asked["params"]["name"] for asked in exchange.asked] == ["SHOP_TOTP", "SHOP_TOTP"]
    assert [asked["id"] for asked in exchange.asked] == [1, 2]
    assert typist.resolved == ["492817", "030551"]


async def test_a_null_reply_resolves_to_nothing() -> None:
    typist = Typist(("SHOP_PASSWORD", SHOP))

    async with serving(runner=typist, settings=_settings) as client:
        exchange = await _exchange(client, _run(secrets=[PASSWORD]), [None])

    assert len(exchange.asked) == 1
    assert typist.resolved == [None]


async def test_the_value_is_in_no_message_the_server_wrote_and_no_log_line(
    caplog: pytest.LogCaptureFixture, capfd: pytest.CaptureFixture[str]
) -> None:
    # The most the server could ever say: there is no debug mode, and this is what one would turn on.
    caplog.set_level(logging.DEBUG)
    typist = Typist(("SHOP_PASSWORD", SHOP), ("SHOP_PASSWORD", SHOP))

    async with serving(runner=typist, settings=_settings) as client:
        exchange = await _exchange(client, _run(secrets=[PASSWORD]), [VALUE, VALUE])
        # An answer nobody is waiting for any more, as after a run that was stopped with its question out.
        await client.send({"jsonrpc": "2.0", "id": 99, "result": VALUE})
        handshake = await client.request("initialize", {"protocol_version": 1})

    assert typist.resolved == [VALUE, VALUE]
    assert exchange.reply["result"]["answer"] == "typed 2 of 2"
    # The events and the reply sit between the questions and after them, and none of them carries it.
    assert exchange.written.count(b"run/event") == 3
    assert client.inbox == []
    captured = capfd.readouterr()
    for written in (exchange.written.decode(), json.dumps(handshake), caplog.text, captured.err, captured.out):
        assert VALUE not in written


async def test_an_answer_to_no_request_gets_no_reply() -> None:
    async with serving(runner=Typist(), settings=_settings) as client:
        await client.send({"jsonrpc": "2.0", "id": 99, "result": VALUE})
        await client.send({"jsonrpc": "2.0", "id": 98, "error": {"code": -32000, "message": "no vault"}})
        handshake = await client.request("initialize", {"protocol_version": 1})

    assert handshake["result"]["protocol_version"] == 1
    assert client.inbox == []


async def test_an_error_reply_raises_where_the_value_was_awaited() -> None:
    typist = Catching(("SHOP_PASSWORD", SHOP))

    async with serving(runner=typist, settings=_settings) as client:
        run = await client.call("run", _run(secrets=[PASSWORD]))
        await client.refuse(await _asked(client), -32000, "the vault is locked")
        reply = await client.reply(run)

    assert reply["result"]["status"] == "complete"
    (raised,) = typist.raised
    assert (raised.method, raised.code) == ("secrets/resolve", -32000)
    assert str(raised) == "secrets/resolve: the client could not resolve the secret 'SHOP_PASSWORD'"


@pytest.mark.parametrize(
    "answer", [{"value": VALUE}, [VALUE], 8675309, True], ids=["an object", "a list", "a number", "a boolean"]
)
async def test_a_reply_that_is_neither_text_nor_null_raises_without_repeating_it(
    answer: Any, caplog: pytest.LogCaptureFixture
) -> None:
    caplog.set_level(logging.DEBUG)
    typist = Catching(("SHOP_PASSWORD", SHOP))

    async with serving(runner=typist, settings=_settings) as client:
        exchange = await _exchange(client, _run(secrets=[PASSWORD]), [answer])

    (raised,) = typist.raised
    assert raised.method == "secrets/resolve" and raised.code is None
    assert typist.resolved == []
    for written in (str(raised), exchange.written.decode(), caplog.text):
        assert VALUE not in written and "8675309" not in written


async def test_the_server_answers_other_requests_while_a_value_is_awaited() -> None:
    typist = Typist(("SHOP_PASSWORD", SHOP))

    async with serving(runner=typist, settings=_settings) as client:
        run = await client.call("run", _run(secrets=[PASSWORD]))
        asked = await _asked(client)
        handshake = await client.result("initialize", {"protocol_version": 1})
        await client.respond(asked, VALUE)
        reply = await client.reply(run)

    assert handshake["protocol_version"] == 1
    assert reply["result"]["status"] == "complete"
    assert typist.resolved == [VALUE]


async def test_input_ending_while_a_value_is_awaited_stops_the_server() -> None:
    typist = Typist(("SHOP_PASSWORD", SHOP))

    async with serving(runner=typist, settings=_settings) as client:
        await client.call("run", _run(secrets=[PASSWORD]))
        await _asked(client)
        await client.close_input()

        assert await client.exit_code() == 0
    assert typist.resolved == []


@pytest.mark.parametrize(
    ("secrets", "named"),
    [
        ([{"name": "SHOP_PASSWORD"}], "secrets.0.origins"),
        ([{"name": "SHOP_PASSWORD", "origins": [SHOP], "value": VALUE}], "secrets.0.value"),
        ({"SHOP_PASSWORD": VALUE}, "secrets"),
    ],
    ids=["a ref with no origins", "a ref carrying a value", "values by name"],
)
async def test_secrets_that_are_not_refs_are_refused_without_repeating_them(secrets: Any, named: str) -> None:
    typist = Typist()

    async with serving(runner=typist, settings=_settings) as client:
        response = await client.request("run", _run(secrets=secrets))

    assert response["error"]["code"] == -32602
    assert named in response["error"]["message"]
    assert VALUE not in json.dumps(response)
    assert typist.secrets is None


def test_the_served_command_keeps_the_value_out_of_stdout_and_stderr() -> None:
    request = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "run",
        "params": _run(local=True, chrome={"binary": sys.executable}, start=f"{SHOP}/login", secrets=[PASSWORD]),
    }

    with subprocess.Popen(
        _command("--stdio", "--run-task", "tests.serve_scripts:type_secret"),
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=_environment(),
    ) as process:
        assert process.stdin is not None and process.stdout is not None and process.stderr is not None
        process.stdin.write(json.dumps(request).encode() + b"\n")
        process.stdin.flush()
        asked = process.stdout.readline()
        answer = {"jsonrpc": "2.0", "id": json.loads(asked)["id"], "result": VALUE}
        process.stdin.write(json.dumps(answer).encode() + b"\n")
        process.stdin.flush()
        event, reply = process.stdout.readline(), process.stdout.readline()
        process.stdin.close()
        rest, stderr = process.stdout.read(), process.stderr.read()
        assert process.wait(timeout=30) == 0

    assert json.loads(asked)["params"] == {"run_id": "run-1", "name": "SHOP_PASSWORD", "origin": SHOP}
    # The stand-in's own proof that the value reached it, in a form that is not the value.
    assert json.loads(reply)["result"]["answer"] == f"typed {len(VALUE)} characters"
    assert json.loads(event)["method"] == "run/event"
    assert VALUE.encode() not in asked + event + reply + rest + stderr
