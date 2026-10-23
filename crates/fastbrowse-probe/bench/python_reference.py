"""The Python path doing what fastbrowse-probe does: launch Chrome, open a tab, navigate, wait for load, snapshot.

Uses fastbrowse's own local_chrome, BrowserSession and CdpPage, so the session setup is what a run pays.

    FASTBROWSE_PROBE_T0_NS=$(python3 -c 'import time; print(time.time_ns())') \
        uv run python crates/fastbrowse-probe/bench/python_reference.py <url>
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
from fastbrowse.config import Config  # noqa: E402
from fastbrowse.models import LocalChrome  # noqa: E402

mark("imported")

# The probe waits for Page.loadEventFired; this is the same moment, read from the page.
_LOADED = (
    "new Promise(resolve => document.readyState === 'complete' ? resolve(true) "
    ": addEventListener('load', () => resolve(true), { once: true }))"
)


async def main(url: str) -> None:
    with tempfile.TemporaryDirectory() as artifacts:
        async with (
            async_local_chrome(LocalChrome()) as connection,
            BrowserSession(connection, DirectorySink(Path(artifacts))) as session,
        ):
            mark("tab")
            page = CdpPage(session, Config())
            sid = session.active_session_id
            result = await session.client.send.Page.navigate(params={"url": url}, session_id=sid)
            if error := result.get("errorText"):
                raise SystemExit(f"Page.navigate failed ({error})")
            await page._evaluate(sid, _LOADED)
            mark("navigated")
            frames, _ = await page._snapshot_all_frames()
            mark("snapshot")
            main = frames["main"].raw
            print(
                json.dumps(
                    {
                        "url": main.url,
                        "title": main.title,
                        "inaccessible_frames": main.inaccessible_frames,
                        "controls": [c.model_dump(mode="json", exclude_unset=True) for c in main.controls],
                        "child_frames": len(frames) - 1,
                    }
                )
            )


asyncio.run(main(sys.argv[1]))
