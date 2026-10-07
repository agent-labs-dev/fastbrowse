"""What the agent calls a wall it cannot pass, and the pager a long listing must still offer, through real Chrome."""

from collections.abc import Mapping

import pytest
from pydantic import JsonValue

from fastbrowse import agent as agent_module
from fastbrowse.agent import Agent
from fastbrowse.browser import BrowserSession, CdpPage
from fastbrowse.config import Config, ObservationLimits
from fastbrowse.jev import Answer, Evaluation, NoulAnswer, NoulQuestion, Question
from fastbrowse.models import CostBreakdown, RunResult, Status
from tests.browser.test_browser import eval_value, observe_until
from tests.test_policy import FREE, ScriptedJev
from tests.test_retrieval import ScriptedLLM

PLAN: JsonValue = {"requirements": [], "answer_expected": False}


class WallJev(ScriptedJev):
    """Judges every page a wall, and says whether the wall is a bot check."""

    def __init__(self, *, bot_check: float) -> None:
        super().__init__({}, noul=0.95)
        self.bot_check = bot_check

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        evaluation = await super().evaluate(state, questions)
        answers: dict[str, Answer] = dict(evaluation.answers)
        if isinstance(questions.get("bot_check"), NoulQuestion):
            answers["bot_check"] = NoulAnswer(probability=self.bot_check)
        return Evaluation(model="test", answers=answers, input_tokens=10, cost=FREE)


@pytest.fixture(autouse=True)
def _short_wait(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(agent_module, "_INTERSTITIAL_SECONDS", 0.6)


async def test_a_captcha_that_never_clears_is_blocked_not_a_sign_in(page: CdpPage, main_site: str) -> None:
    await page.navigate(f"{main_site}/challenge.html")
    result = await Agent(page, WallJev(bot_check=0.9), ScriptedLLM([PLAN]), config=Config(step_frames=True)).run(
        "Find the release date"
    )
    assert result.status is Status.BLOCKED
    assert result.final_frame is not None and result.final_frame.startswith(b"\x89PNG")
    assert result.error is not None and "bot check" in result.error and "sign-in required" not in result.error


async def test_a_sign_in_form_is_still_a_sign_in(page: CdpPage, main_site: str) -> None:
    await page.navigate(f"{main_site}/signin.html")
    result = await Agent(page, WallJev(bot_check=0.05), ScriptedLLM([PLAN])).run("Find the release date")
    assert result.status is Status.NEEDS_LOGIN
    assert result.error is not None and "sign-in required" in result.error


async def test_the_pager_and_load_more_at_the_foot_of_a_long_listing_are_still_offered(
    browser_session: BrowserSession, main_site: str
) -> None:
    # The listing is sized against a 40-control offscreen cap; the default pool now holds all of it.
    page = CdpPage(browser_session, Config(observation=ObservationLimits(max_offscreen_controls=40)))
    await page.navigate(f"{main_site}/long-list.html")
    observation = await observe_until(page, "Book 1")
    labels = [control.label for control in observation.controls]
    assert observation.omitted_controls > 0, "the page must be long enough to hit the cap"
    assert "next" in labels
    assert "Show more books" in labels
    assert len(labels) <= page._config.observation.max_offscreen_controls + len(
        [c for c in observation.controls if not c.offscreen]
    )


@pytest.mark.parametrize("visible_secret", [False, True])
async def test_ending_frame_waits_for_loading_without_exposing_a_secret(
    page: CdpPage, browser_session: BrowserSession, main_site: str, visible_secret: bool
) -> None:
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        """
        document.body.innerHTML = '<div role="progressbar">Loading...</div><p>fixture-secret</p>';
        true
    """,
    )
    agent = Agent(page, ScriptedJev({}), ScriptedLLM([]), config=Config(step_frames=True))
    if visible_secret:
        agent._redactor.register("fixture", "fixture-secret")
    result = RunResult(
        status=Status.BLOCKED, answer=None, data=None, evidence=(), steps=(), cost=CostBreakdown(), artifacts=()
    )
    result = await agent._ending_frame(result)
    assert result.status is Status.BLOCKED
    if visible_secret:
        assert result.final_frame is None
    else:
        assert result.final_frame is not None and result.final_frame.startswith(b"\x89PNG")
