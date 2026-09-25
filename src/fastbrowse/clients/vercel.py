"""Vercel AI Gateway v4 evaluation transport.

Integrator's observed response, with choice, noul (wire boolean), and score:
{"answers":{"green":{"type":"boolean","probability":0.02},
"col":{"type":"choice","choice":"blue","probabilities":{"blue":1,"red":0}},
"sc":{"type":"score","score":1.99,"probabilities":{"0":0,"1":0,"2":1}}},
"rounding":{"probabilityDecimals":2,"scoreDecimals":2},
"usage":{"inputTokens":389,"outputTokens":61},"warnings":[],
"providerMetadata":{"typesafe":{"confidence":{"col":1,"sc":0.99}},
"gateway":{"cost":"0.000016338"}}}

Noul questions use wire type boolean; score criteria are arrays. Rounded
probabilities may be JSON integers. This implementation makes no discovery calls.
"""

from collections.abc import Mapping
from time import monotonic

import httpx
from pydantic import JsonValue

from fastbrowse.clients.validation import (
    RequestUsage,
    asking_open,
    dollars,
    error_detail,
    estimated_cost,
    json_object,
    object_value,
    parse_answers,
    post,
    response_error,
    token_count,
    wire_questions,
    with_discarded,
)
from fastbrowse.jev import Evaluation, Question
from fastbrowse.models import CostBasis, CostComponent, CostLine

GATEWAY_URL = "https://ai-gateway.vercel.sh"


class VercelGatewayJevClient:
    def __init__(self, api_key: str, *, http: httpx.AsyncClient, base_url: str = GATEWAY_URL) -> None:
        self._api_key = api_key
        self._http = http
        self._base_url = base_url.rstrip("/")

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        return await asking_open(questions, lambda asked: self._ask(state, asked), "typesafe-ai/jev")

    async def _ask(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        started = monotonic()
        sent = RequestUsage()
        response = await post(
            self._http,
            f"{self._base_url}/v4/ai/evaluation-model",
            self._api_key,
            {"state": state, "questions": wire_questions(questions, gateway=True)},
            {
                "ai-gateway-auth-method": "api-key",
                "ai-gateway-protocol-version": "0.0.1",
                "ai-model-id": "typesafe-ai/jev",
                "ai-evaluation-model-specification-version": "4",
            },
            usage=sent,
        )
        try:
            payload = json_object(response)
            usage = object_value(payload.get("usage"))
            tokens = token_count(usage.get("inputTokens"))
            output_tokens = token_count(usage.get("outputTokens", 0))
            metadata = object_value(payload.get("providerMetadata", {}))
            confidence = object_value(object_value(metadata.get("typesafe", {})).get("confidence", {}))
            cost_value = object_value(metadata.get("gateway", {})).get("cost")
            cost = (
                estimated_cost(tokens, output_tokens)
                if cost_value is None
                else CostLine(
                    component=CostComponent.JEV,
                    basis=CostBasis.METERED,
                    dollars=dollars(cost_value),
                    input_tokens=tokens,
                    output_tokens=output_tokens,
                )
            )
            return Evaluation(
                model="typesafe-ai/jev",
                answers=parse_answers(payload.get("answers"), questions, gateway=True, confidence=confidence),
                input_tokens=tokens,
                cost=with_discarded(cost, sent).model_copy(update={"seconds": monotonic() - started}),
            )
        except (ValueError, TypeError, OverflowError) as error:
            raise response_error(response, f"Invalid gateway response ({error_detail(error)})") from None
