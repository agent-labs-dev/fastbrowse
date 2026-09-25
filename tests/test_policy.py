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
    JevRetriesExhausted,
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


def observation(controls: tuple[Control, ...], *, can_go_back: bool = False) -> Observation:
    return Observation(
        url="https://example.test/",
        title="t",
        page_key="k",
        captured_at=datetime.now(UTC),
        controls=controls,
        omitted_controls=0,
        viewport_text="",
        tabs=(),
        can_go_back=can_go_back,
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


class SheddingJev(ScriptedJev):
    """Answers 503 until its retries run out for any request offering more than `answers_up_to` elements."""

    def __init__(self, pick: Mapping[str, str], answers_up_to: int) -> None:
        super().__init__(pick)
        self.answers_up_to = answers_up_to
        self.shed: list[int] = []

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        offered = len(state["elements"])  # ty: ignore[invalid-argument-type, not-subscriptable]
        if offered > self.answers_up_to:
            self.shed.append(offered)
            raise JevRetriesExhausted("HTTP 503", seconds=1.0, unaccounted_requests=0, requests=6)
        return await super().evaluate(state, questions)


async def test_a_large_request_the_provider_sheds_is_asked_again_smaller() -> None:
    # A Wikipedia article offered 160 controls in 27k tokens, and the gateway shed that request in six runs of six.
    config = Config(tokens=TokenBudget(batch_tokens=200))
    onscreen = tuple(button(i) for i in range(10))
    controls = (*onscreen, *(button(i, offscreen=True) for i in range(10, 40)))
    jev = SheddingJev({"operation": "click", "click_target": "b3"}, answers_up_to=10)
    decision = await decide(jev, observation(controls), context(), config)
    assert jev.shed == [40]
    assert decision.reduction is Reduction.ONSCREEN_ONLY and decision.offered_controls == 10
    assert decision.target is not None and decision.target.id == "b3"

    halving = SheddingJev({"operation": "click"}, answers_up_to=5)
    decision = await decide(halving, observation(onscreen), context(), config)
    assert halving.shed == [10]
    assert decision.reduction is Reduction.CAPPED and decision.offered_controls < 5


async def test_a_small_request_that_is_shed_is_still_an_outage() -> None:
    jev = SheddingJev({"operation": "click"}, answers_up_to=0)
    with pytest.raises(JevRetriesExhausted):
        await decide(jev, observation((button(0), button(1, offscreen=True))), context(), Config())
    assert jev.shed == [2]


async def test_a_page_too_dense_on_screen_offers_what_fits_and_scrolls_for_the_rest() -> None:
    # Room for about ten buttons and no relevance pass to cap them: ending the run here would give up on a page a
    # scroll could work through.
    config = Config(
        tokens=TokenBudget(state_plus_largest_question=2_000, state_plus_all_questions=2_000),
        observation=ObservationLimits(max_offered_controls=20),
    )
    applied = button(39).model_copy(update={"checked": True})
    controls = (*(button(i) for i in range(39)), applied)
    jev = RelevanceJev({"operation": "scroll"}, set(), fail=True)
    decision = await decide(jev, observation(controls), context(), config)
    assert decision.reduction is Reduction.CAPPED and 0 < decision.offered_controls < len(controls)
    offered = set(jev.requests[-1]["click_target"].criteria)  # ty: ignore[unresolved-attribute]
    assert "b39" in offered and "b0" in offered
    assert "scroll" in jev.requests[-1]["operation"].criteria  # ty: ignore[unresolved-attribute]


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
    # A link told apart only by its query keeps it.
    query_link = Control(
        id="q", frame_id=None, role="link", label="Item", operations=frozenset({Operation.CLICK}), href="/item?id=456"
    )
    assert _element(query_link, compact=True)["href"] == "/item?id=456"


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


async def test_back_is_offered_only_when_the_observation_can_go_back() -> None:
    # A wizard that shares one URL for every step has taken actions but has no earlier same-origin entry, so
    # history alone must never be what puts `back` on the operation menu.
    history = (HistoryEntry(operation=Operation.CLICK, target="b0", outcome=StepOutcome.EXECUTED, page_changed=True),)
    jev = ScriptedJev({"operation": "click", "click_target": "b0"})
    await decide(jev, observation((button(0),), can_go_back=False), context(history=history), Config())
    question = jev.requests[0]["operation"]
    assert isinstance(question, ChoiceQuestion)
    assert "back" not in question.criteria

    jev = ScriptedJev({"operation": "click", "click_target": "b0"})
    await decide(jev, observation((button(0),), can_go_back=True), context(history=()), Config())
    question = jev.requests[0]["operation"]
    assert isinstance(question, ChoiceQuestion)
    assert "back" in question.criteria


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


class RelevanceJev(ScriptedJev):
    """Scores a relevance question high when its element names one of `relevant`; `fail` rejects every one."""

    def __init__(self, pick: Mapping[str, str], relevant: set[str], *, fail: bool = False) -> None:
        super().__init__(pick)
        self.relevant = relevant
        self.fail = fail

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        if "operation" in questions:
            return await super().evaluate(state, questions)
        self.requests.append(questions)
        if self.fail:
            raise JevInputTooLarge("max_tokens_exceeded")
        answers: dict[str, Answer] = {
            key: NoulAnswer(probability=0.9 if any(f'"{label}"' in str(q) for label in self.relevant) else 0.1)
            for key, q in questions.items()
        }
        return Evaluation(model="test", answers=answers, input_tokens=10, cost=FREE)


def _dense(count: int) -> tuple[Control, ...]:
    return tuple(button(i) for i in range(count))


def _offered(jev: ScriptedJev) -> set[str]:
    return set(jev.requests[-1]["click_target"].criteria)  # ty: ignore[unresolved-attribute]


async def test_a_dense_page_offers_the_relevant_control_past_the_document_order_cut() -> None:
    # The target sits after every control a document-order cut keeps, as a result below a long header does.
    config = Config(observation=ObservationLimits(max_offered_controls=10))
    jev = RelevanceJev({"operation": "click", "click_target": "b30"}, {"Button 30"})
    decision = await decide(jev, observation(_dense(40)), context(), config)
    assert decision.reduction is Reduction.RELEVANCE
    assert decision.target is not None and decision.target.id == "b30"
    assert decision.offered_controls == 10
    assert len(decision.cost) == 2  # the relevance pass is on the step's bill


async def test_protected_controls_skip_the_filter_and_survive_it() -> None:
    config = Config(observation=ObservationLimits(max_offered_controls=5))
    filled = button(35).model_copy(update={"label": "Destination", "value": "Paris"})
    blocking = button(36).model_copy(update={"blocking": True})
    jev = RelevanceJev({"operation": "click"}, set())
    await decide(jev, observation((*_dense(35), filled, blocking)), context(), config)
    assert {"b35", "b36"} <= _offered(jev)
    assert not any("Destination" in str(q) or "Button 36" in str(q) for q in jev.requests[0].values())


async def test_unset_options_do_not_crowd_out_the_button_that_applies_them() -> None:
    config = Config(observation=ObservationLimits(max_offered_controls=10))
    options = tuple(button(i).model_copy(update={"role": "checkbox", "checked": False}) for i in range(40))
    apply = button(40).model_copy(update={"label": "Apply filters"})
    jev = RelevanceJev({"operation": "click", "click_target": "b40"}, {"Apply filters"})
    decision = await decide(jev, observation((*options, apply)), context(), config)
    assert decision.target is not None and decision.target.id == "b40"


async def test_a_failed_relevance_pass_falls_back_to_document_order() -> None:
    config = Config(observation=ObservationLimits(max_offered_controls=10))
    jev = RelevanceJev({"operation": "click"}, {"Button 30"}, fail=True)
    decision = await decide(jev, observation(_dense(40)), context(), config)
    assert decision.reduction is Reduction.NONE
    assert _offered(jev) == {f"b{i}" for i in range(10)}


async def test_a_relevance_pass_over_budget_sends_nothing() -> None:
    from fastbrowse.models import Limits
    from fastbrowse.telemetry import BudgetExceeded, Ledger

    # A small request budget splits the pass into more batches than the calls left.
    config = Config(
        observation=ObservationLimits(max_offered_controls=10),
        tokens=TokenBudget(state_plus_all_questions=600),
    )
    jev = RelevanceJev({"operation": "click"}, set())
    with pytest.raises(BudgetExceeded):
        await decide(jev, observation(_dense(40)), context(), config, ledger=Ledger(Limits(max_jev_calls=2)))
    assert jev.requests == []


async def test_a_relevance_pass_the_spend_limit_cannot_cover_sends_nothing() -> None:
    from fastbrowse.models import Limits
    from fastbrowse.telemetry import BudgetExceeded, Ledger

    # The batches run together, so the limit is checked against the whole pass before any is billed.
    config = Config(observation=ObservationLimits(max_offered_controls=10))
    jev = RelevanceJev({"operation": "click"}, set())
    with pytest.raises(BudgetExceeded):
        await decide(jev, observation(_dense(40)), context(), config, ledger=Ledger(Limits(max_dollars=1e-9)))
    assert jev.requests == []


async def test_a_page_under_the_limit_costs_no_relevance_pass() -> None:
    jev = RelevanceJev({"operation": "click"}, set())
    await decide(jev, observation(_dense(20)), context(), Config())
    assert len(jev.requests) == 1
