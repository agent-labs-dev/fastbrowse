"""The scripted model clients the frozen binary's smoke run uses, asked what a run would ask them."""

from fastbrowse.jev import ChoiceAnswer, ChoiceQuestion, NoulAnswer, NoulQuestion
from fastbrowse.models import LLMPurpose
from fastbrowse.planner import Plan
from fastbrowse.scripted import ScriptedJev, ScriptedLLM, Step, steps
from fastbrowse.shortcut import Shortcut

OPERATION = ChoiceQuestion(
    instructions="next", criteria={"read": "Read", "click": "Click", "fill": "Fill", "done": "Done"}
)
TARGETS = ChoiceQuestion(
    instructions="which",
    criteria={"c1": {"label": "Remove", "role": "button"}, "c2": {"label": "Add one", "role": "button"}},
)


def _choice(answer: object) -> str:
    assert isinstance(answer, ChoiceAnswer)
    return answer.choice


def _probability(answer: object) -> float:
    assert isinstance(answer, NoulAnswer)
    return answer.probability


def test_a_task_is_read_as_steps_with_the_value_a_step_picks() -> None:
    assert steps("click Add one; fill Token = secret:token") == (
        Step(operation="click", label="Add one"),
        Step(operation="fill", label="Token", pick="secret:token"),
    )


async def test_each_look_at_the_page_takes_the_next_step_and_finds_its_control_by_label() -> None:
    jev = ScriptedJev(steps("click Add one"))

    first = await jev.evaluate({}, {"operation": OPERATION, "click_target": TARGETS})
    second = await jev.evaluate({}, {"operation": OPERATION, "click_target": TARGETS})

    assert (_choice(first.answers["operation"]), _choice(first.answers["click_target"])) == ("click", "c2")
    assert _choice(second.answers["operation"]) == "done"


async def test_the_value_a_step_names_is_picked_after_the_page_was_looked_at() -> None:
    jev = ScriptedJev(steps("fill Token = secret:token"))
    await jev.evaluate({}, {"operation": OPERATION})

    picked = await jev.evaluate(
        {}, {"pick": ChoiceQuestion(instructions="value", criteria={"none": "Nothing", "secret:token": "The token"})}
    )

    assert _choice(picked.answers["pick"]) == "secret:token"


async def test_no_page_is_a_wall_or_irreversible_and_a_finished_script_is_complete() -> None:
    ask = NoulQuestion(instructions="?")
    jev = ScriptedJev(())

    walls = await jev.evaluate({}, {"login_required": ask, "bot_check": ask, "irreversible": ask})
    done = await jev.evaluate({}, {"complete": ask})

    # The shipped thresholds stop a run above 0.70 for either wall and above 0.50 for an irreversible action.
    assert [_probability(answer) for answer in walls.answers.values()] == [0.0, 0.0, 0.0]
    assert _probability(done.answers["complete"]) == 1.0


async def test_the_plan_and_the_shortcut_are_told_apart_by_purpose_not_by_order() -> None:
    llm = ScriptedLLM()

    shortcut = await llm.generate(LLMPurpose.SHORTCUT, [], Shortcut)
    plan = await llm.generate(LLMPurpose.PLAN, [], Plan)

    assert shortcut.data.url is None
    assert plan.data.requirements == () and plan.data.answer_expected is False
