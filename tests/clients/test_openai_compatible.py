import asyncio
import base64

import httpx
import pytest
from pydantic import JsonValue, TypeAdapter

from fastbrowse.clients import validation
from fastbrowse.clients.openai_compatible import OpenAICompatibleLLM
from fastbrowse.llm import LLMError, Message
from fastbrowse.models import CostBasis, Frozen, Limits, LLMPurpose
from fastbrowse.telemetry import BudgetExceeded, Ledger


class Result(Frozen):
    count: int


async def test_schema_repair_keeps_images_and_accounts_for_both_calls() -> None:
    requests: list[dict[str, JsonValue]] = []
    png = b"\x89PNG\r\n\x1a\nimage"
    jpeg = b"\xff\xd8\xffimage"

    def handler(request: httpx.Request) -> httpx.Response:
        assert str(request.url) == "https://llm.test/v1/chat/completions"
        assert request.headers["authorization"] == "Bearer key"
        requests.append(TypeAdapter(dict[str, JsonValue]).validate_json(request.content))
        return httpx.Response(
            200,
            json={
                "choices": [{"message": {"content": '{"count":"wrong"}' if len(requests) == 1 else '{"count":3}'}}],
                "usage": {"prompt_tokens": 20, "completion_tokens": 5, "cost": 0.001},
            },
        )

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        result = await OpenAICompatibleLLM(
            "key", http=http, base_url="https://llm.test/v1/", models={LLMPurpose.READ: "reader"}
        ).generate(
            LLMPurpose.READ, [Message(role="user", content="Count", images=(png, jpeg))], Result, max_output_tokens=42
        )
    assert result.data.count == 3
    assert result.cost.basis is CostBasis.METERED
    assert result.cost.dollars == 0.002
    assert (result.cost.input_tokens, result.cost.output_tokens) == (40, 10)
    assert result.cost.purpose is LLMPurpose.READ
    body = requests[0]
    assert body["model"] == "reader" and body["max_tokens"] == 42
    assert body["response_format"] == {
        "type": "json_schema",
        "json_schema": {"name": "Result", "schema": Result.model_json_schema(), "strict": False},
    }
    messages = body["messages"]
    assert isinstance(messages, list)
    assert messages[0] == {
        "role": "user",
        "content": [
            {"type": "text", "text": "Count"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64," + base64.b64encode(png).decode()}},
            {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64," + base64.b64encode(jpeg).decode()}},
        ],
    }
    retry = requests[1]["messages"]
    assert isinstance(retry, list)
    assert len(retry) == 3
    assert "Validation errors" in str(retry[-1]) and "count" in str(retry[-1])


@pytest.mark.parametrize("first_cost,second_cost", [(None, None), (0.01, None), (None, 0.02)])
async def test_unknown_price_retains_all_token_counts(first_cost: float | None, second_cost: float | None) -> None:
    calls = 0

    def handler(_: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        return httpx.Response(
            200,
            json={
                "choices": [{"message": {"content": "not json" if calls == 1 else '{"count":0}'}}],
                "usage": {
                    "prompt_tokens": 7,
                    "completion_tokens": 2,
                    "cost": first_cost if calls == 1 else second_cost,
                },
            },
        )

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        result = await OpenAICompatibleLLM(
            "key", http=http, base_url="https://llm.test", models={LLMPurpose.PLAN: "planner"}
        ).generate(LLMPurpose.PLAN, [], Result)
    assert result.cost.basis is CostBasis.UNKNOWN and result.cost.dollars is None
    assert (result.cost.input_tokens, result.cost.output_tokens) == (14, 4)


async def test_exactly_one_retry_then_llm_error() -> None:
    calls = 0

    def handler(_: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        return httpx.Response(200, json={"choices": [{"message": {"content": '{"wrong":1}'}}]})

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        with pytest.raises(LLMError, match="after one retry"):
            await OpenAICompatibleLLM(
                "key", http=http, base_url="https://llm.test", models={LLMPurpose.PLAN: "planner"}
            ).generate(LLMPurpose.PLAN, [], Result)
    assert calls == 2


async def test_invalid_image_is_not_silently_dropped() -> None:
    def handler(_: httpx.Request) -> httpx.Response:
        pytest.fail("Unsupported image must fail before sending")  # ty: ignore[invalid-argument-type]

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        with pytest.raises(LLMError, match="PNG and JPEG"):
            await OpenAICompatibleLLM(
                "key", http=http, base_url="https://llm.test", models={LLMPurpose.PLAN: "planner"}
            ).generate(LLMPurpose.PLAN, [Message(role="user", content="See image", images=(b"GIF89a",))], Result)


async def test_an_overloaded_model_is_retried_then_reported_without_a_schema_repair(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A 429 is a transport condition: repeat it, then report it rather than asking to fix the JSON."""
    monkeypatch.setattr(validation, "RETRY_DELAYS_SECONDS", (0.0, 0.0))
    calls = 0

    def handler(_: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        return httpx.Response(429, text="rate limited" + "x" * 2000)

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        with pytest.raises(LLMError, match="429") as error:
            await OpenAICompatibleLLM(
                "key", http=http, base_url="https://llm.test", models={LLMPurpose.PLAN: "planner"}
            ).generate(LLMPurpose.PLAN, [], Result)
    assert calls == 3 and len(str(error.value)) < 500


async def test_a_retry_reserves_its_own_call_and_keeps_an_unreadable_attempts_cost() -> None:
    """A re-ask is a second billable generation: it must spend budget and never lose its cost."""
    requests = 0

    def handler(_: httpx.Request) -> httpx.Response:
        nonlocal requests
        requests += 1
        if requests <= 2:  # the budgeted call's only attempt, then the unbudgeted call's first
            return httpx.Response(200, json={"choices": [], "usage": {"cost": 0.02, "prompt_tokens": "invalid"}})
        return httpx.Response(
            200,
            json={"choices": [{"message": {"content": '{"count":1}'}}], "usage": {"cost": 0.01}},
        )

    ledger = Ledger(Limits(max_llm_calls=1))
    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        client = OpenAICompatibleLLM(
            "key", http=http, base_url="https://llm.test/v1/", models={LLMPurpose.READ: "reader"}
        )
        with pytest.raises(BudgetExceeded):
            await client.generate(LLMPurpose.READ, [Message(role="user", content="Count")], Result, ledger=ledger)
        assert requests == 1

        result = await client.generate(
            LLMPurpose.READ, [Message(role="user", content="Count")], Result, ledger=Ledger(Limits())
        )
    # The first attempt's usage could not be read, so the total cannot claim to be metered.
    assert requests == 3
    assert result.data.count == 1
    assert result.cost.basis is CostBasis.UNKNOWN
    assert result.cost.dollars is None


async def test_a_hedge_loser_is_charged_as_the_winner(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("fastbrowse.clients.openai_compatible.LLM_HEDGE_SECONDS", 0.01)
    calls = 0
    ledger = Ledger(Limits(max_llm_calls=2, max_dollars=1.0))

    async def handler(_: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        if calls == 1:
            await asyncio.Event().wait()
        return httpx.Response(
            200,
            json={
                "choices": [{"message": {"content": '{"count":1}'}}],
                "usage": {"cost": 0.01, "prompt_tokens": 20, "completion_tokens": 5},
            },
        )

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        result = await OpenAICompatibleLLM(
            "key", http=http, base_url="https://llm.test", models={LLMPurpose.READ: "reader"}
        ).generate(LLMPurpose.READ, [], Result, ledger=ledger)
    ledger.record(result.cost)
    assert calls == ledger.llm_calls == 2
    assert result.data.count == 1
    assert (
        result.cost.basis is CostBasis.ESTIMATED
        and result.cost.dollars is not None
        and round(result.cost.dollars, 6) == 0.02
    )
    assert not ledger.breakdown().has_unknown
    ledger.check(0.0)


async def test_a_hedge_loser_is_recorded_even_when_the_winner_fails(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("fastbrowse.clients.openai_compatible.LLM_HEDGE_SECONDS", 0.01)
    calls = 0
    ledger = Ledger(Limits())

    async def handler(_: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        if calls == 1:
            await asyncio.Event().wait()
        return httpx.Response(401)

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        with pytest.raises(LLMError, match="401"):
            await OpenAICompatibleLLM(
                "key", http=http, base_url="https://llm.test", models={LLMPurpose.READ: "reader"}
            ).generate(LLMPurpose.READ, [], Result, ledger=ledger)
    assert calls == 2 and len(ledger.lines) == 1
    assert ledger.breakdown().has_unknown


async def test_an_error_status_retry_does_not_add_llm_cost() -> None:
    responses = iter(
        [
            httpx.Response(503),
            httpx.Response(
                200,
                json={"choices": [{"message": {"content": '{"count":1}'}}], "usage": {"cost": 0.01}},
            ),
        ]
    )
    ledger = Ledger(Limits(max_llm_calls=2))
    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: next(responses))) as http:
        result = await OpenAICompatibleLLM(
            "key", http=http, base_url="https://llm.test", models={LLMPurpose.READ: "reader"}
        ).generate(LLMPurpose.READ, [], Result, ledger=ledger)
    ledger.record(result.cost)
    assert ledger.llm_calls == 2
    assert result.cost.basis is CostBasis.METERED and result.cost.dollars == 0.01
    assert not ledger.breakdown().has_unknown
