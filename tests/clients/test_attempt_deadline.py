"""Receiving occasional bytes must not keep a provider attempt alive forever."""

import asyncio

import httpx

from fastbrowse.clients.validation import RequestUsage, _send


async def test_trickling_response_is_bounded_by_attempt_deadline() -> None:
    closed = asyncio.Event()

    class TricklingBody(httpx.AsyncByteStream):
        async def __aiter__(self):
            try:
                while True:
                    await asyncio.sleep(0.01)
                    yield b" "
            finally:
                closed.set()

    async def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, stream=TricklingBody())

    usage = RequestUsage()
    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        async with asyncio.timeout(0.5):
            response = await _send(http, "https://provider.test", {}, {}, 0.05, usage)
    assert response is None
    assert usage.requests == 1
    assert "no response within" in usage.failures[0]
    assert closed.is_set()
