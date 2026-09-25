"""Contract for Jev (TypeSafe System One): typed questions in, calibrated answers out."""

from collections.abc import Mapping, Sequence
from typing import Annotated, Literal, Protocol

from pydantic import Field, JsonValue

from fastbrowse.models import CostLine, Frozen, Unavailable

JEV_MODEL = "jev-1.13.0"
MAX_CHOICE_OPTIONS = 255
JEV_DOLLARS_PER_INPUT_TOKEN = 0.042 / 1_000_000
"""Input price; output is free."""


class ChoiceQuestion(Frozen):
    type: Literal["choice"] = "choice"
    instructions: str
    criteria: Mapping[str, JsonValue]
    """Option key -> description (string, object with what/not_for/examples, or null)."""


class NoulQuestion(Frozen):
    type: Literal["noul"] = "noul"
    instructions: str
    true: str | None = None
    false: str | None = None


class ScoreQuestion(Frozen):
    type: Literal["score"] = "score"
    instructions: str
    criteria: tuple[str, ...] = Field(min_length=2, max_length=10)


type Question = Annotated[ChoiceQuestion | NoulQuestion | ScoreQuestion, Field(discriminator="type")]


class ChoiceAnswer(Frozen):
    type: Literal["choice"] = "choice"
    choice: str
    probabilities: Mapping[str, float]
    confidence: float = Field(ge=0, le=1)


class NoulAnswer(Frozen):
    type: Literal["noul"] = "noul"
    probability: float = Field(ge=0, le=1)


class ScoreAnswer(Frozen):
    type: Literal["score"] = "score"
    score: float
    """Probability-weighted mean over the levels, so fractional (e.g. 1.3)."""
    probabilities: Mapping[str, float]
    confidence: float = Field(ge=0, le=1)


type Answer = Annotated[ChoiceAnswer | NoulAnswer | ScoreAnswer, Field(discriminator="type")]


class Evaluation(Frozen):
    model: str
    answers: Mapping[str, Answer]
    input_tokens: int = Field(ge=0)
    cost: CostLine
    requests: int = Field(default=1, ge=0)
    """HTTP requests, including retries and hedges, made to obtain these answers."""


class JevError(RuntimeError):
    """Transport or validation failure; the request may be retried only if it caused no browser action."""


class JevInputTooLarge(JevError):
    """The provider rejected the request for exceeding its input token limits."""


class JevTransportFailed(JevError, Unavailable):
    """Every attempt failed in transport. Not a provider verdict, so it does not move a run to the backup."""


class JevRetriesExhausted(JevError, Unavailable):
    """A retryable HTTP status outlasted the provider's retry budget."""

    def __init__(
        self,
        message: str,
        *,
        seconds: float,
        unaccounted_requests: int,
        requests: int = 0,
        answered: Sequence[Evaluation] = (),
    ) -> None:
        super().__init__(message)
        self.seconds = seconds
        self.unaccounted_requests = unaccounted_requests
        self.requests = requests
        self.answered = tuple(answered)
        """Singles answered before a split batch ran out of retries; paid for, and kept by a failover."""


class JevClient(Protocol):
    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        """Answer every question against one state. Raises `JevInputTooLarge` or `JevError`."""
        ...
