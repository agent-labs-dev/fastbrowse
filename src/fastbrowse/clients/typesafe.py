"""Direct TypeSafe System One transport, using caller-owned HTTP resources."""

from collections.abc import Mapping
from time import monotonic

import httpx
from pydantic import JsonValue

from fastbrowse.clients.validation import (
    RequestUsage,
    asking_open,
    asking_split,
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
from fastbrowse.jev import JEV_MODEL, Evaluation, Question

TYPESAFE_URL = "https://api.typesafe.ai"


class TypeSafeJevClient:
    def __init__(
        self,
        api_key: str,
        *,
        http: httpx.AsyncClient,
        base_url: str = TYPESAFE_URL,
        model: str = JEV_MODEL,
    ) -> None:
        self._api_key = api_key
        self._http = http
        self._base_url = base_url.rstrip("/")
        self._model = model

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        return await asking_open(
            questions,
            lambda asked: asking_split(asked, lambda batch, attempt, split: self._ask(state, batch, attempt, split)),
            self._model,
        )

    async def _ask(
        self, state: JsonValue, questions: Mapping[str, Question], start_attempt: int, split_batch: bool
    ) -> Evaluation:
        started = monotonic()
        sent = RequestUsage()
        response = await post(
            self._http,
            f"{self._base_url}/v1/systemone",
            self._api_key,
            {"model": self._model, "state": state, "questions": wire_questions(questions)},
            usage=sent,
            start_attempt=start_attempt,
            split_batch=split_batch,
        )
        try:
            payload = json_object(response)
            usage = object_value(payload.get("usage"))
            tokens = token_count(usage.get("input_tokens"))
            model = payload.get("model", self._model)
            if not isinstance(model, str):
                raise ValueError("invalid model name")
            return Evaluation(
                requests=sent.requests,
                model=model,
                answers=parse_answers(payload.get("answers"), questions),
                input_tokens=tokens,
                cost=with_discarded(
                    estimated_cost(tokens, token_count(usage.get("output_tokens", 0))), sent
                ).model_copy(update={"seconds": monotonic() - started}),
            )
        except (ValueError, TypeError, OverflowError) as error:
            raise response_error(response, f"Invalid Jev response ({error_detail(error)})") from None
