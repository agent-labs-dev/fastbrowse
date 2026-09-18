"""`CdpPage`: implements the `Page` protocol on a `BrowserSession`.

Freshness (page_key + per-element guard), hit-test-before-input (covered detection), scroll-into-view
for offscreen targets, password masking and the offscreen-nearest cap are ported from jev-ultrafast
(MIT): jev_ultrafast/browser.py + jev_ultrafast/snapshot.js. `act` dispatches at most once per call.
"""

from __future__ import annotations

import asyncio
import base64
import hashlib
import json
import time
from collections.abc import Coroutine
from contextlib import suppress
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Literal, assert_never, cast

from cdp_use.cdp.input.commands import DispatchMouseEventParameters
from cdp_use.cdp.page.commands import CaptureScreenshotParameters

from fastbrowse.browser.session import BrowserSession
from fastbrowse.config import Config
from fastbrowse.models import TARGETED, Artifact, Attachment, Operation, StepOutcome
from fastbrowse.page import (
    Action,
    ActResult,
    Block,
    BlockKind,
    BrowserError,
    Capture,
    Control,
    Dialog,
    Observation,
    Page,
)

_PAGE_JS = (Path(__file__).with_name("snapshot.js")).read_text()
_SNAPSHOT_JS = _PAGE_JS + "('snapshot')"
_CAPTURE_JS = (Path(__file__).with_name("capture.js")).read_text()
_FINGERPRINT_JS = _PAGE_JS + "('fingerprint').fingerprint"
_SELECT_TEXT_JS = (
    "if (typeof e.select === 'function') e.select(); else { const range = e.ownerDocument.createRange(); "
    "range.selectNodeContents(e); const selection = e.ownerDocument.getSelection(); "
    "selection.removeAllRanges(); selection.addRange(range); } "
)

_HIT_TEST_JS = (
    "(id => { const e = window.__fastbrowse?.nodes.get(id); "
    "if (!e?.isConnected || e.matches(':disabled') || e.closest('[aria-disabled=\"true\"],[inert]') || "
    "!e.checkVisibility({checkOpacity: true, checkVisibilityCSS: true})) return null; "
    # Scrolling only when the control is not already in full view: a page scroll closes open menus and
    # popups, so centring an option that was already visible dismissed its menu before the click landed.
    "const w = e.ownerDocument.defaultView, v = e.getBoundingClientRect(); "
    "if (v.top < 0 || v.left < 0 || v.bottom > w.innerHeight || v.right > w.innerWidth) "
    "e.scrollIntoView({block: 'center', inline: 'nearest', behavior: 'instant'}); "
    "const r = e.getBoundingClientRect(); let x = r.x + r.width / 2, y = r.y + r.height / 2; "
    "if (!r.width || !r.height) return null; "
    # Descend through open shadow roots: the document-level hit is only the outermost host.
    "let node = e, doc = e.ownerDocument; while (true) { "
    "const view = doc.defaultView; "
    "if (x < 0 || y < 0 || x >= view.innerWidth || y >= view.innerHeight) return null; "
    "let hit = doc.elementFromPoint(x, y); "
    "while (hit?.shadowRoot) { const inner = hit.shadowRoot.elementFromPoint(x, y); "
    "if (!inner || inner === hit) break; hit = inner; } "
    "if (!node.contains(hit)) return 'covered'; "
    "if (doc === document) break; "
    "node = view.frameElement; if (!node) return null; "
    "const frame = node.getBoundingClientRect(); "
    "x = frame.x + (node.clientLeft + x) * frame.width / node.offsetWidth; "
    "y = frame.y + (node.clientTop + y) * frame.height / node.offsetHeight; doc = node.ownerDocument; } "
    "return [x, y]; })"
)

# A deadline, not a wait: a field with no editor to open settles on the first frame.
_HANDOFF_SECONDS = 0.6
_HANDOFF_QUIET_SECONDS = 0.1
_PRESENTED_JS = (
    "new Promise(done => { const t = setTimeout(done, 100); "
    "requestAnimationFrame(() => requestAnimationFrame(() => { clearTimeout(t); done(); })); })"
)
"""Resolves once the page has drawn a frame, or after 100ms where a hidden page never draws one."""

# A field that opens an editor over itself when clicked (a search overlay, an airport picker) moves focus to
# that editor; typing into the original, now hidden behind it, reaches no suggestion list. A person types
# where focus went, so the fill follows focus to an editable field in the same document that covers the
# spot ours occupied, and otherwise keeps the id it was given. The editor can take focus a frame or a timer
# after the click, so the check waits for the click's DOM changes to go quiet, briefly, first.
_HANDED_FOCUS_JS = (
    "(id => new Promise(resolve => { const r = window.__fastbrowse, e = r?.nodes.get(id); "
    "if (!e?.isConnected) { resolve(id); return; } "
    "const decide = () => { const a = e.getRootNode().activeElement; "
    "if (!e.isConnected || !a || a === e || e.contains(a)) return id; "
    "const text = a.isContentEditable || a.tagName === 'TEXTAREA' || (a.tagName === 'INPUT' && "
    "!['button', 'checkbox', 'color', 'file', 'hidden', 'image', 'radio', 'range', 'reset', 'submit']"
    ".includes(a.type)); if (!text || a.disabled || a.readOnly) return id; "
    "const was = e.getBoundingClientRect(), now = a.getBoundingClientRect(); "
    "const x = was.x + was.width / 2, y = was.y + was.height / 2; "
    "if (x < now.left || x > now.right || y < now.top || y > now.bottom) return id; "
    "if (!r.ids.has(a)) r.ids.set(a, r.next++); const n = r.ids.get(a); r.nodes.set(n, a); return n; }; "
    f"const deadline = performance.now() + {_HANDOFF_SECONDS * 1000}; "
    "const poll = () => { if (performance.now() - (r.lastMutation ?? 0) >= "
    f"{_HANDOFF_QUIET_SECONDS * 1000} || performance.now() >= deadline) resolve(decide()); "
    "else setTimeout(poll, 20); }; "
    # Chrome can hold a cross-origin frame's animation frames indefinitely, and CI hung in pytest for an hour
    # awaiting one; the timer starts the poll anyway once the deadline has passed.
    "let started = false; const start = () => { if (!started) { started = true; poll(); } }; "
    f"setTimeout(start, {_HANDOFF_SECONDS * 1000}); "
    "if (e.ownerDocument.hidden) start(); else requestAnimationFrame(() => setTimeout(start, 0)); }))"
)
type _Point = tuple[float, float] | Literal["covered"] | None

_BLOCK_KIND = {
    "heading": BlockKind.HEADING,
    "paragraph": BlockKind.PARAGRAPH,
    "list_item": BlockKind.LIST_ITEM,
    "table": BlockKind.TABLE,
    "code": BlockKind.CODE,
    "link": BlockKind.LINK,
}

_SETTLE_SECONDS = 5.0
_SETTLE_POLL_SECONDS = 0.1
_SETTLE_QUIET_SECONDS = 0.2
_SCREENSHOT_WAIT_SECONDS = 1.0
# A deadline, not a wait: focus normally lands in one or two ticks. A loaded CI runner took over 0.3s.
_FOCUS_SETTLE_SECONDS = 1.0
# Long enough for a suggestion request to come back over a slow connection, and paid only by a field
# that advertises a popup at all.
_SUGGESTION_SECONDS = 1.2
# A lazily built menu or dialog arrived 0.15-0.7s after the click on the pages this was measured on.
# Paid only by a control that advertised a popup and had not opened one yet.
_POPUP_SECONDS = 1.2

_MAIN = "main"
"""Frame key used for the top frame; OOPIF frames key on their CDP target id, per the browser session."""


class _FrameObservation:
    """Raw snapshot.js output for one frame, tagged with how to reach it again."""

    __slots__ = ("frame_id", "raw", "session_id")

    def __init__(self, frame_id: str | None, session_id: str, raw: dict[str, Any]) -> None:
        self.frame_id = frame_id
        self.session_id = session_id
        self.raw = raw


class _ObservedState:
    """What `act` needs to re-find and freshness-check the exact control an `Observation` named."""

    def __init__(self, page_key: str, controls: dict[str, tuple[str, str, int, list[object] | None]]) -> None:
        self.page_key = page_key
        # control_id -> (session_id, frame_key, local_node_id, guard snapshot)
        self.controls = controls


class CdpPage(Page):
    def __init__(self, session: BrowserSession, config: Config) -> None:
        self._session = session
        self._config = config
        self._last: _ObservedState | None = None

    @property
    def artifacts(self) -> tuple[Artifact, ...]:
        return self._session.artifacts

    # -- observe / capture -------------------------------------------------------------------------

    async def observe(self) -> Observation:
        dialog = self._session.pending_dialog()
        if dialog is not None:
            # A JavaScript dialog blocks the renderer, so any page evaluate would hang until it is handled.
            return self._dialog_observation(dialog)
        frames, inaccessible = await self._snapshot_all_frames()
        main = frames.get(_MAIN)
        controls: list[Control] = []
        control_state: dict[str, tuple[str, str, int, list[object] | None]] = {}
        for frame_key, frame in frames.items():
            raw = frame.raw
            for c in raw["controls"]:
                local_id = int(c["id"])
                control_id = f"{frame_key}:{local_id}"
                fid = f"{frame_key}/{c['frame_path']}" if c.get("frame_path") else frame.frame_id
                controls.append(_control_from_raw(control_id, fid, c))
                guard = cast("list[object] | None", frame.raw["guards"].get(str(local_id)))
                control_state[control_id] = (frame.session_id, frame_key, local_id, guard)

        limits = self._config.observation
        onscreen = [c for c in controls if not c.offscreen]
        offscreen = [c for c in controls if c.offscreen][: limits.max_offscreen_controls]
        omitted = len(controls) - len(onscreen) - len(offscreen)
        kept = (onscreen + offscreen)[: limits.max_controls]
        omitted += max(0, len(onscreen) + len(offscreen) - limits.max_controls)
        control_state = {c.id: control_state[c.id] for c in kept}

        page_key = _combine_page_keys([f.raw["page_key"] for f in frames.values()])
        self._last = _ObservedState(page_key=page_key, controls=control_state)

        title = main.raw["title"] if main else ""
        url = main.raw["url"] if main else await self.origin()
        viewport_text = (main.raw["viewport_text"] if main else "")[: limits.viewport_text_chars]
        return Observation(
            url=url,
            title=title,
            page_key=page_key,
            document_key=main.raw["document_key"] if main else "",
            captured_at=datetime.now(UTC),
            controls=tuple(kept),
            omitted_controls=omitted,
            viewport_text=viewport_text,
            tabs=self._session.tabs(),
            dialog=self._session.pending_dialog(),
            inaccessible_frames=inaccessible,
        )

    def _dialog_observation(self, dialog: Dialog) -> Observation:
        active = next((t for t in self._session.tabs() if t.active), None)
        self._last = _ObservedState(page_key=f"dialog:{dialog.kind}", controls={})
        return Observation(
            url=active.url if active else "",
            title=active.title if active else "",
            page_key=self._last.page_key,
            captured_at=datetime.now(UTC),
            controls=(),
            omitted_controls=0,
            viewport_text="",
            tabs=self._session.tabs(),
            dialog=dialog,
        )

    async def _snapshot_all_frames(self) -> tuple[dict[str, _FrameObservation], int]:
        result = await self._read_frames(_SNAPSHOT_JS)
        coverage = {frame.session_id: int(frame.raw.get("inaccessible_frames", 0)) for frame in result.values()}
        return result, self._inaccessible_frames(coverage)

    async def _read_frames(self, expression: str) -> dict[str, _FrameObservation]:
        result: dict[str, _FrameObservation] = {}
        main_session = self._session.active_session_id
        target_id = self._session.active_target_id
        sources = [(_MAIN, main_session), *self._session.frame_sessions().items()]

        async def read(frame_key: str, session_id: str) -> _FrameObservation | None:
            try:
                raw = await self._evaluate(session_id, expression)
            except BrowserError:
                if frame_key == _MAIN:
                    raise
                return None
            if raw is None:
                return None
            return _FrameObservation(None if frame_key == _MAIN else frame_key, session_id, raw)

        # Preserve source order regardless of completion order so capture offsets and hashes stay stable.
        tasks = [asyncio.create_task(read(key, sid)) for key, sid in sources]
        try:
            frames = await asyncio.gather(*tasks)
        finally:
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
        for (key, _sid), frame in zip(sources, frames, strict=True):
            if frame is not None:
                result[key] = frame
        if main := result.get(_MAIN):
            self._session.set_tab_info(target_id, main.raw["url"], main.raw["title"])
        return result

    def _inaccessible_frames(self, coverage: dict[str, int]) -> int:
        # Each inaccessible child is counted by its parent. Only a successful capture of that
        # parent's OOPIF resolves it; an unrelated frame cannot hide a failed document.
        missing = int(self._session.active_session_id not in coverage)
        for session_id, count in coverage.items():
            captured_children = sum(self._session.frame_parent_session(child) == session_id for child in coverage)
            missing += max(0, count - captured_children)
        return missing

    async def capture(self) -> Capture:
        text_parts: list[str] = []
        blocks: list[Block] = []
        offset = 0
        coverage: dict[str, int] = {}
        frames = await self._read_frames(_CAPTURE_JS)
        main = frames.get(_MAIN)
        title = main.raw["title"] if main else ""
        url = main.raw["url"] if main else await self.origin()
        for frame in frames.values():
            frame_id, session_id, raw = frame.frame_id, frame.session_id, frame.raw
            coverage[session_id] = int(raw.get("inaccessible_frames", 0))
            for block in raw["blocks"]:
                text = str(block["text"])
                start = offset
                text_parts.append(text)
                offset += len(text)
                end = offset
                text_parts.append("\n\n")
                offset += 2
                blocks.append(
                    Block(
                        source_id=f"{frame_id or _MAIN}/{block.get('source_path', '')}:{len(blocks)}",
                        kind=_BLOCK_KIND[block["kind"]],
                        frame_id=f"{frame_id or _MAIN}/{block['frame_path']}" if block.get("frame_path") else frame_id,
                        start=start,
                        end=end,
                        heading_path=tuple(block.get("heading_path", ())),
                        href=block.get("href"),
                    )
                )
        text = "".join(text_parts)
        return Capture(
            url=url,
            title=title,
            captured_at=datetime.now(UTC),
            sha256=hashlib.sha256(text.encode()).hexdigest(),
            text=text,
            blocks=tuple(blocks),
            inaccessible_frames=self._inaccessible_frames(coverage),
        )

    # -- act ----------------------------------------------------------------------------------------

    async def act(self, action: Action, observation: Observation) -> ActResult:
        if self._last is None or self._last.page_key != observation.page_key:
            return ActResult(outcome=StepOutcome.STALE, page_changed=False, detail="observation is out of date")
        target = None
        if action.target_id is not None:
            target = self._last.controls.get(action.target_id)
            if target is None:
                return ActResult(outcome=StepOutcome.STALE, page_changed=False, detail="unknown control id")
        # A pending dialog already blocks the renderer's main thread; evaluating now would hang.
        before_fingerprint = ""
        point: _Point = None
        if self._session.pending_dialog() is None:
            before_fingerprint, live_guard, point = await self._before_action(
                target,
                hit_test=action.operation in TARGETED,
            )
            if target is not None and live_guard != target[3]:
                return ActResult(
                    outcome=StepOutcome.STALE, page_changed=False, detail="control changed since observation"
                )

        outcome, detail = await self._dispatch(action, target, point)
        if outcome != StepOutcome.EXECUTED:
            return ActResult(outcome=outcome, page_changed=False, detail=detail)
        changed = await self._changed_since(before_fingerprint)
        return ActResult(outcome=StepOutcome.EXECUTED, page_changed=changed, detail=detail)

    async def _dispatch(
        self, action: Action, target: tuple[str, str, int, list[object] | None] | None, point: _Point
    ) -> tuple[StepOutcome, str | None]:
        match action.operation:
            case Operation.CLICK:
                return await self._click(target, point)
            case Operation.HOVER:
                return await self._hover(target, point)
            case Operation.FILL:
                return await self._fill(
                    target, action.text or "", point, secret=action.secret, secret_origin=action.secret_origin
                )
            case Operation.SELECT:
                return await self._select(target, action.text or "", point)
            case Operation.ENTER:
                return await self._key(target, "Enter", point)
            case Operation.ESCAPE:
                return await self._key(None, "Escape", None)
            case Operation.SCROLL:
                await self._scroll()
                return StepOutcome.EXECUTED, None
            case Operation.BACK:
                return await self._back()
            case Operation.SWITCH_TAB:
                if action.tab_id is None:
                    return StepOutcome.FAILED, "switch_tab requires tab_id"
                await self._session.switch_tab(action.tab_id)
                return StepOutcome.EXECUTED, None
            case Operation.UPLOAD:
                return await self._upload(target, action.files, point)
            case Operation.DIALOG:
                if action.accept_dialog is None:
                    return StepOutcome.FAILED, "dialog requires accept_dialog"
                await self._session.handle_dialog(action.accept_dialog, action.text)
                return StepOutcome.EXECUTED, None
            case Operation.READ | Operation.DONE | Operation.ESCALATE:
                raise ValueError(f"{action.operation} does not dispatch through the browser layer")
            case _:
                assert_never(action.operation)

    async def _click(
        self, target: tuple[str, str, int, list[object] | None] | None, point: _Point
    ) -> tuple[StepOutcome, str | None]:
        if target is None:
            return StepOutcome.FAILED, "click requires a target"
        session_id, _frame, _local_id, _guard = target
        if point is None:
            return StepOutcome.STALE, "target disconnected"
        if point == "covered":
            return StepOutcome.COVERED, None
        announced = await self._announces_popup(session_id, _local_id)
        await self._click_point(session_id, point)
        if announced:
            await self._await_popup(session_id, _local_id)
        return StepOutcome.EXECUTED, None

    async def _hover(
        self, target: tuple[str, str, int, list[object] | None] | None, point: _Point
    ) -> tuple[StepOutcome, str | None]:
        if target is None:
            return StepOutcome.FAILED, "hover requires a target"
        if point is None:
            return StepOutcome.STALE, "target disconnected"
        if point == "covered":
            return StepOutcome.COVERED, None
        # The pointer stays where it lands, so what the hover reveals is still shown when the page is next read.
        await self._move(target[0], point)
        return StepOutcome.EXECUTED, None

    async def _announces_popup(self, session_id: str, local_id: int) -> bool:
        """Whether this control says, before it is clicked, that clicking opens something it does not yet show."""
        with suppress(Exception):
            return bool(
                await self._evaluate(
                    session_id,
                    f"(e => !!e && e.getAttribute('aria-expanded') !== 'true' && "
                    "(!!e.getAttribute('aria-haspopup') || e.getAttribute('aria-expanded') === 'false' "
                    "|| !!e.getAttribute('aria-controls') || !!e.getAttribute('aria-owns')))"
                    f"(window.__fastbrowse?.nodes.get({local_id}))",
                )
            )
        return False

    async def _await_popup(self, session_id: str, local_id: int) -> None:
        """Give a menu, picker or dialog the click opens time to arrive before the page is observed.

        A header that mutates the moment it is clicked settles the fingerprint and quiets the DOM before the
        dialog's own bundle has loaded, so the next observation still shows the page as it was and the agent
        acts on whatever else is on screen. The wait is bounded, resolves the instant the control reports
        itself expanded or the element it controls becomes visible, and is paid only by a control that said
        a popup was coming. Best effort by construction: a click that navigates discards the promise.
        """
        with suppress(Exception):
            await self._evaluate(
                session_id,
                "new Promise(resolve => { const e = window.__fastbrowse?.nodes.get("
                f"{local_id}); "
                "const owned = () => { const id = e.getAttribute('aria-controls') || e.getAttribute('aria-owns'); "
                "return id ? e.ownerDocument.getElementById(id) : null; }; "
                "const open = () => e.getAttribute('aria-expanded') === 'true' "
                "|| !!owned()?.checkVisibility({checkOpacity: true, checkVisibilityCSS: true}); "
                "if (!e || open()) { resolve(true); return; } "
                "const stop = ok => { observer.disconnect(); clearTimeout(timer); resolve(ok); }; "
                "const observer = new MutationObserver(() => { if (open()) stop(true); }); "
                "observer.observe(e.ownerDocument.documentElement, {childList: true, subtree: true, "
                "attributes: true}); "
                f"const timer = setTimeout(() => stop(false), {_POPUP_SECONDS * 1000}); }})",
            )

    async def _fill(
        self,
        target: tuple[str, str, int, list[object] | None] | None,
        text: str,
        point: _Point,
        *,
        secret: bool = False,
        secret_origin: str | None = None,
    ) -> tuple[StepOutcome, str | None]:
        if target is None:
            return StepOutcome.FAILED, "fill requires a target"
        session_id, _frame, local_id, _guard = target
        if point is None:
            return StepOutcome.STALE, "target disconnected"
        if point == "covered":
            return StepOutcome.COVERED, None
        if secret and not secret_origin:
            return StepOutcome.FAILED, "secret fill requires an authorized origin"
        # Ported from browser-use/jev-ultrafast (MIT), browser.py: fill clicks before typing.
        # Focus alone bypasses pointer handlers that open autocomplete and calendar pickers.
        await self._click_point(session_id, point)
        if not secret:
            local_id = int(await self._evaluate(session_id, f"({_HANDED_FOCUS_JS})({local_id})"))
        for attempt in range(2):
            if not await self._focus(session_id, local_id, prepare_fill=True, secret=secret):
                return StepOutcome.FAILED, "target did not receive keyboard focus"
            # A secret must be checked and inserted in one renderer task: CDP insertText would leave a
            # navigation/focus race between checking the origin and dispatching the secret to the page.
            script = (
                "((id, text, origin) => { const e = window.__fastbrowse?.nodes.get(id); "
                "if (!e?.isConnected || (origin !== null && e.ownerDocument.location.origin !== origin)) return false; "
                "const doc = e.ownerDocument; if (e.getRootNode().activeElement !== e) return false; "
                "if (typeof e.select === 'function') e.select(); else { const range = doc.createRange(); "
                "range.selectNodeContents(e); const selection = doc.getSelection(); selection.removeAllRanges(); "
                "selection.addRange(range); } "
                "return doc.execCommand('insertText', false, text); })"
                f"({local_id}, {json.dumps(text)}, {json.dumps(secret_origin if secret else None)})"
            )
            if secret:
                inserted = await self._evaluate(session_id, script)
                if not inserted:
                    return StepOutcome.FAILED, "secret origin or focus changed before insertion"
            else:
                await self._session.client.send.Input.insertText(params={"text": text}, session_id=session_id)
            landed = await self._evaluate(
                session_id,
                # The text can land in another field than ours: a framework may swap ours for a hydrated copy while
                # the text is inserted, or focusing ours opens an editor over it that takes focus, a moment too
                # late for the hand-off above to see. That field is accepted only when it is focused in the same
                # document and covers the point ours occupied: a field elsewhere holding the same text is not
                # evidence that ours took it.
                f"((e, text) => {{ const holds = n => !!n && (n.value ?? n.innerText) === text; "
                "if (e?.isConnected && holds(e)) return true; "
                "const was = window.__fastbrowse?.filled; if (!was) return false; "
                "const now = was.doc.activeElement; if (!now || now === e) return false; "
                "const r = now.getBoundingClientRect(); "
                "const inPlace = was.x >= r.left && was.x <= r.right && was.y >= r.top && was.y <= r.bottom; "
                "return inPlace && holds(now); })"
                f"(window.__fastbrowse?.nodes.get({local_id}), {json.dumps(text)})",
            )
            if landed:
                break
            # An editor that took focus after the hand-off looked, and so never got the text, is typed into
            # once more, as a person would on seeing the text had gone nowhere.
            handed = local_id if secret else int(await self._evaluate(session_id, f"({_HANDED_FOCUS_JS})({local_id})"))
            if attempt or handed == local_id:
                return StepOutcome.FAILED, "field did not retain the supplied text"
            local_id = handed
        await self._await_suggestions(session_id, local_id)
        return StepOutcome.EXECUTED, None

    async def _await_suggestions(self, session_id: str, local_id: int) -> None:
        """Give an autocomplete field's suggestions time to arrive before the page is read.

        Typing settles the fingerprint immediately, but the suggestions it asks for arrive over the
        network afterwards, so the snapshot caught an empty popup and Jev was asked to choose from a
        list that had not loaded. That is what stalled `wiki-godel` at an uncertain next step with the
        query typed and nothing chosen. The wait is one round trip, resolves as soon as options appear
        and returns at once for a field that has no popup to wait for.

        The wait is best effort by construction. Typing into a search box can submit it, and a
        navigation discards the renderer promise we are awaiting, so a fill that worked would fail on
        the wait that was only there to help it. A page that moved on has answered the question.
        """
        with suppress(Exception):
            await self._evaluate(
                session_id,
                "new Promise(resolve => { const e = window.__fastbrowse?.nodes.get("
                f"{local_id}); "
                "const owned = () => { const id = e.getAttribute('aria-controls') || e.getAttribute('aria-owns'); "
                "return id ? e.ownerDocument.getElementById(id) : null; }; "
                "const expects = !!e && (e.getAttribute('role') === 'combobox' || e.type === 'search' "
                "|| !!e.getAttribute('aria-autocomplete') || !!owned()); "
                "if (!expects) { resolve(false); return; } "
                # A popup that is open is not a popup that is populated, and an empty one is exactly what we
                # are waiting to stop seeing, so expansion on its own does not end the wait.
                "const listed = () => !!owned()?.querySelector('[role=\"option\"], li, td') "
                '|| !!e.ownerDocument.querySelector(\'[role="listbox"] [role="option"]\'); '
                "if (listed()) { resolve(true); return; } "
                "const stop = ok => { observer.disconnect(); clearTimeout(timer); resolve(ok); }; "
                "const observer = new MutationObserver(() => { if (listed()) stop(true); }); "
                "observer.observe(e.ownerDocument.body, {childList: true, subtree: true, attributes: true, "
                "attributeFilter: ['aria-expanded']}); "
                f"const timer = setTimeout(() => stop(false), {_SUGGESTION_SECONDS * 1000}); }})",
            )

    async def _select(
        self, target: tuple[str, str, int, list[object] | None] | None, option: str, point: _Point
    ) -> tuple[StepOutcome, str | None]:
        if target is None:
            return StepOutcome.FAILED, "select requires a target"
        session_id, _frame, local_id, _guard = target
        if point is None:
            return StepOutcome.STALE, "target disconnected"
        if point == "covered":
            return StepOutcome.COVERED, None
        script = (
            "((id, label) => { const e = window.__fastbrowse?.nodes.get(id); "
            "if (!e?.isConnected || e.tagName !== 'SELECT') return null; "
            "const opt = [...e.options].find(o => o.label === label && !o.disabled); "
            "if (!opt) return false; e.value = opt.value; "
            "e.dispatchEvent(new Event('input', {bubbles: true})); "
            "e.dispatchEvent(new Event('change', {bubbles: true})); return true; })"
            f"({local_id}, {json.dumps(option)})"
        )
        result = await self._evaluate(session_id, script)
        if result is None:
            return StepOutcome.STALE, "select target disconnected"
        if result is False:
            return StepOutcome.FAILED, f"no option labelled {option!r}"
        return StepOutcome.EXECUTED, None

    async def _key(
        self, target: tuple[str, str, int, list[object] | None] | None, key: str, point: _Point
    ) -> tuple[StepOutcome, str | None]:
        session_id = target[0] if target is not None else self._session.active_session_id
        if target is not None:
            _session_id, _frame, local_id, _guard = target
            if point is None:
                return StepOutcome.STALE, "target disconnected"
            if point == "covered":
                return StepOutcome.COVERED, None
            if not await self._focus(session_id, local_id):
                return StepOutcome.FAILED, "target did not receive keyboard focus"
        code, virtual_key = {"Enter": ("\r", 13), "Escape": ("", 27)}[key]
        await self._input(
            self._session.client.send.Input.dispatchKeyEvent(
                params={"type": "keyDown", "key": key, "code": key, "text": code, "windowsVirtualKeyCode": virtual_key},
                session_id=session_id,
            )
        )
        await self._input(
            self._session.client.send.Input.dispatchKeyEvent(
                params={"type": "keyUp", "key": key, "code": key, "windowsVirtualKeyCode": virtual_key},
                session_id=session_id,
            )
        )
        return StepOutcome.EXECUTED, None

    async def _scroll(self) -> None:
        session_id = self._session.active_session_id
        await self._session.client.send.Input.dispatchMouseEvent(
            params={"type": "mouseWheel", "x": 550, "y": 650, "deltaX": 0, "deltaY": 560},
            session_id=session_id,
        )

    async def _back(self) -> tuple[StepOutcome, str | None]:
        session_id = self._session.active_session_id
        history = await self._session.client.send.Page.getNavigationHistory(params=None, session_id=session_id)
        index = history["currentIndex"]
        if index <= 0:
            return StepOutcome.FAILED, "no earlier history entry"
        entry_id = history["entries"][index - 1]["id"]
        await self._session.client.send.Page.navigateToHistoryEntry(params={"entryId": entry_id}, session_id=session_id)
        return StepOutcome.EXECUTED, None

    async def _upload(
        self, target: tuple[str, str, int, list[object] | None] | None, files: tuple[Attachment, ...], point: _Point
    ) -> tuple[StepOutcome, str | None]:
        if target is None:
            return StepOutcome.FAILED, "upload requires a target"
        if not files:
            return StepOutcome.FAILED, "no files supplied"
        total = sum(len(f.content) for f in files)
        if total > self._config.max_upload_bytes:
            return StepOutcome.FAILED, f"{total} bytes exceeds max_upload_bytes ({self._config.max_upload_bytes})"
        session_id, _frame, local_id, _guard = target
        if point is None:
            return StepOutcome.STALE, "target disconnected"
        if point == "covered":
            return StepOutcome.COVERED, None
        payload = json.dumps(
            [{"name": f.name, "type": f.mime_type, "data": base64.b64encode(f.content).decode()} for f in files]
        )
        script = (
            "((id, files) => { const e = window.__fastbrowse?.nodes.get(id); "
            "if (!e?.isConnected) return null; "
            "const dt = new DataTransfer(); "
            "for (const f of files) { const bin = atob(f.data); "
            "const bytes = Uint8Array.from(bin, c => c.charCodeAt(0)); "
            "dt.items.add(new File([bytes], f.name, {type: f.type})); } "
            "e.files = dt.files; "
            "e.dispatchEvent(new Event('input', {bubbles: true})); "
            "e.dispatchEvent(new Event('change', {bubbles: true})); return true; })"
            f"({local_id}, {payload})"
        )
        result = await self._evaluate(session_id, script)
        if result is None:
            return StepOutcome.STALE, "upload target disconnected"
        return StepOutcome.EXECUTED, None

    async def _move(self, session_id: str, point: tuple[float, float]) -> None:
        """Move the pointer to a point measured in the DOM, once the browser routes input by the same layout.

        Chrome sends input to a frame by the last frame the page drew, not by the DOM. A fill that scrolled a
        cross-origin frame into view left the drawn layout a frame behind, so the click meant for the button
        below it went to the old position and the popup it opens never appeared.
        """
        with suppress(Exception):
            await asyncio.wait_for(self._evaluate(self._session.active_session_id, _PRESENTED_JS), 0.5)
        params: DispatchMouseEventParameters = {"type": "mouseMoved", "x": point[0], "y": point[1]}
        await self._input(self._session.client.send.Input.dispatchMouseEvent(params=params, session_id=session_id))

    async def _click_point(self, session_id: str, point: tuple[float, float]) -> None:
        # Arrive before pressing, as a pointer does: menus built on pointer events ignore a press with no hover.
        await self._move(session_id, point)
        x, y = point
        for kind in ("mousePressed", "mouseReleased"):
            params: DispatchMouseEventParameters = {"type": kind, "x": x, "y": y, "button": "left", "clickCount": 1}
            await self._input(self._session.client.send.Input.dispatchMouseEvent(params=params, session_id=session_id))

    async def _input(self, send: Coroutine[None, None, object]) -> None:
        """Dispatch an input event without waiting on a handler that a JavaScript dialog is blocking.

        CDP answers an input event only after the page's handlers return, and `confirm()` inside a handler
        does not return until the dialog is handled; the event has been delivered either way.
        """
        task = asyncio.ensure_future(send)
        dialog = asyncio.create_task(self._session.wait_for_dialog())
        try:
            await asyncio.wait({task, dialog}, return_when=asyncio.FIRST_COMPLETED)
            if task.done():
                task.result()
        finally:
            # Cancelling the response wait does not undo an event already delivered to a dialog handler.
            task.cancel()
            dialog.cancel()
            await asyncio.gather(task, dialog, return_exceptions=True)

    async def screenshot(self) -> bytes:
        """Capture the active tab, activating it only if a background tab produces no frame to capture.

        An idle background tab composites nothing new, so a capture can wait many seconds; focus emulation and
        compositor-level nudges proved unreliable, while activating always yields a frame at once. Screenshots
        are rare (recovery and uncertain completion), so focus moves only when it has to.
        """
        client, session_id = self._session.client, self._session.active_session_id
        params: CaptureScreenshotParameters = {"format": "jpeg", "quality": 70}
        capture = asyncio.ensure_future(client.send.Page.captureScreenshot(params=params, session_id=session_id))
        try:
            done, _ = await asyncio.wait({capture}, timeout=_SCREENSHOT_WAIT_SECONDS)
            if not done:
                await client.send.Target.activateTarget(params={"targetId": self._session.active_target_id})
            return base64.b64decode((await capture)["data"])
        finally:
            capture.cancel()
            await asyncio.gather(capture, return_exceptions=True)

    async def origin(self) -> str:
        raw = await self._evaluate(self._session.active_session_id, "location.origin")
        return str(raw) if raw is not None else ""

    async def navigate(self, url: str, load_timeout_seconds: float = 15.0) -> None:
        """Setup helper (tests, initial task URL): navigate the active tab and wait until its document is usable.

        Waiting for `complete` also waits on every image and tracker, which behind a proxy can outlast the page
        becoming interactive; observation settles the rest.
        """
        session_id = self._session.active_session_id
        result = await self._session.client.send.Page.navigate(params={"url": url}, session_id=session_id)
        if result.get("errorText"):
            raise BrowserError("Page.navigate failed (NavigationError)")
        deadline = asyncio.get_event_loop().time() + load_timeout_seconds
        while asyncio.get_event_loop().time() < deadline:
            state = await self._evaluate(session_id, "document.readyState")
            if state in {"interactive", "complete"}:
                return
            await asyncio.sleep(0.05)
        raise BrowserError("Page.navigate failed (TimeoutError)")

    # -- shared helpers -------------------------------------------------------------------------------

    async def _focus(self, session_id: str, local_id: int, *, prepare_fill: bool = False, secret: bool = False) -> bool:
        # Background local tabs can report activeElement while routing keyboard input elsewhere.
        await self._session.client.send.Target.activateTarget(params={"targetId": self._session.active_target_id})
        mask = (
            "e.dataset.fastbrowseSecret = '1'; e.style.setProperty('-webkit-text-security', 'disc', 'important'); "
            if secret
            else ""
        )
        # Hydration can replace the field during insertion. Keep its position to verify that the
        # replacement occupies the same place, rather than accepting a different field with the same text.
        prepare = (
            "const r = e.getBoundingClientRect(); window.__fastbrowse.filled = "
            "{doc: e.ownerDocument, x: r.x + r.width / 2, y: r.y + r.height / 2}; "
            + (_SELECT_TEXT_JS if not secret else "")
            if prepare_fill
            else ""
        )
        return bool(
            await self._evaluate(
                session_id,
                "(id => { const e = window.__fastbrowse?.nodes.get(id); if (!e?.isConnected) return false; "
                + mask
                + "e.ownerDocument.defaultView.focus(); e.focus({preventScroll: true}); "
                "if (!e.isConnected || e.getRootNode().activeElement !== e) return false; "
                # activeElement is set by focus() before it returns, but hasFocus() is answered by the
                # browser's focus controller, which does not run inside the task that called focus().
                # For a field inside an iframe it therefore reads false for a tick or two, so judging it
                # here in the same task rejects a field that is in fact focused. Poll instead of guessing.
                "return new Promise(resolve => { "
                f"const deadline = Date.now() + {_FOCUS_SETTLE_SECONDS * 1000}; "
                # Every condition is rechecked on the tick that succeeds. Waiting for the focus signal
                # means focus can move while we wait, and reporting success on a stale activeElement
                # would authorize the caller to send keystrokes to whatever holds focus now.
                "const check = () => { if (!e.isConnected || e.getRootNode().activeElement !== e) "
                "{ resolve(false); return; } "
                "if (e.ownerDocument.hasFocus()) { " + prepare + "resolve(true); return; } "
                "if (Date.now() > deadline) { resolve(false); return; } "
                f"setTimeout(check, 10); }}; check(); }}); }})({local_id})",
            )
        )

    async def _fingerprint(self) -> str:
        result = await self._evaluate(self._session.active_session_id, _FINGERPRINT_JS)
        return str(result)

    async def _before_action(
        self, target: tuple[str, str, int, list[object] | None] | None, *, hit_test: bool
    ) -> tuple[str, list[object] | None, _Point]:
        if target is None:
            return await self._fingerprint(), None, None
        session_id, _frame, local_id, guard = target
        same_session = session_id == self._session.active_session_id

        async def target_state() -> tuple[str, list[object] | None, _Point]:
            # Guard validation and hit testing share a renderer task, so no page script can swap the
            # verified control between them. Stale controls must never be scrolled into view.
            result = await self._evaluate(
                session_id,
                "(() => { const r = window.__fastbrowse; "
                f"const fingerprint = {_FINGERPRINT_JS if same_session else "''"}; "
                f"const guard = r?.guard ? r.guard(r.nodes.get({local_id})) : null; "
                f"const point = {json.dumps(hit_test)} && JSON.stringify(guard) === "
                f"JSON.stringify({json.dumps(guard)}) ? ({_HIT_TEST_JS})({local_id}) : null; "
                "return [fingerprint, guard, point]; })()",
            )
            point = result[2]
            if point is not None and point != "covered":
                point = (float(point[0]), float(point[1]))
            return str(result[0]), cast("list[object] | None", result[1]), point

        if same_session:
            return await target_state()
        fingerprint_task = asyncio.create_task(self._fingerprint())
        target_task = asyncio.create_task(target_state())
        try:
            fingerprint, (_, live_guard, point) = await asyncio.gather(fingerprint_task, target_task)
            return fingerprint, live_guard, point
        finally:
            fingerprint_task.cancel()
            target_task.cancel()
            await asyncio.gather(fingerprint_task, target_task, return_exceptions=True)

    async def _changed_since(self, before: str) -> bool:
        """Wait for the page to settle after an action, then report whether it changed.

        A click that navigates returns before the navigation starts, so an immediate fingerprint would describe
        the old page. Settled means an interactive document with a quiet DOM. A blocking JavaScript dialog
        freezes the renderer, and an evaluate that fails mid-navigation is itself evidence of change.
        """
        deadline = time.monotonic() + _SETTLE_SECONDS
        dialog = asyncio.create_task(self._session.wait_for_dialog())
        try:
            await asyncio.sleep(_SETTLE_QUIET_SECONDS)
            while (remaining := deadline - time.monotonic()) > 0:
                if self._session.pending_dialog() is not None:
                    return True
                settled = asyncio.create_task(self._settled_fingerprint(remaining))
                try:
                    done, _ = await asyncio.wait(
                        {settled, dialog}, timeout=remaining, return_when=asyncio.FIRST_COMPLETED
                    )
                    if dialog in done or settled not in done:
                        return True
                    stable, current = settled.result()
                    if stable:
                        return current != before
                except BrowserError:
                    # Navigation destroys the promise with its execution context. Only this read is retried.
                    pass
                finally:
                    settled.cancel()
                    await asyncio.gather(settled, return_exceptions=True)
                await asyncio.sleep(_SETTLE_POLL_SECONDS)
            return True
        finally:
            dialog.cancel()
            await asyncio.gather(dialog, return_exceptions=True)

    async def _settled_fingerprint(self, timeout_seconds: float) -> tuple[bool, str | None]:
        # Poll inside the renderer: a cloud round trip can exceed the entire old per-poll timeout.
        # Hidden tabs throttle timers, so return a single sample for the caller to poll in that case.
        result = await self._evaluate(
            self._session.active_session_id,
            f"new Promise(resolve => {{ const sample = {_PAGE_JS}; "
            f"const deadline = performance.now() + {timeout_seconds * 1000}; "
            "const poll = () => { "
            "const state = sample('fingerprint'); "
            f"const stable = state.ready && state.quietFor >= {_SETTLE_QUIET_SECONDS * 1000}; "
            "if (stable || state.hidden || performance.now() >= deadline) { "
            "resolve([stable, state.ready ? state.fingerprint : null]); return; } "
            f"setTimeout(poll, state.ready ? Math.min({_SETTLE_POLL_SECONDS * 1000}, "
            f"Math.max(1, {_SETTLE_QUIET_SECONDS * 1000} - state.quietFor)) "
            f": {_SETTLE_POLL_SECONDS * 1000}); }}; "
            # Wait for one rendered frame first: a menu shown in an animation frame callback mutates only when that
            # frame runs, and a late frame would otherwise let 200ms of quiet pass before the menu exists.
            "if (document.hidden) poll(); else requestAnimationFrame(() => setTimeout(poll, 0)); })",
        )
        return bool(result[0]), cast("str | None", result[1])

    async def _evaluate(self, session_id: str, expression: str) -> Any:
        out = await self._session.client.send.Runtime.evaluate(
            params={"expression": expression, "returnByValue": True, "awaitPromise": True}, session_id=session_id
        )
        if "exceptionDetails" in out:
            raise BrowserError("Runtime.evaluate failed (JavaScriptError)")
        return out["result"].get("value")


def _control_from_raw(control_id: str, frame_id: str | None, c: dict[str, Any]) -> Control:
    return Control(
        id=control_id,
        frame_id=frame_id,
        frame_origin=c.get("frame_origin"),
        role=c["role"],
        label=c["label"],
        context=c.get("context"),
        operations=frozenset(Operation(op) for op in c["operations"]),
        value=c.get("value"),
        href=c.get("href"),
        options=tuple(c.get("options", ())),
        input_type=c.get("input_type"),
        submit_semantics=c.get("submit_semantics"),
        checked=c.get("checked"),
        selected=c.get("selected"),
        expanded=c.get("expanded"),
        sensitive=bool(c.get("sensitive", False)),
        offscreen=bool(c.get("offscreen", False)),
    )


def _combine_page_keys(keys: list[str]) -> str:
    return hashlib.sha256("|".join(sorted(keys)).encode()).hexdigest()
