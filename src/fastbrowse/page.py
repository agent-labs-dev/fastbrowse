"""Seam between the browser layer and the decision loop.

The browser layer produces `Observation` (bounded, for Jev's action choice) and `Capture` (complete, for reading),
and executes `Action`s. Nothing above this seam touches CDP.
"""

from datetime import datetime
from enum import StrEnum
from typing import Protocol

from pydantic import Field

from fastbrowse.models import Artifact, Attachment, Frozen, Operation, StepOutcome


class Control(Frozen):
    id: str
    """Kept across observations for as long as the same DOM node survives."""
    frame_id: str | None
    frame_origin: str | None = None
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
    blocking: bool = False
    """A field its form will not submit without: required and still empty, or marked invalid by the page."""


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


class BlockKind(StrEnum):
    HEADING = "heading"
    PARAGRAPH = "paragraph"
    LIST_ITEM = "list_item"
    TABLE = "table"
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

    async def screenshot(self) -> bytes: ...

    async def origin(self) -> str: ...
