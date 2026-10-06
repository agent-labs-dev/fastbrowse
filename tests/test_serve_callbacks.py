"""Frames, the `until` check and a failing client callback over the serve protocol.

`run_task` is replaced by a stand-in that calls what it was handed the way a run does: the frame handler as
the page is drawn, the `until` check with the last address once the run would otherwise be accepted.
"""

import base64
import logging
from typing import Any

import pytest

from fastbrowse.models import BrowserEvent, EventHandler, FrameHandler, RunResult, Status, UntilCheck
from tests.serve_client import FakeClient, serving
from tests.test_serve_run import _result, _run, _settings
from tests.test_serve_secrets import PASSWORD, SHOP, VALUE, Typist, _asked, _exchange

# Two JPEG headers with bytes after them that are not text, so a frame written as anything but base64 fails.
FRAMES = (b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\xfb\xff", b"\xff\xd8\xff\xe1\x00\x00\xfe\xfa")
ORDERS = "https://shop.example.com/orders/1042"


class Browsing:
    """Stands where `run_task` does: it draws `frames`, then finishes on `final_url`.

    Like a run, it is `complete` only when the `until` check it was given accepts the address it ended on.
    """

    def __init__(self, *, frames: tuple[bytes, ...] = (), final_url: str = ORDERS) -> None:
        self._frames = frames
        self._final_url = final_url
        self.on_frame: FrameHandler | None = None
        self.until: UntilCheck | None = None
        self.closed = False
        """Whether the run has let go of its browser, which a real one does on every way out."""

    async def __call__(
        self,
        task: str,
        *,
        on_event: EventHandler,
        on_frame: FrameHandler | None = None,
        until: UntilCheck | None = None,
        **_: Any,
    ) -> RunResult:
        self.on_frame, self.until = on_frame, until
        try:
            await on_event(BrowserEvent(live_url=None))
            for frame in self._frames:
                assert on_frame is not None
                await on_frame(frame)
            accepted = until is None or await until(self._final_url)
            return _result(Status.COMPLETE if accepted else Status.UNVERIFIED, final_url=self._final_url)
        finally:
            self.closed = True


async def test_frames_arrive_as_notifications_carrying_the_run_id_when_the_run_asks_for_them() -> None:
    browsing = Browsing(frames=FRAMES)

    async with serving(runner=browsing, settings=_settings) as client:
        response = await client.request("run", _run("run-7", frames=True))

    assert response["result"]["status"] == "complete"
    sent = [message for message in client.inbox if message["method"] == "run/frame"]
    assert all("id" not in message for message in sent)
    assert [message["params"]["run_id"] for message in sent] == ["run-7", "run-7"]
    assert tuple(base64.b64decode(message["params"]["frame"], validate=True) for message in sent) == FRAMES


async def test_a_run_that_does_not_ask_for_frames_gets_no_frame_handler_and_no_frame_is_written() -> None:
    browsing = Browsing()

    async with serving(runner=browsing, settings=_settings) as client:
        response = await client.request("run", _run())

    assert response["result"]["status"] == "complete"
    assert browsing.on_frame is None
    assert [message["method"] for message in client.inbox] == ["run/event"]


async def _asked_until(client: FakeClient) -> dict[str, Any]:
    """The server's next `run/until`, left unanswered."""
    while (message := await client.receive()).get("method") != "run/until":
        pass
    return message


async def test_a_client_that_holds_a_check_is_asked_with_the_final_url_and_its_yes_lets_the_run_complete() -> None:
    browsing = Browsing(final_url=ORDERS)

    async with serving(runner=browsing, settings=_settings) as client:
        run = await client.call("run", _run("run-7", until=True))
        asked = await _asked_until(client)
        await client.respond(asked, True)
        reply = await client.reply(run)

    assert asked == {"jsonrpc": "2.0", "id": 1, "method": "run/until", "params": {"run_id": "run-7", "url": ORDERS}}
    assert reply["result"]["status"] == "complete"


async def test_a_no_from_the_clients_check_keeps_the_run_from_complete() -> None:
    browsing = Browsing()

    async with serving(runner=browsing, settings=_settings) as client:
        run = await client.call("run", _run(until=True))
        await client.respond(await _asked_until(client), False)
        reply = await client.reply(run)

    assert reply["result"]["status"] == "unverified"


async def test_a_run_whose_client_holds_no_check_gets_none_and_is_not_asked() -> None:
    browsing = Browsing()

    async with serving(runner=browsing, settings=_settings) as client:
        response = await client.request("run", _run(until=False))

    assert response["result"]["status"] == "complete"
    assert browsing.until is None
    assert [message["method"] for message in client.inbox] == ["run/event"]


ERRORED = {
    "status": "error",
    "budget": None,
    "answer": None,
    "data": None,
    "evidence": [],
    "steps": [],
    "cost": {"lines": []},
    "artifacts": [],
    "citations": [],
    "final_url": None,
    "would_fire": [],
    "recordings": [],
}
"""A run that a client's callback ended, apart from its `error`: nothing the run did before then is in it."""


async def test_an_error_reply_to_the_check_ends_the_run_as_an_error_result_after_its_browser_closed() -> None:
    browsing = Browsing()

    async with serving(runner=browsing, settings=_settings) as client:
        run = await client.call("run", _run(until=True))
        await client.refuse(await _asked_until(client), -32000, "orders page never loaded")
        reply = await client.reply(run)
        closed_by_the_reply = browsing.closed
        # The run is over, so the server takes the next one.
        after = await client.request("run", _run("run-2"))

    assert "error" not in reply
    assert reply["result"] == ERRORED | {"error": "run/until: orders page never loaded"}
    assert closed_by_the_reply
    assert after["result"]["status"] == "complete"


@pytest.mark.parametrize("answer", [None, "yes", 1, {"ok": True}], ids=["null", "text", "a number", "an object"])
async def test_a_reply_to_the_check_that_is_not_a_boolean_ends_the_run_as_an_error_result(answer: Any) -> None:
    browsing = Browsing()

    async with serving(runner=browsing, settings=_settings) as client:
        run = await client.call("run", _run(until=True))
        await client.respond(await _asked_until(client), answer)
        reply = await client.reply(run)

    assert reply["result"]["status"] == "error"
    assert reply["result"]["error"].startswith("run/until: the result cannot be used: ")


async def test_an_error_reply_to_a_secret_ends_the_run_as_an_error_result() -> None:
    typist = Typist(("SHOP_PASSWORD", SHOP))

    async with serving(runner=typist, settings=_settings) as client:
        run = await client.call("run", _run(secrets=[PASSWORD]))
        await client.refuse(await _asked(client), -32000, "the vault is locked")
        reply = await client.reply(run)

    assert reply["result"] == ERRORED | {"error": "secrets/resolve: the vault is locked"}


async def test_a_secret_reply_of_the_wrong_shape_ends_the_run_as_an_error_result_that_does_not_repeat_it(
    caplog: pytest.LogCaptureFixture,
) -> None:
    caplog.set_level(logging.DEBUG)
    typist = Typist(("SHOP_PASSWORD", SHOP))

    async with serving(runner=typist, settings=_settings) as client:
        exchange = await _exchange(client, _run(secrets=[PASSWORD]), [{"value": VALUE}])

    assert exchange.reply["result"]["status"] == "error"
    assert exchange.reply["result"]["error"].startswith("secrets/resolve: the result cannot be used: ")
    assert VALUE not in exchange.written.decode() + caplog.text


async def test_a_run_that_fails_on_its_own_is_still_an_internal_error_and_not_a_result() -> None:
    async def broken(task: str, **_: Any) -> RunResult:
        raise ValueError("no such page")

    async with serving(runner=broken, settings=_settings) as client:
        response = await client.request("run", _run(until=True, frames=True))

    assert response["error"] == {"code": -32603, "message": "ValueError: no such page"}
