"""Run one task end to end: assemble a browser, the model clients and the agent, then return the result.

This is the entry point for embedding fastbrowse in something else. The terminal (`fastbrowse.cli`) and
the live eval are both callers of it, so the assembly has one definition rather than one per caller, and
an embedder gets the parts that are easy to forget: the cloud browser's own cost folded into the result,
downloads kept when a directory is given, and the browser closed on every path out.
"""

from collections.abc import AsyncGenerator, Mapping, Sequence
from contextlib import asynccontextmanager, nullcontext
from pathlib import Path
from tempfile import TemporaryDirectory

import httpx
from pydantic import BaseModel

from fastbrowse.adapters.browser_use_cloud import BrowserUseCloudBrowser
from fastbrowse.adapters.local_chrome import async_local_chrome
from fastbrowse.agent import Agent
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
    Limits,
    LocalChrome,
    RunResult,
    SecretResolver,
    Status,
    UntilCheck,
)
from fastbrowse.page import BrowserError


@asynccontextmanager
async def _browser(
    key: str | None, chrome: LocalChrome, http: httpx.AsyncClient, cost: list[CostLine]
) -> AsyncGenerator[BrowserConnection]:
    """A cloud browser when a key is given, otherwise local Chrome."""
    if key is None:
        async with async_local_chrome(chrome) as connection:
            yield connection
        return
    remote = BrowserUseCloudBrowser(key, http=http)
    try:
        async with remote:
            yield remote.connection
    finally:
        # Keep the last reported cost even when setup or teardown fails.
        cost.extend(remote.cost)


async def run_task(
    task: str,
    *,
    start: str,
    browser_api_key: str | None = None,
    chrome: LocalChrome | None = None,
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
    until: UntilCheck | None = None,
    config: Config | None = None,
    http: httpx.AsyncClient | None = None,
    record: Path | None = None,
) -> RunResult:
    """Open `start`, pursue `task`, and return what the run could prove.

    `browser_api_key` picks the browser: a Browser Use Cloud key runs there, and None runs local
    Chrome as `chrome` describes (default: from `Settings`, headless with a throwaway profile). `jev` and
    `llm` default to clients built from `Settings` (the environment, then `.env`), so an embedder that
    resolves its own credentials, or serves Jev from somewhere else, passes them instead.

    Files the run downloads are discarded unless `downloads` names a directory to keep them in. `record` saves
    an MP4 of the tab, ending on the answer; it needs ffmpeg, and shows whatever the pages showed.
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
            try:
                async with _browser(
                    browser_api_key, chrome or settings.local_chrome(), client, browser_cost
                ) as connection:
                    if on_event is not None:
                        await on_event(BrowserEvent(live_url=connection.live_url))
                    session = BrowserSession(connection, sink, refuse_cookie_banners=config.refuse_cookie_banners)
                    async with session:
                        page = CdpPage(session, config)
                        agent = Agent(page, jev, llm, config=config, secrets=secrets, on_event=on_event)
                        async with nullcontext() if record is None else Recording(session, record) as recording:
                            result = await agent.run(
                                task,
                                start=start,
                                output_schema=output_schema,
                                inputs=inputs,
                                attachments=attachments,
                                limits=limits,
                                authorization=authorization,
                                until=until,
                            )
                            if recording is not None:
                                await recording.show_result(task, result)
            except BrowserError as exc:
                result = result or RunResult(
                    status=Status.ERROR,
                    answer=None,
                    data=None,
                    evidence=(),
                    steps=(),
                    cost=CostBreakdown(),
                    artifacts=session.artifacts if session is not None else (),
                )
                result = result.model_copy(update={"status": Status.ERROR, "error": str(exc)})
    assert result is not None
    return result.model_copy(update={"cost": CostBreakdown(lines=(*result.cost.lines, *browser_cost))})


@asynccontextmanager
async def _borrowed(http: httpx.AsyncClient) -> AsyncGenerator[httpx.AsyncClient]:
    """A caller's client outlives the run, so hand it back rather than closing it."""
    yield http
