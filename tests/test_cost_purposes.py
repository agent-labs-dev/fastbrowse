"""Decision receipts keep Jev attribution separate from LLM purposes."""

import pytest

from fastbrowse.models import CostBasis, CostComponent, CostLine, JevPurpose, LLMPurpose


@pytest.mark.parametrize("purpose", list(JevPurpose))
def test_decision_purpose_round_trips_without_changing_the_charge(purpose):
    line = CostLine(
        component=CostComponent.JEV,
        basis=CostBasis.METERED,
        dollars=0.001,
        decision_purpose=purpose,
    )
    assert CostLine.model_validate_json(line.model_dump_json()) == line
    assert line.label == f"jev:{purpose.value}"
    assert line.purpose is None
    assert line.dollars == 0.001


def test_old_receipts_keep_their_serialized_shape():
    previous = (
        '{"component":"jev","basis":"metered","dollars":0.001,"purpose":null,'
        '"input_tokens":0,"output_tokens":0,"seconds":null}'
    )
    line = CostLine.model_validate_json(previous)
    assert line.decision_purpose is None
    assert line.model_dump_json() == previous
    assert line.label == "jev"


def test_llm_receipts_retain_the_existing_purpose():
    line = CostLine(component=CostComponent.LLM, basis=CostBasis.UNKNOWN, purpose=LLMPurpose.COMPOSE)
    assert line.label == "llm:compose"
    assert "decision_purpose" not in line.model_dump()
