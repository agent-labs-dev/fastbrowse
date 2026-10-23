"""The pages snapshot.js is measured on and checked against, and what the two share.

Each page builds its DOM from `?n=`: the golden snapshots use a small `n` so their JSON stays reviewable,
and the benchmarks a large one that loads the hot paths the way a dense site does.
"""

from __future__ import annotations

import functools
import json
import threading
from collections.abc import Generator
from contextlib import contextmanager
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

from fastbrowse.browser.page import CdpPage

SITES = Path(__file__).parent / "sites"
GOLDEN = Path(__file__).parent / "golden"

# Page name to (golden n, benchmark n).
PAGES = {
    "links": (40, 600),
    "cards": (8, 300),
    "calendar": (2, 24),
    "forms": (4, 80),
    "frames": (6, 150),
}

# Every frame has finished loading, so the snapshot sees the same-origin frame's controls.
_LOADED_JS = (
    "new Promise(resolve => { const poll = () => [document, ...[...document.querySelectorAll('iframe')]"
    ".map(f => f.contentDocument)].every(d => d?.readyState === 'complete') ? resolve(true) : setTimeout(poll, 10); "
    "poll(); })"
)


@contextmanager
def serve() -> Generator[str]:
    """Serve `SITES` on a free local port and yield its origin."""

    class Quiet(SimpleHTTPRequestHandler):
        def log_message(self, format: str, *args: object) -> None:
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Quiet, directory=str(SITES)))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        yield f"http://127.0.0.1:{server.server_address[1]}"
    finally:
        server.shutdown()


async def open_page(page: CdpPage, origin: str, name: str, n: int) -> None:
    await page.navigate(f"{origin}/{name}.html?n={n}")
    await page._evaluate(page._session.active_session_id, _LOADED_JS)


async def snapshot(page: CdpPage) -> Any:
    return await page._evaluate(page._session.active_session_id, page._page_js + "('snapshot')")


async def fingerprint(page: CdpPage) -> Any:
    return await page._evaluate(page._session.active_session_id, page._fingerprint_js)


def normalize(value: Any, origin: str) -> Any:
    """What a run on another port or at another moment would equally produce."""
    if isinstance(value, str):
        return value.replace(origin, "http://fixture")
    if isinstance(value, list):
        return [normalize(v, origin) for v in value]
    if isinstance(value, dict):
        return {k: normalize(v, origin) for k, v in value.items()}
    return value


def golden_view(result: dict[str, Any], fingerprint: str, origin: str) -> dict[str, Any]:
    """The snapshot as the golden file records it: the document's time origin is a clock reading, and
    `page_key` is decoded so a change shows as a diff of its parts."""
    view = dict(result)
    del view["document_key"]
    view["page_key"] = json.loads(view["page_key"])
    # A guard holds its document's `performance.timeOrigin` at index 15.
    view["guards"] = {k: g if g is None else [*g[:15], None, *g[16:]] for k, g in view["guards"].items()}
    view["fingerprint"] = fingerprint
    return normalize(view, origin)
