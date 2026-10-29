"""Structured chat completions with one schema repair and complete usage accounting."""

import asyncio
import base64
from collections.abc import Mapping, Sequence
from enum import StrEnum
from time import monotonic

import httpx
from pydantic import BaseModel, JsonValue, TypeAdapter, ValidationError

from fastbrowse.clients.validation import (
    LLM_ATTEMPT_SECONDS,
    RequestUsage,
    body_excerpt,
    describe,
    dollars,
    error_detail,
    json_object,
    object_value,
    post_with_retry,
    retryable,
    token_count,
    with_discarded,
)
from fastbrowse.llm import DEFAULT_MAX_OUTPUT_TOKENS, Generation, LLMError, LLMRetriesExhausted, Message
from fastbrowse.models import CostBasis, CostComponent, CostLine, LLMPurpose, combined_basis
from fastbrowse.telemetry import BudgetExceeded, Ledger, call_span

_TRUNCATION_RETRY_FACTOR = 4
"""How much larger the output cap is on the one retry after a response ran into it."""

OPENROUTER_PROVIDER_ROUTING: dict[str, JsonValue] = {"require_parameters": True, "sort": "latency"}
"""OpenRouter routing: an endpoint that ignores response_format would treat the schema as a hint, so none is
routed to. OpenRouter's default routing sent gemini-3.8-flash reads to Vertex at a 2.4s median where AI Studio
answered the same read in 1.3s; sorting by latency keeps fallbacks and follows the faster endpoint."""

GATEWAY_PROVIDER_ROUTING: dict[str, JsonValue] = {"sort": "ttft"}
"""The Vercel AI Gateway documents `ttft` for the lowest time to first token where OpenRouter calls that sort
`latency`, and takes no `require_parameters`: the gateway matches the schema itself."""

RESERVED_BODY_KEYS = frozenset({"model", "max_tokens", "messages", "response_format", "provider", "reasoning"})
"""Request fields fastbrowse sets itself. An extra body that named one would replace the schema, the messages or
the output cap the truncation retry grows, so the run would fail far from the setting that caused it."""


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


def _truncated(payload: dict[str, JsonValue]) -> bool:
    """The model ran out of output tokens: what it wrote is cut off mid-value and says nothing of its schema."""
    choices = payload.get("choices")
    if not isinstance(choices, list) or not choices:
        return False
    return object_value(choices[0]).get("finish_reason") == "length"


def _ends_mid_json(error: ValidationError) -> bool:
    return any(
        detail["type"] == "json_invalid" and str(detail.get("ctx", {}).get("error", "")).startswith("EOF")
        for detail in error.errors(include_url=False)
    )


def _short_of_cap(payload: dict[str, JsonValue], cap: JsonValue) -> int | None:
    """Output tokens of JSON that ended mid-value short of the cap, or None where the cap may have cut it.

    Some providers report a response that hit the cap as an ordinary stop, so JSON ending mid-value is read as
    truncation unless usage shows the model stopped short: then the provider dropped the tail, and more room
    would not help.
    """
    try:
        written = token_count(object_value(payload.get("usage", {})).get("completion_tokens"))
    except (ValueError, TypeError, OverflowError):
        return None
    return written if isinstance(cap, int) and 0 < written < cap else None


def strict_schema(schema: JsonValue) -> JsonValue:
    """The schema with every property required and no defaults, as strict structured output demands.

    A field with a default is optional to pydantic, which strict mode rejects; the model instead writes the
    empty value, and validation accepts it as it would the default. An array's `maxItems` is left to validation
    too: a provider behind OpenRouter answered HTTP 400 to every read whose claims list carried `maxItems: 60`.

    A map with free keys is refused: Gemini through the Vercel AI Gateway returned such a map empty where
    OpenRouter's endpoint filled it, so the same request answers differently by route. A list of keyed entries
    says the same thing on both.
    """
    if isinstance(schema, list):
        return [strict_schema(item) for item in schema]
    if not isinstance(schema, dict):
        return schema
    result: dict[str, JsonValue] = {}
    for key, value in schema.items():
        if key in ("default", "maxItems"):
            continue
        if key == "additionalProperties" and isinstance(value, dict):
            raise ValueError("a strict schema cannot hold a map with free keys; use a list of keyed entries")
        # These map names to schemas, so a property called "default" is a name, not a keyword.
        if key in ("properties", "$defs") and isinstance(value, dict):
            result[key] = {name: strict_schema(item) for name, item in value.items()}
        else:
            result[key] = strict_schema(value)
    if isinstance(properties := schema.get("properties"), dict):
        result["required"] = list(properties)
    return result


def _grow_cap(body: dict[str, JsonValue], attempt: int, cap: int, purpose: LLMPurpose) -> None:
    """Make room for a response that ran into the output cap, or report it as truncated when it did so again.

    Repairing the JSON would send the same prompt under the same cap and be cut off again, so the one retry is
    spent on more room instead, and a second truncation is reported as what it is.
    """
    ceiling = body.get("max_tokens")
    if attempt == 1:
        raise LLMError(
            f"LLM response truncated at the {ceiling if ceiling is not None else 'no'} token output cap ({purpose.value})"
        )
    # No cap was sent, so there is nothing to grow: the model stopped on its own, and asking again under a larger
    # ceiling would only spend the one retry on the same reply. The caller's repair turn is the next move.
    if ceiling is None:
        raise LLMError(
            f"LLM response was cut off with no output cap set ({purpose.value}); it ended before the JSON did"
        )
    body["max_tokens"] = cap * _TRUNCATION_RETRY_FACTOR


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
    except (ValueError, TypeError, OverflowError):
        return CostLine(component=CostComponent.LLM, purpose=purpose, basis=CostBasis.UNKNOWN, dollars=None)


def _total_cost(costs: Sequence[CostLine], purpose: LLMPurpose) -> CostLine:
    basis = combined_basis(costs)
    return CostLine(
        component=CostComponent.LLM,
        purpose=purpose,
        basis=basis,
        dollars=None if basis is CostBasis.UNKNOWN else sum(line.dollars or 0 for line in costs),
        input_tokens=sum(line.input_tokens for line in costs),
        output_tokens=sum(line.output_tokens for line in costs),
    )


_BYOK_FEE = 0.05
"""OpenRouter's fee on a request sent with the caller's own provider key, as a share of the listed price, billed
beside the provider's own charge. A bound without it would fall short of that pair."""

_PER_REQUEST_PRICES = frozenset({"request", "web_search"})
"""OpenRouter price keys charged once per request; `image` is charged per input image and every other key per
token. A key added later is read as per token, which can only raise the bound."""


def _ceiling(payload: dict[str, JsonValue]) -> tuple[int, float, float, float] | None:
    """The context window, highest token price, per-request fees and per-image price across a model's endpoints.

    Routing can send a request to any of them, so the largest of each is taken. None when a price cannot be read
    or is negative, as OpenRouter lists a price set per request: no ceiling holds for those.
    """
    endpoints = object_value(payload.get("data")).get("endpoints")
    if not isinstance(endpoints, list) or not endpoints:
        return None
    context, per_token, per_request, per_image = 0, 0.0, 0.0, 0.0
    for entry in endpoints:
        endpoint = object_value(entry)
        context = max(context, token_count(endpoint.get("context_length")))
        request, image = 0.0, 0.0
        for key, value in object_value(endpoint.get("pricing")).items():
            if key == "discount":
                continue  # a discount lowers the charge, and leaving it out keeps the bound above it
            price = dollars(value)
            if key in _PER_REQUEST_PRICES:
                request += price
            elif key == "image":
                image = price
            else:
                per_token = max(per_token, price)
        per_request, per_image = max(per_request, request), max(per_image, image)
    return (context, per_token, per_request, per_image) if context else None


def _images(body: dict[str, JsonValue]) -> int:
    messages = body.get("messages")
    if not isinstance(messages, list):
        return 0
    return sum(
        1
        for message in messages
        if isinstance(message, dict) and isinstance(parts := message.get("content"), list)
        for part in parts
        if isinstance(part, dict) and part.get("type") == "image_url"
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
        provider_routing: JsonValue | None = OPENROUTER_PROVIDER_ROUTING,
        strict: bool = True,
        extra_body: Mapping[str, JsonValue] | None = None,
    ) -> None:
        """An empty `api_key` sends no Authorization header, which is what a keyless local server expects."""
        if clashes := sorted(RESERVED_BODY_KEYS.intersection(extra_body or {})):
            raise ValueError(f"extra request fields replace ones fastbrowse sets: {', '.join(clashes)}")
        self._api_key = api_key
        self._strict = strict
        self._extra_body = dict(extra_body or {})
        self._http = http
        self._base_url = base_url.rstrip("/")
        self._models = dict(models)
        self._reasoning_effort = reasoning_effort
        self._provider_routing = provider_routing
        self._ceilings: dict[str, tuple[int, float, float, float]] = {}

    def _headers(self) -> dict[str, str]:
        return {"Authorization": f"Bearer {self._api_key}"} if self._api_key else {}

    def _scrubbed(self, text: str) -> str:
        return text.replace(self._api_key, "[api key]") if self._api_key else text

    async def _request(
        self, body: dict[str, JsonValue], ledger: Ledger | None, usage: RequestUsage, purpose: LLMPurpose
    ) -> dict[str, JsonValue]:
        started = monotonic()
        try:
            response = await post_with_retry(
                self._http,
                f"{self._base_url}/chat/completions",
                body,
                self._headers(),
                call=f"llm {body.get('model')}",
                attempt_seconds=LLM_ATTEMPT_SECONDS,
                before_retry=None if ledger is None else lambda: ledger.reserve(CostComponent.LLM),
                usage=usage,
                settle_on_cancel=True,
            )
        except httpx.HTTPError as error:
            raise LLMError(f"LLM request could not be sent ({type(error).__name__})") from None
        if response is None:
            raise LLMRetriesExhausted(
                f"LLM transport failed after {usage.history(monotonic() - started)}; last: {usage.failures[-1]}"
            )
        if retryable(response):
            raise LLMRetriesExhausted(
                f"LLM request failed after {usage.history(monotonic() - started)}; last: {describe(response)}"
            )
        if not response.is_success:
            raise LLMError(f"LLM {describe(response)}")
        try:
            return json_object(response)
        except ValueError:
            raise LLMError(f"Invalid LLM response; HTTP {response.status_code}: {body_excerpt(response)}") from None

    async def _settled_cost(
        self, payload: dict[str, JsonValue], purpose: LLMPurpose, body: dict[str, JsonValue]
    ) -> CostLine:
        cost = await self._receipt_cost(payload, purpose)
        return cost if cost.dollars is not None else await self._bounded(cost, body)

    async def _receipt_cost(self, payload: dict[str, JsonValue], purpose: LLMPurpose) -> CostLine:
        cost = _cost(payload, purpose)
        generation_id = payload.get("id")
        if cost.dollars is not None or not isinstance(generation_id, str) or not generation_id:
            return cost
        host = httpx.URL(self._base_url).host
        if host not in {"openrouter.ai", "ai-gateway.vercel.sh"}:
            return cost
        # Usage receipts can lag the completion; recover that exact charge without generating the text again.
        for attempt in range(3):
            if attempt:
                await asyncio.sleep(1)
            try:
                async with asyncio.timeout(2.0):
                    response = await self._http.get(
                        f"{self._base_url}/generation",
                        params={"id": generation_id},
                        headers=self._headers(),
                        timeout=2.0,
                    )
                if not response.is_success:
                    continue
                receipt = object_value(json_object(response).get("data"))
                if receipt.get("id") != generation_id:
                    return cost
                amount = receipt.get("total_cost")
                if amount is not None:
                    basis = CostBasis.METERED
                    charge = dollars(amount)
                    byok = receipt.get("is_byok")
                    if byok is not None and not isinstance(byok, bool):
                        return cost
                    upstream_amount = receipt.get("upstream_inference_cost")
                    if byok is True and upstream_amount is None:
                        return cost
                    # BYOK inference is billed outside the gateway; a normal receipt already includes it.
                    if byok is not False and upstream_amount is not None:
                        upstream = dollars(upstream_amount)
                        if upstream:
                            basis = CostBasis.ESTIMATED
                            charge += upstream
                    return cost.model_copy(update={"basis": basis, "dollars": charge})
            except (httpx.HTTPError, TimeoutError, ValueError, TypeError, OverflowError):
                continue
        return cost

    async def _bounded(self, cost: CostLine, body: dict[str, JsonValue]) -> CostLine:
        """An unknown charge bounded from the model's listed prices, or left unknown when none can be read.

        A request that timed out, or a response without usage whose receipt never came, stopped every run with a
        dollar cap although it had spent a fraction of a cent. No call can bill more tokens than its context
        window holds, prompt and output together, so that window at the highest price is a true ceiling where the
        output cap would not be: some routes do not count reasoning against it.
        """
        model = body.get("model")
        if not isinstance(model, str) or httpx.URL(self._base_url).host != "openrouter.ai":
            return cost
        ceiling = self._ceilings.get(model)
        for attempt in range(0 if ceiling is not None else 3):
            if attempt:
                await asyncio.sleep(1)
            try:
                async with asyncio.timeout(2.0):
                    response = await self._http.get(
                        f"{self._base_url}/models/{model}/endpoints",
                        headers=self._headers(),
                        timeout=2.0,
                    )
            except (httpx.HTTPError, TimeoutError):
                continue
            if not response.is_success:
                continue
            try:
                ceiling = _ceiling(json_object(response))
            except (ValueError, TypeError, OverflowError):
                return cost
            if ceiling is None:
                return cost
            self._ceilings[model] = ceiling
            break
        if ceiling is None:
            return cost
        context, per_token, per_request, per_image = ceiling
        bound = (context * per_token + per_request + per_image * _images(body)) * (1 + _BYOK_FEE)
        return cost.model_copy(update={"basis": CostBasis.BOUNDED, "dollars": bound})

    async def generate[T: BaseModel](
        self,
        purpose: LLMPurpose,
        messages: Sequence[Message],
        schema: type[T],
        *,
        max_output_tokens: int = DEFAULT_MAX_OUTPUT_TOKENS,
        ledger: Ledger | None = None,
    ) -> Generation[T]:
        with call_span(f"llm:{purpose.value}", schema=schema.__name__):
            return await self._generate(purpose, messages, schema, max_output_tokens=max_output_tokens, ledger=ledger)

    async def _generate[T: BaseModel](
        self,
        purpose: LLMPurpose,
        messages: Sequence[Message],
        schema: type[T],
        *,
        max_output_tokens: int = DEFAULT_MAX_OUTPUT_TOKENS,
        ledger: Ledger | None = None,
    ) -> Generation[T]:
        model = self._models.get(purpose)
        if model is None:
            raise LLMError(f"No model configured for {purpose}")
        wire_messages = [_message(message) for message in messages]
        body: dict[str, JsonValue] = {
            **self._extra_body,
            "model": model,
            "messages": wire_messages,
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": schema.__name__,
                    # Strict mode needs every property required and no defaults; a decoder without strict
                    # support (some local vLLM builds) gets the model's own schema instead.
                    "schema": (
                        TypeAdapter(JsonValue).validate_python(strict_schema(schema.model_json_schema()))
                        if self._strict
                        else TypeAdapter(JsonValue).validate_python(schema.model_json_schema())
                    ),
                    "strict": self._strict,
                },
            },
        }
        # Zero is the caller saying no ceiling: fastbrowse never caps, truncates or crops an output token, so the
        # field is left off the request entirely rather than sent as a limit of zero.
        if max_output_tokens > 0:
            body["max_tokens"] = max_output_tokens
        else:
            body.pop("max_tokens", None)
        if self._provider_routing is not None:
            body["provider"] = self._provider_routing
        if self._reasoning_effort is not None:
            body["reasoning"] = {"effort": self._reasoning_effort.value}
        costs: list[CostLine] = []
        started = monotonic()
        try:
            short_retried = False
            for turn in range(3):
                # A reply the provider cut short is asked for again without spending the one retry the rest share.
                attempt = turn - int(short_retried)
                if ledger is not None:
                    ledger.reserve(CostComponent.LLM)
                usage = RequestUsage()
                try:
                    payload = await self._request(body, ledger, usage, purpose)
                except BaseException:
                    offset = len(costs)
                    costs.extend(_cost({}, purpose) for _ in range(usage.unaccounted_requests))

                    async def settle(responses: Sequence[httpx.Response], start: int) -> None:
                        # A request whose response arrived but was never delivered still carries its receipt.
                        for index, response in enumerate(responses, start=start):
                            try:
                                receipt = json_object(response)
                            except ValueError:
                                continue
                            costs[index] = _cost(receipt, purpose)
                            costs[index] = await self._settled_cost(receipt, purpose, body)
                        for index in range(start, len(costs)):
                            if costs[index].dollars is None:
                                costs[index] = await self._bounded(costs[index], body)

                    # Repeated cancellation must join receipt lookups before the run reports its final spend.
                    settling = asyncio.create_task(settle(usage.discarded_responses, offset))
                    cancelled = False
                    while not settling.done():
                        try:
                            await asyncio.shield(settling)
                        except asyncio.CancelledError:
                            cancelled = True
                    settling.result()
                    if cancelled:
                        raise asyncio.CancelledError from None
                    raise
                # Recorded before the envelope is read: a generation we cannot parse was still billed, and
                # dropping it would let an unaccounted request pass a dollar cap. A request sent but never
                # delivered carried the same prompt, so it is charged as the answer was; charging it as unknown
                # instead made every run with one slow call stop at its dollar cap.
                costs.append(with_discarded(_cost(payload, purpose), usage))
                costs[-1] = with_discarded(await self._settled_cost(payload, purpose, body), usage)
                if _truncated(payload):
                    _grow_cap(body, attempt, max_output_tokens, purpose)
                    continue
                try:
                    content = _content(payload)
                except (ValueError, TypeError, OverflowError) as error:
                    # An empty completion comes back intermittently (a dropped or refused generation); ask once more.
                    if attempt == 0:
                        continue
                    raise LLMError(f"Invalid completion envelope: {self._scrubbed(str(error))[:400]}") from None
                try:
                    data = schema.model_validate_json(content)
                except ValidationError as error:
                    # Paths and reasons are enough to repair the schema. A path is the model's own key, which can
                    # be anything it read, so it is scrubbed too.
                    detail = self._scrubbed(error_detail(error))
                    if _ends_mid_json(error):
                        if (written := _short_of_cap(payload, body.get("max_tokens"))) is None:
                            _grow_cap(body, attempt, max_output_tokens, purpose)
                        elif short_retried:
                            ceiling = body.get("max_tokens")
                            raise LLMRetriesExhausted(
                                f"LLM response ended mid-JSON at {written} of {ceiling if ceiling is not None else 'no'}"
                                " output tokens twice"
                            ) from None
                        else:
                            short_retried = True
                        continue
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
        except asyncio.CancelledError:
            if ledger is not None:
                # An unknown receipt must not replace cancellation with a new budget failure.
                ledger.lines.extend(costs)
            raise
        except (LLMError, BudgetExceeded):
            _charge(ledger, costs)
            raise
        raise AssertionError("unreachable")


def _charge(ledger: Ledger | None, costs: Sequence[CostLine]) -> None:
    """Record what a failed generation was billed. A success hands its cost to the caller to record instead."""
    if ledger is not None:
        ledger.record(*costs)
