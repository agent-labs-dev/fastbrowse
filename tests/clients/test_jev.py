import math
from collections.abc import Mapping

import httpx
import pytest
from pydantic import JsonValue, TypeAdapter

from fastbrowse.clients.typesafe import TypeSafeJevClient
from fastbrowse.clients.vercel import VercelGatewayJevClient
from fastbrowse.jev import (
    ChoiceAnswer,
    ChoiceQuestion,
    JevError,
    JevInputTooLarge,
    NoulAnswer,
    NoulQuestion,
    Question,
    ScoreAnswer,
    ScoreQuestion,
)
from fastbrowse.models import CostBasis


def choice() -> ChoiceQuestion:
    return ChoiceQuestion(instructions="Which colour?", criteria={"blue": "Blue", "red": "Red"})


def choice_answer(**updates: JsonValue) -> dict[str, JsonValue]:
    return {
        "type": "choice",
        "choice": "blue",
        "probabilities": {"blue": 0.9, "red": 0.1},
        "confidence": 0.8,
        **updates,
    }


async def test_direct_encodes_all_question_types_and_estimates_usage() -> None:
    questions: Mapping[str, Question] = {
        "col": choice(),
        "bare": NoulQuestion(instructions="Wrong?"),
        "green": NoulQuestion(instructions="Green?", true="green", false="not green"),
        "partial": NoulQuestion(instructions="Present?", true="present"),
        "score": ScoreQuestion(instructions="How much?", criteria=("none", "some", "all")),
    }

    def handler(request: httpx.Request) -> httpx.Response:
        assert str(request.url) == "https://direct.test/v1/systemone"
        assert request.headers["authorization"] == "Bearer key"
        payload = TypeAdapter(dict[str, JsonValue]).validate_json(request.content)
        assert payload["state"] == {"text": "hello"}
        assert payload["model"] == "custom-jev"
        assert payload["questions"] == {
            "col": {"type": "choice", "instructions": "Which colour?", "criteria": {"blue": "Blue", "red": "Red"}},
            "bare": {"type": "noul", "instructions": "Wrong?"},
            "green": {"type": "noul", "instructions": "Green?", "criteria": {"true": "green", "false": "not green"}},
            "partial": {"type": "noul", "instructions": "Present?", "criteria": {"true": "present"}},
            "score": {"type": "score", "instructions": "How much?", "criteria": ["none", "some", "all"]},
        }
        return httpx.Response(
            200,
            json={
                "answers": {
                    "col": choice_answer(),
                    "bare": {"type": "noul", "noul": 0},
                    "green": {"type": "noul", "noul": 0.02},
                    "partial": {"type": "noul", "noul": 1},
                    "score": {
                        "type": "score",
                        "score": 1.99,
                        "probabilities": {"0": 0, "1": 0, "2": 1},
                        "confidence": 0.99,
                    },
                },
                "usage": {"input_tokens": 389},
            },
        )

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        result = await TypeSafeJevClient(
            "key", http=http, base_url="https://direct.test/", model="custom-jev"
        ).evaluate({"text": "hello"}, questions)
    assert result.cost.basis is CostBasis.ESTIMATED
    assert result.cost.dollars is not None and math.isclose(result.cost.dollars, 389 * 0.042 / 1_000_000)
    assert result.cost.input_tokens == result.input_tokens == 389
    assert isinstance(result.answers["col"], ChoiceAnswer)
    assert result.answers["green"] == NoulAnswer(probability=0.02)
    score = result.answers["score"]
    assert isinstance(score, ScoreAnswer)
    assert score.score == 1.99


@pytest.mark.parametrize(
    "answer",
    [
        choice_answer(choice="absent"),
        choice_answer(choice="red"),
        choice_answer(probabilities={"blue": 1}),
        choice_answer(probabilities={"blue": 1, "red": 0, "extra": 0}),
        choice_answer(probabilities={"blue": 0.3, "red": 0.3}),
        choice_answer(probabilities={"blue": 1.1, "red": -0.1}),
        choice_answer(probabilities={"blue": "0.9", "red": 0.1}),
        choice_answer(probabilities={"blue": True, "red": 0}),
        choice_answer(confidence=None),
        choice_answer(confidence=1.01),
        choice_answer(type="noul"),
    ],
)
async def test_direct_rejects_invalid_answers(answer: dict[str, JsonValue]) -> None:
    async with httpx.AsyncClient(
        transport=httpx.MockTransport(
            lambda _: httpx.Response(200, json={"answers": {"q": answer}, "usage": {"input_tokens": 10}})
        )
    ) as http:
        with pytest.raises(JevError, match="HTTP 200"):
            await TypeSafeJevClient("key", http=http).evaluate("state", {"q": choice()})


@pytest.mark.parametrize("invalid", ["NaN", "Infinity", "-Infinity"])
async def test_nonfinite_probabilities_are_rejected(invalid: str) -> None:
    body = (
        '{"answers":{"q":{"type":"choice","choice":"blue","probabilities":{"blue":'
        + invalid
        + ',"red":0},"confidence":1}},"usage":{"input_tokens":1}}'
    )
    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda _: httpx.Response(200, text=body))) as http:
        with pytest.raises(JevError):
            await TypeSafeJevClient("key", http=http).evaluate("state", {"q": choice()})


async def test_unused_speculative_head_is_validated_and_missing_head_rejected() -> None:
    async with httpx.AsyncClient(
        transport=httpx.MockTransport(
            lambda _: httpx.Response(200, json={"answers": {"q": choice_answer()}, "usage": {"input_tokens": 10}})
        )
    ) as http:
        with pytest.raises(JevError, match="answer ids"):
            await TypeSafeJevClient("key", http=http).evaluate("state", {"q": choice(), "unused": choice()})


@pytest.mark.parametrize("gateway", [False, True])
@pytest.mark.parametrize(
    "status,body,too_large",
    [(400, "max_tokens_exceeded", True), (500, "max_tokens_exceeded", False), (401, "bad key", False)],
)
async def test_http_failures_and_token_limits(gateway: bool, status: int, body: str, too_large: bool) -> None:
    async with httpx.AsyncClient(
        transport=httpx.MockTransport(lambda _: httpx.Response(status, text=body + "x" * 2000))
    ) as http:
        client = VercelGatewayJevClient("key", http=http) if gateway else TypeSafeJevClient("key", http=http)
        with pytest.raises(JevError) as error:
            await client.evaluate("state", {"q": choice()})
    assert isinstance(error.value, JevInputTooLarge) is too_large
    assert str(status) in str(error.value)
    assert len(str(error.value)) < 600


@pytest.mark.parametrize("cost", ["0.000016338", 0.000016338, 0, None])
async def test_gateway_remaps_wire_types_confidence_and_cost(cost: JsonValue) -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert str(request.url) == "https://ai-gateway.vercel.sh/v4/ai/evaluation-model"
        for key, value in {
            "authorization": "Bearer key",
            "ai-gateway-auth-method": "api-key",
            "ai-gateway-protocol-version": "0.0.1",
            "ai-model-id": "typesafe-ai/jev",
            "ai-evaluation-model-specification-version": "4",
        }.items():
            assert request.headers[key] == value
        payload = TypeAdapter(dict[str, JsonValue]).validate_json(request.content)
        assert set(payload) == {"state", "questions"}
        wire = payload["questions"]
        assert isinstance(wire, dict)
        assert wire["green"] == {"type": "boolean", "instructions": "Green?"}
        return httpx.Response(
            200,
            json={
                "answers": {
                    "green": {"type": "boolean", "probability": 0.02},
                    "col": {"type": "choice", "choice": "blue", "probabilities": {"blue": 1, "red": 0}},
                    "sc": {"type": "score", "score": 1.99, "probabilities": {"0": 0, "1": 0, "2": 1}},
                },
                "rounding": {"probabilityDecimals": 2, "scoreDecimals": 2},
                "usage": {"inputTokens": 389, "outputTokens": 61},
                "warnings": [],
                "providerMetadata": {"typesafe": {"confidence": {"col": 1, "sc": 0.99}}, "gateway": {"cost": cost}},
            },
        )

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        result = await VercelGatewayJevClient("key", http=http).evaluate(
            "state",
            {
                "green": NoulQuestion(instructions="Green?"),
                "col": choice(),
                "sc": ScoreQuestion(instructions="Score", criteria=("low", "medium", "high")),
            },
        )
    assert result.answers["green"] == NoulAnswer(probability=0.02)
    selected = result.answers["col"]
    assert isinstance(selected, ChoiceAnswer)
    assert selected.confidence == 1
    assert result.input_tokens == 389
    assert result.cost.output_tokens == 61
    assert result.cost.basis is (CostBasis.ESTIMATED if cost is None else CostBasis.METERED)
    expected = 389 * 0.042 / 1_000_000 if cost is None else float(str(cost))
    assert result.cost.dollars is not None and math.isclose(result.cost.dollars, expected)


@pytest.mark.parametrize("probabilities", [{"0": 0.2, "1": 0.8}, {"0": 0, "1": 0, "2": 0.8}, {"0": -1, "1": 1, "2": 1}])
async def test_score_distributions_are_validated(probabilities: dict[str, float]) -> None:
    async with httpx.AsyncClient(
        transport=httpx.MockTransport(
            lambda _: httpx.Response(
                200,
                json={
                    "answers": {
                        "q": {"type": "score", "score": 1.3, "probabilities": probabilities, "confidence": 0.8}
                    },
                    "usage": {"input_tokens": 1},
                },
            )
        )
    ) as http:
        with pytest.raises(JevError):
            await TypeSafeJevClient("key", http=http).evaluate(
                "state", {"q": ScoreQuestion(instructions="Score", criteria=("a", "b", "c"))}
            )


async def test_probability_rounding_boundary_and_near_ties_are_accepted() -> None:
    async with httpx.AsyncClient(
        transport=httpx.MockTransport(
            lambda _: httpx.Response(
                200,
                json={
                    "answers": {"q": choice_answer(probabilities={"blue": 0.49, "red": 0.49})},
                    "usage": {"input_tokens": 1},
                },
            )
        )
    ) as http:
        result = await TypeSafeJevClient("key", http=http).evaluate("state", {"q": choice()})
    assert isinstance(result.answers["q"], ChoiceAnswer)


async def test_transport_error_is_wrapped_without_exposing_key() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("sensitive transport detail key", request=request)

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        with pytest.raises(JevError, match="transport failed") as error:
            await TypeSafeJevClient("key", http=http).evaluate("state", {"q": choice()})
    assert "sensitive" not in str(error.value)


async def test_invalid_speculative_answer_is_not_ignored() -> None:
    async with httpx.AsyncClient(
        transport=httpx.MockTransport(
            lambda _: httpx.Response(
                200,
                json={
                    "answers": {"chosen_head": choice_answer(), "unused_head": choice_answer(choice="absent")},
                    "usage": {"input_tokens": 10},
                },
            )
        )
    ) as http:
        with pytest.raises(JevError, match="unused_head"):
            await TypeSafeJevClient("key", http=http).evaluate(
                "state", {"chosen_head": choice(), "unused_head": choice()}
            )


async def test_gateway_requires_metadata_confidence() -> None:
    async with httpx.AsyncClient(
        transport=httpx.MockTransport(
            lambda _: httpx.Response(
                200,
                json={
                    "answers": {"q": choice_answer()},
                    "usage": {"inputTokens": 10},
                },
            )
        )
    ) as http:
        with pytest.raises(JevError):
            await VercelGatewayJevClient("key", http=http).evaluate("state", {"q": choice()})


@pytest.mark.parametrize("probability", [-0.1, 1.1, "0.2", True])
async def test_noul_probability_is_validated(probability: JsonValue) -> None:
    async with httpx.AsyncClient(
        transport=httpx.MockTransport(
            lambda _: httpx.Response(
                200,
                json={
                    "answers": {"q": {"type": "noul", "noul": probability}},
                    "usage": {"input_tokens": 1},
                },
            )
        )
    ) as http:
        with pytest.raises(JevError):
            await TypeSafeJevClient("key", http=http).evaluate("state", {"q": NoulQuestion(instructions="Wrong?")})


async def test_a_choice_one_rounding_unit_below_the_top_is_accepted() -> None:
    # Sum-to-one rounding put escalate 0.40 as the choice over read 0.41 on a live run.
    answer = choice_answer(choice="red", probabilities={"blue": 0.5, "red": 0.49})
    async with httpx.AsyncClient(
        transport=httpx.MockTransport(
            lambda _: httpx.Response(200, json={"answers": {"q": answer}, "usage": {"input_tokens": 10}})
        )
    ) as http:
        evaluation = await TypeSafeJevClient("key", http=http).evaluate("state", {"q": choice()})
    result = evaluation.answers["q"]
    assert isinstance(result, ChoiceAnswer) and result.choice == "red"
