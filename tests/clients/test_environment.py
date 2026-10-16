import json
from typing import Any

import httpx
import pytest
from pydantic import JsonValue, TypeAdapter

from fastbrowse.clients.environment import DEFAULT_MODELS, ConfigurationError, JevSource, Settings
from fastbrowse.jev import JevError, JevRetriesExhausted, NoulQuestion
from fastbrowse.llm import Message
from fastbrowse.models import CostBasis, CostBreakdown, Frozen, LLMPurpose


@pytest.fixture(autouse=True)
def no_ambient_keys(monkeypatch: pytest.MonkeyPatch) -> None:
    for name in (
        "TYPESAFE_API_KEY",
        "AI_GATEWAY_API_KEY",
        "OPENROUTER_API_KEY",
        "FASTBROWSE_JEV_SOURCE",
        "FASTBROWSE_JEV_BASE_URL",
        "FASTBROWSE_JEV_MODEL",
        "FASTBROWSE_LLM_MODEL",
        "FASTBROWSE_LLM_BASE_URL",
        "FASTBROWSE_LLM_API_KEY",
        "FASTBROWSE_LLM_STRICT",
        "FASTBROWSE_LLM_EXTRA_BODY",
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
        (
            {"OPENROUTER_API_KEY": "o", "llm_base_url": "http://127.0.0.1:8000/v1", "llm_model": "qwen"},
            "LLM qwen via local endpoint",
        ),
    ],
)
def test_providers_names_the_llm_route(values: dict[str, Any], expected: str) -> None:
    assert expected in settings(**values).providers()


LOCAL = {"llm_base_url": "http://127.0.0.1:8000/v1/", "llm_model": "qwen"}


def _local_reply(cost: float | None = None) -> httpx.Response:
    usage: dict[str, JsonValue] = {"prompt_tokens": 3, "completion_tokens": 2}
    if cost is not None:
        usage["cost"] = cost
    return httpx.Response(200, json={"choices": [{"message": {"content": '{"answer":"ok"}'}}], "usage": usage})


async def _local_requests(values: dict[str, Any]) -> tuple[list[httpx.Request], Any]:
    seen: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        return _local_reply()

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        result = (
            await settings(**values).llm(http).generate(LLMPurpose.READ, [Message(role="user", content="?")], Answer)
        )
    return seen, result


@pytest.mark.parametrize(
    "keys",
    [
        {},
        {"OPENROUTER_API_KEY": "o"},
        {"AI_GATEWAY_API_KEY": "g"},
        {"OPENROUTER_API_KEY": "o", "AI_GATEWAY_API_KEY": "g"},
    ],
)
async def test_a_base_url_takes_the_llm_from_openrouter_and_the_gateway(keys: dict[str, str]) -> None:
    seen, result = await _local_requests({**keys, **LOCAL})
    assert [str(request.url) for request in seen] == ["http://127.0.0.1:8000/v1/chat/completions"]
    # A keyless local server gets no Authorization header, and never the OpenRouter or gateway key.
    assert "authorization" not in seen[0].headers
    body = TypeAdapter(dict[str, JsonValue]).validate_json(seen[0].content)
    assert body["model"] == "qwen"
    assert "provider" not in body
    assert result.data.answer == "ok"


def test_an_empty_base_url_leaves_the_llm_on_openrouter() -> None:
    assert settings(OPENROUTER_API_KEY="o", llm_base_url="").llm_source() == "openrouter"


async def test_a_base_url_sends_its_own_key_and_no_other() -> None:
    seen, _ = await _local_requests(
        {"OPENROUTER_API_KEY": "o-secret", "AI_GATEWAY_API_KEY": "g-secret", "llm_api_key": "local-secret", **LOCAL}
    )
    assert seen[0].headers["authorization"] == "Bearer local-secret"
    sent = str(seen[0].headers) + seen[0].content.decode()
    assert "o-secret" not in sent and "g-secret" not in sent


def test_the_local_key_is_not_printed_with_the_settings() -> None:
    assert "local-secret" not in repr(settings(llm_api_key="local-secret", **LOCAL))


async def test_a_base_url_without_a_model_names_the_setting() -> None:
    async with httpx.AsyncClient() as http:
        with pytest.raises(ConfigurationError, match="FASTBROWSE_LLM_MODEL"):
            settings(llm_base_url="http://127.0.0.1:8000/v1").llm(http)


async def test_every_purpose_pinned_is_enough_without_one_model() -> None:
    pins = {f"llm_model_{purpose.value}": "qwen" for purpose in LLMPurpose}
    seen, _ = await _local_requests({"llm_base_url": "http://127.0.0.1:8000/v1", **pins})
    assert TypeAdapter(dict[str, JsonValue]).validate_json(seen[0].content)["model"] == "qwen"


async def test_strict_off_sends_the_models_own_schema() -> None:
    seen, _ = await _local_requests({**LOCAL, "llm_strict": False})
    body = TypeAdapter(dict[str, Any]).validate_json(seen[0].content)
    json_schema = body["response_format"]["json_schema"]
    assert json_schema["strict"] is False
    assert json_schema["schema"] == Answer.model_json_schema()


async def test_strict_by_default_on_a_local_endpoint() -> None:
    seen, _ = await _local_requests(LOCAL)
    body = TypeAdapter(dict[str, Any]).validate_json(seen[0].content)
    assert body["response_format"]["json_schema"]["strict"] is True
    assert body["response_format"]["json_schema"]["schema"]["additionalProperties"] is False


async def test_extra_body_is_merged_into_each_request() -> None:
    extra = {"chat_template_kwargs": {"enable_thinking": False}, "top_k": 20}
    seen, _ = await _local_requests({**LOCAL, "llm_extra_body": json.dumps(extra)})
    body = TypeAdapter(dict[str, JsonValue]).validate_json(seen[0].content)
    assert body["chat_template_kwargs"] == {"enable_thinking": False} and body["top_k"] == 20
    assert body["model"] == "qwen" and "response_format" in body


@pytest.mark.parametrize(
    ("extra", "message"),
    [
        ("{not json", "not valid JSON"),
        ("[1, 2]", "must be a JSON object"),
        ('"text"', "must be a JSON object"),
        ('{"model": "other"}', "cannot set model"),
        ('{"messages": [], "response_format": {}}', "cannot set messages, response_format"),
        ('{"provider": {}}', "cannot set provider"),
        ('{"reasoning": {"effort": "high"}}', "cannot set reasoning"),
        ('{"max_tokens": 1}', "cannot set max_tokens"),
    ],
)
async def test_an_unusable_extra_body_is_a_configuration_error(extra: str, message: str) -> None:
    async with httpx.AsyncClient() as http:
        with pytest.raises(ConfigurationError, match=message):
            settings(**LOCAL, llm_extra_body=extra).llm(http)


async def test_strict_and_extra_body_do_not_reshape_openrouter_requests() -> None:
    request, _ = await _llm_request({"OPENROUTER_API_KEY": "o", "llm_strict": False, "llm_extra_body": '{"top_k": 20}'})
    body = TypeAdapter(dict[str, Any]).validate_json(request.content)
    assert "top_k" not in body and body["response_format"]["json_schema"]["strict"] is True


async def test_a_local_endpoint_without_a_price_reports_an_unknown_cost_and_asks_nowhere_else() -> None:
    seen, result = await _local_requests(LOCAL)
    # No receipt or price lookup: those belong to OpenRouter and the gateway, and the key would follow them.
    assert len(seen) == 1
    assert result.cost.basis is CostBasis.UNKNOWN and result.cost.dollars is None
    assert (result.cost.input_tokens, result.cost.output_tokens) == (3, 2)
    assert CostBreakdown(lines=(result.cost,)).has_unknown


async def test_a_local_endpoint_that_reports_a_price_is_metered() -> None:
    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: _local_reply(0.0))) as http:
        result = (
            await settings(**LOCAL).llm(http).generate(LLMPurpose.READ, [Message(role="user", content="?")], Answer)
        )
    assert result.cost.basis is CostBasis.METERED and result.cost.dollars == 0.0


def test_the_local_settings_read_their_documented_variables(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("FASTBROWSE_LLM_BASE_URL", "http://127.0.0.1:8000/v1")
    monkeypatch.setenv("FASTBROWSE_LLM_API_KEY", "k")
    monkeypatch.setenv("FASTBROWSE_LLM_STRICT", "0")
    monkeypatch.setenv("FASTBROWSE_LLM_EXTRA_BODY", '{"top_k": 20}')
    loaded = settings()
    assert loaded.llm_source() == "local" and loaded.llm_api_key is not None
    assert loaded.llm_api_key.get_secret_value() == "k"
    assert loaded.llm_strict is False and loaded.llm_extra_body == '{"top_k": 20}'
