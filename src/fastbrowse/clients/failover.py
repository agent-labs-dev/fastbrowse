"""Keep a run on its backup after the chosen Jev provider exhausts its retries."""

import logging
from collections.abc import Mapping

from pydantic import JsonValue

from fastbrowse.clients.validation import (
    estimated_cost,
    jev_spend,
    merged_cost,
    record_jev_spend,
)
from fastbrowse.jev import Evaluation, JevClient, JevRetriesExhausted, Question
from fastbrowse.models import CostBasis, CostLine
from fastbrowse.telemetry import trace

logger = logging.getLogger(__name__)


class FailoverJevClient:
    def __init__(self, primary: JevClient, backup: JevClient) -> None:
        self._active = primary
        self._backup = backup

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        paid: list[CostLine] = []
        try:
            with jev_spend(paid):
                result = await self._evaluate(state, questions)
        except BaseException:
            record_jev_spend(paid)
            raise
        if paid:
            cost = merged_cost([*paid, result.cost]).model_copy(update={"seconds": result.cost.seconds})
            result = result.model_copy(update={"cost": cost})
        return result

    async def _evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        client = self._active
        try:
            return await client.evaluate(state, questions)
        except JevRetriesExhausted as error:
            if client is self._backup:
                raise
            # Calls already in flight may fail together; assigning the same backup without awaiting cannot flap.
            self._active = self._backup
            logger.warning("jev: %s; moving this run to the backup provider", error)
            trace("jev_failover", reason=str(error))
            answered = error.answered
            held = {key: answer for result in answered for key, answer in result.answers.items()}
            remaining = {key: question for key, question in questions.items() if key not in held}
            try:
                result = await self._backup.evaluate(state, remaining)
            except JevRetriesExhausted as backup_error:
                raise JevRetriesExhausted(
                    str(backup_error),
                    seconds=error.seconds + backup_error.seconds,
                    unaccounted_requests=error.unaccounted_requests + backup_error.unaccounted_requests,
                    requests=error.requests + backup_error.requests,
                    answered=(*error.answered, *backup_error.answered),
                ) from backup_error

            cost = result.cost
            if error.unaccounted_requests:
                # Only unanswered requests may be billed. Estimate their inputs without multiplying backup hedges.
                discarded = estimated_cost(result.input_tokens * error.unaccounted_requests)
                cost = cost.model_copy(
                    update={
                        "basis": CostBasis.UNKNOWN if cost.dollars is None else CostBasis.ESTIMATED,
                        "dollars": None if cost.dollars is None else cost.dollars + (discarded.dollars or 0),
                        "input_tokens": cost.input_tokens + discarded.input_tokens,
                    }
                )
            return Evaluation(
                model=result.model,
                answers={**held, **result.answers},
                input_tokens=result.input_tokens + sum(part.input_tokens for part in answered),
                requests=result.requests + error.requests,
                cost=cost.model_copy(update={"seconds": error.seconds + (cost.seconds or 0)}),
            )
