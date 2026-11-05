import pytest

from fastbrowse.jev import Evaluation, NoulAnswer
from fastbrowse.models import CostBasis, CostBreakdown, CostComponent, CostLine


def test_evaluation_parses_mixed_answer_types_by_discriminator() -> None:
    evaluation = Evaluation.model_validate(
        {
            "model": "jev-1.13.0",
            "answers": {
                "operation": {"type": "choice", "choice": "click", "probabilities": {"click": 0.9}, "confidence": 0.8},
                "login_required": {"type": "noul", "probability": 0.2},
            },
            "input_tokens": 12,
            "cost": {"component": "jev", "basis": "metered", "dollars": 0.000001},
        }
    )
    assert isinstance(evaluation.answers["login_required"], NoulAnswer)


def test_unknown_cost_is_flagged_not_counted_as_zero() -> None:
    cost = CostBreakdown(
        lines=(
            CostLine(component=CostComponent.JEV, basis=CostBasis.METERED, dollars=0.01),
            CostLine(component=CostComponent.BROWSER, basis=CostBasis.UNKNOWN),
        )
    )
    assert cost.known_dollars == 0.01
    assert cost.has_unknown


@pytest.mark.parametrize(
    ("basis", "incomplete"),
    [
        (CostBasis.METERED, False),
        (CostBasis.ESTIMATED, False),
        (CostBasis.BOUNDED, False),
        (CostBasis.UNSETTLED, True),
        (CostBasis.UNKNOWN, True),
    ],
)
def test_provisional_cost_keeps_the_total_incomplete(basis: CostBasis, incomplete: bool) -> None:
    cost = CostBreakdown(lines=(CostLine(component=CostComponent.PROXY, basis=basis, dollars=0.01),))
    assert cost.known_dollars == 0.01
    assert cost.has_unknown is incomplete
