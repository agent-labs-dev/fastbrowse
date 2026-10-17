"""Every dispatched model call leaves its span in the trace, however it ended: a failed or cancelled call is billed
too, and missing from the trace it made that time look free."""

import asyncio

import httpx
import pytest

from fastbrowse.clients.openai_compatible import OpenAICompatibleLLM
from fastbrowse.clients.validation import post
from fastbrowse.llm import LLMError, Message
from fastbrowse.models import Frozen, LLMPurpose
from fastbrowse.telemetry import traced


class _Answer(Frozen):
    value: str


def _spans(events: list[object]) -> list[tuple[str, str]]:
    spans = [e for e in events if isinstance(e, dict) and e["event"] == "call"]
    assert all(span["ended"] >= span["began"] for span in spans)
    return [(span["call"], span["outcome"]) for span in spans]


async def _hang(_: httpx.Request) -> httpx.Response:
    await asyncio.Event().wait()
    raise AssertionError("the call should have been cancelled")


async def _cancelled(call) -> None:
    task = asyncio.create_task(call)
    await asyncio.sleep(0.01)
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task


async def test_llm_calls_are_traced_when_they_fail_or_are_cancelled(monkeypatch: pytest.MonkeyPatch) -> None:
    # A cancelled call settles its in-flight attempt at the attempt deadline, so the deadline is kept short.
    monkeypatch.setattr("fastbrowse.clients.openai_compatible.LLM_ATTEMPT_SECONDS", 0.02)

    def broken(_: httpx.Request) -> httpx.Response:
        return httpx.Response(200, json={"choices": [{"message": {"content": "not json"}}], "usage": {"cost": 0.0}})

    messages = [Message(role="user", content="Answer.")]
    with traced() as events:
        async with httpx.AsyncClient(transport=httpx.MockTransport(broken)) as http:
            llm = OpenAICompatibleLLM("key", http=http, base_url="https://llm.test", models={LLMPurpose.READ: "m"})
            with pytest.raises(LLMError):
                await llm.generate(LLMPurpose.READ, messages, _Answer)
        async with httpx.AsyncClient(transport=httpx.MockTransport(_hang)) as http:
            llm = OpenAICompatibleLLM("key", http=http, base_url="https://llm.test", models={LLMPurpose.READ: "m"})
            await _cancelled(llm.generate(LLMPurpose.READ, messages, _Answer))
    assert _spans(events) == [("llm:read", "error"), ("llm:read", "cancelled")]


async def test_jev_calls_are_traced_when_they_fail_or_are_cancelled() -> None:
    with traced() as events:
        async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(401))) as http:
            with pytest.raises(Exception, match="Jev request failed"):
                await post(http, "https://jev.test/v1", "key", {})
        async with httpx.AsyncClient(transport=httpx.MockTransport(_hang)) as http:
            await _cancelled(post(http, "https://jev.test/v1", "key", {}))
        async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(200, json={}))) as http:
            await post(http, "https://jev.test/v1", "key", {})
    assert _spans(events) == [("jev", "error"), ("jev", "cancelled"), ("jev", "ok")]
