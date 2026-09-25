"""Shared wire encoding and strict validation for both Jev transports."""

import asyncio
import logging
import math
import time
from collections.abc import Awaitable, Callable, Generator, Mapping, Sequence
from contextlib import contextmanager
from contextvars import ContextVar
from dataclasses import dataclass, field
from time import monotonic
from typing import assert_never

import httpx
from pydantic import JsonValue, TypeAdapter, ValidationError

from fastbrowse.jev import (
    JEV_DOLLARS_PER_INPUT_TOKEN,
    Answer,
    ChoiceAnswer,
    ChoiceQuestion,
    Evaluation,
    JevError,
    JevInputTooLarge,
    JevRetriesExhausted,
    JevTransportFailed,
    NoulAnswer,
    NoulQuestion,
    Question,
    ScoreAnswer,
    ScoreQuestion,
)
from fastbrowse.models import CostBasis, CostComponent, CostLine
from fastbrowse.telemetry import trace

logger = logging.getLogger(__name__)


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
    return _scrubbed(response, response.text)[:400]


def _scrubbed(response: httpx.Response, text: str) -> str:
    try:
        credential = response.request.headers.get("authorization", "").removeprefix("Bearer ").strip()
    except RuntimeError:  # a response built without a request carries no credential to leak
        credential = ""
    return text.replace(credential, "[api key]") if credential else text


def _field(value: JsonValue, *keys: str) -> JsonValue:
    for key in keys:
        if not isinstance(value, dict):
            return None
        value = value.get(key)
    return value


def describe(response: httpx.Response) -> str:
    """A failed response as one line someone can act on: the status, the provider's own error type and message,
    and the upstream provider a gateway routed to. The start of the body when it is not the usual error JSON."""
    try:
        body: JsonValue = response.json()
    except ValueError:
        body = None
    message = _field(body, "error", "message")
    if not isinstance(message, str):
        return f"HTTP {response.status_code}: {body_excerpt(response) or '(empty body)'}"
    kind = _field(body, "error", "type")
    upstream = _field(body, "providerMetadata", "gateway", "routing", "resolvedProvider")
    # Scrubbed before it is cut: a cut through an echoed key leaves a fragment no exact replacement matches.
    text = f"HTTP {response.status_code}{f' {kind}' if isinstance(kind, str) else ''}: "
    text += _scrubbed(response, message)[:300] + (f" (via {upstream})" if isinstance(upstream, str) else "")
    return _scrubbed(response, text)


def error_detail(error: Exception) -> str:
    """An error's text without the values it rejected. Pydantic abbreviates a long value to its two ends, and an
    abbreviated key no longer matches the exact replacement that scrubs it."""
    return error.json(include_input=False, include_url=False) if isinstance(error, ValidationError) else str(error)


def response_error(response: httpx.Response, detail: str) -> JevError:
    # `detail` can quote the body, as a validation error's input does.
    return JevError(f"{_scrubbed(response, detail)[:300]}; {describe(response)}")


RETRY_DELAYS_SECONDS = (0.5, 1.5, 4.0, 8.0, 8.0)
"""About 22s in all. With 6s, a Jev 503 ended 5 of 311 eval runs, and each time the next run, started 0 to 15s
later, got through: the outages are brief, and a run lost to one costs far more than the wait. Only a longer
outage moves the run to the backup provider (`clients/failover.py`)."""
RETRYABLE_STATUS = frozenset({408, 429, 500, 502, 503, 504, 520, 521, 522, 523, 524, 529})
"""Statuses that say nothing about the request, so a repeat can clear them. 520 to 524 are Cloudflare's edge failing
to reach the provider behind it: a 520 arrived seconds after a retried 503 from the same outage and, unlisted, failed
its eval row for good. A status still listed once the retries run out ends the run unavailable, not in error, so
the eval harness retries the row too."""
TRANSIENT_TRANSPORT = (
    httpx.TimeoutException,
    httpx.NetworkError,
    httpx.RemoteProtocolError,
    httpx.ProxyError,
    httpx.DecodingError,
)
"""Transport failures a repeat can clear, a body garbled in transit included. Any other (an unsupported scheme, an
invalid URL) fails the same way every time, so it is raised at once rather than retried."""
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
    requests: int = 0
    consecutive_5xx: int = 0
    unaccounted_requests: int = 0
    """Requests that may have been billed but whose usage was not returned to the caller."""
    failures: list[str] = field(default_factory=list[str])
    """Why each request that came back unusable failed, in order: its status and reason, or its transport error."""

    def history(self, seconds: float) -> str:
        """How the call went before it gave up, so an error says whether it was one blip or a sustained outage."""
        return f"{len(self.failures)} failed requests in {seconds:.1f}s"


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
    call: str,
    attempt_seconds: float,
    hedge_seconds: float,
    before_retry: Callable[[], None] | None = None,
    usage: RequestUsage | None = None,
    start_attempt: int = 0,
    split_batch: bool = False,
) -> httpx.Response | None:
    """Retry an overloaded or dropped request, which produced nothing and is always safe to repeat.

    Each attempt is hedged: a request still unanswered after `hedge_seconds` is raced by an identical one,
    because a provider's slowest calls are stalls, not work, and a fresh request tends to land on a healthy
    replica. `before_retry` runs ahead of every repeat and every hedge, so a budget counts each request
    actually sent. `usage` counts discarded or timed-out requests that may still have been billed.
    Returns None when the transport never completed, leaving each client to name its own failure. Every retry is
    logged as a warning naming `call`, the failure and the wait, and each failure is kept on `usage`.
    """
    usage = usage if usage is not None else RequestUsage()
    response: httpx.Response | None = None
    began = time.monotonic()
    request_limit = len(RETRY_DELAYS_SECONDS) + 1 - start_attempt if start_attempt else None
    for attempt in range(start_attempt, len(RETRY_DELAYS_SECONDS) + 1):
        delay = RETRY_DELAYS_SECONDS[attempt] if attempt < len(RETRY_DELAYS_SECONDS) else None
        if attempt and before_retry is not None:
            before_retry()
        attempted = time.monotonic()
        response = await _hedged(
            http,
            url,
            body,
            headers,
            attempt_seconds=attempt_seconds,
            hedge_seconds=hedge_seconds,
            before_hedge=before_retry,
            usage=usage,
            allow_hedge=request_limit is None or usage.requests + 1 < request_limit,
        )
        if response is not None and response.status_code not in RETRYABLE_STATUS:
            if attempt:
                _transient(call, began, attempted)
            return response
        if request_limit is not None and usage.requests >= request_limit:
            break
        if split_batch and usage.consecutive_5xx >= 2 and delay is not None:
            _transient(call, began, time.monotonic())
            raise _SplitBatch(attempt + 1, usage)
        if delay is not None:
            wait = _backoff(response, delay)
            reason = usage.failures[-1] if usage.failures else "no usable response"
            logger.warning(
                "%s: %s; retry %d of %d in %.1fs", call, reason, attempt + 1, len(RETRY_DELAYS_SECONDS), wait
            )
            trace("request_retry", call=call, attempt=attempt + 1, reason=reason, wait=round(wait, 2))
            await asyncio.sleep(wait)
    _transient(call, began, time.monotonic())
    return response


def _transient(call: str, began: float, until: float) -> None:
    """The stretch a provider's transient failures cost this call: its failed attempts and the waits between them,
    up to the attempt that answered or to giving up. Evals leave it out of a run's time; it says nothing about the
    agent. The ends are `monotonic()` readings, so a collector can merge calls that overlapped."""
    trace("request_transient", call=call, began=began, ended=until, seconds=round(until - began, 2))


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
    usage: RequestUsage,
    allow_hedge: bool = True,
) -> httpx.Response | None:
    """The first usable response from one request, raced by a second if the first outlasts `hedge_seconds`."""
    requests = {asyncio.create_task(_send(http, url, body, headers, attempt_seconds, usage))}
    winner: asyncio.Task[httpx.Response | None] | None = None
    try:
        done, _ = await asyncio.wait(requests, timeout=hedge_seconds)
        if not done and allow_hedge:
            if before_hedge is not None:
                before_hedge()
            requests.add(asyncio.create_task(_send(http, url, body, headers, attempt_seconds, usage)))
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
        for request in requests:
            if request is winner:
                continue
            result = None if request.cancelled() or request.exception() else request.result()
            # Providers do not bill error statuses, so retries after those add no cost.
            if result is None or result.is_success:
                usage.unaccounted_requests += 1


async def _send(
    http: httpx.AsyncClient,
    url: str,
    body: dict[str, JsonValue],
    headers: Mapping[str, str],
    attempt_seconds: float,
    usage: RequestUsage,
) -> httpx.Response | None:
    usage.requests += 1
    try:
        response = await http.post(url, json=body, headers=headers, timeout=attempt_seconds)
    except httpx.TimeoutException as error:
        failure = f"no response within {attempt_seconds:.0f}s ({type(error).__name__})"
        response = None
    except TRANSIENT_TRANSPORT as error:
        # The type names the failure; its text can quote the request, credential included.
        failure = f"no response ({type(error).__name__})"
        response = None
    else:
        if response.status_code not in RETRYABLE_STATUS:
            return response
        failure = describe(response)
    usage.consecutive_5xx = usage.consecutive_5xx + 1 if response is not None and response.status_code >= 500 else 0
    usage.failures.append(failure)
    return response


async def post(
    http: httpx.AsyncClient,
    url: str,
    api_key: str,
    body: dict[str, JsonValue],
    headers: Mapping[str, str] | None = None,
    *,
    usage: RequestUsage | None = None,
    start_attempt: int = 0,
    split_batch: bool = False,
) -> httpx.Response:
    started = monotonic()
    usage = usage if usage is not None else RequestUsage()
    auth = {"Authorization": f"Bearer {api_key}", **(headers or {})}
    try:
        response = await post_with_retry(
            http,
            url,
            body,
            auth,
            call="jev",
            attempt_seconds=JEV_ATTEMPT_SECONDS,
            hedge_seconds=JEV_HEDGE_SECONDS,
            usage=usage,
            start_attempt=start_attempt,
            split_batch=split_batch,
        )
    except httpx.HTTPError as error:
        raise JevError(f"Jev request could not be sent ({type(error).__name__})") from None
    seconds = monotonic() - started
    if response is None:
        raise JevTransportFailed(f"Jev transport failed after {usage.history(seconds)}; last: {usage.failures[-1]}")
    if response.status_code in RETRYABLE_STATUS:
        raise JevRetriesExhausted(
            f"Jev request failed after {usage.history(seconds)}; last: {describe(response)}",
            requests=usage.requests,
            seconds=seconds,
            unaccounted_requests=usage.unaccounted_requests,
        )
    if response.status_code == 400 and "max_tokens_exceeded" in response.text:
        raise JevInputTooLarge(f"Jev input too large; {describe(response)}")
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


class _SplitBatch(Exception):
    def __init__(self, start_attempt: int, usage: RequestUsage) -> None:
        self.start_attempt = start_attempt
        self.usage = usage


_partial_spend: ContextVar[list[CostLine] | None] = ContextVar("jev_partial_spend", default=None)


@contextmanager
def jev_spend(lines: list[CostLine]) -> Generator[None]:
    """Keep answered singles billable when another single fails or the caller cancels the batch."""
    token = _partial_spend.set(lines)
    try:
        yield
    finally:
        _partial_spend.reset(token)


def record_jev_spend(costs: Sequence[CostLine]) -> None:
    if (lines := _partial_spend.get()) is not None:
        lines.extend(costs)


def merged_cost(lines: Sequence[CostLine]) -> CostLine:
    basis = next(
        (basis for basis in (CostBasis.UNKNOWN, CostBasis.ESTIMATED) if any(c.basis is basis for c in lines)),
        CostBasis.METERED,
    )
    return CostLine(
        component=CostComponent.JEV,
        basis=basis,
        dollars=None if any(c.dollars is None for c in lines) else sum(c.dollars or 0 for c in lines),
        input_tokens=sum(c.input_tokens for c in lines),
        output_tokens=sum(c.output_tokens for c in lines),
        seconds=sum(c.seconds or 0 for c in lines),
    )


async def asking_split(
    questions: Mapping[str, Question],
    ask: Callable[[Mapping[str, Question], int, bool], Awaitable[Evaluation]],
) -> Evaluation:
    """Ask a batch, then its questions one at a time within the retries left once it failed twice with a 5xx."""
    if len(questions) < 2:
        return await ask(questions, 0, False)
    started = monotonic()
    try:
        return await ask(questions, 0, True)
    except _SplitBatch as split:
        start_attempt, usage = split.start_attempt, split.usage
    answered: list[Evaluation] = []
    try:
        for key, question in questions.items():
            answered.append(await ask({key: question}, start_attempt, False))
    except BaseException as error:
        record_jev_spend([result.cost for result in answered])
        if isinstance(error, JevRetriesExhausted):
            raise JevRetriesExhausted(
                str(error),
                seconds=monotonic() - started,
                unaccounted_requests=error.unaccounted_requests + usage.unaccounted_requests,
                requests=usage.requests + sum(result.requests for result in answered) + error.requests,
                answered=answered,
            ) from error
        raise
    cost = merged_cost([result.cost for result in answered])
    if usage.unaccounted_requests:
        # The failed batch carried all questions; estimate its unanswered hedges from the answered singles.
        discarded = estimated_cost(sum(result.input_tokens for result in answered) * usage.unaccounted_requests)
        cost = merged_cost([cost, discarded])
    return Evaluation(
        model=answered[0].model,
        answers={key: answer for result in answered for key, answer in result.answers.items()},
        input_tokens=sum(result.input_tokens for result in answered),
        cost=cost.model_copy(update={"seconds": monotonic() - started}),
        requests=usage.requests + sum(result.requests for result in answered),
    )


async def asking_open(
    questions: Mapping[str, Question], ask: Callable[[Mapping[str, Question]], Awaitable[Evaluation]], model: str
) -> Evaluation:
    """Answer each choice of one option here and send `ask` only the rest.

    Jev began refusing a choice with a single option ("choice requires at least 2 options"), which the
    typesafe-ai route reported as a 503: wiki-godel's shortcut opened the article with one field to fill, and the
    run retried that refusal as an outage for hours. Such a choice was never in doubt, so it is not asked.
    """
    forced = {
        key: ChoiceAnswer(choice=only, probabilities={only: 1.0}, confidence=1.0)
        for key, question in questions.items()
        if isinstance(question, ChoiceQuestion) and len(question.criteria) == 1
        for only in question.criteria
    }
    if not forced:
        return await ask(questions)
    rest = {key: question for key, question in questions.items() if key not in forced}
    if not rest:
        return Evaluation(model=model, answers=forced, input_tokens=0, cost=estimated_cost(0), requests=0)
    evaluation = await ask(rest)
    return evaluation.model_copy(update={"answers": {**evaluation.answers, **forced}})


def estimated_cost(input_tokens: int, output_tokens: int = 0) -> CostLine:
    return CostLine(
        component=CostComponent.JEV,
        basis=CostBasis.ESTIMATED,
        dollars=input_tokens * JEV_DOLLARS_PER_INPUT_TOKEN,
        input_tokens=input_tokens,
        output_tokens=output_tokens,
    )
