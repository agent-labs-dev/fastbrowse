"""A page that replaces its document while it is being read, as a self-clearing bot check does, through real Chrome."""

from collections.abc import Mapping
from contextlib import suppress

import pytest
from pydantic import JsonValue

from fastbrowse.agent import Agent
from fastbrowse.browser import BrowserSession, CdpPage
from fastbrowse.browser import page as page_module
from fastbrowse.browser.session import _browser_error
from fastbrowse.jev import Evaluation, NoulAnswer, Question
from fastbrowse.models import Status
from fastbrowse.page import BrowserError, ContextLost
from tests.browser.test_browser import eval_value, wait_until
from tests.test_policy import ScriptedJev
from tests.test_retrieval import ScriptedLLM


def _destroyed() -> BrowserError:
    """What a Runtime.evaluate whose document was replaced mid-command raises, built by the real wrapper."""
    return _browser_error(
        "Runtime.evaluate", RuntimeError({"code": -32000, "message": "Execution context was destroyed."})
    )


class OpenJev(ScriptedJev):
    """Finishes once the page is `target`, and judges the challenge a bot check until it clears."""

    def __init__(self, target: str) -> None:
        super().__init__({})
        self.target = target
        self.seen: list[str] = []

    async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
        current = state.get("page") if isinstance(state, dict) else None
        url = current.get("url") if isinstance(current, dict) else None
        if isinstance(url, str):
            self.seen.append(url)
            self.pick = {**self.pick, "operation": "done" if url == self.target else "scroll"}
        result = await super().evaluate(state, questions)
        answers = dict(result.answers)
        for key, answer in answers.items():
            if isinstance(answer, NoulAnswer):
                there = url == self.target
                answers[key] = NoulAnswer(
                    probability=(0.99 if there else 0.01)
                    if key in {"complete", "destination"}
                    else (0.01 if there else 0.99)
                    if key == "bot_check"
                    else 0.01
                )
        return result.model_copy(update={"answers": answers})


def _plan(target: str) -> JsonValue:
    return {
        "requirements": [{"id": "destination", "kind": "action", "text": f"Open {target}"}],
        "answer_expected": False,
    }


async def test_a_read_whose_document_was_destroyed_once_reads_the_page_again(
    page: CdpPage, main_site: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    target = f"{main_site}/release.html"
    await page.navigate(target)
    real = page._evaluate
    failed = 0

    async def once(
        session_id: str, expression: str, *, context_id: int | None = None, unique_context_id: str | None = None
    ) -> JsonValue:
        nonlocal failed
        # The status read has no fallback of its own, so a single loss there ended the run.
        if (context_id is not None or unique_context_id is not None) and not failed:
            failed += 1
            raise _destroyed()
        return await real(session_id, expression, context_id=context_id, unique_context_id=unique_context_id)

    monkeypatch.setattr(page, "_evaluate", once)
    result = await Agent(page, OpenJev(target), ScriptedLLM([_plan(target)])).run(f"Open {target}")
    assert failed == 1
    assert result.status is Status.COMPLETE, result.error
    assert result.final_url == target


async def test_a_challenge_that_navigates_during_observation_is_read_as_the_page_it_became(
    page: CdpPage, browser_session: BrowserSession, main_site: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    target = f"{main_site}/release.html"
    await page.navigate(f"{main_site}/pypi-challenge.html")
    real = page._evaluate
    straddled = 0

    async def title() -> object:
        with suppress(BrowserError):
            return await eval_value(browser_session, browser_session.active_session_id, "document.title")
        return None

    async def straddling(
        session_id: str, expression: str, *, context_id: int | None = None, unique_context_id: str | None = None
    ) -> JsonValue:
        nonlocal straddled
        # Hold the status read, whose isolated world belongs to the challenge, until the challenge's own script has
        # replaced it: Chrome then refuses the read with -32000, as it did on PyPI across a cloud round trip.
        if (
            (context_id is not None or unique_context_id is not None)
            and not straddled
            and await title() == "Client Challenge"
        ):
            straddled += 1
            await wait_until(lambda: title_is_release(), timeout_seconds=10)
        return await real(session_id, expression, context_id=context_id, unique_context_id=unique_context_id)

    async def title_is_release() -> bool:
        return await title() == "Release 2.1.0"

    monkeypatch.setattr(page, "_evaluate", straddling)
    jev = OpenJev(target)
    result = await Agent(page, jev, ScriptedLLM([_plan(target)])).run(f"Open {target}")
    assert straddled == 1
    assert result.status is Status.COMPLETE, result.error
    assert result.final_url == target
    assert target in jev.seen


async def test_a_page_that_never_stops_replacing_itself_ends_with_what_happened(
    page: CdpPage, main_site: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    await page.navigate(f"{main_site}/release.html")
    real = page._evaluate
    status_reads = 0

    async def always(
        session_id: str, expression: str, *, context_id: int | None = None, unique_context_id: str | None = None
    ) -> JsonValue:
        nonlocal status_reads
        if context_id is not None:
            status_reads += 1
            raise _destroyed()
        return await real(session_id, expression, context_id=context_id, unique_context_id=unique_context_id)

    monkeypatch.setattr(page, "_evaluate", always)
    with pytest.raises(ContextLost) as lost:
        await page.observe()
    assert status_reads == page_module._REREADS + 1
    assert str(lost.value).startswith("Runtime.evaluate failed (CDP -32000) on ")
    assert "reads in a row" in str(lost.value)


async def test_a_javascript_error_is_not_read_again(
    page: CdpPage, main_site: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    await page.navigate(f"{main_site}/release.html")
    reads = 0

    async def throws(
        session_id: str, expression: str, *, context_id: int | None = None, unique_context_id: str | None = None
    ) -> JsonValue:
        nonlocal reads
        reads += 1
        raise BrowserError("Runtime.evaluate failed (JavaScriptError)")

    monkeypatch.setattr(page, "_evaluate", throws)
    with pytest.raises(BrowserError) as failed:
        await page.capture()
    assert not isinstance(failed.value, ContextLost)
    # The capture's read and its one plain fallback, never a re-read after waiting for the page.
    assert reads == 2
