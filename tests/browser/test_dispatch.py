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


@pytest.mark.parametrize("destination_change", ["none", "covered", "renamed"])
async def test_a_drag_hit_tests_the_destination_before_pressing(
    page: CdpPage, browser_session: BrowserSession, main_site: str, destination_change: str
) -> None:
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        """document.body.innerHTML = `
          <div id=source draggable=true style="position:fixed;left:20px;top:20px;width:100px;height:80px">Card A</div>
          <button id=drop style="position:fixed;left:220px;top:20px;width:100px;height:80px">Done</button>`;
        window.presses = 0;
        source.onpointerdown = () => window.presses++;
        source.ondragstart = e => e.dataTransfer.setData('text/plain', 'Card A');
        drop.ondragover = e => e.preventDefault();
        drop.ondrop = e => { e.preventDefault(); drop.textContent = e.dataTransfer.getData('text/plain'); };
        true""",
    )
    obs = await page.observe()
    source, destination = find(obs, "Card A"), find(obs, "Done")
    if destination_change != "none":
        await eval_value(
            browser_session,
            browser_session.active_session_id,
            "document.body.insertAdjacentHTML('beforeend', "
            "'<div style=\"position:fixed;inset:0 auto auto 200px;width:160px;height:120px;z-index:10\"></div>'); true"
            if destination_change == "covered"
            else "drop.textContent = 'Trash'; true",
        )

    result = await page.act(Action(operation=Operation.DRAG, target_id=source.id, destination_id=destination.id), obs)
    assert result.outcome is (StepOutcome.EXECUTED if destination_change == "none" else StepOutcome.STALE)
    assert await eval_value(browser_session, browser_session.active_session_id, "window.presses") == (
        1 if destination_change == "none" else 0
    )
    if destination_change == "none":
        assert result.page_changed
        assert await eval_value(browser_session, browser_session.active_session_id, "drop.textContent") == "Card A"


@pytest.mark.parametrize("change", ["scroll", "move", "replace", "destination"])
async def test_a_drag_revalidates_its_endpoints_after_scrolling_and_pointer_entry(
    page: CdpPage, browser_session: BrowserSession, main_site: str, change: str
) -> None:
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        """document.body.innerHTML = `
          <div id=source draggable=true
            style="position:absolute;left:20px;top:20px;width:100px;height:80px">Card A</div>
          <button id=drop style="position:absolute;left:220px;top:20px;width:100px;height:80px">Done</button>`;
        window.presses = [];
        document.addEventListener('pointerdown', e => window.presses.push(e.target.id));
        source.ondragstart = e => e.dataTransfer.setData('text/plain', 'Card A');
        drop.ondragover = e => e.preventDefault();
        drop.ondrop = e => { e.preventDefault(); drop.textContent = e.dataTransfer.getData('text/plain'); };
        """
        + {
            "scroll": "drop.style.top = '2000px';",
            "move": "source.onpointerenter = () => { source.style.top = '200px'; };",
            "replace": "source.onpointerenter = () => { source.replaceWith(source.cloneNode(true)); };",
            "destination": "source.onpointerenter = () => { drop.textContent = 'Trash'; };",
        }[change],
    )
    obs = await page.observe()
    result = await page.act(
        Action(operation=Operation.DRAG, target_id=find(obs, "Card A").id, destination_id=find(obs, "Done").id), obs
    )

    assert result.outcome is (StepOutcome.EXECUTED if change == "move" else StepOutcome.STALE)
    assert await eval_value(browser_session, browser_session.active_session_id, "window.presses") == (
        ["source"] if change == "move" else []
    )
    if change == "move":
        assert await eval_value(browser_session, browser_session.active_session_id, "drop.textContent") == "Card A"


@pytest.mark.parametrize(
    "change,dropped",
    [
        # Another control under the pointer: the native drag is cancelled, so not even the source sees a drop.
        ("drop.textContent = 'Trash';", []),
        # The same control somewhere else: the release follows it, after the dragover that lets it take the drop.
        ("drop.style.top = '300px';", ["drop"]),
        ("drop.ondragover = () => { drop.style.transform = 'translateY(2px)'; };", ["drop"]),
        # Away and back with no move in between: Chrome still targets the control it last hovered.
        (
            "drop.style.left = '600px'; document.body.insertAdjacentHTML('beforeend', "
            "'<div id=decoy style=\"position:fixed;left:220px;top:20px;width:100px;height:80px\"></div>'); "
            "decoy.ondragover = e => { if (e.clientX >= 270) { drop.style.left = '220px'; "
            "decoy.style.left = '600px'; } };",
            ["drop"],
        ),
    ],
    ids=["renamed", "moved", "nudged", "returned"],
)
async def test_a_destination_changed_by_the_drag_itself_is_dropped_on_only_if_it_is_the_same_control(
    page: CdpPage, browser_session: BrowserSession, main_site: str, change: str, dropped: list[str]
) -> None:
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        """document.body.innerHTML = `
          <div id=home style="position:fixed;left:0;top:0;width:160px;height:120px">
            <div id=source draggable=true
              style="position:absolute;left:20px;top:20px;width:100px;height:80px">Card A</div></div>
          <button id=drop style="position:fixed;left:220px;top:20px;width:100px;height:80px">Done</button>`;
        window.drops = [];
        document.addEventListener('dragover', e => e.preventDefault());
        document.addEventListener('drop', e => { e.preventDefault(); window.drops.push(e.target.id); });
        source.ondragstart = e => { e.dataTransfer.setData('text/plain', 'Card A'); """
        + change
        + " }; true",
    )
    obs = await page.observe()
    source, destination = find(obs, "Card A"), find(obs, "Done")
    result = await page.act(Action(operation=Operation.DRAG, target_id=source.id, destination_id=destination.id), obs)

    assert result.outcome is (StepOutcome.EXECUTED if dropped else StepOutcome.STALE)
    assert await eval_value(browser_session, browser_session.active_session_id, "window.drops") == dropped


async def test_a_drag_onto_its_own_source_is_refused_before_pressing(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.body.innerHTML = '<button id=source draggable=true>Card A</button>'; "
        "window.clicks = 0; source.onclick = () => window.clicks++; true",
    )
    obs = await page.observe()
    source = find(obs, "Card A")
    result = await page.act(Action(operation=Operation.DRAG, target_id=source.id, destination_id=source.id), obs)

    assert result.outcome is StepOutcome.FAILED
    assert await eval_value(browser_session, browser_session.active_session_id, "window.clicks") == 0


async def test_a_drag_across_frame_sessions_is_refused_before_pressing(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(main_site)
    await observe_until(page, "Frame button")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.body.insertAdjacentHTML('afterbegin', '<div draggable=true id=source>Card A</div>'); "
        "window.presses = 0; document.addEventListener('pointerdown', () => window.presses++);",
    )
    obs = await page.observe()
    destination = find(obs, "Frame button")
    assert destination.frame_id in browser_session.frame_sessions()
    result = await page.act(
        Action(operation=Operation.DRAG, target_id=find(obs, "Card A").id, destination_id=destination.id), obs
    )

    assert result.outcome is StepOutcome.FAILED
    assert result.detail == "drag across browser frame sessions is not supported"
    assert await eval_value(browser_session, browser_session.active_session_id, "window.presses") == 0


async def test_an_unstable_target_expires_without_a_press(page: CdpPage, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(page_module, "_TARGET_STABILITY_SECONDS", 0)
    monkeypatch.setattr(page, "_move", AsyncMock())
    monkeypatch.setattr(page, "_evaluate", AsyncMock())
    monkeypatch.setattr(page, "_before_action", AsyncMock(return_value=("fingerprint", ["guard"], (20.0, 20.0), False)))
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


@pytest.mark.parametrize("detail_race", [False, True])
@pytest.mark.parametrize("semantic", [False, True])
async def test_covered_target_reports_visible_overlay_controls(
    page: CdpPage,
    browser_session: BrowserSession,
    main_site: str,
    monkeypatch: pytest.MonkeyPatch,
    detail_race: bool,
    semantic: bool,
) -> None:
    await page.navigate(f"{main_site}/dispatch.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "const cover = document.getElementById('cover'); cover.style.display = 'block'; "
        "cover.setAttribute('role','dialog'); cover.setAttribute('aria-label','Basket'); "
        "cover.innerHTML = '<button aria-label=Close>Close</button>'; "
        "cover.firstElementChild.onclick = () => cover.style.display = 'none';",
    )
    if not semantic:
        await eval_value(
            browser_session,
            browser_session.active_session_id,
            "const cover = document.getElementById('cover'); cover.removeAttribute('role'); "
            "cover.removeAttribute('aria-label'); "
            "cover.style.cssText = 'display:block;position:fixed;inset:0;width:auto;height:auto;z-index:20'; "
            'cover.innerHTML = \'<div style="position:absolute;inset:0;background:white"></div>'
            '<button style="position:relative" aria-label=Close>Close</button>\'; '
            "cover.lastElementChild.onclick = () => cover.style.display = 'none';",
        )

    if detail_race:
        evaluate = page._evaluate

        async def racing_evaluate(session_id: str, expression: str, *, context_id: int | None = None) -> object:
            if "let cover = hit?.closest" in expression:
                raise BrowserError("Execution context changed while describing the obstruction")
            return await evaluate(session_id, expression, context_id=context_id)

        monkeypatch.setattr(page, "_evaluate", racing_evaluate)
    before = await page.observe()
    result = await page.act(Action(operation=Operation.CLICK, target_id=find(before, "One way").id), before)
    assert result.outcome is StepOutcome.COVERED
    if detail_race:
        assert result.detail == "Target is covered."
    else:
        assert "Close" in (result.detail or "")
        if semantic:
            assert "Basket" in (result.detail or "")
    assert await eval_value(browser_session, browser_session.active_session_id, "window.clicks") == []
    closed = await page.act(
        Action(operation=Operation.CLICK, target_id=find(await page.observe(), "Close").id), await page.observe()
    )
    assert closed.outcome is StepOutcome.EXECUTED
    after = await page.observe()
    retried = await page.act(Action(operation=Operation.CLICK, target_id=find(after, "One way").id), after)
    assert retried.outcome is StepOutcome.EXECUTED


async def test_covered_target_does_not_offer_its_fixed_parent_controls(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(f"{main_site}/dispatch.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "const shell = document.createElement('div'); shell.style.cssText = 'position:fixed;inset:0'; "
        "document.body.append(shell); "
        "shell.append(document.getElementById('target'), document.getElementById('cover')); "
        "const cover = document.getElementById('cover'); cover.style.display = 'block'; "
        "shell.insertAdjacentHTML('beforeend', '<button>Unrelated action</button>'); true",
    )
    obs = await page.observe()
    result = await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "One way").id), obs)
    assert result.outcome is StepOutcome.COVERED
    assert "Unrelated action" not in (result.detail or "")
    assert "One way" not in (result.detail or "")


def _anchored_form(*, scrolls: bool, band: str) -> str:
    """A login form whose button sits under a band fixed to the window's foot, as an anchored ad does."""
    body = (
        '<form id=login onsubmit="event.preventDefault(); window.submitted = (window.submitted || 0) + 1" '
        'style="position:absolute;left:20px;bottom:30px">'
        "<input name=user aria-label=Username><input name=pass type=password aria-label=Password>"
        '<button id=go onclick="window.pressed = (window.pressed || 0) + 1">Login</button></form>'
    )
    inside = (
        '<iframe aria-label=Advertisement srcdoc="<p>Ad</p>" style="width:100%;height:100%;border:0"></iframe>'
        if band == "iframe"
        else '<button onclick="window.adClicks++">Shop now</button>'
    )
    return (
        "window.adClicks = 0; document.documentElement.style.height = '100%'; "
        "document.body.style.cssText = 'margin:0;position:relative;height:"
        + ("3000px" if scrolls else "100%;overflow:hidden")
        + "'; "
        f"document.body.innerHTML = {body!r}; "
        "const band = document.createElement('ins'); "
        "band.style.cssText = 'display:block;position:fixed;left:0;right:0;bottom:0;height:120px;"
        "background:white;z-index:9'; "
        "band.addEventListener('mousedown', () => window.adClicks++); "
        f"band.innerHTML = {inside!r}; document.body.append(band); "
        # The form starts in the band's place on the first screen, whatever the page's length.
        "document.getElementById('login').style.top = (innerHeight - 70) + 'px'; "
        "document.getElementById('login').style.bottom = ''; true"
    )


@pytest.mark.parametrize("band", ["div", "iframe"])
@pytest.mark.parametrize("scrolls", [True, False])
async def test_a_control_under_a_band_fixed_to_the_window_is_reached_without_touching_the_band(
    page: CdpPage, browser_session: BrowserSession, main_site: str, scrolls: bool, band: str
) -> None:
    """expandtesting-login: an ad anchored to the window's foot covered Login twice and the run ended stuck. A
    person scrolls the button clear of it; where the page cannot scroll, Login is pressed from the keyboard."""
    await page.navigate(main_site)
    session_id = browser_session.active_session_id
    await eval_value(browser_session, session_id, _anchored_form(scrolls=scrolls, band=band))
    assert (
        await eval_value(
            browser_session,
            session_id,
            "(r => document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2)?.id)"
            "(document.getElementById('go').getBoundingClientRect())",
        )
        != "go"
    ), "the fixture must start with Login under the band"
    obs = await page.observe()
    result = await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "Login").id), obs)
    assert result.outcome is StepOutcome.EXECUTED, result.detail
    # A page that scrolls has Login clicked where it was scrolled clear of the band; the keyboard is the fallback.
    assert (result.detail is None) is scrolls
    pressed, submitted, ad_clicks = await eval_value(
        browser_session, session_id, "[window.pressed, window.submitted, window.adClicks]"
    )
    assert (pressed, submitted, ad_clicks) == (1, 1, 0)


async def test_a_field_under_a_band_is_filled_and_submitted_from_the_keyboard(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    """The same ad covered the password field once the run had typed the username, so Enter was refused too."""
    await page.navigate(main_site)
    session_id = browser_session.active_session_id
    await eval_value(browser_session, session_id, _anchored_form(scrolls=False, band="div"))
    obs = await page.observe()
    filled = await page.act(Action(operation=Operation.FILL, target_id=find(obs, "Password").id, text="hunter2"), obs)
    assert filled.outcome is StepOutcome.EXECUTED, filled.detail
    obs = await page.observe()
    entered = await page.act(Action(operation=Operation.ENTER, target_id=find(obs, "Password").id), obs)
    assert entered.outcome is StepOutcome.EXECUTED, entered.detail
    value, submitted, ad_clicks = await eval_value(
        browser_session,
        session_id,
        "[document.querySelector('[name=pass]').value, window.submitted, window.adClicks]",
    )
    assert (value, submitted, ad_clicks) == ("hunter2", 1, 0)


@pytest.mark.parametrize("cover", ["dialog", "menu", "popover", "full-window"])
async def test_a_fixed_cover_the_page_means_to_be_answered_is_still_reported(
    page: CdpPage, browser_session: BrowserSession, main_site: str, cover: str
) -> None:
    """Only a band floating over the page is routed around. A dialog asks its question first, an open menu or
    popover is the page's to close, and a cover over the whole window shows something other than the control, so
    none is pressed through, by pointer or keyboard."""
    await page.navigate(main_site)
    session_id = browser_session.active_session_id
    await eval_value(browser_session, session_id, _anchored_form(scrolls=False, band="div"))
    # A setup script that throws evaluates to nothing, and the cover it meant to draw would never be tested.
    assert (
        await eval_value(
            browser_session,
            session_id,
            {
                "dialog": "document.querySelector('ins').setAttribute('role', 'dialog'); true",
                "menu": "document.querySelector('ins').setAttribute('role', 'menu'); true",
                # A shown popover sits in the top layer; its own inset is kept at the window's foot.
                "popover": "{ const shown = document.querySelector('ins'); shown.setAttribute('popover', 'manual'); "
                "shown.style.cssText += ';top:auto;margin:0;border:0;padding:0;width:auto'; shown.showPopover(); } "
                "document.querySelector('ins').matches(':popover-open')",
                "full-window": "document.querySelector('ins').style.cssText += ';top:0;height:auto'; true",
            }[cover],
        )
        is True
    )
    obs = await page.observe()
    result = await page.act(Action(operation=Operation.CLICK, target_id=find(obs, "Login").id), obs)
    assert result.outcome is StepOutcome.COVERED
    entered = await page.act(Action(operation=Operation.ENTER, target_id=find(obs, "Password").id), obs)
    assert entered.outcome is StepOutcome.COVERED
    assert await eval_value(browser_session, session_id, "[window.pressed, window.submitted, window.adClicks]") == [
        None,
        None,
        0,
    ]


@pytest.mark.parametrize("control", ["button", "field", "date"])
async def test_a_control_repurposed_while_scrolled_clear_of_a_band_is_never_pressed_or_typed_into(
    page: CdpPage, browser_session: BrowserSession, main_site: str, control: str
) -> None:
    """Scrolling a control out from under a band runs the page's scroll handlers. One that turned an authorized
    Login into "Delete account", or a username field into a card number field, would have had the keyboard act on
    what was never classified or authorized; the pointer path rechecks before every press, and so does this one. A
    date field's value is set without a key, and that branch once returned before the check could run."""
    await page.navigate(main_site)
    session_id = browser_session.active_session_id
    await eval_value(browser_session, session_id, _anchored_form(scrolls=True, band="div"))
    # Bands at the top, middle and foot of the window leave no alignment where the control is clear of them.
    assert (
        await eval_value(
            browser_session,
            session_id,
            "{ const foot = document.querySelector('ins'); "
            "for (const top of ['0px', 'calc(50% - 60px)']) { const band = foot.cloneNode(true); "
            "band.style.top = top; band.style.bottom = 'auto'; band.addEventListener('mousedown', () => "
            "window.adClicks++); document.body.append(band); } } "
            "document.getElementById('login').insertAdjacentHTML('beforeend', "
            "'<input type=date name=when aria-label=Departure>'); "
            "window.keys = 0; document.addEventListener('keydown', () => window.keys++, true); "
            "addEventListener('scroll', () => { document.getElementById('go').textContent = 'Delete account'; "
            "document.querySelector('[name=user]').setAttribute('aria-label', 'Card number'); "
            "document.querySelector('[name=when]').setAttribute('aria-label', 'Return date'); }, {once: true}); true",
        )
        is True
    )
    obs = await page.observe()
    action = {
        "button": Action(operation=Operation.CLICK, target_id=find(obs, "Login").id),
        "field": Action(operation=Operation.FILL, target_id=find(obs, "Username").id, text="ada"),
        "date": Action(operation=Operation.FILL, target_id=find(obs, "Departure").id, text="2026-11-14"),
    }[control]
    result = await page.act(action, obs)
    assert result.outcome is StepOutcome.STALE, result.detail
    # Refused by the route around the bands, never by a pointer path they do not let it take: the control changed,
    # or, on a loaded machine, its scroll had not been seen to finish, so nothing was acted on either way.
    assert result.detail in {"control changed before keyboard use", "page did not finish scrolling the control clear"}
    keys, pressed, values, ad_clicks = await eval_value(
        browser_session,
        session_id,
        "[window.keys, window.pressed, ['user', 'when'].map(n => document.querySelector(`[name=${n}]`).value), "
        "window.adClicks]",
    )
    assert (keys, pressed, values, ad_clicks) == (0, None, ["", ""], 0)
