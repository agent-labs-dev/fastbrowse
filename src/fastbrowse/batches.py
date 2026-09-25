"""Many independent questions against one shared state, packed under Jev's input limits and sent together."""

import asyncio
import json
from collections.abc import Mapping

from pydantic import JsonValue

from fastbrowse.clients.validation import jev_spend
from fastbrowse.config import TokenBudget
from fastbrowse.jev import JEV_DOLLARS_PER_INPUT_TOKEN, Answer, JevClient, JevError, Question
from fastbrowse.models import CostComponent, CostLine, Frozen
from fastbrowse.telemetry import Ledger


class Answered(Frozen):
    answers: Mapping[str, Answer]
    """Answers from every batch Jev accepted; a key left out went unscored."""
    cost: tuple[CostLine, ...]
    input_tokens: int
    requests: int


async def evaluate_batches(
    jev: JevClient,
    state: JsonValue,
    questions: Mapping[str, Question],
    *,
    tokens: TokenBudget,
    ledger: Ledger | None,
) -> Answered | None:
    """None when no batch was answered; a question too large to send with the state alone goes unscored."""
    ratio = tokens.chars_per_token
    state_size = len(json.dumps(state)) / ratio
    batches: list[dict[str, Question]] = []
    batch: dict[str, Question] = {}
    batch_size = 0.0
    for key, question in questions.items():
        size = len(question.model_dump_json()) / ratio
        if (
            state_size + size > tokens.state_plus_largest_question
            or state_size + size > tokens.state_plus_all_questions
        ):
            continue
        if batch and state_size + batch_size + size > min(tokens.batch_tokens, tokens.state_plus_all_questions):
            batches.append(batch)
            batch = {}
            batch_size = 0.0
        batch[key] = question
        batch_size += size
    if batch:
        batches.append(batch)
    if not batches:
        return None

    if ledger is not None:
        # The batches run together, so a limit checked per batch would pass them all and learn of the overrun only
        # once billed: the whole pass is priced, and every call counted, before any is sent.
        sent = sum(len(q.model_dump_json()) for b in batches for q in b.values()) / ratio
        ledger.reserve(
            CostComponent.JEV, (len(batches) * state_size + sent) * JEV_DOLLARS_PER_INPUT_TOKEN, calls=len(batches)
        )

    paid: list[CostLine] = []

    async def evaluate(questions: Mapping[str, Question]) -> Mapping[str, Answer] | None:
        try:
            evaluation = await jev.evaluate(state, questions)
        except JevError:
            return None
        paid.append(evaluation.cost)
        nonlocal input_tokens, requests
        input_tokens += evaluation.input_tokens
        requests += evaluation.requests
        return evaluation.answers

    input_tokens = 0
    requests = 0
    try:
        with jev_spend(paid, ledger=ledger):
            tasks = [asyncio.create_task(evaluate(batch)) for batch in batches]
            try:
                results = await asyncio.gather(*tasks)
            except BaseException:
                # A budget stop in one batch must cancel its peers before their paid singles are collected.
                for task in tasks:
                    task.cancel()
                await asyncio.gather(*tasks, return_exceptions=True)
                raise
    finally:
        if ledger is not None:
            # A deadline can cancel the pass after some batches were billed. Their cost is kept without the limit
            # check, so the cancellation, not a BudgetExceeded raised over it, is what stops the run.
            ledger.lines.extend(paid)
    if ledger is not None:
        ledger.check()
    if not paid:
        return None
    return Answered(
        answers={key: answer for result in results if result for key, answer in result.items()},
        cost=tuple(paid),
        input_tokens=input_tokens,
        requests=requests,
    )
