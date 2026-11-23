"""A change the site keeps, made on a page that has not redrawn it: the run is told whether what it typed shows."""

from collections.abc import Mapping

import pytest
from pydantic import JsonValue

from fastbrowse import run_task
from fastbrowse.jev import Evaluation, NoulAnswer, Question
from fastbrowse.models import Authorization
from tests.scripted import ScriptedJev, ScriptedLLM, steps


class Watching(ScriptedJev):
    """Takes every click as a commit the task asks for, and keeps the page facts each step was asked with."""

    def __init__(self, script: str) -> None:
        super().__init__(steps(script))
        self.facts: list[list[str]] = []

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        if "operation" in questions and isinstance(state, dict):
            facts = state.get("page_facts")
            self.facts.append([fact for fact in facts if isinstance(fact, str)] if isinstance(facts, list) else [])
        evaluation = await super().evaluate(state, questions)
        answers = dict(evaluation.answers)
        for key in answers:
            if key.endswith(("irreversible", "requested")):
                answers[key] = NoulAnswer(probability=1.0)
        return evaluation.model_copy(update={"answers": answers})


@pytest.mark.timeout(180)
async def test_the_run_is_told_the_page_does_not_show_its_reply_until_it_is_loaded_again(
    cdp_url: str, main_site: str
) -> None:
    url = f"{main_site}/comments-stale.html"
    jev = Watching(f"fill Reply text = Glad it helps!; click Post; navigate {url}; read")
    await run_task(
        "Reply to the comment with 'Glad it helps!'",
        start=url,
        cdp_url=cdp_url,
        jev=jev,
        llm=ScriptedLLM(),
        authorization=Authorization(irreversible_actions=True),
    )
    stated = [any("has not been loaded since" in fact for fact in facts) for facts in jev.facts]
    # Asked before the fill, before the send, after the send, then after the reload.
    assert stated[:4] == [False, False, True, False], jev.facts
    assert any(
        fact.startswith(
            "The run's click 'Post (Stale Thread)' changed what the site keeps, and this page has not been loaded since"
        )
        and "typed into 'Reply text (Stale Thread)' does not appear anywhere in this page's text" in fact
        and "Opening this page's own address asks the site for its copy again." in fact
        for fact in jev.facts[2]
    ), jev.facts[2]
    assert not any("has not been loaded since" in fact for fact in jev.facts[3]), jev.facts[3]
