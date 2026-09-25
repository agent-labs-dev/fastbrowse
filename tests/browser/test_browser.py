"""End-to-end tests of the browser layer against a real headless Chrome and fixture sites."""

from __future__ import annotations

import asyncio
import hashlib
import inspect
import os
from collections.abc import Awaitable, Callable
from typing import Any

import pytest

from fastbrowse.browser.page import CdpPage
from fastbrowse.browser.session import BrowserSession
from fastbrowse.config import Config, ObservationLimits
from fastbrowse.effects import effect
from fastbrowse.models import Attachment, BrowserConnection, Operation, StepOutcome
from fastbrowse.page import Action, BrowserError, Control, Observation
from tests.browser.conftest import RecordingArtifactSink

pytestmark = pytest.mark.asyncio


def find(observation: Observation, label: str) -> Control:
    for control in observation.controls:
        if label in control.label:
            return control
    raise AssertionError(f"no control labelled like {label!r} in {[c.label for c in observation.controls]}")


async def observe_until(page: CdpPage, label: str, timeout_seconds: float = 5.0) -> Observation:
    """Observe until a control is actually there: an attached frame session predates its document."""
    observation: Observation | None = None

    def present() -> bool:
        nonlocal observation
        return any(label in control.label for control in observation.controls) if observation else False

    async def observed() -> bool:
        nonlocal observation
        observation = await page.observe()
        return present()

    await wait_until(observed, timeout_seconds)
    assert observation is not None
    return observation


async def wait_until(predicate: Callable[[], Awaitable[bool] | bool], timeout_seconds: float = 5.0) -> None:
    deadline = asyncio.get_event_loop().time() + timeout_seconds
    while asyncio.get_event_loop().time() < deadline:
        result = predicate()
        if await result if inspect.isawaitable(result) else result:
            return
        await asyncio.sleep(0.05)
    raise AssertionError("condition was not met in time")


async def eval_value(session: BrowserSession, session_id: str, expression: str) -> Any:
    out = await session.client.send.Runtime.evaluate(
        params={"expression": expression, "returnByValue": True}, session_id=session_id
    )
    return out["result"].get("value")


@pytest.fixture
async def loaded_page(page: CdpPage, main_site: str) -> CdpPage:
    await page.navigate(f"{main_site}/")
    return page


async def test_observe_lists_controls_and_masks_password(loaded_page: CdpPage) -> None:
    obs = await loaded_page.observe()
    sign_in = find(obs, "Sign in")
    assert Operation.CLICK in sign_in.operations

    password = find(obs, "Password")
    assert password.sensitive is True
    fill = await loaded_page.act(Action(operation=Operation.FILL, target_id=password.id, text="s3cr3t"), obs)
    assert fill.outcome == StepOutcome.EXECUTED

    obs2 = await loaded_page.observe()
    password2 = find(obs2, "Password")
    assert password2.value == "•" * len("s3cr3t")


async def test_secret_typed_into_text_field_is_masked_but_preserves_value(
    loaded_page: CdpPage, browser_session: BrowserSession
) -> None:
    obs = await loaded_page.observe()
    username = find(obs, "Username")
    action = Action(
        operation=Operation.FILL,
        target_id=username.id,
        text="tok-123",
        secret=True,
        secret_origin=username.frame_origin,
    )
    assert (await loaded_page.act(action, obs)).outcome == StepOutcome.EXECUTED

    field = find(await loaded_page.observe(), "Username")
    assert field.sensitive is True and field.value == "•" * len("tok-123")
    session_id = browser_session.active_session_id
    assert await eval_value(browser_session, session_id, "document.getElementById('username').value") == "tok-123"


async def test_fill_and_click_submits_form_and_page_changed(loaded_page: CdpPage) -> None:
    obs = await loaded_page.observe()
    username = find(obs, "Username")
    result = await loaded_page.act(Action(operation=Operation.FILL, target_id=username.id, text="alice"), obs)
    assert result.outcome == StepOutcome.EXECUTED

    obs2 = await loaded_page.observe()
    submit = find(obs2, "Sign in")
    result2 = await loaded_page.act(Action(operation=Operation.CLICK, target_id=submit.id), obs2)
    assert result2.outcome == StepOutcome.EXECUTED
    assert result2.page_changed is True


async def test_covered_click_dispatches_nothing(loaded_page: CdpPage, browser_session: BrowserSession) -> None:
    obs = await loaded_page.observe()
    open_modal = find(obs, "Open modal")
    opened = await loaded_page.act(Action(operation=Operation.CLICK, target_id=open_modal.id), obs)
    assert opened.outcome == StepOutcome.EXECUTED

    obs2 = await loaded_page.observe()
    covered = find(obs2, "Covered target")
    result = await loaded_page.act(Action(operation=Operation.CLICK, target_id=covered.id), obs2)
    assert result.outcome == StepOutcome.COVERED
    assert result.page_changed is False

    clicked = await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.getElementById('covered-target').dataset.clicked || null",
    )
    assert clicked is None


async def test_dom_swap_after_observe_makes_act_stale(loaded_page: CdpPage, browser_session: BrowserSession) -> None:
    obs = await loaded_page.observe()
    target = find(obs, "Swap target")

    await eval_value(
        browser_session, browser_session.active_session_id, "document.getElementById('swap-trigger').click(); true"
    )

    result = await loaded_page.act(Action(operation=Operation.CLICK, target_id=target.id), obs)
    assert result.outcome == StepOutcome.STALE
    assert result.page_changed is False


async def test_upload_bytes_match_sha256(loaded_page: CdpPage, browser_session: BrowserSession) -> None:
    obs = await loaded_page.observe()
    file_input = next(c for c in obs.controls if c.input_type == "file")
    content = os.urandom(4096)
    attachment = Attachment(name="payload.bin", mime_type="application/octet-stream", content=content)

    result = await loaded_page.act(
        Action(operation=Operation.UPLOAD, target_id=file_input.id, files=(attachment,)), obs
    )
    assert result.outcome == StepOutcome.EXECUTED

    async def sha_ready() -> bool:
        value = await eval_value(
            browser_session, browser_session.active_session_id, "document.getElementById('upload-sha').textContent"
        )
        return bool(value)

    await wait_until(sha_ready)
    value = await eval_value(
        browser_session, browser_session.active_session_id, "document.getElementById('upload-sha').textContent"
    )
    assert value == hashlib.sha256(content).hexdigest()


async def test_download_becomes_artifact_with_checksum(
    loaded_page: CdpPage, browser_session: BrowserSession, artifact_sink: RecordingArtifactSink
) -> None:
    obs = await loaded_page.observe()
    link = find(obs, "Download attachment")
    result = await loaded_page.act(Action(operation=Operation.CLICK, target_id=link.id), obs)
    assert result.outcome == StepOutcome.EXECUTED

    async def has_artifact() -> bool:
        return len(artifact_sink.artifacts) > 0

    await wait_until(has_artifact)
    artifact = artifact_sink.artifacts[0]
    expected = hashlib.sha256(b"fastbrowse fixture attachment bytes for download checksum test").hexdigest()
    assert artifact.sha256 == expected
    assert browser_session.artifacts == (artifact,)


async def test_cross_origin_iframe_control_is_observed_and_clickable(
    loaded_page: CdpPage, browser_session: BrowserSession
) -> None:
    obs = await observe_until(loaded_page, "Frame button")
    frame_button = find(obs, "Frame button")
    assert frame_button.frame_id is not None

    result = await loaded_page.act(Action(operation=Operation.CLICK, target_id=frame_button.id), obs)
    assert result.outcome == StepOutcome.EXECUTED

    frame_session_id = browser_session.frame_sessions()[frame_button.frame_id]
    value = await eval_value(browser_session, frame_session_id, "document.getElementById('frame-button').textContent")
    assert value == "Clicked"


async def test_popup_becomes_tab_and_switch_tab_works(loaded_page: CdpPage, browser_session: BrowserSession) -> None:
    obs = await loaded_page.observe()
    open_popup = find(obs, "Open popup")
    result = await loaded_page.act(Action(operation=Operation.CLICK, target_id=open_popup.id), obs)
    assert result.outcome == StepOutcome.EXECUTED

    async def popup_loaded() -> bool:
        return any("popup.html" in t.url for t in browser_session.tabs())

    await wait_until(popup_loaded)

    obs2 = await loaded_page.observe()
    popup_tab = next(t for t in obs2.tabs if "popup.html" in t.url)
    switch = await loaded_page.act(Action(operation=Operation.SWITCH_TAB, tab_id=popup_tab.id), obs2)
    assert switch.outcome == StepOutcome.EXECUTED

    obs3 = await loaded_page.observe()
    assert "popup.html" in obs3.url


async def test_confirm_dialog_handled_via_dialog_operation(
    loaded_page: CdpPage, browser_session: BrowserSession
) -> None:
    obs = await loaded_page.observe()
    open_dialog = find(obs, "Confirm")
    result = await loaded_page.act(Action(operation=Operation.CLICK, target_id=open_dialog.id), obs)
    assert result.outcome == StepOutcome.EXECUTED

    async def dialog_pending() -> bool:
        return browser_session.pending_dialog() is not None

    await wait_until(dialog_pending)

    obs2 = await loaded_page.observe()
    assert obs2.dialog is not None
    assert obs2.dialog.kind == "confirm"

    handled = await loaded_page.act(Action(operation=Operation.DIALOG, accept_dialog=True), obs2)
    assert handled.outcome == StepOutcome.EXECUTED

    value = await eval_value(
        browser_session, browser_session.active_session_id, "document.getElementById('dialog-result').textContent"
    )
    assert value == "confirmed"


async def test_identical_labels_carry_their_card_as_context(loaded_page: CdpPage) -> None:
    obs = await loaded_page.observe()
    adds = [c for c in obs.controls if c.label == "Add to cart"]
    assert [c.context for c in adds] == ["Brass Kettle", "Copper Pan"]
    # The price sits in a wrapper closer to the button than the card is; the card is what tells them apart.
    assert find(obs, "Sign in").context is None


async def test_lazily_built_menu_is_observed_after_the_click_that_opens_it(loaded_page: CdpPage) -> None:
    obs = await loaded_page.observe()
    opener = find(obs, "Search or jump to")
    assert (await loaded_page.act(Action(operation=Operation.CLICK, target_id=opener.id), obs)).outcome == (
        StepOutcome.EXECUTED
    )
    # No re-observation loop here: the menu must be there on the next observation the agent takes.
    assert any(c.label == "Jump to a repository" for c in (await loaded_page.observe()).controls)


async def test_shadow_dom_button_is_clickable(loaded_page: CdpPage, browser_session: BrowserSession) -> None:
    obs = await loaded_page.observe()
    shadow_button = find(obs, "Shadow button")
    result = await loaded_page.act(Action(operation=Operation.CLICK, target_id=shadow_button.id), obs)
    assert result.outcome == StepOutcome.EXECUTED

    value = await eval_value(browser_session, browser_session.active_session_id, "window.__shadowClicked === true")
    assert value is True


async def test_capture_offsets_slice_exactly_to_each_block(loaded_page: CdpPage) -> None:
    capture = await loaded_page.capture()
    assert capture.blocks, "expected at least one block"
    for block in capture.blocks:
        slice_text = capture.text[block.start : block.end]
        assert slice_text, f"block {block.source_id} sliced to empty text"
    headings = [b for b in capture.blocks if b.kind.value == "heading"]
    assert any(capture.text[h.start : h.end] == "Table" for h in headings)
    tables = [b for b in capture.blocks if b.kind.value == "table"]
    assert tables
    assert "Ada" in capture.text[tables[0].start : tables[0].end]
    links = [b for b in capture.blocks if b.kind.value == "link"]
    assert any(link.href == "https://example.com/docs" for link in links)


async def test_fill_succeeds_when_the_field_is_replaced_while_typing(
    loaded_page: CdpPage, browser_session: BrowserSession
) -> None:
    """Wikipedia's search box swaps itself for a hydrated copy mid-insertion; the text still landed."""
    obs = await loaded_page.observe()
    field = find(obs, "Hydrating field")
    result = await loaded_page.act(Action(operation=Operation.FILL, target_id=field.id, text="godel"), obs)
    assert result.outcome == StepOutcome.EXECUTED

    obs2 = await loaded_page.observe()
    assert find(obs2, "Hydrating field").value == "godel"
    assert await eval_value(browser_session, browser_session.active_session_id, "!document.getElementById('hydrating')")


async def test_fill_fails_when_another_field_holds_the_text_we_could_not_insert(loaded_page: CdpPage) -> None:
    """The replacement must be the target's own: a decoy holding the same text does not vouch for it."""
    obs = await loaded_page.observe()
    field = find(obs, "Rejecting field")
    result = await loaded_page.act(Action(operation=Operation.FILL, target_id=field.id, text="godel"), obs)
    assert result.outcome == StepOutcome.FAILED


async def test_observation_limits_can_exceed_old_javascript_caps(
    loaded_page: CdpPage, browser_session: BrowserSession
) -> None:
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.body.replaceChildren(); "
        "const text = document.createElement('div'); text.textContent = 'x'.repeat(8000); "
        "document.body.append(text); "
        "for (let i = 0; i < 60; i++) { const b = document.createElement('button'); "
        "b.textContent = 'Button ' + i; b.style.cssText = 'position:absolute;left:20px;top:' + (10000+i*40) + 'px'; "
        "document.body.append(b); }",
    )
    default = await loaded_page.observe()
    assert len(default.controls) == 60 and default.omitted_controls == 0
    assert default.viewport_text == (
        "x" * 6000 + "\n[Viewport text cut: 2000 characters omitted; read the page for the rest]"
    )

    page = CdpPage(
        browser_session,
        Config(observation=ObservationLimits(max_controls=100, max_offscreen_controls=55, viewport_text_chars=7500)),
    )
    expanded = await page.observe()
    assert len(expanded.controls) == 55 and expanded.omitted_controls == 5
    assert expanded.viewport_text == (
        "x" * 7500 + "\n[Viewport text cut: 500 characters omitted; read the page for the rest]"
    )


async def test_page_exception_is_typed_and_does_not_echo_page_text(
    loaded_page: CdpPage, browser_session: BrowserSession
) -> None:
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "Object.defineProperty(document, 'title', {get() { throw new Error('echoed-secret'); }});",
    )
    with pytest.raises(BrowserError, match=r"Runtime.evaluate failed \(JavaScriptError\)") as raised:
        await loaded_page.observe()
    assert "echoed-secret" not in str(raised.value)


@pytest.mark.parametrize("kind", ["radio", "checkbox"])
async def test_transparent_native_choices_keep_todo_context_and_change_only_state(
    page: CdpPage, browser_session: BrowserSession, main_site: str, kind: str
) -> None:
    """Unassociated sibling labels left todo toggles absent from the choices offered to the policy."""
    await page.navigate(f"{main_site}/todos.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        f"document.querySelectorAll('input').forEach(e => e.type = '{kind}')",
    )
    before = await page.observe()
    choices = [c for c in before.controls if c.role == kind]
    assert [c.context for c in choices] == ["buy oat milk", "walk the dog", "ship fastbrowse 0.5"]
    assert all(c.label == "Toggle Todo" and c.checked is False for c in choices)
    target = choices[1]
    result = await page.act(Action(operation=Operation.CLICK, target_id=target.id), before)
    assert result.outcome is StepOutcome.EXECUTED and result.page_changed
    after = await page.observe()
    assert [c.checked for c in after.controls if c.role == kind] == [False, True, False]
    assert after.viewport_text == before.viewport_text
    assert effect(before, after, target).set_something


@pytest.mark.parametrize(
    "change",
    [
        "e.style.display = 'none'",
        "e.style.visibility = 'hidden'",
        "e.style.width = '0'",
        "e.parentElement.setAttribute('aria-hidden', 'true')",
        "e.parentElement.inert = true",
        "e.parentElement.style.opacity = '0'",
        "e.disabled = true",
        "e.parentElement.setAttribute('aria-disabled', 'true')",
        "e.type = 'text'",
    ],
)
async def test_transparent_choice_exception_does_not_admit_hidden_or_disabled_controls(
    page: CdpPage, browser_session: BrowserSession, main_site: str, change: str
) -> None:
    await page.navigate(f"{main_site}/todos.html")
    before = await page.observe()
    target = next(c for c in before.controls if c.context == "walk the dog")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        f"const e = document.querySelectorAll('input')[1]; {change}",
    )
    result = await page.act(Action(operation=Operation.CLICK, target_id=target.id), before)
    assert result.outcome is StepOutcome.STALE
    assert target.id not in {c.id for c in (await page.observe()).controls}
    assert not await eval_value(browser_session, browser_session.active_session_id, "e.checked")


async def test_transparent_choice_under_a_boxless_parent_is_offered(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    """A `display: contents` parent has no layout box, yet hides nothing."""
    await page.navigate(f"{main_site}/todos.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.querySelectorAll('input')[1].parentElement.style.display = 'contents'",
    )
    assert any(c.context == "walk the dog" for c in (await page.observe()).controls)


@pytest.mark.parametrize("kind", ["radio", "checkbox"])
@pytest.mark.parametrize(
    "style",
    [
        "opacity:0",
        "display:none",
        "opacity:0;pointer-events:none",
        # TodoMVC's "Mark all as complete": only its label can be clicked.
        "opacity:0;width:1px;height:1px;position:absolute;right:100%",
    ],
)
async def test_styled_choice_uses_visible_label_and_native_state(
    page: CdpPage, browser_session: BrowserSession, main_site: str, kind: str, style: str
) -> None:
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        f'document.body.innerHTML = \'<input id=choice type={kind} style="{style}">'
        "<label for=choice>Direct service</label>'; true",
    )
    obs = await page.observe()
    target = find(obs, "Direct service")
    assert target.role == kind and target.checked is False
    assert len([c for c in obs.controls if c.role == kind]) == 1
    assert (await page.act(Action(operation=Operation.CLICK, target_id=target.id), obs)).outcome is StepOutcome.EXECUTED
    assert find(await page.observe(), "Direct service").checked is True

    await eval_value(
        browser_session, browser_session.active_session_id, "document.querySelector('input').disabled = true"
    )
    assert not any(c.label == "Direct service" for c in (await page.observe()).controls)


async def test_styled_choice_rejects_reassociated_label(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        'document.body.innerHTML = \'<input id=a type=checkbox style="opacity:0">'
        '<input id=b type=checkbox style="opacity:0"><label for=a>Direct service</label>\'; true',
    )
    obs = await page.observe()
    target = find(obs, "Direct service")
    await eval_value(
        browser_session, browser_session.active_session_id, "document.querySelector('label').htmlFor = 'b'"
    )
    assert (await page.act(Action(operation=Operation.CLICK, target_id=target.id), obs)).outcome is StepOutcome.STALE
    assert not await eval_value(
        browser_session, browser_session.active_session_id, "document.getElementById('b').checked"
    )


async def test_link_label_ignores_nested_style_and_script(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        """document.body.innerHTML = '<a href="/pen"><style>.tw-reset * { box-sizing: border-box; }</style>'
          + '<script>window.x = 1;</script><span>Parker Jotter</span></a>';
        true""",
    )
    assert find(await page.observe(), "Parker Jotter").label == "Parker Jotter"


async def test_fill_activates_picker_before_typing_and_offers_suggestion(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(main_site)
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        """document.body.innerHTML = '<input aria-label="Station" role="combobox" aria-controls="choices">'
          + '<ul id="choices" role="listbox" hidden><li role="option">York Central</li></ul>';
        const input = document.querySelector('input');
        const choices = document.querySelector('ul');
        input.onclick = () => { choices.hidden = false; };
        choices.onclick = () => { input.value = 'York Central'; choices.hidden = true; };
        true""",
    )
    obs = await page.observe()
    target = find(obs, "Station")
    assert (
        await page.act(Action(operation=Operation.FILL, target_id=target.id, text="York"), obs)
    ).outcome is StepOutcome.EXECUTED
    obs = await page.observe()
    assert find(obs, "Station").value == "York"
    option = find(obs, "York Central")
    assert (await page.act(Action(operation=Operation.CLICK, target_id=option.id), obs)).outcome is StepOutcome.EXECUTED
    assert find(await page.observe(), "Station").value == "York Central"


async def test_frameset_pages_are_read_through_their_frames(page: CdpPage, main_site: str) -> None:
    await page.navigate(f"{main_site}/frames.html")
    # A frameset document has no text of its own: everything a user sees is in its frames.
    assert "Left pane" in (await page.observe()).viewport_text
    text = (await page.capture()).text
    assert "Left pane" in text
    assert "Right pane" in text


async def test_a_table_cell_drawn_with_an_icon_is_not_read_as_blank(page: CdpPage, main_site: str) -> None:
    await page.navigate(f"{main_site}/icons.html")
    text = (await page.capture()).text
    assert "| First | [flag icon] | [yes] |" in text
    # A size or weight class shares the icon font's prefix without naming the glyph.
    assert "| Second |  | [xmark icon] |" in text


async def test_a_menu_shown_on_the_next_animation_frame_opens(page: CdpPage, main_site: str) -> None:
    await page.navigate(f"{main_site}/animated.html")
    obs = await page.observe()
    opener = find(obs, "Trip type")
    assert (await page.act(Action(operation=Operation.CLICK, target_id=opener.id), obs)).outcome == (
        StepOutcome.EXECUTED
    )
    assert any(c.label == "One way" for c in (await page.observe()).controls)


async def test_content_shown_only_under_the_pointer_is_reached_by_hovering(page: CdpPage, main_site: str) -> None:
    await page.navigate(f"{main_site}/hovers.html")
    obs = await page.observe()
    hovers = [c for c in obs.controls if Operation.HOVER in c.operations]
    # The two avatars and the menu reveal hidden content; the link's hover rule only recolours it.
    assert sorted(c.label for c in hovers) == ["Products", "User Avatar", "User Avatar"]
    assert [c.context for c in hovers if c.label == "User Avatar"] == ["1 of 2", "2 of 2"]
    assert "grace" not in obs.viewport_text
    second = next(c for c in hovers if c.context == "2 of 2")
    result = await page.act(Action(operation=Operation.HOVER, target_id=second.id), obs)
    assert result.outcome == StepOutcome.EXECUTED
    assert result.page_changed
    after = await page.observe()
    assert "name: grace" in after.viewport_text
    assert "name: ada" not in after.viewport_text
    assert any(c.label == "View profile" for c in after.controls)


async def test_a_visible_option_is_clicked_without_scrolling_its_menu_shut(page: CdpPage, main_site: str) -> None:
    await page.navigate(f"{main_site}/menu.html")
    obs = await page.observe()
    trip = next(c for c in obs.controls if c.label == "Round trip" and c.role == "button")
    await page.act(Action(operation=Operation.CLICK, target_id=trip.id), obs)
    obs = await page.observe()
    option = next(c for c in obs.controls if c.label == "One way")
    result = await page.act(Action(operation=Operation.CLICK, target_id=option.id), obs)
    assert result.outcome == StepOutcome.EXECUTED
    after = await page.observe()
    assert any(c.role == "button" and c.label == "One way" for c in after.controls)


@pytest.mark.parametrize("opens", ["", "?late"])
async def test_a_fill_follows_focus_to_the_editor_its_click_opened(page: CdpPage, main_site: str, opens: str) -> None:
    await page.navigate(f"{main_site}/overlay.html{opens}")
    obs = await page.observe()
    field = next(c for c in obs.controls if c.label == "Where from?")
    result = await page.act(Action(operation=Operation.FILL, target_id=field.id, text="Lond"), obs)
    assert result.outcome == StepOutcome.EXECUTED
    after = await page.observe()
    assert [c.label for c in after.controls if c.role == "option"] == ["London", "Londonderry"]


async def test_an_anchor_with_no_destination_but_click_semantics_is_offered_as_a_button(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    # jQuery UI's datepicker appends its Next/Prev anchors to <body> once the field is focused, with a click
    # handler and no href: they carry no destination, so they must never be offered, or capped, as a pager.
    await page.navigate(f"{main_site}/icons.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        'document.body.innerHTML = \'<a class="ui-datepicker-next" data-handler="next" '
        'data-event="click" title="Next" onclick="this.dataset.clicked = String(1)">Next</a>\'',
    )
    obs = await observe_until(page, "Next")
    control = find(obs, "Next")
    assert control.role == "button"
    assert control.href is None
    result = await page.act(Action(operation=Operation.CLICK, target_id=control.id), obs)
    assert result.outcome is StepOutcome.EXECUTED
    clicked = await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.querySelector('.ui-datepicker-next').dataset.clicked",
    )
    assert clicked == "1"


async def test_what_a_form_holds_is_quotable_but_a_password_is_not(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(f"{main_site}/icons.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.body.innerHTML = '<h2>Booking</h2>"
        '<label>Start Date<input type="date" value="2026-09-28"></label>'
        '<p><label for="end">End Date</label><input id="end" type="date" value="2026-10-07"></p>'
        '<select aria-label="Size"><option>Small</option><option selected>Large</option></select>'
        '<label>Password<input type="password" value="hunter2"></label>'
        '<input type="text" value="" placeholder="Empty">\'',
    )
    text = (await page.capture()).text
    assert "Start Date: 2026-09-28" in text
    assert "End Date: 2026-10-07" in text
    assert "Size: Large" in text
    assert "hunter2" not in text
    assert "Empty" not in text


async def test_a_uniquely_named_date_field_still_gets_its_section_heading(
    page: CdpPage, browser_session: BrowserSession, main_site: str
) -> None:
    await page.navigate(f"{main_site}/icons.html")
    await eval_value(
        browser_session,
        browser_session.active_session_id,
        "document.body.innerHTML = '<section><h2>Date Picker 3</h2>"
        '<label>Start Date<input type="date" id="start"></label>'
        '<label>End Date<input type="date" id="end"></label></section>\'',
    )
    obs = await observe_until(page, "Start Date")
    start = find(obs, "Start Date")
    assert start.input_type == "date"
    assert start.context == "Date Picker 3"


async def test_a_browser_handed_over_by_cdp_url_drives_and_survives_the_run(
    chrome_connection: BrowserConnection, artifact_sink: RecordingArtifactSink, main_site: str
) -> None:
    """The whole point of `cdp_url`: drive a browser someone else started, and leave it running."""
    async with BrowserSession(
        BrowserConnection(cdp_url=chrome_connection.cdp_url, live_url=None, remote=True), artifact_sink
    ) as session:
        page = CdpPage(session, Config())
        await page.navigate(main_site)
        observation = await page.observe()
        assert observation.controls, "a browser reached over cdp_url must index like any other"
    # The session closed its own tab; the browser it was handed is still answering.
    async with BrowserSession(chrome_connection, artifact_sink) as after:
        assert await CdpPage(after, Config()).observe() is not None
