"""Browser outcome contracts shared by fixture and publication checks.

Maintainer benchmark tasks and orchestration live in Parallax.
"""

from collections.abc import Awaitable, Callable, Mapping
from dataclasses import dataclass, field
from enum import StrEnum

import httpx
from pydantic import BaseModel

from fastbrowse.models import Status


class Category(StrEnum):
    LOOKUP = "lookup"
    LOGIN = "login"
    CHECKOUT = "checkout"
    SAFETY = "safety"
    WIDGET = "widget"
    NAVIGATE = "navigate"


class Release(BaseModel):
    package: str
    version: str


class Newer(BaseModel):
    package: str


class PageEvidence(BaseModel):
    """What the final document was, read before the harness touches the page: a URL alone cannot tell an article from
    the HTTP 419 error page served at the same address."""

    status: int | None = None
    """The navigation response's HTTP status; None when the browser did not report one, which is not a pass."""
    title: str | None = None
    text: str | None = None
    """The first characters of the visible text, enough to see an error page; `text_length` is the whole."""
    text_length: int | None = None
    inner_width: int | None = None
    inner_height: int | None = None
    device_pixel_ratio: float | None = None
    """The CSS viewport the document was laid out in, recorded for every arm: a page that reflows at another width is
    a different page, so rows whose viewports differ are not comparable."""


def page_defect(evidence: PageEvidence | None) -> str | None:
    """Why a navigation task's final page is no evidence of arrival; missing evidence is a defect, not a pass."""
    if evidence is None:
        return "no final page evidence"
    if evidence.status is None or evidence.status < 200:
        return "final page status not observed"
    if evidence.status >= 400:
        return f"final page is an HTTP {evidence.status} document"
    if not evidence.text_length and not (evidence.title or "").strip():
        return "final page is empty"
    return None


@dataclass(frozen=True, slots=True)
class Outcome:
    answer: str | None
    data: object
    final_url: str | None
    """Where the browser ended; the Browser Use agent's SDK does not say, so it is None there."""
    quotes: tuple[tuple[str, str], ...] | None = None
    """(url, quote) pairs located verbatim in page captures; the fastbrowse arm's evidence, None for hosted."""
    controls: tuple[tuple[str, str | None], ...] | None = None
    """(label, value) of every control on the page the run ended on, observed after the run by the harness, not
    reported by the agent; None where an arm has no final page."""
    evidence: PageEvidence | None = None
    """Status, title and text of the final document, observed by the harness; a navigation task fails without it."""
    unobservable: bool = False
    """The arm cannot say where its browser ended (the hosted Browser Use SDK), so an answer task's page half is
    not graded for it. Every other arm's final page is observed by the harness, and a missing one fails."""


type Truth = Callable[[httpx.AsyncClient], Awaitable[object]]
type Check = Callable[[Outcome, object], str | None]


@dataclass(frozen=True, slots=True)
class LiveTask:
    id: str
    start: str
    task: str
    truth: Truth
    check: Check
    category: Category
    secrets: Mapping[str, str] = field(default_factory=dict[str, str])
    bitwarden_item: str | None = None
    """The vault item holding `secrets` as username and password; see the private Parallax eval vault."""
    output_schema: type[BaseModel] | None = None
    authorize: bool = False
    expect: Status = Status.COMPLETE
    """The fastbrowse arm's required status: a safety task passes only by stopping."""
    arms: tuple[str, ...] = ("fastbrowse", "browser-use")
    """The arms the task can grade on equal terms. An answer task leaves out jev-ultrafast, which returns no answer; a
    navigation task, graded on the page the run ended on, leaves out the Browser Use agent, whose SDK does not say
    where its browser ended; a safety task needs the pause before irreversible actions only fastbrowse has."""
    rolling: Mapping[str, str] = field(default_factory=dict[str, str])
    """Text that changes from run to run by design, such as a date four weeks out, and the name its version
    fingerprint uses in its place, so the task keeps its version from one day to the next."""


def prompt(task: LiveTask) -> str:
    """What every arm is asked; part of each task's version fingerprint, since it is what the task asks."""
    return f"Start at {task.start}. {task.task}"
