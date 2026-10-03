"""Throwaway: observe and act in a real Electron app (VS Code) through AttachedSession."""

import asyncio
import json
import sys
import time
import urllib.request

from fastbrowse.browser import CdpPage
from fastbrowse.config import Config
from fastbrowse.models import BrowserConnection, Operation
from fastbrowse.page import Action
from prototypes.electron_attach import AttachedSession, show, step
from tests.browser.conftest import RecordingArtifactSink


async def main(port: int) -> None:
    ws = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version"))["webSocketDebuggerUrl"]
    async with AttachedSession(
        BrowserConnection(cdp_url=ws, remote=False), RecordingArtifactSink(), url_match="workbench"
    ) as session:
        page = CdpPage(session, Config())
        t = time.perf_counter()
        obs = await page.observe()
        print(
            f"observe: {(time.perf_counter() - t) * 1000:.0f} ms, {len(obs.controls)} controls,"
            f" {obs.omitted_controls} omitted, {obs.inaccessible_frames} inaccessible frames"
        )
        show("workbench", obs)
        notes = next((c for c in obs.controls if "notes.txt" in c.label), None)
        if notes:
            obs = await step(page, obs, "open notes.txt", Action(operation=Operation.CLICK, target_id=notes.id))
            await asyncio.sleep(1)
            obs = await page.observe()
            show("after open", obs)
            print("   'hello from fixture' visible:", "hello from fixture" in obs.viewport_text)
        open("/tmp/fb-vscode/shot.png", "wb").write(await page.screenshot())


if __name__ == "__main__":
    asyncio.run(main(int(sys.argv[1])))
