"""Contract for the injected LLM: structured generation for a named purpose."""

from collections.abc import Sequence
from typing import Literal, Protocol

from pydantic import BaseModel

from fastbrowse.models import CostLine, Frozen, LLMPurpose, Unavailable
from fastbrowse.telemetry import Ledger

# Planning and field responses are short; readers and composers pass their larger configured caps.
DEFAULT_MAX_OUTPUT_TOKENS = 2000


class Message(Frozen):
    role: Literal["system", "user", "assistant"]
    content: str
    images: tuple[bytes, ...] = ()
    """JPEG or PNG bytes; clients that cannot see images must raise `LLMError`, never drop them silently."""


class Generation[T: BaseModel](Frozen):
    data: T
    cost: CostLine


class LLMError(RuntimeError):
    """The model failed to return data matching the schema, or the transport failed."""


class LLMRetriesExhausted(LLMError, Unavailable):
    """A retryable HTTP status or a transport failure outlasted the provider's retry budget."""


class LLMClient(Protocol):
    async def generate[T: BaseModel](
        self,
        purpose: LLMPurpose,
        messages: Sequence[Message],
        schema: type[T],
        *,
        max_output_tokens: int = DEFAULT_MAX_OUTPUT_TOKENS,
        ledger: Ledger | None = None,
    ) -> Generation[T]:
        """Return `schema`-validated data. `purpose` lets callers route models and attribute cost.

        The client reserves against `ledger` once per HTTP request it makes, because only it knows
        how many a repair or a re-ask costs; a caller that reserved instead would undercount them.
        Cancellation must stop owned requests, record their settled or unknown charges, and propagate
        `CancelledError`. Clients and their transports must finish cancellation cleanup; the run joins
        them before returning so no paid request outlives its cost result.
        """
        ...
