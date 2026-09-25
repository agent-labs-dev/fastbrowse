import asyncio
import json
from collections.abc import Mapping

import httpx
import pytest
from pydantic import JsonValue

from fastbrowse.batches import evaluate_batches
from fastbrowse.clients.failover import FailoverJevClient
from fastbrowse.clients.typesafe import TypeSafeJevClient
from fastbrowse.clients.vercel import VercelGatewayJevClient
from fastbrowse.config import TokenBudget
from fastbrowse.jev import Evaluation, NoulAnswer, NoulQuestion, Question
from fastbrowse.models import CostBasis, CostComponent, CostLine, Limits
from fastbrowse.telemetry import BudgetExceeded, Ledger

# Room for one question per request, so every question is its own batch.
_ONE_PER_BATCH = TokenBudget(state_plus_largest_question=40, state_plus_all_questions=40)
_QUESTIONS = {"a": NoulQuestion(instructions="first"), "b": NoulQuestion(instructions="second")}


class _SlowSecond:
    def __init__(self) -> None:
        self.sent: list[Mapping[str, Question]] = []

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        self.sent.append(questions)
        if "b" in questions:
            await asyncio.Event().wait()
        return Evaluation(
            model="test",
            answers={key: NoulAnswer(probability=0.5) for key in questions},
            input_tokens=10,
            cost=CostLine(component=CostComponent.JEV, basis=CostBasis.ESTIMATED, dollars=0.01),
        )


async def test_a_pass_cancelled_by_a_deadline_keeps_the_cost_already_billed() -> None:
    jev, ledger = _SlowSecond(), Ledger(Limits())
    with pytest.raises(TimeoutError):
        async with asyncio.timeout(0.05):
            await evaluate_batches(jev, {}, _QUESTIONS, tokens=_ONE_PER_BATCH, ledger=ledger)
    assert len(jev.sent) == 2 and ledger.breakdown().known_dollars == pytest.approx(0.01)


async def test_a_pass_the_call_limit_cannot_cover_counts_no_call() -> None:
    jev, ledger = _SlowSecond(), Ledger(Limits(max_jev_calls=1))
    with pytest.raises(BudgetExceeded):
        await evaluate_batches(jev, {}, _QUESTIONS, tokens=_ONE_PER_BATCH, ledger=ledger)
    assert jev.sent == [] and ledger.jev_calls == 0


class _Answers:
    def __init__(self) -> None:
        self.sent: list[Mapping[str, Question]] = []

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        self.sent.append(questions)
        return Evaluation(
            model="test",
            answers={key: NoulAnswer(probability=0.5) for key in questions},
            input_tokens=10,
            cost=CostLine(component=CostComponent.JEV, basis=CostBasis.ESTIMATED, dollars=0.01),
        )


async def test_many_questions_go_as_small_requests_even_when_one_would_fit() -> None:
    jev = _Answers()
    questions = {f"q{i}": NoulQuestion(instructions="x" * 300) for i in range(40)}
    answered = await evaluate_batches(jev, {}, questions, tokens=TokenBudget(batch_tokens=1000), ledger=None)
    assert answered is not None and len(answered.answers) == 40
    assert len(jev.sent) > 1 and all(len(sent) < 40 for sent in jev.sent)


async def test_split_requests_and_partial_spend_survive_batch_accounting(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("fastbrowse.clients.validation.RETRY_DELAYS_SECONDS", (0, 0, 0))
    calls = 0

    def handler(request: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        keys = tuple(json.loads(request.content)["questions"])
        if len(keys) > 1:
            return httpx.Response(503)
        return httpx.Response(
            200, json={"answers": {key: {"type": "noul", "noul": 1} for key in keys}, "usage": {"input_tokens": 100}}
        )

    ledger = Ledger(Limits())
    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        result = await evaluate_batches(
            TypeSafeJevClient("key", http=http), {}, _QUESTIONS, tokens=TokenBudget(), ledger=ledger
        )
    assert result is not None and result.requests == calls == 4
    assert result.input_tokens == 200
    assert sum(line.input_tokens for line in ledger.lines) == 200


@pytest.mark.parametrize("client_type", [TypeSafeJevClient, VercelGatewayJevClient])
@pytest.mark.parametrize("failover", [False, True])
@pytest.mark.parametrize("limit", [0.00006, 0.000042])
async def test_split_singles_stop_when_the_dollar_budget_is_spent(
    monkeypatch: pytest.MonkeyPatch,
    client_type: type[TypeSafeJevClient] | type[VercelGatewayJevClient],
    failover: bool,
    limit: float,
) -> None:
    monkeypatch.setattr("fastbrowse.clients.validation.RETRY_DELAYS_SECONDS", (0, 0, 0, 0, 0))
    singles: list[str] = []
    gateway = client_type is VercelGatewayJevClient

    def handler(request: httpx.Request) -> httpx.Response:
        keys = tuple(json.loads(request.content)["questions"])
        if len(keys) > 1:
            return httpx.Response(503)
        singles.extend(keys)
        return httpx.Response(
            200,
            json={
                "answers": {
                    key: {"type": "boolean", "probability": 1} if gateway else {"type": "noul", "noul": 1}
                    for key in keys
                },
                "usage": {"inputTokens" if gateway else "input_tokens": 1000},
            },
        )

    ledger = Ledger(Limits(max_dollars=limit))
    questions = {f"q{i}": NoulQuestion(instructions="x") for i in range(10)}
    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        primary = client_type("key", http=http)
        client = FailoverJevClient(primary, client_type("backup", http=http)) if failover else primary
        with pytest.raises(BudgetExceeded):
            await evaluate_batches(client, {}, questions, tokens=TokenBudget(), ledger=ledger)
    expected = 2 if limit == 0.00006 else 1
    assert singles == list(questions)[:expected]
    assert ledger.breakdown().known_dollars == pytest.approx(expected * 0.000042)


async def test_a_split_budget_stop_cancels_other_batches_and_keeps_their_spend(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr("fastbrowse.clients.validation.RETRY_DELAYS_SECONDS", (0, 0, 0))
    waiting = asyncio.Event()
    stopped = asyncio.Event()

    async def handler(request: httpx.Request) -> httpx.Response:
        keys = tuple(json.loads(request.content)["questions"])
        if len(keys) > 1:
            return httpx.Response(503)
        if keys == ("q0",):
            await waiting.wait()
        if keys == ("q3",):
            waiting.set()
            try:
                await asyncio.Event().wait()
            finally:
                stopped.set()
        return httpx.Response(
            200, json={"answers": {key: {"type": "noul", "noul": 1} for key in keys}, "usage": {"input_tokens": 1000}}
        )

    ledger = Ledger(Limits(max_dollars=0.00006))
    questions = {f"q{i}": NoulQuestion(instructions="x") for i in range(4)}
    question_tokens = len(questions["q0"].model_dump_json()) / TokenBudget().chars_per_token
    tokens = TokenBudget(batch_tokens=int(2 * question_tokens + 2))
    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
        with pytest.raises(BudgetExceeded):
            await evaluate_batches(TypeSafeJevClient("key", http=http), {}, questions, tokens=tokens, ledger=ledger)
        assert stopped.is_set()
    assert ledger.breakdown().known_dollars == pytest.approx(0.000084)
