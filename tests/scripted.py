"""Model clients that follow a script, so a run on the Rust core can be driven with no model behind it.

They cross to the core as a caller's own `jev=` and `llm=` do. The script is a string of steps. Each step is an
operation and the label of the control it lands on, steps are separated by `;`, and a step that has to pick a value
names it after `=`:

    click Add one; fill Token = secret:token

`ScriptedLLM` drives the run from it, one step a turn, and once the steps are spent calls the run done. The binary
carries its own twin of these behind `--run-task fastbrowse.scripted:run_task`.
"""

import json
from collections.abc import Mapping, Sequence

from pydantic import BaseModel, JsonValue

from fastbrowse.jev import Answer, ChoiceAnswer, ChoiceQuestion, Evaluation, NoulAnswer, NoulQuestion, Question
from fastbrowse.llm import Generation, Message
from fastbrowse.models import CostBasis, CostComponent, CostLine, Frozen, LLMPurpose, Operation
from fastbrowse.telemetry import Ledger


class Step(Frozen):
    operation: Operation
    label: str
    pick: str | None = None


def steps(task: str) -> tuple[Step, ...]:
    """The script a task spells out."""
    found: list[Step] = []
    for part in filter(None, map(str.strip, task.split(";"))):
        action, _, pick = map(str.strip, part.partition("="))
        operation, _, label = action.partition(" ")
        found.append(Step(operation=Operation(operation), label=label.strip(), pick=pick or None))
    return tuple(found)


_READING = frozenset(
    {
        Operation.READ,
        Operation.DONE,
        Operation.NAVIGATE,
        Operation.SCROLL,
        Operation.SCROLL_UP,
        Operation.BACK,
        Operation.SWITCH_TAB,
        Operation.WAIT,
    }
)


class ScriptedJev:
    """Answers every question so that the run goes on.

    A run's options do not cross the protocol, so the thresholds are the shipped ones. A flat probability
    cannot serve them all: the irreversible check stops a run above its threshold, and the check that the answer
    covers the task stops it below its own.
    """

    def __init__(self, script: Sequence[Step]) -> None:
        # What the task asks to be done on a page is answered from the script: whether it takes a step besides
        # opening and reading pages.
        self._acts = any(step.operation not in _READING for step in script)

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        answers: dict[str, Answer] = {}
        for key, question in questions.items():
            match question:
                case ChoiceQuestion():
                    options = list(question.criteria)
                    choice = self._choice(key)
                    if choice not in options:
                        choice = options[0]
                    answers[key] = ChoiceAnswer(
                        choice=choice,
                        probabilities={option: float(option == choice) for option in options},
                        confidence=1.0,
                    )
                case NoulQuestion():
                    answers[key] = NoulAnswer(probability=float(key in ("coverage", "exact")))
                case _:
                    raise AssertionError(f"the scripted Jev has no answer for {key!r}")
        return Evaluation(
            model="scripted",
            answers=answers,
            input_tokens=0,
            cost=CostLine(component=CostComponent.JEV, basis=CostBasis.ESTIMATED, dollars=0.0),
        )

    def _choice(self, key: str) -> str | None:
        if key == "steps":
            return "acts" if self._acts else "reads"
        if key == "read_assessment":
            return "absent"
        return None


_REPLIES: dict[LLMPurpose, JsonValue] = {
    LLMPurpose.COMPOSE: {"claims": []},
    LLMPurpose.IMAGE_READ: {"text": None},
    LLMPurpose.FILL: {"value": None},
    LLMPurpose.PAGES: {"first": None, "last": None},
    LLMPurpose.DATES: {"dates": []},
    LLMPurpose.KNOWN_PAGES: {"urls": []},
    LLMPurpose.RECOVERY_PAGES: {"urls": []},
    LLMPurpose.UNOBTAINABLE: {"item": "", "reason": "", "quote": ""},
    LLMPurpose.GROUPS: {"records": []},
    LLMPurpose.SEARCH: {"queries": []},
}


def _number(shown: str, label: str) -> str:
    """The number of the control labelled `label` on the page `shown` lists, one control a line as
    `[number] role "label" ...`, with a `*` before the number of one new since the last look. A label no control has is
    sent as it is, as a model naming a control the page lacks would, and the driver is told it is not there."""
    for line in shown.splitlines():
        number, bracket, rest = line.lstrip("*").removeprefix("[").partition("] ")
        if not bracket or not line.lstrip("*").startswith("["):
            continue
        _, _, quoted = rest.partition(" ")
        try:
            found, _ = json.JSONDecoder().raw_decode(quoted)
        except ValueError:
            continue
        if found == label:
            return number
    return label


def _turn(step: Step | None, shown: str) -> dict[str, JsonValue]:
    """The driver's action that takes `step` on the page `shown`."""
    if step is None:
        return {"tool": "done", "answer": [], "numbered": False}
    number = _number(shown, step.label)
    match step.operation:
        case Operation.CLICK:
            return {"tool": "click", "id": number}
        case Operation.FILL if step.pick and step.pick.startswith("secret:"):
            return {"tool": "type_secret", "id": number, "name": step.pick.removeprefix("secret:")}
        case Operation.FILL:
            return {"tool": "type", "id": number, "text": step.pick}
        case Operation.SELECT:
            return {"tool": "select", "id": number, "option": step.pick}
        case Operation.ENTER:
            return {"tool": "key", "id": number, "key": "Enter"}
        case Operation.ESCAPE:
            return {"tool": "key", "id": number, "key": "Escape"}
        case Operation.PRESS:
            return {"tool": "key", "id": number, "key": step.pick}
        case Operation.SCROLL | Operation.SCROLL_UP:
            return {"tool": "scroll", "id": number if step.label else None, "down": step.operation is Operation.SCROLL}
        case Operation.NAVIGATE:
            return {"tool": "goto", "url": step.label}
        case Operation.DRAG:
            # `drag Card A = Done` drops the control on the one labelled after `=`; a bare drag draws its path.
            return {"tool": "drag", "id": number, "to": _number(shown, step.pick) if step.pick else None}
        case Operation.BACK:
            return {"tool": "back"}
        case Operation.WAIT:
            # `wait=N` asks for N seconds; a bare `wait` asks for an hour, longer than any scripted run lasts.
            return {"tool": "wait", "seconds": float(step.pick) if step.pick else 3600.0}
        case Operation.DIALOG:
            return {"tool": "dialog"}
        case Operation.DONE:
            return {"tool": "done", "answer": json.loads(step.pick) if step.pick else [], "numbered": False}
        case other:
            raise AssertionError(f"the chat driver has no tool for {other}")


class ScriptedLLM:
    """Drives the run from `script`, one step a turn, naming each step's control by the number the turn's page shows it
    under, found by its exact label. Once the steps are spent the run is called done. It proposes `start` as the start
    page, and knows no page that holds what the task names."""

    def __init__(self, start: str | None = None, script: Sequence[Step] = ()) -> None:
        self.start = start
        self._ahead = list(script)

    async def generate[T: BaseModel](
        self,
        purpose: LLMPurpose,
        messages: Sequence[Message],
        schema: type[T],
        *,
        ledger: Ledger | None = None,
    ) -> Generation[T]:
        if ledger is not None:
            ledger.reserve(CostComponent.LLM)
        if purpose is LLMPurpose.ACTION:
            step = self._ahead.pop(0) if self._ahead else None
            answer: JsonValue = {"next": "", "actions": [_turn(step, messages[-1].content if messages else "")]}
        else:
            answer = _REPLIES.get(purpose, {"url": self.start})
        return Generation(
            data=schema.model_validate(answer),
            cost=CostLine(component=CostComponent.LLM, basis=CostBasis.ESTIMATED, dollars=0.0, purpose=purpose),
        )
