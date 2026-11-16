"""What a page reads and does, as `connect_cdp`'s page returns it, and the errors it raises.

An `Observation` is bounded, for Jev's action choice; a `Capture` is complete, for reading; an `Action` is what the
page is asked to do.
"""

from datetime import datetime
from enum import StrEnum

from pydantic import Field, field_serializer

from fastbrowse.models import Attachment, Frozen, Link, Operation, SourceControl, StepOutcome, Unavailable


class Control(Frozen):
    id: str
    """Kept across observations for as long as the same DOM node survives."""
    frame_id: str | None
    frame_origin: str | None = None
    form_id: str | None = None
    """Owner form or local field group, scoped to the frame and document that observed it."""
    retarget_key: str | None = Field(default=None, exclude=True)
    """Browser guard without node ids, tying a replacement to the same semantics and receiving document."""
    role: str
    label: str
    context: str | None = None
    """What tells this control apart from others reading exactly the same: the heading or first line of the
    nearest card, row or section holding it and none of its twins. Table column headers and date field sections
    also name controls whose labels do not collide."""
    operations: frozenset[Operation]
    value: str | None = None
    """Current value; masked for sensitive fields."""
    focused: bool | None = Field(default=None, exclude_if=lambda focused: focused is None)
    """Whether this control owns keyboard focus in its document, when the browser reports it."""
    href: str | None = None
    options: tuple[str, ...] = ()
    input_type: str | None = None
    input_name: str | None = None
    autocomplete: str | None = None
    submit_semantics: str | None = None
    """The form submission triggered by Enter in a field or clicking a submit button."""
    checked: bool | None = None
    selected: bool | None = None
    expanded: bool | None = None
    sensitive: bool = False
    offscreen: bool = False
    required: bool | None = None
    """A field the page marks required (`required` or `aria-required`)."""
    invalid: bool | None = None
    """A field marked invalid by ARIA or a failed native constraint, without implying submission."""
    validation_message: str | None = Field(default=None, exclude_if=lambda message: message is None)
    """The browser's own words for why a native constraint refuses this field."""
    next_page: bool | None = None
    """A link the page marks `rel="next"`: the next page of the list it belongs to."""
    download: bool | None = None
    """A link the page marks with a `download` attribute: activating it saves a file instead of opening a page."""
    drawn: bool | None = Field(default=None, exclude_if=lambda drawn: drawn is None)
    """An image, canvas or video the page takes input on: what it shows is pixels, with no page elements inside it
    to read or act on, such as a streamed remote browser or a canvas app."""

    @field_serializer("operations", when_used="json")
    def _sorted_operations(self, operations: frozenset[Operation]) -> list[Operation]:
        # A frozenset of strings iterates in hash order, which changes with each process's hash seed, so the
        # same control reached the field writer's prompt as ["click", "fill"] in one run and reversed in the next.
        return sorted(operations)


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


class Media(Frozen):
    """One audio or video player's state, read from the element when the page was observed."""

    kind: str
    """`audio` or `video`."""
    label: str | None = None
    """The player's own accessible name, when it has one."""
    heading: str | None = None
    """The nearest heading before the player, which tells unnamed players apart."""
    paused: bool
    ended: bool
    muted: bool
    elapsed_seconds: float
    duration_seconds: float | None = None
    """None while the length is unknown or the stream has no end."""


class Observation(Frozen):
    url: str
    title: str
    page_key: str
    """Fingerprint of the observed page. `act` refuses a key other than the latest observation's, then re-checks
    the target element itself before dispatch; an unrelated change elsewhere on the live page does not block it."""
    captured_at: datetime
    response_status: int | None = Field(default=None, ge=100, le=999)
    """HTTP status of this document, when the browser exposes it; absent for non-HTTP pages."""
    document_key: str = ""
    """Document identity, independent of field edits and scrolling, for access-wall checks."""
    controls: tuple[Control, ...]
    viewport_text: str
    """The visible text in the viewport of every frame the observation read; a cross-origin frame's text follows the
    page's own under a `[Frame: origin]` line."""
    offscreen_text: str = ""
    """The visible text outside the viewport, from every frame the observation read in the same order and form: what
    a read takes in that `viewport_text` leaves out."""
    visual_content: bool = False
    """A visible image, canvas, drawing or video carries state the text and the controls do not, so the verifier
    needs the screenshot even on a text-rich page whose state is the picture itself."""
    modal: str | None = None
    """The accessible name of the open modal dialog the rest of the page sits behind, empty when it has none; None
    when no modal is open. Controls behind an open modal are left out of `controls`."""
    media: tuple[Media, ...] = ()
    """Every audio and video player on the page, shown or not: their state lives in the element, not the page text."""
    tabs: tuple[Tab, ...]
    dialog: Dialog | None = None
    inaccessible_frames: int = Field(default=0, ge=0)
    """Frames whose content could not be read; reported so answers never claim full coverage."""
    can_go_back: bool = False
    """A same-origin http(s) entry sits immediately before the current one in browser history: BACK would leave
    the task's own site otherwise, as a wizard sharing one URL with no earlier entry but `about:blank` does."""
    history_entry: str | None = None
    """The tab's browser history entry this page is: Back, Forward and a reload return to the same one, and a
    navigation, a new tab or `pushState` makes another. None when the browser would not say."""


class BlockKind(StrEnum):
    HEADING = "heading"
    PARAGRAPH = "paragraph"
    LIST_ITEM = "list_item"
    TABLE = "table"
    RECORD = "record"
    CODE = "code"
    LINK = "link"
    OBSERVATION = "observation"


class Block(Frozen):
    source_id: str
    source_url: str | None = None
    complete_source: bool = False
    """This block contains the entire downloaded text, with no omitted bytes."""
    kind: BlockKind
    frame_id: str | None
    start: int = Field(ge=0)
    end: int = Field(ge=0)
    """Offsets into `Capture.text`; `text[start:end]` is this block's rendering."""
    heading_path: tuple[str, ...] = ()
    control_context: SourceControl | None = None
    href: str | None = None
    links: tuple[Link, ...] = Field(default=(), exclude_if=lambda links: not links)
    """The links inside this block, with their text's place in `Capture.text`: a link's text alone does not say where
    it leads, nor where a value the page set apart as its own element ends."""
    whole_record: bool = Field(default=False, exclude_if=lambda whole: not whole)
    """This block is all the text the page renders for one item it repeats, such as a comment, card or list row, with
    each part of it that shows no text named in square brackets. A record can be a `list_item` as well as a `record`,
    so its kind does not say this."""


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
    url: str | None = None
    target_id: str | None = None
    destination_id: str | None = None
    """Drop target of a drag: the control the dragged element is released onto."""
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


class ContextLost(BrowserError):
    """CDP's -32000 server error, which Chrome answers when the document a command addressed was replaced while
    it ran: "Execution context was destroyed", "Cannot find context with specified id", "Inspected target
    navigated or closed". Only the code is kept, as the message can carry page text, and other -32000 failures
    share it, so a reader treats it as a navigation only by reading the page again."""


class BrowserUnavailable(BrowserError, Unavailable):
    """The browser session no longer has a page available for this run."""


class ScreenshotsUnavailable(BrowserError):
    """A page cannot safely attribute pixels to its permitted documents."""


class NavigationTimeout(BrowserError):
    """`Page.navigate` gave up waiting for a document: the CDP command timed out, or the page never became ready.

    Before a run's first step no agent code has acted, so the run ends `unavailable`. A navigation action reports
    a failed step so the agent can try another destination."""


class SiteUnreachable(BrowserError):
    """`Page.navigate` got nothing from the site: Chrome named a dropped, refused or empty connection.

    Treated as `NavigationTimeout` is: before the first step the site was down, not the agent wrong. A name that
    does not resolve is not one of these; that can be a mistyped address."""


class BrowserUnresponsive(BrowserError, Unavailable):
    """The browser stopped answering: an outage the same run may not meet again, not a failed task."""


class PageUnresponsive(BrowserError):
    """A tab that stopped answering while the browser behind it still answers: a failure driving the page, which ends
    the run as its own error rather than an outage."""


class OriginNotAllowed(BrowserError):
    """A document outside `allowed_origins`. The message never names it: an address can carry a credential."""

    def __init__(self) -> None:
        super().__init__("the page is outside allowed_origins")


class _ScriptError(BrowserError):
    """A script the core evaluated threw in the page, as one can when the page replaced a global it calls."""


class ActResult(Frozen):
    outcome: StepOutcome
    page_changed: bool
    detail: str | None = None
    moved: bool = False
    """The action itself took the page elsewhere: its own input event began a navigation or changed the address,
    or it opened a tab. A page that moves on its own time, even to a new document just after the action, is not."""
