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
    if mode == "browser-use-cloud":
        return None
    if mode != "local-chrome":
        return f"browser mode {mode!r} is not one public-site runs are admitted in (local-chrome, browser-use-cloud)"
    if browser.get("headed") is True or browser.get("profile") is True:
        return None
    return (
        "headless local Chrome with a throwaway profile is refused by public benchmark sites; "
        "run headed, with a kept profile, or in a cloud browser"
    )


class PublicSiteRefused(RuntimeError):
    """A public-site run the campaign must not start: its rows could never be admitted, so its spend is waste."""


def admit_public_run(browser: object) -> None:
    """Refuse to start a public-site run in a browser that the publication rule would refuse later.

    `browser_refusal` is the rule; this is the same rule at the only point where refusing it saves anything,
    before the first step. A campaign calls this once per run, with the same browser record it will publish,
    so a headless throwaway-profile run against a public site fails immediately instead of minting rows that
    can never be admitted (issue #389: `pypi-newer` was `blocked` on three repeats of three, each row costing
    its whole budget to produce).
    """
    if (refusal := browser_refusal(browser)) is not None:
        raise PublicSiteRefused(refusal)


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


ARM_BROWSERS = {
    "fastbrowse": ("browser-use-cloud", "local-chrome"),
    "fastbrowse-pool": ("fastbrowse-pool",),
    "jev-ultrafast": ("browser-use-cloud",),
    "browser-use": ("browser-use-hosted",),
}
"""The browsers each arm is measured in. The pool and Browser Use's hosted browser are each the one their arm sells,
so a pool row's block rate is part of what the backend comparison measures; binding the mode to the arm keeps a
swapped record from publishing one backend under another's name."""


def arm_browser_refusal(run: Mapping[str, Any], arm: object) -> str | None:
    """Why `arm`'s row of `run` must not be published for the browser it ran in; None when it may.

    A campaign records one browser per arm (`browsers`), and each arm is judged by its own. An older campaign
    recorded a single `browser` for every arm, which only `browser_refusal` can judge."""
    browsers = run.get("browsers")
    if not isinstance(browsers, Mapping):
        return browser_refusal(run.get("browser"))
    browser = browsers.get(arm)
    if not isinstance(browser, Mapping):
        return f"no browser record for {arm}; a public-site run must record the browser each arm ran in"
    admitted = ARM_BROWSERS.get(str(arm), ())
    if browser.get("mode") not in admitted:
        named = ", ".join(admitted) or "no browser this build knows"
        return f"{arm} ran in browser mode {browser.get('mode')!r}; it is measured only in {named}"
    return browser_refusal(browser) if browser["mode"] in ("browser-use-cloud", "local-chrome") else None
