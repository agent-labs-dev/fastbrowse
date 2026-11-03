"""Jev question and answer contracts shared with the native host bridge."""

import pytest
from pydantic import TypeAdapter, ValidationError

from fastbrowse.jev import BoxAnswer, BoxQuestion, Edges, Evaluation, Question


def test_box_question_and_answer_cross_the_host_contract() -> None:
    questions = TypeAdapter(dict[str, Question]).validate_python(
        {
            "target": {"type": "box", "instructions": "Locate the selected control"},
        }
    )
    assert isinstance(questions["target"], BoxQuestion)
    payload = {
        "model": "jev",
        "answers": {"target": {"type": "box", "box": {"top": 0.1, "bottom": 0.3, "left": 0.2, "right": 0.4}}},
        "input_tokens": 1,
        "cost": {"component": "jev", "basis": "metered", "dollars": 0.01},
    }
    evaluation = Evaluation.model_validate(payload)
    answer = evaluation.answers["target"]
    assert isinstance(answer, BoxAnswer)
    assert answer.box == Edges(top=0.1, bottom=0.3, left=0.2, right=0.4)
    assert answer.confidence is None
    assert Evaluation.model_validate_json(evaluation.model_dump_json()) == evaluation
    with pytest.raises(ValidationError):
        Evaluation.model_validate(
            payload
            | {
                "answers": {"target": {"type": "box", "box": answer.box.model_dump(), "confidence": 1.1}},
            }
        )
