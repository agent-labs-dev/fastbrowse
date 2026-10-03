"""Throwaway: drive an Electron app's existing window through the real CdpPage via an attach-mode session.

Run prototypes/electron-fixture with --remote-debugging-port=PORT, then: PYTHONPATH=. uv run python prototypes/electron_attach.py PORT
"""

import asyncio
import json
import sys
import time
import urllib.request

from fastbrowse.browser import BrowserSession, CdpPage
from fastbrowse.browser.session import _TabState, _together
from fastbrowse.config import Config
from fastbrowse.models import BrowserConnection, Operation
from fastbrowse.page import Action, Observation
from tests.browser.conftest import RecordingArtifactSink


class AttachedSession(BrowserSession):
    """Attach to a window the app already has instead of creating a tab; never close app windows."""

    def __init__(self, *args, url_match: str = "", **kwargs) -> None:
        super().__init__(*args, refuse_cookie_banners=False, **kwargs)
        self._url_match = url_match

    async def _open_owned_tab(self, url: str) -> str:
        targets = (await self.client.send.Target.getTargets())["targetInfos"]
        pages = [t for t in targets if t["type"] == "page" and self._url_match in t["url"]]
        if not pages:
            raise RuntimeError(f"no page target matching {self._url_match!r}: {[t['url'] for t in targets]}")
        return await self._attach(pages[0]["targetId"], pages[0]["url"], pages[0]["title"])

    async def _attach(self, target_id: str, url: str, title: str, opener_id: str | None = None) -> str:
        attach = await self.client.send.Target.attachToTarget(params={"targetId": target_id, "flatten": True})
        session_id = attach["sessionId"]
        await _together(
            self.client.send.Target.activateTarget(params={"targetId": target_id}), self._prepare_session(session_id)
        )
        # addScriptToEvaluateOnNewDocument only reaches the next document; the loaded one needs it now.
        for source in self._new_document_scripts:
            await self.client.send.Runtime.evaluate(params={"expression": source}, session_id=session_id)
        self._tabs[target_id] = _TabState(
            target_id=target_id, session_id=session_id, url=url, title=title, opener_id=opener_id
        )
        self._set_active_target(target_id)
        return target_id

    def _on_target_created(self, event, session_id) -> None:
        # Electron windows opened by the main process carry no openerId: adopt every new page, own none.
        info = event["targetInfo"]
        if not self._closing and info["type"] == "page" and info["targetId"] not in self._tabs:
            opener = self._active_target_id
            self._popups[info["targetId"]] = (opener, asyncio.get_running_loop().create_future())
            self._spawn(self._adopt_window(info["targetId"], opener))

    async def _adopt_window(self, target_id: str, opener: str) -> None:
        try:
            before = self._active_target_id
            await self._attach(target_id, "", "", opener_id=opener)
            self._set_active_target(before)  # adoption is not a switch; follow_popup decides that
            info = (await self.client.send.Target.getTargetInfo(params={"targetId": target_id}))["targetInfo"]
            self.set_tab_info(target_id, info["url"], info["title"])
        finally:
            fut = self._popups[target_id][1]
            if not fut.done():
                fut.set_result(target_id in self._tabs)


def show(label: str, obs: Observation) -> None:
    print(f"\n== {label}: {obs.url}  title={obs.title!r}  tabs={[(t.title, t.active) for t in obs.tabs]}")
    print("   controls:", [(c.id, c.label) for c in obs.controls])
    print("   text:", obs.viewport_text[:300].replace("\n", " | "))
    if obs.dialog:
        print("   dialog:", obs.dialog)


def find(obs: Observation, label: str):
    return next(c for c in obs.controls if label in c.label)


async def step(page: CdpPage, obs: Observation, label: str, action: Action) -> Observation:
    t = time.perf_counter()
    result = await page.act(action, obs)
    after = await page.observe()
    print(f"-- {label}: {result.outcome}  ({(time.perf_counter() - t) * 1000:.0f} ms act+observe)")
    return after


async def main(port: int) -> None:
    ws = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version"))["webSocketDebuggerUrl"]
    async with AttachedSession(
        BrowserConnection(cdp_url=ws, remote=False), RecordingArtifactSink(), url_match="index.html"
    ) as session:
        page = CdpPage(session, Config())
        obs = await page.observe()
        show("attached", obs)
        print("   origin():", await page.origin(), " can_go_back:", obs.can_go_back)

        for text in ("Buy milk", "Ship prototype"):
            obs = await step(
                page,
                obs,
                f"fill {text!r}",
                Action(operation=Operation.FILL, target_id=find(obs, "New todo").id, text=text),
            )
            obs = await step(page, obs, "enter", Action(operation=Operation.ENTER, target_id=find(obs, "New todo").id))
        show("after adding", obs)

        obs = await step(
            page, obs, "check first todo", Action(operation=Operation.CLICK, target_id=find(obs, "Buy milk").id)
        )
        obs = await step(
            page, obs, "save (IPC to main)", Action(operation=Operation.CLICK, target_id=find(obs, "Save").id)
        )
        show("after save", obs)
        await asyncio.sleep(0.3)
        show("after save +300ms", await page.observe())
        print(
            "   DOM status:",
            (
                await session.client.send.Runtime.evaluate(
                    params={"expression": "document.getElementById('status').textContent", "returnByValue": True},
                    session_id=session.active_session_id,
                )
            )["result"],
        )
        obs = await page.observe()

        obs = await step(page, obs, "about tab", Action(operation=Operation.CLICK, target_id=find(obs, "About").id))
        show("about", obs)
        obs = await step(page, obs, "todos tab", Action(operation=Operation.CLICK, target_id=find(obs, "Todos").id))

        obs = await step(page, obs, "open menu", Action(operation=Operation.CLICK, target_id=find(obs, "More").id))
        known = session.popups()
        obs = await step(
            page, obs, "open settings window", Action(operation=Operation.CLICK, target_id=find(obs, "settings").id)
        )
        show("settings window", obs)
        if obs.title == "Settings":
            obs = await step(
                page, obs, "toggle dark", Action(operation=Operation.CLICK, target_id=find(obs, "Dark").id)
            )
            show("after toggle", obs)
            main_tab = next(t for t in obs.tabs if t.title == "Todo Desk")
            obs = await step(page, obs, "switch back", Action(operation=Operation.SWITCH_TAB, tab_id=main_tab.id))

        if not any("Clear" in c.label for c in obs.controls):
            obs = await step(page, obs, "open menu", Action(operation=Operation.CLICK, target_id=find(obs, "More").id))
        obs = await step(
            page, obs, "clear all (confirm)", Action(operation=Operation.CLICK, target_id=find(obs, "Clear").id)
        )
        show("confirm pending", obs)
        if obs.dialog:
            obs = await step(page, obs, "accept", Action(operation=Operation.DIALOG, accept_dialog=True))
            show("after clear", obs)

        png = await page.screenshot()
        open("/tmp/fb-electron/shot.png", "wb").write(png)
        print("\nscreenshot bytes:", len(png))
    print(
        "targets after teardown:",
        [(t["type"], t["title"]) for t in json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list"))],
    )


if __name__ == "__main__":
    asyncio.run(main(int(sys.argv[1])))
