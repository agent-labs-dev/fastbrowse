"""The fixture runner reads transient provider failures off the wire, where it can see the Rust core's retries."""

import asyncio
import time
from collections.abc import AsyncIterator, Callable

import httpx
import pytest

from fastbrowse.evals.runner import _TransientSpans
from fastbrowse.telemetry import transient_seconds


class _Broken(httpx.AsyncByteStream):
    """A body that fails after its status arrived."""

    async def __aiter__(self) -> AsyncIterator[bytes]:
        yield b"{"
        raise httpx.ReadError("connection reset")


def scripted(*answers: httpx.Response | Exception) -> Callable[[httpx.Request], httpx.Response]:
    queue = list(answers)

    def handle(_: httpx.Request) -> httpx.Response:
        answer = queue.pop(0)
        if isinstance(answer, Exception):
            raise answer
        return answer

    return handle


async def lost(transport: _TransientSpans, began: float, extra: float = 0.0) -> float:
    ended = time.monotonic() + extra
    return transient_seconds(transport.finish(ended), began, ended)


async def test_a_retried_call_counts_from_its_failure_to_the_attempt_that_is_answered() -> None:
    transport = _TransientSpans(
        httpx.MockTransport(scripted(httpx.Response(503), httpx.ConnectError("refused"), httpx.Response(200)))
    )
    async with httpx.AsyncClient(transport=transport) as http:
        assert (await http.post("https://llm.example/v1", content=b"plan")).status_code == 503
        with pytest.raises(httpx.ConnectError):
            await http.post("https://llm.example/v1", content=b"plan")
        assert (await http.post("https://llm.example/v1", content=b"plan")).status_code == 200
    assert len(transport.finish(time.monotonic())) == 1


async def test_a_call_beside_a_failing_one_does_not_close_its_span() -> None:
    transport = _TransientSpans(httpx.MockTransport(scripted(httpx.Response(429), httpx.Response(200))))
    began = time.monotonic()
    async with httpx.AsyncClient(transport=transport) as http:
        await http.post("https://llm.example/v1", content=b"read")
        await http.post("https://llm.example/v1", content=b"plan")
    assert await lost(transport, began, extra=1.0) >= 1.0


async def test_a_body_that_fails_after_its_status_is_a_transient_failure() -> None:
    transport = _TransientSpans(httpx.MockTransport(lambda _: httpx.Response(200, stream=_Broken())))
    began = time.monotonic()
    async with httpx.AsyncClient(transport=transport) as http:
        with pytest.raises(httpx.ReadError):
            await http.post("https://llm.example/v1", content=b"plan")
    assert await lost(transport, began, extra=1.0) >= 1.0


async def test_an_attempt_cut_off_counts_once_the_call_goes_out_again() -> None:
    answered = asyncio.Event()

    async def handle(request: httpx.Request) -> httpx.Response:
        if not answered.is_set():
            answered.set()
            await asyncio.sleep(10)
        return httpx.Response(200)

    transport = _TransientSpans(httpx.MockTransport(handle))
    began = time.monotonic()
    async with httpx.AsyncClient(transport=transport) as http:
        with pytest.raises(TimeoutError):
            async with asyncio.timeout(0.2):
                await http.post("https://llm.example/v1", content=b"plan")
        await http.post("https://llm.example/v1", content=b"plan")
    assert await lost(transport, began) >= 0.2


async def test_a_call_the_run_cut_off_and_never_sent_again_costs_nothing() -> None:
    async def handle(_: httpx.Request) -> httpx.Response:
        await asyncio.sleep(10)
        return httpx.Response(200)

    transport = _TransientSpans(httpx.MockTransport(handle))
    began = time.monotonic()
    async with httpx.AsyncClient(transport=transport) as http:
        with pytest.raises(TimeoutError):
            async with asyncio.timeout(0.2):
                await http.post("https://llm.example/v1", content=b"shortcut")
    assert await lost(transport, began, extra=1.0) == 0.0
