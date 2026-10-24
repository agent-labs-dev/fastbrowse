"""Serve the probe's fixture pages with the browser test suite's own handler.

`main` is tests/browser/sites/main with `__IFRAME_ORIGIN__` filled in as `tests/browser/conftest.py` fills it, so
its iframe is served from another site (localhost vs 127.0.0.1) and Chrome puts it in a process of its own.

    uv run python crates/fastbrowse-probe/bench/serve.py    # prints the URLs and serves until killed
"""

import sys
import threading
from http.server import ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT))

from tests.browser.conftest import SITES, _handler_for  # noqa: E402


def _serve(handler: type, host: str) -> str:
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return f"http://{host}:{server.server_address[1]}"


def serve_all() -> dict[str, str]:
    iframe = _serve(_handler_for(SITES / "iframe"), "localhost")
    main = _serve(_handler_for(SITES / "main", iframe_origin=iframe), "127.0.0.1")
    probe = _serve(_handler_for(Path(__file__).with_name("sites")), "127.0.0.1")
    return {
        "main": f"{main}/index.html",
        "dialog": f"{probe}/dialog.html",
        "alert-on-load": f"{probe}/alert-on-load.html",
        "slow": f"{probe}/slow.html",
    }


if __name__ == "__main__":
    for name, url in serve_all().items():
        print(name, url, flush=True)
    threading.Event().wait()
