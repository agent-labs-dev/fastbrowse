"""Directed navigation to a caller-supplied address, through the real browser."""

import asyncio
import json
from collections.abc import Mapping

import pytest
from pydantic import JsonValue

from fastbrowse.agent import Agent
from fastbrowse.browser import BrowserSession, CdpPage
from fastbrowse.jev import Evaluation, NoulAnswer, Question
from fastbrowse.models import BrowserConnection, Limits, Operation, Status, StepOutcome
from fastbrowse.page import Action
from tests.browser.test_browser import eval_value, find, wait_until
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


async def test_the_history_entry_a_page_is_follows_the_browser_not_the_address(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    """The run's lineage table (`Agent._note_lineage`) rests on these: a navigation and pushState make an entry,
    a reload and replaceState keep it, and Back and Forward return to the entries they left."""
    session_id = browser_session.active_session_id

    async def entry() -> str | None:
        return (await page.observe()).history_entry

    async def entry_is(expected: str | None) -> bool:
        return await entry() == expected

    async def settled(script: str) -> None:
        await eval_value(browser_session, session_id, "window.marker = 1; " + script)
        await wait_until(lambda: eval_value(browser_session, session_id, "window.marker === undefined"))

    await page.navigate(main_site)
    first = await entry()
    await page.navigate(f"{main_site}/dispatch.html")
    second = await entry()
    assert first and second and first != second
    await settled("location.reload()")
    assert await entry() == second
    await eval_value(browser_session, session_id, "history.pushState({}, '', '/dispatch.html?pushed')")
    pushed = await entry()
    assert pushed not in {first, second}
    await eval_value(browser_session, session_id, "history.replaceState({}, '', '/dispatch.html?replaced')")
    assert await entry() == pushed
    observed = await page.observe()
    assert (await page.act(Action(operation=Operation.BACK), observed)).outcome is StepOutcome.EXECUTED
    await wait_until(lambda: entry_is(second))
    await eval_value(browser_session, session_id, "history.forward()")
    await wait_until(lambda: entry_is(pushed))


@pytest.mark.parametrize("pushes", [1, 10])
async def test_a_push_state_between_history_and_snapshot_never_pairs_a_page_with_another_entry(
    page: CdpPage, browser_session: BrowserSession, main_site: str, monkeypatch: pytest.MonkeyPatch, pushes: int
) -> None:
    """History and the snapshot are read at once. A pushState landing between them paired the new page with the old
    entry, which then took the new page's lineage. The pair is read again until both name one address, and a pair
    that never agrees names no entry."""
    session_id = browser_session.active_session_id
    await page.navigate(f"{main_site}/dispatch.html")
    read_history, snapshot = page._history, page._snapshot_all_frames
    snapped = asyncio.Event()
    pushed = 0

    async def snapshot_first() -> object:
        result = await snapshot()
        snapped.set()
        return result

    async def history_after_a_push() -> tuple[bool, str | None, str | None]:
        nonlocal pushed
        await snapped.wait()
        snapped.clear()
        if pushed < pushes:
            pushed += 1
            await eval_value(
                browser_session, session_id, f"history.pushState({{}}, '', '/dispatch.html?step={pushed}')"
            )
        return await read_history()

    monkeypatch.setattr(page, "_snapshot_all_frames", snapshot_first)
    monkeypatch.setattr(page, "_history", history_after_a_push)
    observed = await page.observe()
    monkeypatch.undo()
    if pushes == 1:
        # Read again once the push had landed: the page and its entry agree.
        assert observed.url.endswith("/dispatch.html?step=1")
        assert observed.history_entry is not None and observed.history_entry == (await page.observe()).history_entry
    else:
        assert observed.history_entry is None


@pytest.mark.parametrize(
    ("handler", "moved"),
    [
        # A form submitted to the same path loads a new document after the click.
        ("this.form.submit()", True),
        # A single-page app answering the click in its own task, its promise reactions included.
        ("history.pushState({}, '', '?q=corrected')", True),
        ("Promise.resolve().then(() => history.pushState({}, '', '?q=corrected'))", True),
        ("history.replaceState({}, '', '?q=corrected')", True),
        # A rewrite the page makes later, on a timer or after a fetch, cannot be tied to the click.
        ("setTimeout(() => history.pushState({}, '', '?q=later'), 40)", False),
        ("", False),
    ],
)
async def test_an_action_reports_only_the_moves_it_made_itself(
    page: CdpPage, browser_session: BrowserSession, main_site: str, handler: str, moved: bool
) -> None:
    """A guessed page that rewrote its own address on a timer just after a click was taken for the site answering
    the click, and the guess was cleared. Only a new document after the action, or an address its own input event
    changed, is a move the action made."""
    await page.navigate(f"{main_site}/?q=guess")
    session_id = browser_session.active_session_id
    form = (
        '<form action="/" method=get><input name=q value=corrected>'
        f'<button type=button onclick="{handler}">Search</button></form>'
    )
    assert (
        await eval_value(
            browser_session,
            session_id,
            f"document.body.innerHTML = {json.dumps(form)}; true",
        )
        is True
    )
    obs = await page.observe()
    result = await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "Search").id), obs)
    assert result.outcome is StepOutcome.EXECUTED
    assert result.moved is moved


async def test_accepting_a_dialog_that_navigates_is_a_move_the_run_made(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    """A dialog blocks the page, so no mark of its document could be taken before accepting it, and a confirm that
    navigated once accepted was never reported as the run's move."""
    await page.navigate(f"{main_site}/?q=guess")
    session_id = browser_session.active_session_id
    form = "<button onclick=\"if (confirm('Search again?')) location.href = '/inner.html'\">Search</button>"
    assert await eval_value(browser_session, session_id, f"document.body.innerHTML = {json.dumps(form)}; true") is True
    obs = await page.observe()
    clicked = await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "Search").id), obs)
    assert clicked.outcome is StepOutcome.EXECUTED
    await wait_until(lambda: browser_session.pending_dialog() is not None)
    confirm = await page.observe()
    async with asyncio.timeout(10):
        accepted = await page.act(Action(operation=Operation.DIALOG, accept_dialog=True), confirm)
    assert accepted.outcome is StepOutcome.EXECUTED
    assert accepted.moved is True


async def test_accepting_a_dialog_a_new_page_opened_as_it_loaded_is_no_move_of_the_runs(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    """A click led to a page that opened an alert as it loaded. Accepting it was judged against the mark taken on the
    page the click left, so the new document read as a move the acceptance made, and a guess could be cleared."""
    await page.navigate(f"{main_site}/?q=guess")
    session_id = browser_session.active_session_id
    link = '<a href="/alert-on-load.html">Results</a>'
    assert await eval_value(browser_session, session_id, f"document.body.innerHTML = {json.dumps(link)}; true") is True
    obs = await page.observe()
    await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "Results").id), obs)
    await wait_until(lambda: browser_session.pending_dialog() is not None)
    alert = await page.observe()
    async with asyncio.timeout(10):
        accepted = await page.act(Action(operation=Operation.DIALOG, accept_dialog=True), alert)
    assert accepted.outcome is StepOutcome.EXECUTED
    assert accepted.moved is False


@pytest.mark.parametrize(
    ("markup", "moved"),
    [
        ('<a href="/inner.html">Go</a>', True),
        ('<form action="/inner.html"><button>Go</button></form>', True),
        ("<button onclick=\"location.href = '/inner.html'\">Go</button>", True),
        # The page sends itself elsewhere later, on a timer or after a fetch, not as the click's own doing.
        ("<button onclick=\"setTimeout(() => location.href = '/inner.html', 60)\">Go</button>", False),
    ],
)
async def test_only_a_navigation_the_runs_input_began_is_its_move(
    page: CdpPage, browser_session: BrowserSession, main_site: str, markup: str, moved: bool
) -> None:
    """A page script that set its location after a delay loaded a new document during an unrelated click, and the
    new document was read as the click's move, so a guess could be cleared."""
    await page.navigate(f"{main_site}/?q=guess")
    session_id = browser_session.active_session_id
    assert (
        await eval_value(
            browser_session,
            session_id,
            "{ const range = document.createRange(); range.selectNode(document.body); "
            f"document.body.replaceChildren(range.createContextualFragment({json.dumps(markup)})); }} true",
        )
        is True
    )
    obs = await page.observe()
    result = await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "Go").id), obs)
    assert result.outcome is StepOutcome.EXECUTED
    assert result.moved is moved
