"""Shared wire encoding and strict validation for both Jev transports."""

import asyncio
import math
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from typing import assert_never

import httpx
from pydantic import JsonValue, TypeAdapter

from fastbrowse.jev import (
    Answer,
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
from fastbrowse.models import CostBasis, CostComponent, CostLine


def wire_questions(questions: Mapping[str, Question], *, gateway: bool = False) -> dict[str, JsonValue]:
    result: dict[str, JsonValue] = {}
    for key, question in questions.items():
        body: dict[str, JsonValue] = {"instructions": question.instructions}
        match question:
            case ChoiceQuestion():
                body.update(type="choice", criteria=dict(question.criteria))
            case NoulQuestion():
                body["type"] = "boolean" if gateway else "noul"
                criteria: dict[str, JsonValue] = {}
                if question.true is not None:
                    criteria["true"] = question.true
                if question.false is not None:
                    criteria["false"] = question.false
                if criteria:
                    body["criteria"] = criteria
            case ScoreQuestion():
                body.update(type="score", criteria=list(question.criteria))
            case _:
                assert_never(question)
        result[key] = body
    return result


def object_value(value: JsonValue) -> dict[str, JsonValue]:
    if not isinstance(value, dict):
        raise ValueError("expected an object")
    return value


def number(value: JsonValue) -> float:
    if isinstance(value, bool) or not isinstance(value, int | float) or not math.isfinite(value):
        raise ValueError("expected a finite number")
    return float(value)


def probability(value: JsonValue) -> float:
    result = number(value)
    if not 0 <= result <= 1:
        raise ValueError("probability outside [0, 1]")
    return result


def token_count(value: JsonValue) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ValueError("expected a nonnegative integer token count")
    return value


def dollars(value: JsonValue) -> float:
    result = float(value) if isinstance(value, str) else number(value)
    if not math.isfinite(result) or result < 0:
        raise ValueError("expected a finite nonnegative cost")
    return result


def json_object(response: httpx.Response) -> dict[str, JsonValue]:
    return TypeAdapter(dict[str, JsonValue]).validate_json(response.content)


def body_excerpt(response: httpx.Response) -> str:
    """The start of a response body for an error message, minus the credential it was requested with.

    Some providers echo the rejected key back in an authentication error, and error text is logged.
    """
    text = response.text
    try:
        credential = response.request.headers.get("authorization", "").removeprefix("Bearer ").strip()
    except RuntimeError:  # a response built without a request carries no credential to leak
        credential = ""
    if credential:
        text = text.replace(credential, "[api key]")
    return text[:400]


def response_error(response: httpx.Response, detail: str) -> JevError:
    return JevError(f"{detail[:300]}; HTTP {response.status_code}: {body_excerpt(response)}")


RETRY_DELAYS_SECONDS = (0.5, 1.5, 4.0)
RETRYABLE_STATUS = frozenset({408, 429, 500, 502, 503, 504, 529})
_MAX_BACKOFF_SECONDS = 10.0
JEV_ATTEMPT_SECONDS = 15.0
"""Jev answers in about a second, so an attempt this old is stuck upstream, and a retry beats waiting on it."""
LLM_ATTEMPT_SECONDS = 30.0
"""Leaves room for long reads and verification while bounding upstream stalls; flash-lite plans take about 0.8s."""
JEV_HEDGE_SECONDS = 1.5
"""Over twice the slowest of 25 measured gateway calls (0.28s median, 0.62s worst); a live run once spent 34s of
one task in Jev, and a second request sent here wins those for a fraction of a cent."""
LLM_HEDGE_SECONDS = 4.0
"""About twice a typical read or verify (2 to 2.7s). Live runs saw single PLAN and READ calls take 7 to 9s while
the rest took 2s; hedging here duplicates only that tail, and these calls cost a fraction of a cent."""


@dataclass(slots=True)
class RequestUsage:
    unaccounted_requests: int = 0
    """Requests that may have been billed but whose usage was not returned to the caller."""


def with_discarded(cost: CostLine, usage: RequestUsage) -> CostLine:
    """A Jev call's cost including requests raced and discarded: each carried the same input, so each is
    charged as the one that answered, as an estimate."""
    if not usage.unaccounted_requests:
        return cost
    sent = 1 + usage.unaccounted_requests
    return cost.model_copy(
        update={
            "basis": CostBasis.ESTIMATED if cost.dollars is not None else CostBasis.UNKNOWN,
            "dollars": None if cost.dollars is None else cost.dollars * sent,
            "input_tokens": cost.input_tokens * sent,
        }
    )


async def post_with_retry(
    http: httpx.AsyncClient,
    url: str,
    body: dict[str, JsonValue],
    headers: Mapping[str, str],
    *,
    attempt_seconds: float,
    hedge_seconds: float,
    before_retry: Callable[[], None] | None = None,
    usage: RequestUsage | None = None,
) -> httpx.Response | None:
    """Retry an overloaded or dropped request, which produced nothing and is always safe to repeat.

    Each attempt is hedged: a request still unanswered after `hedge_seconds` is raced by an identical one,
    because a provider's slowest calls are stalls, not work, and a fresh request tends to land on a healthy
    replica. `before_retry` runs ahead of every repeat and every hedge, so a budget counts each request
    actually sent. `usage` counts discarded or timed-out requests that may still have been billed.
    Returns None when the transport never completed, leaving each client to name its own failure.
    """
    response: httpx.Response | None = None
    for attempt, delay in enumerate((*RETRY_DELAYS_SECONDS, None)):
        if attempt and before_retry is not None:
            before_retry()
        response = await _hedged(
            http,
            url,
            body,
            headers,
            attempt_seconds=attempt_seconds,
            hedge_seconds=hedge_seconds,
            before_hedge=before_retry,
            usage=usage,
        )
        if response is not None and response.status_code not in RETRYABLE_STATUS:
            return response
        if delay is not None:
            await asyncio.sleep(_backoff(response, delay))
    return response


def _backoff(response: httpx.Response | None, delay: float) -> float:
    """The server's own `Retry-After` when it sends one on a 429 or 529, as TypeSafe's SDK honours it; capped
    so an overloaded provider cannot hold a run past its budget."""
    if response is None:
        return delay
    for header, scale in (("retry-after-ms", 0.001), ("retry-after", 1.0)):
        raw = response.headers.get(header)
        if raw is None:
            continue
        try:
            return min(max(float(raw) * scale, 0.0), _MAX_BACKOFF_SECONDS)
        except ValueError:
            break
    return delay


async def _hedged(
    http: httpx.AsyncClient,
    url: str,
    body: dict[str, JsonValue],
    headers: Mapping[str, str],
    *,
    attempt_seconds: float,
    hedge_seconds: float,
    before_hedge: Callable[[], None] | None,
    usage: RequestUsage | None,
) -> httpx.Response | None:
    """The first usable response from one request, raced by a second if the first outlasts `hedge_seconds`."""
    requests = {asyncio.create_task(_send(http, url, body, headers, attempt_seconds))}
    winner: asyncio.Task[httpx.Response | None] | None = None
    try:
        done, _ = await asyncio.wait(requests, timeout=hedge_seconds)
        if not done:
            if before_hedge is not None:
                before_hedge()
            requests.add(asyncio.create_task(_send(http, url, body, headers, attempt_seconds)))
        response: httpx.Response | None = None
        pending = set(requests)
        while pending:
            done, pending = await asyncio.wait(pending, return_when=asyncio.FIRST_COMPLETED)
            for request in done:
                response = request.result()
                if response is not None and response.status_code not in RETRYABLE_STATUS:
                    winner = request
                    return response
        return response
    finally:
        # The losing request is still open on the provider; cancel it and wait, so nothing outlives the call.
        for request in requests:
            request.cancel()
        await asyncio.gather(*requests, return_exceptions=True)
        if usage is not None:
            for request in requests:
                if request is winner:
                    continue
                result = None if request.cancelled() or request.exception() else request.result()
                # Providers do not bill error statuses, so retries after those add no cost.
                if result is None or result.is_success:
                    usage.unaccounted_requests += 1


async def _send(
    http: httpx.AsyncClient, url: str, body: dict[str, JsonValue], headers: Mapping[str, str], attempt_seconds: float
) -> httpx.Response | None:
    try:
        return await http.post(url, json=body, headers=headers, timeout=attempt_seconds)
    except httpx.HTTPError:
        return None


async def post(
    http: httpx.AsyncClient,
    url: str,
    api_key: str,
    body: dict[str, JsonValue],
    headers: Mapping[str, str] | None = None,
    *,
    usage: RequestUsage | None = None,
) -> httpx.Response:
    auth = {"Authorization": f"Bearer {api_key}", **(headers or {})}
    response = await post_with_retry(
        http, url, body, auth, attempt_seconds=JEV_ATTEMPT_SECONDS, hedge_seconds=JEV_HEDGE_SECONDS, usage=usage
    )
    if response is None:
        raise JevError("Jev transport failed")
    if response.status_code == 400 and "max_tokens_exceeded" in response.text:
        raise JevInputTooLarge(f"Jev input too large; HTTP 400: {body_excerpt(response)}")
    if not response.is_success:
        raise response_error(response, "Jev request failed")
    return response


def _probabilities(value: JsonValue, keys: set[str]) -> dict[str, float]:
    raw = object_value(value)
    if not keys or set(raw) != keys:
        raise ValueError("probability keys differ from criteria")
    result = {key: probability(value) for key, value in raw.items()}
    # Each probability is rounded to two decimals on the wire, so the sum can drift by up to half a unit per
    # option (the AI SDK's own check); a fixed 0.02 rejected valid answers over many options.
    tolerance = max(0.02, 0.005 * len(result))
    if abs(math.fsum(result.values()) - 1) > tolerance + 1e-12:
        raise ValueError(f"probabilities do not sum to one within {tolerance}")
    return result


_ROUNDING_UNIT = 0.01
"""The wire's two-decimal probability step."""


def parse_answers(
    value: JsonValue,
    questions: Mapping[str, Question],
    *,
    gateway: bool = False,
    confidence: Mapping[str, JsonValue] | None = None,
) -> dict[str, Answer]:
    raw = object_value(value)
    if set(raw) != set(questions):
        raise ValueError("answer ids differ from question ids")
    answers: dict[str, Answer] = {}
    for key, question in questions.items():
        answer = object_value(raw[key])
        expected_type = "boolean" if gateway and isinstance(question, NoulQuestion) else question.type
        if answer.get("type") != expected_type:
            raise ValueError(f"incorrect answer type for {key}")
        confidence_value = (confidence or {}).get(key) if gateway else answer.get("confidence")
        match question:
            case ChoiceQuestion():
                probs = _probabilities(answer.get("probabilities"), set(question.criteria))
                choice = answer.get("choice")
                if not isinstance(choice, str) or choice not in probs:
                    raise ValueError(f"choice outside criteria for {key}")
                # Probabilities are rounded to sum to one, which can move an option by one unit and put a near-tie
                # a hundredth the wrong way round: a live run failed on escalate 0.40 chosen over read 0.41.
                if probs[choice] < max(probs.values()) - _ROUNDING_UNIT - 1e-6:
                    raise ValueError(f"chosen option is not maximal for {key}")
                answers[key] = ChoiceAnswer(
                    choice=choice, probabilities=probs, confidence=probability(confidence_value)
                )
            case NoulQuestion():
                # v1 of the direct API names P(yes) `noul` (https://docs.typesafe.ai/migrating-to-v1.md); the
                # gateway maps it to a boolean answer's `probability`.
                field = "probability" if gateway else "noul"
                answers[key] = NoulAnswer(probability=probability(answer.get(field)))
            case ScoreQuestion():
                probs = _probabilities(answer.get("probabilities"), {str(i) for i in range(len(question.criteria))})
                score = number(answer.get("score"))
                if not 0 <= score <= len(question.criteria) - 1:
                    raise ValueError(f"score outside levels for {key}")
                answers[key] = ScoreAnswer(score=score, probabilities=probs, confidence=probability(confidence_value))
            case _:
                assert_never(question)
    return answers


def estimated_cost(input_tokens: int, output_tokens: int = 0) -> CostLine:
    return CostLine(
        component=CostComponent.JEV,
        basis=CostBasis.ESTIMATED,
        dollars=input_tokens * 0.042 / 1_000_000,
        input_tokens=input_tokens,
        output_tokens=output_tokens,
    )
