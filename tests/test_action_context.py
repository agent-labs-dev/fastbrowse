from collections.abc import Mapping
from unittest.mock import AsyncMock, Mock

import pytest
from pydantic import JsonValue

from fastbrowse.agent import Agent, _Stop
from fastbrowse.config import Config
from fastbrowse.jev import ChoiceQuestion, Evaluation, Question
from fastbrowse.memory import Fact
from fastbrowse.models import FactReader, Operation, Status
from fastbrowse.page import BlockKind, Control, Page
from fastbrowse.planner import Plan, Requirement, RequirementKind
from fastbrowse.policy import build_request
from tests.test_agent import run_state
from tests.test_memory import evidence
from tests.test_policy import ScriptedJev, button, context, observation
from tests.test_retrieval import ScriptedLLM, capture


@pytest.mark.parametrize("operation", [Operation.BACK, Operation.READ])
@pytest.mark.parametrize("answered", [False, True])
async def test_action_value_is_preserved_before_returning_to_the_form(operation: Operation, answered: bool) -> None:
    state = await run_state()
    state.task = "Copy the reference from the receipt into the pending return form."
    state.ready_plan = Plan(
        requirements=(Requirement(id="return", text=state.task, kind=RequirementKind.ACTION),),
        answer_expected=False,
    )
    if answered:
        state.ready_plan = state.ready_plan.model_copy(
            update={
                "requirements": (
                    *state.ready_plan.requirements,
                    Requirement(id="email", text="Find the account email", kind=RequirementKind.INFORMATION),
                )
            }
        )
        state.notes.add(
            Fact(
                text="Account email: user@example.test",
                requirement_id="email",
                evidence=evidence(),
                reader=FactReader.LLM,
            )
        )
    quote = "Return reference: R-418"
    obs = observation((), can_go_back=True).model_copy(update={"viewport_text": quote})
    page = Mock(spec=Page)
    page.observe = AsyncMock(return_value=obs)
    page.capture = AsyncMock(return_value=capture((BlockKind.PARAGRAPH, quote)))
    llm = ScriptedLLM([{"claims": [{"text": quote, "cite": {"first": "s0", "last": "s0"}}], "answered": False}])

    async def return_to_form(*args: object) -> None:
        assert quote in [e.quote for e in state.notes.evidence.values()]
        raise _Stop(Status.UNVERIFIED)

    page.act = AsyncMock(side_effect=return_to_form)

    class ReceiptJev(ScriptedJev):
        async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
            if "operation" in questions and isinstance(state, dict) and quote in str(state.get("notes")):
                self.pick = {"operation": "back", "read_assessment": "absent"}
            return await super().evaluate(state, questions)

    agent = Agent(page, ReceiptJev({"operation": operation.value, "read_assessment": "evidence"}, noul=0.0), llm)
    agent._finish = AsyncMock(side_effect=AssertionError("the pending form still needs its reference"))
    with pytest.raises(_Stop):
        await agent._loop(state, None, None)
    assert [step.operation for step in state.steps] == [Operation.READ]
    assert len(llm.calls) == 1
    assert not state.notes.evidenced("return")


async def test_decision_sees_pending_actions_and_whether_this_page_was_read() -> None:
    state = await run_state()
    state.ready_plan = Plan(
        requirements=(Requirement(id="return", text="Enter the receipt reference", kind=RequirementKind.ACTION),),
        answer_expected=False,
    )
    agent = Agent(Mock(spec=Page), ScriptedJev({}), ScriptedLLM([]))
    obs = observation(())
    for read_here in (False, True):
        state.read_here = read_here
        step = agent._context(state, (), check_login=False, check_bot=False)
        request = build_request(obs, obs.controls, step, Config())
        assert isinstance(request.state, dict)
        assert request.state["action_requirements"] == ["Enter the receipt reference"]
        page = request.state["page"]
        assert isinstance(page, dict) and page["read"] is read_here
        assert "read_assessment" in request.questions


@pytest.mark.parametrize("controls,omitted,offered", [((), 0, True), ((button(0),), 0, False), ((), 1, False)])
def test_control_free_page_can_return_to_the_caller_start(
    controls: tuple[Control, ...], omitted: int, offered: bool
) -> None:
    start = "https://example.test/form"
    obs = observation(controls).model_copy(update={"url": "chrome-error://chromewebdata/", "omitted_controls": omitted})
    request = build_request(obs, controls, context(start_url=start), Config())
    operation = request.questions["operation"]
    assert isinstance(operation, ChoiceQuestion)
    assert ("navigate" in operation.criteria) is offered
    if offered:
        target = request.questions["navigate_target"]
        assert isinstance(target, ChoiceQuestion)
        assert list(target.criteria.values()) == [start]
        navigation = operation.criteria["navigate"]
        assert isinstance(navigation, dict) and navigation["destinations"] == list(target.criteria.values())
