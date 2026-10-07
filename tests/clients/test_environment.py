import json
from typing import Any

import httpx
import pytest
from pydantic import JsonValue, TypeAdapter

from fastbrowse.clients.environment import DEFAULT_MODELS, ConfigurationError, JevSource, Settings
from fastbrowse.jev import JevError, JevRetriesExhausted, NoulQuestion
from fastbrowse.llm import Message
from fastbrowse.models import CostBasis, Frozen, LLMPurpose


@pytest.fixture(autouse=True)
def no_ambient_keys(monkeypatch: pytest.MonkeyPatch) -> None:
    for name in (
        "TYPESAFE_API_KEY",
        "AI_GATEWAY_API_KEY",
        "OPENROUTER_API_KEY",
        "FASTBROWSE_JEV_SOURCE",
        "FASTBROWSE_JEV_BASE_URL",
        "FASTBROWSE_JEV_MODEL",
    ):
        monkeypatch.delenv(name, raising=False)


def settings(**values: Any) -> Settings:
    return Settings(_env_file=None, **values)


@pytest.mark.parametrize(
    ("values", "expected_url"),
    [
        ({"OPENROUTER_API_KEY": "o"}, "https://openrouter.ai/api/v1/systemone"),
        (
            {"OPENROUTER_API_KEY": "o", "TYPESAFE_API_KEY": "t", "AI_GATEWAY_API_KEY": "g"},
            "https://api.typesafe.ai/v1/systemone",
        ),
        (
            {"OPENROUTER_API_KEY": "o", "TYPESAFE_API_KEY": "t", "jev_source": JevSource.OPENROUTER},
            "https://openrouter.ai/api/v1/systemone",
        ),
        ({"TYPESAFE_API_KEY": "t", "AI_GATEWAY_API_KEY": "g"}, "https://api.typesafe.ai/v1/systemone"),
        (
            {"TYPESAFE_API_KEY": "t", "AI_GATEWAY_API_KEY": "g", "jev_source": JevSource.GATEWAY},
            "https://ai-gateway.vercel.sh/v4/ai/evaluation-model",
        ),
        ({"AI_GATEWAY_API_KEY": "g", "jev_base_url": "http://proxy:8080/"}, "http://proxy:8080/v4/ai/evaluation-model"),
        ({"TYPESAFE_API_KEY": "t", "jev_base_url": "http://proxy:8080"}, "http://proxy:8080/v1/systemone"),
        ({"OPENROUTER_API_KEY": "o", "jev_base_url": "http://proxy:8080/"}, "http://proxy:8080/v1/systemone"),
    ],
)
async def test_jev_requests_go_to_the_chosen_source(values: dict[str, object], expected_url: str) -> None:
    seen: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(str(request.url))
        return httpx.Response(400, json={"error": "stop"})

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        jev = settings(**values).jev(http)
        with pytest.raises(JevError):
            await jev.evaluate("page", {"q": NoulQuestion(instructions="Is it?")})
    assert seen == [expected_url]


@pytest.mark.parametrize(
    ("source", "missing"),
    [
        (JevSource.OPENROUTER, "OPENROUTER_API_KEY"),
        (JevSource.TYPESAFE, "TYPESAFE_API_KEY"),
        (JevSource.GATEWAY, "AI_GATEWAY_API_KEY"),
    ],
)
async def test_a_chosen_source_without_its_key_is_a_configuration_error(source: JevSource, missing: str) -> None:
    values = dict.fromkeys(("OPENROUTER_API_KEY", "TYPESAFE_API_KEY", "AI_GATEWAY_API_KEY"), "key")
    values.pop(missing)
    async with httpx.AsyncClient() as http:
        with pytest.raises(ConfigurationError, match=missing):
            settings(**values, jev_source=source).jev(http)


@pytest.mark.parametrize("source", JevSource)
@pytest.mark.parametrize("restriction", ["one_key", "proxy", "model", "default_model"])
async def test_failover_requires_both_keys_and_default_routing(source: JevSource, restriction: str) -> None:
    values = {
        "TYPESAFE_API_KEY": "t" if restriction != "one_key" or source is JevSource.TYPESAFE else None,
        "AI_GATEWAY_API_KEY": "g" if restriction != "one_key" or source is JevSource.GATEWAY else None,
        "OPENROUTER_API_KEY": "o" if restriction != "one_key" or source is JevSource.OPENROUTER else None,
        "jev_source": source,
        "jev_base_url": "https://proxy.test" if restriction == "proxy" else None,
        "jev_model": {
            "model": "custom-jev",
            "default_model": {
                JevSource.OPENROUTER: "jev-1.13",
                JevSource.TYPESAFE: "jev-1.13.0",
                JevSource.GATEWAY: "typesafe-ai/jev",
            }[source],
        }.get(restriction),
    }
    seen: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(str(request.url))
        return httpx.Response(503)

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        client = settings(**values).jev(http)
        with pytest.raises(JevRetriesExhausted):
            await client.evaluate("page", {"q": NoulQuestion(instructions="Is it?")})
    host = (
        "proxy.test"
        if restriction == "proxy"
        else {
            JevSource.OPENROUTER: "openrouter.ai/api",
            JevSource.TYPESAFE: "api.typesafe.ai",
            JevSource.GATEWAY: "ai-gateway.vercel.sh",
        }[source]
    )
    path = "/v4/ai/evaluation-model" if source is JevSource.GATEWAY else "/v1/systemone"
    assert seen == [f"https://{host}{path}"] * 6


@pytest.mark.parametrize(
    ("source", "pin", "expected"),
    [
        (JevSource.OPENROUTER, None, "jev-1.13"),
        (JevSource.TYPESAFE, None, "jev-1.13.0"),
        (JevSource.OPENROUTER, "jev-1.13.0", "jev-1.13.0"),
        (JevSource.TYPESAFE, "jev-latest", "jev-latest"),
    ],
)
async def test_source_model_defaults_and_explicit_pins(source: JevSource, pin: str | None, expected: str) -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert json.loads(request.content)["model"] == expected
        return httpx.Response(400)

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        client = settings(OPENROUTER_API_KEY="o", TYPESAFE_API_KEY="t", jev_source=source, jev_model=pin).jev(http)
        with pytest.raises(JevError):
            await client.evaluate("page", {"q": NoulQuestion(instructions="Is it?")})


class Answer(Frozen):
    answer: str


async def _llm_request(values: dict[str, Any]) -> tuple[httpx.Request, Any]:
    seen: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        return httpx.Response(
            200,
            json={
                "choices": [{"message": {"content": '{"answer":"ok"}'}}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "cost": 0.0001},
            },
        )

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        result = (
            await settings(**values).llm(http).generate(LLMPurpose.READ, [Message(role="user", content="?")], Answer)
        )
    return seen[0], result


async def test_llm_prefers_openrouter_when_its_key_is_set() -> None:
    request, result = await _llm_request({"OPENROUTER_API_KEY": "o", "AI_GATEWAY_API_KEY": "g"})
    assert str(request.url) == "https://openrouter.ai/api/v1/chat/completions"
    assert request.headers["authorization"] == "Bearer o"
    body = TypeAdapter(dict[str, JsonValue]).validate_json(request.content)
    assert body["provider"] == {"require_parameters": True, "sort": "latency"}
    assert result.cost.basis is CostBasis.METERED


async def test_llm_uses_the_gateway_without_an_openrouter_key() -> None:
    request, result = await _llm_request({"AI_GATEWAY_API_KEY": "g"})
    assert str(request.url) == "https://ai-gateway.vercel.sh/v1/chat/completions"
    # The credential goes only to the host it belongs to.
    assert request.headers["authorization"] == "Bearer g"
    body = TypeAdapter(dict[str, JsonValue]).validate_json(request.content)
    assert body["model"] == DEFAULT_MODELS[LLMPurpose.READ]
    assert body["provider"] == {"sort": "ttft"}
    assert result.cost.basis is CostBasis.METERED


async def test_gateway_usage_without_a_cost_stays_unknown() -> None:
    def handler(_: httpx.Request) -> httpx.Response:
        # The gateway reports `usage.cost`; a response without it must not read as free.
        return httpx.Response(
            200,
            json={
                "choices": [{"message": {"content": '{"answer":"ok"}'}}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 2},
            },
        )

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        result = (
            await settings(AI_GATEWAY_API_KEY="g")
            .llm(http)
            .generate(LLMPurpose.READ, [Message(role="user", content="?")], Answer)
        )
    assert result.cost.basis is CostBasis.UNKNOWN and result.cost.dollars is None


async def test_llm_without_a_key_names_both_settings() -> None:
    async with httpx.AsyncClient() as http:
        with pytest.raises(ConfigurationError) as error:
            settings().llm(http)
    assert "OPENROUTER_API_KEY" in str(error.value) and "AI_GATEWAY_API_KEY" in str(error.value)


@pytest.mark.parametrize(
    ("values", "expected"),
    [
        ({"OPENROUTER_API_KEY": "o", "AI_GATEWAY_API_KEY": "g"}, "via OpenRouter"),
        ({"AI_GATEWAY_API_KEY": "g"}, "via Vercel AI Gateway"),
    ],
)
def test_providers_names_the_llm_route(values: dict[str, Any], expected: str) -> None:
    assert expected in settings(**values).providers()
