from collections.abc import Mapping
from datetime import UTC, datetime

import pytest
from pydantic import JsonValue

from fastbrowse.config import Config, ObservationLimits, TokenBudget
from fastbrowse.jev import (
    Answer,
    ChoiceAnswer,
    ChoiceQuestion,
    Evaluation,
    JevInputTooLarge,
    NoulAnswer,
    NoulQuestion,
    Question,
)
from fastbrowse.models import CostBasis, CostComponent, CostLine, Operation, StepOutcome
from fastbrowse.page import Control, Observation
from fastbrowse.policy import HistoryEntry, ObservationTooLarge, Reduction, StepContext, decide

FREE = CostLine(component=CostComponent.JEV, basis=CostBasis.ESTIMATED, dollars=0.0)


class ScriptedJev:
    """Picks `pick` for every choice whose criteria contain it, else the first option."""

    def __init__(self, pick: Mapping[str, str], *, reject_offscreen: bool = False) -> None:
        self.pick = pick
        self.reject_offscreen = reject_offscreen
        self.requests: list[Mapping[str, Question]] = []

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        self.requests.append(questions)
        if self.reject_offscreen and "offscreen" in str(state):
            raise JevInputTooLarge("max_tokens_exceeded")
        answers: dict[str, Answer] = {}
        for key, question in questions.items():
            match question:
                case ChoiceQuestion():
                    options = list(question.criteria)
                    choice = self.pick.get(key, options[0])
                    answers[key] = ChoiceAnswer(
                        choice=choice, probabilities={o: float(o == choice) for o in options}, confidence=0.9
                    )
                case NoulQuestion():
                    answers[key] = NoulAnswer(probability=0.8)
                case _:
                    raise AssertionError(question)
        return Evaluation(model="test", answers=answers, input_tokens=10, cost=FREE)


def button(i: int, *, offscreen: bool = False) -> Control:
    return Control(
        id=f"b{i}",
        frame_id=None,
        role="button",
        label=f"Button {i}",
        operations=frozenset({Operation.CLICK}),
        offscreen=offscreen,
    )


def observation(controls: tuple[Control, ...]) -> Observation:
    return Observation(
        url="https://example.test/",
        title="t",
        page_key="k",
        captured_at=datetime.now(UTC),
        controls=controls,
        omitted_controls=0,
        viewport_text="",
        tabs=(),
    )


def context(**changes: object) -> StepContext:
    base = StepContext(
        task="press button 3",
        subgoal=None,
        requirements=(),
        notes="",
        history=(),
        check_login=False,
        has_attachments=False,
        secrets=(),
    )
    return base.model_copy(update=changes)


async def test_picks_target_for_chosen_operation() -> None:
    jev = ScriptedJev({"operation": "click", "click_target": "b3"})
    history = (HistoryEntry(operation=Operation.SCROLL, target=None, outcome=StepOutcome.EXECUTED, page_changed=True),)
    decision = await decide(jev, observation(tuple(button(i) for i in range(5))), context(history=history), Config())
    assert (decision.operation, decision.target and decision.target.id) == (Operation.CLICK, "b3")
    assert decision.login_required is None


async def test_too_many_candidates_go_group_then_element() -> None:
    config = Config(observation=ObservationLimits(max_choice_options=10, group_size=4))
    jev = ScriptedJev({"operation": "click", "click_group": "2", "click_target": "b9"})
    decision = await decide(jev, observation(tuple(button(i) for i in range(12))), context(), config)
    assert decision.target is not None and decision.target.id == "b9"
    assert set(jev.requests[1]["click_target"].criteria) == {"b8", "b9", "b10", "b11"}  # type: ignore[union-attr]
    assert len(decision.cost) == 2


async def test_oversized_request_drops_offscreen_then_gives_up() -> None:
    controls = (button(0), button(1, offscreen=True))
    jev = ScriptedJev({"operation": "click"}, reject_offscreen=True)
    decision = await decide(jev, observation(controls), context(), Config())
    assert decision.reduction is Reduction.ONSCREEN_ONLY
    assert decision.offered_controls == 1

    tiny = Config(tokens=TokenBudget(state_plus_largest_question=1, state_plus_all_questions=1))
    with pytest.raises(ObservationTooLarge):
        await decide(ScriptedJev({}), observation(controls), context(), tiny)


@pytest.mark.parametrize("retry", [False, True])
async def test_each_request_including_groups_and_retries_needs_budget(retry: bool) -> None:
    from fastbrowse.models import Limits
    from fastbrowse.telemetry import BudgetExceeded, Ledger

    jev = ScriptedJev({"operation": "click", "click_group": "0"}, reject_offscreen=retry)
    controls = tuple(button(i, offscreen=retry and i == 0) for i in range(12))
    config = Config(observation=ObservationLimits(max_choice_options=10, group_size=4))
    ledger = Ledger(Limits(max_jev_calls=1))
    with pytest.raises(BudgetExceeded):
        await decide(jev, observation(controls), context(), config, ledger=ledger)
    assert len(jev.requests) == 1 and ledger.jev_calls == 1
