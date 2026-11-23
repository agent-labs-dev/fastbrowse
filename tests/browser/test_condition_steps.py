"""A conditional commit is judged with the steps the run took, on a page that cannot show them (#1006)."""

import threading
from collections.abc import Iterator, Mapping
from http.server import ThreadingHTTPServer

import pytest
from pydantic import JsonValue

from fastbrowse import run_task
from fastbrowse.jev import Answer, ChoiceAnswer, ChoiceQuestion, Evaluation, NoulAnswer, Question
from fastbrowse.models import Authorization, CostBasis, CostComponent, CostLine
from tests.browser.conftest import SITES, _handler_for
from tests.scripted import ScriptedJev, ScriptedLLM, steps

CLEARED = "- click 'Cancel (Reorder Media)'"


class StepReadingJev(ScriptedJev):
    """A Jev that takes the delete as irreversible, asked for and conditional, and judges the condition only from what a
    question states: the steps the run took. It answers `happened` yes when they include the cancelled dialog."""

    def __init__(self, script: str, questions: list[dict[str, str]]) -> None:
        super().__init__(steps(script))
        self.questions = questions

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        if "met" not in questions:
            if set(questions) & {"irreversible", "requested", "lasting", "conditional"}:
                odds = {"irreversible": 0.95, "requested": 0.97, "lasting": 0.95, "conditional": 0.9}
                deleting = any("'delete'" in question.instructions for question in questions.values())
                return Evaluation(
                    model="scripted",
                    answers={
                        key: NoulAnswer(probability=odds.get(key, 0.0) if deleting else 0.0)
                        for key, question in questions.items()
                        if not isinstance(question, ChoiceQuestion)
                    },
                    input_tokens=0,
                    cost=CostLine(component=CostComponent.JEV, basis=CostBasis.ESTIMATED, dollars=0.0),
                )
            return await super().evaluate(state, questions)
        choices = {key: question for key, question in questions.items() if isinstance(question, ChoiceQuestion)}
        texts = {key: question.instructions for key, question in choices.items()}
        self.questions.append(texts)
        picked = {"met": "asked", "values": "later", "happened": "yes" if CLEARED in texts["happened"] else "no"}
        answers: dict[str, Answer] = {
            key: ChoiceAnswer(
                choice=choice,
                probabilities={option: float(option == choice) for option in choices[key].criteria},
                confidence=1.0,
            )
            for key, choice in picked.items()
        }
        return Evaluation(
            model="scripted",
            answers=answers,
            input_tokens=0,
            cost=CostLine(component=CostComponent.JEV, basis=CostBasis.ESTIMATED, dollars=0.0),
        )


@pytest.fixture
def gallery() -> Iterator[tuple[str, list[str]]]:
    posted: list[str] = []
    server = ThreadingHTTPServer(("127.0.0.1", 0), _handler_for(SITES / "main", posted=posted))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_address[1]}/gallery-reorder.html", posted
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


async def delete_after(script: str, cdp_url: str, start: str) -> list[dict[str, str]]:
    questions: list[dict[str, str]] = []
    await run_task(
        "Click Reorder gallery, then click Cancel there. Then delete the gallery image titled Test image.",
        start=start,
        cdp_url=cdp_url,
        jev=StepReadingJev(script, questions),
        llm=ScriptedLLM(),
        authorization=Authorization(irreversible_actions=True),
    )
    return questions


@pytest.mark.timeout(120)
async def test_a_delete_after_a_dialog_was_opened_and_cancelled_goes_ahead(
    cdp_url: str, gallery: tuple[str, list[str]]
) -> None:
    start, posted = gallery
    questions = await delete_after("click Reorder gallery; click Cancel; click delete", cdp_url, start)
    assert posted == ["Test image"], questions
    assert all(CLEARED in texts[key] for texts in questions for key in ("met", "values", "happened")), questions


@pytest.mark.timeout(120)
async def test_a_delete_whose_condition_is_unmet_stays_refused(cdp_url: str, gallery: tuple[str, list[str]]) -> None:
    start, posted = gallery
    questions = await delete_after("click Reorder gallery; click delete", cdp_url, start)
    assert posted == [], questions
    assert questions and all(CLEARED not in texts["happened"] for texts in questions), questions
