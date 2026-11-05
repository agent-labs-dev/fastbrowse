"""Physical efficiency counts survive retry aggregation without inventing historical counts."""

from fastbrowse.evals.runner import _efficiency
from fastbrowse.models import CostBasis, CostBreakdown, CostComponent, CostLine


def test_efficiency_counts_requests_inside_aggregated_receipts():
    cost = CostBreakdown(
        lines=(
            CostLine(component=CostComponent.LLM, basis=CostBasis.METERED, dollars=0.03, physical_requests=2),
            CostLine(component=CostComponent.LLM, basis=CostBasis.UNKNOWN, physical_requests=1),
        )
    )

    assert _efficiency(cost)["llm_calls"] == 3
    assert _efficiency(cost)["llm_calls_basis"] == "physical_requests"
    assert cost.known_dollars == 0.03


def test_efficiency_omits_total_when_a_historical_receipt_has_no_count():
    historical = CostLine(component=CostComponent.LLM, basis=CostBasis.METERED, dollars=0.01)
    current = historical.model_copy(update={"physical_requests": 2})

    assert "llm_calls" not in _efficiency(CostBreakdown(lines=(historical, current)))
    assert "physical_requests" not in historical.model_dump(mode="json")
    assert CostLine.model_validate(current.model_dump(mode="json")).physical_requests == 2


def test_no_llm_requests_has_a_known_zero_count():
    assert _efficiency(CostBreakdown())["llm_calls"] == 0
