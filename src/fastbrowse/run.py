"""Run one task end to end, or drive a page without the agent, on the Rust core in this process.

This is the entry point for embedding fastbrowse in something else. Every argument crosses to the core
(`fastbrowse.native`), which assembles the browser, the model clients and the agent, folds the cloud
browser's own cost into the result, keeps downloads when a directory is given, and closes owned tabs on
every path out.
"""

from collections.abc import AsyncGenerator, Awaitable, Callable, Mapping, Sequence
from contextlib import asynccontextmanager
from pathlib import Path

import httpx
from pydantic import BaseModel

from fastbrowse import native
from fastbrowse.config import Config
from fastbrowse.jev import JevClient
from fastbrowse.llm import LLMClient
from fastbrowse.models import (
    ArtifactSink,
    Attachment,
    Authorization,
    EventHandler,
    FrameHandler,
    Limits,
    LocalChrome,
    RunResult,
    SecretResolver,
    UntilCheck,
)


@asynccontextmanager
async def connect_cdp(
    port: int | None = None,
    *,
    cdp_url: str | None = None,
    host: str = "127.0.0.1",
    target_match: str | None = None,
    attach: bool = True,
    foreground: bool = False,
    allowed_origins: Sequence[str] | None = None,
    check_access: Callable[[], Awaitable[None]] | None = None,
    config: Config | None = None,
    artifact_sink: ArtifactSink | None = None,
    downloads: Path | None = None,
) -> AsyncGenerator[native.NativePage]:
    """A page on a browser or Electron app that is already running, for driving it without the agent.

    Pass `port` to look the websocket URL up from `http://<host>:<port>/json/version`, or `cdp_url` directly. By default
    the page is an existing window (the first whose title or URL contains `target_match`, if given), and it is left open
    on exit; with `allowed_origins`, no `target_match` and no page inside the grant, it is an empty `about:blank` window
    no page opened, else a new blank tab, also left open. The window is neither raised nor focused unless `foreground`
    is set, though Chrome itself activates a visible window whose front tab receives a pointer press. `allowed_origins`
    limits the page to those exact origins and `check_access` is awaited before every browser read and action (both
    described on `run_task`). Downloads go to `artifact_sink`, else to `downloads`, else to a scratch directory removed
    on exit. The page is the Rust core's (`fastbrowse.native.NativePage`).
    """
    async with native.connect_cdp(locals()) as page:
        yield page


async def run_task(
    task: str,
    *,
    start: str | None = None,
    context: str | None = None,
    browser_api_key: str | None = None,
    chrome: LocalChrome | None = None,
    cloud_profile: str | None = None,
    cdp_url: str | None = None,
    cdp_port: int | None = None,
    attach: bool = False,
    target_match: str | None = None,
    foreground: bool = False,
    allowed_origins: Sequence[str] | None = None,
    check_access: Callable[[], Awaitable[None]] | None = None,
    proxy_country: str | None = None,
    viewport: tuple[int, int] | None = None,
    cloud_extensions: Sequence[str] = (),
    cloud_timeout_minutes: int | None = None,
    jev: JevClient | None = None,
    llm: LLMClient | None = None,
    output_schema: type[BaseModel] | None = None,
    inputs: Mapping[str, str] | None = None,
    attachments: Sequence[Attachment] = (),
    limits: Limits | None = None,
    authorization: Authorization | None = None,
    secrets: SecretResolver | None = None,
    downloads: Path | None = None,
    on_event: EventHandler | None = None,
    on_frame: FrameHandler | None = None,
    until: UntilCheck | None = None,
    config: Config | None = None,
    http: httpx.AsyncClient | None = None,
    record: Path | None = None,
    cursor: bool = False,
    fetch: bool = False,
    fetch_first: bool = False,
    settings: Mapping[str, str] | None = None,
) -> RunResult:
    """Open `start`, pursue `task`, and return what the run could prove.

    `start` may be omitted, for a caller whose own interface takes a goal and no URL: the first address is
    then proposed from the task and the run begins there, which is what a person does with the same sentence.

    The browser is one of three. `cdp_url` attaches to a browser that is already running, wherever it is
    (a container, a VM, a machine the caller owns), and the run neither starts nor stops it: it opens a tab
    and closes the tabs it owns. Cookies and task changes can persist. `cdp_url` and `browser_api_key` cannot
    be combined. `cdp_port` is the same as `cdp_url`, for a browser or Electron app started with
    `--remote-debugging-port`, its URL read from `http://127.0.0.1:<port>/json/version`. With `attach` (implied by
    `target_match`) the run drives a window already open instead of opening a tab: the first page whose title or URL
    contains `target_match`, or else the first page; a scoped run with neither a `target_match` nor a page inside
    `allowed_origins`, such as a fresh browser on `about:blank`, takes over that empty window, or opens a blank tab when
    there is none. It leaves that window open, and with no `start` it begins on whatever the window shows. New windows
    that no page opened, as an Electron app's main process opens them, join the run only with `target_match`; without it
    they could be tabs a person opened in the same browser.
    A run in a visible window (a handed-over browser, or local Chrome with `LocalChrome.headed`) works in the
    background: its tab opens behind the window's current one and the window is neither raised nor focused.
    `foreground` brings the tab and its window to the front instead, to watch the run.
    Otherwise the run starts a Browser Use Cloud browser with `browser_api_key`, or `BROWSER_USE_API_KEY` from the
    environment or `.env`. A `chrome` argument runs local Chrome as it describes instead, as do `FASTBROWSE_HEADED` and
    `FASTBROWSE_PROFILE`; `LocalChrome()` is headless with a throwaway profile.
    `cloud_profile` names a profile on that cloud account, so a site someone signed into once in that
    profile is still signed in here; it is the remote counterpart of `LocalChrome.profile`. `proxy_country`
    and `viewport` shape a cloud browser this run starts: `viewport` is its real screen size. `cloud_extensions`
    lists up to three distinct extension IDs (UUIDs of ready extensions on that account) to load into the cloud
    browser; it is an error with a local or attached browser rather than ignored. `cloud_timeout_minutes` is how
    long that browser may live (15 by default; the service allows up to 240), so a run that needs longer asks for it
    rather than losing its browser. The other cloud options mean nothing for the other two browsers. `jev` and
    `llm` default to clients the Rust core builds from the environment, then `.env`, so an embedder that
    resolves its own credentials, or serves Jev from somewhere else, passes them instead.

    `allowed_origins` limits the documents the run may inspect and control to those exact `http(s)://host[:port]`
    origins (no wildcards, paths or userinfo; an empty list is an error, and `None` is unscoped). A navigation,
    redirect, frame or popup outside them is refused before it is sent, and a page or frame already open outside
    them is never read, captured or screenshotted. It is a document grant, not network egress: images, scripts and
    requests a granted page makes to other hosts still load, and a sign-in that redirects through another origin
    needs that origin listed too. Service workers are bypassed so every navigation meets the gate. A scoped run
    delivers no live frames or screenshots and cannot `record`, since pixels cannot be attributed to a document.

    `check_access` is awaited before browser startup and every observation, capture, screenshot, address, navigation
    and action, scoped or not. If it raises, `run_task` returns a `RunResult` with status `error` and `connect_cdp`
    raises a `BrowserError`; either error names only the exception's type, and its text is not kept.

    Files the run downloads are discarded unless `downloads` names a directory to keep them in. `record` saves
    an MP4 of the tab, ending on the answer; it needs ffmpeg, and shows whatever the pages showed.
    `on_frame` receives JPEG bytes from the active tab. Frames are acknowledged after delivery, with no fixed
    frame rate; only the latest pending frame is kept. Handler failures are logged without interrupting the run.
    A handler still running when the run ends is cancelled, and the run waits at most two seconds for it to stop.
    A handler must honour cancellation: it runs on the caller's event loop, so one that catches `CancelledError`
    without re-raising keeps that loop alive, and `asyncio.run` then waits for it at shutdown. Do not block the
    loop either, since delivery and the run share it.
    Live frames and recordings are held back while a resolved secret shows on the page, as PNG step frames are.
    No handler means no live capture.

    `cursor` draws the agent's cursor over a visible local or attached Chrome through the Cua Driver, on Linux
    with X11; it never moves the real pointer or changes focus, and does nothing where it cannot place the cursor
    exactly, including with no `cua-driver` installed, no display, a headless or cloud browser. It implies
    `foreground`, since the cursor lies over the window's front tab.

    `fetch` reads every page over HTTP and starts no browser at all, for a task that only has to read: the
    document becomes the same `Capture` blocks a browser capture produces, so reads, quotes and citations
    are unchanged. A step that would click, fill, download or look at pixels has nothing to act on, so the
    run ends `blocked` with the reason instead of failing a step or answering from a page it never saw. What
    a page builds with scripts is not there either: a document with no readable text in its markup is refused
    as needing a browser. Browser, recording, download and cursor options mean nothing here and are an error
    rather than ignored.

    `fetch_first` reads the same way, and when the fetched run ends `blocked` because the page refused
    something only a browser can do, runs the task again on the browser the other options describe. That run
    spends what the attempt left of `limits`, and its cost lists the attempt's calls first. A recording or an
    attached window has to exist from the first step, so `record`, `attach` and `target_match` are an error.

    `context` is background the run may use but never reads as the task: where the request came from, what the
    caller already knows. The planner, recovery and the answer see it in its own section headed as not
    requirements, and Jev sees it as `background`; requirements come from `task` alone.

    `settings` configures the run by the environment variables' own names (`OPENROUTER_API_KEY`,
    `AI_GATEWAY_API_KEY`, `TYPESAFE_API_KEY`, `FASTBROWSE_DECISION_SOURCE`, `FASTBROWSE_LLM_PROVIDER`,
    `FASTBROWSE_LLM_MODEL` and the rest), read instead of the environment and `.env`: neither is consulted, so a
    key the process happens to hold reaches no provider it was not given for. It is for an embedder that keeps its
    credentials elsewhere and decides the model backend itself; settings that do not configure a backend raise
    `ConfigurationError`, as an unconfigured environment does. `jev` and `llm`, when given, still win.

    The run goes to the Rust core in this process, every argument with it (`fastbrowse.native`).
    """
    return await native.run(task, locals())
