"""One owned tab on a `BrowserConnection`: attach, OOPIF sessions, popup ownership, dialogs, downloads.

Session lifecycle, bringing the owned tab forward when the run may, and the freshness/guard
primitives `page.py` builds on were proven live in jev-ultrafast (MIT): jev_ultrafast/browser.py.
"""

from __future__ import annotations

import asyncio
import base64
import contextlib
import hashlib
import json
import logging
from collections.abc import AsyncGenerator, Awaitable, Callable, Coroutine
from dataclasses import dataclass
from types import TracebackType
from typing import Any, Self, cast

from cdp_use.cdp.fetch.events import RequestPausedEvent
from cdp_use.cdp.fetch.types import RequestPattern
from cdp_use.cdp.page.events import (
    JavascriptDialogClosedEvent,
    JavascriptDialogOpeningEvent,
    ScreencastFrameEvent,
    WindowOpenEvent,
)
from cdp_use.cdp.runtime.events import BindingCalledEvent, ExecutionContextCreatedEvent
from cdp_use.cdp.target.commands import CreateTargetParameters
from cdp_use.cdp.target.events import (
    AttachedToTargetEvent,
    DetachedFromTargetEvent,
    TargetCreatedEvent,
    TargetDestroyedEvent,
    TargetInfoChangedEvent,
)
from cdp_use.cdp.target.types import TargetInfo
from cdp_use.client import CDPClient
from pydantic import JsonValue
from websockets.exceptions import ConnectionClosed, InvalidMessage, InvalidStatus

from fastbrowse.clients.validation import RETRYABLE_STATUS
from fastbrowse.datafiles import data_file
from fastbrowse.models import Artifact, ArtifactKind, ArtifactSink, FrameHandler, Frozen, Unavailable
from fastbrowse.models import BrowserConnection as BrowserConnectionModel
from fastbrowse.origins import OriginGrant
from fastbrowse.page import BrowserError, BrowserUnavailable, ContextLost, Dialog, Tab

logger = logging.getLogger(__name__)

_SCREENCAST_COMMAND_SECONDS = 0.5
_FRAME_RELEASE_SECONDS = 2.0
"""How long closing waits for a cancelled frame callback. The callback is the caller's code, and one that swallows
its cancellation held the run open with no result while its cloud browser went on billing."""
CDP_REPLY_SECONDS = 60.0
"""How long a CDP command waits before the browser is asked whether it is still there. A cloud browser's proxy can
keep the socket open, answering pings, after losing the browser behind it, and a run with no wall-clock limit would
otherwise wait on that reply forever. A slow reply is not a lost one: `Page.navigate` waits for a slow server's
headers, so a live browser can keep waiting until CDP_COMMAND_SECONDS."""
CDP_COMMAND_SECONDS = 120.0
"""A responsive browser can still leave one document command pending forever."""
CDP_ALIVE_SECONDS = 10.0
"""`Browser.getVersion` is answered at once by any browser that is there."""
CDP_DEADLINES_BEFORE_STOP = 2
"""Command deadlines one connection may meet before its tab's commands fail at once, as `PageUnresponsive`. A read
that waits out the deadline is often one whose caller treats a failed read as an unreadable page and carries on, so the
next read waited it out again: one login run met it thirteen times, 26 minutes on a page it could not use.
Browser-level commands (closing the run's tabs) are still sent."""


class BrowserUnresponsive(BrowserError, Unavailable):
    """The browser stopped answering: an outage the same run may not meet again, not a failed task."""


class PageUnresponsive(BrowserError):
    """A tab that stopped answering while the browser behind it still answers: a failure driving the page, which ends
    the run as its own error. It is not an outage, since the connection and the browser are both there; reporting it
    as one would leave the run's own failures out of what it is measured on."""


class OriginNotAllowed(BrowserError):
    """A document outside `allowed_origins`. The message never names it: an address can carry a credential."""

    def __init__(self) -> None:
        super().__init__("the page is outside allowed_origins")


async def check_browser_access(check: Callable[[], Awaitable[None]] | None) -> None:
    """Check a caller's live permission without exposing its exception details."""
    if check is None:
        return
    try:
        await check()
    except Exception as exc:
        raise BrowserError(f"check_access refused the browser ({type(exc).__name__}, caller supplied)") from None


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
# A scoped run also pauses each document request before it is sent, to refuse one outside the grant. Redirect
# hops, iframes and popups all pause as Documents, so one check covers them.
SCOPE_PATTERN: RequestPattern = {"urlPattern": "*", "resourceType": "Document", "requestStage": "Request"}
_ENABLE_DOMAINS = ("Page", "Runtime", "DOM")
_KEEP_PAINTING: dict[str, object] = {
    "format": "jpeg",
    "quality": 1,
    "maxWidth": 16,
    "maxHeight": 16,
    "everyNthFrame": 1000,
}
_SNAPSHOT_JS = data_file("browser", "snapshot.js").read_text(encoding="utf-8")
SNAPSHOT_UNINSTALLED = "fastbrowse: snapshot.js is not installed in this document"
_REFUSE_COOKIES_JS = data_file("browser", "autoconsent", "autoconsent.standalone.js").read_text(encoding="utf-8")
INPUT_WORLD = "fastbrowse-input"
"""The isolated world the input watch runs in: the page's own scripts cannot see a global, a listener or a binding
there, and a binding or listener in the page's world is a fingerprint any site can read."""
INPUT_NAVIGATION_BINDING = "__fastbrowseInputNavigation"
"""The binding the input watch calls, from its isolated world only, when the run's own input moved the page."""
# Whether the run's own input moved the page. A trusted input event's task, its promise reactions included, and the
# task after it are the run's: a form's submit() is planned as a task of its own, queued after the first timer. A
# navigation the Navigation API reports in that window, or one the user agent began for a link or form itself, to
# another document or on the same one, is reported through the binding, which outlives the document. A page that
# moves later, on a timer or after a fetch, is not: a script that set its location after a delay was once read as the
# move of whatever click came before it. DOM events and the navigate event reach an isolated world's listeners too.
INPUT_HISTORY_JS = (
    "(() => { if (globalThis.__watching) return; globalThis.__watching = true; let input = 0; "
    # Code's own events are not trusted, so code opens the window around them, from this world only: a page that
    # could open it could pass its own move off as the run's. A window left open closes itself.
    "const later = () => setTimeout(() => setTimeout(() => { input--; }, 0), 0); "
    "globalThis.__openInput = () => { input++; return setTimeout(() => { input--; }, 2000); }; "
    "globalThis.__closeInput = timer => { clearTimeout(timer); later(); }; "
    "for (const type of ['click', 'submit', 'keydown', 'keyup', 'input', 'change', 'pointerup', 'mouseup']) "
    "addEventListener(type, ev => { if (!ev.isTrusted) return; input++; later(); }, "
    "{capture: true, passive: true}); "
    "if (globalThis.navigation) navigation.addEventListener('navigate', ev => { "
    "if (!(ev.userInitiated || input > 0)) return; "
    f"const report = globalThis[{INPUT_NAVIGATION_BINDING!r}]; if (typeof report === 'function') report('1'); }}); "
    "})()"
)
"""DuckDuckGo's autoconsent (MPL-2.0, unmodified): refuses consent banners on known platforms and hides them
before they paint, so neither the agent's steps nor a recording are spent on one."""


class _HideEndpoint(logging.Filter):
    """cdp-use logs the full CDP URL at INFO, and a cloud browser's URL is the credential to drive it."""

    def filter(self, record: logging.LogRecord) -> bool:
        if isinstance(record.msg, str) and record.msg.startswith("Connecting to "):
            record.msg, record.args = "Connecting to the browser's CDP endpoint", None
        return True


logging.getLogger("cdp_use.client").addFilter(_HideEndpoint())


POPUP_ANNOUNCE_SECONDS = 5.0
"""How long a click that is opening a window waits for Chrome to announce its tab. In ten runs of a link opening a new
window, the tab was announced from 0.08 to 2.5 seconds after the click, and the one a loaded machine announced later
than the settle after the click was never followed: the run read the page that opened it."""

STATUS_WORLD = "fastbrowse-status"
"""The isolated world a tab's HTTP status is read in, out of reach of the page's scripts."""

_SERVER_ERROR = -32000
"""The code Chrome answers when the document or frame a command addressed is gone, as after a navigation."""


def _browser_error(method: str, cause: Exception, *, lost_is_outage: bool = True) -> BrowserError:
    detail = type(cause).__name__
    if lost_is_outage and isinstance(cause, (ConnectionClosed, ConnectionError)):
        # The connection to the browser is gone: an outage, not the run's failure.
        return BrowserUnresponsive(f"{method} failed ({detail})")
    if cause.args and isinstance(cause.args[0], dict):
        code = cast("dict[str, object]", cause.args[0]).get("code")
        if type(code) is int:
            detail = f"CDP {code}"
            if code == _SERVER_ERROR:
                return ContextLost(f"{method} failed ({detail})")
    return BrowserError(f"{method} failed ({detail})")


@dataclass(frozen=True, slots=True)
class _Frame:
    data: str
    ack_id: int
    session_id: str | None


_APPROVAL_HINT = (
    "If this is a Chrome you started yourself, it may be waiting for you to approve remote debugging, "
    "or the address may be stale; check the browser window and the address, then retry."
)


class _BrowserClient(CDPClient):
    _answered = False
    """Whether the browser has replied to anything on this connection. Chrome can accept the socket and then hold
    every command while its remote debugging approval prompt waits, which would otherwise read as a lost browser
    only after CDP_REPLY_SECONDS."""

    async def start(self) -> None:
        try:
            await super().start()
        except Exception as exc:
            if _unreachable(exc):
                raise BrowserUnresponsive(
                    f"CDP.start failed ({type(exc).__name__}): the browser did not complete the DevTools handshake. "
                    + _APPROVAL_HINT
                ) from exc
            raise _browser_error("CDP.start", exc) from exc

    async def stop(self) -> None:
        try:
            await super().stop()
        except Exception as exc:
            # A connection lost while closing comes after the run's own work, so it is never reported as an outage.
            raise _browser_error("CDP.stop", exc, lost_is_outage=False) from exc

    _deadlines = 0
    """Commands on this connection that outlived CDP_COMMAND_SECONDS."""

    async def send_raw(self, method: str, params: Any = None, session_id: str | None = None) -> dict[str, Any]:
        if session_id is not None and self._deadlines >= CDP_DEADLINES_BEFORE_STOP:
            raise PageUnresponsive(
                f"{method} not sent: the page stopped answering, {self._deadlines} commands already exceeded their "
                "deadline while the browser answered"
            )
        reply = asyncio.ensure_future(super().send_raw(method, params, session_id))
        try:
            try:
                async with asyncio.timeout(CDP_COMMAND_SECONDS):
                    while not (
                        await asyncio.wait({reply}, timeout=CDP_REPLY_SECONDS if self._answered else CDP_ALIVE_SECONDS)
                    )[0]:
                        # A reply that landed while the probe waited still counts, however the probe ended.
                        if not await self._alive() and not reply.done():
                            if not self._answered:
                                raise BrowserUnresponsive(
                                    f"{method} got no reply: the browser accepted the DevTools connection but has "
                                    "not answered anything on it. " + _APPROVAL_HINT
                                )
                            raise BrowserUnresponsive(f"{method} got no reply, and the browser stopped answering")
                    # An error reply is an answer too: the browser is there and reading the connection.
                    self._answered = True
                    return reply.result()
            except TimeoutError:
                # The browser can go during the last wait, after the probe before it answered, so it is asked once
                # more: only a browser that is still there makes this the page's failure rather than an outage.
                alive = reply.done() or await self._alive()
                if reply.done() and not reply.cancelled():
                    # The page answered while the browser was being asked: the command is done, however late.
                    self._answered = True
                    return reply.result()
                if not alive:
                    raise BrowserUnresponsive(f"{method} got no reply, and the browser stopped answering") from None
                self._deadlines += 1
                raise PageUnresponsive(
                    f"{method} exceeded its command deadline; the page did not answer while the browser did"
                ) from None
        except (BrowserUnresponsive, PageUnresponsive):
            raise
        except Exception as exc:
            # CDP error messages can contain evaluated source or page text, including secrets.
            raise _browser_error(method, exc) from exc
        finally:
            # Join the abandoned reply, as a cancelled caller's cleanup must not run ahead of it.
            reply.cancel()
            await asyncio.gather(reply, return_exceptions=True)
            # cdp-use warns on cancelled waits and logs reply contents for removed ones; drain late replies.
            for request_id, pending in tuple(self.pending_requests.items()):
                if pending.cancelled():
                    drain = pending.get_loop().create_future()
                    drain.add_done_callback(lambda done: None if done.cancelled() else done.exception())
                    self.pending_requests[request_id] = drain

    async def _alive(self) -> bool:
        try:
            async with asyncio.timeout(CDP_ALIVE_SECONDS):
                await super().send_raw("Browser.getVersion")
        except Exception:  # no answer, or a closed socket: either way nothing is there to reply
            return False
        self._answered = True
        return True


@dataclass
class _TabState:
    target_id: str
    session_id: str
    url: str = "about:blank"
    title: str = ""
    opener_id: str | None = None


class DownloadText(Frozen):
    mime: str
    name: str
    url: str
    text: str
    complete: bool = False


_DOWNLOAD_TEXT_BYTES = 16_384
_DOWNLOAD_TEXT_TOTAL = 65_536


class SnapshotScript:
    """snapshot.js, compiled once in each document and called by name after that.

    Sent whole with every evaluate, its source cost about 3ms of transfer and compiling per call, and each call
    started again from unoptimized code. A document installs it as it starts, from the tracking script every new
    document runs; a document older than the session installs it when a call first finds it missing. The key
    names the source and the grant, so another build or another grant never calls this one's function.
    """

    def __init__(self, grant: OriginGrant | None) -> None:
        origins = "null" if grant is None else json.dumps(sorted(grant.origins))
        key = json.dumps(hashlib.sha256(f"{origins}\n{_SNAPSHOT_JS}".encode()).hexdigest()[:16])
        self.install = (
            f"((globalThis.__fastbrowseScripts ||= {{}})[{key}] ||= "
            f"(snapshot => mode => snapshot(mode, {origins}))({_SNAPSHOT_JS}))"
        )
        """An expression that installs the function, if this document has none, and evaluates to it."""
        self.function = (
            f"(globalThis.__fastbrowseScripts?.[{key}] ?? "
            f"(() => {{ throw new Error({json.dumps(SNAPSHOT_UNINSTALLED)}); }})())"
        )
        """An expression for the installed function, which throws `SNAPSHOT_UNINSTALLED` where there is none."""


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
        check_access: Callable[[], Awaitable[None]] | None = None,
    ) -> None:
        self._connection = connection
        self._check_access = check_access
        self._grant = None if connection.allowed_origins is None else OriginGrant(connection.allowed_origins)
        self.denials = 0
        """Document requests the grant refused, so a navigation that was cut short is told from one that failed."""
        self._artifact_sink = artifact_sink
        self._max_download_bytes = max_download_bytes
        self.snapshot = SnapshotScript(self._grant)
        track = f"{self.snapshot.install}('fingerprint')"
        self._new_document_scripts = (track, _REFUSE_COOKIES_JS) if refuse_cookie_banners else (track,)
        self._client: CDPClient | None = None
        self._tabs: dict[str, _TabState] = {}
        self._owned: set[str] = set()
        self._keepers: dict[str, str] = {}
        """Tab target id -> the session whose cast keeps that tab painting, in a background run."""
        self._open_before: set[str] = set()
        """Targets already open when an attached session began, which never join the run."""
        self._popups: dict[str, tuple[str, asyncio.Future[bool]]] = {}
        self.window_opens = 0
        """`Page.windowOpen` events from the run's pages: each one is a window on its way, announced or not."""
        self._popup_signal = asyncio.Event()
        """Set, and replaced, each time a popup is begun, so a wait for one wakes as it is announced."""
        self._status_contexts: dict[tuple[str, int], str] = {}
        """(session, context id) -> unique id, for each world named `STATUS_WORLD` Chrome reported creating."""
        """Popup target id -> (opener target id, whether it was adopted), in the order they opened."""
        self._active_target_id = ""
        # A scoped run casts nothing: a frame is pixels of whatever the tab shows, and the browser's reports of
        # where it is can trail the screen, so no ordering of events makes a delivered frame provably in the grant.
        self._on_frame = None if self._grant is not None else on_frame
        # Set while the page may show a secret: frames are still acked, so the cast keeps pace, but not delivered.
        self._frames_withheld = False
        self._screencast_session_id: str | None = None
        self._screencast_lock = asyncio.Lock()
        self._pending_frame: _Frame | None = None
        self._frame_scheduled = False
        self._delivery: asyncio.Task[None] | None = None
        self._frame_sessions: dict[str, str] = {}
        """OOPIF frame_id -> session_id, keyed by target id per the plan's `frameId/targetId` guidance."""
        self._frame_parents: dict[str, str] = {}
        self._artifacts: list[Artifact] = []
        self._downloads_captured = 0
        """Captured downloads by count: a navigation that aborts on one is not a navigation that failed."""
        self._download_texts: list[DownloadText] = []
        """A captured download's readable text, oldest first: kept as evidence for a capture, not consumed."""
        self._download_texts_shown = 0
        """How many of them an observation has already shown, so each is shown to the run once."""
        self._dialogs: dict[str, Dialog] = {}
        self._dialog_urls: dict[str, str] = {}
        self._input_navigations = 0
        """The address of the document each pending dialog came from, which `Dialog` does not carry."""
        """Pending JS dialog per tab session id; cleared once handled."""
        self._dialog_opened = asyncio.Event()
        self._background: set[asyncio.Task[None]] = set()
        self._closing = False

    @property
    def frames_withheld(self) -> bool:
        """Always true in a scoped run, which delivers and records no frames at all."""
        return self._frames_withheld or self._grant is not None

    @frames_withheld.setter
    def frames_withheld(self, withheld: bool) -> None:
        self._frames_withheld = withheld

    @property
    def grant(self) -> OriginGrant | None:
        return self._grant

    async def check_access(self) -> None:
        """Ask the caller whether the run may still touch the browser, failing closed on any error it raises."""
        await check_browser_access(self._check_access)

    async def assert_clear(self) -> None:
        """Refuse revoked callers and active documents outside the exact-origin grant."""
        await self.check_access()
        if self._grant is None:
            return
        info = await self.client.send.Target.getTargetInfo(params={"targetId": self._active_target_id})
        if not self._grant.allows(info["targetInfo"]["url"]):
            raise OriginNotAllowed

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
        tab = self._tabs.get(self._active_target_id)
        if tab is None:
            raise BrowserUnavailable("active browser tab closed; no owned tab remains")
        return tab.session_id

    def frame_sessions(self) -> dict[str, str]:
        """Only frame sessions descended from the active tab may contribute observations."""

        def belongs(session_id: str) -> bool:
            seen: set[str] = set()
            while session_id in self._frame_parents and session_id not in seen:
                seen.add(session_id)
                session_id = self._frame_parents[session_id]
            return session_id == self.active_session_id

        return {fid: sid for fid, sid in self._frame_sessions.items() if belongs(sid)}

    def frame_family(self, frame_id: str) -> set[str]:
        """`frame_id` and every frame drawn inside it, whose sessions descend from that frame's session."""
        root = self._frame_sessions.get(frame_id)
        if root is None:
            return set()

        def inside(session_id: str) -> bool:
            seen: set[str] = set()
            while session_id not in seen:
                if session_id == root:
                    return True
                seen.add(session_id)
                if session_id not in self._frame_parents:
                    return False
                session_id = self._frame_parents[session_id]
            return False

        return {fid for fid, sid in self.frame_sessions().items() if inside(sid)}

    @property
    def artifacts(self) -> tuple[Artifact, ...]:
        return tuple(self._artifacts)

    @property
    def downloads_captured(self) -> int:
        return self._downloads_captured

    async def download_settled(self, since: int, *, timeout_seconds: float = 3.0) -> bool:
        """Whether a download landed since `since`, waiting briefly for the intercept's own task.

        The capture runs off the request-paused event in its own task, so it can complete a moment after the
        navigation it belongs to aborts. A short bounded wait tells a download abort from a plain failure
        without racing that task; a download that never came is not this kind of abort.
        """
        deadline = asyncio.get_event_loop().time() + timeout_seconds
        while self._downloads_captured <= since:
            if asyncio.get_event_loop().time() >= deadline:
                return False
            await asyncio.sleep(0.05)
        return True

    def download_texts(self) -> tuple[DownloadText, ...]:
        """Every readable download captured so far, oldest first.

        Kept rather than consumed: an observation shows each download once, but a capture has to be able to
        record it as evidence for the answer long after the observation that first surfaced it.
        """
        return tuple(self._download_texts)

    def pending_download_texts(self) -> tuple[DownloadText, ...]:
        """Readable downloads the run has not been shown yet, and now marked as shown."""
        pending = tuple(self._download_texts[self._download_texts_shown :])
        self._download_texts_shown = len(self._download_texts)
        return pending

    def frame_parent_session(self, session_id: str) -> str | None:
        return self._frame_parents.get(session_id)

    def tabs(self) -> tuple[Tab, ...]:
        def granted(t: _TabState) -> bool:
            return self._grant is None or self._grant.allows(t.url)

        return tuple(
            Tab(
                id=t.target_id,
                url=t.url if granted(t) else "about:blank",
                title=t.title if granted(t) else "",
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

    def pending_dialog_url(self) -> str | None:
        """The address of the document the pending dialog came from, if one is pending."""
        return next(
            (
                self._dialog_urls.get(sid)
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
        await self.check_access()
        self._closing = False
        self._client = _BrowserClient(self._connection.cdp_url)
        try:
            await self._client.start()
            await self.check_access()
            self._register_events()
            if self._connection.attach:
                # Turning discovery on replays targetCreated for every window already open, so it waits until
                # those are known and can be told apart from windows opened during the run.
                await self._attach_existing_tab(self._connection.target_match)
                await self.client.send.Target.setDiscoverTargets(params={"discover": True})
            else:
                # Discovery has to be on only before the tab can open a popup, which it cannot do while it is still
                # being created, so the two are sent at once rather than paying a cloud round trip for each.
                await _together(
                    self.client.send.Target.setDiscoverTargets(params={"discover": True}),
                    self._open_owned_tab("about:blank"),
                )
            if self._grant is not None:
                # A popup's first request can leave before a session attached after targetCreated has Fetch on, so
                # a scoped run has every new page attach paused and resumes it only once its Fetch is enabled.
                await self.client.send_raw(
                    "Target.setAutoAttach",
                    {
                        "autoAttach": True,
                        "waitForDebuggerOnStart": True,
                        "flatten": True,
                        "filter": [{"type": "page", "exclude": False}, {"exclude": True}],
                    },
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
        delivery = self._delivery
        # Download finalizers still need the socket to release intercepted requests.
        await asyncio.gather(*(task for task in background if task is not delivery), return_exceptions=True)
        if delivery is not None and delivery in background:
            await asyncio.wait({delivery}, timeout=_FRAME_RELEASE_SECONDS)
            if not delivery.done():
                logger.warning("The frame callback ignored cancellation; closing without it")
        if self._screencast_session_id is not None:
            await self._screencast_command("Page.stopScreencast", None, self._screencast_session_id)
            self._screencast_session_id = None
        if self._grant is not None:
            for target_id, tab in self._tabs.items():
                if target_id not in self._owned:  # a window the user keeps gets its service workers back
                    with contextlib.suppress(Exception):
                        await self._bypass_service_workers(tab.session_id, False)
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
                self._keepers.clear()

    async def switch_tab(self, target_id: str) -> None:
        if target_id not in self._tabs:
            raise ValueError(f"Unknown tab {target_id}")
        self._set_active_target(target_id)
        await self.bring_to_front()

    async def bring_to_front(self) -> None:
        """Put the active tab in front where the run may. A background run sends nothing: its tabs already render
        where they are, and Chrome raises and focuses the whole window along with an activated tab."""
        await self._activate(self._active_target_id)

    async def _activate(self, target_id: str) -> bool:
        """Activate a tab when the run may bring it forward, and say whether it did."""
        if self._connection.foreground:
            await self.client.send.Target.activateTarget(params={"targetId": target_id})
        return self._connection.foreground

    async def _show(self, target_id: str) -> None:
        """Have a tab that joins the run render: in front of its window, or behind it and kept painting."""
        if await self._activate(target_id):
            return
        # Focus emulation alone leaves a tab behind another with a compositor that sleeps between repaints, where
        # a screenshot of an idle page waits seconds for a frame. Chrome keeps a tab painting while it is being
        # cast, so a cast too small and too sparse to cost anything runs on a session of its own, where the casts
        # that deliver live frames and recordings cannot replace it. A start can go unanswered while a tab swaps
        # renderer, and a tab without the cast is only slower to capture, so it is bounded and never fails the run.
        try:
            async with asyncio.timeout(_SCREENCAST_COMMAND_SECONDS):
                keeper = await self.client.send.Target.attachToTarget(params={"targetId": target_id, "flatten": True})
        except Exception as exc:
            # Transport exceptions may contain credentials or page text.
            logger.warning("Target.attachToTarget for the paint keeper failed (%s)", type(exc).__name__)
            return
        self._keepers[target_id] = keeper["sessionId"]
        await self._screencast_command("Page.startScreencast", _KEEP_PAINTING, keeper["sessionId"])

    async def wake(self) -> bool:
        """Get the active tab painting again when a capture of it stalls. True when the tab was brought to the
        front, which always yields a frame; a background run restarts the cast that keeps it painting instead,
        which may have gone unanswered or ended with the renderer it was started on."""
        if await self._activate(self._active_target_id):
            return True
        if keeper := self._keepers.get(self._active_target_id):
            await self._screencast_command("Page.startScreencast", _KEEP_PAINTING, keeper)
        return False

    async def _attach_existing_tab(self, target_match: str | None) -> str:
        targets = (await self.client.send.Target.getTargets())["targetInfos"]
        self._open_before = {t["targetId"] for t in targets}
        # An Electron app or a browser with DevTools open lists the DevTools window as a page too.
        listed = [t for t in targets if t["type"] == "page"]
        if self._grant is not None:
            # A window outside the grant is never matched, named or touched: it is not the run's to see.
            listed = [t for t in listed if self._grant.allows(t["url"])]
        # An Electron app or a browser with DevTools open lists the DevTools window as a page too.
        pages = [t for t in listed if not t["url"].startswith("devtools://")]
        if target_match is not None:
            pages = [t for t in pages if target_match in t["url"] or target_match in t["title"]]
        if not pages:
            seen = [f"{t['title']!r} ({t['url']})" for t in listed]
            wanted = "no page" if target_match is None else f"no page matching {target_match!r}"
            raise BrowserError(f"{wanted} to attach to; pages: {seen}")
        target = pages[0]
        target_id = target["targetId"]
        attach = await self.client.send.Target.attachToTarget(params={"targetId": target_id, "flatten": True})
        session_id = attach["sessionId"]
        await _together(self._show(target_id), self._prepare_session(session_id))
        await self._run_document_scripts(session_id)
        self._tabs[target_id] = _TabState(
            target_id=target_id, session_id=session_id, url=target["url"], title=target["title"]
        )
        self._set_active_target(target_id)
        return target_id

    async def _run_document_scripts(self, session_id: str) -> None:
        # A window that was already open loaded its document before the new-document scripts were registered, so
        # without this its freshness tracking and banner refusal would start only at its next navigation.
        await asyncio.gather(
            *(
                self.client.send.Runtime.evaluate(params={"expression": source}, session_id=session_id)
                for source in self._new_document_scripts
            ),
            self._watch_input_now(session_id),
            return_exceptions=True,
        )

    @contextlib.asynccontextmanager
    async def input_window(self, session_id: str) -> AsyncGenerator[None]:
        """Count a move the page makes while code dispatches its own input events as the run's: a select's or a
        date field's change handler that moved the page is the run's doing, though no event it got was trusted."""
        context: int | None = None
        timer: JsonValue = None
        with contextlib.suppress(Exception):
            context = await self._input_context(session_id)
            opened = await self.client.send_raw(
                "Runtime.evaluate",
                {"expression": "globalThis.__openInput?.()", "contextId": context, "returnByValue": True},
                session_id=session_id,
            )
            timer = opened.get("result", {}).get("value")
        try:
            yield
        finally:
            if context is not None:
                with contextlib.suppress(Exception):
                    await self.client.send_raw(
                        "Runtime.evaluate",
                        {"expression": f"globalThis.__closeInput?.({json.dumps(timer)})", "contextId": context},
                        session_id=session_id,
                    )

    async def _input_context(self, session_id: str) -> int:
        """The input watch's isolated world in the session's main frame; asking by name returns the same world."""
        tree = await self.client.send_raw("Page.getFrameTree", session_id=session_id)
        world = await self.client.send_raw(
            "Page.createIsolatedWorld",
            {"frameId": tree["frameTree"]["frame"]["id"], "worldName": INPUT_WORLD},
            session_id=session_id,
        )
        return world["executionContextId"]

    async def _watch_input_now(self, session_id: str) -> None:
        """Start the input watch in a document that was already open, in its own isolated world as on a new one."""
        await self.client.send_raw(
            "Runtime.evaluate",
            {"expression": INPUT_HISTORY_JS, "contextId": await self._input_context(session_id)},
            session_id=session_id,
        )

    async def _open_owned_tab(self, url: str) -> str:
        # A tab that is neither in front nor focus-emulated does not render: animation frames never fire, so menus
        # that animate open never become visible, screenshots hang and clicks read as covered. A foreground run
        # activates its tab. A background run opens it behind the window's current tab, because Chrome focuses the
        # window of a tab it opens in front, and `_prepare_session` turns focus emulation on instead.
        params: CreateTargetParameters = {"url": url}
        if not self._connection.foreground:
            params["background"] = True
        created = await self.client.send.Target.createTarget(params=params)
        target_id = created["targetId"]
        self._owned.add(target_id)
        attach = await self.client.send.Target.attachToTarget(params={"targetId": target_id, "flatten": True})
        session_id = attach["sessionId"]
        # Foregrounding the tab and enabling its session are independent, and both finish before the tab is used.
        await _together(self._show(target_id), self._prepare_session(session_id))
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

    async def _emulate_focus(self, session_id: str) -> None:
        await self.client.send.Emulation.setFocusEmulationEnabled(params={"enabled": True}, session_id=session_id)

    async def _prepare_session(self, session_id: str) -> None:
        await self.check_access()
        scoped = self._grant is not None
        patterns = [*DOWNLOAD_PATTERNS, SCOPE_PATTERN] if scoped else list(DOWNLOAD_PATTERNS)
        # These domains are independent, but all must be ready before the session can be used.
        await _together(
            *(self.client.send_raw(f"{domain}.enable", session_id=session_id) for domain in _ENABLE_DOMAINS),
            self.client.send_raw(
                "Runtime.addBinding",
                {"name": INPUT_NAVIGATION_BINDING, "executionContextName": INPUT_WORLD},
                session_id=session_id,
            ),
            self.client.send_raw(
                "Page.addScriptToEvaluateOnNewDocument",
                {"source": INPUT_HISTORY_JS, "worldName": INPUT_WORLD},
                session_id=session_id,
            ),
            self.client.send_raw(
                "Target.setAutoAttach",
                {"autoAttach": True, "waitForDebuggerOnStart": scoped, "flatten": True},
                session_id=session_id,
            ),
            self.client.send.Fetch.enable(params={"patterns": patterns}, session_id=session_id),
            # A page behind another tab is hidden and unfocused, so it paints nothing and takes no typing. Focus
            # emulation has it render and hold focus where it is, which is what lets a run leave the window alone.
            *((self._emulate_focus(session_id),) if not self._connection.foreground else ()),
            *((self._bypass_service_workers(session_id, True),) if scoped else ()),
            # Track parsing and hydration before the first post-navigation read, so an already
            # quiet document does not pay another full window just to install its observer.
            *(
                self.client.send.Page.addScriptToEvaluateOnNewDocument(params={"source": source}, session_id=session_id)
                for source in self._new_document_scripts
            ),
        )
        if scoped:
            await self.client.send_raw("Runtime.runIfWaitingForDebugger", session_id=session_id)

    async def _bypass_service_workers(self, session_id: str, bypass: bool) -> None:
        # A service worker answers a navigation without the request reaching the Fetch gate, so a scoped run
        # sends every one to the network, where `SCOPE_PATTERN` sees it. The flag is the Network agent's, and
        # Chrome ignores it until that domain is on.
        await self.client.send_raw("Network.enable", session_id=session_id)
        await self.client.send_raw("Network.setBypassServiceWorker", {"bypass": bypass}, session_id=session_id)

    def _register_events(self) -> None:
        client = self.client
        client.register.Runtime.executionContextCreated(self._on_context_created)
        client.register.Page.windowOpen(self._on_window_open)
        client.register.Target.attachedToTarget(self._on_attached)
        client.register.Target.detachedFromTarget(self._on_detached)
        client.register.Target.targetCreated(self._on_target_created)
        client.register.Target.targetInfoChanged(self._on_target_info_changed)
        client.register.Target.targetDestroyed(self._on_target_destroyed)
        client.register.Page.javascriptDialogOpening(self._on_dialog)
        client.register.Runtime.bindingCalled(self._on_binding)
        client.register.Page.javascriptDialogClosed(self._on_dialog_closed)
        # A background run casts without a frame handler too, and a frame nobody acks stalls the cast that sent it.
        if self._on_frame is not None or not self._connection.foreground:
            client.register.Page.screencastFrame(self._on_screencast_frame)
        client.register.Fetch.requestPaused(self._on_request_paused)

    def _spawn(self, coro: Coroutine[None, None, None]) -> asyncio.Task[None] | None:
        if self._closing:
            coro.close()
            return None
        task = asyncio.ensure_future(coro)
        self._background.add(task)
        task.add_done_callback(self._background_finished)
        return task

    def _background_finished(self, task: asyncio.Task[None]) -> None:
        self._background.discard(task)
        if not task.cancelled():
            task.exception()

    def _on_window_open(self, event: WindowOpenEvent, session_id: str | None) -> None:
        self.window_opens += 1

    def _on_context_created(self, event: ExecutionContextCreatedEvent, session_id: str | None) -> None:
        context = event["context"]
        if session_id is not None and context.get("name") == STATUS_WORLD:
            self._status_contexts[(session_id, context["id"])] = context["uniqueId"]

    def status_context(self, session_id: str, context_id: int) -> str | None:
        """The unique id of the status world `context_id` names on `session_id`, once, if Chrome reported it."""
        return self._status_contexts.pop((session_id, context_id), None)

    # -- Target/frame tracking -------------------------------------------------------------------------

    def _on_attached(self, event: AttachedToTargetEvent, session_id: str | None) -> None:
        info = event["targetInfo"]
        if session_id is None:
            if self._grant is not None and event["waitingForDebugger"]:
                self._on_new_page(event)
        elif info["type"] == "iframe":
            self._frame_sessions[info["targetId"]] = event["sessionId"]
            self._frame_parents[event["sessionId"]] = session_id
            self._spawn(self._enable_frame_domains(event["sessionId"]))
        elif self._grant is not None and event["waitingForDebugger"]:
            self._spawn(self._resume(event["sessionId"]))  # a worker has no document to scope

    async def _enable_frame_domains(self, session_id: str) -> None:
        await self._prepare_session(session_id)

    async def _resume(self, session_id: str) -> None:
        await self.client.send_raw("Runtime.runIfWaitingForDebugger", session_id=session_id)

    def _on_new_page(self, event: AttachedToTargetEvent) -> None:
        """A page that opened since a scoped run asked for them paused. Sessions this run attached itself, and
        windows open before it asked, arrive with nothing to wait on and are left as they are."""
        info, session_id = event["targetInfo"], event["sessionId"]
        opener = self._popup_opener(info)
        if opener is not None:
            self._begin_popup(info["targetId"], opener, session_id)
        else:
            # Not the run's page, as a tab a person opened beside an attached window: let it start, and let go.
            self._spawn(self._release(session_id))

    async def _release(self, session_id: str) -> None:
        await self._resume(session_id)
        await self.client.send.Target.detachFromTarget(params={"sessionId": session_id})

    def _on_detached(self, event: DetachedFromTargetEvent, session_id: str | None) -> None:
        self._frame_parents.pop(event["sessionId"], None)
        for frame_id in [fid for fid, sid in self._frame_sessions.items() if sid == event["sessionId"]]:
            del self._frame_sessions[frame_id]

    def _on_target_created(self, event: TargetCreatedEvent, session_id: str | None) -> None:
        # A scoped run adopts popups where they attach paused, not here, after their first request may have left.
        if self._grant is None and (opener := self._popup_opener(event["targetInfo"])) is not None:
            self._begin_popup(event["targetInfo"]["targetId"], opener)

    def _popup_opener(self, info: TargetInfo) -> str | None:
        """The tab a new page belongs to, when the run adopts it."""
        opener_id = info.get("openerId")
        if self._closing or info["type"] != "page":
            return None
        if opener_id in self._owned:
            return opener_id
        # An attached window is not owned, so neither are its popups: they stay open with it. An Electron
        # app's main process opens windows with no opener, which count as the active window's. A browser
        # opens one too for every tab its user opens, so they are adopted only when a target was matched.
        if (
            self._connection.attach
            and info["targetId"] not in self._tabs
            and info["targetId"] not in self._open_before
            and (opener_id in self._tabs or (not opener_id and self._connection.target_match is not None))
        ):
            return opener_id or self._active_target_id
        return None

    def _begin_popup(self, target_id: str, opener_id: str, session_id: str | None = None) -> None:
        if target_id in self._popups:
            return
        if opener_id in self._owned:
            self._owned.add(target_id)
        self._popups[target_id] = (opener_id, asyncio.get_running_loop().create_future())
        self._popup_signal.set()
        self._popup_signal = asyncio.Event()
        self._spawn(self._adopt_popup(target_id, opener_id, session_id))

    async def _adopt_popup(self, target_id: str, opener_id: str, session_id: str | None = None) -> None:
        try:
            if session_id is None:
                attach = await self.client.send.Target.attachToTarget(params={"targetId": target_id, "flatten": True})
                session_id = attach["sessionId"]
            await _together(self._show(target_id), self._prepare_session(session_id))
            if self._connection.attach:
                await self._run_document_scripts(session_id)
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

    async def follow_popup(self, known: tuple[str, ...], *, expected: bool = False) -> str | None:
        """Make active the latest tab the active tab opened since `known` once it is adopted, and return its id.

        A link that opens its page in a new window left the run on the opener: the next step was spent choosing
        the new tab, and the read before it credited the opener's heading to the new window. Adoption is waited
        out however long it takes, since each of its commands has its own deadline: a fixed two seconds for it
        was outlasted on a loaded machine, and the run read the opener while the tab it had opened sat unused.

        With `expected`, the click is known to be opening a window (it named a new browsing context, or Chrome
        reported a window opening since `known` was taken), so a tab not announced yet is waited for, up to
        `POPUP_ANNOUNCE_SECONDS`; any other click returns at once when nothing has been announced.
        """
        opener = self._active_target_id
        deadline = asyncio.get_running_loop().time() + (POPUP_ANNOUNCE_SECONDS if expected else 0)
        while not (fresh := [t for t, (o, _) in self._popups.items() if o == opener and t not in known]):
            signal = self._popup_signal
            remaining = deadline - asyncio.get_running_loop().time()
            if remaining <= 0:
                return None
            with contextlib.suppress(TimeoutError):
                await asyncio.wait_for(signal.wait(), remaining)
        if await asyncio.shield(self._popups[fresh[-1]][1]) and fresh[-1] in self._tabs:
            await self.switch_tab(fresh[-1])
            return fresh[-1]
        return None

    def _on_target_info_changed(self, event: TargetInfoChangedEvent, session_id: str | None) -> None:
        info = event["targetInfo"]
        if info["targetId"] in self._tabs:
            self.set_tab_info(info["targetId"], info["url"], info["title"])
        # An opener Chrome reports only after the target was created makes it the run's popup from then on.
        elif (
            self._grant is None
            and info.get("openerId")
            and info["targetId"] not in self._popups
            and (opener := self._popup_opener(info)) is not None
        ):
            self._begin_popup(info["targetId"], opener)

    def _on_target_destroyed(self, event: TargetDestroyedEvent, session_id: str | None) -> None:
        target_id = event["targetId"]
        self._tabs.pop(target_id, None)
        self._keepers.pop(target_id, None)
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
            self._delivery = self._spawn(self._deliver_frames())

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
        self._dialog_urls[session_id] = event.get("url", "")
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

    def _on_binding(self, event: BindingCalledEvent, session_id: str | None) -> None:
        if event["name"] == INPUT_NAVIGATION_BINDING:
            self._input_navigations += 1

    @property
    def input_navigations(self) -> int:
        """How many navigations to another document the run's own input has begun, on any page, so far."""
        return self._input_navigations

    def _on_dialog_closed(self, event: JavascriptDialogClosedEvent, session_id: str | None) -> None:
        # Navigation can dismiss a dialog without going through handle_dialog.
        if session_id is not None:
            self._dialogs.pop(session_id, None)
            self._dialog_urls.pop(session_id, None)

    # -- Downloads ------------------------------------------------------------------------------------

    def _on_request_paused(self, event: RequestPausedEvent, session_id: str | None) -> None:
        if session_id is None:
            return
        # A response-stage pause carries a status or an error; a request-stage one is before anything is sent.
        if self._grant is not None and "responseStatusCode" not in event and "responseErrorReason" not in event:
            if not self._grant.allows(event["request"]["url"]):
                # Counted here, not in the task, so a navigation that returns next sees it. ERR_ABORTED rather
                # than a block page, which would commit a document that names the address it refused.
                self.denials += 1
                self._spawn(self._refuse(event["requestId"], session_id))
                return
            self._spawn(self._release_request(event["requestId"], session_id))
            return
        self._spawn(self._handle_paused(event, session_id))

    async def _refuse(self, request_id: str, session_id: str) -> None:
        with contextlib.suppress(Exception):  # the tab may already be gone
            await self.client.send.Fetch.failRequest(
                params={"requestId": request_id, "errorReason": "Aborted"}, session_id=session_id
            )

    async def _release_request(self, request_id: str, session_id: str) -> None:
        with contextlib.suppress(Exception):
            await self.client.send.Fetch.continueRequest(params={"requestId": request_id}, session_id=session_id)

    async def _handle_paused(self, event: RequestPausedEvent, session_id: str) -> None:
        request_id = event["requestId"]
        try:
            headers = {h["name"].lower(): h["value"] for h in event.get("responseHeaders", [])}
            disposition = headers.get("content-disposition", "")
            mime = headers.get("content-type", "")
            if _is_download(mime, disposition):
                await self._capture_download(
                    request_id,
                    session_id,
                    event["request"]["url"],
                    disposition,
                    mime,
                    complete=event.get("responseStatusCode") == 200 and "content-range" not in headers,
                )
        finally:
            # Never leave a request paused, even if capture above raised. The request/session may
            # already be gone (navigation, tab close), which is fine to swallow here.
            with contextlib.suppress(Exception):
                await self.client.send.Fetch.continueRequest(params={"requestId": request_id}, session_id=session_id)

    async def _capture_download(
        self, request_id: str, session_id: str, url: str, disposition: str, mime: str, *, complete: bool
    ) -> None:
        body = await self.client.send.Fetch.getResponseBody(params={"requestId": request_id}, session_id=session_id)
        raw = base64.b64decode(body["body"]) if body["base64Encoded"] else body["body"].encode()
        if len(raw) > self._max_download_bytes:
            return  # bounded size: refuse to hold an oversized body in memory as an artifact
        name = _filename_from_disposition(disposition) or url.rsplit("/", 1)[-1] or "download"
        artifact = await self._artifact_sink.put(
            ArtifactKind.DOWNLOAD, name, _response_mime(mime) or "application/octet-stream", raw
        )
        self._artifacts.append(artifact)
        self._downloads_captured += 1
        if text := _readable_text(mime, raw):
            self._download_texts.append(
                DownloadText(
                    mime=_response_mime(mime),
                    name=name,
                    url=url,
                    text=text,
                    complete=complete and len(raw) <= _DOWNLOAD_TEXT_BYTES,
                )
            )
            while sum(len(item.text) for item in self._download_texts) > _DOWNLOAD_TEXT_TOTAL:
                self._download_texts.pop(0)
                self._download_texts_shown = max(0, self._download_texts_shown - 1)


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


# Chrome hands these to the download manager rather than rendering them as a document, so a document
# navigation to one is a download and aborts. A `content-disposition: attachment` (or a save-as filename)
# is the site saying so explicitly; a bare `text/csv` says it by type. This is a property of the response,
# not of any one site, so a link whose target is a file is captured the same way everywhere.
_DOWNLOAD_MIME_TYPES = frozenset(
    {
        "text/csv",
        "application/csv",
        "application/octet-stream",
        "application/zip",
        "application/gzip",
        "application/x-gzip",
        "application/x-tar",
        "application/x-7z-compressed",
        "application/vnd.ms-excel",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    }
)


def _response_mime(mime: str) -> str:
    return mime.split(";", 1)[0].strip().lower()


def _is_download(mime: str, disposition: str) -> bool:
    """Whether this response is a file Chrome downloads rather than a document it renders."""
    lowered = disposition.lower()
    if "attachment" in lowered or "filename=" in lowered:
        return True
    return _response_mime(mime) in _DOWNLOAD_MIME_TYPES


def _readable_text(mime: str, raw: bytes) -> str | None:
    """The response's text when it is a text file the run should be able to read, else None."""
    kind = _response_mime(mime)
    if kind.startswith("text/") or kind in {"application/csv", "application/json"}:
        text = raw[:_DOWNLOAD_TEXT_BYTES].decode("utf-8", errors="replace")
        if len(raw) > _DOWNLOAD_TEXT_BYTES:
            text += f"\n[Download cut: {len(raw) - _DOWNLOAD_TEXT_BYTES} bytes omitted; full file in artifact]"
        return text
    return None
