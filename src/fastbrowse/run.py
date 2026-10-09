"""Run one task end to end: assemble a browser, the model clients and the agent, then return the result.

This is the entry point for embedding fastbrowse in something else. The terminal (`fastbrowse.cli`) and
the live eval are both callers of it, so the assembly has one definition rather than one per caller, and
an embedder gets the parts that are easy to forget: the cloud browser's own cost folded into the result,
downloads kept when a directory is given, and owned tabs closed on every path out.
"""

from collections.abc import AsyncGenerator, Awaitable, Callable, Mapping, Sequence
from contextlib import asynccontextmanager, nullcontext
from pathlib import Path
from tempfile import TemporaryDirectory

import httpx
from pydantic import BaseModel

from fastbrowse.adapters.browser_use_cloud import BrowserUseCloudBrowser
from fastbrowse.adapters.local_chrome import async_local_chrome
from fastbrowse.agent import Agent, HeadStart
from fastbrowse.artifacts import DirectorySink
from fastbrowse.browser import BrowserSession, CdpPage
from fastbrowse.browser.recording import Recording
from fastbrowse.browser.session import check_browser_access
from fastbrowse.clients.environment import load_settings
from fastbrowse.config import Config
from fastbrowse.jev import JevClient
from fastbrowse.llm import LLMClient
from fastbrowse.models import (
    ArtifactSink,
    Attachment,
    Authorization,
    BrowserConnection,
    BrowserEvent,
    CostBreakdown,
    CostLine,
    EventHandler,
    FrameHandler,
    Limits,
    LocalChrome,
    RunResult,
    SecretResolver,
    Status,
    StepEvent,
    Unavailable,
    UntilCheck,
)
from fastbrowse.origins import parse_origins
from fastbrowse.page import BrowserError


async def resolve_cdp_port(port: int, host: str = "127.0.0.1", *, http: httpx.AsyncClient | None = None) -> str:
    """The websocket URL of the browser serving DevTools on `port`, read from its `/json/version` endpoint."""
    endpoint = f"http://{host}:{port}/json/version"
    try:
        async with httpx.AsyncClient(timeout=5) if http is None else _borrowed(http) as client:
            response = await client.get(endpoint)
            response.raise_for_status()
            version = response.json()
    except (httpx.HTTPError, ValueError) as exc:
        raise BrowserError(f"no DevTools endpoint at {endpoint} ({type(exc).__name__})") from None
    ws_url = version.get("webSocketDebuggerUrl") if isinstance(version, dict) else None
    if not isinstance(ws_url, str) or not ws_url:
        raise BrowserError(f"{endpoint} named no webSocketDebuggerUrl")
    return ws_url


@asynccontextmanager
async def connect_cdp(
    port: int | None = None,
    *,
    cdp_url: str | None = None,
    host: str = "127.0.0.1",
    target_match: str | None = None,
    attach: bool = True,
    allowed_origins: Sequence[str] | None = None,
    check_access: Callable[[], Awaitable[None]] | None = None,
    config: Config | None = None,
    artifact_sink: ArtifactSink | None = None,
    downloads: Path | None = None,
) -> AsyncGenerator[CdpPage]:
    """A page on a browser or Electron app that is already running, for driving it without the agent.

    Pass `port` to look the websocket URL up from `http://<host>:<port>/json/version`, or `cdp_url` directly.
    By default the page is an existing window (the first whose title or URL contains `target_match`, if given),
    and it is left open on exit. `allowed_origins` limits the page to those exact origins and `check_access` is
    awaited before every browser read and action (both described on `run_task`). Downloads go to `artifact_sink`,
    else to `downloads`, else to a scratch directory removed on exit.
    """
    await check_browser_access(check_access)
    if port is not None and cdp_url is not None:
        raise BrowserError("port and cdp_url both name a browser to attach to; pass one")
    if cdp_url is None:
        if port is None:
            raise BrowserError("connect_cdp needs a port or a cdp_url")
        cdp_url = await resolve_cdp_port(port, host)
    connection = BrowserConnection(
        cdp_url=cdp_url,
        live_url=None,
        remote=True,
        attach=attach or target_match is not None,
        target_match=target_match,
        allowed_origins=None if allowed_origins is None else tuple(allowed_origins),
    )
    config = config or Config()
    with TemporaryDirectory() as scratch:
        sink = artifact_sink or DirectorySink(downloads or Path(scratch))
        async with BrowserSession(
            connection, sink, refuse_cookie_banners=config.refuse_cookie_banners, check_access=check_access
        ) as session:
            yield CdpPage(session, config)


@asynccontextmanager
async def _browser(
    key: str | None,
    chrome: LocalChrome,
    http: httpx.AsyncClient,
    cost: list[CostLine],
    *,
    profile: str | None = None,
    cdp_url: str | None = None,
    cdp_port: int | None = None,
    attach: bool = False,
    target_match: str | None = None,
    proxy_country: str | None = "us",
    viewport: tuple[int, int] | None = None,
    allow_resizing: bool = False,
    cloud_extensions: Sequence[str] = (),
) -> AsyncGenerator[BrowserConnection]:
    """The browser a run drives: one it is handed, a cloud browser, or local Chrome."""
    if cloud_extensions and (key is None or cdp_url is not None or cdp_port is not None):
        raise BrowserError("cloud_extensions load into a Browser Use Cloud browser this run starts, which needs a key")
    if cdp_url is not None and cdp_port is not None:
        raise BrowserError("cdp_url and cdp_port both name a browser to attach to; pass one")
    if cdp_url is not None or cdp_port is not None:
        if key is not None:
            raise BrowserError("cdp_url/cdp_port is a browser to attach to; a cloud key would start a second one")
        if profile is not None:
            raise BrowserError("cloud_profile belongs to a browser fastbrowse starts, not to one it is handed")
        if cdp_url is None:
            assert cdp_port is not None
            cdp_url = await resolve_cdp_port(cdp_port, http=http)
        # Nothing to start and nothing to stop: the caller's browser outlives the run. The session either opens
        # its own tab and closes only that, or, attaching, drives a window already open and leaves it open.
        yield BrowserConnection(
            cdp_url=cdp_url,
            live_url=None,
            remote=True,
            attach=attach or (target_match is not None),
            target_match=target_match,
        )
        return
    if attach or target_match is not None:
        raise BrowserError("attach and target_match require cdp_url or cdp_port")
    if key is None:
        if profile is not None:
            raise BrowserError("cloud_profile names a Browser Use Cloud profile, which needs a cloud browser")
        async with async_local_chrome(chrome) as connection:
            yield connection
        return
    remote = BrowserUseCloudBrowser(
        key,
        http=http,
        profile=profile,
        proxy_country=proxy_country,
        viewport=viewport,
        allow_resizing=allow_resizing,
        extensions=cloud_extensions,
    )
    try:
        async with remote:
            yield remote.connection
    finally:
        # Keep the last reported cost even when setup or teardown fails.
        cost.extend(remote.cost)


async def run_task(
    task: str,
    *,
    start: str | None = None,
    browser_api_key: str | None = None,
    chrome: LocalChrome | None = None,
    cloud_profile: str | None = None,
    cdp_url: str | None = None,
    cdp_port: int | None = None,
    attach: bool = False,
    target_match: str | None = None,
    allowed_origins: Sequence[str] | None = None,
    check_access: Callable[[], Awaitable[None]] | None = None,
    proxy_country: str | None = "us",
    viewport: tuple[int, int] | None = None,
    cloud_allow_resizing: bool = False,
    cloud_extensions: Sequence[str] = (),
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
) -> RunResult:
    """Open `start`, pursue `task`, and return what the run could prove.

    `start` may be omitted, for a caller whose own interface takes a goal and no URL: the first address is
    then proposed from the task and the run begins there, which is what a person does with the same sentence.

    The browser is one of three. `cdp_url` attaches to a browser that is already running, wherever it is
    (a container, a VM, a machine the caller owns), and the run neither starts nor stops it: it opens a tab
    and closes the tabs it owns. Cookies and task changes can persist. `cdp_url` and `browser_api_key` cannot
    be combined. `cdp_port` is the same as `cdp_url`, for a browser or Electron app started with
    `--remote-debugging-port`, its URL read from `http://127.0.0.1:<port>/json/version`. With `attach` (implied by
    `target_match`) the run drives a window already open instead of opening a tab: the first page whose title or
    URL contains `target_match`, or else the first page. It leaves that window open, and with no `start` it begins
    on whatever the window shows. New windows that no page opened, as an Electron app's main process opens them,
    join the run only with `target_match`; without it they could be tabs a person opened in the same browser.
    Otherwise `browser_api_key` runs on a Browser Use Cloud browser, and with neither,
    local Chrome as `chrome` describes (default: from `Settings`, headless with a throwaway profile).
    `cloud_profile` names a profile on that cloud account, so a site someone signed into once in that
    profile is still signed in here; it is the remote counterpart of `LocalChrome.profile`. `proxy_country`
    and `viewport` shape a cloud browser this run starts. `cloud_allow_resizing` opts that browser into
    CDP viewport changes, which the cloud service otherwise ignores. `cloud_extensions` lists up to three
    distinct extension IDs (UUIDs of ready extensions on that account) to load into the cloud browser; it is
    an error with a local or attached browser rather than ignored. The other cloud options mean nothing for
    the other two browsers. `jev` and
    `llm` default to clients built from `Settings` (the environment, then `.env`), so an embedder that
    resolves its own credentials, or serves Jev from somewhere else, passes them instead.

    `allowed_origins` limits the documents the run may inspect and control to those exact `http(s)://host[:port]`
    origins (no wildcards, paths or userinfo; an empty list is an error, and `None` is unscoped). A navigation,
    redirect, frame or popup outside them is refused before it is sent, and a page or frame already open outside
    them is never read, captured or screenshotted. It is a document grant, not network egress: images, scripts and
    requests a granted page makes to other hosts still load, and a sign-in that redirects through another origin
    needs that origin listed too. Service workers are bypassed so every navigation meets the gate. A scoped run
    delivers no live frames or screenshots and cannot `record`, since pixels cannot be attributed to a document.

    `check_access` is awaited before browser startup and every observation, capture, screenshot, address, navigation
    and action, scoped or not. If it raises, the run stops with a `BrowserError` and the exception's text is not kept.

    Files the run downloads are discarded unless `downloads` names a directory to keep them in. `record` saves
    an MP4 of the tab, ending on the answer; it needs ffmpeg, and shows whatever the pages showed.
    `on_frame` receives JPEG bytes from the active tab. Frames are acknowledged after delivery, with no fixed
    frame rate; only the latest pending frame is kept. Handler failures are logged without interrupting the run.
    Live frames and recordings are held back while a resolved secret shows on the page, as PNG step frames are.
    No handler means no live capture.
    """
    config = config or Config()
    settings = load_settings()
    origins = None if allowed_origins is None else parse_origins(allowed_origins)
    if origins is not None and record is not None:
        raise ValueError("record shows whatever the tab shows, so it cannot be combined with allowed_origins")
    browser_cost: list[CostLine] = []
    with TemporaryDirectory() as scratch:
        sink = DirectorySink(downloads or Path(scratch))
        async with httpx.AsyncClient(timeout=60) if http is None else _borrowed(http) as client:
            jev = jev or settings.jev(client)
            llm = llm or settings.llm(client)
            session: BrowserSession | None = None
            result: RunResult | None = None
            recording: Recording | None = None
            # Begun before the browser, which takes 2 to 3 seconds to start and open a tab, so the shortcut is
            # ready to open when the tab is.
            head = HeadStart.begin(llm, task, start=start, limits=limits)
            try:
                await check_browser_access(check_access)
                async with _browser(
                    browser_api_key,
                    chrome or settings.local_chrome(),
                    client,
                    browser_cost,
                    profile=cloud_profile,
                    cdp_url=cdp_url,
                    cdp_port=cdp_port,
                    attach=attach,
                    target_match=target_match,
                    proxy_country=proxy_country,
                    viewport=viewport,
                    allow_resizing=cloud_allow_resizing,
                    cloud_extensions=cloud_extensions,
                ) as connection:
                    connection = connection.model_copy(update={"allowed_origins": origins})
                    if on_event is not None:
                        await on_event(BrowserEvent(live_url=connection.live_url, browser_id=connection.browser_id))
                    session = BrowserSession(
                        connection,
                        sink,
                        refuse_cookie_banners=config.refuse_cookie_banners,
                        on_frame=on_frame,
                        check_access=check_access,
                    )
                    async with session:
                        page = CdpPage(session, config)
                        async with nullcontext() if record is None else Recording(session, record) as recording:
                            agent = Agent(
                                page, jev, llm, config=config, secrets=secrets, on_event=_captioned(on_event, recording)
                            )
                            result = await agent.run(
                                task,
                                start=start,
                                # With no page named, the first address is worked out from the task,
                                # unless attaching to an existing window where the page is preserved.
                                choose_start=start is None and not connection.attach,
                                output_schema=output_schema,
                                inputs=inputs,
                                attachments=attachments,
                                authorization=authorization,
                                until=until,
                                head_start=head,
                            )
                            # The card is a data: document navigated into the tab, which would replace the
                            # caller's own app page; an attached window is theirs and stays where it is.
                            if recording is not None and not connection.attach:
                                await recording.show_result(task, result)
            except (BrowserError, Unavailable) as exc:
                # A cloud browser that cannot be started is an outage, not a failed run.
                status = Status.UNAVAILABLE if isinstance(exc, Unavailable) else Status.ERROR
                result = result or RunResult(
                    status=status,
                    answer=None,
                    data=None,
                    evidence=(),
                    steps=(),
                    # The plan and shortcut were asked for before the browser failed, and what they spent is real.
                    cost=CostBreakdown(lines=await head.abandon()),
                    artifacts=session.artifacts if session is not None else (),
                )
                # `budget` names the limit a budget_exceeded run hit, and means nothing under another status.
                result = result.model_copy(update={"status": status, "error": str(exc), "budget": None})
            finally:
                # A run that never began leaves its head start running; one that did has discarded it already.
                await head.discard()
            # Read after the browser closes either way: a failed result card still leaves the finished videos.
            if recording is not None:
                result = result.model_copy(update={"recordings": recording.outputs})
    assert result is not None
    return result.model_copy(update={"cost": CostBreakdown(lines=(*result.cost.lines, *browser_cost))})


def _captioned(on_event: EventHandler | None, recording: Recording | None) -> EventHandler | None:
    """Pass each step to the recording as well, which captions it."""
    if recording is None:
        return on_event

    async def handle(event: StepEvent | BrowserEvent) -> None:
        if isinstance(event, StepEvent):
            recording.caption(event.step)
        if on_event is not None:
            await on_event(event)

    return handle


@asynccontextmanager
async def _borrowed(http: httpx.AsyncClient) -> AsyncGenerator[httpx.AsyncClient]:
    """A caller's client outlives the run, so hand it back rather than closing it."""
    yield http
