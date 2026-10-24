"""Python's `CdpPage` running the probe's script, step for step, printing the same JSON fastbrowse-probe prints.

    uv run python crates/fastbrowse-probe/bench/python_reference.py <script.json> <main-origin> [--only <page>]

Uses fastbrowse's own local_chrome, BrowserSession and CdpPage, with the default `Config`, one session per page on
one launched Chrome, so what each step reads and does is what a run's browser layer reads and does.
"""

import asyncio
import json
import sys
import time

from fastbrowse.adapters.local_chrome import async_local_chrome
from fastbrowse.browser.page import CdpPage
from fastbrowse.browser.session import BrowserSession
from fastbrowse.config import Config
from fastbrowse.models import Artifact, ArtifactKind, BrowserConnection, LocalChrome, Operation
from fastbrowse.page import Action, BrowserError, Observation


class _NullSink:
    """Downloads are dropped: the check compares what the page reports, and a capture of a download's text."""

    async def put(self, kind: ArtifactKind, name: str, mime_type: str, content: bytes) -> Artifact:
        return Artifact(kind=kind, name=name, mime_type=mime_type, size_bytes=len(content), sha256="", uri=f"memory://{name}")


def _error(error: Exception) -> dict:
    kind = type(error).__name__
    return {"error": kind, "message": str(error)}


def _target(observation: Observation, step: dict, key: str) -> str | None:
    label = step.get(key)
    nth = step.get(f"{key}_nth", 0)
    matches = [c.id for c in observation.controls if c.label == label]
    return matches[nth] if len(matches) > nth else None


async def _step(page: CdpPage, last: list[Observation], base: str, step: dict) -> object:
    op = step["op"]
    try:
        if op == "observe":
            observation = await page.observe()
            last[:] = [observation]
            return observation.model_dump(mode="json")
        if op == "capture":
            return (await page.capture()).model_dump(mode="json")
        if op == "goto":
            await page.navigate(f"{base}{step.get('path', '/')}")
            return {"ok": True}
        if op == "sleep":
            await asyncio.sleep(step.get("seconds", 0))
            return {"ok": True}
        try:
            operation = Operation(op)
        except ValueError:
            return {"error": "UnknownStep", "message": op}
        if not last:
            return {"error": "NoObservation"}
        observation = last[0]
        fields: dict[str, object] = {"operation": operation}
        if "label" in step:
            target = _target(observation, step, "label")
            if target is None:
                return {"error": "NoSuchControl", "message": step["label"]}
            fields["target_id"] = target
        if "to" in step:
            fields["destination_id"] = _target(observation, step, "to")
        if "text" in step:
            fields["text"] = step["text"]
        if "accept" in step:
            fields["accept_dialog"] = step["accept"]
        if "path" in step:
            fields["url"] = f"{base}{step['path']}"
        if "tab" in step and step["tab"] < len(observation.tabs):
            fields["tab_id"] = observation.tabs[step["tab"]].id
        return (await page.act(Action.model_validate(fields), observation)).model_dump(mode="json")
    except BrowserError as error:
        return _error(error)


async def _page(connection: BrowserConnection, base: str, spec: dict) -> dict:
    async with BrowserSession(connection, _NullSink()) as session:
        page = CdpPage(session, Config())
        steps = []
        started = time.perf_counter()
        try:
            await page.navigate(f"{base}{spec['path']}")
            out: object = {"ok": True}
        except BrowserError as error:
            out = _error(error)
        steps.append({"op": "navigate", "ms": (time.perf_counter() - started) * 1000, "out": out})
        if out == {"ok": True}:
            last: list[Observation] = []
            for step in spec["steps"]:
                started = time.perf_counter()
                out = await _step(page, last, base, step)
                steps.append({"op": step["op"], "ms": (time.perf_counter() - started) * 1000, "out": out})
        return {"name": spec["name"], "steps": steps}


async def main(script_path: str, base: str, only: str | None) -> None:
    with open(script_path, encoding="utf-8") as file:
        script = json.load(file)
    started = time.perf_counter()
    async with async_local_chrome(LocalChrome()) as connection:
        launch_ms = (time.perf_counter() - started) * 1000
        pages = [await _page(connection, base, spec) for spec in script["pages"] if only in (None, spec["name"])]
    print(json.dumps({"launch_ms": launch_ms, "pages": pages}))


_args = sys.argv[1:]
asyncio.run(main(_args[0], _args[1], _args[_args.index("--only") + 1] if "--only" in _args else None))
