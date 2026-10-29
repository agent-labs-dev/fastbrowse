"""Run Parallax's live tasks through the current fastbrowse build, keeping every run's full trace.

Parallax itself imports the pre-Rust Python modules, so this loads only its task definitions and grades with
their own truth and check functions. Each run gets a fresh headed Chrome (under Xvfb) on a fresh profile, so no
cookie, sign-in or cart carries from one run to the next, attached from about:blank so the final tab survives the run
and can be observed.
"""

# Each blocking call is short and local: Chrome's DevTools on localhost, its launch, and one append per run.
# ruff: noqa: ASYNC210, ASYNC220, ASYNC230

import argparse
import asyncio
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

sys.path.insert(0, os.environ.get("PARALLAX_DIR", str(Path.home() / "drive/agl/parallax")))

import httpx
import websockets
from parallax.browser_use.live_tasks import TASKS
from parallax.browser_use.more_tasks import DEV, HELDOUT, STRETCH_DEV, STRETCH_HELDOUT

from fastbrowse.evals.live_tasks import Outcome, PageEvidence, prompt
from fastbrowse.models import Authorization, Limits, SecretRef
from fastbrowse.run import run_task
from fastbrowse.telemetry import traced

CHROME = str(Path.home() / ".cache/ms-playwright/chromium-1243/chrome-linux64/chrome")
SUITES = {"core": TASKS, "dev": DEV, "heldout": HELDOUT, "stretch-dev": STRETCH_DEV, "stretch-heldout": STRETCH_HELDOUT}

# The final page for the grader: the whole visible text first, then the
# agent's own snapshot of every control with modal-hidden ones unhidden, so a cart's Remove button or a flight
# listing is a control the checks can find.
SNAPSHOT_JS = (Path(os.environ.get("FASTBROWSE_TREE", ".")) / "crates/fastbrowse-browser/js/snapshot.js").read_text()
EVIDENCE = """(() => {
  const nav = performance.getEntriesByType('navigation')[0];
  const text = (document.body?.innerText || '').trim();
  return {url: location.href, status: nav?.responseStatus || null, title: document.title || null,
    text, text_length: text.length,
    inner_width: window.innerWidth, inner_height: window.innerHeight, device_pixel_ratio: window.devicePixelRatio};
})()"""
UNHIDE = """(() => {
  const marked = [...document.querySelectorAll('[aria-hidden="true"],[inert]')];
  window.__fastbrowseHidden = marked.map(e => [e, e.getAttribute('aria-hidden'), e.hasAttribute('inert')]);
  marked.forEach(e => { e.removeAttribute('aria-hidden'); e.removeAttribute('inert'); });
})()"""
RESTORE = """(() => {
  for (const [e, hidden, inert] of window.__fastbrowseHidden || []) {
    if (hidden !== null) e.setAttribute('aria-hidden', hidden);
    if (inert) e.setAttribute('inert', '');
  }
  delete window.__fastbrowseHidden;
})()"""


class Secrets:
    def __init__(self, values, origin):
        self.values, self.origin = values, origin

    def available(self):
        return tuple(SecretRef(name=name, origins=(self.origin,)) for name in self.values)

    async def resolve(self, name, origin):
        return self.values.get(name) if origin == self.origin else None


def origin_of(url):
    from urllib.parse import urlsplit

    parts = urlsplit(url)
    return f"{parts.scheme}://{parts.netloc}"


async def observe(port, final_url):
    pages = [t for t in json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list")) if t["type"] == "page"]
    chosen = next((p for p in reversed(pages) if p["url"] == final_url), None)
    chosen = chosen or next((p for p in reversed(pages) if p["url"] != "about:blank"), pages[-1] if pages else None)
    if chosen is None:
        return None, [p["url"] for p in pages]
    async with websockets.connect(chosen["webSocketDebuggerUrl"], max_size=2**26) as ws:
        ids = iter(range(1, 100))

        async def evaluate(expression):
            n = next(ids)
            await ws.send(
                json.dumps(
                    {"id": n, "method": "Runtime.evaluate", "params": {"expression": expression, "returnByValue": True}}
                )
            )
            while True:
                reply = json.loads(await ws.recv())
                if reply.get("id") == n:
                    return reply.get("result", {}).get("result", {}).get("value")

        seen = await evaluate(EVIDENCE) or {}
        await evaluate(UNHIDE)
        try:
            snapshot = await evaluate(SNAPSHOT_JS.strip().rstrip(";") + "('snapshot')") or {}
        finally:
            await evaluate(RESTORE)
        seen["controls"] = [[c.get("label") or "", c.get("value")] for c in snapshot.get("controls", [])]
        return seen, [p["url"] for p in pages]


async def one(task, suite, repeat, worker, args, http):
    port = 9400 + worker
    profile = tempfile.mkdtemp(prefix=f"{task.id}-{repeat}-", dir=args.profiles)
    chrome = subprocess.Popen(
        [
            CHROME,
            f"--remote-debugging-port={port}",
            f"--user-data-dir={profile}",
            "--no-first-run",
            "--no-default-browser-check",
            "--window-size=1280,900",
            "about:blank",
        ],
        env=os.environ | {"DISPLAY": args.display},
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    row = {
        "config": args.config,
        "suite": suite,
        "task": task.id,
        "repeat": repeat,
        "category": task.category.value,
        "expect": task.expect.value,
        "prompt": prompt(task),
        "start": task.start,
    }
    try:
        for _ in range(50):
            try:
                ws = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version"))["webSocketDebuggerUrl"]
                break
            except OSError:
                await asyncio.sleep(0.2)
        secrets = Secrets(dict(task.secrets), origin_of(task.start)) if task.secrets else None
        began = time.monotonic()
        with traced() as events:
            try:
                result = await run_task(
                    prompt(task),
                    start=task.start,
                    cdp_url=ws,
                    attach=True,
                    output_schema=task.output_schema,
                    secrets=secrets,
                    limits=Limits(),
                    authorization=Authorization(irreversible_actions=task.authorize),
                    http=http,
                )
            except Exception as error:  # a harness or core failure is kept, not hidden
                row |= {"crash": repr(error), "events": list(events), "seconds": time.monotonic() - began}
                return row
        row["seconds"] = time.monotonic() - began
        seen, pages = await observe(port, result.final_url)
        evidence = (
            PageEvidence(
                **{
                    k: seen[k]
                    for k in (
                        "status",
                        "title",
                        "text",
                        "text_length",
                        "inner_width",
                        "inner_height",
                        "device_pixel_ratio",
                    )
                }
            )
            if seen
            else None
        )
        controls = tuple((label, value) for label, value in seen["controls"]) if seen else None
        quotes = tuple((e.url, e.quote) for e in result.evidence)
        outcome = Outcome(
            result.answer, result.data, (seen or {}).get("url") or result.final_url, quotes, controls, evidence=evidence
        )
        try:
            truth = await task.truth(http)
            check = task.check(outcome, truth)
        except Exception as error:
            truth, check = None, f"grader error: {error!r}"
        status_ok = result.status.value == task.expect.value
        row |= {
            "status": result.status.value,
            "passed": status_ok and check is None,
            "failure": None
            if status_ok and check is None
            else (check or f"status {result.status.value}, expected {task.expect.value}"),
            "truth": truth if isinstance(truth, (str, int, float, bool, list, dict, type(None))) else repr(truth),
            "answer": result.answer,
            "data": result.data,
            "error": result.error,
            "final_url": result.final_url,
            "observed": seen,
            "pages": pages,
            "result": result.model_dump(mode="json"),
            "events": list(events),
        }
        return row
    finally:
        chrome.terminate()
        try:
            chrome.wait(10)
        except subprocess.TimeoutExpired:
            chrome.kill()
            chrome.wait()
        shutil.rmtree(profile, ignore_errors=True)


async def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", required=True)
    parser.add_argument("--suites", nargs="+", default=list(SUITES))
    parser.add_argument("--only", nargs="*")
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--workers", type=int, default=2)
    parser.add_argument("--worker-base", type=int, default=0)
    parser.add_argument("--out", required=True)
    parser.add_argument("--profiles", required=True)
    parser.add_argument("--display", default=":99")
    args = parser.parse_args()
    os.makedirs(args.profiles, exist_ok=True)
    jobs = [
        (t, s, r)
        for r in range(args.repeat)
        for s in args.suites
        for t in SUITES[s]
        if not args.only or t.id in args.only
    ]
    queue = asyncio.Queue()
    for job in jobs:
        queue.put_nowait(job)
    lock = asyncio.Lock()

    async def worker(index):
        async with httpx.AsyncClient(timeout=60, follow_redirects=True) as http:
            while not queue.empty():
                task, suite, repeat = queue.get_nowait()
                row = await one(task, suite, repeat, args.worker_base + index, args, http)
                async with lock:
                    with open(args.out, "a") as out:
                        out.write(json.dumps(row, default=str) + "\n")
                note = row.get("failure") or row.get("crash") or ""
                print(
                    f"{time.strftime('%T')} {args.config} {task.id}#{repeat} {row.get('status')} "
                    f"passed={row.get('passed')} {row.get('seconds', 0):.1f}s {note}",
                    flush=True,
                )

    await asyncio.gather(*(worker(i) for i in range(args.workers)))


if __name__ == "__main__":
    asyncio.run(main())
