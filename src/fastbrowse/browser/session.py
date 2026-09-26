"""One owned tab on a `BrowserConnection`: attach, OOPIF sessions, popup ownership, dialogs, downloads.

Session lifecycle, the "foreground the owned tab" behaviour, and the freshness/guard
primitives `page.py` builds on were proven live in jev-ultrafast (MIT): jev_ultrafast/browser.py.
"""

from __future__ import annotations

import asyncio
import base64
import contextlib
import logging
from collections.abc import Coroutine
from dataclasses import dataclass
from pathlib import Path
from types import TracebackType
from typing import Any, Self, cast

from cdp_use.cdp.fetch.events import RequestPausedEvent
from cdp_use.cdp.fetch.types import RequestPattern
from cdp_use.cdp.page.events import JavascriptDialogOpeningEvent, ScreencastFrameEvent
from cdp_use.cdp.target.events import (
    AttachedToTargetEvent,
    DetachedFromTargetEvent,
    TargetCreatedEvent,
    TargetDestroyedEvent,
    TargetInfoChangedEvent,
)
from cdp_use.client import CDPClient
from websockets.exceptions import ConnectionClosed, InvalidMessage, InvalidStatus

from fastbrowse.clients.validation import RETRYABLE_STATUS
from fastbrowse.models import Artifact, ArtifactKind, ArtifactSink, FrameHandler, Unavailable
from fastbrowse.models import BrowserConnection as BrowserConnectionModel
from fastbrowse.page import BrowserError, Dialog, Tab

logger = logging.getLogger(__name__)

_SCREENCAST_COMMAND_SECONDS = 0.5
CDP_REPLY_SECONDS = 60.0
"""How long a CDP command waits before the browser is asked whether it is still there. A cloud browser's proxy can
keep the socket open, answering pings, after losing the browser behind it, and a run with no wall-clock limit would
otherwise wait on that reply forever. A slow reply is not a lost one: `Page.navigate` waits for a slow server's
headers, so the command keeps waiting for as long as the browser answers."""
CDP_ALIVE_SECONDS = 10.0
"""`Browser.getVersion` is answered at once by any browser that is there."""


class BrowserUnresponsive(BrowserError, Unavailable):
    """The browser stopped answering: an outage the same run may not meet again, not a failed task."""


def _unreachable(cause: Exception) -> bool:
    """A connection the browser's endpoint dropped or refused for now, as a cloud browser still starting does."""
    if isinstance(cause, InvalidStatus):
        return cause.response.status_code in RETRYABLE_STATUS
    # A malformed reply, such as a proxy's bare 401, and a TLS failure both need the configuration changed.
    if isinstance(cause, InvalidMessage):
        return isinstance(cause.__cause__, (EOFError, ConnectionError))
    return isinstance(cause, (ConnectionClosed, ConnectionError, TimeoutError))


# Response-stage interception is enough: fastbrowse only needs the bytes of a save-as download, never to
# rewrite a request. Chrome also classifies download-attribute anchors as Document; intercepting Other
# needlessly pauses favicons. Browser download events only expose paths on the browser's filesystem,
# so they cannot deliver remote file bytes and changing context-wide behavior would affect unowned tabs.
DOWNLOAD_PATTERNS: tuple[RequestPattern, ...] = (
    {"urlPattern": "*", "resourceType": "Document", "requestStage": "Response"},
)
_ENABLE_DOMAINS = ("Page", "Runtime", "DOM")
_TRACK_DOCUMENT_JS = Path(__file__).with_name("snapshot.js").read_text(encoding="utf-8") + "('fingerprint')"
_REFUSE_COOKIES_JS = (Path(__file__).parent / "autoconsent" / "autoconsent.standalone.js").read_text(encoding="utf-8")
"""DuckDuckGo's autoconsent (MPL-2.0, unmodified): refuses consent banners on known platforms and hides them
before they paint, so neither the agent's steps nor a recording are spent on one."""


class _HideEndpoint(logging.Filter):
    """cdp-use logs the full CDP URL at INFO, and a cloud browser's URL is the credential to drive it."""

    def filter(self, record: logging.LogRecord) -> bool:
        if isinstance(record.msg, str) and record.msg.startswith("Connecting to "):
            record.msg, record.args = "Connecting to the browser's CDP endpoint", None
        return True


logging.getLogger("cdp_use.client").addFilter(_HideEndpoint())


def _browser_error(method: str, cause: Exception) -> BrowserError:
    detail = type(cause).__name__
    if cause.args and isinstance(cause.args[0], dict):
        code = cast("dict[str, object]", cause.args[0]).get("code")
        if type(code) is int:
            detail = f"CDP {code}"
    return BrowserError(f"{method} failed ({detail})")


@dataclass(frozen=True, slots=True)
class _Frame:
    data: str
    ack_id: int
    session_id: str | None


class _BrowserClient(CDPClient):
    async def start(self) -> None:
        try:
            await super().start()
        except Exception as exc:
            if _unreachable(exc):
                raise BrowserUnresponsive(f"CDP.start failed ({type(exc).__name__})") from exc
            raise _browser_error("CDP.start", exc) from exc

    async def stop(self) -> None:
        try:
            await super().stop()
        except Exception as exc:
            raise _browser_error("CDP.stop", exc) from exc

    async def send_raw(self, method: str, params: Any = None, session_id: str | None = None) -> dict[str, Any]:
        reply = asyncio.ensure_future(super().send_raw(method, params, session_id))
        try:
            while not (await asyncio.wait({reply}, timeout=CDP_REPLY_SECONDS))[0]:
                # A reply that landed while the probe waited still counts, however the probe ended.
                if not await self._alive() and not reply.done():
                    raise BrowserUnresponsive(f"{method} got no reply, and the browser stopped answering")
            return reply.result()
        except BrowserUnresponsive:
            raise
        except Exception as exc:
            # CDP error messages can contain evaluated source or page text, including secrets.
            raise _browser_error(method, exc) from exc
        finally:
            # Join the abandoned reply, as a cancelled caller's cleanup must not run ahead of it.
            reply.cancel()
            await asyncio.gather(reply, return_exceptions=True)

    async def _alive(self) -> bool:
        try:
            async with asyncio.timeout(CDP_ALIVE_SECONDS):
                await super().send_raw("Browser.getVersion")
        except Exception:  # no answer, or a closed socket: either way nothing is there to reply
            return False
        return True


@dataclass
class _TabState:
    target_id: str
    session_id: str
    url: str = "about:blank"
    title: str = ""
    opener_id: str | None = None


class BrowserSession:
    """Async context manager over one owned tab (plus any popups it opens).

    Closes only the targets it owns; the caller's browser and any of its other tabs are never touched.
    Holds no module-level state, so many sessions can run concurrently in one process.
    """

    def __init__(
        self,
        connection: BrowserConnectionModel,
        artifact_sink: ArtifactSink,
        max_download_bytes: int = 200 * 1024 * 1024,
        *,
        refuse_cookie_banners: bool = True,
        on_frame: FrameHandler | None = None,
    ) -> None:
        self._connection = connection
        self._artifact_sink = artifact_sink
        self._max_download_bytes = max_download_bytes
        self._new_document_scripts = (
            (_TRACK_DOCUMENT_JS, _REFUSE_COOKIES_JS) if refuse_cookie_banners else (_TRACK_DOCUMENT_JS,)
        )
        self._client: CDPClient | None = None
        self._tabs: dict[str, _TabState] = {}
        self._owned: set[str] = set()
        self._popups: dict[str, tuple[str, asyncio.Future[bool]]] = {}
        """Popup target id -> (opener target id, whether it was adopted), in the order they opened."""
        self._active_target_id = ""
        self._on_frame = on_frame
        # Set while the page may show a secret: frames are still acked, so the cast keeps pace, but not delivered.
        self.frames_withheld = False
        self._screencast_session_id: str | None = None
        self._screencast_lock = asyncio.Lock()
        self._pending_frame: _Frame | None = None
        self._frame_scheduled = False
        self._frame_sessions: dict[str, str] = {}
        """OOPIF frame_id -> session_id, keyed by target id per the plan's `frameId/targetId` guidance."""
        self._frame_parents: dict[str, str] = {}
        self._artifacts: list[Artifact] = []
        self._dialogs: dict[str, Dialog] = {}
        """Pending JS dialog per tab session id; cleared once handled."""
        self._dialog_opened = asyncio.Event()
        self._background: set[asyncio.Task[None]] = set()
        self._closing = False

    @property
    def client(self) -> CDPClient:
        if self._client is None:
            raise BrowserError("CDP.client failed (SessionClosed)")
        return self._client

    @property
    def active_target_id(self) -> str:
        return self._active_target_id

    @property
    def active_session_id(self) -> str:
        return self._tabs[self._active_target_id].session_id

    def frame_sessions(self) -> dict[str, str]:
        """Only frame sessions descended from the active tab may contribute observations."""

        def belongs(session_id: str) -> bool:
            seen: set[str] = set()
            while session_id in self._frame_parents and session_id not in seen:
                seen.add(session_id)
                session_id = self._frame_parents[session_id]
            return session_id == self.active_session_id

        return {fid: sid for fid, sid in self._frame_sessions.items() if belongs(sid)}

    @property
    def artifacts(self) -> tuple[Artifact, ...]:
        return tuple(self._artifacts)

    def frame_parent_session(self, session_id: str) -> str | None:
        return self._frame_parents.get(session_id)

    def tabs(self) -> tuple[Tab, ...]:
        return tuple(
            Tab(
                id=t.target_id,
                url=t.url,
                title=t.title,
                active=t.target_id == self._active_target_id,
                opener_id=t.opener_id,
            )
            for t in self._tabs.values()
        )

    def set_tab_info(self, target_id: str, url: str, title: str) -> None:
        if target_id in self._tabs:
            self._tabs[target_id].url = url
            self._tabs[target_id].title = title

    def pending_dialog(self) -> Dialog | None:
        return next(
            (
                self._dialogs[sid]
                for sid in (self.active_session_id, *self.frame_sessions().values())
                if sid in self._dialogs
            ),
            None,
        )

    async def wait_for_dialog(self) -> None:
        while True:
            self._dialog_opened.clear()
            if self.pending_dialog() is not None:
                return
            await self._dialog_opened.wait()

    async def __aenter__(self) -> Self:
        self._closing = False
        self._client = _BrowserClient(self._connection.cdp_url)
        try:
            await self._client.start()
            self._register_events()
            # Discovery has to be on only before the tab can open a popup, which it cannot do while it is still
            # being created, so the two are sent at once rather than paying a cloud round trip for each.
            await _together(
                self.client.send.Target.setDiscoverTargets(params={"discover": True}),
                self._open_owned_tab("about:blank"),
            )
        except BaseException:
            with contextlib.suppress(Exception):
                await self._close()
            raise
        return self

    async def __aexit__(
        self, exc_type: type[BaseException] | None, exc: BaseException | None, tb: TracebackType | None
    ) -> None:
        try:
            await self._close()
        except Exception:
            if exc is None:
                raise

    async def _close(self) -> None:
        self._closing = True
        background = list(self._background)
        for task in background:
            task.cancel()
        # Download finalizers still need the socket to release intercepted requests.
        await asyncio.gather(*background, return_exceptions=True)
        if self._screencast_session_id is not None:
            await self._screencast_command("Page.stopScreencast", None, self._screencast_session_id)
            self._screencast_session_id = None
        for target_id in list(self._owned):
            with contextlib.suppress(Exception):  # best-effort teardown; the browser may already be gone
                await self.client.send.Target.closeTarget(params={"targetId": target_id})
        if self._client is not None:
            try:
                await self._client.stop()
            finally:
                self._client = None
                self._owned.clear()
                self._tabs.clear()

    async def switch_tab(self, target_id: str) -> None:
        if target_id not in self._tabs:
            raise ValueError(f"Unknown tab {target_id}")
        self._set_active_target(target_id)
        await self.client.send.Target.activateTarget(params={"targetId": target_id})

    async def _open_owned_tab(self, url: str) -> str:
        # Every browser a session drives was started for it (a cloud browser, or a Chrome launched with its own
        # profile), so there is no user tab to protect. A background target does not render: animation frames
        # never fire, so menus that animate open never become visible, screenshots hang and clicks read as covered.
        created = await self.client.send.Target.createTarget(params={"url": url})
        target_id = created["targetId"]
        self._owned.add(target_id)
        attach = await self.client.send.Target.attachToTarget(params={"targetId": target_id, "flatten": True})
        session_id = attach["sessionId"]
        # Foregrounding the tab and enabling its session are independent, and both finish before the tab is used.
        await _together(
            self.client.send.Target.activateTarget(params={"targetId": target_id}), self._prepare_session(session_id)
        )
        self._tabs[target_id] = _TabState(target_id=target_id, session_id=session_id, url=url)
        self._set_active_target(target_id)
        return target_id

    def _set_active_target(self, target_id: str) -> None:
        self._active_target_id = target_id
        if self._pending_frame is not None:
            self._ack(self._pending_frame)
            self._pending_frame = None
        if self._on_frame is not None:
            self._spawn(self._update_screencast())

    async def _prepare_session(self, session_id: str) -> None:
        # These domains are independent, but all must be ready before the session can be used.
        await _together(
            *(self.client.send_raw(f"{domain}.enable", session_id=session_id) for domain in _ENABLE_DOMAINS),
            self.client.send_raw(
                "Target.setAutoAttach",
                {"autoAttach": True, "waitForDebuggerOnStart": False, "flatten": True},
                session_id=session_id,
            ),
            self.client.send.Fetch.enable(params={"patterns": list(DOWNLOAD_PATTERNS)}, session_id=session_id),
            # Track parsing and hydration before the first post-navigation read, so an already
            # quiet document does not pay another full window just to install its observer.
            *(
                self.client.send.Page.addScriptToEvaluateOnNewDocument(params={"source": source}, session_id=session_id)
                for source in self._new_document_scripts
            ),
        )

    def _register_events(self) -> None:
        client = self.client
        client.register.Target.attachedToTarget(self._on_attached)
        client.register.Target.detachedFromTarget(self._on_detached)
        client.register.Target.targetCreated(self._on_target_created)
        client.register.Target.targetInfoChanged(self._on_target_info_changed)
        client.register.Target.targetDestroyed(self._on_target_destroyed)
        client.register.Page.javascriptDialogOpening(self._on_dialog)
        if self._on_frame is not None:
            client.register.Page.screencastFrame(self._on_screencast_frame)
        client.register.Fetch.requestPaused(self._on_request_paused)

    def _spawn(self, coro: Coroutine[None, None, None]) -> None:
        if self._closing:
            coro.close()
            return
        task = asyncio.ensure_future(coro)
        self._background.add(task)
        task.add_done_callback(self._background_finished)

    def _background_finished(self, task: asyncio.Task[None]) -> None:
        self._background.discard(task)
        if not task.cancelled():
            task.exception()

    # -- Target/frame tracking -------------------------------------------------------------------------

    def _on_attached(self, event: AttachedToTargetEvent, session_id: str | None) -> None:
        info = event["targetInfo"]
        if info["type"] == "iframe" and session_id is not None:
            self._frame_sessions[info["targetId"]] = event["sessionId"]
            self._frame_parents[event["sessionId"]] = session_id
            self._spawn(self._enable_frame_domains(event["sessionId"]))

    async def _enable_frame_domains(self, session_id: str) -> None:
        await self._prepare_session(session_id)

    def _on_detached(self, event: DetachedFromTargetEvent, session_id: str | None) -> None:
        self._frame_parents.pop(event["sessionId"], None)
        for frame_id in [fid for fid, sid in self._frame_sessions.items() if sid == event["sessionId"]]:
            del self._frame_sessions[frame_id]

    def _on_target_created(self, event: TargetCreatedEvent, session_id: str | None) -> None:
        info = event["targetInfo"]
        opener_id = info.get("openerId")
        if not self._closing and info["type"] == "page" and opener_id in self._owned:
            self._owned.add(info["targetId"])
            self._popups[info["targetId"]] = (opener_id, asyncio.get_running_loop().create_future())
            self._spawn(self._adopt_popup(info["targetId"], opener_id))

    async def _adopt_popup(self, target_id: str, opener_id: str) -> None:
        try:
            attach = await self.client.send.Target.attachToTarget(params={"targetId": target_id, "flatten": True})
            session_id = attach["sessionId"]
            await self.client.send.Target.activateTarget(params={"targetId": target_id})
            await self._prepare_session(session_id)
            self._tabs[target_id] = _TabState(target_id=target_id, session_id=session_id, opener_id=opener_id)
            # The popup may already have navigated to its final URL before this coroutine got scheduled
            # (targetCreated -> targetInfoChanged can both fire while we're still awaiting attachToTarget
            # above), so a targetInfoChanged event carrying it can arrive and be dropped because the tab
            # wasn't in self._tabs yet. Fetch current info now rather than relying on a future event.
            info = await self.client.send.Target.getTargetInfo(params={"targetId": target_id})
            self.set_tab_info(target_id, info["targetInfo"]["url"], info["targetInfo"]["title"])
        finally:
            adopted = self._popups[target_id][1]
            if not adopted.done():
                adopted.set_result(target_id in self._tabs)

    def popups(self) -> tuple[str, ...]:
        return tuple(self._popups)

    async def follow_popup(self, known: tuple[str, ...], within: float) -> str | None:
        """Make active the latest tab the active tab opened since `known` once it is adopted, and return its id.

        A link that opens its page in a new window left the run on the opener: the next step was spent choosing
        the new tab, and the read before it credited the opener's heading to the new window.
        """
        opener = self._active_target_id
        fresh = [t for t, (o, _) in self._popups.items() if o == opener and t not in known]
        if not fresh:
            return None
        with contextlib.suppress(TimeoutError):
            if await asyncio.wait_for(asyncio.shield(self._popups[fresh[-1]][1]), within) and fresh[-1] in self._tabs:
                await self.switch_tab(fresh[-1])
                return fresh[-1]
        return None

    def _on_target_info_changed(self, event: TargetInfoChangedEvent, session_id: str | None) -> None:
        info = event["targetInfo"]
        if info["targetId"] in self._tabs:
            self.set_tab_info(info["targetId"], info["url"], info["title"])

    def _on_target_destroyed(self, event: TargetDestroyedEvent, session_id: str | None) -> None:
        target_id = event["targetId"]
        self._tabs.pop(target_id, None)
        self._owned.discard(target_id)
        if target_id == self._active_target_id:
            self._set_active_target(next(iter(self._tabs), ""))

    # -- Live frames ---------------------------------------------------------------------------------

    async def _screencast_command(self, method: str, params: dict[str, object] | None, session_id: str | None) -> None:
        try:
            async with asyncio.timeout(_SCREENCAST_COMMAND_SECONDS):
                await self.client.send_raw(method, params, session_id)
        except Exception as exc:
            # Transport and consumer exceptions may contain credentials or page text.
            logger.warning("%s failed (%s)", method, type(exc).__name__)

    async def _update_screencast(self) -> None:
        # A tab can switch again while Chrome is answering the previous start or stop.
        async with self._screencast_lock:
            active = self._tabs.get(self._active_target_id)
            session_id = active.session_id if active is not None else None
            if self._closing or session_id == self._screencast_session_id:
                return
            if self._screencast_session_id is not None:
                await self._screencast_command("Page.stopScreencast", None, self._screencast_session_id)
                self._screencast_session_id = None
            active = self._tabs.get(self._active_target_id)
            if active is not None and not self._closing:
                self._screencast_session_id = active.session_id
                await self._screencast_command(
                    "Page.startScreencast",
                    {"format": "jpeg", "quality": 60, "maxWidth": 1280, "maxHeight": 800},
                    active.session_id,
                )

    def _on_screencast_frame(self, event: ScreencastFrameEvent, session_id: str | None) -> None:
        frame = _Frame(event["data"], event["sessionId"], session_id)
        active = self._tabs.get(self._active_target_id)
        if self._on_frame is None or self._closing or active is None or session_id != active.session_id:
            self._ack(frame)
            return
        # Chrome sends the next frame only once this one is acked, so acking after delivery paces the cast to
        # what the consumer can take, with no fixed rate. Latest wins: a frame superseded while one is being
        # delivered is acked unseen rather than dropped unacked, because an unacked frame stalls the cast.
        if self._pending_frame is not None:
            self._ack(self._pending_frame)
        self._pending_frame = frame
        if not self._frame_scheduled:
            self._frame_scheduled = True
            self._spawn(self._deliver_frames())

    def _ack(self, frame: _Frame) -> None:
        self._spawn(self._screencast_command("Page.screencastFrameAck", {"sessionId": frame.ack_id}, frame.session_id))

    async def _deliver_frames(self) -> None:
        try:
            while (frame := self._pending_frame) is not None and not self._closing:
                self._pending_frame = None
                try:
                    active = self._tabs.get(self._active_target_id)
                    if (
                        self._on_frame is not None
                        and not self.frames_withheld
                        and active is not None
                        and frame.session_id == active.session_id
                    ):
                        await self._on_frame(base64.b64decode(frame.data, validate=True))
                finally:
                    self._ack(frame)
        except Exception as exc:
            logger.warning("Live frame delivery failed (%s)", type(exc).__name__)
        finally:
            self._frame_scheduled = False
            if self._pending_frame is not None:
                self._ack(self._pending_frame)
                self._pending_frame = None

    # -- Dialogs ------------------------------------------------------------------------------------

    def _on_dialog(self, event: JavascriptDialogOpeningEvent, session_id: str | None) -> None:
        if session_id is None:
            return
        self._dialogs[session_id] = Dialog(
            kind=event["type"], message=event["message"], default_prompt=event.get("defaultPrompt")
        )
        self._dialog_opened.set()

    async def handle_dialog(self, accept: bool, prompt_text: str | None = None) -> None:
        session_id = next(
            sid for sid in (self.active_session_id, *self.frame_sessions().values()) if sid in self._dialogs
        )
        params: dict[str, bool | str] = {"accept": accept}
        if prompt_text is not None:
            params["promptText"] = prompt_text
        await self.client.send.Page.handleJavaScriptDialog(params=params, session_id=session_id)  # ty: ignore[invalid-argument-type]
        self._dialogs.pop(session_id, None)

    # -- Downloads ------------------------------------------------------------------------------------

    def _on_request_paused(self, event: RequestPausedEvent, session_id: str | None) -> None:
        if session_id is not None:
            self._spawn(self._handle_paused(event, session_id))

    async def _handle_paused(self, event: RequestPausedEvent, session_id: str) -> None:
        request_id = event["requestId"]
        try:
            headers = {h["name"].lower(): h["value"] for h in event.get("responseHeaders", [])}
            disposition = headers.get("content-disposition", "")
            if "attachment" in disposition.lower():
                await self._capture_download(request_id, session_id, event["request"]["url"], disposition)
        finally:
            # Never leave a request paused, even if capture above raised. The request/session may
            # already be gone (navigation, tab close), which is fine to swallow here.
            with contextlib.suppress(Exception):
                await self.client.send.Fetch.continueRequest(params={"requestId": request_id}, session_id=session_id)

    async def _capture_download(self, request_id: str, session_id: str, url: str, disposition: str) -> None:
        body = await self.client.send.Fetch.getResponseBody(params={"requestId": request_id}, session_id=session_id)
        raw = base64.b64decode(body["body"]) if body["base64Encoded"] else body["body"].encode()
        if len(raw) > self._max_download_bytes:
            return  # bounded size: refuse to hold an oversized body in memory as an artifact
        name = _filename_from_disposition(disposition) or url.rsplit("/", 1)[-1] or "download"
        artifact = await self._artifact_sink.put(ArtifactKind.DOWNLOAD, name, "application/octet-stream", raw)
        self._artifacts.append(artifact)


async def _together(*commands: Coroutine[Any, Any, object]) -> None:
    """Send independent commands at once and wait for all of them, raising the first `BrowserError` as sequential
    sends would, and cancelling the rest when one fails so none outlives the session it was sent on."""
    try:
        async with asyncio.TaskGroup() as tasks:
            for command in commands:
                tasks.create_task(command)
    except* BrowserError as errors:
        raise errors.exceptions[0] from errors


def _filename_from_disposition(disposition: str) -> str | None:
    for part in disposition.split(";"):
        part = part.strip()
        if part.lower().startswith("filename="):
            return part.split("=", 1)[1].strip('"')
    return None
