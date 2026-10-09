"""Cursor geometry must belong to the page that supplied the action point."""

from fastbrowse.browser.page import CdpPage
from fastbrowse.browser.session import BrowserSession


async def test_geometry_rejects_a_point_from_another_tab(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(f"{main_site}/")
    observation = await page.observe()
    assert await page._cursor_geometry(browser_session.active_session_id, observation.document_key) is not None
    assert await page._cursor_geometry("another-session", observation.document_key) is None


async def test_geometry_rejects_a_point_from_the_previous_document(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(f"{main_site}/")
    before = await page.observe()
    session_id = browser_session.active_session_id
    await page.navigate(f"{main_site}/dispatch.html")
    after = await page.observe()
    assert before.document_key != after.document_key
    assert await page._cursor_geometry(session_id, before.document_key) is None
    assert await page._cursor_geometry(session_id, after.document_key) is not None


async def test_driver_failure_does_not_repeat_or_change_a_browser_action(browser_session: BrowserSession) -> None:
    from unittest.mock import AsyncMock, Mock

    from fastbrowse.browser.cursor import CursorFeedback, _Driver
    from fastbrowse.config import Config
    from fastbrowse.models import Operation, StepOutcome
    from fastbrowse.page import Action
    from tests.browser.test_browser import find

    driver = Mock(spec=_Driver)
    driver.tool = AsyncMock(side_effect=RuntimeError("driver unavailable"))
    driver.close = AsyncMock()
    feedback = CursorFeedback(driver, "test-owned-session")
    page = CdpPage(browser_session, Config(), cursor=feedback)
    await page.navigate(
        "data:text/html,<button onclick=\"this.textContent='Clicked '+(++window.clicks)\">Click</button>"
        "<script>window.clicks=0</script>"
    )
    observation = await page.observe()
    action = Action(operation=Operation.CLICK, target_id=find(observation, "Click").id)
    result = await page.act(action, observation)
    await feedback.settled()
    assert result.outcome is StepOutcome.EXECUTED
    assert await page._evaluate(browser_session.active_session_id, "window.clicks") == 1
    assert "Clicked 1" in (await page.capture()).text
    driver.close.assert_awaited_once()
    await feedback.close()
