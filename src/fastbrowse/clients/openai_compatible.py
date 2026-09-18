"""Structured chat completions with one schema repair and complete usage accounting."""

import base64
from collections.abc import Mapping, Sequence
from enum import StrEnum
from time import monotonic

import httpx
from pydantic import BaseModel, JsonValue, TypeAdapter, ValidationError

from fastbrowse.clients.validation import (
    LLM_ATTEMPT_SECONDS,
    LLM_HEDGE_SECONDS,
    RequestUsage,
    body_excerpt,
    dollars,
    json_object,
    object_value,
    post_with_retry,
    token_count,
    with_discarded,
)
from fastbrowse.llm import Generation, LLMError, Message
from fastbrowse.models import CostBasis, CostComponent, CostLine, LLMPurpose
from fastbrowse.telemetry import BudgetExceeded, Ledger


def _image_url(content: bytes) -> str:
    if content.startswith(b"\x89PNG\r\n\x1a\n"):
        mime = "image/png"
    elif content.startswith(b"\xff\xd8\xff"):
        mime = "image/jpeg"
    else:
        raise LLMError("Only PNG and JPEG images are supported")
    return f"data:{mime};base64,{base64.b64encode(content).decode('ascii')}"


def _message(message: Message) -> JsonValue:
    if not message.images:
        return {"role": message.role, "content": message.content}
    parts: list[JsonValue] = [{"type": "text", "text": message.content}]
    parts.extend({"type": "image_url", "image_url": {"url": _image_url(image)}} for image in message.images)
    return {"role": message.role, "content": parts}


def _content(payload: dict[str, JsonValue]) -> str:
    choices = payload.get("choices")
    if not isinstance(choices, list) or not choices:
        raise ValueError("missing completion choices")
    message = object_value(object_value(choices[0]).get("message"))
    content = message.get("content")
    if not isinstance(content, str):
        raise ValueError("missing completion text (possibly a refusal)")
    return content


def _cost(payload: dict[str, JsonValue], purpose: LLMPurpose) -> CostLine:
    """Never raises: usage we cannot read is an unknown cost, which a dollar cap treats as unaffordable."""
    try:
        usage = object_value(payload.get("usage", {}))
        cost = usage.get("cost")
        return CostLine(
            component=CostComponent.LLM,
            purpose=purpose,
            basis=CostBasis.UNKNOWN if cost is None else CostBasis.METERED,
            dollars=None if cost is None else dollars(cost),
            input_tokens=token_count(usage.get("prompt_tokens", 0)),
            output_tokens=token_count(usage.get("completion_tokens", 0)),
        )
    except ValueError, TypeError, OverflowError:
        return CostLine(component=CostComponent.LLM, purpose=purpose, basis=CostBasis.UNKNOWN, dollars=None)


def _total_cost(costs: Sequence[CostLine], purpose: LLMPurpose) -> CostLine:
    bases = {line.basis for line in costs}
    basis = next(b for b in (CostBasis.UNKNOWN, CostBasis.ESTIMATED, CostBasis.METERED) if b in bases)
    return CostLine(
        component=CostComponent.LLM,
        purpose=purpose,
        basis=basis,
        dollars=None if basis is CostBasis.UNKNOWN else sum(line.dollars or 0 for line in costs),
        input_tokens=sum(line.input_tokens for line in costs),
        output_tokens=sum(line.output_tokens for line in costs),
    )


class ReasoningEffort(StrEnum):
    """How much hidden reasoning a model may spend before it answers, in OpenRouter's normalized terms."""

    LOW = "low"
    MEDIUM = "medium"
    HIGH = "high"


class OpenAICompatibleLLM:
    def __init__(
        self,
        api_key: str,
        *,
        http: httpx.AsyncClient,
        base_url: str,
        models: Mapping[LLMPurpose, str],
        reasoning_effort: ReasoningEffort | None = None,
    ) -> None:
        self._api_key = api_key
        self._http = http
        self._base_url = base_url.rstrip("/")
        self._models = dict(models)
        self._reasoning_effort = reasoning_effort

    async def _request(
        self, body: dict[str, JsonValue], ledger: Ledger | None, usage: RequestUsage
    ) -> dict[str, JsonValue]:
        response = await post_with_retry(
            self._http,
            f"{self._base_url}/chat/completions",
            body,
            {"Authorization": f"Bearer {self._api_key}"},
            attempt_seconds=LLM_ATTEMPT_SECONDS,
            hedge_seconds=LLM_HEDGE_SECONDS,
            before_retry=None if ledger is None else lambda: ledger.reserve(CostComponent.LLM),
            usage=usage,
        )
        if response is None:
            raise LLMError("LLM transport failed")
        if not response.is_success:
            raise LLMError(f"LLM HTTP {response.status_code}: {body_excerpt(response)}")
        try:
            return json_object(response)
        except ValueError:
            raise LLMError(f"Invalid LLM response; HTTP {response.status_code}: {body_excerpt(response)}") from None

    async def generate[T: BaseModel](
        self,
        purpose: LLMPurpose,
        messages: Sequence[Message],
        schema: type[T],
        *,
        max_output_tokens: int = 2000,
        ledger: Ledger | None = None,
    ) -> Generation[T]:
        model = self._models.get(purpose)
        if model is None:
            raise LLMError(f"No model configured for {purpose}")
        wire_messages = [_message(message) for message in messages]
        body: dict[str, JsonValue] = {
            "model": model,
            "max_tokens": max_output_tokens,
            "messages": wire_messages,
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": schema.__name__,
                    "schema": TypeAdapter(JsonValue).validate_python(schema.model_json_schema()),
                    "strict": False,
                },
            },
        }
        if self._reasoning_effort is not None:
            body["reasoning"] = {"effort": self._reasoning_effort.value}
        costs: list[CostLine] = []
        started = monotonic()
        try:
            for attempt in range(2):
                if ledger is not None:
                    ledger.reserve(CostComponent.LLM)
                usage = RequestUsage()
                try:
                    payload = await self._request(body, ledger, usage)
                except BaseException:
                    # With no answer to estimate from, a request that may have been billed is an unknown cost.
                    costs.extend(_cost({}, purpose) for _ in range(usage.unaccounted_requests))
                    raise
                # Recorded before the envelope is read: a generation we cannot parse was still billed, and
                # dropping it would let an unaccounted request pass a dollar cap. A hedge's discarded twin
                # carried the same prompt, so it is charged as the answer was; charging it as unknown instead
                # made every run with one slow call stop at its dollar cap.
                costs.append(with_discarded(_cost(payload, purpose), usage))
                try:
                    content = _content(payload)
                except (ValueError, TypeError, OverflowError) as error:
                    # An empty completion comes back intermittently (a dropped or refused generation); ask once more.
                    if attempt == 0:
                        continue
                    raise LLMError(f"Invalid completion envelope: {str(error)[:400]}") from None
                try:
                    data = schema.model_validate_json(content)
                except ValidationError as error:
                    # Avoid echoing rejected values: validation paths and reasons are enough to repair the schema.
                    detail = error.json(include_input=False, include_url=False)
                    if attempt == 1:
                        raise LLMError(f"LLM schema validation failed after one retry: {detail[:1000]}") from None
                    wire_messages.extend(
                        [
                            {"role": "assistant", "content": content},
                            {
                                "role": "user",
                                "content": f"Correct the JSON to match the schema. Validation errors:\n{detail}",
                            },
                        ]
                    )
                else:
                    cost = _total_cost(costs, purpose).model_copy(update={"seconds": monotonic() - started})
                    return Generation(data=data, cost=cost)
        except LLMError, BudgetExceeded:
            _charge(ledger, costs)
            raise
        raise AssertionError("unreachable")


def _charge(ledger: Ledger | None, costs: Sequence[CostLine]) -> None:
    """Record what a failed generation was billed. A success hands its cost to the caller to record instead."""
    if ledger is not None:
        ledger.record(*costs)
