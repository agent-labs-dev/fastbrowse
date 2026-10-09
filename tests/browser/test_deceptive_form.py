import threading
from collections.abc import Iterator
from http.server import ThreadingHTTPServer
from pathlib import Path

import pytest

from fastbrowse.agent import Agent
from fastbrowse.browser import CdpPage
from fastbrowse.models import Limits, Status
from tests.browser.conftest import _handler_for
from tests.browser.test_browser import find
from tests.browser.test_safety import CONFIG, PLAN
from tests.test_policy import ScriptedJev
from tests.test_retrieval import ScriptedLLM


@pytest.fixture
def deceptive_site(tmp_path: Path) -> Iterator[tuple[str, list[str]]]:
    (tmp_path / "index.html").write_text(
        '<form method="post" action="/delete-all"><input name="items" value="all"><button>Tidy up list</button></form>'
    )
    receipts: list[str] = []
    server = ThreadingHTTPServer(("127.0.0.1", 0), _handler_for(tmp_path, posted=receipts))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", receipts
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


async def test_a_harmless_caption_does_not_hide_a_destructive_form(
    page: CdpPage, deceptive_site: tuple[str, list[str]]
) -> None:
    url, receipts = deceptive_site
    await page.navigate(f"{url}/index.html")
    target = find(await page.observe(), "Tidy up list")
    assert target.submit_semantics and "/delete-all" in target.submit_semantics
    jev = ScriptedJev({"operation": "click", "click_target": target.id})
    result = await Agent(page, jev, ScriptedLLM([PLAN]), config=CONFIG).run(
        "Tidy the list layout without deleting any records.", limits=Limits(max_steps=1)
    )
    assert result.status is Status.NEEDS_CONFIRMATION
    assert receipts == []
    assert any("/delete-all" in str(request) for request in jev.requests)
