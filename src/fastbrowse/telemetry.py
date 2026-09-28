"""Spend and call accounting against `Limits`, checked before each call rather than discovered after."""

import logging
from collections.abc import Generator, Iterable
from contextlib import contextmanager
from contextvars import ContextVar
from dataclasses import dataclass, field
from decimal import Decimal
from time import monotonic

from fastbrowse.models import BudgetStop, CostBreakdown, CostComponent, CostLine, Limits, Status


def _dollars(amount: float) -> str:
    """A limit as someone typed it: fixed-point, where `str` would print 1e-05 for a small one."""
    return format(Decimal(repr(amount)), "f")


class BudgetExceeded(RuntimeError):
    def __init__(self, reason: str, *, budget: BudgetStop) -> None:
        super().__init__(reason)
        self.status = Status.BUDGET_EXCEEDED
        self.budget = budget


@dataclass(slots=True)
class Ledger:
    limits: Limits
    started: float = field(default_factory=monotonic)
    lines: list[CostLine] = field(default_factory=list[CostLine])
    steps: int = 0
    jev_calls: int = 0
    llm_calls: int = 0

    def reserve(self, component: CostComponent, estimate_dollars: float = 0.0, calls: int = 1) -> None:
        """Raise before calls that would break a limit; `record` the actual lines afterwards.

        Calls sent together are reserved together, so a limit reached partway counts none of them.
        """
        match component:
            case CostComponent.JEV:
                if self.jev_calls + calls > self.limits.max_jev_calls:
                    raise BudgetExceeded(
                        f"Jev call limit {self.limits.max_jev_calls} reached",
                        budget=BudgetStop(resource="jev_calls", limit=self.limits.max_jev_calls),
                    )
            case CostComponent.LLM:
                if self.llm_calls + calls > self.limits.max_llm_calls:
                    raise BudgetExceeded(
                        f"LLM call limit {self.limits.max_llm_calls} reached",
                        budget=BudgetStop(resource="llm_calls", limit=self.limits.max_llm_calls),
                    )
            case CostComponent.BROWSER | CostComponent.PROXY:
                pass
        self.check(estimate_dollars)
        if self.limits.max_dollars is not None and self.breakdown().known_dollars >= self.limits.max_dollars:
            raise BudgetExceeded(
                f"spend limit ${_dollars(self.limits.max_dollars)} reached",
                budget=BudgetStop(resource="dollars", limit=self.limits.max_dollars),
            )
        # Failed requests still consume a call, including retries after an input-size rejection.
        if component is CostComponent.JEV:
            self.jev_calls += calls
        elif component is CostComponent.LLM:
            self.llm_calls += calls

    def check(self, extra_dollars: float = 0.0) -> None:
        limits = self.limits
        if limits.max_seconds is not None and monotonic() - self.started > limits.max_seconds:
            raise BudgetExceeded(
                f"time limit {limits.max_seconds}s reached",
                budget=BudgetStop(resource="seconds", limit=limits.max_seconds),
            )
        if limits.max_dollars is not None:
            spent = self.breakdown()
            # An unpriced call could have spent anything, so a dollar cap cannot be enforced past it.
            if spent.has_unknown:
                raise BudgetExceeded(
                    f"spend limit ${_dollars(limits.max_dollars)} cannot be enforced: a call reported no cost",
                    budget=BudgetStop(resource="dollars", limit=limits.max_dollars),
                )
            if spent.known_dollars + extra_dollars > limits.max_dollars:
                raise BudgetExceeded(
                    f"spend limit ${_dollars(limits.max_dollars)} reached",
                    budget=BudgetStop(resource="dollars", limit=limits.max_dollars),
                )
        if self.steps >= limits.max_steps:
            raise BudgetExceeded(
                f"step limit {limits.max_steps} reached", budget=BudgetStop(resource="steps", limit=limits.max_steps)
            )

    def record(self, *lines: CostLine) -> None:
        self.lines.extend(lines)
        self.check()

    def breakdown(self) -> CostBreakdown:
        return CostBreakdown(lines=tuple(self.lines))


TRACE = logging.getLogger("fastbrowse.trace")
"""Why a run went the way it did, one record per judgement, for evals to keep beside each result: ids, verdicts,
scores and redacted addresses, never page text. Nothing is built unless DEBUG is enabled on this logger."""


def trace(event: str, **fields: object) -> None:
    if TRACE.isEnabledFor(logging.DEBUG):
        TRACE.debug(event, extra={"trace": {"event": event, **fields}})


_trace_events: ContextVar[list[object] | None] = ContextVar("trace_events", default=None)


class _Collect(logging.Handler):
    def __init__(self) -> None:
        super().__init__(logging.DEBUG)
        self.events: list[object] = []
        self.previous_level = next(
            (handler.previous_level for handler in TRACE.handlers if isinstance(handler, _Collect)), TRACE.level
        )

    def emit(self, record: logging.LogRecord) -> None:
        if _trace_events.get() is self.events:
            self.events.append(getattr(record, "trace", record.getMessage()))


@contextmanager
def traced() -> Generator[list[object]]:
    """Trace records from this context and its child tasks only, so runs that overlap keep separate traces."""
    handler = _Collect()
    token = _trace_events.set(handler.events)
    TRACE.addHandler(handler)
    TRACE.setLevel(logging.DEBUG)
    try:
        yield handler.events
    finally:
        TRACE.removeHandler(handler)
        _trace_events.reset(token)
        # Runs can finish out of order; the last collector restores the level from before any run started.
        if not any(isinstance(active, _Collect) for active in TRACE.handlers):
            TRACE.setLevel(handler.previous_level)


def transient_seconds(events: Iterable[object], began: float, ended: float) -> float:
    """Time within `began`..`ended` that calls spent on transient provider failures, calls that overlapped
    counted once. Evals run an attempt with any of it again rather than subtract it, which would credit the one arm
    whose requests are visible. An upper bound on the delay: a call running beside the failed one may have taken as
    long anyway."""
    spans = sorted(
        (max(float(event["began"]), began), min(float(event["ended"]), ended))
        for event in events
        if isinstance(event, dict) and event.get("event") == "request_transient"
    )
    total, reach = 0.0, began
    for start, end in spans:
        start = max(start, reach)
        if end > start:
            total += end - start
            reach = end
    return total
