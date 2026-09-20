"""Run-level contracts: what a caller passes in and what a run returns."""

from collections.abc import Awaitable, Callable
from datetime import datetime
from enum import StrEnum
from pathlib import Path
from typing import Literal, Protocol

from pydantic import BaseModel, ConfigDict, Field, JsonValue


class Frozen(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")


class Status(StrEnum):
    COMPLETE = "complete"
    """Every planner requirement is evidenced and the caller's `until` check (if any) passed."""
    UNVERIFIED = "unverified"
    """The run believes it finished but could not evidence every requirement."""
    NEEDS_CONFIRMATION = "needs_confirmation"
    """Stopped before an irreversible action the caller has not authorized, having committed nothing."""
    NEEDS_LOGIN = "needs_login"
    """Authentication is required and no authorized credential covers this origin."""
    NEEDS_INPUT = "needs_input"
    """A field needs a value the caller did not supply and the LLM must not invent."""
    STUCK = "stuck"
    """Recovery was exhausted without progress."""
    BUDGET_EXCEEDED = "budget_exceeded"
    OBSERVATION_LIMIT = "observation_limit"
    """The page could not be represented within Jev's input limits even after reduction."""
    ERROR = "error"


class Operation(StrEnum):
    CLICK = "click"
    HOVER = "hover"
    FILL = "fill"
    SELECT = "select"
    ENTER = "enter"
    ESCAPE = "escape"
    SCROLL = "scroll"
    BACK = "back"
    SWITCH_TAB = "switch_tab"
    UPLOAD = "upload"
    DIALOG = "dialog"
    READ = "read"
    DONE = "done"
    ESCALATE = "escalate"


TARGETED = frozenset(
    {Operation.CLICK, Operation.HOVER, Operation.FILL, Operation.SELECT, Operation.ENTER, Operation.UPLOAD}
)
"""Operations aimed at one observed control, which is hit-tested before input reaches it."""


class Decider(StrEnum):
    JEV = "jev"
    LLM = "llm"
    CODE = "code"
    """A step code took without a model's choice, such as the next page of a list the reader said continues."""


class StepOutcome(StrEnum):
    EXECUTED = "executed"
    COVERED = "covered"
    """The target was not the topmost element at its point; nothing was dispatched."""
    STALE = "stale"
    """The page changed between observation and dispatch; nothing was dispatched."""
    FAILED = "failed"


class Evidence(Frozen):
    source_id: str
    url: str
    frame_id: str | None
    captured_at: datetime
    capture_sha256: str
    start: int = Field(ge=0)
    end: int = Field(ge=0)
    quote: str


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
    UNKNOWN = "unknown"
    """No charge information; never counted as zero."""


class LLMPurpose(StrEnum):
    PLAN = "plan"
    READ = "read"
    FIELD_TEXT = "field_text"
    RECOVER = "recover"
    VERIFY = "verify"
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
    page_changed: bool | None = None
    """Whether the page's fingerprint changed; None for steps that do not act (read, escalate)."""
    duration_ms: int = Field(ge=0)


class RunResult(Frozen):
    status: Status
    answer: str | None
    data: JsonValue | None
    evidence: tuple[Evidence, ...]
    steps: tuple[StepResult, ...]
    cost: CostBreakdown
    artifacts: tuple[Artifact, ...]
    error: str | None = None
    final_url: str | None = None
    """Where the browser was last observed; what a caller checks when the task was to arrive somewhere."""

    @property
    def succeeded(self) -> bool:
        return self.status is Status.COMPLETE


class BrowserConnection(Frozen):
    cdp_url: str
    """Browser-level DevTools WebSocket URL (`ws://` or `wss://`)."""
    live_url: str | None = None
    remote: bool
    """True when the browser runs on another host: tabs must be foregrounded and files move as bytes."""


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
    max_steps: int = Field(default=60, gt=0)
    max_jev_calls: int = Field(default=150, gt=0)
    """Logical Jev evaluations. A hedged or retried request adds cost but not a call."""
    max_llm_calls: int = Field(default=40, gt=0)
    max_dollars: float | None = Field(default=None, gt=0)
    """Bounds Jev and LLM spend as it happens. A cloud browser bills when it stops, after the run, so its
    cost is reported in the result but cannot stop the run that incurred it."""
    max_seconds: float | None = Field(default=None, gt=0)


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


class StepEvent(Frozen):
    type: Literal["step"] = "step"
    step: StepResult


class BrowserEvent(Frozen):
    """Sent once, when the browser is open and before the first step."""

    type: Literal["browser"] = "browser"
    live_url: str | None
    """Where a cloud browser can be watched live; None for local Chrome."""


type EventHandler = Callable[[StepEvent | BrowserEvent], Awaitable[None]]
type UntilCheck = Callable[[str], Awaitable[bool]]
"""Caller assertion over the final page URL; COMPLETE requires it to return True when supplied."""
