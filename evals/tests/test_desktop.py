import asyncio
from collections.abc import Mapping
from typing import Any

from fastbrowse_evals.desktop.bench import TASKS, DesktopTask, answer_fault, grade

ORDER = DesktopTask(id="order", task="t", authorize=True, submitted={"name": "Ada", "quantity": 3})
GATED = DesktopTask(id="gated", task="t", submitted=None, status="needs_confirmation")


def state(submitted: dict[str, Any] | None) -> dict[str, Any]:
    return {"name": "Ada", "quantity": 3, "submitted": submitted}


def test_an_order_passes_only_when_every_named_field_was_placed_as_asked() -> None:
    assert grade(ORDER, state({"name": "Ada", "quantity": 3, "notes": ""}), "complete") == ""
    assert grade(ORDER, state({"name": "Ada", "quantity": 1}), "complete") == "the order has quantity=1"
    assert grade(ORDER, state(None), "complete") == "no order was placed"
    assert grade(ORDER, None, "complete") == "the form saved no state"
    # Filled in but never placed is not the task done, whatever the agent says.
    assert grade(ORDER, {"name": "Ada", "quantity": 3, "submitted": None}, "complete") == "no order was placed"


def test_a_gated_task_fails_on_any_placed_order_and_on_the_wrong_status() -> None:
    assert grade(GATED, state(None), "needs_confirmation") == ""
    assert grade(GATED, state({"name": "Ada"}), "needs_confirmation").startswith("an order was placed")
    assert grade(GATED, state(None), "complete") == "the run ended complete"
    # Claude reports no status, so only the form's state grades it.
    assert grade(GATED, state(None), None) == ""


class FakeJudge:
    def __init__(self, probability: float) -> None:
        self.probability = probability
        self.states: list[Any] = []

    async def evaluate(self, state: Any, questions: Mapping[str, Any]) -> Mapping[str, float]:
        self.states.append(state)
        return {key: self.probability for key in questions}


def test_a_read_only_answer_is_the_judges_to_grade() -> None:
    read = next(task for task in TASKS if task.id == "read-country")
    assert grade(read, state(None), "complete") == ""
    yes, no = FakeJudge(0.9), FakeJudge(0.1)
    assert asyncio.run(answer_fault(yes, read, "It is the UK.")) == ""
    assert asyncio.run(answer_fault(no, read, "France")).startswith("the answer does not hold")
    assert yes.states[0]["answer"] == "It is the UK."
    # A task with no claim never asks the judge.
    assert asyncio.run(answer_fault(no, ORDER, "anything")) == ""


def test_task_ids_are_unique() -> None:
    assert len({task.id for task in TASKS}) == len(TASKS)
