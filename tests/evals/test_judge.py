"""The judge's own Jev calls: the provider it picks, the wire form it speaks, and a busy provider retried."""

import json
from pathlib import Path

import httpx
import pytest

from fastbrowse.evals import judge as judging
from fastbrowse.evals.judge import Claim, Jev, jev_from_environment, judge

parallax_jev = pytest.importorskip("parallax.jev")

CLAIMS = (Claim("The answer gives the total as 43.25."),)
KEYS = ("OPENROUTER_API_KEY", "TYPESAFE_API_KEY", "AI_GATEWAY_API_KEY")


def _http(*responses: httpx.Response | Exception, sent: list[httpx.Request]) -> httpx.AsyncClient:
    queue = list(responses)

    def handle(request: httpx.Request) -> httpx.Response:
        sent.append(request)
        reply = queue.pop(0)
        if isinstance(reply, Exception):
            raise reply
        return reply

    return httpx.AsyncClient(transport=httpx.MockTransport(handle))


def _answers(field: str, probability: float) -> httpx.Response:
    answer = {"type": "noul", "noul": probability} if field == "noul" else {field: probability}
    return httpx.Response(
        200,
        json={"answers": {"claim_0": answer}, "usage": {"input_tokens": 1, "output_tokens": 1, "cost": 0.01}},
    )


@pytest.fixture(autouse=True)
def _no_waiting(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(parallax_jev, "BACKOFF_S", (0.0, 0.0))


async def test_a_busy_provider_is_asked_again_rather_than_failing_the_row() -> None:
    sent: list[httpx.Request] = []
    replies = (httpx.Response(429), httpx.ConnectError("reset"), _answers("noul", 0.9))
    async with _http(*replies, sent=sent) as http:
        jev = Jev(http, api_key="k", base_url=judging.OPENROUTER_URL, model=judging.OPENROUTER_MODEL)
        assert await judge(jev, task="t", answer="43.25", claims=CLAIMS) is None
    assert len(sent) == 3


async def test_a_provider_still_busy_after_every_retry_fails_the_judgement() -> None:
    sent: list[httpx.Request] = []
    async with _http(*[httpx.Response(503)] * 3, sent=sent) as http:
        jev = Jev(http, api_key="k", base_url=judging.OPENROUTER_URL, model=judging.OPENROUTER_MODEL)
        assert await judge(jev, task="t", answer="43.25", claims=CLAIMS) is not None
    assert len(sent) == 3


async def test_a_refusal_is_not_retried() -> None:
    sent: list[httpx.Request] = []
    async with _http(httpx.Response(401), sent=sent) as http:
        jev = Jev(http, api_key="k", base_url=judging.OPENROUTER_URL, model=judging.OPENROUTER_MODEL)
        assert await judge(jev, task="t", answer="43.25", claims=CLAIMS) is not None
    assert len(sent) == 1


async def test_the_gateway_uses_the_parallax_typed_request() -> None:
    sent: list[httpx.Request] = []
    async with _http(_answers("probability", 0.1), sent=sent) as http:
        failure = await judge(
            Jev(http, api_key="k", base_url="https://ai-gateway.vercel.sh", model="typesafe-ai/jev-1.14.0"),
            task="t",
            answer="34.25",
            claims=CLAIMS,
        )
    assert failure is not None
    assert str(sent[0].url) == "https://ai-gateway.vercel.sh/systemone"
    body = json.loads(sent[0].content)
    assert body["model"] == "typesafe-ai/jev-1.14.0"
    assert body["questions"]["claim_0"]["type"] == "noul"


@pytest.mark.parametrize(
    ("keys", "url"),
    [
        (KEYS, "https://openrouter.ai/api/v1/systemone"),
        (KEYS[1:], "https://api.typesafe.ai/systemone"),
        (KEYS[2:], "https://ai-gateway.vercel.sh/systemone"),
    ],
)
async def test_the_judge_starts_on_the_provider_a_run_would(
    keys: tuple[str, ...], url: str, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.chdir(tmp_path)
    for name in KEYS:
        monkeypatch.delenv(name, raising=False)
    (tmp_path / ".env").write_text("".join(f"{name}=k\n" for name in keys), encoding="utf-8")
    sent: list[httpx.Request] = []
    async with _http(_answers("noul", 0.9), sent=sent) as http:
        assert await judge(jev_from_environment(http), task="t", answer="43.25", claims=CLAIMS) is None
    assert str(sent[0].url) == url


async def test_the_judge_follows_the_chosen_decision_source(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.chdir(tmp_path)
    for name in (*KEYS, "FASTBROWSE_DECISION_SOURCE"):
        monkeypatch.delenv(name, raising=False)
    env = "".join(f"{name}=k\n" for name in KEYS) + "FASTBROWSE_DECISION_SOURCE=gateway\n"
    (tmp_path / ".env").write_text(env, encoding="utf-8")
    sent: list[httpx.Request] = []
    async with _http(_answers("noul", 0.9), sent=sent) as http:
        assert await judge(jev_from_environment(http), task="t", answer="43.25", claims=CLAIMS) is None
    assert str(sent[0].url) == "https://ai-gateway.vercel.sh/systemone"


def test_no_key_says_which_the_judge_takes(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.chdir(tmp_path)
    for name in KEYS:
        monkeypatch.delenv(name, raising=False)
    with pytest.raises(RuntimeError, match="AI_GATEWAY_API_KEY"):
        jev_from_environment(httpx.AsyncClient())


async def test_the_judge_reads_every_post_the_run_sent() -> None:
    sent: list[httpx.Request] = []
    posts = {"/submit/contact": [{"message": "first"}, {"message": "second"}]}
    async with _http(_answers("noul", 0.9), sent=sent) as http:
        jev = Jev(http, api_key="k", base_url=judging.OPENROUTER_URL, model=judging.OPENROUTER_MODEL)
        assert await judge(jev, task="t", answer="43.25", submitted=posts, claims=CLAIMS) is None
    assert json.loads(sent[0].content)["state"]["submitted"] == posts
