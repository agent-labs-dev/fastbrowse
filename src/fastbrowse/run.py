"""Run one task end to end: assemble a browser, the model clients and the agent, then return the result.

This is the entry point for embedding fastbrowse in something else. The terminal (`fastbrowse.cli`) and
the live eval are both callers of it, so the assembly has one definition rather than one per caller, and
an embedder gets the parts that are easy to forget: the cloud browser's own cost folded into the result,
downloads kept when a directory is given, and owned tabs closed on every path out.
"""

from collections.abc import AsyncGenerator, Mapping, Sequence
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
from fastbrowse.clients.environment import load_settings
from fastbrowse.config import Config
from fastbrowse.jev import JevClient
from fastbrowse.llm import LLMClient
from fastbrowse.models import (
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
from fastbrowse.page import BrowserError


@asynccontextmanager
async def _browser(
    key: str | None,
    chrome: LocalChrome,
    http: httpx.AsyncClient,
    cost: list[CostLine],
    *,
    profile: str | None = None,
    cdp_url: str | None = None,
    proxy_country: str | None = "us",
    viewport: tuple[int, int] | None = None,
) -> AsyncGenerator[BrowserConnection]:
    """The browser a run drives: one it is handed, a cloud browser, or local Chrome."""
    if cdp_url is not None:
        if key is not None:
            raise BrowserError("cdp_url is a browser to attach to; a cloud key would start a second one")
        if profile is not None:
            raise BrowserError("cloud_profile belongs to a browser fastbrowse starts, not to one it is handed")
        # Nothing to start and nothing to stop: the caller's browser outlives the run. The session opens its
        # own tab and closes only that, so a browser handed over is left exactly as it was found.
        yield BrowserConnection(cdp_url=cdp_url, live_url=None, remote=True)
        return
    if key is None:
        if profile is not None:
            raise BrowserError("cloud_profile names a Browser Use Cloud profile, which needs a cloud browser")
        async with async_local_chrome(chrome) as connection:
            yield connection
        return
    remote = BrowserUseCloudBrowser(key, http=http, profile=profile, proxy_country=proxy_country, viewport=viewport)
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
    proxy_country: str | None = "us",
    viewport: tuple[int, int] | None = None,
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
    be combined. Otherwise `browser_api_key` runs on a Browser Use Cloud browser, and with neither,
    local Chrome as `chrome` describes (default: from `Settings`, headless with a throwaway profile).
    `cloud_profile` names a profile on that cloud account, so a site someone signed into once in that
    profile is still signed in here; it is the remote counterpart of `LocalChrome.profile`. `proxy_country`
    and `viewport` shape a cloud browser this run starts, and mean nothing for the other two. `jev` and
    `llm` default to clients built from `Settings` (the environment, then `.env`), so an embedder that
    resolves its own credentials, or serves Jev from somewhere else, passes them instead.

    Files the run downloads are discarded unless `downloads` names a directory to keep them in. `record` saves
    an MP4 of the tab, ending on the answer; it needs ffmpeg, and shows whatever the pages showed.
    `on_frame` receives JPEG bytes from the active tab. Frames are acknowledged after delivery, with no fixed
    frame rate; only the latest pending frame is kept. Handler failures are logged without interrupting the run.
    Live frames and recordings are held back while a resolved secret shows on the page, as PNG step frames are.
    No handler means no live capture.
    """
    config = config or Config()
    settings = load_settings()
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
                async with _browser(
                    browser_api_key,
                    chrome or settings.local_chrome(),
                    client,
                    browser_cost,
                    profile=cloud_profile,
                    cdp_url=cdp_url,
                    proxy_country=proxy_country,
                    viewport=viewport,
                ) as connection:
                    if on_event is not None:
                        await on_event(BrowserEvent(live_url=connection.live_url, browser_id=connection.browser_id))
                    session = BrowserSession(
                        connection, sink, refuse_cookie_banners=config.refuse_cookie_banners, on_frame=on_frame
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
                                # With no page named, the first address is worked out from the task. That
                                # holds for an attached browser too: the run opens its own tab rather than
                                # taking over one already open, so there is no page it is "already on".
                                choose_start=start is None,
                                output_schema=output_schema,
                                inputs=inputs,
                                attachments=attachments,
                                authorization=authorization,
                                until=until,
                                head_start=head,
                            )
                            if recording is not None:
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
                result = result.model_copy(update={"status": status, "error": str(exc)})
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
