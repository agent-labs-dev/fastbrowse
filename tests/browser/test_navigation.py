"""Directed navigation to a caller-supplied address, through the real browser."""

import asyncio
from collections.abc import Mapping

import pytest
from pydantic import JsonValue

from fastbrowse.agent import Agent
from fastbrowse.browser import BrowserSession, CdpPage
from fastbrowse.jev import Evaluation, NoulAnswer, Question
from fastbrowse.models import BrowserConnection, Limits, Operation, Status, StepOutcome
from fastbrowse.page import Action
from tests.browser.test_browser import find, wait_until
from tests.browser.test_safety import scoped
from tests.test_policy import ScriptedJev
from tests.test_retrieval import ScriptedLLM


async def test_navigate_crosses_origins_and_reports_the_landing(
    page: CdpPage, main_site: str, iframe_site: str
) -> None:
    await page.navigate(f"{main_site}/")
    before = await page.observe()
    target = f"{iframe_site}/iframe.html"
    result = await page.act(Action(operation=Operation.NAVIGATE, url=target), before)
    assert result.outcome is StepOutcome.EXECUTED and result.page_changed
    after = await page.observe()
    assert await page.address() == target
    assert after.url == target
    assert after.page_key != before.page_key
    assert await page.document_changed(before)


async def test_navigate_to_the_current_address_changes_nothing(page: CdpPage, main_site: str) -> None:
    here = f"{main_site}/index.html"
    await page.navigate(here)
    before = await page.observe()
    result = await page.act(Action(operation=Operation.NAVIGATE, url=here), before)
    assert result.outcome is StepOutcome.EXECUTED
    assert await page.address() == here
    assert find(await page.observe(), "Confirm")


@pytest.mark.parametrize("url", [None, "javascript:alert(1)", "file:///etc/passwd", "data:text/html,hi"])
async def test_navigate_refuses_a_missing_or_non_http_address(page: CdpPage, main_site: str, url: str | None) -> None:
    await page.navigate(f"{main_site}/")
    before = await page.observe()
    result = await page.act(Action(operation=Operation.NAVIGATE, url=url), before)
    assert result.outcome is StepOutcome.FAILED and not result.page_changed
    assert await page.address() == f"{main_site}/"


async def _navigate_from_a_dialog(
    page: CdpPage, browser_session: BrowserSession, main_site: str, iframe_site: str
) -> tuple[str, StepOutcome, bool]:
    await page.navigate(f"{main_site}/")
    obs = await page.observe()
    await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "Confirm").id), obs)
    await wait_until(lambda: browser_session.pending_dialog() is not None)
    blocked = await page.observe()
    assert blocked.dialog is not None
    target = f"{iframe_site}/iframe.html"
    result = await asyncio.wait_for(page.act(Action(operation=Operation.NAVIGATE, url=target), blocked), timeout=30)
    return target, result.outcome, result.page_changed


async def test_navigate_from_a_pending_dialog_lands_on_the_address(
    page: CdpPage, browser_session: BrowserSession, main_site: str, iframe_site: str
) -> None:
    target, outcome, changed = await _navigate_from_a_dialog(page, browser_session, main_site, iframe_site)
    assert outcome is StepOutcome.EXECUTED and changed
    assert await asyncio.wait_for(page.address(), 10) == target


async def test_navigating_away_clears_the_dialog_of_the_old_document(
    page: CdpPage, browser_session: BrowserSession, main_site: str, iframe_site: str
) -> None:
    await _navigate_from_a_dialog(page, browser_session, main_site, iframe_site)
    assert browser_session.pending_dialog() is None
    assert (await asyncio.wait_for(page.observe(), 10)).dialog is None


async def test_navigation_cannot_leave_the_callers_origin_grant(
    chrome_connection: BrowserConnection, main_site: str, iframe_site: str
) -> None:
    from fastbrowse.browser.session import OriginNotAllowed

    async with scoped(chrome_connection, main_site) as (_, page):
        await page.navigate(f"{main_site}/dispatch.html")
        before = await page.observe()
        with pytest.raises(OriginNotAllowed):
            await page.act(Action(operation=Operation.NAVIGATE, url=f"{iframe_site}/iframe.html"), before)
        assert await page.address() == before.url


async def test_agent_reaches_a_second_supplied_site_without_a_link(
    page: CdpPage, main_site: str, iframe_site: str
) -> None:
    target = f"{iframe_site}/iframe.html"

    class NavigationJev(ScriptedJev):
        async def evaluate(self, state: JsonValue, questions: Mapping[str, Question]) -> Evaluation:
            current = state.get("page") if isinstance(state, dict) else None
            if isinstance(current, dict):
                self.pick = {**self.pick, "operation": "done" if current.get("url") == target else "navigate"}
            result = await super().evaluate(state, questions)
            answers = dict(result.answers)
            for key, answer in answers.items():
                if isinstance(answer, NoulAnswer):
                    answers[key] = NoulAnswer(probability=0.99 if key in {"complete", "destination"} else 0.01)
            return result.model_copy(update={"answers": answers})

    await page.navigate(f"{main_site}/dispatch.html")
    plan: JsonValue = {
        "requirements": [{"id": "destination", "kind": "action", "text": f"Open {target}"}],
        "answer_expected": False,
    }
    result = await Agent(page, NavigationJev({"navigate_target": "0"}), ScriptedLLM([plan])).run(
        f"Open {target}",
        limits=Limits(max_steps=4),
    )
    assert result.status is Status.COMPLETE
    assert result.final_url == target
    assert any(step.operation is Operation.NAVIGATE and step.target == target for step in result.steps)


@pytest.mark.parametrize("failure", ["unreachable", "timeout", "browser"])
async def test_failed_navigation_is_a_recoverable_step(
    page: CdpPage, main_site: str, monkeypatch: pytest.MonkeyPatch, failure: str
) -> None:
    from fastbrowse.page import BrowserError, NavigationTimeout, SiteUnreachable

    await page.navigate(main_site)
    before = await page.observe()
    error = {"unreachable": SiteUnreachable, "timeout": NavigationTimeout, "browser": BrowserError}[failure]

    async def broken(*args, **kwargs) -> None:
        raise error("Page.navigate failed (net::ERR_CONNECTION_REFUSED)")

    monkeypatch.setattr(page, "navigate", broken)
    result = await page.act(Action(operation=Operation.NAVIGATE, url=f"{main_site}/missing"), before)
    assert result.outcome is StepOutcome.FAILED
    assert "ERR_CONNECTION_REFUSED" in (result.detail or "")
    assert await page.address() == before.url


async def test_navigation_does_not_swallow_a_lost_browser(
    page: CdpPage, main_site: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    from fastbrowse.page import BrowserUnavailable

    await page.navigate(main_site)
    before = await page.observe()

    async def closed(*args, **kwargs) -> None:
        raise BrowserUnavailable("active browser tab closed")

    monkeypatch.setattr(page, "navigate", closed)
    with pytest.raises(BrowserUnavailable):
        await page.act(Action(operation=Operation.NAVIGATE, url=f"{main_site}/other"), before)
