"""Seam between the browser layer and the decision loop.

The browser layer produces `Observation` (bounded, for Jev's action choice) and `Capture` (complete, for reading),
and executes `Action`s. Nothing above this seam touches CDP.
"""

import json
import re
from datetime import datetime
from enum import StrEnum
from typing import Protocol

from pydantic import Field

from fastbrowse.models import Artifact, Attachment, Frozen, Operation, StepOutcome


def cut_marker(omitted_chars: int) -> str:
    return f"\n[Viewport text cut: {omitted_chars} characters omitted; read the page for the rest]"


def cut_text(text: str, max_chars: int, *, json_encoded: bool = False) -> str:
    """`text` within `max_chars`, ending with how much was cut so a model knows the page goes on."""

    def size(value: str) -> int:
        # A JSON state escapes quotes and newlines; its budget must count those extra characters.
        return len(json.dumps(value)) - len('""') if json_encoded else len(value)

    if size(text) <= max_chars:
        return text

    def fits(keep: int) -> bool:
        return size(text[:keep] + cut_marker(len(text) - keep)) <= max_chars

    # The longest prefix that fits with its marker. Escaping makes one character cost up to six, so an excess
    # measured in encoded characters cannot be subtracted from a count of source characters.
    low, high = 0, min(len(text), max_chars)
    while low < high:
        middle = (low + high + 1) // 2
        if fits(middle):
            low = middle
        else:
            high = middle - 1
    return text[:low] + cut_marker(len(text) - low) if low > 0 and fits(low) else ""


class Control(Frozen):
    id: str
    """Kept across observations for as long as the same DOM node survives."""
    frame_id: str | None
    frame_origin: str | None = None
    retarget_key: str | None = Field(default=None, exclude=True)
    """Browser guard without node ids, tying a replacement to the same semantics and receiving document."""
    role: str
    label: str
    context: str | None = None
    """What tells this control apart from others reading exactly the same: the heading or first line of the
    nearest card, row or section holding it and none of its twins. Set only where labels collide."""
    operations: frozenset[Operation]
    value: str | None = None
    """Current value; masked for sensitive fields."""
    href: str | None = None
    options: tuple[str, ...] = ()
    input_type: str | None = None
    submit_semantics: str | None = None
    """The enclosing form's implicit submission, if Enter in this control can submit it."""
    checked: bool | None = None
    selected: bool | None = None
    expanded: bool | None = None
    sensitive: bool = False
    offscreen: bool = False
    blocking: bool | None = None
    """A field its form will not submit without: required and still empty, or marked invalid by the page."""
    next_page: bool | None = None
    """A link the page marks `rel="next"`: the next page of the list it belongs to."""


_ARROWS = "›»→>"  # noqa: RUF001 - the chevrons pagers draw, not a typo for ">"
_NEXT_PAGE = re.compile(
    rf"(?:next(?: page)?|more results|older(?: posts)?)\s*[{_ARROWS}]*|[{_ARROWS}]{{1,2}}", re.IGNORECASE
)


def pages_forward(control: Control) -> bool:
    """Whether a link is a pager's way to the next page of a list: marked `rel="next"`, or labelled like one.

    "next", "Next" and an arrow, "Next page", a lone chevron. The page's own mark says so in any language and behind
    an icon, so the label is only the fallback.
    """
    return bool(control.next_page) or _NEXT_PAGE.fullmatch(" ".join(control.label.split())) is not None


_LOAD_MORE = re.compile(r"(?:show|view|load|see)(?: \d+)? more(?: [\w-]+){0,3}", re.IGNORECASE)


def loads_more(control: Control) -> bool:
    """Whether a control is labelled as loading more of the list it ends: "View more flights", "Show 20 more".

    It sits at the foot of the list like a pager does, past what the off-screen cap keeps, and may carry no
    mark but its words: Google Flights' button has only its label. "Learn more" and the like do not match.
    """
    return Operation.CLICK in control.operations and _LOAD_MORE.fullmatch(" ".join(control.label.split())) is not None


def pager_link(control: Control) -> bool:
    """Whether this control is one the loop could actually walk a list with: a link, to an address.

    A carousel's arrow and a wizard's button read exactly like a pager, and a busy page draws many of them. Only a
    link to somewhere else is ever followed, so anything else by that name earns no special treatment.
    """
    return (
        control.role == "link"
        and Operation.CLICK in control.operations
        and bool(control.href)
        and pages_forward(control)
    )


class Tab(Frozen):
    id: str
    url: str
    title: str
    active: bool
    opener_id: str | None


class Dialog(Frozen):
    kind: str
    message: str
    default_prompt: str | None = None


class Observation(Frozen):
    url: str
    title: str
    page_key: str
    """Fingerprint of the observed page. `act` refuses a key other than the latest observation's, then re-checks
    the target element itself before dispatch; an unrelated change elsewhere on the live page does not block it."""
    captured_at: datetime
    document_key: str = ""
    """Document identity, independent of field edits and scrolling, for access-wall checks."""
    controls: tuple[Control, ...]
    omitted_controls: int = Field(ge=0)
    """Controls present but not offered because of observation limits."""
    viewport_text: str
    tabs: tuple[Tab, ...]
    dialog: Dialog | None = None
    inaccessible_frames: int = Field(default=0, ge=0)
    """Frames whose content could not be read; reported so answers never claim full coverage."""
    can_go_back: bool = False
    """A same-origin http(s) entry sits immediately before the current one in browser history: BACK would leave
    the task's own site otherwise, as a wizard sharing one URL with no earlier entry but `about:blank` does."""


class BlockKind(StrEnum):
    HEADING = "heading"
    PARAGRAPH = "paragraph"
    LIST_ITEM = "list_item"
    TABLE = "table"
    RECORD = "record"
    CODE = "code"
    LINK = "link"


class Block(Frozen):
    source_id: str
    kind: BlockKind
    frame_id: str | None
    start: int = Field(ge=0)
    end: int = Field(ge=0)
    """Offsets into `Capture.text`; `text[start:end]` is this block's rendering."""
    heading_path: tuple[str, ...] = ()
    href: str | None = None


class Capture(Frozen):
    url: str
    title: str
    captured_at: datetime
    sha256: str
    """SHA-256 of `text`; evidence records it so a quote can be re-validated later."""
    text: str
    blocks: tuple[Block, ...]
    inaccessible_frames: int = Field(default=0, ge=0)


class Action(Frozen):
    operation: Operation
    target_id: str | None = None
    text: str | None = None
    """Value to type or option to select. May hold a resolved secret: never log or persist an Action."""
    secret: bool = False
    """`text` is a resolved secret: the page masks the field so it never renders in a screenshot."""
    tab_id: str | None = None
    secret_origin: str | None = None
    """Origin authorized by the resolver; the receiving document must still match at insertion."""
    files: tuple[Attachment, ...] = ()
    accept_dialog: bool | None = None


class BrowserError(RuntimeError):
    """A browser failure a `Page` raises, with a message safe to put in a run result: never page text."""


class NavigationTimeout(BrowserError):
    """`Page.navigate` gave up waiting for a document: the CDP command timed out, or the page never became ready.

    Before a run's first step no agent code has acted, so the run ends `unavailable`; after it, the agent's own
    navigation timed out and the run ends `error` as any other browser failure does."""


class ActResult(Frozen):
    outcome: StepOutcome
    page_changed: bool
    detail: str | None = None


class Page(Protocol):
    @property
    def artifacts(self) -> tuple[Artifact, ...]: ...

    async def observe(self) -> Observation: ...

    async def navigate(self, url: str) -> None: ...

    async def capture(self) -> Capture: ...

    async def act(self, action: Action, observation: Observation) -> ActResult:
        """Dispatch at most once. Returns STALE or COVERED without dispatching when guards fail."""
        ...

    async def redrawn(self, observation: Observation, timeout_seconds: float, *, target_id: str | None = None) -> bool:
        """Whether `target_id`, or with none any control `observation` offered, changed within `timeout_seconds`.

        `act` refuses a control that changed since it was observed, so work deciding on it is already spent. A change
        is reported once the page has settled after it, ready to be observed again.
        """
        ...

    async def screenshot(self) -> bytes: ...

    def withhold_frames(self, withheld: bool) -> None:
        """Hold back live and recorded frames while the page may show a secret, which pixels cannot mask."""
        ...

    async def address(self) -> str:
        """The address the current document was served from, after any redirect."""
        ...

    async def response_status(self) -> int | None:
        """The HTTP status the current document was served with, or None when the browser does not say."""
        ...
