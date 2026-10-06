"""Model clients that follow a script, so a run can be driven with no model behind it.

    fastbrowse serve --stdio --run-task fastbrowse.scripted:run_task

This ships in the package, though nothing a user runs reaches it, because the frozen binary is tested as it is
released: CI has no model keys, and an executable cannot import a test's helpers. The hidden `--run-task`
option is the only way in.

The task is the script. Each step is an operation and the label of the control it lands on, steps are
separated by `;`, and a step that has to pick a value names it after `=`:

    click Add one; fill Token = secret:token

Once the steps are spent the run is called done.
"""

from collections.abc import Mapping, Sequence
from typing import Any

from pydantic import BaseModel, JsonValue

from fastbrowse import run
from fastbrowse.jev import Answer, ChoiceAnswer, ChoiceQuestion, Evaluation, NoulAnswer, NoulQuestion, Question
from fastbrowse.llm import DEFAULT_MAX_OUTPUT_TOKENS, Generation, Message
from fastbrowse.models import CostBasis, CostComponent, CostLine, Frozen, LLMPurpose, Operation, RunResult
from fastbrowse.telemetry import Ledger


class Step(Frozen):
    operation: str
    label: str
    pick: str | None = None


def steps(task: str) -> tuple[Step, ...]:
    """The script a task spells out."""
    found: list[Step] = []
    for part in filter(None, map(str.strip, task.split(";"))):
        action, _, pick = map(str.strip, part.partition("="))
        operation, _, label = action.partition(" ")
        found.append(Step(operation=operation, label=label.strip(), pick=pick or None))
    return tuple(found)


class ScriptedJev:
    """Answers each choice as the script's current step says, and every yes/no so that the run goes on.

    A run's options do not cross the protocol, so the thresholds are the shipped ones. A flat probability
    cannot serve them all: the walls and the irreversible check stop a run above theirs, and the done check
    stops it below its own.
    """

    def __init__(self, script: Sequence[Step]) -> None:
        self._ahead = list(script)
        self._step: Step | None = None

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        if "operation" in questions:
            # The page is asked about once per step, so this question is what moves the script on.
            self._step = self._ahead.pop(0) if self._ahead else None
        answers: dict[str, Answer] = {}
        for key, question in questions.items():
            match question:
                case ChoiceQuestion():
                    options = list(question.criteria)
                    choice = self._choice(key, question)
                    if choice not in options:
                        choice = options[0]
                    answers[key] = ChoiceAnswer(
                        choice=choice,
                        probabilities={option: float(option == choice) for option in options},
                        confidence=1.0,
                    )
                case NoulQuestion():
                    answers[key] = NoulAnswer(probability=float(key == "complete"))
                case _:
                    raise AssertionError(f"the scripted Jev has no answer for {key!r}")
        return Evaluation(
            model="scripted",
            answers=answers,
            input_tokens=0,
            cost=CostLine(component=CostComponent.JEV, basis=CostBasis.ESTIMATED, dollars=0.0),
        )

    def _choice(self, key: str, question: ChoiceQuestion) -> str | None:
        step = self._step
        if key == "operation":
            return step.operation if step else Operation.DONE.value
        if key == "read_assessment":
            return "absent"
        if step is None:
            return None
        if key == "pick":
            return step.pick
        if key == f"{step.operation}_target":
            # A control's id is only known once the page is indexed, so the script names it by its label.
            for option, described in question.criteria.items():
                if isinstance(described, Mapping) and described.get("label") == step.label:
                    return option
        return None


class ScriptedLLM:
    """A plan that requires nothing, no shortcut, and a verifier that agrees: the answer by what is asked.

    Planning and the shortcut are asked for at the same moment, so the order of the calls says nothing.
    """

    _REPLIES: Mapping[LLMPurpose, JsonValue] = {
        LLMPurpose.PLAN: {"requirements": [], "answer_expected": False},
        LLMPurpose.SHORTCUT: {"url": None},
        LLMPurpose.VERIFY: {"complete": True, "missing": []},
    }

    async def generate[T: BaseModel](
        self,
        purpose: LLMPurpose,
        messages: Sequence[Message],
        schema: type[T],
        *,
        max_output_tokens: int = DEFAULT_MAX_OUTPUT_TOKENS,
        ledger: Ledger | None = None,
    ) -> Generation[T]:
        if ledger is not None:
            ledger.reserve(CostComponent.LLM)
        return Generation(
            data=schema.model_validate(self._REPLIES[purpose]),
            cost=CostLine(component=CostComponent.LLM, basis=CostBasis.ESTIMATED, dollars=0.0, purpose=purpose),
        )


async def run_task(task: str, **arguments: Any) -> RunResult:
    """`run_task` on a real browser, with the script in `task` standing where the models do."""
    return await run.run_task(task, **arguments, jev=ScriptedJev(steps(task)), llm=ScriptedLLM())
