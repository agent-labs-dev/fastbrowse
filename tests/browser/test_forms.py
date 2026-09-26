"""A batch continues only while the original form retains exactly the requested edits."""

import asyncio
import json

import pytest

from fastbrowse.browser import BrowserSession, CdpPage
from fastbrowse.models import Operation, StepOutcome
from fastbrowse.page import Action
from tests.browser.test_browser import eval_value, find


async def form(page: CdpPage, session: BrowserSession, site: str, script: str = "") -> None:
    await page.navigate(f"{site}/dispatch.html")
    html = "<form><label>Name<input id=name required></label><label>Email<input id=email></label></form>"
    await eval_value(session, session.active_session_id, f"document.body.innerHTML = {json.dumps(html)}; {script}")


async def test_two_fills_use_one_observation(page: CdpPage, browser_session: BrowserSession, main_site: str) -> None:
    await form(page, browser_session, main_site)
    before = await page.observe()
    name, email = find(before, "Name"), find(before, "Email")
    assert name.form_id == email.form_id and name.form_id is not None
    for target, text in [(name, "Ada"), (email, "ada@example.com")]:
        result = await page.act(
            Action(operation=Operation.FILL, target_id=target.id, text=text, form_fill=True), before
        )
        assert result.outcome is StepOutcome.EXECUTED
        assert result.form_unchanged
    after = await page.observe()
    assert find(after, "Name").value == "Ada"
    assert find(after, "Email").value == "ada@example.com"


@pytest.mark.parametrize(
    "change",
    [
        "document.querySelector('form').insertAdjacentHTML('beforeend', '<input aria-label=Extra>')",
        "document.getElementById('email').disabled = true",
        "document.getElementById('email').value = 'dependency'",
        "document.getElementById('email').setAttribute('aria-invalid', 'true')",
        "document.getElementById('name').value = ''",
        "document.getElementById('email').outerHTML = '<input id=email aria-label=Email>'",
        "document.querySelector('form').action = '/delete'",
        "location.hash = 'changed'",
    ],
)
async def test_dependent_change_stops_batch(
    page: CdpPage, browser_session: BrowserSession, main_site: str, change: str
) -> None:
    await form(page, browser_session, main_site, f"document.getElementById('name').oninput = () => {{ {change}; }}")
    before = await page.observe()
    result = await page.act(
        Action(operation=Operation.FILL, target_id=find(before, "Name").id, text="Ada", form_fill=True), before
    )
    assert not result.form_unchanged


async def test_change_between_fills_stops_before_typing(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await form(page, browser_session, main_site)
    before = await page.observe()
    result = await page.act(
        Action(operation=Operation.FILL, target_id=find(before, "Name").id, text="Ada", form_fill=True), before
    )
    assert result.form_unchanged
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.querySelector('form').insertAdjacentHTML('beforeend', '<input aria-label=Extra>')",
    )
    result = await page.act(
        Action(operation=Operation.FILL, target_id=find(before, "Email").id, text="ada@example.com", form_fill=True),
        before,
    )
    assert result.outcome is StepOutcome.STALE
    assert find(await page.observe(), "Email").value == ""


async def test_dialog_stops_form_fill(page: CdpPage, browser_session: BrowserSession, main_site: str) -> None:
    await form(page, browser_session, main_site, "document.getElementById('name').oninput = () => alert('Pause')")
    before = await page.observe()
    result = await asyncio.wait_for(
        page.act(
            Action(operation=Operation.FILL, target_id=find(before, "Name").id, text="Ada", form_fill=True), before
        ),
        5,
    )
    assert not result.form_unchanged
    assert browser_session.pending_dialog() is not None


async def test_navigation_during_input_stops_batch(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await form(
        page,
        browser_session,
        main_site,
        "document.getElementById('name').oninput = () => { location.href = '/safety.html'; }",
    )
    before = await page.observe()
    result = await asyncio.wait_for(
        page.act(
            Action(operation=Operation.FILL, target_id=find(before, "Name").id, text="Ada", form_fill=True), before
        ),
        5,
    )
    assert not result.form_unchanged
    assert not await page._form_unchanged((browser_session.active_session_id, "main", 1, None), "Ada")


async def test_input_can_announce_suggestions_after_typing(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await form(
        page,
        browser_session,
        main_site,
        "document.getElementById('name').oninput = event => { "
        "event.target.setAttribute('aria-controls', 'suggestions'); "
        "document.body.insertAdjacentHTML('beforeend', '<ul id=suggestions role=listbox></ul>'); "
        "setTimeout(() => { document.getElementById('suggestions').innerHTML = "
        "'<li role=option>Suggested person</li>'; }, 500); };",
    )
    before = await page.observe()
    result = await page.act(
        Action(operation=Operation.FILL, target_id=find(before, "Name").id, text="Ada", form_fill=True), before
    )
    assert result.outcome is StepOutcome.EXECUTED
    assert not result.form_unchanged
    assert find(await page.observe(), "Suggested person")


async def test_local_field_groups_do_not_include_a_sidebar_search(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await form(page, browser_session, main_site)
    html = (
        "<aside><label>Search<input type=search></label></aside>"
        "<main><div><label>Name<input></label><label>Email<input></label></div>"
        "<div><label>Other<input></label></div></main>"
    )
    await eval_value(
        browser_session, browser_session.active_session_id, f"document.body.innerHTML = {json.dumps(html)}"
    )
    before = await page.observe()
    assert find(before, "Name").form_id == find(before, "Email").form_id
    assert find(before, "Name").form_id is not None
    assert find(before, "Search").form_id is None
    assert find(before, "Other").form_id is None
