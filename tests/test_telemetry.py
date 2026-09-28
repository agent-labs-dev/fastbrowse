import pytest

from fastbrowse.models import CostBasis, CostComponent, CostLine, Limits
from fastbrowse.telemetry import BudgetExceeded, Ledger, transient_seconds


@pytest.mark.parametrize(
    ("limit", "shown"), [(0.00001, "0.00001"), (0.25, "0.25"), (1.0, "1.0"), (0.5, "0.5"), (12.5, "12.5")]
)
def test_the_spend_limit_is_printed_as_a_currency_amount(limit: float, shown: str) -> None:
    ledger = Ledger(Limits(max_dollars=limit))
    with pytest.raises(BudgetExceeded) as error:
        ledger.record(CostLine(component=CostComponent.LLM, basis=CostBasis.METERED, dollars=limit + 1))
    assert str(error.value) == f"spend limit ${shown} reached"


def test_transient_time_counts_overlapping_calls_once_and_only_within_the_run() -> None:
    def span(began: float, ended: float) -> dict[str, object]:
        return {"event": "request_transient", "call": "jev", "began": began, "ended": ended}

    # Parallel batches failing over 2..5 and 4..6 lost 4s, not 5; a span before the run and other events add none.
    events = [span(4.0, 6.0), span(2.0, 5.0), span(-3.0, -1.0), {"event": "request_retry"}, "a message"]
    assert transient_seconds(events, 0.0, 10.0) == pytest.approx(4.0)
    assert transient_seconds(events, 0.0, 3.0) == pytest.approx(1.0)


def test_budget_stop_names_the_exhausted_limit() -> None:
    ledger = Ledger(Limits(max_steps=1))
    ledger.steps = 1
    with pytest.raises(BudgetExceeded) as error:
        ledger.check()
    assert error.value.budget.resource == "steps"
    assert error.value.budget.limit == 1
