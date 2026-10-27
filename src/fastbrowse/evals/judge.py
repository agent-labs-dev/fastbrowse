"""Grading an answer with a judge, never with a string match.

A substring or a regex over free text grades the wording rather than the fact. It fails a run that states the
same thing in other words, and passes one that repeats the expected number in an unrelated sentence. Every
answer is graded by Jev instead: one typed question per claim, answered against the task, the answer and any
structured output, so a figure written with a currency, a comma or a trailing zero is the same figure.

The judge is the harness's, not the run's, so it calls Jev itself rather than through either core's client.
"""

import os
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Protocol

import httpx

from fastbrowse.models import JsonValue

ACCEPTED = 0.5
"""The probability at which Jev is taken to have said a claim holds."""

OPENROUTER_URL = "https://openrouter.ai/api"
OPENROUTER_MODEL = "jev-1.13"
TYPESAFE_URL = "https://api.typesafe.ai"
TYPESAFE_MODEL = "jev-1.13.0"


@dataclass(frozen=True, slots=True)
class Claim:
    """One fact a run's output must state, in the words a judge can check against the task.

    Written as a sentence about the output ("The answer gives the total as 43.25") so the same claim reads the
    same whether the run answered in prose or through an output schema.
    """

    statement: str


class Judge(Protocol):
    async def evaluate(self, state: JsonValue, questions: Mapping[str, JsonValue]) -> Mapping[str, float]:
        """P(yes) for each yes/no question, asked in Jev's wire form against one state."""
        ...


class Jev:
    """Jev's System One endpoint, the one OpenRouter and the direct API both serve.

    The request body is the one a run's own Jev client sends, so a judgement recorded in a benchmark cassette
    replays under the same key.
    """

    def __init__(self, http: httpx.AsyncClient, *, api_key: str, base_url: str, model: str) -> None:
        self._http = http
        self._api_key = api_key
        self._url = base_url.rstrip("/") + "/v1/systemone"
        self._model = model

    async def evaluate(self, state: JsonValue, questions: Mapping[str, JsonValue]) -> Mapping[str, float]:
        response = await self._http.post(
            self._url,
            json={"model": self._model, "state": state, "questions": dict(questions)},
            headers={"Authorization": f"Bearer {self._api_key}"},
        )
        response.raise_for_status()
        answers = response.json()["answers"]
        return {key: float(answers[key]["noul"]) for key in questions}


def jev_from_environment(http: httpx.AsyncClient) -> Jev:
    """The judge keyed from OPENROUTER_API_KEY, else TYPESAFE_API_KEY."""
    if key := os.environ.get("OPENROUTER_API_KEY"):
        return Jev(http, api_key=key, base_url=OPENROUTER_URL, model=OPENROUTER_MODEL)
    if key := os.environ.get("TYPESAFE_API_KEY"):
        return Jev(http, api_key=key, base_url=TYPESAFE_URL, model=TYPESAFE_MODEL)
    raise RuntimeError("the judge needs OPENROUTER_API_KEY or TYPESAFE_API_KEY")


async def judge(
    jev: Judge,
    *,
    task: str,
    answer: str | None = None,
    data: JsonValue = None,
    submitted: Mapping[str, object] | None = None,
    claims: Sequence[Claim] = (),
) -> str | None:
    """Why the run's output does not state every claim, or None when it states them all.

    One Jev request covers every claim of a run, so a task costs one judgement however many facts it asks for.
    """
    if not claims:
        return None
    state: dict[str, JsonValue] = {"task": task, "answer": (answer or "").strip() or None, "data": data}
    if submitted:
        state["submitted"] = {name: str(value) for name, value in submitted.items()}
    questions: dict[str, JsonValue] = {
        f"claim_{index}": {
            "instructions": (
                f"The task asked for: {task}\n"
                "The fields below are what the run returned: its answer, its structured output and, where it "
                "submitted a form, the fields it sent. Judge only whether each claim holds of what the run "
                "returned, not how it was worded. A figure written with a currency, a thousands separator or a "
                "trailing zero is the same figure, and a fact stated in other words is the same fact.\n\n"
                f"Does this hold: {claim.statement}"
            ),
            "type": "noul",
            "criteria": {
                "true": "What the run returned states it.",
                "false": "What the run returned does not state it, or states something else.",
            },
        }
        for index, claim in enumerate(claims)
    }
    probabilities = await jev.evaluate(state, questions)
    for index, claim in enumerate(claims):
        probability = probabilities.get(f"claim_{index}")
        if probability is None or probability < ACCEPTED:
            return f"the output does not hold: {claim.statement} (answer {answer!r}, data {data!r})"
    return None
