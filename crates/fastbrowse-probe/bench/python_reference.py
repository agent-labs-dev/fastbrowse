"""The Python path doing what fastbrowse-probe does: launch Chrome, open a tab, navigate, wait for load, observe.

Uses fastbrowse's own local_chrome, BrowserSession and CdpPage, so the session setup is what a run pays.

    FASTBROWSE_PROBE_T0_NS=$(python3 -c 'import time; print(time.time_ns())') \
        uv run python crates/fastbrowse-probe/bench/python_reference.py <url> [--click <js> [--accept-dialog]]
"""

import os
import sys
import time

T0 = int(os.environ.get("FASTBROWSE_PROBE_T0_NS", time.time_ns()))


def mark(phase: str) -> None:
    print(f"{phase}: {(time.time_ns() - T0) / 1e6:.1f} ms", file=sys.stderr)


import asyncio  # noqa: E402
import json  # noqa: E402
import tempfile  # noqa: E402
from pathlib import Path  # noqa: E402

from fastbrowse.adapters.local_chrome import async_local_chrome  # noqa: E402
from fastbrowse.artifacts import DirectorySink  # noqa: E402
from fastbrowse.browser.page import CdpPage  # noqa: E402
from fastbrowse.browser.session import BrowserSession  # noqa: E402
from fastbrowse.config import Config, ObservationLimits  # noqa: E402
from fastbrowse.models import LocalChrome  # noqa: E402
from fastbrowse.page import Observation  # noqa: E402

mark("imported")

# The probe waits for Page.loadEventFired; this is the same moment, read from the page.
_LOADED = (
    "new Promise(resolve => document.readyState === 'complete' ? resolve(true) "
    ": addEventListener('load', () => resolve(true), { once: true }))"
)


def _observation(observation: Observation) -> dict:
    """The fields fastbrowse-probe prints for one observation."""
    return {
        "url": observation.url,
        "title": observation.title,
        "inaccessible_frames": observation.inaccessible_frames,
        "dialog": observation.dialog.model_dump(mode="json") if observation.dialog else None,
        "controls": [c.model_dump(mode="json", exclude_unset=True) for c in observation.controls],
    }


async def main(url: str, click: str | None, accept_dialog: bool) -> None:
    # The caps are observation policy, not the browser layer; the probe merges without them.
    config = Config(observation=ObservationLimits(max_controls=100_000, max_offscreen_controls=100_000))
    with tempfile.TemporaryDirectory() as artifacts:
        async with (
            async_local_chrome(LocalChrome()) as connection,
            BrowserSession(connection, DirectorySink(Path(artifacts))) as session,
        ):
            mark("tab")
            page = CdpPage(session, config)
            sid = session.active_session_id
            result = await session.client.send.Page.navigate(params={"url": url}, session_id=sid)
            if error := result.get("errorText"):
                raise SystemExit(f"Page.navigate failed ({error})")
            await page._until_dialog(page._evaluate(sid, _LOADED))
            mark("navigated")
            steps = [_observation(await page.observe())]
            mark("snapshot")
            if click is not None:
                await page._until_dialog(page._evaluate(sid, click))
                steps.append(_observation(await page.observe()))
                if accept_dialog:
                    await session.handle_dialog(True)
                    steps.append(_observation(await page.observe()))
            print(json.dumps({"steps": steps}))


_args = sys.argv[1:]
_click = _args[_args.index("--click") + 1] if "--click" in _args else None
asyncio.run(main(_args[0], _click, "--accept-dialog" in _args))
