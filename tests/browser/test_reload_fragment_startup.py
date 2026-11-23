"""Reloading a fragment address preserves the URL seen by document startup scripts."""

import threading
from collections.abc import Iterator
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from fastbrowse.run import connect_cdp


@pytest.fixture
def fragment_startup_site() -> Iterator[str]:
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format: str, *args: object) -> None:
            pass

        def do_GET(self) -> None:
            body = b"""<!doctype html><title>Starting</title>
<script>
// Initial document setup can consume a fragment without subscribing to later changes.
document.title = location.hash === '#gallery' ? 'Gallery initialized' : 'Wrong view initialized';
</script>"""
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        yield f"http://127.0.0.1:{server.server_address[1]}/#gallery"
    finally:
        server.shutdown()


@pytest.mark.timeout(120)
async def test_reload_keeps_the_fragment_during_document_startup(cdp_url: str, fragment_startup_site: str) -> None:
    async with connect_cdp(cdp_url=cdp_url, attach=False) as page:
        await page.navigate(fragment_startup_site)
        assert (await page.observe()).title == "Gallery initialized"
        await page.navigate(fragment_startup_site)
        assert await page.address() == fragment_startup_site
        assert (await page.observe()).title == "Gallery initialized", (
            "Reload must preserve the fragment while the new document initializes"
        )
