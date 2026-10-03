"""Throwaway: does the unmodified BrowserSession attach to an Electron app?"""

import asyncio, json, sys, urllib.request
from fastbrowse.browser import BrowserSession
from fastbrowse.models import BrowserConnection
from tests.browser.conftest import RecordingArtifactSink


async def main(port: int) -> None:
    ws = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version"))["webSocketDebuggerUrl"]
    try:
        async with BrowserSession(BrowserConnection(cdp_url=ws, remote=False), RecordingArtifactSink()) as s:
            print("stock session opened:", s.tabs())
    except Exception as exc:
        print("stock session failed:", type(exc).__name__, exc)
    print(
        "targets after:",
        [(t["type"], t["url"]) for t in json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list"))],
    )


asyncio.run(main(int(sys.argv[1])))
