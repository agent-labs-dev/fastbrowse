"""Grading an answer with a judge, never with a string match.

A substring or a regex over free text grades the wording rather than the fact. It fails a run that states the
same thing in other words, and passes one that repeats the expected number in an unrelated sentence. Every
answer is graded by Jev instead: one typed question per claim, answered against the task, the answer and any
structured output, so a figure written with a currency, a comma or a trailing zero is the same figure.
"""

from collections.abc import Mapping, Sequence
from dataclasses import dataclass

from fastbrowse.jev import JevClient, NoulQuestion
from fastbrowse.models import JsonValue

ACCEPTED = 0.5
"""The probability at which Jev is taken to have said a claim holds."""


@dataclass(frozen=True, slots=True)
class Claim:
    """One fact a run's output must state, in the words a judge can check against the task.

    Written as a sentence about the output ("The answer gives the total as 43.25") so the same claim reads the
    same whether the run answered in prose or through an output schema.
    """

    statement: str


async def judge(
    jev: JevClient,
    *,
    task: str,
    answer: str | None = None,
    data: JsonValue = None,
    submitted: Mapping[str, object] | None = None,
    claims: Sequence[Claim] = (),
) -> str | None:
    """Why the run's output does not state every claim, or None when it states them all.

    One Jev request covers every claim of a run, so a task costs one judgement however many facts it asks for.
    """
    if not claims:
        return None
    state: dict[str, JsonValue] = {"task": task, "answer": (answer or "").strip() or None, "data": data}
    if submitted:
        state["submitted"] = {name: str(value) for name, value in submitted.items()}
    questions = {
        f"claim_{index}": NoulQuestion(
            instructions=(
                f"The task asked for: {task}\n"
                "The fields below are what the run returned: its answer, its structured output and, where it "
                "submitted a form, the fields it sent. Judge only whether each claim holds of what the run "
                "returned, not how it was worded. A figure written with a currency, a thousands separator or a "
                "trailing zero is the same figure, and a fact stated in other words is the same fact.\n\n"
                f"Does this hold: {claim.statement}"
            ),
            true="What the run returned states it.",
            false="What the run returned does not state it, or states something else.",
        )
        for index, claim in enumerate(claims)
    }
    evaluation = await jev.evaluate(state, questions)
    for index, claim in enumerate(claims):
        answered = evaluation.answers.get(f"claim_{index}")
        probability = getattr(answered, "probability", None)
        if probability is None or probability < ACCEPTED:
            return f"the output does not hold: {claim.statement} (answer {answer!r}, data {data!r})"
    return None
