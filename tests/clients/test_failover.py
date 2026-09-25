import asyncio
from collections import Counter
from typing import Any

import httpx
import pytest

from fastbrowse.clients.environment import JevSource, Settings
from fastbrowse.clients.validation import RETRY_DELAYS_SECONDS, RETRYABLE_STATUS
from fastbrowse.jev import JEV_MODEL, JevError, JevInputTooLarge, JevRetriesExhausted, NoulAnswer, NoulQuestion
from fastbrowse.models import CostBasis

QUESTIONS = {"q": NoulQuestion(instructions="Is it?")}
ATTEMPTS = len(RETRY_DELAYS_SECONDS) + 1


def source(request: httpx.Request) -> JevSource:
    return JevSource.TYPESAFE if request.url.host == "api.typesafe.ai" else JevSource.GATEWAY


def answer(provider: JevSource) -> httpx.Response:
    if provider is JevSource.TYPESAFE:
        return httpx.Response(
            200,
            json={"answers": {"q": {"type": "noul", "noul": 1}}, "usage": {"input_tokens": 100, "output_tokens": 7}},
        )
    return httpx.Response(
        200,
        json={
            "answers": {"q": {"type": "boolean", "probability": 1}},
            "usage": {"inputTokens": 100, "outputTokens": 7},
            "providerMetadata": {"gateway": {"cost": "0.001"}},
        },
    )


def settings(primary: JevSource) -> Settings:
    values: dict[str, Any] = {
        "TYPESAFE_API_KEY": "direct-key",
        "AI_GATEWAY_API_KEY": "gateway-key",
        "jev_source": primary,
        "jev_base_url": None,
        "jev_model": JEV_MODEL,
    }
    return Settings(_env_file=None, **values)


@pytest.mark.parametrize("primary", JevSource)
@pytest.mark.parametrize("status", sorted(RETRYABLE_STATUS))
async def test_exhausted_primary_switches_once_and_each_run_starts_fresh(primary: JevSource, status: int) -> None:
    seen: list[JevSource] = []
    backup = JevSource.GATEWAY if primary is JevSource.TYPESAFE else JevSource.TYPESAFE

    def handler(request: httpx.Request) -> httpx.Response:
        provider = source(request)
        seen.append(provider)
        expected_key = "direct-key" if provider is JevSource.TYPESAFE else "gateway-key"
        assert request.headers["authorization"] == f"Bearer {expected_key}"
        return httpx.Response(status) if provider is primary else answer(provider)

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        configured = settings(primary)
        client = configured.jev(http)
        first = await client.evaluate("page", QUESTIONS)
        second = await client.evaluate("page", QUESTIONS)
        assert seen == [primary] * ATTEMPTS + [backup, backup]
        assert first.answers == second.answers == {"q": NoulAnswer(probability=1)}
        assert first.cost.dollars == second.cost.dollars
        await configured.jev(http).evaluate("page", QUESTIONS)
        assert seen == [primary] * ATTEMPTS + [backup, backup] + [primary] * ATTEMPTS + [backup]


@pytest.mark.parametrize("primary", JevSource)
async def test_simultaneous_failures_stay_on_backup(primary: JevSource) -> None:
    seen: Counter[JevSource] = Counter()
    both_started = asyncio.Event()

    async def handler(request: httpx.Request) -> httpx.Response:
        provider = source(request)
        seen[provider] += 1
        if provider is primary:
            if seen[provider] == 2:
                both_started.set()
            await both_started.wait()
            return httpx.Response(503)
        return answer(provider)

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        client = settings(primary).jev(http)
        results = await asyncio.gather(client.evaluate("first", QUESTIONS), client.evaluate("second", QUESTIONS))
        await client.evaluate("third", QUESTIONS)
    assert all(result.answers == {"q": NoulAnswer(probability=1)} for result in results)
    assert seen[primary] == 2 * ATTEMPTS
    assert sum(seen.values()) == 2 * ATTEMPTS + 3


@pytest.mark.parametrize("primary", JevSource)
@pytest.mark.parametrize("status", [400, 401, 403, 404, 422, 200])
async def test_request_errors_and_malformed_responses_do_not_switch(primary: JevSource, status: int) -> None:
    seen: list[JevSource] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(source(request))
        return httpx.Response(status, json={"error": "invalid"})

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        client = settings(primary).jev(http)
        for _ in range(2):
            with pytest.raises(JevError):
                await client.evaluate("page", QUESTIONS)
    assert seen == [primary, primary]


@pytest.mark.parametrize("primary", JevSource)
async def test_input_limits_and_transport_errors_do_not_switch(primary: JevSource) -> None:
    seen: list[JevSource] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(source(request))
        if len(seen) == 1:
            return httpx.Response(400, text="max_tokens_exceeded")
        raise httpx.ConnectError("connection dropped", request=request)

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        client = settings(primary).jev(http)
        with pytest.raises(JevInputTooLarge):
            await client.evaluate("page", QUESTIONS)
        with pytest.raises(JevError, match="transport failed") as error:
            await client.evaluate("page", QUESTIONS)
    assert not isinstance(error.value, JevRetriesExhausted)
    assert seen == [primary] * (1 + ATTEMPTS)


@pytest.mark.parametrize("primary", JevSource)
async def test_recovery_within_retry_budget_keeps_primary(primary: JevSource) -> None:
    seen: list[JevSource] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(source(request))
        return httpx.Response(503) if len(seen) < ATTEMPTS else answer(primary)

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        client = settings(primary).jev(http)
        await client.evaluate("page", QUESTIONS)
        await client.evaluate("page", QUESTIONS)
    assert seen == [primary] * (ATTEMPTS + 1)


@pytest.mark.parametrize("primary", JevSource)
async def test_backup_exhaustion_does_not_switch_back(primary: JevSource, monkeypatch: pytest.MonkeyPatch) -> None:
    seen: Counter[JevSource] = Counter()
    times = iter([0.0, 2.0, 2.0, 5.0, 5.0, 9.0])
    monkeypatch.setattr("fastbrowse.clients.validation.monotonic", lambda: next(times))

    def handler(request: httpx.Request) -> httpx.Response:
        provider = source(request)
        seen[provider] += 1
        if seen[provider] == 1:
            raise httpx.ReadTimeout("no answer", request=request)
        return httpx.Response(503 if provider is primary else 429)

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        client = settings(primary).jev(http)
        with pytest.raises(JevRetriesExhausted) as first:
            await client.evaluate("page", QUESTIONS)
        with pytest.raises(JevRetriesExhausted) as second:
            await client.evaluate("page", QUESTIONS)
    assert "HTTP 429" in str(first.value) and "HTTP 429" in str(second.value)
    assert first.value.seconds == 5
    assert first.value.unaccounted_requests == 2
    assert second.value.seconds == 4
    assert second.value.unaccounted_requests == 0
    assert seen[primary] == ATTEMPTS
    assert sum(seen.values()) == 3 * ATTEMPTS


@pytest.mark.parametrize("primary", JevSource)
@pytest.mark.parametrize("after_switch", [False, True])
async def test_cancellation_cleans_up_and_keeps_the_selected_provider(primary: JevSource, after_switch: bool) -> None:
    seen: list[JevSource] = []
    started = asyncio.Event()
    cancelled = asyncio.Event()

    async def handler(request: httpx.Request) -> httpx.Response:
        provider = source(request)
        seen.append(provider)
        if cancelled.is_set():
            return answer(provider)
        if after_switch and provider is primary:
            return httpx.Response(503)
        started.set()
        try:
            await asyncio.Event().wait()
        finally:
            cancelled.set()
        raise AssertionError("the request was not cancelled")

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        client = settings(primary).jev(http)
        pending = asyncio.create_task(client.evaluate("page", QUESTIONS))
        await started.wait()
        pending.cancel()
        with pytest.raises(asyncio.CancelledError):
            await pending
        assert cancelled.is_set()
        result = await client.evaluate("page", QUESTIONS)
    assert result.answers == {"q": NoulAnswer(probability=1)}
    backup = JevSource.GATEWAY if primary is JevSource.TYPESAFE else JevSource.TYPESAFE
    assert seen == ([primary] * ATTEMPTS + [backup, backup] if after_switch else [primary, primary])


@pytest.mark.parametrize("primary", JevSource)
async def test_failover_accounts_for_each_providers_unanswered_requests(
    primary: JevSource, monkeypatch: pytest.MonkeyPatch
) -> None:
    seen: Counter[JevSource] = Counter()
    clock = 0.0
    for module in ("validation", "typesafe", "vercel"):
        monkeypatch.setattr(f"fastbrowse.clients.{module}.monotonic", lambda: clock)

    def handler(request: httpx.Request) -> httpx.Response:
        nonlocal clock
        clock += 1
        provider = source(request)
        seen[provider] += 1
        if seen[provider] == 1:
            raise httpx.ReadTimeout("may have been billed", request=request)
        return httpx.Response(503) if provider is primary else answer(provider)

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        client = settings(primary).jev(http)
        first = await client.evaluate("page", QUESTIONS)
        second = await client.evaluate("page", QUESTIONS)
    assert first.cost.basis is CostBasis.ESTIMATED
    assert first.input_tokens == 100
    assert first.cost.input_tokens == 300
    assert second.cost.input_tokens == 100
    assert first.cost.output_tokens == second.cost.output_tokens == 7
    assert first.cost.seconds == ATTEMPTS + 2
    assert second.cost.seconds == 1
    assert second.cost.dollars is not None
    assert first.cost.dollars == pytest.approx(second.cost.dollars * 2 + 100 * 0.042 / 1_000_000)
