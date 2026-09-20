from typing import Any

import httpx
import pytest

from fastbrowse.clients.environment import ConfigurationError, JevSource, Settings
from fastbrowse.jev import JevError, NoulQuestion


@pytest.fixture(autouse=True)
def no_ambient_keys(monkeypatch: pytest.MonkeyPatch) -> None:
    for name in ("TYPESAFE_API_KEY", "AI_GATEWAY_API_KEY", "FASTBROWSE_JEV_SOURCE", "FASTBROWSE_JEV_BASE_URL"):
        monkeypatch.delenv(name, raising=False)


def settings(**values: Any) -> Settings:
    return Settings(_env_file=None, **values)


@pytest.mark.parametrize(
    ("values", "expected_url"),
    [
        ({"TYPESAFE_API_KEY": "t", "AI_GATEWAY_API_KEY": "g"}, "https://api.typesafe.ai/v1/systemone"),
        (
            {"TYPESAFE_API_KEY": "t", "AI_GATEWAY_API_KEY": "g", "jev_source": JevSource.GATEWAY},
            "https://ai-gateway.vercel.sh/v4/ai/evaluation-model",
        ),
        ({"AI_GATEWAY_API_KEY": "g", "jev_base_url": "http://proxy:8080/"}, "http://proxy:8080/v4/ai/evaluation-model"),
        ({"TYPESAFE_API_KEY": "t", "jev_base_url": "http://proxy:8080"}, "http://proxy:8080/v1/systemone"),
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


def test_a_chosen_source_without_its_key_is_a_configuration_error() -> None:
    with pytest.raises(ConfigurationError, match="TYPESAFE_API_KEY"):
        settings(AI_GATEWAY_API_KEY="g", jev_source=JevSource.TYPESAFE).jev(httpx.AsyncClient())
