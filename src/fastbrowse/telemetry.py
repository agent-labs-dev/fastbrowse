"""Spend accounting a caller's own model client sees, and the trace of the runs made in this process."""

from collections.abc import Generator
from contextlib import contextmanager
from contextvars import ContextVar
from typing import Protocol

from fastbrowse.models import BudgetStop, CostBreakdown, CostComponent, CostLine, Limits, Status


class BudgetExceeded(RuntimeError):
    def __init__(self, reason: str, *, budget: BudgetStop) -> None:
        super().__init__(reason)
        self.status = Status.BUDGET_EXCEEDED
        self.budget = budget


class Ledger(Protocol):
    """The run's ledger, which the core keeps, as a caller's `LLMClient` is handed it for one call.

    `reserve` before a request and `record` its receipt after; either raises `BudgetExceeded` when the call would
    break a limit. Calls sent together are reserved together, so a limit reached partway counts none of them.
    """

    @property
    def limits(self) -> Limits: ...

    @property
    def lines(self) -> list[CostLine]: ...

    @property
    def capped(self) -> bool:
        """Whether a dollar or call cap is set, so a call waiting on a sibling's receipt must keep waiting."""
        ...

    def reserve(self, component: CostComponent, estimate_dollars: float = 0.0, calls: int = 1) -> None: ...

    def check(self, extra_dollars: float = 0.0) -> None: ...

    def check_spend(self, extra_dollars: float = 0.0) -> None: ...

    def record(self, *lines: CostLine) -> None: ...

    def breakdown(self) -> CostBreakdown: ...


_trace_events: ContextVar[list[object] | None] = ContextVar("trace_events", default=None)


def collecting() -> list[object] | None:
    """The list `traced()` is collecting into in this context, if one is, which `fastbrowse.native` adds the core's
    events to."""
    return _trace_events.get()


@contextmanager
def traced() -> Generator[list[object]]:
    """The trace events of the runs made in this context and its child tasks, so runs that overlap keep separate
    traces: each a dict naming its `event`. A run given neither `jev` nor `llm` begins with `route`, whose
    `providers` names the Jev and LLM providers it calls. Each model request that failed transiently is a
    `request_transient` with its `began` and `ended` on `time.monotonic()`, beside the model requests' other events."""
    events: list[object] = []
    token = _trace_events.set(events)
    try:
        yield events
    finally:
        _trace_events.reset(token)
