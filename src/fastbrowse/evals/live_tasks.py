"""Browser outcome contracts shared by fixture and publication checks.

Maintainer benchmark tasks and orchestration live in Parallax.
"""

from collections.abc import Awaitable, Callable, Mapping
from dataclasses import dataclass, field
from enum import StrEnum
from typing import Any

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


def run_browser(run: Mapping[str, Any], arm: object) -> object:
    """The browser `arm` ran in: its entry in a campaign's per-arm `browsers` record, or the single `browser` an
    older campaign recorded for every arm."""
    browsers = run.get("browsers")
    if isinstance(browsers, Mapping):
        return browsers.get(arm)
    return run.get("browser")


_HOSTED_BROWSERS = ("browser-use-cloud", "browser-use-hosted", "fastbrowse-pool")
"""Browsers a provider runs, each the one its arm sells: Browser Use Cloud's, the browser Browser Use's hosted agent
drives, and the hosted fastbrowse pool. A pool row's block rate is part of what the backend comparison measures, so
it is admitted rather than refused."""


def browser_refusal(browser: object) -> str | None:
    """Why a benchmark run on public sites must not start, or be published, in `browser`; None when it may.

    `browser` is the run's browser as a campaign records it: `mode`, and for `local-chrome` whether it was
    `headed` and whether it kept a `profile`, both booleans. In headless Chrome with a throwaway profile about
    half of the public-site tasks ended `blocked` on an HTTP 403 or a bot check before their first step, against
    a few percent in a cloud browser (#317), so those rows measured the sites' refusal of that browser, not the
    agent. A local Chrome record that leaves either field out is read as Chrome's own default, headless with a
    throwaway profile, since that is what `run_task` starts when nobody asks for more. A missing, malformed or
    unknown record is refused too: otherwise dropping the record would publish the refused mode unseen.
    """
    if not isinstance(browser, Mapping):
        return "no browser record; a public-site run must record the browser it ran in"
    mode = browser.get("mode")
    if mode in _HOSTED_BROWSERS:
        return None
    if mode != "local-chrome":
        admitted = ", ".join(("local-chrome", *_HOSTED_BROWSERS))
        return f"browser mode {mode!r} is not one public-site runs are admitted in ({admitted})"
    if browser.get("headed") is True or browser.get("profile") is True:
        return None
    return (
        "headless local Chrome with a throwaway profile is refused by public benchmark sites; "
        "run headed, with a kept profile, or in a cloud browser"
    )


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
