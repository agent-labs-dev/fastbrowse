"""Final page evidence read by the harness before the browser is released."""

from contextlib import suppress
from pathlib import Path

from cdp_use.client import CDPClient
from pydantic import BaseModel

from fastbrowse.browser import CdpPage
from fastbrowse.page import Observation

# A modal hides controls from the agent, but the grader still needs the values behind it.
_UNHIDE = """(() => {
  const marked = [...document.querySelectorAll('[aria-hidden="true"],[inert]')];
  window.__fastbrowseHidden = marked.map(e => [e, e.getAttribute('aria-hidden'), e.hasAttribute('inert')]);
  marked.forEach(e => { e.removeAttribute('aria-hidden'); e.removeAttribute('inert'); });
})()"""
_RESTORE = """(() => {
  for (const [e, hidden, inert] of window.__fastbrowseHidden || []) {
    if (hidden !== null) e.setAttribute('aria-hidden', hidden);
    if (inert) e.setAttribute('inert', '');
  }
  delete window.__fastbrowseHidden;
})()"""
_SNAPSHOT = (Path(__file__).parents[1] / "browser" / "snapshot.js").read_text(encoding="utf-8") + "('snapshot')"


class GradedPage(CdpPage):
    async def observe_all(self) -> Observation:
        session_id = self._session.active_session_id
        await self._evaluate(session_id, _UNHIDE)
        try:
            return await self.observe()
        finally:
            await self._evaluate(session_id, _RESTORE)


class FinalPage(BaseModel):
    url: str | None = None
    controls: tuple[tuple[str, str | None], ...] | None = None
    error: str | None = None


class _Control(BaseModel):
    label: str
    value: str | None = None


class _Snapshot(BaseModel):
    url: str
    controls: list[_Control]


async def observe_browser(cdp_url: str) -> FinalPage:
    """Observe the focused tab; ambiguity fails closed instead of grading an arbitrary background page."""
    client = CDPClient(cdp_url)
    try:
        await client.start()
        targets = (await client.send.Target.getTargets())["targetInfos"]
        candidates: list[tuple[str, bool]] = []
        for target in targets:
            if target["type"] != "page" or not target["url"].startswith(("http://", "https://")):
                continue
            attached = await client.send.Target.attachToTarget(params={"targetId": target["targetId"], "flatten": True})
            session = attached["sessionId"]
            focus = await client.send.Runtime.evaluate(
                params={"expression": "document.hasFocus()", "returnByValue": True}, session_id=session
            )
            candidates.append((session, focus["result"].get("value") is True))
        focused = [session for session, focus in candidates if focus]
        choices = focused or [session for session, _ in candidates]
        if len(choices) != 1:
            return FinalPage(error=f"expected one final tab, found {len(choices)}")
        session = choices[0]
        await client.send.Runtime.evaluate(params={"expression": _UNHIDE}, session_id=session)
        try:
            result = await client.send.Runtime.evaluate(
                params={"expression": _SNAPSHOT, "returnByValue": True}, session_id=session
            )
            snapshot = _Snapshot.model_validate(result["result"].get("value"))
            return FinalPage(url=snapshot.url, controls=tuple((c.label, c.value) for c in snapshot.controls))
        finally:
            await client.send.Runtime.evaluate(params={"expression": _RESTORE}, session_id=session)
    except Exception as exc:
        return FinalPage(error=f"final observation failed ({type(exc).__name__})")
    finally:
        # Losing the socket during cleanup must not discard a grade and its already incurred costs.
        with suppress(Exception):
            await client.stop()
