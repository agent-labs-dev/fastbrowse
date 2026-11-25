"""Styling a nested reply differently must not give it the parent author's name."""

import os
import shutil
from pathlib import Path

import pytest

ROOT = Path(__file__).parents[1]


@pytest.mark.parametrize("card", ["li", "article", 'div role="listitem"', "div"])
@pytest.mark.parametrize("root", ["comment", "comment root"])
@pytest.mark.parametrize("title", ["h3", "b"])
def test_reply_class_modifier_keeps_the_reply_owner(card: str, title: str, root: str) -> None:
    playwright = pytest.importorskip("playwright.sync_api")
    chrome = os.environ.get("FASTBROWSE_CHROME") or shutil.which("chromium") or shutil.which("google-chrome")
    if chrome is None:
        pytest.skip("Chrome is not installed")
    tag = card.split()[0]
    html = (
        "<section><h2>Comments</h2><ul>"
        f'<{card} class="{root}"><{title}>Ada</{title}><p>Parent comment.</p><button>Reply</button>'
        "<button>Hide 2 Replies</button><ul>"
        f'<{card} class="comment reply"><b>Grace</b><p>A reply to Ada.</p><button>Reply</button>'
        f"<button>Hide 3 Replies</button></{tag}>"
        f'<{card} class="comment reply"><b>Linus</b><p>Another reply to Ada.</p><button>Reply</button>'
        f"<button>Hide 4 Replies</button></{tag}></ul></{tag}>"
        f'<{card} class="{root}"><h3>Ruth</h3><p>Another comment.</p><button>Reply</button>'
        f"<button>Hide 5 Replies</button></{tag}></ul></section>"
    )
    snapshot = (ROOT / "crates/fastbrowse-browser/js/snapshot.js").read_text()
    with playwright.sync_playwright() as runtime:
        browser = runtime.chromium.launch(executable_path=chrome, args=["--no-sandbox"])
        try:
            page = browser.new_page()
            page.set_content(html)
            observation = page.evaluate(snapshot + "('snapshot', null, {listeners: null})")
        finally:
            browser.close()
    replies = [control.get("context") for control in observation["controls"] if control["label"] == "Reply"]
    assert replies == ["Ada", "Grace", "Linus", "Ruth"]
    counters = {
        control["label"]: control.get("context")
        for control in observation["controls"]
        if control["label"] in {"Hide 3 Replies", "Hide 4 Replies"}
    }
    assert counters == {"Hide 3 Replies": "Grace", "Hide 4 Replies": "Linus"}, counters
