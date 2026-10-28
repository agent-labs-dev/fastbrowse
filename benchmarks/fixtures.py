"""The pages snapshot.js is measured on and checked against.

Each page builds its DOM from `?n=`: the golden snapshots (crates/fastbrowse-browser/tests/capture.rs) use a small
`n` so their JSON stays reviewable, and the benchmarks a large one that loads the hot paths the way a dense site does.
"""

from __future__ import annotations

import functools
import threading
from collections.abc import Generator
from contextlib import contextmanager
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

SITES = Path(__file__).parent / "sites"

# Page name to (golden n, benchmark n).
PAGES = {
    "links": (40, 600),
    "cards": (8, 300),
    "calendar": (2, 24),
    "forms": (4, 80),
    "frames": (6, 150),
}


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
