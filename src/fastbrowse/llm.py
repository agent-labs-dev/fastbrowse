"""Contract for the injected LLM: structured generation for a named purpose."""

from collections.abc import Awaitable, Sequence
from typing import Literal, Protocol

from pydantic import BaseModel

from fastbrowse.models import CostLine, Frozen, LLMPurpose, Unavailable
from fastbrowse.telemetry import Ledger

# Fastbrowse never caps, truncates or crops an output token: 0 means no ceiling, so the client leaves the field off
# the request and the schema and the model's own maximum are the only bounds.
DEFAULT_MAX_OUTPUT_TOKENS = 0


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


async def recorded[T: BaseModel](generating: Awaitable[Generation[T]], ledger: Ledger | None) -> Generation[T]:
    """The generation, its receipt already in `ledger`.

    A successful generation leaves its receipt to the caller. A call run as a task beside another returned into a
    task that could be cancelled or discarded before its caller recorded it: a verifier that finished while the
    audit beside it failed was paid for and never billed. Recorded in the task that made it, a finished call is
    a billed call, whatever happens to its sibling."""
    generated = await generating
    if ledger is not None:
        ledger.record(generated.cost)
    return generated
