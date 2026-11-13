"""The final page every arm's browser ended on, read by the harness after the run, and the viewport runs see.

What the judges grade beside an arm's own report: the address, the document's status, title and text, the viewport it
was laid out in, a PNG of its visible viewport, and the label and value of every control. The address, status and text
are read with this module's scripts; the controls are fastbrowse's own observation through its public `connect_cdp`,
the same reading
for every arm, taken with any modal's `aria-hidden` and `inert` lifted so a form behind it still shows its values.
"""

import contextlib
from collections.abc import AsyncIterator, Mapping, Sequence
from contextlib import suppress
from typing import Any, cast

from cdp_use.cdp.emulation.commands import SetDeviceMetricsOverrideParameters
from cdp_use.client import CDPClient
from fastbrowse import connect_cdp
from fastbrowse.config import Config
from fastbrowse.evals import observe
from pydantic import BaseModel

from fastbrowse_evals.live_tasks import PageEvidence

VIEWPORT = cast(SetDeviceMetricsOverrideParameters, observe.VIEWPORT)
"""The viewport every arm that can be given one runs at (fastbrowse.evals.observe)."""

# Read-only and run before UNHIDE, so the evidence is the page as the run left it. responseStatus is the document's own
# navigation response (0 or absent when the browser withholds it), so an error page served at the right address shows.
# The text is the whole document's: the judges read the page as the run left it, and no bound is put on it here.
EVIDENCE = """(() => {
  const nav = performance.getEntriesByType('navigation')[0];
  const text = (document.body?.innerText || '').trim();
  return {
    status: nav?.responseStatus || null, title: document.title || null,
    text, text_length: text.length,
    inner_width: window.innerWidth, inner_height: window.innerHeight, device_pixel_ratio: window.devicePixelRatio,
  };
})()"""
LOCATION = "location.href"
FRAME_VIEWPORT = """({inner_width: window.innerWidth, inner_height: window.innerHeight,
  device_pixel_ratio: window.devicePixelRatio})"""

# A modal hides controls from the agent, but the grader still needs the values behind it.
UNHIDE = """(() => {
  const marked = [...document.querySelectorAll('[aria-hidden="true"],[inert]')];
  window.__parallaxHidden = marked.map(e => [e, e.getAttribute('aria-hidden'), e.hasAttribute('inert')]);
  marked.forEach(e => { e.removeAttribute('aria-hidden'); e.removeAttribute('inert'); });
})()"""
RESTORE = """(() => {
  for (const [e, hidden, inert] of window.__parallaxHidden || []) {
    if (hidden !== null) e.setAttribute('aria-hidden', hidden);
    if (inert) e.setAttribute('inert', '');
  }
  delete window.__parallaxHidden;
})()"""

OBSERVER_CONFIG = Config(refuse_cookie_banners=False)
"""The observer reads the page; it must not answer a cookie banner the run left open."""


class FrameViewport(BaseModel):
    inner_width: int
    inner_height: int
    device_pixel_ratio: float


class FinalPage(BaseModel):
    url: str | None = None
    evidence: PageEvidence | None = None
    controls: tuple[tuple[str, str | None], ...] | None = None
    error: str | None = None
    frame: str | None = None
    frame_viewport: FrameViewport | None = None
    frame_withheld: str | None = None


def evidence_of(raw: object) -> PageEvidence | None:
    try:
        return PageEvidence.model_validate(raw)
    except ValueError:
        return None


async def _evaluate(client: CDPClient, session: str, expression: str) -> Any:
    result = await client.send.Runtime.evaluate(
        params={"expression": expression, "returnByValue": True}, session_id=session
    )
    if "exceptionDetails" in result:
        raise RuntimeError("final page script threw")
    return result["result"].get("value")


async def _pages(client: CDPClient) -> list[dict[str, Any]]:
    targets = (await client.send.Target.getTargets())["targetInfos"]
    return [dict(t) for t in targets if t["type"] == "page"]


@contextlib.asynccontextmanager
async def prepared_tab(cdp_url: str, *, viewport: bool) -> AsyncIterator[None]:
    """A tab for fastbrowse to attach to, held at `VIEWPORT` while the run lasts when `viewport` is set.

    fastbrowse attaches to the browser's first page, so one is opened when the browser has none. The override belongs
    to this session and holds for every client of the tab until it detaches, so it is held through the final reading;
    a popup the run opens later is not covered, and its evidence records the size it really had."""
    client = CDPClient(cdp_url)
    await client.start()
    try:
        pages = await _pages(client)
        target = (
            pages[0]["targetId"]
            if pages
            else (await client.send.Target.createTarget(params={"url": "about:blank"}))["targetId"]
        )
        if viewport:
            attached = await client.send.Target.attachToTarget(params={"targetId": target, "flatten": True})
            await client.send.Emulation.setDeviceMetricsOverride(params=VIEWPORT, session_id=attached["sessionId"])
        yield
    finally:
        with suppress(Exception):
            await client.stop()


async def _focused(client: CDPClient, pages: Sequence[dict[str, Any]], url: str | None) -> str | int:
    """The session of the one tab the run ended on, or how many candidates there were when that is ambiguous: the tab
    at the address the run reported, else the focused web tab, else the only web tab."""
    web = [p for p in pages if p["url"].startswith(("http://", "https://"))]
    named = [p for p in web if url is not None and p["url"] == url]
    candidates: list[tuple[str, bool]] = []
    for page in named if len(named) == 1 else web:
        attached = await client.send.Target.attachToTarget(params={"targetId": page["targetId"], "flatten": True})
        session = attached["sessionId"]
        candidates.append((session, await _evaluate(client, session, "document.hasFocus()") is True))
    if len(candidates) == 1:
        return candidates[0][0]
    focused = [session for session, focus in candidates if focus]
    return focused[0] if len(focused) == 1 else len(focused or candidates)


async def observe_browser(
    cdp_url: str,
    *,
    url: str | None = None,
    evidence: PageEvidence | None = None,
    secrets: Mapping[str, str] | None = None,
) -> FinalPage:
    """Read the tab the run ended on (`url` when the arm reported it); ambiguity fails closed instead of grading an
    arbitrary background page. `evidence` already read by the arm's own runner is kept rather than read again.
    The selected session supplies a viewport image before UNHIDE. Images are withheld unless the caller explicitly
    declares no task secrets, since checking text cannot rule out a secret appearing only in pixels."""
    client = CDPClient(cdp_url)
    address = None
    frame = None
    frame_viewport = None
    withheld = None
    try:
        await client.start()
        session = await _focused(client, await _pages(client), url)
        if not isinstance(session, str):
            return FinalPage(
                evidence=evidence,
                error=f"expected one final tab, found {session}",
                frame_withheld="the final tab is ambiguous",
            )
        if evidence is None:
            evidence = evidence_of(await _evaluate(client, session, EVIDENCE))
        unread = None if evidence is not None else "the final page's evidence did not validate"
        address = await _evaluate(client, session, LOCATION)
        withheld = "task secrets were not declared" if secrets is None else None
        if secrets:
            # Text checks cannot detect credentials rendered only as pixels or a page changing during capture.
            withheld = "the task declares secrets"
        if withheld is None:
            try:
                # A runner can retain text from before its daemon reset the viewport; the image has its own layout.
                viewport = FrameViewport.model_validate(await _evaluate(client, session, FRAME_VIEWPORT))
                frame = "data:image/png;base64," + await _shot(client, session)
                frame_viewport = viewport
            except Exception as exc:
                withheld = f"final capture failed ({type(exc).__name__})"
        await _evaluate(client, session, UNHIDE)
        try:
            async with connect_cdp(cdp_url=cdp_url, target_match=address, config=OBSERVER_CONFIG) as page:
                observation = await page.observe()
        finally:
            await _evaluate(client, session, RESTORE)
        if observation.url != address:
            return FinalPage(
                url=address,
                evidence=evidence,
                error="the final tab could not be told apart by address",
                frame=frame,
                frame_viewport=frame_viewport,
                frame_withheld=withheld,
            )
        return FinalPage(
            url=observation.url,
            controls=tuple((c.label, c.value) for c in observation.controls),
            evidence=evidence,
            error=unread,
            frame=frame,
            frame_viewport=frame_viewport,
            frame_withheld=withheld,
        )
    except Exception as exc:
        return FinalPage(
            url=address,
            evidence=evidence,
            frame=frame,
            frame_viewport=frame_viewport,
            frame_withheld=withheld,
            error=f"final observation failed ({type(exc).__name__})",
        )
    finally:
        # Losing the socket during cleanup must not discard a grade and its already incurred costs.
        with suppress(Exception):
            await client.stop()


async def _shot(client: CDPClient, session: str) -> str:
    shot = await client.send.Page.captureScreenshot(
        params={"format": "png", "captureBeyondViewport": False}, session_id=session
    )
    return shot["data"]


async def capture_browser(cdp_url: str, *, url: str | None = None) -> bytes | None:
    """A PNG of the tab the run ended on (`url` when known), for judges that grade a final frame; None when the tab
    cannot be told apart or the capture fails, never another tab's picture."""
    import base64

    client = CDPClient(cdp_url)
    try:
        await client.start()
        session = await _focused(client, await _pages(client), url)
        if not isinstance(session, str):
            return None
        return base64.b64decode(await _shot(client, session))
    except Exception:
        return None
    finally:
        with suppress(Exception):
            await client.stop()
