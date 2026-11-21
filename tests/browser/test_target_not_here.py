"""A page that lists an item only after a reload: the run reloads instead of acting on another item (#1448)."""

import threading
from collections.abc import Iterator
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from fastbrowse import run_task
from tests.scripted import ScriptedJev, ScriptedLLM, steps

PAGE = """<!doctype html><title>Gallery</title><h1>Gallery</h1>{cards}
<script>
for (const card of document.querySelectorAll('button')) {{
  card.onclick = () => {{
    fetch('/clicked?card=' + encodeURIComponent(card.textContent), {{method: 'POST'}});
    document.body.append('Selected ' + card.textContent);
  }};
}}
</script>"""


@pytest.fixture
def site() -> Iterator[tuple[str, list[str], list[int]]]:
    clicked: list[str] = []
    loads: list[int] = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format: str, *args: object) -> None:
            pass

        def do_POST(self) -> None:
            clicked.append(self.path.split("card=", 1)[1].replace("%20", " "))
            self.send_response(204)
            self.end_headers()

        def do_GET(self) -> None:
            # The first load lists the originals only, as a dialog a site builds once per load does.
            loads.append(1)
            cards = ["Original one", "Original two"] + (["New image"] if len(loads) > 1 else [])
            body = PAGE.format(cards="".join(f"<button>{card}</button>" for card in cards)).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        yield f"http://127.0.0.1:{server.server_address[1]}/", clicked, loads
    finally:
        server.shutdown()


@pytest.mark.timeout(120)
async def test_a_missing_item_is_reloaded_for_instead_of_acting_on_another(
    cdp_url: str, site: tuple[str, list[str], list[int]]
) -> None:
    base, clicked, loads = site
    # The first step names an item the page does not list, which the scripted Jev answers as the question lets it:
    # not here. The operation is asked again, and the script's next step reloads the page.
    await run_task(
        "Click the new image",
        start=base,
        cdp_url=cdp_url,
        jev=ScriptedJev(steps("click New image; navigate; click New image")),
        llm=ScriptedLLM(),
    )
    # The first click is the new image, which only a reload lists. What the scripted Jev does once its script is spent
    # is no part of this test.
    assert clicked[:1] == ["New image"], clicked
    assert len(loads) >= 2, loads
