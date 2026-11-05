"""Drives the #793 spike: the extension relay against direct CDP on the same headless browser."""

import asyncio
import itertools
import json
import statistics
import sys
import time
from pathlib import Path

import websockets

SOCK, PROFILE, URL = sys.argv[1], sys.argv[2], sys.argv[3]
DEVTOOLS_PORT_FILE = Path(PROFILE, "DevToolsActivePort")
ids = itertools.count(1)


class Relay:
    def __init__(s, r, w):
        s.r, s.w, s.pending, s.events = r, w, {}, []

    async def pump(s):
        async for line in s.r:
            m = json.loads(line)
            if "id" in m and m["id"] in s.pending:
                s.pending.pop(m["id"]).set_result(m)
            else:
                s.events.append(m)

    async def call(s, op, **kw):
        i = next(ids)
        f = asyncio.get_running_loop().create_future()
        s.pending[i] = f
        s.w.write((json.dumps({"id": i, "op": op, **kw}) + "\n").encode())
        await s.w.drain()
        m = await asyncio.wait_for(f, 10)
        if "error" in m:
            raise RuntimeError(m["error"])
        return m["result"]


def ms(xs):
    return f"median {statistics.median(xs) * 1000:.2f} ms, p95 {sorted(xs)[int(len(xs) * 0.95)] * 1000:.2f} ms"


async def main():
    out = {}
    r, w = await asyncio.open_unix_connection(SOCK)
    rl = Relay(r, w)
    pump = asyncio.create_task(rl.pump())
    out["hello"] = await rl.call("hello")
    tab = await rl.call("newTab", url=URL, run="spike run")
    tid = tab["tabId"]
    await rl.call("attach", tabId=tid)

    def cdp(method, **p):
        return rl.call("cdp", tabId=tid, method=method, params=p)

    await cdp("Runtime.enable")
    await asyncio.sleep(1)

    def ev(expr):
        return cdp("Runtime.evaluate", expression=expr, returnByValue=True)

    out["title"] = (await ev("document.title"))["result"]["value"]
    box = (
        await ev(
            "(()=>{const r=document.getElementById('go').getBoundingClientRect();"
            "return [r.x+r.width/2,r.y+r.height/2]})()"
        )
    )["result"]["value"]
    for t in ("mousePressed", "mouseReleased"):
        await cdp("Input.dispatchMouseEvent", type=t, x=box[0], y=box[1], button="left", clickCount=1)
    await ev("document.getElementById('name').focus()")
    await cdp("Input.insertText", text="Ada")
    out["click_isTrusted"] = (await ev("window.clicks"))["result"]["value"]
    out["typed"] = (await ev("document.getElementById('name').value"))["result"]["value"]
    shot = await cdp("Page.captureScreenshot", format="png")
    out["screenshot_bytes"] = len(shot["data"]) * 3 // 4
    for method in (
        "Target.getTargets",
        "Target.setDiscoverTargets",
        "Fetch.enable",
        "Emulation.setDeviceMetricsOverride",
        "Page.startScreencast",
    ):
        params = {"discover": True} if method == "Target.setDiscoverTargets" else {}
        if method == "Emulation.setDeviceMetricsOverride":
            params = {"width": 390, "height": 844, "deviceScaleFactor": 3, "mobile": True}
        try:
            await cdp(method, **params)
            out[method] = "allowed"
        except RuntimeError as e:
            out[method] = f"refused: {e}"
    await cdp("Fetch.disable")
    await cdp("Page.stopScreencast")
    await cdp("Emulation.clearDeviceMetricsOverride")
    relay_t = []
    for _ in range(200):
        t = time.perf_counter()
        await ev("1+1")
        relay_t.append(time.perf_counter() - t)
    out["relay_evaluate"] = ms(relay_t)

    port, path = (await asyncio.to_thread(DEVTOOLS_PORT_FILE.read_text)).split()[:2]
    async with websockets.connect(f"ws://127.0.0.1:{port}{path}", max_size=None) as ws:

        async def raw(method, sid=None, **p):
            i = next(ids)
            msg = {"id": i, "method": method, "params": p}
            if sid:
                msg["sessionId"] = sid
            await ws.send(json.dumps(msg))
            while True:
                m = json.loads(await ws.recv())
                if m.get("id") == i:
                    return m

        targets = (await raw("Target.getTargets"))["result"]["targetInfos"]
        target = next(t for t in targets if t["type"] == "page" and t["url"].startswith(URL))
        sid = (await raw("Target.attachToTarget", targetId=target["targetId"], flatten=True))["result"]["sessionId"]
        direct_t = []
        for _ in range(200):
            t = time.perf_counter()
            await raw("Runtime.evaluate", sid, expression="1+1", returnByValue=True)
            direct_t.append(time.perf_counter() - t)
        out["direct_cdp_evaluate"] = ms(direct_t)

    await rl.call("closeTab", tabId=tid)
    await asyncio.sleep(0.5)
    out["events_after_close"] = [e for e in rl.events if e.get("event") == "detached"]
    pump.cancel()
    print(json.dumps(out, indent=2))


asyncio.run(main())
