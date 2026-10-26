"""The fixture runner reads transient provider failures off the wire, where it can see the Rust core's retries."""

import time

import httpx
import pytest

from fastbrowse.evals.runner import _TransientSpans
from fastbrowse.telemetry import transient_seconds


async def test_a_retried_request_counts_from_its_failure_to_the_answer() -> None:
    answers = iter([httpx.Response(503), httpx.ConnectError("refused"), httpx.Response(200), httpx.Response(200)])

    def handle(request: httpx.Request) -> httpx.Response:
        answer = next(answers)
        if isinstance(answer, Exception):
            raise answer
        return answer

    transport = _TransientSpans(httpx.MockTransport(handle))
    began = time.monotonic()
    async with httpx.AsyncClient(transport=transport) as http:
        assert (await http.post("https://llm.example/v1")).status_code == 503
        with pytest.raises(httpx.ConnectError):
            await http.post("https://llm.example/v1")
        assert (await http.post("https://llm.example/v1")).status_code == 200
        assert (await http.post("https://jev.example/v1")).status_code == 200
    ended = time.monotonic()
    spans = transport.finish(ended)
    assert len(spans) == 1
    assert 0 < transient_seconds(spans, began, ended) <= ended - began


async def test_a_failure_nothing_answered_counts_to_the_end_of_the_run() -> None:
    transport = _TransientSpans(httpx.MockTransport(lambda _: httpx.Response(429)))
    began = time.monotonic()
    async with httpx.AsyncClient(transport=transport) as http:
        await http.post("https://llm.example/v1")
    ended = time.monotonic() + 1.0
    assert transient_seconds(transport.finish(ended), began, ended) >= 1.0
