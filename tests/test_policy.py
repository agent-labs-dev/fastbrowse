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
from fastbrowse.policy import (
    HistoryEntry,
    ObservationTooLarge,
    ReadAssessment,
    Reduction,
    StepContext,
    _element,
    decide,
)

FREE = CostLine(component=CostComponent.JEV, basis=CostBasis.ESTIMATED, dollars=0.0)


class ScriptedJev:
    """Picks `pick` for every choice whose criteria contain it, else the first option."""

    def __init__(self, pick: Mapping[str, str], *, reject_offscreen: bool = False, noul: float = 0.8) -> None:
        self.pick = pick
        self.reject_offscreen = reject_offscreen
        self.noul = noul
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
                    answers[key] = NoulAnswer(probability=self.noul)
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
        check_bot=False,
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


@pytest.mark.parametrize("assessment", list(ReadAssessment))
async def test_read_assessment_is_independent_of_the_chosen_action(assessment: ReadAssessment) -> None:
    jev = ScriptedJev({"operation": "click", "click_target": "b0", "read_assessment": assessment.value})
    decision = await decide(
        jev, observation((button(0),)), context(unread_requirements=("Report why login failed",)), Config()
    )
    assert decision.operation is Operation.CLICK and decision.read_assessment is assessment
    assert len(jev.requests) == 1
    question = jev.requests[0]["read_assessment"]
    assert isinstance(question, ChoiceQuestion)
    assert set(question.criteria) == {assessment.value for assessment in ReadAssessment}
    assert "untrusted data" in question.instructions


async def test_evidenced_requirements_do_not_ask_for_preservation() -> None:
    jev = ScriptedJev({"operation": "click", "click_target": "b0"})
    decision = await decide(jev, observation((button(0),)), context(unread_requirements=()), Config())
    assert "read_assessment" not in jev.requests[0]
    assert decision.read_assessment is ReadAssessment.ABSENT


async def test_too_many_candidates_go_group_then_element() -> None:
    config = Config(observation=ObservationLimits(max_choice_options=10, group_size=4))
    jev = ScriptedJev({"operation": "click", "click_group": "2", "click_target": "b9"})
    decision = await decide(jev, observation(tuple(button(i) for i in range(12))), context(), config)
    assert decision.target is not None and decision.target.id == "b9"
    assert set(jev.requests[1]["click_target"].criteria) == {"b8", "b9", "b10", "b11"}  # ty: ignore[unresolved-attribute]
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


async def test_a_results_page_of_long_titles_and_tracking_links_compacts_instead_of_giving_up() -> None:
    # The shape of Amazon's signed-in results page, which stopped a purchase run at observation_limit.
    title = "Zebra Pen Z Grip Black Ballpoint Pens with Pocket Clip 8pk, Retractable Black Ink Ballpoint Pens, " * 2
    links = tuple(
        Control(
            id=f"l{i}",
            frame_id=None,
            role="link",
            label=f"{title}listing {i}",
            operations=frozenset({Operation.CLICK}),
            href=f"/Zebra-Ballpoint-Retractable-Reliable-Multipack/dp/B0CT3JS5{i:02d}/ref=sr_1_{i}?dib={'x' * 420}",
        )
        for i in range(112)
    )
    jev = ScriptedJev({"operation": "click", "click_target": "l7"})
    decision = await decide(jev, observation(links), context(), Config())
    assert decision.target is not None and decision.target.id == "l7"
    assert decision.reduction is Reduction.COMPACT
    assert "/dp/B0CT3JS507/" in str(jev.requests[-1]["click_target"])


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


async def test_duplicate_labels_reach_the_chooser_with_their_context() -> None:
    twins = tuple(
        button(i).model_copy(update={"label": "Add to cart", "context": name})
        for i, name in enumerate(("Sauce Labs Backpack", "Sauce Labs Bike Light"))
    )
    jev = ScriptedJev({"operation": "click", "click_target": "b1"})
    await decide(jev, observation(twins), context(subgoal="Add the Bike Light"), Config())
    question = jev.requests[0]["click_target"]
    assert isinstance(question, ChoiceQuestion)
    rendered = question.model_dump_json()
    assert '"context":"Sauce Labs Backpack"' in rendered
    assert '"context":"Sauce Labs Bike Light"' in rendered


def test_a_field_the_form_will_not_submit_without_is_marked_for_jev() -> None:
    assert _element(button(1).model_copy(update={"blocking": True}))["blocking"] is True
    assert "blocking" not in _element(button(2))


async def test_a_page_checked_for_a_wall_is_also_asked_whether_it_is_a_bot_check() -> None:
    jev = ScriptedJev({}, noul=0.9)
    decision = await decide(jev, observation((button(1),)), context(check_login=True, check_bot=True), Config())
    assert {"login_required", "bot_check"} <= set(jev.requests[0])
    assert decision.login_required == decision.bot_check == 0.9


async def test_a_page_not_checked_for_a_wall_is_not_asked_about_bot_checks() -> None:
    jev = ScriptedJev({})
    decision = await decide(jev, observation((button(1),)), context(), Config())
    assert "bot_check" not in jev.requests[0]
    assert decision.bot_check is None


async def test_a_bot_check_is_asked_on_its_own_where_a_credential_answers_the_sign_in_question() -> None:
    # Held credentials make a sign-in wall a step to take rather than a stop; they pass no CAPTCHA.
    jev = ScriptedJev({}, noul=0.9)
    decision = await decide(jev, observation((button(1),)), context(check_bot=True), Config())
    assert "login_required" not in jev.requests[0]
    assert decision.login_required is None
    assert decision.bot_check == 0.9
