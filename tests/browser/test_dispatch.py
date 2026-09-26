"""Pointer handlers can invalidate a target before its press or replace an editor before typing."""

import asyncio
from collections.abc import Coroutine
from unittest.mock import AsyncMock

import pytest

from fastbrowse.agent import Agent
from fastbrowse.browser import BrowserSession, CdpPage
from fastbrowse.browser import page as page_module
from fastbrowse.effects import effect
from fastbrowse.models import Operation, StepOutcome
from fastbrowse.page import Action, BrowserError
from tests.browser.test_browser import eval_value, find, observe_until, wait_until
from tests.test_policy import ScriptedJev
from tests.test_retrieval import ScriptedLLM


@pytest.mark.parametrize("covered", [False, True])
@pytest.mark.parametrize("nested", ["document", "shadow", "iframe"])
async def test_click_uses_an_exposed_point_but_never_passes_through_a_cover(
    page: CdpPage, browser_session: BrowserSession, main_site: str, covered: bool, nested: str
) -> None:
    """Flight rows had a covered centre even when another part of the same control was exposed."""
    await page.navigate(f"{main_site}/dispatch.html")
    if nested == "iframe":
        await eval_value(
            browser_session,
            browser_session.active_session_id,
            "document.body.innerHTML = '<iframe width=700 height=300 src=/dispatch.html></iframe>'",
        )
        await observe_until(page, "One way")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "window.fixtureRoot = "
        + {
            "document": "document",
            "iframe": "document.querySelector('iframe').contentDocument",
            "shadow": "document.body.appendChild(document.createElement('div')).attachShadow({mode: 'open'})",
        }[nested]
        + "; "
        + (
            "fixtureRoot.append(...document.querySelectorAll('style, #target, #decoy, #cover')); "
            if nested == "shadow"
            else ""
        )
        + "const cover = fixtureRoot.querySelector('#cover'); cover.style.display = 'block'; "
        + ("" if covered else "cover.style.cssText += 'left:130px;top:90px;width:40px;height:20px'; ")
        + "window.clickedAt = null; fixtureRoot.querySelector('#target').addEventListener('click', "
        "e => window.clickedAt = [e.clientX, e.clientY]);",
    )
    before = await page.observe()
    target = find(before, "One way")
    result = await page.act(Action(operation=Operation.CLICK, target_id=target.id), before)
    point = await eval_value(browser_session, browser_session.active_session_id, "window.clickedAt")
    after = await page.observe()
    if covered:
        assert result.outcome is StepOutcome.COVERED and not result.page_changed
        assert point is None
        assert find(after, "One way").selected is False
    else:
        assert result.outcome is StepOutcome.EXECUTED and result.page_changed
        assert point == [105, 90]
        assert find(after, "One way").selected is True
        assert effect(before, after, target).set_something


@pytest.mark.parametrize(
    "nested",
    [
        "<button>Delete</button>",
        "<label><input type=checkbox>Archive</label>",
        "<div role=gridcell>Cell</div>",
    ],
)
async def test_exposed_edge_belonging_to_a_nested_control_is_not_the_target(
    page: CdpPage, browser_session: BrowserSession, main_site: str, nested: str
) -> None:
    """A covered row whose exposed edges are its own Delete button must not have Delete pressed for it."""
    await page.navigate(f"{main_site}/dispatch.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "const row = document.createElement('div'); row.id = 'target'; row.setAttribute('role', 'option'); "
        "row.style.cssText = getComputedStyle(document.getElementById('target')).cssText; "
        "row.textContent = 'One way'; document.getElementById('target').replaceWith(row); "
        f"row.insertAdjacentHTML('beforeend', {nested!r}); "
        "row.lastElementChild.style.cssText = 'position:absolute;inset:0;margin:0'; "
        "const cover = document.getElementById('cover'); cover.style.display = 'block'; "
        "cover.style.cssText += 'left:130px;top:90px;width:40px;height:20px';",
    )
    before = await page.observe()
    result = await page.act(Action(operation=Operation.CLICK, target_id=find(before, "One way").id), before)
    assert result.outcome is StepOutcome.COVERED
    assert await eval_value(browser_session, browser_session.active_session_id, "window.clicks") == []


async def test_transparent_checkbox_filling_its_label_is_the_labels_target(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        'document.body.innerHTML = \'<label style="position:relative;display:inline-block;padding:12px">'
        'Direct service<input type=checkbox style="opacity:0;position:absolute;inset:0;margin:0"></label>\'; true',
    )
    obs = await page.observe()
    result = await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "Direct service").id), obs)
    assert result.outcome is StepOutcome.EXECUTED
    assert await eval_value(
        browser_session, browser_session.active_session_id, "document.querySelector('input').checked"
    )


async def test_hover_target_filling_a_button_does_not_cover_it(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.head.insertAdjacentHTML('beforeend', '<style>.tip{display:none}.face:hover .tip{display:block}"
        "</style>'); document.body.innerHTML = '<button onclick=\"window.pressed = (window.pressed || 0) + 1\" "
        'style="padding:0"><span class=face style="display:block;padding:12px">Save<span class=tip>'
        '<img alt="" width=8 height=8 src="data:image/gif;base64,R0lGODlhAQABAAAAACw="></span></span></button>\'; true',
    )
    obs = await page.observe()
    button = next(c for c in obs.controls if c.role == "button")
    result = await page.act(Action(operation=Operation.CLICK, target_id=button.id), obs)
    assert result.outcome is StepOutcome.EXECUTED, result.detail
    assert await eval_value(browser_session, browser_session.active_session_id, "window.pressed") == 1


@pytest.mark.parametrize("associated", [False, True])
async def test_transparent_input_hit_must_be_the_input_or_its_own_label(
    page: CdpPage, browser_session: BrowserSession, main_site: str, associated: bool
) -> None:
    await page.navigate(f"{main_site}/todos.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "const input = document.querySelectorAll('input')[1]; input.id = 'choice'; "
        "const label = input.nextElementSibling; "
        "label.style.cssText = 'position:absolute;inset:0;background:white'; "
        + ("label.htmlFor = 'choice';" if associated else ""),
    )
    obs = await page.observe()
    # An associated visible label is the control itself, named by its text; an unassociated one only covers.
    target = next(c for c in obs.controls if "walk the dog" in (c.context, c.label))
    result = await page.act(Action(operation=Operation.CLICK, target_id=target.id), obs)
    assert result.outcome is (StepOutcome.EXECUTED if associated else StepOutcome.COVERED)
    assert result.page_changed is associated
    assert await eval_value(browser_session, browser_session.active_session_id, "input.checked") is associated


async def test_link_inside_a_label_does_not_count_as_its_input(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    """A link filling the label takes the click itself; the input never toggles."""
    await page.navigate(f"{main_site}/todos.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "const input = document.querySelectorAll('input')[1]; input.id = 'choice'; "
        "input.style.opacity = '1'; const label = input.nextElementSibling; label.htmlFor = 'choice'; "
        "label.style.cssText = 'position:absolute;inset:0;background:white'; "
        "label.innerHTML = '<a href=\\'#linked\\' style=\\'display:block;height:100%\\'>walk the dog</a>';",
    )
    obs = await page.observe()
    target = next(c for c in obs.controls if c.input_type == "checkbox" and "walk the dog" in (c.context, c.label))
    result = await page.act(Action(operation=Operation.CLICK, target_id=target.id), obs)
    assert result.outcome is StepOutcome.COVERED
    assert await eval_value(browser_session, browser_session.active_session_id, "[input.checked, location.hash]") == [
        False,
        "",
    ]


@pytest.mark.parametrize("nested", ["document", "shadow", "iframe"])
async def test_fingerprint_tracks_selection_without_counting_text_field_values(
    page: CdpPage, browser_session: BrowserSession, main_site: str, nested: str
) -> None:
    """Checked and selected properties can change without changing any text or DOM attributes."""
    await page.navigate(f"{main_site}/todos.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.body.innerHTML = '<div></div><iframe></iframe>'; window.fixtureRoot = "
        + {
            "document": "document.querySelector('div')",
            "shadow": "document.querySelector('div').attachShadow({mode: 'open'})",
            "iframe": "document.querySelector('iframe').contentDocument.body",
        }[nested]
        + "; fixtureRoot.innerHTML = '<input type=checkbox><select multiple><option>First</option>' "
        "+ '<option>Second</option></select><button role=switch aria-checked=false>Nonstop only</button>' "
        "+ '<button role=option aria-selected=false>One way</button><input type=text>' "
        "+ '<button hidden role=tab aria-selected=false>Slide 2</button>';",
    )
    before = await page._fingerprint()
    for change in (
        "fixtureRoot.querySelector('input').checked = true",
        "fixtureRoot.querySelectorAll('option')[1].selected = true",
        "fixtureRoot.querySelector('[role=switch]').setAttribute('aria-checked', 'true')",
        "fixtureRoot.querySelector('[role=option]').setAttribute('aria-selected', 'true')",
    ):
        await eval_value(browser_session, browser_session.active_session_id, change)
        after = await page._fingerprint()
        assert after != before
        before = after
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "fixtureRoot.querySelector('[type=text]').value = 'requests'; "
        "fixtureRoot.querySelector('[role=tab]').setAttribute('aria-selected', 'true')",
    )
    # Neither a typed value nor a hidden carousel's selection is progress.
    assert await page._fingerprint() == before


@pytest.mark.parametrize("mode", ["move", "animate", "cover", "relabel"])
async def test_pointer_entry_rechecks_the_target_before_pressing(
    page: CdpPage, browser_session: BrowserSession, main_site: str, mode: str
) -> None:
    await page.navigate(f"{main_site}/dispatch.html?{mode}")
    obs = await page.observe()
    target = find(obs, "One way")
    result = await page.act(Action(operation=Operation.CLICK, target_id=target.id), obs)
    presses, clicks, selected = await eval_value(
        browser_session,
        browser_session.active_session_id,
        "[presses, clicks, document.getElementById('target').getAttribute('aria-selected')]",
    )
    if mode in {"move", "animate"}:
        assert result.outcome is StepOutcome.EXECUTED
        assert presses == clicks == ["target"]
        assert selected == "true"
        assert await eval_value(browser_session, browser_session.active_session_id, "pressPositions") == [400]
    else:
        assert result.outcome is (StepOutcome.COVERED if mode == "cover" else StepOutcome.STALE)
        assert presses == clicks == []
        assert selected == "false"


@pytest.mark.parametrize("framed", [False, True])
@pytest.mark.parametrize("change", ["cover", "relabel", "detach"])
async def test_target_changed_in_a_pointer_entry_frame_is_never_pressed(
    page: CdpPage, browser_session: BrowserSession, main_site: str, framed: bool, change: str
) -> None:
    await page.navigate(main_site)
    session_id = browser_session.active_session_id
    if framed:
        # Chrome throttles animation frames in offscreen children; this fixture needs its entry frame to run.
        await eval_value(browser_session, session_id, "document.querySelector('iframe').scrollIntoView()")
        obs = await observe_until(page, "Frame button")
        frame_id = find(obs, "Frame button").frame_id
        assert frame_id is not None
        session_id = browser_session.frame_sessions()[frame_id]
    mutation = {
        "cover": "document.getElementById('cover').style.display = 'block'",
        "relabel": "e.textContent = 'Delete reservation'",
        "detach": "e.replaceWith(e.cloneNode(true))",
    }[change]
    await eval_value(
        browser_session,
        session_id,
        "document.body.innerHTML = '<button id=target>One way</button><div id=cover "
        'style="display:none;position:fixed;inset:0;background:white;z-index:1"></div>\'; '
        "window.presses = 0; document.addEventListener('mousedown', () => window.presses++); "
        "const e = document.getElementById('target'); "
        f"e.onpointerenter = () => requestAnimationFrame(() => {{ {mutation}; }});",
    )
    obs = await observe_until(page, "One way")
    result = await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "One way").id), obs)
    assert result.outcome is (StepOutcome.COVERED if change == "cover" else StepOutcome.STALE)
    assert await eval_value(browser_session, session_id, "window.presses") == 0


@pytest.mark.parametrize("mode", ["replace", "replace-late", "replace-decoy"])
@pytest.mark.parametrize("framed", [False, True])
async def test_fill_follows_a_replacement_only_at_the_original_position(
    page: CdpPage, browser_session: BrowserSession, main_site: str, mode: str, framed: bool
) -> None:
    await page.navigate(f"{main_site}/dispatch.html?{mode}")
    if framed:
        await eval_value(
            browser_session,
            browser_session.active_session_id,
            f"document.body.innerHTML = '<iframe width=700 height=300 src=\"/dispatch.html?{mode}\"></iframe>'",
        )
    obs = await observe_until(page, "Origin")
    result = await page.act(Action(operation=Operation.FILL, target_id=find(obs, "Origin").id, text="London"), obs)
    assert result.outcome is (StepOutcome.FAILED if mode == "replace-decoy" else StepOutcome.EXECUTED)
    assert find(await page.observe(), "Origin").value == ("" if mode == "replace-decoy" else "London")
    view = "document.querySelector('iframe').contentWindow" if framed else "window"
    assert await eval_value(browser_session, browser_session.active_session_id, f"{view}.clicks") == ["target"]


async def test_a_native_date_field_is_committed_through_the_value_setter(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    # Typing into a native date input lands in whichever locale segment its own picker has focused, not the ISO
    # value a field writer produces, so the value is set the way the picker itself would commit one.
    await page.navigate(f"{main_site}/dispatch.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        'document.body.innerHTML = \'<label>Start Date<input type="date" id="start"></label>\'',
    )
    obs = await observe_until(page, "Start Date")
    target = find(obs, "Start Date")
    assert target.input_type == "date"
    result = await page.act(Action(operation=Operation.FILL, target_id=target.id, text="2026-09-28"), obs)
    assert result.outcome is StepOutcome.EXECUTED
    assert find(await page.observe(), "Start Date").value == "2026-09-28"


async def test_a_native_date_field_that_refuses_a_malformed_value_fails(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(f"{main_site}/dispatch.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        'document.body.innerHTML = \'<label>Start Date<input type="date" id="start"></label>\'',
    )
    obs = await observe_until(page, "Start Date")
    target = find(obs, "Start Date")
    # A native date input silently keeps an empty value for anything that is not its own ISO shape.
    result = await page.act(Action(operation=Operation.FILL, target_id=target.id, text="28 September 2026"), obs)
    assert result.outcome is StepOutcome.FAILED


async def test_secret_fill_does_not_follow_a_replacement(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(f"{main_site}/dispatch.html?replace")
    obs = await page.observe()
    result = await page.act(
        Action(
            operation=Operation.FILL,
            target_id=find(obs, "Origin").id,
            text="secret-value",
            secret=True,
            secret_origin=main_site,
        ),
        obs,
    )
    assert result.outcome is StepOutcome.FAILED
    assert find(await page.observe(), "Origin").value == ""
    assert await eval_value(browser_session, browser_session.active_session_id, "clicks") == ["target"]


@pytest.mark.parametrize("change", ["clone", "reload", "scope"])
async def test_retargeting_requires_the_receiving_document_and_guard_to_survive(
    page: CdpPage, browser_session: BrowserSession, main_site: str, change: str
) -> None:
    await page.navigate(f"{main_site}/dispatch.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.body.innerHTML = '<iframe width=700 height=300 src=\"/dispatch.html\"></iframe>'",
    )
    before = await observe_until(page, "One way")
    target = find(before, "One way")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "const frame = document.querySelector('iframe'); "
        + (
            "frame.contentWindow.location.reload();"
            if change == "reload"
            else "const e = frame.contentDocument.getElementById('target'); e.replaceWith(e.cloneNode(true));"
            + (
                "frame.contentDocument.getElementById('decoy').textContent = 'Changed context';"
                if change == "scope"
                else ""
            )
        ),
    )
    if change == "reload":
        # The frame may still expose the old document on the tick reload() returns.
        async def reloaded() -> bool:
            return any(
                c.label == "One way" and c.retarget_key != target.retarget_key for c in (await page.observe()).controls
            )

        await wait_until(reloaded)
    result = await Agent(page, ScriptedJev({}), ScriptedLLM([]))._act_on_twin(
        Action(operation=Operation.CLICK, target_id=target.id), before, target
    )
    if change == "clone":
        assert result is not None and result.outcome is StepOutcome.EXECUTED
    else:
        assert result is None
    assert await eval_value(
        browser_session, browser_session.active_session_id, "document.querySelector('iframe').contentWindow.presses"
    ) == (["target"] if change == "clone" else [])


async def test_uncertain_press_is_never_replayed(
    page: CdpPage, browser_session: BrowserSession, main_site: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    await page.navigate(f"{main_site}/dispatch.html")
    obs = await page.observe()
    original = page._input
    sends = 0

    async def lose_press_response(send: Coroutine[None, None, object]) -> None:
        nonlocal sends
        await original(send)
        sends += 1
        if sends == 2:
            raise BrowserError("press response lost")

    monkeypatch.setattr(page, "_input", lose_press_response)
    with pytest.raises(BrowserError, match="press response lost"):
        await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "One way").id), obs)
    assert await eval_value(browser_session, browser_session.active_session_id, "[presses, clicks]") == [["target"], []]


@pytest.mark.parametrize("deferred", [False, True])
async def test_pointer_entry_dialog_does_not_block_a_fresh_hit_test(
    page: CdpPage, browser_session: BrowserSession, main_site: str, deferred: bool
) -> None:
    await page.navigate(f"{main_site}/dispatch.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.getElementById('target').onpointerenter = () => "
        + ("requestAnimationFrame(() => confirm('Continue?'))" if deferred else "confirm('Continue?')"),
    )
    obs = await page.observe()
    # The bound catches a click that hangs behind the dialog; a busy CI runner took over 2s to get through it.
    async with asyncio.timeout(10):
        result = await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "One way").id), obs)
    assert result.outcome is StepOutcome.FAILED
    assert browser_session.pending_dialog() is not None
    await browser_session.handle_dialog(False)
    assert await eval_value(browser_session, browser_session.active_session_id, "presses") == []


async def test_an_unstable_target_expires_without_a_press(page: CdpPage, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(page_module, "_TARGET_STABILITY_SECONDS", 0)
    monkeypatch.setattr(page, "_move", AsyncMock())
    monkeypatch.setattr(page, "_evaluate", AsyncMock())
    monkeypatch.setattr(page, "_before_action", AsyncMock(return_value=("fingerprint", ["guard"], (20.0, 20.0))))
    pressed = AsyncMock()
    monkeypatch.setattr(page, "_input", pressed)
    outcome, _ = await page._click_point(("session", "main", 1, ["guard"]), (10.0, 10.0))
    assert outcome is StepOutcome.STALE
    pressed.assert_not_called()


async def test_hydration_marking_a_control_enabled_does_not_make_it_stale(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    """Google Flights adds aria-disabled="false" as it hydrates, after the run first observed its controls."""
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.body.innerHTML = '<button onclick=\"this.dataset.clicked = 1\">Round trip</button>'; true",
    )
    obs = await page.observe()
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.querySelector('button').setAttribute('aria-disabled', 'false'); true",
    )
    target = next(c for c in obs.controls if c.label == "Round trip")
    result = await page.act(Action(operation=Operation.CLICK, target_id=target.id), obs)
    assert result.outcome is StepOutcome.EXECUTED


async def test_select_the_page_refuses_is_not_executed(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    """A change handler that restores the previous option must not read as a first write, which is progress."""
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.body.innerHTML = '<label>Plan <select id=plan><option>Free</option><option>Pro</option>"
        "</select></label>'; document.getElementById('plan').onchange = e => { e.target.value = 'Free'; }; true",
    )
    obs = await page.observe()
    action = Action(operation=Operation.SELECT, target_id=find(obs, "Plan").id, text="Pro")
    result = await page.act(action, obs)
    assert result.outcome is StepOutcome.FAILED
    assert result.detail and "'Free'" in result.detail
