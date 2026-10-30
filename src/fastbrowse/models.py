"""Run-level contracts: what a caller passes in and what a run returns."""

from __future__ import annotations

import base64
from collections.abc import Awaitable, Callable
from datetime import datetime
from enum import StrEnum
from pathlib import Path
from typing import Annotated, Literal, Protocol

from pydantic import BaseModel, ConfigDict, Field, JsonValue, PlainSerializer


class Frozen(BaseModel):
    # A field's docstring is the model's only guidance for that field when the class is a response schema.
    model_config = ConfigDict(frozen=True, extra="forbid", use_attribute_docstrings=True)


class Status(StrEnum):
    COMPLETE = "complete"
    """Every planner requirement is evidenced and the caller's `until` check (if any) passed."""
    UNVERIFIED = "unverified"
    """The run believes it finished but could not evidence every requirement."""
    NEEDS_CONFIRMATION = "needs_confirmation"
    """Stopped before an irreversible action the caller has not authorized, having committed nothing."""
    NEEDS_LOGIN = "needs_login"
    """Authentication is required and no authorized credential covers this origin."""
    BLOCKED = "blocked"
    """A bot check (a CAPTCHA, or a browser verification that never clears) stands in the way. Not a sign-in:
    no credential passes it, and a person is needed only if they choose to solve it."""
    NEEDS_INPUT = "needs_input"
    """A required value or file is missing, or an upload exceeds the configured size limit."""
    STUCK = "stuck"
    """Recovery was exhausted without progress."""
    BUDGET_EXCEEDED = "budget_exceeded"
    OBSERVATION_LIMIT = "observation_limit"
    """The page or requirement evidence could not fit the configured input budgets."""
    UNAVAILABLE = "unavailable"
    """A model or browser provider stayed unavailable through every retry. Nothing about the task failed; the same
    run later may pass."""
    ERROR = "error"


class Unavailable(RuntimeError):
    """A provider answered only with retryable statuses, or not at all, until its retries ran out."""


class ConfigurationError(RuntimeError):
    """A missing key or an invalid setting in the environment, reported without a traceback."""


class RecordingError(RuntimeError):
    """ffmpeg is missing. Later failures are logged instead: a video is never worth a run's result."""


class Operation(StrEnum):
    CLICK = "click"
    HOVER = "hover"
    DRAG = "drag"
    FILL = "fill"
    SELECT = "select"
    ENTER = "enter"
    ESCAPE = "escape"
    SCROLL = "scroll"
    SCROLL_UP = "scroll_up"
    BACK = "back"
    SWITCH_TAB = "switch_tab"
    UPLOAD = "upload"
    DIALOG = "dialog"
    READ = "read"
    DONE = "done"
    ESCALATE = "escalate"
    NAVIGATE = "navigate"


class Decider(StrEnum):
    """Who chose a step: Jev, the LLM (a recovery or a reader that named it), or a rule in code, such as the
    next page of a list or DONE once a lookup is answered."""

    JEV = "jev"
    LLM = "llm"
    CODE = "code"


class StepOutcome(StrEnum):
    EXECUTED = "executed"
    COVERED = "covered"
    """The target was not the topmost element at its point; nothing was dispatched."""
    STALE = "stale"
    """The page changed between observation and dispatch; nothing was dispatched."""
    FAILED = "failed"


class SourceControl(Frozen):
    role: str
    label: str
    context: str | None = None


class Evidence(Frozen):
    source_id: str
    url: str
    frame_id: str | None
    captured_at: datetime
    capture_sha256: str
    start: int = Field(ge=0)
    end: int = Field(ge=0)
    quote: str
    complete_source: bool = False
    """The quote covers an entire downloaded text file, apart from surrounding whitespace."""
    control_context: SourceControl | None = None
    rendered_text: bool = True
    """False for typed DOM observations whose quote is not literal rendered page text."""
    heading_path: tuple[str, ...] = ()
    """The headings the quote sits under on the page: which record a bare "£10.69" is the price of."""


class Citation(Frozen):
    id: int = Field(ge=1)
    """The number used by this fact's links in the answer, stable within a run."""
    text: str
    requirement_id: str | None = None
    url: str
    quote: str
    deep_link: str


class FactReader(StrEnum):
    JEV_CHOICE = "jev_choice"
    LLM = "llm"


class StepFact(Frozen):
    text: str
    requirement_id: str | None = None
    quote: str | None = None
    """None for a count, total or winner derived from other facts rather than read from the page."""
    url: str | None = None
    reader: FactReader
    deep_link: str | None = None


class StepChoice(Frozen):
    id: str
    role: str
    label: str
    """The label as Jev read it, with resolved secrets masked."""
    context: str | None = None
    """The card or row that tells this control from its twins, as Jev read it, with resolved secrets masked."""
    operations: tuple[Operation, ...]
    chosen: bool = False
    """Whether Jev chose this control: the target, and for a drag the destination too."""


class ArtifactKind(StrEnum):
    DOWNLOAD = "download"


class Artifact(Frozen):
    kind: ArtifactKind
    name: str
    mime_type: str
    size_bytes: int = Field(ge=0)
    sha256: str
    uri: str
    """Reference returned by the caller's `ArtifactSink`; fastbrowse never keeps bytes past the run."""


class CostComponent(StrEnum):
    JEV = "jev"
    LLM = "llm"
    BROWSER = "browser"
    PROXY = "proxy"


class CostBasis(StrEnum):
    METERED = "metered"
    """Reported by the provider for this exact call."""
    ESTIMATED = "estimated"
    """Computed from token counts and a price table."""
    BOUNDED = "bounded"
    """No charge was reported, and `dollars` is the most the call could have cost: its model's whole context
    window at the highest token price any of its routes lists. Never less than the charge."""
    UNKNOWN = "unknown"
    """No charge information; never counted as zero."""


class LLMPurpose(StrEnum):
    PLAN = "plan"
    READ = "read"
    FIELD_TEXT = "field_text"
    RECOVER = "recover"
    COMPOSE = "compose"
    SHORTCUT = "shortcut"


class CostLine(Frozen):
    component: CostComponent
    basis: CostBasis
    dollars: float | None = Field(default=None, ge=0)
    purpose: LLMPurpose | None = None
    input_tokens: int = Field(default=0, ge=0)
    output_tokens: int = Field(default=0, ge=0)
    seconds: float | None = Field(default=None, ge=0)
    """Wall time of the call, retries included. None for a line that is not one call, such as browser time."""

    @property
    def label(self) -> str:
        return self.component.value if self.purpose is None else f"{self.component.value}:{self.purpose.value}"


class CostBreakdown(Frozen):
    lines: tuple[CostLine, ...] = ()

    def seconds_by_call(self) -> dict[str, float]:
        """Wall time per kind of call. Calls can overlap (the plan runs beside the first steps), so the sum
        can exceed the run's wall time."""
        totals: dict[str, float] = {}
        for line in self.lines:
            if line.seconds is not None:
                totals[line.label] = round(totals.get(line.label, 0.0) + line.seconds, 2)
        return totals

    @property
    def known_dollars(self) -> float:
        return sum(line.dollars for line in self.lines if line.dollars is not None)

    @property
    def has_unknown(self) -> bool:
        return any(line.basis is CostBasis.UNKNOWN for line in self.lines)


class StepResult(Frozen):
    index: int = Field(ge=0)
    operation: Operation
    decided_by: Decider
    outcome: StepOutcome
    url: str
    target: str | None = None
    """Human-readable label of the chosen element; never contains a secret value."""
    confidence: float | None = Field(default=None, ge=0, le=1)
    note: str | None = None
    facts: tuple[StepFact, ...] = ()
    """Facts added to the notes by this step, with resolved secrets redacted."""
    page_changed: bool | None = None
    """Whether the page's fingerprint changed; None for steps that do not act (read, escalate)."""
    duration_ms: int = Field(ge=0)
    choices: tuple[StepChoice, ...] = Field(default=(), exclude_if=lambda choices: not choices)
    """The controls Jev was offered for this step's decision, when `Config.step_choices` asked for them."""


class BudgetStop(Frozen):
    resource: Literal["steps", "dollars", "seconds", "jev_calls", "llm_calls"]
    limit: float


class WallKind(StrEnum):
    """What a sign-in or bot wall asks of the person who has to get the run past it."""

    PASSWORD = "password"
    """A password field: the account's credentials."""
    ONE_TIME_CODE = "one_time_code"
    """Only one-time-code fields: a code the site sent by email, text or an authenticator app."""
    SIGN_IN = "sign_in"
    """A sign-in to act on (a button, a provider, an account name) and no password or code field yet."""
    BOT_CHECK = "bot_check"
    """A bot check, which no credential passes."""


class Wall(Frozen):
    """The wall a `needs_login` or `blocked` run stopped at, typed so a caller can route it without parsing `error`."""

    kind: WallKind
    origin: str
    """The origin of the page that showed the wall."""
    fields: tuple[str, ...] = ()
    """The visible labels of the fields it asked for, redacted as the error is."""
    seen_at_ms: int
    """When the run stopped at it, in UTC epoch milliseconds."""


class RunResult(Frozen):
    status: Status
    budget: BudgetStop | None = None
    answer: str | None
    data: JsonValue | None
    evidence: tuple[Evidence, ...]
    steps: tuple[StepResult, ...]
    cost: CostBreakdown
    artifacts: tuple[Artifact, ...]
    citations: tuple[Citation, ...] = ()
    error: str | None = None
    final_url: str | None = None
    """Where the browser was last observed; what a caller checks when the task was to arrive somewhere."""
    final_frame: bytes | None = Field(default=None, exclude=True, repr=False)
    """A safe final PNG when step frames are enabled; kept out of text results."""
    recordings: tuple[Path, ...] = ()
    """The videos this run finished writing, captioned then plain; empty when it recorded nothing or encoding failed."""
    wall: Wall | None = None
    """The sign-in or bot wall a `needs_login` or `blocked` run stopped at, when the page showed one."""

    @property
    def succeeded(self) -> bool:
        return self.status is Status.COMPLETE


class Build(Frozen):
    """The build that produced a result."""

    fastbrowse_version: str | None
    git_sha: str | None
    """The full commit, or None when no commit is known (the standalone binary or a released wheel)."""
    git_dirty: bool | None
    """Whether the tree had uncommitted changes; None outside a checkout."""


class CliResult(RunResult):
    """What `fastbrowse --json` prints: the run's result and the build that produced it."""

    build: Build


class LocalChrome(Frozen):
    binary: str | None = None
    """A name or path that replaces discovery of the Chrome binary."""
    headed: bool = False
    """Show the window, to watch a run."""
    profile: Path | None = None
    """A profile directory kept between runs, so a site signed into there stays signed in. Without one,
    every run starts from a fresh profile that is deleted afterwards."""


class Attachment(Frozen):
    name: str
    mime_type: str
    content: bytes


class Limits(Frozen):
    max_steps: int | None = Field(default=None, gt=0)
    """Steps that reached the page. A click whose element was redrawn before it landed dispatched nothing and is
    not counted; the stall budget ends a page that keeps redrawing."""
    max_jev_calls: int | None = Field(default=None, gt=0)
    """Logical Jev evaluations. A retried request adds cost but not a call."""
    max_llm_calls: int | None = Field(default=None, gt=0)
    max_dollars: float | None = Field(default=None, gt=0)
    """Bounds Jev and LLM spend as it happens. A cloud browser bills when it stops, after the run, so its
    cost is reported in the result but cannot stop the run that incurred it."""
    max_seconds: float | None = Field(default=None, gt=0)
    """Stops active work at the time budget. Returning also waits for owned request and browser cleanup."""


class Authorization(Frozen):
    irreversible_actions: bool = False
    """Allow submit/pay/delete/send style actions without pausing for confirmation."""


class SecretRef(Frozen):
    name: str
    origins: tuple[str, ...]
    """Origins (scheme://host[:port]) the value may be typed into; models only ever see `name`."""


class SecretResolver(Protocol):
    def available(self) -> tuple[SecretRef, ...]: ...

    async def resolve(self, name: str, origin: str) -> str | None: ...


class ArtifactSink(Protocol):
    async def put(self, kind: ArtifactKind, name: str, mime_type: str, content: bytes) -> Artifact: ...


def _as_base64(image: bytes) -> str:
    return base64.b64encode(image).decode("ascii")


# Pydantic writes bytes into JSON as UTF-8, which an image is not, so a model holding one could not be dumped
# at all. The standard alphabet, not pydantic's URL-safe one: it is what a browser's `atob` reads.
Image = Annotated[bytes, PlainSerializer(_as_base64, return_type=str, when_used="json")]
"""The bytes of a PNG or JPEG. In JSON they are base64."""


class StepEvent(Frozen):
    type: Literal["step"] = "step"
    step: StepResult
    frame: Image | None = None
    """A PNG of the page this step acted on, when `Config.step_frames` asked for one. None when it did not,
    and also when a resolved secret was showing as page text: pixels cannot be masked the way text is."""


class BrowserEvent(Frozen):
    """Sent once, when the browser is open and before the first step."""

    type: Literal["browser"] = "browser"
    live_url: str | None
    """Where a cloud browser can be watched live; None for local Chrome."""
    browser_id: str | None = None
    """The cloud browser's id, for a caller that may have to stop it out of band. None when the run did not
    start a browser of its own (local Chrome, or one handed over through `cdp_url`)."""


type EventHandler = Callable[[StepEvent | BrowserEvent], Awaitable[None]]
type FrameHandler = Callable[[bytes], Awaitable[None]]
"""Receives the active tab's JPEG bytes on the caller's event loop. A handler must honour cancellation: one that
swallows `CancelledError` keeps that loop alive, so `asyncio.run` waits for it at shutdown."""
type UntilCheck = Callable[[str], Awaitable[bool]]
"""Caller assertion over the final page URL; COMPLETE requires it to return True when supplied."""
