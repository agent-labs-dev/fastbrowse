"""Grading an answer with a judge, never with a string match.

A substring or a regex over free text grades the wording rather than the fact. It fails a run that states the
same thing in other words, and passes one that repeats the expected number in an unrelated sentence. Every
answer is graded by Jev instead: one typed question per claim, answered against the task, the answer and any
structured output, so a figure written with a currency, a comma or a trailing zero is the same figure.

The judge is the harness's, not the run's, so it calls Jev itself rather than through either core's client.
"""

import asyncio
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Protocol

import httpx

from fastbrowse.evals.harness import environment
from fastbrowse.models import JsonValue

ACCEPTED = 0.5
"""The probability at which Jev is taken to have said a claim holds."""

OPENROUTER_URL = "https://openrouter.ai/api"
OPENROUTER_MODEL = "jev-1.14"
TYPESAFE_URL = "https://api.typesafe.ai"
TYPESAFE_MODEL = "jev-1.14.0"
GATEWAY_URL = "https://ai-gateway.vercel.sh"

RETRY_DELAYS = (0.5, 1.5, 4.0, 8.0)
"""Seconds before each retry of a judgement the provider failed transiently, so a busy provider costs a campaign
a few seconds rather than the row of a run already paid for."""

RETRYABLE_STATUS = frozenset({408, 429, 500, 502, 503, 504, 529})


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
        self._base_url = base_url.rstrip("/")
        self._model = model

    async def evaluate(self, state: JsonValue, questions: Mapping[str, JsonValue]) -> Mapping[str, float]:
        url, body, headers = self._request(state, questions)
        headers = {"Authorization": f"Bearer {self._api_key}", **headers}
        for delay in (*RETRY_DELAYS, None):
            try:
                response = await self._http.post(url, json=body, headers=headers)
            except httpx.TransportError:
                if delay is None:
                    raise
            else:
                if delay is None or response.status_code not in RETRYABLE_STATUS:
                    response.raise_for_status()
                    answers: Mapping[str, Mapping[str, float]] = response.json()["answers"]
                    return {key: self._probability(answers[key]) for key in questions}
            await asyncio.sleep(delay)
        raise AssertionError("unreachable: the last attempt returns or raises")

    def _request(
        self, state: JsonValue, questions: Mapping[str, JsonValue]
    ) -> tuple[str, dict[str, JsonValue], dict[str, str]]:
        return (
            f"{self._base_url}/v1/systemone",
            {"model": self._model, "state": state, "questions": dict(questions)},
            {},
        )

    def _probability(self, answer: Mapping[str, float]) -> float:
        return float(answer["noul"])


class GatewayJev(Jev):
    """Jev through the Vercel AI Gateway, which names a yes/no question `boolean` and its P(yes) `probability`."""

    def __init__(self, http: httpx.AsyncClient, *, api_key: str, base_url: str = GATEWAY_URL) -> None:
        super().__init__(http, api_key=api_key, base_url=base_url, model="typesafe-ai/jev-1.14.0")

    def _request(
        self, state: JsonValue, questions: Mapping[str, JsonValue]
    ) -> tuple[str, dict[str, JsonValue], dict[str, str]]:
        wire: dict[str, JsonValue] = {
            key: {**question, "type": "boolean"} if isinstance(question, dict) else question
            for key, question in questions.items()
        }
        headers = {
            "ai-gateway-auth-method": "api-key",
            "ai-gateway-protocol-version": "0.0.1",
            "ai-model-id": self._model,
            "ai-evaluation-model-specification-version": "4",
        }
        return f"{self._base_url}/v4/ai/evaluation-model", {"state": state, "questions": wire}, headers

    def _probability(self, answer: Mapping[str, float]) -> float:
        return float(answer["probability"])


JEV_KEYS = {"openrouter": "OPENROUTER_API_KEY", "typesafe": "TYPESAFE_API_KEY", "gateway": "AI_GATEWAY_API_KEY"}
"""Each Jev source and its key, in the order the core picks the first keyed one when FASTBROWSE_DECISION_SOURCE is
unset."""


def jev_source(env: Mapping[str, str]) -> str:
    """The source Jev starts on: FASTBROWSE_DECISION_SOURCE, else the first keyed one, else the gateway."""
    if chosen := env.get("FASTBROWSE_DECISION_SOURCE"):
        if chosen not in JEV_KEYS:
            raise ValueError(f"FASTBROWSE_DECISION_SOURCE is {chosen!r}, not one of {', '.join(JEV_KEYS)}")
        return chosen
    return next((source for source, key in JEV_KEYS.items() if env.get(key)), "gateway")


def jev_from_environment(http: httpx.AsyncClient) -> Jev:
    """The judge on the source a run would start on."""
    # Matched without case, as the core reads its settings.
    env = {name.upper(): value for name, value in environment().items()}
    source = jev_source(env)
    key = env.get(JEV_KEYS[source])
    if not key:
        raise RuntimeError(f"the judge needs {JEV_KEYS[source]} for the {source} route")
    if source == "typesafe":
        return Jev(http, api_key=key, base_url=TYPESAFE_URL, model=TYPESAFE_MODEL)
    if source == "openrouter":
        return Jev(http, api_key=key, base_url=OPENROUTER_URL, model=OPENROUTER_MODEL)
    return GatewayJev(http, api_key=key)


async def judge(
    jev: Judge,
    *,
    task: str,
    answer: str | None = None,
    data: JsonValue = None,
    submitted: Mapping[str, list[dict[str, str]]] | None = None,
    claims: Sequence[Claim] = (),
) -> str | None:
    """Why the run's output does not state every claim, or None when it states them all.

    One Jev request covers every claim of a run, so a task costs one judgement however many facts it asks for.
    """
    if not claims:
        return None
    state: dict[str, JsonValue] = {"task": task, "answer": (answer or "").strip() or None, "data": data}
    if submitted:
        state["submitted"] = {
            path: [{name: value for name, value in post.items()} for post in posts] for path, posts in submitted.items()
        }
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
